use stremflix_rs::addon::*;

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
fn genre_extra() {
    let m = parse_manifest(br#"{"id":"a","version":"1","name":"n","resources":["catalog"],"types":["movie"],"catalogs":[
      {"type":"movie","id":"g","extra":[{"name":"genre","isRequired":true,"options":["Action"," Sci\u0007-Fi ","",7,"Action"]},{"name":"skip"}]},
      {"type":"movie","id":"opt","extra":[{"name":"genre","options":"bad"}]},
      {"type":"movie","id":"none","extra":[{"name":"genre","isRequired":true}]},
      {"type":"movie","id":"s","extra":[{"name":"search","isRequired":true}]},
      {"type":"movie","id":"legacy","extraRequired":["genre"]}]}"#).unwrap();
    let c = &m.catalogs;
    assert_eq!(c[0].extra[0].options, ["Action", "Sci-Fi", "Action"], "sanitized, empty and non-string options dropped");
    assert!(c[0].discoverable() && c[0].pickers().count() == 1);
    assert!(c[1].pickers().count() == 0 && c[1].discoverable());
    assert!(!c[2].discoverable(), "required genre without options");
    assert!(!c[3].discoverable(), "required extra without options");
    assert!(!c[4].discoverable());
    let many: Vec<String> = (0..500).map(|i| format!("\"g{i}\"")).collect();
    let body = format!(r#"{{"id":"a","version":"1","name":"n","resources":["catalog"],"types":["movie"],"catalogs":[{{"type":"movie","id":"g","extra":[{{"name":"genre","options":[{}]}}]}}]}}"#, many.join(","));
    assert_eq!(parse_manifest(body.as_bytes()).unwrap().catalogs[0].extra[0].options.len(), 200);
}

#[test]
fn discover_url() {
    let x = |v: &[(&str, &str)]| v.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect::<Vec<_>>();
    assert_eq!(discover_path("movie", "top", &[]), "catalog/movie/top.json");
    assert_eq!(discover_path("movie", "top", &x(&[("genre", "Sci-Fi & Fantasy")])), "catalog/movie/top/genre=Sci-Fi%20%26%20Fantasy.json");
    assert_eq!(discover_path("movie", "top", &x(&[("genre", "a/b=c?")])), "catalog/movie/top/genre=a%2Fb%3Dc%3F.json");
    assert_eq!(discover_path("movie", "top", &x(&[("year", "2020"), ("sort", "a&b")])), "catalog/movie/top/year=2020&sort=a%26b.json");
    let base = addon_base("https://x.example/a/manifest.json").unwrap();
    assert_eq!(base.join(&discover_path("series", "top", &x(&[("genre", "a/../b")]))).unwrap().as_str(), "https://x.example/a/catalog/series/top/genre=a%2F..%2Fb.json");
}

#[test]
fn valid_extras() {
    let m = parse_manifest(br#"{"id":"a","version":"1","name":"n","resources":["catalog"],"types":["movie"],"catalogs":[
      {"type":"movie","id":"c","extra":[{"name":"genre","options":["A","B"]},{"name":"year","isRequired":true,"options":["2020","2021"]},{"name":"skip"}]}]}"#).unwrap();
    let c = &m.catalogs[0];
    let x = |v: &[(&str, &str)]| v.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect::<Vec<_>>();
    assert!(c.discoverable() && c.pickers().count() == 2, "skip has no options");
    assert_eq!(c.valid_extras(&[]), x(&[("year", "2020")]), "required defaults to first option, optional stays unset");
    assert_eq!(c.valid_extras(&x(&[("year", "2021"), ("genre", "B")])), x(&[("genre", "B"), ("year", "2021")]));
    assert_eq!(c.valid_extras(&x(&[("genre", "Z"), ("year", "1999"), ("x", "1")])), x(&[("year", "2020")]), "stale values dropped");
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
fn meta_trailers() {
    let m = parse_meta(br#"{"meta": {"id": "tt1", "type": "movie", "name": "One", "trailerStreams": [
        {"title": "Official", "ytId": "dQw4w9WgXcQ", "url": "https://evil.example/x"},
        {"title": "Bad", "ytId": "../../etc"}, {"title": "Torrent", "infoHash": "0123456789abcdef0123456789abcdef01234567"}, 3
    ], "trailers": [{"source": "aaaaaaaaaaa", "type": "Trailer"}]}}"#)
    .unwrap();
    assert_eq!(m.trailers.len(), 1, "invalid ids, non-YouTube and malformed entries dropped; legacy ignored");
    assert_eq!(m.trailers[0].source().as_deref(), Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), "url stripped");
    assert_eq!(m.trailers[0].title.as_deref(), Some("Official"));
    let m = parse_meta(br#"{"meta": {"id": "tt1", "type": "movie", "name": "One", "trailers": [{"source": "aaaaaaaaaaa", "type": "Trailer"}, {"source": 5}]}}"#).unwrap();
    assert_eq!((m.trailers.len(), m.trailers[0].yt_id.as_deref()), (1, Some("aaaaaaaaaaa")), "legacy fallback");
    assert!(parse_meta(br#"{"meta": {"id": "tt1", "type": "movie", "name": "One", "trailers": "x"}}"#).unwrap().trailers.is_empty());
    let m = parse_meta(br#"{"meta": {"id": "tt1", "type": "movie", "name": "One", "trailerStreams": [{"ytId": "bad"}], "trailers": [{"source": "aaaaaaaaaaa"}]}}"#).unwrap();
    assert_eq!(m.trailers.len(), 1, "legacy used when every trailerStreams entry is invalid");
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
    assert_eq!(m.videos[1].title.as_deref(), Some("Special"));
    assert_eq!(v.thumbnail.as_deref(), Some("https://img.example/e.jpg"));
    assert_eq!(m.videos[1].thumbnail, None, "non-http thumbnail dropped");
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
    // Base32 (32 chars, uppercase as librqbit reads it) is a valid info hash too; other lengths are not.
    let b32 = "MFRGGZDFMZTWQ2LKNNWG23TPOBYXE43U";
    assert_eq!(parse_magnet_hash(&format!("magnet:?xt=urn:btih:{b32}")).as_deref(), Some(b32));
    assert_eq!(parse_magnet_hash(&format!("magnet:?xt=urn:btih:{}", b32.to_lowercase())), None);
    assert_eq!(parse_magnet_hash("magnet:?xt=urn:btih:MFRGGZDFMZTWQ2LKNNWG23TPOBYXE43"), None);
    assert_eq!(parse_magnet_hash("magnet:?xt=urn:btih:MFRGGZDFMZTWQ2LKNNWG23TPOBYXE431"), None, "1 is not base32");
}

#[test]
fn external_and_youtube_streams() {
    let body = br#"{"streams": [
        {"externalUrl": "https://site.example/watch", "title": "Web"},
        {"externalUrl": "stremio:///detail/movie/tt1"},
        {"externalUrl": "javascript:alert(1)"},
        {"ytId": "dQw4w9WgXcQ", "title": "Trailer"},
        {"ytId": "bad/id&x=1"},
        {"ytId": "dQw4w9WgXcQ", "url": "https://cdn.example/v.mp4"},
        {"ytId": "dQw4w9WgXcQ", "url": "ftp://cdn.example/v.mp4"}
    ]}"#;
    let s = parse_streams(body).unwrap();
    assert_eq!(s.len(), 4, "non-http external URLs and bad ytIds dropped");
    assert_eq!(s[3].source().as_deref(), Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), "unplayable url falls back to ytId");
    assert_eq!((s[0].external_url.as_deref(), s[0].target()), (Some("https://site.example/watch"), None));
    let yt = s[1].source().unwrap();
    assert_eq!(yt, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    assert!(is_youtube(&yt));
    assert_eq!(s[2].source().as_deref(), Some("https://cdn.example/v.mp4"), "url wins over ytId");
    for not in ["https://cdn.example/v.mp4", "https://www.youtube.com/watch?v=dQw4w9WgXcQ&x=1", "https://www.youtube.com/watch?v=short"] {
        assert!(!is_youtube(not), "{not}");
    }
}

#[test]
fn stream_binge_group() {
    let long = "g".repeat(1000);
    let body = format!(
        r#"{{"streams": [
        {{"url": "https://a.example/1", "behaviorHints": {{"bingeGroup": " addon-1080p\u0007 "}}}},
        {{"url": "https://a.example/2", "behaviorHints": {{"bingeGroup": 7}}}},
        {{"url": "https://a.example/3", "behaviorHints": "junk"}},
        {{"url": "https://a.example/4", "behaviorHints": {{"bingeGroup": "{long}"}}}},
        {{"url": "https://a.example/5", "behaviorHints": {{"bingeGroup": "  "}}}},
        {{"url": "https://a.example/6"}}
    ]}}"#
    );
    let s = parse_streams(body.as_bytes()).unwrap();
    let groups: Vec<_> = s.iter().map(|s| s.hints.binge_group.as_deref().map(str::len)).collect();
    assert_eq!(s[0].hints.binge_group.as_deref(), Some("addon-1080p"));
    assert_eq!(groups, [Some(11), None, None, Some(300), None, None]);
}

#[test]
fn stream_behavior_hints() {
    let many: Vec<String> = (0..40).map(|i| format!(r#""X-H{i:02}": "v""#)).collect();
    let body = format!(
        r#"{{"streams": [
        {{"url": "https://a.example/1", "behaviorHints": {{"filename": " Movie.2024\n.1080p.mkv\u0007", "videoSize": 2345000000, "videoHash": "8E245D9679D31E12",
            "proxyHeaders": {{"request": {{"User-Agent": " Moz (KHTML, like Gecko) ", "Referer": "https://a.example/",
                "X-Inject": "a\r\nX-Evil: 1", "X-Nul": "a\u0000b", "Bad Name": "v", "Bad:Name": "v", "": "v", "Host": "evil.example",
                "Content-Length": "0", "X-Num": 7, "X-Uni": "café", "X-Long": "{long}"}}}}}}}},
        {{"url": "https://a.example/2", "behaviorHints": {{"videoSize": -1, "filename": 3, "videoHash": "../../x.json", "proxyHeaders": {{"request": "junk"}}}}}},
        {{"url": "https://a.example/3", "behaviorHints": {{"videoHash": "8e245d9679d31e1g", "proxyHeaders": {{"request": {{{many}}}}}}}}}
    ]}}"#,
        long = "x".repeat(5000),
        many = many.join(",")
    );
    let s = parse_streams(body.as_bytes()).unwrap();
    let h = &s[0].hints;
    assert_eq!(h.filename.as_deref(), Some("Movie.2024 .1080p.mkv"));
    assert_eq!(h.video_size, Some(2_345_000_000));
    assert_eq!((h.video_hash.as_deref(), &s[1].hints.video_hash), (Some("8e245d9679d31e12"), &None));
    let names: Vec<_> = h.request_headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["Referer", "User-Agent"]);
    assert_eq!(h.request_headers[1].1, "Moz (KHTML, like Gecko)");
    assert_eq!((s[1].hints.video_size, &s[1].hints.filename, s[1].hints.request_headers.len()), (None, &None, 0));
    assert_eq!(s[2].hints.request_headers.len(), 32);
    assert_eq!(s[2].hints.video_hash, None, "16 chars but not hex");
    assert!(valid_header("X-Debrid-Token", "abc, def;q=1") && !valid_header("X-A", "a\nb") && !valid_header("TE", "trailers"));
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

#[test]
fn update_check() {
    let rel = br#"{"tag_name":"v1.2.0","target_commitish":"abc","html_url":"https://github.com/o/r/releases/tag/v1.2.0"}"#;
    assert_eq!(newer_release("v1.1.0", "zzz", rel).unwrap().as_deref(), Some("https://github.com/o/r/releases/tag/v1.2.0"));
    assert_eq!(newer_release("v1.2.0", "zzz", rel).unwrap(), None);
    // an older latest tag is not offered as an update
    assert_eq!(newer_release("v1.3.0-beta", "zzz", rel).unwrap(), None);
    assert!(newer_release("v1.10.0", "zzz", br#"{"tag_name":"v1.9.0","target_commitish":"a","html_url":"https://x.io/"}"#).unwrap().is_none());
    let sha = "a".repeat(40);
    let nightly = format!(r#"{{"tag_name":"nightly","target_commitish":"{sha}","html_url":"https://github.com/o/r/releases/tag/nightly"}}"#);
    assert_eq!(newer_release("nightly", &sha, nightly.as_bytes()).unwrap(), None);
    assert!(newer_release("nightly", "def", nightly.as_bytes()).unwrap().is_some());
    let branch = br#"{"tag_name":"nightly","target_commitish":"main","html_url":"https://github.com/o/r/releases/tag/nightly"}"#;
    assert_eq!(newer_release("nightly", "def", branch).unwrap(), None);
    let bad_url = br#"{"tag_name":"v2","target_commitish":"a","html_url":"javascript:alert(1)"}"#;
    assert_eq!(newer_release("v1", "a", bad_url).unwrap(), None);
    assert!(newer_release("v1", "a", b"{}").is_err());
}

#[test]
fn addon_catalog() {
    let m = parse_manifest(br#"{"id":"a","version":"1","name":"n","resources":["addon_catalog"],"types":["all"],
      "addonCatalogs":[{"type":"all","id":"community","name":" Community\u0007"},{"type":"all","id":"a/b"},{"id":"no-type"},null]}"#).unwrap();
    assert_eq!(m.addon_catalogs.len(), 1, "invalid and malformed entries dropped");
    let junk = br#"{"id":"a","version":"1","name":"n","resources":["stream"],"types":["movie"],"addonCatalogs":null}"#;
    assert!(parse_manifest(junk).unwrap().addon_catalogs.is_empty(), "a malformed field never fails the manifest");
    assert_eq!(m.addon_catalogs[0].name.as_deref(), Some("Community"));
    // Survives the DB round trip (manifests are stored as serialized JSON).
    assert_eq!(parse_manifest(serde_json::to_string(&m).unwrap().as_bytes()).unwrap(), m);

    let ok = r#"{"id":"x","version":"1","name":"X","resources":["stream"],"types":["movie"]}"#;
    let body = format!(r#"{{"addons":[
      {{"transportName":"http","transportUrl":" https://x.exa\tmple/manifest.json ","manifest":{ok}}},
      {{"transportUrl":"file:///etc/manifest.json","manifest":{ok}}},
      {{"transportUrl":"https://y.example/other.json","manifest":{ok}}},
      {{"transportUrl":"https://z.example/manifest.json","manifest":{{"id":"z"}}}},
      {{"manifest":{ok}}}, 7]}}"#);
    let list = parse_addon_catalog(body.as_bytes()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].transport_url, "https://x.example/manifest.json");
    assert_eq!(list[0].manifest.name, "X");
    assert!(parse_addon_catalog(b"{}").is_err());
}

#[test]
fn deep_links() {
    let addon = |s: &str| match parse_deep_link(s) {
        Some(DeepLink::Addon(u)) => Some(u.to_string()),
        _ => None,
    };
    assert_eq!(addon("stremio://v3-cinemeta.strem.io/manifest.json").as_deref(), Some("https://v3-cinemeta.strem.io/manifest.json"));
    assert_eq!(addon("stremio://127.0.0.1:7000/cfg/manifest.json").as_deref(), Some("http://127.0.0.1:7000/cfg/manifest.json"));
    assert_eq!(addon("stremio://localhost:7000/manifest.json").as_deref(), Some("http://localhost:7000/manifest.json"));
    assert_eq!(addon("STREMIO://localhost:7000/manifest.json").as_deref(), Some("http://localhost:7000/manifest.json"));
    assert_eq!(addon("stremio://example.com/other.json"), None);
    assert_eq!(addon("https://example.com/manifest.json"), None);
    assert_eq!(addon("stremio://example.com/manifest.json?x=1"), None);
    assert_eq!(addon("stremio://example.com/manifest.json#y"), None);
    assert_eq!(
        parse_deep_link("stremio:///detail/series/tt0903747/tt0903747%3A1%3A1"),
        Some(DeepLink::Detail { kind: "series".into(), id: "tt0903747".into(), video: Some("tt0903747:1:1".into()) })
    );
    assert_eq!(parse_deep_link("stremio:///detail/movie/tt1"), Some(DeepLink::Detail { kind: "movie".into(), id: "tt1".into(), video: None }));
    for bad in ["stremio:///detail/movie", "stremio:///detail/movie/a%2Fb", "stremio:///detail/movie/x%00", "stremio:///detail/movie/a%E2%80%AEb", "stremio:///search?q=x", "stremio:///detail/movie/tt1/v/extra"] {
        assert_eq!(parse_deep_link(bad), None, "{bad}");
    }
}

#[test]
fn opensubtitles_hash_and_subtitle_extras() {
    let zero = vec![0u8; HASH_CHUNK];
    assert_eq!(opensubtitles_hash(131_072, &zero, &zero), "0000000000020000");
    let mut max = zero.clone();
    max[..8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(opensubtitles_hash(131_072, &max, &zero), "000000000001ffff", "wraps");
    assert_eq!(subtitles_path("movie", "tt1", &[]), "subtitles/movie/tt1.json");
    let extra = [("videoHash", "8e245d9679d31e12".to_string()), ("videoSize", "12".to_string()), ("filename", "a b&c=/../d.mkv".to_string())];
    assert_eq!(subtitles_path("series", "tt1:1:2", &extra), "subtitles/series/tt1%3A1%3A2/videoHash=8e245d9679d31e12&videoSize=12&filename=a%20b%26c%3D%2F..%2Fd.mkv.json");
}
