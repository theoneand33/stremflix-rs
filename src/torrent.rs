//! Torrent engine: an embedded librqbit session plus its HTTP API bound to
//! 127.0.0.1, which serves torrent files with Range support for mpv.

use anyhow::{Context, Result, bail};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, Session, SessionOptions};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::addon::{magnet_peers, parse_magnet_hash};
use crate::library::MEDIA_EXTS;

/// How long to wait for peers to send a magnet's metadata.
const METADATA_TIMEOUT: Duration = Duration::from_secs(60);

/// Cap on a local `.torrent` file; real ones are a few hundred KB.
const MAX_TORRENT_FILE: u64 = 16 * 1024 * 1024;

/// Peer connections per torrent: Soft is gentle on slow links, Fast opens many.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Profile {
    Default,
    Soft,
    Fast,
}

impl Profile {
    pub const ALL: [Profile; 3] = [Profile::Default, Profile::Soft, Profile::Fast];

    pub fn label(self) -> &'static str {
        match self {
            Profile::Default => "Default",
            Profile::Soft => "Soft",
            Profile::Fast => "Fast",
        }
    }

    /// Parses a stored [`label`](Self::label); anything else is Default.
    pub fn from_label(s: &str) -> Self {
        Self::ALL.into_iter().find(|p| p.label() == s).unwrap_or(Profile::Default)
    }

    /// `None` keeps librqbit's default (128).
    fn peer_limit(self) -> Option<usize> {
        match self {
            Profile::Default => None,
            Profile::Soft => Some(40),
            Profile::Fast => Some(300),
        }
    }
}

/// How long a finished torrent keeps seeding once released: until the upload ratio or the time is reached.
/// Zero means no limit of that kind; both zero stops it at once.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SeedLimit {
    pub ratio: f64,
    pub secs: u64,
}

impl SeedLimit {
    fn enabled(self) -> bool {
        self.ratio > 0.0 || self.secs > 0
    }

    /// Whether a torrent that uploaded `up` of `size` bytes over `elapsed` has hit a limit.
    pub fn reached(self, up: u64, size: u64, elapsed: Duration) -> bool {
        (self.ratio > 0.0 && up as f64 >= self.ratio * size as f64) || (self.secs > 0 && elapsed.as_secs() >= self.secs)
    }
}

/// One torrent for the manager list.
pub struct TorrentRow {
    pub id: usize,
    pub name: String,
    pub progress: (u64, u64),
    pub uploaded: u64,
    pub paused: bool,
}

/// Torrent ids (oldest first, with their downloaded bytes) to delete so the rest fit in `cache` bytes.
pub fn evict(torrents: &[(usize, u64)], cache: u64) -> Vec<usize> {
    let mut total: u64 = torrents.iter().map(|t| t.1).sum();
    let mut sorted = torrents.to_vec();
    sorted.sort();
    // Ids grow with each add, so lower ids are older.
    sorted.into_iter().take_while(|&(_, b)| (cache == 0 || total > cache) && { total -= b; true }).map(|t| t.0).collect()
}

/// `(index, name, size)` of the playable files, largest first.
pub fn media_files(files: &[(String, u64)]) -> Vec<(usize, String, u64)> {
    let mut v: Vec<_> = files
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| n.rsplit_once('.').is_some_and(|(_, e)| MEDIA_EXTS.contains(&e.to_ascii_lowercase().as_str())))
        .map(|(i, (n, l))| (i, n.clone(), *l))
        .collect();
    v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    v
}

pub struct TorrentEngine {
    session: Arc<Session>,
    http: SocketAddr,
    /// Bytes of stopped torrents kept on disk for a quick replay; 0 deletes them at once.
    cache: u64,
    /// Torrents being downloaded, once per download: never stopped by [`release_except`](Self::release_except).
    /// Held across a whole release and across [`stream_url`](Self::stream_url)'s unpause+pin, so neither interleaves.
    pinned: tokio::sync::Mutex<Vec<usize>>,
    seed: SeedLimit,
    /// Released, finished torrents still seeding, with when they started.
    seeding: std::sync::Mutex<std::collections::HashMap<usize, Instant>>,
}

impl TorrentEngine {
    /// Must be called inside a tokio runtime. `dht=false` is for offline/CI use.
    /// Clears `download_dir` first: without session persistence, leftover files belong to no torrent.
    pub async fn start(download_dir: PathBuf, dht: bool, cache: u64, profile: Profile, seed: SeedLimit) -> Result<Arc<Self>> {
        let _ = std::fs::remove_dir_all(&download_dir);
        std::fs::create_dir_all(&download_dir)?;
        let mut opts = SessionOptions { persistence: None, fastresume: false, peer_limit: profile.peer_limit(), ..Default::default() };
        if !dht {
            opts.dht = None;
        }
        let session = Session::new_with_opts(download_dir, opts).await.context("start torrent session")?;
        let listener = librqbit_dualstack_sockets::TcpListener::bind_tcp(
            "127.0.0.1:0".parse().unwrap(),
            Default::default(),
        )
        .context("bind torrent HTTP")?;
        let http = listener.bind_addr();
        let api = Api::new(session.clone(), None, None);
        let server = librqbit::http_api::HttpApi::new(api, Some(librqbit::http_api::HttpApiOptions { read_only: true, ..Default::default() }));
        tokio::spawn(server.make_http_api_and_run(listener, None));
        let engine = Arc::new(Self { session, http, cache, pinned: Default::default(), seed, seeding: Default::default() });
        // Weak: the sweep must not keep a dropped engine alive.
        let weak = Arc::downgrade(&engine);
        tokio::spawn(async move {
            while let Some(e) = weak.upgrade() {
                e.stop_seeded().await;
                drop(e);
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
        Ok(engine)
    }

    /// Pause seeding torrents that reached the limit. shortcut: they stay on disk until the next release evicts them.
    async fn stop_seeded(&self) {
        let due: Vec<_> = {
            let seeding = self.seeding.lock().unwrap();
            self.session.with_torrents(|it| it.filter_map(|(id, h)| seeding.get(&id).map(|t| (id, h.clone(), t.elapsed()))).collect())
        };
        for (id, h, elapsed) in due {
            let st = h.stats();
            if h.is_paused() || self.seed.reached(st.uploaded_bytes, st.total_bytes, elapsed) {
                self.seeding.lock().unwrap().remove(&id);
                if !h.is_paused() && let Err(e) = self.session.pause(&h).await {
                    eprintln!("pause torrent {id}: {e:#}");
                }
            }
        }
    }

    /// Every torrent in the session, for the manager list.
    pub fn list(&self) -> Vec<TorrentRow> {
        self.session.with_torrents(|it| {
            it.map(|(id, h)| {
                let st = h.stats();
                TorrentRow { id, name: h.name().unwrap_or_else(|| format!("torrent {id}")), progress: (st.progress_bytes, st.total_bytes), uploaded: st.uploaded_bytes, paused: h.is_paused() }
            })
            .collect()
        })
    }

    /// Stop torrent `id` and delete its files.
    pub async fn remove(&self, id: usize) {
        self.seeding.lock().unwrap().remove(&id);
        if let Err(e) = self.session.delete(id.into(), true).await {
            eprintln!("delete torrent {id}: {e:#}");
        }
    }

    /// Add a magnet or local `.torrent` file; returns its handle and the file index chosen by `file_idx` or a magnet's `so=`.
    async fn add(&self, magnet: &str, file_idx: Option<usize>) -> Result<(Arc<librqbit::ManagedTorrent>, Option<usize>)> {
        let (add, peers, file_idx) = if magnet.starts_with("magnet:") {
            parse_magnet_hash(magnet).context("invalid magnet link")?;
            // `so=<idx>` (BEP 53 select-only) carries the addon's fileIdx through saved progress.
            let so = url::Url::parse(magnet).ok().and_then(|u| u.query_pairs().find(|(k, _)| k == "so").and_then(|(_, v)| v.parse().ok()));
            (AddTorrent::from_url(magnet), magnet_peers(magnet), file_idx.or(so))
        } else {
            let len = tokio::fs::metadata(magnet).await.with_context(|| format!("open {magnet}"))?.len();
            anyhow::ensure!(len <= MAX_TORRENT_FILE, "{magnet} is too large for a .torrent file");
            (AddTorrent::from_bytes(tokio::fs::read(magnet).await?), Vec::new(), file_idx)
        };
        let opts = AddTorrentOptions {
            overwrite: true,
            initial_peers: (!peers.is_empty()).then_some(peers),
            ..Default::default()
        };
        // A magnet waits for its metadata from peers; with none, it would wait forever.
        let added = tokio::time::timeout(METADATA_TIMEOUT, self.session.add_torrent(add, Some(opts)))
            .await
            .map_err(|_| anyhow::anyhow!("no peers sent the torrent's metadata within {}s", METADATA_TIMEOUT.as_secs()))??;
        match added {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => Ok((h, file_idx)),
            AddTorrentResponse::ListOnly(_) => bail!("unexpected list-only response"),
        }
    }

    /// The media files of a torrent with no chosen file, largest first, when there is more than one to pick from.
    pub async fn media_choices(&self, magnet: &str) -> Result<Vec<(usize, String, u64)>> {
        if magnet_chooses_file(magnet) {
            return Ok(Vec::new());
        }
        let (handle, _) = self.add(magnet, None).await?;
        let files: Vec<(String, u64)> = handle.with_metadata(|m| m.file_infos.iter().map(|f| (f.relative_filename.to_string_lossy().into_owned(), f.len)).collect())?;
        let media = media_files(&files);
        Ok(if media.len() > 1 { media } else { Vec::new() })
    }

    /// Add a magnet or local `.torrent` file and return a local HTTP URL streaming the chosen (or largest) file, and the torrent's id.
    /// `pin` holds the torrent for a download (see `pinned`); undo with [`unpin`](Self::unpin).
    pub async fn stream_url(&self, magnet: &str, file_idx: Option<usize>, pin: bool) -> Result<(String, usize)> {
        let (handle, file_idx) = self.add(magnet, file_idx).await?;
        let lengths: Vec<u64> = handle.with_metadata(|m| m.file_infos.iter().map(|f| f.len).collect())?;
        let idx = match file_idx {
            Some(i) if i < lengths.len() => i,
            Some(i) => bail!("fileIdx {i} out of range ({} files)", lengths.len()),
            None => (0..lengths.len()).max_by_key(|&i| lengths[i]).context("torrent has no files")?,
        };
        // A cached torrent was paused when its playback stopped. Last, so a pin can't leak on an error above.
        let mut pinned = self.pinned.lock().await;
        self.seeding.lock().unwrap().remove(&handle.id());
        if handle.is_paused() {
            self.session.unpause(&handle).await?;
        }
        if pin {
            pinned.push(handle.id());
        }
        Ok((format!("http://{}/torrents/{}/stream/{idx}", self.http, handle.id()), handle.id()))
    }

    /// Drop one download's hold on torrent `id` taken by `stream_url(.., pin: true)`.
    pub async fn unpin(&self, id: usize) {
        let mut p = self.pinned.lock().await;
        if let Some(i) = p.iter().position(|&x| x == id) {
            p.remove(i);
        }
    }

    /// Stop every torrent but `keep` and pinned ones, so none keeps downloading or seeding. Stopped torrents stay on disk
    /// while they fit in the cache; the oldest are deleted with their files first.
    pub async fn release_except(&self, keep: Option<usize>) {
        let pinned = self.pinned.lock().await;
        let mut seeding = self.seeding.lock().unwrap().clone();
        let stopped: Vec<_> = self.session.with_torrents(|it| it.filter(|(id, _)| Some(*id) != keep && !pinned.contains(id)).map(|(id, h)| (id, h.clone())).collect());
        // A finished torrent keeps seeding under the limit; its files stay, so it is not evicted.
        let keep_seeding = |id: usize, h: &Arc<librqbit::ManagedTorrent>, seeding: &mut std::collections::HashMap<usize, Instant>| {
            let st = h.stats();
            if !self.seed.enabled() || !st.finished || h.is_paused() && !seeding.contains_key(&id) {
                return false;
            }
            !self.seed.reached(st.uploaded_bytes, st.total_bytes, seeding.entry(id).or_insert_with(Instant::now).elapsed())
        };
        let stopped: Vec<_> = stopped.into_iter().filter(|(id, h)| !keep_seeding(*id, h, &mut seeding)).collect();
        seeding.retain(|id, _| !stopped.iter().any(|(s, _)| s == id));
        *self.seeding.lock().unwrap() = seeding;
        for (_, h) in stopped.iter().filter(|(_, h)| !h.is_paused()) {
            if let Err(e) = self.session.pause(h).await {
                eprintln!("pause torrent {}: {e:#}", h.id());
            }
        }
        // shortcut: counts downloaded bytes, not on-disk size (sparse files make them close); use file lengths if they drift.
        let sizes: Vec<(usize, u64)> = stopped.iter().map(|(id, h)| (*id, h.stats().progress_bytes)).collect();
        for id in evict(&sizes, self.cache) {
            if let Err(e) = self.session.delete(id.into(), true).await {
                eprintln!("delete torrent {id}: {e:#}");
            }
        }
    }
}

/// A magnet with any `so` key already names its file, so the picker is skipped. Any value counts,
/// since the picker appends its own `so=` and a BEP 53 range (`so=1-3`) would duplicate it.
pub fn magnet_chooses_file(magnet: &str) -> bool {
    url::Url::parse(magnet).is_ok_and(|u| u.query_pairs().any(|(k, _)| k == "so"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_with_so_skips_picker() {
        assert!(magnet_chooses_file("magnet:?xt=urn:btih:abc&so=2"));
        assert!(magnet_chooses_file("magnet:?xt=urn:btih:abc&so=1-3"), "BEP 53 range also counts");
        assert!(!magnet_chooses_file("magnet:?xt=urn:btih:abc&dn=x"));
    }

    #[test]
    fn media_files_filters_and_sorts_largest_first() {
        let f = |n: &str, l| (n.to_string(), l);
        let files = [f("a/readme.txt", 9), f("a/small.MKV", 5), f("a/big.mp4", 50), f("a/sample.mkv", 5), f("noext", 99)];
        assert_eq!(media_files(&files), vec![(2, "a/big.mp4".into(), 50), (1, "a/small.MKV".into(), 5), (3, "a/sample.mkv".into(), 5)]);
    }

    #[test]
    fn evicts_oldest_until_cache_fits() {
        let t = [(3, 30), (1, 10), (2, 20)];
        assert_eq!(evict(&t, 0), [1, 2, 3], "no cache: delete all");
        assert_eq!(evict(&t, 45), [1, 2], "oldest first");
        assert_eq!(evict(&t, 50), [1]);
        assert!(evict(&t, 60).is_empty() && evict(&t, u64::MAX).is_empty());
        assert_eq!(evict(&[(1, 0)], 0), [1], "empty torrents go too");
        assert_eq!((Profile::from_label("Fast"), Profile::from_label("junk")), (Profile::Fast, Profile::Default));
    }

    #[test]
    fn seed_limit_reached_by_ratio_or_time() {
        let l = SeedLimit { ratio: 1.0, secs: 60 };
        let s = Duration::from_secs;
        assert!(!l.reached(99, 100, s(59)) && l.reached(100, 100, s(0)) && l.reached(0, 100, s(60)));
        assert!(!SeedLimit { ratio: 0.0, secs: 0 }.reached(u64::MAX, 1, s(u64::MAX)), "no limit never trips");
        assert!(!SeedLimit { ratio: 2.0, secs: 0 }.reached(150, 100, s(9999)));
    }

    #[tokio::test]
    async fn pinned_torrent_survives_release() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.mkv"), vec![7u8; 64 * 1024]).unwrap();
        let t = librqbit::create_torrent(&src.join("a.mkv"), Default::default(), &librqbit::spawn_utils::BlockingSpawner::new(1)).await.unwrap();
        let file = dir.path().join("a.torrent");
        std::fs::write(&file, t.as_bytes().unwrap()).unwrap();
        let engine = TorrentEngine::start(dir.path().join("dl"), false, u64::MAX, Profile::Default, Default::default()).await.unwrap();
        let (_, id) = engine.stream_url(file.to_str().unwrap(), None, true).await.unwrap();
        let paused = || engine.session.get(id.into()).unwrap().is_paused();
        engine.release_except(None).await;
        assert!(!paused(), "pinned: kept running");
        engine.unpin(id).await;
        engine.release_except(None).await;
        assert!(paused(), "unpinned: stopped");
        assert_eq!(engine.list().len(), 1);
        engine.remove(id).await;
        assert!(engine.list().is_empty());
    }
}
