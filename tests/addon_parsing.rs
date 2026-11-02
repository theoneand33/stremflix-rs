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
    assert!(m.catalogs[0].searchable() && m.catalogs[1].searchable());
    let m = parse_manifest(br#"{"id":"a","version":"1","name":"n","resources":["catalog"],"types":["movie"],"catalogs":[
      {"type":"movie","id":"legacy","extraRequired":["search"]},
      {"type":"movie","id":"genre","extra":[{"name":"search"},{"name":"genre","isRequired":true}]}]}"#).unwrap();
    assert!(m.catalogs[0].searchable() && m.catalogs[0].requires_extra());
    assert!(m.catalogs[1].supports("search") && !m.catalogs[1].searchable());
}

#[test]
fn poster_dimension_limits() {
    let png = |w, h| {
        let mut b = Vec::new();
        image::RgbImage::new(w, h).write_to(&mut std::io::Cursor::new(&mut b), image::ImageFormat::Png).unwrap();
        b
    };
    assert!(check_poster(&png(240, 360)).is_ok());
    let wide = png(MAX_POSTER_SIDE + 1, 1);
    assert!(wide.len() < 4096, "tiny file, huge dimensions");
    assert!(check_poster(&wide).is_err());
    assert!(check_poster(b"<html>not an image</html>").is_err());
    assert!(check_poster(b"").is_err());
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
        {"id": "tt2", "type": "movie", "name": "Two", "poster": "javascript:alert(1)", "background": "file:///etc/passwd"},
        {"id": "tt3", "type": "movie"},
        {"id": "", "type": "movie", "name": "Empty id"},
        42
    ]}"#;
    let metas = parse_catalog(body.as_bytes()).unwrap();
    assert_eq!(metas.len(), 2);
    assert_eq!(metas[0].poster.as_deref(), Some("https://img.example/1.jpg"));
    assert_eq!(metas[1].poster, None, "unsafe poster URL removed");
    assert_eq!(metas[1].background, None, "unsafe background URL removed");
    assert!(parse_catalog(br#"{"metas": {}}"#).is_err());
}

#[test]
fn meta_parsing() {
    let m = parse_meta(r#"{"meta": {"id": "tt1", "type": "movie", "name": "One", "releaseInfo": "2020", "description": "a\u202Eb"}}"#.as_bytes()).unwrap();
    assert_eq!((m.id.as_str(), m.release_info.as_deref()), ("tt1", Some("2020")));
    assert_eq!(m.description.as_deref(), Some("ab"), "bidi override stripped");
    assert!(parse_meta(br#"{"meta": null}"#).is_err());
    assert!(parse_meta(br#"{"meta": {"id": "tt1", "type": "movie", "name": ""}}"#).is_err());
    assert!(m.videos.is_empty());
}

#[test]
fn meta_detail_fields() {
    let m = parse_meta(br#"{"meta": {"id": "tt1", "type": "movie", "name": "One",
        "genres": ["Drama", " ", "Sci\u0007-Fi", 7], "cast": ["A", "B"], "director": ["D"],
        "runtime": "118 min", "imdbRating": 7.8, "background": "https://img.example/bg.jpg", "logo": "file:///etc/passwd",
        "links": [
            {"name": "7.8", "category": "imdb", "url": "https://imdb.com/title/tt1"},
            {"name": "Drama", "category": "Genres", "url": "stremio:///discover/x"},
            {"name": "", "category": "x", "url": "https://a.example/"},
            {"category": "x", "url": "https://a.example/"},
            "junk"
        ]}}"#)
    .unwrap();
    assert_eq!(m.genres, ["Drama", "Sci-Fi"]);
    assert_eq!((m.cast.len(), m.director.as_slice()), (2, ["D".to_string()].as_slice()));
    assert_eq!((m.runtime.as_deref(), m.imdb_rating.as_deref()), (Some("118 min"), Some("7.8")));
    assert_eq!((m.background.as_deref(), m.logo.as_deref()), (Some("https://img.example/bg.jpg"), None));
    assert_eq!(m.links.len(), 1, "non-http, nameless and malformed links dropped");
    assert_eq!((m.links[0].category.as_str(), m.links[0].url.as_str()), ("imdb", "https://imdb.com/title/tt1"));
    // Legacy `genre`, wrong types and oversized lists never fail the meta.
    let many: Vec<String> = (0..100).map(|i| format!("\"p{i}\"")).collect();
    let body = format!(
        r#"{{"meta": {{"id": "tt1", "type": "movie", "name": "One", "genre": ["Comedy"], "cast": [{}],
        "director": "D", "links": {{}}, "runtime": [1], "imdbRating": null, "releaseInfo": 2020, "logo": 5}}}}"#,
        many.join(",")
    );
    let m = parse_meta(body.as_bytes()).unwrap();
    assert_eq!(m.genres, ["Comedy"]);
    assert_eq!(m.cast.len(), 30, "cast capped");
    assert!(m.director.is_empty() && m.links.is_empty() && m.runtime.is_none() && m.imdb_rating.is_none() && m.logo.is_none());
    assert_eq!(m.release_info.as_deref(), Some("2020"));
}

#[test]
fn meta_videos_parsing() {
    let m = parse_meta(br#"{"meta": {"id": "tt9", "type": "series", "name": "Show", "videos": [
        {"id": "tt9:1:2", "name": "Pilot\u0007 ", "season": 1, "episode": 2, "released": "2020-01-01T00:00:00.000Z", "thumbnail": "https://img.example/e.jpg"},
        {"id": "tt9:0:1", "title": "Special", "season": 0, "episode": 1, "thumbnail": "javascript:alert(1)"},
        {"id": "tt9/../x", "season": 1, "episode": 3},
        {"id": "", "season": 1, "episode": 4},
        {"id": "tt9:1:5", "season": "one"},
        {"season": 1},
        42
    ]}}"#)
    .unwrap();
    assert_eq!(m.videos.len(), 2, "invalid ids and malformed entries dropped, meta kept");
    let v = &m.videos[0];
    assert_eq!((v.id.as_str(), v.title.as_deref(), v.season, v.episode), ("tt9:1:2", Some("Pilot"), Some(1), Some(2)));
    assert_eq!(v.released.as_deref(), Some("2020-01-01T00:00:00.000Z"));
    assert_eq!(v.thumbnail.as_deref(), Some("https://img.example/e.jpg"));
    assert_eq!((m.videos[1].title.as_deref(), m.videos[1].thumbnail.as_deref()), (Some("Special"), None));
    for videos in ["null", "{}", "\"x\""] {
        let body = format!(r#"{{"meta": {{"id": "tt9", "type": "series", "name": "Show", "videos": {videos}}}}}"#);
        assert!(parse_meta(body.as_bytes()).unwrap().videos.is_empty(), "{videos}");
    }
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
fn stream_binge_group() {
    let long = "g".repeat(1000);
    let body = format!(
        r#"{{"streams": [
        {{"url": "https://a.example/1", "behaviorHints": {{"bingeGroup": " addon-1080p\u0007 ", "notWebReady": true}}}},
        {{"url": "https://a.example/2", "behaviorHints": {{"bingeGroup": 7}}}},
        {{"url": "https://a.example/3", "behaviorHints": "junk"}},
        {{"url": "https://a.example/4", "behaviorHints": {{"bingeGroup": "{long}"}}}},
        {{"url": "https://a.example/5", "behaviorHints": {{"bingeGroup": "  "}}}},
        {{"url": "https://a.example/6"}}
    ]}}"#
    );
    let s = parse_streams(body.as_bytes()).unwrap();
    let groups: Vec<_> = s.iter().map(|s| s.binge_group.as_deref().map(str::len)).collect();
    assert_eq!(s[0].binge_group.as_deref(), Some("addon-1080p"));
    assert_eq!(groups, [Some(11), None, None, Some(300), None, None]);
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
