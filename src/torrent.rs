//! Torrent engine: an embedded librqbit session plus its HTTP API bound to
//! 127.0.0.1, which serves torrent files with Range support for mpv.

use anyhow::{Context, Result, bail};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Api, Session, SessionOptions};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::addon::{magnet_peers, parse_magnet_hash};

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

/// Torrent ids (oldest first, with their downloaded bytes) to delete so the rest fit in `cache` bytes.
pub fn evict(torrents: &[(usize, u64)], cache: u64) -> Vec<usize> {
    let mut total: u64 = torrents.iter().map(|t| t.1).sum();
    let mut sorted = torrents.to_vec();
    sorted.sort();
    // Ids grow with each add, so lower ids are older.
    sorted.into_iter().take_while(|&(_, b)| (cache == 0 || total > cache) && { total -= b; true }).map(|t| t.0).collect()
}

pub struct TorrentEngine {
    session: Arc<Session>,
    http: SocketAddr,
    /// Bytes of stopped torrents kept on disk for a quick replay; 0 deletes them at once.
    cache: u64,
}

impl TorrentEngine {
    /// Must be called inside a tokio runtime. `dht=false` is for offline/CI use.
    /// Clears `download_dir` first: without session persistence, leftover files belong to no torrent.
    pub async fn start(download_dir: PathBuf, dht: bool, cache: u64, profile: Profile) -> Result<Self> {
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
        Ok(Self { session, http, cache })
    }

    /// Add a magnet or local `.torrent` file and return a local HTTP URL streaming the chosen (or largest) file, and the torrent's id.
    pub async fn stream_url(&self, magnet: &str, file_idx: Option<usize>) -> Result<(String, usize)> {
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
        let handle = match added {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => bail!("unexpected list-only response"),
        };
        // A cached torrent was paused when its playback stopped.
        if handle.is_paused() {
            self.session.unpause(&handle).await?;
        }
        let lengths: Vec<u64> = handle.with_metadata(|m| m.file_infos.iter().map(|f| f.len).collect())?;
        let idx = match file_idx {
            Some(i) if i < lengths.len() => i,
            Some(i) => bail!("fileIdx {i} out of range ({} files)", lengths.len()),
            None => (0..lengths.len()).max_by_key(|&i| lengths[i]).context("torrent has no files")?,
        };
        Ok((format!("http://{}/torrents/{}/stream/{idx}", self.http, handle.id()), handle.id()))
    }

    /// Stop every torrent but `keep`, so none keeps downloading or seeding. Stopped torrents stay on disk
    /// while they fit in the cache; the oldest are deleted with their files first.
    pub async fn release_except(&self, keep: Option<usize>) {
        let stopped: Vec<_> = self.session.with_torrents(|it| it.filter(|(id, _)| Some(*id) != keep).map(|(id, h)| (id, h.clone())).collect());
        for (_, h) in stopped.iter().filter(|(_, h)| !h.is_paused()) {
            if let Err(e) = self.session.pause(h).await {
                eprintln!("pause torrent {}: {e:#}", h.id());
            }
        }
        let sizes: Vec<(usize, u64)> = stopped.iter().map(|(id, h)| (*id, h.stats().progress_bytes)).collect();
        for id in evict(&sizes, self.cache) {
            if let Err(e) = self.session.delete(id.into(), true).await {
                eprintln!("delete torrent {id}: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
