use stremio_lite::addon::*;

const MANIFEST: &str = r#"{
  "id": "org.example.x", "version": "1.2.3", "name": "  Example\u0007 ",
  "description": "desc", "resources": ["catalog", {"name": "stream", "types": ["movie"], "idPrefixes": ["tt"]}],
  "types": ["movie"], "catalogs": [{"type": "movie", "id": "top", "name": "Top"}, {"type": "movie", "id": ""}]
}"#;

#[test]
fn valid_manifest() {
    let m = parse_manifest(MANIFEST.as_bytes()).unwrap();
    assert_eq!(m.id, "org.example.x");
    assert_eq!(m.name, "Example"); // control char stripped, trimmed
    assert!(m.has_resource("catalog") && m.has_resource("stream") && !m.has_resource("meta"));
    assert_eq!(m.catalogs.len(), 1, "catalog with empty id dropped");
    assert!(!m.catalogs[0].supports("search") && !m.catalogs[0].requires_extra());
}

#[test]
fn catalog_extras() {
    let m = parse_manifest(br#"{"id":"a","version":"1","name":"n","resources":["catalog"],"types":["movie"],"catalogs":[
      {"type":"movie","id":"s","extra":[{"name":"search","isRequired":true}]},
      {"type":"movie","id":"legacy","extraSupported":["search","genre"]}]}"#).unwrap();
    assert!(m.catalogs[0].supports("search") && m.catalogs[0].requires_extra());
    assert!(m.catalogs[1].supports("search") && m.catalogs[1].supports("genre") && !m.catalogs[1].requires_extra());
}

#[test]
fn malformed_manifests_rejected() {
    for bad in [
        "",
        "null",
        "[]",
        "{",
        r#"{"id": 42}"#,
        r#"{"id":"a","version":"1","name":"n","resources":[],"types":["movie"]}"#,
        r#"{"id":"","version":"1","name":"n","resources":["stream"],"types":["movie"]}"#,
        r#"{"id":"a/b","version":"1","name":"n","resources":["stream"],"types":["movie"]}"#,
        r#"{"id":"a","version":"1","name":"\u0001","resources":["stream"],"types":["movie"]}"#,
        r#"{"id":"a","version":"1","name":"n","resources":["stream"],"types":[]}"#,
    ] {
        assert!(parse_manifest(bad.as_bytes()).is_err(), "should reject: {bad}");
    }
    let huge = vec![b' '; MAX_BODY + 1];
    assert!(parse_manifest(&huge).is_err());
}

#[test]
fn catalog_parsing_drops_bad_entries() {
    let body = r#"{"metas": [
        {"id": "tt1", "type": "movie", "name": "One", "poster": "https://img.example/1.jpg"},
        {"id": "tt2", "type": "movie", "name": "Two", "poster": "javascript:alert(1)"},
        {"id": "tt3", "type": "movie"},
        {"id": "", "type": "movie", "name": "Empty id"},
        42
    ]}"#;
    let metas = parse_catalog(body.as_bytes()).unwrap();
    assert_eq!(metas.len(), 2);
    assert_eq!(metas[0].poster.as_deref(), Some("https://img.example/1.jpg"));
    assert_eq!(metas[1].poster, None, "unsafe poster URL removed");
    assert!(parse_catalog(br#"{"metas": {}}"#).is_err());
}

#[test]
fn meta_parsing() {
    let m = parse_meta(r#"{"meta": {"id": "tt1", "type": "movie", "name": "One", "releaseInfo": "2020", "description": "a\u202Eb"}}"#.as_bytes()).unwrap();
    assert_eq!((m.id.as_str(), m.release_info.as_deref()), ("tt1", Some("2020")));
    assert_eq!(m.description.as_deref(), Some("ab"), "bidi override stripped");
    assert!(parse_meta(br#"{"meta": null}"#).is_err());
    assert!(parse_meta(br#"{"meta": {"id": "tt1", "type": "movie", "name": ""}}"#).is_err());
}

#[test]
fn stream_parsing_and_targets() {
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let body = format!(
        r#"{{"streams": [
        {{"url": "https://cdn.example/v.mp4", "title": "HTTP"}},
        {{"infoHash": "{hash}", "fileIdx": 1, "sources": ["tracker:http://tr.example/announce", "peer:127.0.0.1:6881", "dht:{hash}"]}},
        {{"url": "file:///etc/passwd"}},
        {{"url": "javascript:alert(1)"}},
        {{"infoHash": "nothex"}},
        {{"title": "no source"}},
        {{"url": "magnet:?xt=urn:btih:{hash}"}}
    ]}}"#
    );
    let s = parse_streams(body.as_bytes()).unwrap();
    assert_eq!(s.len(), 3);
    assert!(matches!(s[0].target(), Some(PlayTarget::Http(u)) if u.as_str() == "https://cdn.example/v.mp4"));
    let Some(PlayTarget::Magnet(m, Some(1))) = s[1].target() else { panic!("expected magnet") };
    assert_eq!(parse_magnet_hash(&m).as_deref(), Some(hash));
    assert!(m.contains("&tr=http%3A%2F%2Ftr.example%2Fannounce"));
    assert_eq!(magnet_peers(&m), vec!["127.0.0.1:6881".parse().unwrap()]);
    assert!(matches!(s[2].target(), Some(PlayTarget::Magnet(_, None))));
}

#[test]
fn subtitle_parsing() {
    let s = parse_subtitles(br#"{"subtitles": [
        {"id": "1", "url": "https://subs.example/en.vtt", "lang": "eng"},
        {"url": "data:text/plain,hi", "lang": "eng"},
        {"url": "https://subs.example/x.srt"}
    ]}"#)
    .unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].lang, "eng");
}

#[test]
fn malformed_responses_never_panic() {
    let samples: [&[u8]; 6] = [
        br#"{"metas": [{"id": "tt1", "type": "movie", "name": "One"}]}"#,
        br#"{"meta": {"id": "tt1", "type": "movie", "name": "One"}}"#,
        br#"{"streams": [{"url": "https://a.example/v.mp4"}]}"#,
        br#"{"subtitles": [{"url": "https://a.example/s.vtt", "lang": "en"}]}"#,
        MANIFEST.as_bytes(),
        b"\xff\xfe\x00garbage",
    ];
    // Every truncation of every sample must return Ok/Err, never panic.
    for s in samples {
        for n in 0..=s.len() {
            let b = &s[..n];
            let _ = parse_manifest(b);
            let _ = parse_catalog(b);
            let _ = parse_meta(b);
            let _ = parse_streams(b);
            let _ = parse_subtitles(b);
        }
    }
    assert!(parse_streams(b"not json").is_err());
    assert!(parse_catalog(br#"{"streams": []}"#).is_err());
}

#[test]
fn url_safety() {
    assert!(safe_http_url("https://a.example/x").is_some());
    for bad in ["file:///x", "javascript:alert(1)", "data:,x", "ftp://a/x", "http://", "/relative"] {
        assert!(safe_http_url(bad).is_none(), "{bad}");
    }
    assert_eq!(addon_base("https://a.example/sub/manifest.json").unwrap().as_str(), "https://a.example/sub/");
    assert!(addon_base("https://a.example/sub/other.json").is_err());
    assert!(addon_base("file:///manifest.json").is_err());
}
