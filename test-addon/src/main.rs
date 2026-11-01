//! test-addon [--port 7070] [--media-dir DIR] [--seed]
//! With --seed, also seeds DIR/test-pattern.mp4 over BitTorrent (librqbit, no DHT/trackers)
//! and advertises it as an infoHash stream plus a printed magnet link.

use anyhow::{Context, Result};
use std::path::PathBuf;
use test_addon::{Config, VIDEO_FILE, serve};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let port = arg("--port").unwrap_or_else(|| "7070".into());
    let media_dir = arg("--media-dir").map(PathBuf::from);
    let mut cfg = Config { media_dir: media_dir.clone(), torrent: None };

    let _session; // keep the seeder alive
    if args.iter().any(|a| a == "--seed") {
        let dir = media_dir.context("--seed requires --media-dir")?;
        let file = dir.join(VIDEO_FILE);
        let spawner = librqbit::spawn_utils::BlockingSpawner::new(2);
        let torrent = librqbit::create_torrent(&file, Default::default(), &spawner).await?;
        let hash = torrent.info_hash().as_string();
        let mut opts = librqbit::SessionOptions { persistence: None, fastresume: false, ..Default::default() };
        opts.dht = None;
        opts.disable_local_service_discovery = true;
        opts.listen = Some(librqbit::ListenerOptions {
            listen_addr: "127.0.0.1:0".parse()?,
            ..Default::default()
        });
        let session = librqbit::Session::new_with_opts(dir.clone(), opts).await?;
        let add = librqbit::AddTorrentOptions {
            overwrite: true,
            output_folder: Some(dir.to_string_lossy().into_owned()),
            disable_trackers: true,
            ..Default::default()
        };
        session.add_torrent(librqbit::AddTorrent::from_bytes(torrent.as_bytes()?), Some(add)).await?;
        let peer = session.listen_addr().context("seeder has no listen address")?;
        println!("seeding {} as magnet:?xt=urn:btih:{hash}&dn={VIDEO_FILE}&x.pe={peer}", file.display());
        cfg.torrent = Some((hash, peer.to_string()));
        _session = Some(session);
    } else {
        _session = None;
    }

    let listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}"))?;
    println!("test addon: http://{}/manifest.json", listener.local_addr()?);
    tokio::task::spawn_blocking(move || serve(listener, cfg)).await?;
    Ok(())
}
