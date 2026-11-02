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

pub struct TorrentEngine {
    session: Arc<Session>,
    http: SocketAddr,
}

impl TorrentEngine {
    /// Must be called inside a tokio runtime. `dht=false` is for offline/CI use.
    /// Clears `download_dir` first: without session persistence, leftover files belong to no torrent.
    pub async fn start(download_dir: PathBuf, dht: bool) -> Result<Self> {
        let _ = std::fs::remove_dir_all(&download_dir);
        std::fs::create_dir_all(&download_dir)?;
        let mut opts = SessionOptions { persistence: None, fastresume: false, ..Default::default() };
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
        Ok(Self { session, http })
    }

    /// Add a magnet and return a local HTTP URL streaming the chosen (or largest) file, and the torrent's id.
    pub async fn stream_url(&self, magnet: &str, file_idx: Option<usize>) -> Result<(String, usize)> {
        parse_magnet_hash(magnet).context("invalid magnet link")?;
        let peers = magnet_peers(magnet);
        // `so=<idx>` (BEP 53 select-only) carries the addon's fileIdx through saved progress.
        let file_idx = file_idx.or_else(|| {
            url::Url::parse(magnet).ok()?.query_pairs().find(|(k, _)| k == "so").and_then(|(_, v)| v.parse().ok())
        });
        let opts = AddTorrentOptions {
            overwrite: true,
            initial_peers: (!peers.is_empty()).then_some(peers),
            ..Default::default()
        };
        // A magnet waits for its metadata from peers; with none, it would wait forever.
        let added = tokio::time::timeout(METADATA_TIMEOUT, self.session.add_torrent(AddTorrent::from_url(magnet), Some(opts)))
            .await
            .map_err(|_| anyhow::anyhow!("no peers sent the torrent's metadata within {}s", METADATA_TIMEOUT.as_secs()))??;
        let handle = match added {
            AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => bail!("unexpected list-only response"),
        };
        let lengths: Vec<u64> = handle.with_metadata(|m| m.file_infos.iter().map(|f| f.len).collect())?;
        let idx = match file_idx {
            Some(i) if i < lengths.len() => i,
            Some(i) => bail!("fileIdx {i} out of range ({} files)", lengths.len()),
            None => (0..lengths.len()).max_by_key(|&i| lengths[i]).context("torrent has no files")?,
        };
        Ok((format!("http://{}/torrents/{}/stream/{idx}", self.http, handle.id()), handle.id()))
    }

    /// Stop and delete every torrent but `keep`, with its files, so none keeps downloading or seeding.
    pub async fn release_except(&self, keep: Option<usize>) {
        let ids: Vec<usize> = self.session.with_torrents(|it| it.map(|(id, _)| id).filter(|&id| Some(id) != keep).collect());
        for id in ids {
            if let Err(e) = self.session.delete(id.into(), true).await {
                eprintln!("delete torrent {id}: {e:#}");
            }
        }
    }
}
