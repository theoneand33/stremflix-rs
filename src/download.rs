//! Downloads for offline playback: a stream (http URL, or the torrent engine's local stream URL) saved to a file.

use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

use crate::db::{Db, Download};
use crate::library::MEDIA_EXTS;
use crate::torrent::TorrentEngine;

/// File name for `title` with no path separators, `..` or control characters, so addon text can't
/// escape the downloads folder. The extension comes from `hint` (a file name or URL path) if it's a media type.
pub fn file_name(title: &str, hint: Option<&str>) -> String {
    let mut stem: String = title.chars().map(|c| if c.is_alphanumeric() || " -_()[],'&".contains(c) { c } else { '_' }).collect();
    // Bytes, not chars: file systems cap names at 255 bytes, and non-ASCII takes up to 4 each.
    while stem.len() > 120 {
        stem.pop();
    }
    let stem = stem.trim();
    let stem = if stem.is_empty() { "download" } else { stem };
    // Windows reserves these device names even with an extension.
    const RESERVED: [&str; 22] = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"];
    let stem = if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) { format!("_{stem}") } else { stem.to_string() };
    let ext = (hint.and_then(|h| Path::new(h).extension()?.to_str()).map(str::to_ascii_lowercase))
        .filter(|e| MEDIA_EXTS.contains(&e.as_str()))
        .unwrap_or_else(|| "mkv".into());
    format!("{stem}.{ext}")
}

/// `<sanitized title> <lang>.srt`; addon text never reaches the path. mpv sniffs the format, so `.srt` fits any text subtitle.
pub fn subtitle_name(title: &str, lang: &str) -> String {
    let n = file_name(&format!("{title} {lang}"), None);
    format!("{}.srt", n.strip_suffix(".mkv").unwrap_or(&n))
}

/// Write `data` to a free `subtitle_name` in `dir`; the path it landed at.
pub async fn save_subtitle(dir: &Path, title: &str, lang: &str, data: &[u8]) -> Result<PathBuf> {
    let path = reserve(dir, &subtitle_name(title, lang))?;
    let res = match tokio::fs::write(part(&path), data).await {
        Ok(()) => tokio::fs::rename(part(&path), &path).await,
        Err(e) => Err(e),
    };
    if let Err(e) = res {
        let _ = std::fs::remove_file(part(&path));
        return Err(e.into());
    }
    Ok(path)
}

/// Where a download is written until it completes.
fn part(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".part");
    p.into()
}

/// Claim a free path for `name` in `dir` (`name (2).ext`, ... if taken) by creating its `.part` file.
fn reserve(dir: &Path, name: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "mkv"));
    for n in 1..1000 {
        let path = dir.join(if n == 1 { name.to_string() } else { format!("{stem} ({n}).{ext}") });
        if path.exists() {
            continue;
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(part(&path)) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create {}", part(&path).display())),
        }
    }
    anyhow::bail!("no free file name for {name}")
}

/// At most this many downloads transfer at once; the rest wait queued.
const MAX_ACTIVE: usize = 2;

/// Where a download is in the queue (finished ones leave it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Queued,
    Active,
    Paused,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Failed => "failed",
        }
    }

    /// An `active` row means the app quit mid-transfer, so it comes back paused.
    pub fn from_db(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "failed" => Self::Failed,
            _ => Self::Paused,
        }
    }
}

/// A download that is waiting, running, paused or failed.
pub struct Job {
    pub d: Download,
    pub state: State,
    pub got: u64,
    pub total: u64,
    /// Bytes per second, smoothed.
    pub speed: f64,
    pub err: Option<String>,
    /// http URL or magnet.
    source: String,
    headers: Vec<(String, String)>,
    /// Which transfer this job is on, so a stopped one's late events can be told from the current one's.
    run: u64,
    /// When and at how many bytes the speed was last measured.
    last: (Instant, u64),
    /// Dropping or sending on it stops the transfer.
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    /// ETag or Last-Modified (or `IMMUTABLE`) of the file as first fetched; Resume restarts if it is empty or changed.
    validator: String,
    /// A transfer task is still alive (a paused one flushes its last write first); no new run starts until it reports.
    running: bool,
}

impl Job {
    fn new(d: Download, source: String, headers: Vec<(String, String)>, state: State, validator: String, total: u64) -> Self {
        let got = part_len(&d.path);
        // A torrent stream is fixed by its info hash, so it needs no validator.
        let validator = if is_torrent(&source) { IMMUTABLE.into() } else { validator };
        Self { d, source, headers, state, run: 0, got, total, speed: 0.0, last: (Instant::now(), got), cancel: None, err: None, validator, running: false }
    }
}

fn is_torrent(source: &str) -> bool {
    crate::addon::parse_magnet_hash(source).is_some()
}

fn part_len(path: &Path) -> u64 {
    std::fs::metadata(part(path)).map_or(0, |m| m.len())
}

/// A transfer the queue started, for the caller to run with [`run`].
pub struct Transfer {
    pub path: PathBuf,
    run: u64,
    source: String,
    headers: Vec<(String, String)>,
    prev: (String, u64),
    cancel: tokio::sync::oneshot::Receiver<()>,
}

/// What a running transfer reports back to [`Queue::on_event`].
#[derive(Debug)]
pub enum Event {
    /// Bytes got, total and the file's validator, about 4 times a second.
    Progress { path: PathBuf, run: u64, got: u64, total: u64, validator: String },
    /// Finished, failed or stopped.
    Done { path: PathBuf, run: u64, res: std::result::Result<(), String> },
}

/// What a queue change asks of the caller: transfers to run, errors to show, a download that finished.
#[derive(Default)]
#[must_use = "started transfers only run when the caller spawns them"]
pub struct Step {
    pub start: Vec<Transfer>,
    pub errors: Vec<String>,
    pub finished: Option<Download>,
}

/// Downloads waiting, running, paused or failed, oldest first, into `<data>/downloads`; at most `MAX_ACTIVE` transfer at once.
pub struct Queue {
    dir: PathBuf,
    jobs: Vec<Job>,
    runs: u64,
}

impl Queue {
    /// The unfinished downloads of `db`; a download that was transferring when the app quit comes back paused.
    /// `.part` files no job owns are deleted.
    pub fn load(db: &Db, data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("downloads");
        let jobs: Vec<Job> = db.download_queue()?.into_iter().map(|q| Job::new(q.d, q.source, q.headers, State::from_db(&q.status), q.validator, q.total)).collect();
        clear_partial(&dir, &jobs.iter().map(|j| j.d.path.clone()).collect::<Vec<_>>());
        Ok(Self { dir, jobs, runs: 0 })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    /// Queue `source` (http URL or magnet) as `d`, named from its title; `hint` (a file name or URL path) gives the extension.
    pub fn add(&mut self, db: &Db, mut d: Download, source: String, headers: Vec<(String, String)>, hint: Option<&str>) -> Step {
        let res = reserve(&self.dir, &file_name(&d.title, hint)).and_then(|p| {
            d.path = p;
            db.queue_download(&d, &source, &headers, State::Queued.as_str()).inspect_err(|_| drop(std::fs::remove_file(part(&d.path))))
        });
        if let Err(e) = res {
            return Step { errors: vec![format!("download: {e:#}")], ..Default::default() };
        }
        self.jobs.push(Job::new(d, source, headers, State::Queued, String::new(), 0));
        self.pump(db, Step::default())
    }

    /// Stop job `i`'s transfer; it keeps its `.part` file.
    pub fn pause(&mut self, db: &Db, i: usize) -> Step {
        if let Some(c) = self.jobs[i].cancel.take() {
            let _ = c.send(());
        }
        let step = self.set_state(db, i, State::Paused, Step::default());
        self.pump(db, step)
    }

    pub fn resume(&mut self, db: &Db, i: usize) -> Step {
        let step = self.set_state(db, i, State::Queued, Step::default());
        self.pump(db, step)
    }

    /// Forget job `i`: stop it, delete its `.part` file and its row.
    pub fn remove(&mut self, db: &Db, i: usize) -> Step {
        let j = self.jobs.remove(i);
        if let Some(c) = j.cancel {
            let _ = c.send(());
        }
        drop(std::fs::remove_file(part(&j.d.path)));
        let mut step = Step::default();
        if let Err(e) = db.remove_download(&j.d) {
            step.errors.push(format!("removing download {}: {e:#}", j.d.title));
        }
        self.pump(db, step)
    }

    /// Apply what a transfer reported; events of a stopped or replaced run are dropped.
    pub fn on_event(&mut self, db: &Db, e: Event) -> Step {
        let mut step = Step::default();
        match e {
            Event::Progress { path, run, got, total, validator } => {
                let Some(j) = self.jobs.iter_mut().find(|j| j.d.path == path && j.run == run && j.state == State::Active) else { return step };
                if (j.total, &j.validator) != (total, &validator) {
                    (j.total, j.validator) = (total, validator);
                    if let Err(e) = db.set_download_meta(&j.d, &j.validator, j.total) {
                        step.errors.push(format!("saving download state: {e:#}"));
                    }
                }
                let dt = j.last.0.elapsed().as_secs_f64();
                if dt > 0.0 {
                    let rate = got.saturating_sub(j.last.1) as f64 / dt;
                    j.speed = if j.speed == 0.0 { rate } else { j.speed * 0.5 + rate * 0.5 };
                }
                (j.got, j.total, j.last) = (got, total, (Instant::now(), got));
                step
            }
            Event::Done { path, run, res } => {
                let Some(i) = self.jobs.iter().position(|j| j.d.path == path && j.run == run) else { return step };
                self.jobs[i].running = false;
                // A paused download's error only frees its slot; it was flushed, so Resume continues from the `.part` length.
                // A finished one (paused as it completed) is already renamed into place, so it is kept.
                if self.jobs[i].state != State::Active && res.is_err() {
                    self.jobs[i].got = part_len(&path);
                    return self.pump(db, step);
                }
                match res {
                    // The `.part` stays so Resume can continue it.
                    Err(e) => {
                        self.jobs[i].err = Some(e);
                        step = self.set_state(db, i, State::Failed, step);
                    }
                    Ok(()) => {
                        let d = self.jobs.remove(i).d;
                        // A finished file the database can't list would be orphaned on disk, so it goes.
                        match db.add_download(&d).inspect_err(|_| drop(std::fs::remove_file(&d.path))) {
                            Ok(()) => step.finished = Some(d),
                            Err(e) => step.errors.push(format!("download {}: {e:#}", d.title)),
                        }
                    }
                }
                self.pump(db, step)
            }
        }
    }

    fn set_state(&mut self, db: &Db, i: usize, state: State, mut step: Step) -> Step {
        let j = &mut self.jobs[i];
        (j.state, j.speed) = (state, 0.0);
        if let Err(e) = db.set_download_state(&j.d, state.as_str()) {
            step.errors.push(format!("saving download state: {e:#}"));
        }
        step
    }

    /// Start queued jobs, oldest first, while fewer than `MAX_ACTIVE` transfer.
    fn pump(&mut self, db: &Db, mut step: Step) -> Step {
        // A stopped job whose task is still flushing keeps its slot and can't restart yet.
        let busy = self.jobs.iter().filter(|j| j.running || j.state == State::Active).count();
        let start: Vec<usize> = self.jobs.iter().enumerate().filter(|(_, j)| j.state == State::Queued && !j.running).map(|(i, _)| i).take(MAX_ACTIVE.saturating_sub(busy)).collect();
        for i in start {
            step = self.set_state(db, i, State::Active, step);
            self.runs += 1;
            let (cancel, cancelled) = tokio::sync::oneshot::channel();
            let j = &mut self.jobs[i];
            (j.run, j.cancel, j.err, j.last, j.running) = (self.runs, Some(cancel), None, (Instant::now(), j.got), true);
            step.start.push(Transfer { path: j.d.path.clone(), run: j.run, source: j.source.clone(), headers: j.headers.clone(), prev: (j.validator.clone(), j.total), cancel: cancelled });
        }
        step
    }
}

/// Run a transfer to its end, reporting through `report`. `engine` is awaited only for a magnet, whose torrent is
/// pinned meanwhile so playing or stopping another title doesn't pause it mid-download.
pub async fn run(t: Transfer, engine: impl Future<Output = Result<std::sync::Arc<TorrentEngine>>>, report: impl Fn(Event)) {
    let Transfer { path, run, source, headers, prev, mut cancel } = t;
    let res = async {
        let (url, headers, pin) = if is_torrent(&source) {
            let engine = engine.await?;
            let (url, id) = engine.stream_url(&source, None, true).await?;
            (url, vec![], Some((engine, id)))
        } else {
            (source, headers, None)
        };
        let progress = |got, total, validator: &str| report(Event::Progress { path: path.clone(), run, got, total, validator: validator.to_string() });
        // Pausing or dropping the job sends or drops the sender; fetch flushes its file before returning.
        let res = fetch(&url, &headers, &path, (&prev.0, prev.1), &mut cancel, progress).await;
        // shortcut: a paused torrent is unpinned but keeps its cache copy until the next playback releases it; resume re-adds it.
        if let Some((engine, id)) = pin {
            engine.unpin(id).await;
        }
        res
    };
    let res = res.await.map_err(|e| format!("{e:#}"));
    report(Event::Done { path, run, res });
}

/// Delete `.part` files left by downloads cut off when the app quit, except those of `keep` (queued or paused downloads).
fn clear_partial(dir: &Path, keep: &[PathBuf]) {
    let keep: Vec<PathBuf> = keep.iter().map(|p| part(p)).collect();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "part") && !keep.contains(&e.path()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Stored in place of an ETag for sources that cannot change under us (a torrent stream is fixed by its info hash).
const IMMUTABLE: &str = "immutable";

/// Byte offset to continue from after asking for `bytes=have-`: `have` only if the server answered 206
/// with a `Content-Range` that starts there and whose total is `total`, else 0 (start over).
pub fn resume_start(have: u64, status: u16, content_range: Option<&str>, total: u64) -> u64 {
    let range = content_range.and_then(|c| c.strip_prefix("bytes ")?.split_once('/'));
    let start = range.and_then(|(r, _)| r.split('-').next()?.parse::<u64>().ok());
    let all = range.and_then(|(_, t)| t.parse::<u64>().ok());
    if status == 206 && start == Some(have) && all == Some(total) { have } else { 0 }
}

/// Download `url` (with the addon's request `headers`) into `part(dest)`, then move it to `dest`.
/// Continues a `.part` left by a pause only if the file is unchanged: `prev` is the validator (ETag or
/// Last-Modified, sent as `If-Range`) and total size stored when it started; without them it restarts.
/// `progress(got, total, validator)` runs about 4 times a second. When `cancel` fires (or its sender
/// drops) the written bytes are flushed first, so the `.part` length is the resume offset.
pub async fn fetch(url: &str, headers: &[(String, String)], dest: &Path, prev: (&str, u64), cancel: &mut tokio::sync::oneshot::Receiver<()>, mut progress: impl FnMut(u64, u64, &str)) -> Result<()> {
    // No total timeout: a movie can take hours. A stalled connection still fails.
    let client = crate::addon::with_proxy(reqwest::Client::builder().user_agent(crate::addon::user_agent()).connect_timeout(Duration::from_secs(15)).read_timeout(Duration::from_secs(120)))?.build()?;
    let tmp = part(dest);
    let mut have = tokio::fs::metadata(&tmp).await.map_or(0, |m| m.len());
    if prev.0.is_empty() || prev.1 == 0 {
        have = 0;
    }
    let (mut resp, start) = loop {
        let mut req = client.get(url);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
            if prev.0 != IMMUTABLE {
                req = req.header(reqwest::header::IF_RANGE, prev.0);
            }
        }
        let resp = tokio::select! { r = req.send() => r.context("download")?, _ = &mut *cancel => anyhow::bail!("stopped") };
        // 416: the `.part` is not a prefix of the file (or is all of it); start over.
        if have > 0 && resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            have = 0;
            continue;
        }
        let resp = resp.error_for_status()?;
        let cr = resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok());
        let start = if have > 0 { resume_start(have, resp.status().as_u16(), cr, prev.1) } else { 0 };
        // A 206 that doesn't continue our `.part` (the file changed): ask again for the whole file.
        if have > 0 && start == 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            have = 0;
            continue;
        }
        break (resp, start);
    };
    let kind = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
    ensure!(!["mpegurl", "dash+xml"].iter().any(|t| kind.contains(t)), "{kind} is a streaming playlist, not a video file, so it can't be downloaded");
    ensure!(!kind.starts_with("text/html"), "the server sent a web page, not a video file");
    // A live or endless stream has no length; refusing it keeps a download from filling the disk.
    let Some(body) = resp.content_length() else { anyhow::bail!("the server gave no file size (likely a live stream), so it can't be downloaded") };
    ensure!(body > 0 || start > 0, "the server sent an empty file");
    let len = start + body;
    let h = |n| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    let validator = h("etag").filter(|e| !e.starts_with("W/")).or_else(|| h("last-modified")).unwrap_or_else(|| if prev.0 == IMMUTABLE { IMMUTABLE.into() } else { String::new() });
    let mut f = tokio::fs::OpenOptions::new().write(true).create(true).append(start > 0).truncate(start == 0).open(&tmp).await?;
    let mut got = start;
    let mut last = Instant::now();
    progress(got, len, &validator);
    loop {
        let chunk = tokio::select! {
            c = resp.chunk() => c?,
            _ = &mut *cancel => {
                // Await the last buffered write so the `.part` length is exactly what was received.
                f.flush().await?;
                f.sync_all().await?;
                anyhow::bail!("stopped")
            }
        };
        let Some(chunk) = chunk else { break };
        f.write_all(&chunk).await?;
        got += chunk.len() as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            progress(got, len, &validator);
        }
    }
    ensure!(len == got, "download cut off at {got} of {len} bytes");
    f.flush().await?;
    f.sync_all().await?;
    finish(&tmp, dest)
}

/// Move `part` to `dest`, failing rather than overwriting a file created there since `reserve`.
fn finish(part: &Path, dest: &Path) -> Result<()> {
    match std::fs::hard_link(part, dest) {
        Ok(()) => {
            // The file is complete; a stale `.part` is cleared at the next start.
            let _ = std::fs::remove_file(part);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e).with_context(|| format!("{} already exists", dest.display())),
        // shortcut: no hard links on this file system (e.g. FAT), so check then rename; racy, but only against other apps writing our downloads folder.
        Err(_) => {
            ensure!(!dest.exists(), "{} already exists", dest.display());
            Ok(std::fs::rename(part, dest)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_stay_in_the_folder() {
        assert_eq!(file_name("Movie: The Sequel", Some("Movie.2024.1080p.MP4")), "Movie_ The Sequel.mp4");
        assert_eq!(file_name("../../etc/passwd", None), "______etc_passwd.mkv");
        assert_eq!(file_name("a\\b\u{202e}c\n", Some("/stream/0")), "a_b_c_.mkv");
        assert_eq!(file_name("  ", Some("x.exe")), "download.mkv", "blank title, non-media extension");
        assert_eq!(file_name("..", None), "__.mkv");
        assert_eq!(file_name(&"x".repeat(500), None).len(), 124);
        assert!(file_name(&"é".repeat(500), None).len() <= 124, "capped by bytes");
        assert_eq!(file_name("nul", None), "_nul.mkv");
        assert_eq!(subtitle_name("../x/Film: 2", "eng"), "___x_Film_ 2 eng.srt");
    }

    #[test]
    fn reserve_picks_a_free_name() {
        let dir = tempfile::tempdir().unwrap();
        let a = reserve(dir.path(), "A.mkv").unwrap();
        let b = reserve(dir.path(), "A.mkv").unwrap();
        assert_eq!((a.file_name().unwrap(), b.file_name().unwrap()), ("A.mkv".as_ref(), "A (2).mkv".as_ref()));
        assert!(part(&a).exists() && !a.exists());
        clear_partial(dir.path(), std::slice::from_ref(&b));
        assert!(part(&b).exists() && !part(&a).exists());
        clear_partial(dir.path(), &[]);
        assert!(!part(&a).exists() && !part(&b).exists());
    }

    fn queue() -> (tempfile::TempDir, Db, Queue) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let q = Queue::load(&db, dir.path()).unwrap();
        (dir, db, q)
    }

    fn add(db: &Db, q: &mut Queue, title: &str) -> Step {
        let d = Download { path: PathBuf::new(), kind: "movie".into(), meta_id: title.into(), video_id: title.into(), title: title.into() };
        q.add(db, d, format!("https://x.example/{title}.mkv"), vec![], None)
    }

    fn done(q: &mut Queue, db: &Db, t: &Transfer, res: std::result::Result<(), String>) -> Step {
        q.on_event(db, Event::Done { path: t.path.clone(), run: t.run, res })
    }

    #[test]
    fn queue_runs_the_oldest_up_to_the_limit() {
        let (_d, db, mut q) = queue();
        let a = add(&db, &mut q, "A").start;
        let b = add(&db, &mut q, "B").start;
        assert!(add(&db, &mut q, "C").start.is_empty(), "both slots taken");
        assert_eq!(q.jobs().iter().map(|j| j.state).collect::<Vec<_>>(), [State::Active, State::Active, State::Queued]);
        // A paused job keeps its slot until its task has flushed and reported.
        assert!(q.pause(&db, 0).start.is_empty());
        let next = done(&mut q, &db, &a[0], Err("cancelled".into())).start;
        assert_eq!(next.len(), 1);
        assert_eq!(q.jobs()[0].state, State::Paused);
        assert!(q.jobs()[0].err.is_none(), "a pause is not a failure");
        // A finished job leaves the queue for the finished list.
        let fin = done(&mut q, &db, &b[0], Ok(()));
        assert_eq!(fin.finished.map(|d| d.title), Some("B".into()));
        assert_eq!(db.downloads().unwrap().len(), 1);
        let failed = done(&mut q, &db, &next[0], Err("404".into()));
        assert!(failed.finished.is_none());
        assert_eq!((q.jobs()[1].state, q.jobs()[1].err.as_deref()), (State::Failed, Some("404")));
        // Resume restarts the paused one; removing forgets it.
        assert_eq!(q.resume(&db, 0).start.len(), 1);
        _ = q.remove(&db, 0);
        assert_eq!(q.jobs().len(), 1);
    }

    #[test]
    fn events_of_a_replaced_run_are_dropped() {
        let (_d, db, mut q) = queue();
        let first = add(&db, &mut q, "A").start;
        _ = q.pause(&db, 0);
        _ = done(&mut q, &db, &first[0], Err("cancelled".into()));
        let second = q.resume(&db, 0).start;
        let progress = |t: &Transfer, got| Event::Progress { path: t.path.clone(), run: t.run, got, total: 10, validator: "v".into() };
        _ = q.on_event(&db, progress(&first[0], 7));
        assert_eq!(q.jobs()[0].got, 0, "the old run's progress is ignored");
        _ = q.on_event(&db, progress(&second[0], 4));
        assert_eq!((q.jobs()[0].got, q.jobs()[0].total), (4, 10));
        assert!(done(&mut q, &db, &first[0], Ok(())).finished.is_none());
        assert_eq!(q.jobs()[0].state, State::Active);
    }

    #[test]
    fn a_download_cut_off_by_quitting_comes_back_paused() {
        let (dir, db, mut q) = queue();
        _ = add(&db, &mut q, "A");
        let q = Queue::load(&db, dir.path()).unwrap();
        assert_eq!(q.jobs()[0].state, State::Paused);
        assert_eq!(State::from_db(State::Queued.as_str()), State::Queued);
    }

    #[test]
    fn resume_needs_a_matching_206() {
        assert_eq!(resume_start(5, 206, Some("bytes 5-9/10"), 10), 5);
        assert_eq!(resume_start(5, 200, None, 10), 0, "range ignored");
        assert_eq!(resume_start(5, 206, Some("bytes 0-9/10"), 10), 0, "wrong offset");
        assert_eq!(resume_start(5, 206, Some("bytes 5-12/13"), 10), 0, "file size changed");
        assert_eq!(resume_start(5, 206, Some("bytes 5-9/*"), 10), 0, "unknown total");
        assert_eq!(resume_start(5, 206, None, 10), 0);
    }

    #[test]
    fn finish_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let dest = reserve(dir.path(), "a.mp4").unwrap();
        std::fs::write(part(&dest), b"new").unwrap();
        std::fs::write(&dest, b"theirs").unwrap();
        assert!(finish(&part(&dest), &dest).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"theirs");
        std::fs::remove_file(&dest).unwrap();
        finish(&part(&dest), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(!part(&dest).exists());
    }

    #[tokio::test]
    async fn fetch_writes_the_file() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0; 1024]);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = reserve(dir.path(), "a.mp4").unwrap();
        let (_tx, mut rx) = tokio::sync::oneshot::channel();
        fetch(&format!("http://{addr}/a.mp4"), &[], &dest, ("", 0), &mut rx, |_, _, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
        assert!(!part(&dest).exists());
    }

    /// Serves one canned response per connection and returns the lower-cased requests.
    fn serve(responses: Vec<&'static [u8]>) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            use std::io::{Read, Write};
            responses
                .into_iter()
                .map(|r| {
                    let (mut s, _) = listener.accept().unwrap();
                    let mut buf = [0; 1024];
                    let n = s.read(&mut buf).unwrap();
                    s.write_all(r).unwrap();
                    String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase()
                })
                .collect()
        });
        (addr, h)
    }

    #[tokio::test]
    async fn fetch_resumes_with_a_range() {
        let (addr, server) = serve(vec![b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-4/5\r\nContent-Length: 3\r\nConnection: close\r\n\r\nllo"]);
        let dir = tempfile::tempdir().unwrap();
        let dest = reserve(dir.path(), "a.mp4").unwrap();
        std::fs::write(part(&dest), b"he").unwrap();
        let (_tx, mut rx) = tokio::sync::oneshot::channel();
        fetch(&format!("http://{addr}/a.mp4"), &[], &dest, ("\"v1\"", 5), &mut rx, |_, _, _| {}).await.unwrap();
        let req = &server.join().unwrap()[0];
        assert!(req.contains("range: bytes=2-") && req.contains("if-range: \"v1\""), "{req}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn fetch_restarts_when_the_file_changed() {
        // The server ignores If-Range's mismatch by sending a 206 of a different total, then serves the whole new file.
        let (addr, server) = serve(vec![
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 2-8/9\r\nContent-Length: 7\r\nConnection: close\r\n\r\nrld-new",
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nETag: \"v2\"\r\nConnection: close\r\n\r\nworld-new",
        ]);
        let dir = tempfile::tempdir().unwrap();
        let dest = reserve(dir.path(), "a.mp4").unwrap();
        std::fs::write(part(&dest), b"he").unwrap();
        let (_tx, mut rx) = tokio::sync::oneshot::channel();
        let seen = std::cell::RefCell::new(String::new());
        fetch(&format!("http://{addr}/a.mp4"), &[], &dest, ("\"v1\"", 5), &mut rx, |_, _, v| *seen.borrow_mut() = v.to_string()).await.unwrap();
        let reqs = server.join().unwrap();
        assert!(reqs[0].contains("range: bytes=2-") && !reqs[1].contains("range:"), "{reqs:?}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"world-new");
        assert_eq!(*seen.borrow(), "\"v2\"");
    }

    #[tokio::test]
    async fn fetch_without_a_validator_restarts() {
        let (addr, server) = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"]);
        let dir = tempfile::tempdir().unwrap();
        let dest = reserve(dir.path(), "a.mp4").unwrap();
        std::fs::write(part(&dest), b"he").unwrap();
        let (_tx, mut rx) = tokio::sync::oneshot::channel();
        fetch(&format!("http://{addr}/a.mp4"), &[], &dest, ("", 5), &mut rx, |_, _, _| {}).await.unwrap();
        assert!(!server.join().unwrap()[0].contains("range:"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }
}
