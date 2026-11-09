//! Downloads for offline playback: a stream (http URL, or the torrent engine's local stream URL) saved to a file.

use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

use crate::library::MEDIA_EXTS;

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
pub fn part(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".part");
    p.into()
}

/// Claim a free path for `name` in `dir` (`name (2).ext`, ... if taken) by creating its `.part` file.
pub fn reserve(dir: &Path, name: &str) -> Result<PathBuf> {
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
pub const MAX_ACTIVE: usize = 2;

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

/// Indices of the queued downloads to start now: oldest first, up to `max` active in total.
pub fn to_start(states: &[State], max: usize) -> Vec<usize> {
    let free = max.saturating_sub(states.iter().filter(|s| **s == State::Active).count());
    states.iter().enumerate().filter(|(_, s)| **s == State::Queued).map(|(i, _)| i).take(free).collect()
}

/// Delete `.part` files left by downloads cut off when the app quit, except those of `keep` (queued or paused downloads).
pub fn clear_partial(dir: &Path, keep: &[PathBuf]) {
    let keep: Vec<PathBuf> = keep.iter().map(|p| part(p)).collect();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "part") && !keep.contains(&e.path()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Stored in place of an ETag for sources that cannot change under us (a torrent stream is fixed by its info hash).
pub const IMMUTABLE: &str = "immutable";

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

    #[test]
    fn queue_starts_oldest_queued_up_to_the_limit() {
        use State::*;
        assert_eq!(to_start(&[Queued, Queued, Queued], 2), [0, 1]);
        assert_eq!(to_start(&[Active, Paused, Queued, Queued], 2), [2]);
        assert_eq!(to_start(&[Active, Active, Queued], 2), Vec::<usize>::new());
        assert_eq!(to_start(&[Failed, Paused], 2), Vec::<usize>::new());
        assert_eq!(State::from_db("active"), Paused);
        assert_eq!(State::from_db(State::Queued.as_str()), Queued);
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
