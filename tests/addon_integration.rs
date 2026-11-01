//! Starts the bundled test addon and exercises the real addon client against it.
use stremio_lite::addon::{AddonClient, PlayTarget};

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

    let metas = c.catalog(&url, &cat.kind, &cat.id).await.unwrap();
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

    let subs = c.subtitles(&url, "movie", &meta.id).await.unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].url, format!("{base}/subs/test-en.vtt"));

    // Malformed and missing resources are errors, not panics.
    assert!(c.fetch_manifest(&format!("{base}/malformed/manifest.json")).await.is_err());
    assert!(c.catalog(&url, "movie", "nope").await.is_err());
    assert!(c.fetch_manifest(&format!("{base}/manifest.txt")).await.is_err());
}
