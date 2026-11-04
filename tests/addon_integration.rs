//! Starts the bundled test addon and exercises the real addon client against it.
use stremflix_rs::addon::{AddonClient, HASH_CHUNK, PlayTarget, opensubtitles_hash};

#[tokio::test]
async fn client_against_test_addon() {
    let cfg = test_addon::Config {
        media_dir: None,
        torrent: Some(("0123456789abcdef0123456789abcdef01234567".into(), "127.0.0.1:6881".into())),
    };
    let base = test_addon::spawn(cfg).unwrap();
    let url = format!("{base}/manifest.json");
    let c = AddonClient::new();

    let m = c.fetch_manifest(&url).await.unwrap();
    assert_eq!(m.id, "org.example.testaddon");
    assert!(["catalog", "meta", "stream", "subtitles"].iter().all(|r| m.has_resource(r)));
    let cat = &m.catalogs[0];

    let metas = c.catalog(&url, &cat.kind, &cat.id, 0).await.unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].poster.as_deref(), Some(format!("{base}/media/poster.jpg").as_str()));
    assert!(cat.supports("search") && !cat.requires_extra());
    assert_eq!(c.search(&url, &cat.kind, &cat.id, "test pat").await.unwrap().len(), 1);
    assert!(c.search(&url, &cat.kind, &cat.id, "nothing").await.unwrap().is_empty());
    assert!(c.poster(&format!("{base}/media/poster.jpg")).await.is_err(), "no media_dir: 404");
    assert!(c.poster("file:///etc/passwd").await.is_err());
    let meta = c.meta(&url, "movie", &metas[0].id).await.unwrap();
    assert_eq!(meta.name, "Test Pattern");

    let streams = c.streams(&url, "movie", &meta.id).await.unwrap();
    assert_eq!(streams.len(), 2);
    assert!(matches!(streams[0].target(), Some(PlayTarget::Http(u)) if u.as_str() == format!("{base}/media/test-pattern.mp4")));
    assert!(matches!(streams[1].target(), Some(PlayTarget::Magnet(m, Some(0))) if m.contains("x.pe=127.0.0.1:6881")));

    let subs = c.subtitles(&url, "movie", &meta.id, &[]).await.unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].url, format!("{base}/subs/test-en.vtt"));

    // Hash over range requests; the test addon ignores the range end, so the client must cap each read.
    let dir = tempfile::tempdir().unwrap();
    let video: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(dir.path().join(test_addon::VIDEO_FILE), &video).unwrap();
    let media = test_addon::spawn(test_addon::Config { media_dir: Some(dir.path().into()), torrent: None }).unwrap();
    let want = opensubtitles_hash(200_000, &video[..HASH_CHUNK], &video[200_000 - HASH_CHUNK..]);
    assert_eq!(c.video_hash(&format!("{media}/media/{}", test_addon::VIDEO_FILE), &[]).await.unwrap(), (want, 200_000));
    assert!(c.video_hash(&format!("{media}/missing.mp4"), &[]).await.is_err());
    let extra = [("videoHash", "8e245d9679d31e12".to_string()), ("videoSize", "200000".to_string())];
    assert_eq!(c.subtitles(&url, "movie", &meta.id, &extra).await.unwrap().len(), 1, "extras path reaches the addon");

    // Malformed and missing resources are errors, not panics.
    assert!(c.fetch_manifest(&format!("{base}/malformed/manifest.json")).await.is_err());
    assert!(c.catalog(&url, "movie", "nope", 0).await.is_err());
    assert!(c.fetch_manifest(&format!("{base}/manifest.txt")).await.is_err());
}
