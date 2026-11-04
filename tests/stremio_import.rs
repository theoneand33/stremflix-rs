use std::collections::HashMap;
use stremflix_rs::db::{Db, Watched};
use stremflix_rs::stremio_import::Import;
use stremflix_rs::addon::Video;
use stremflix_rs::stremio_import::{from_account, import, parse, read_storage, watched_episodes, watched_videos};

/// A Stremio 5 localStorage snapshot: 2 addons (1 invalid), a movie in progress, a watched series,
/// a watched movie, and an item with a `javascript:` poster.
fn storage() -> HashMap<String, String> {
    let manifest = r#"{"id":"a","version":"1","name":"Addon A","resources":["stream"],"types":["movie"]}"#;
    let profile = format!(
        r#"{{"addons":[{{"transportUrl":"https://a.example/manifest.json","manifest":{manifest}}},
                       {{"transportUrl":"file:///etc/manifest.json","manifest":{manifest}}}]}}"#
    );
    // 3 of 5 episodes watched: bits 0b10101 zlib-compressed, base64.
    let bits = {
        use std::io::Write;
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&[0b10101]).unwrap();
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, z.finish().unwrap())
    };
    let library = format!(
        r#"{{"items":{{
        "tt1":{{"_id":"tt1","name":"Movie One","type":"movie","poster":"https://p.example/1.jpg","_mtime":"2026-01-02T00:00:00.000Z",
               "state":{{"lastWatched":"2026-01-02T00:00:00.000Z","timeOffset":61500,"duration":120000,"timesWatched":0,"video_id":"tt1"}}}},
        "tt2":{{"_id":"tt2","name":"Show Two","type":"series","poster":"javascript:alert(1)","_mtime":"2026-01-03T00:00:00.000Z",
               "state":{{"lastWatched":"2026-01-03T00:00:00.000Z","timeOffset":30000,"duration":60000,"video_id":"tt2:1:3","watched":"tt2:1:1:5:{bits}"}}}},
        "tt3":{{"_id":"tt3","name":"Movie Three","type":"movie","_mtime":"2026-01-01T00:00:00.000Z",
               "state":{{"lastWatched":"2026-01-01T00:00:00.000Z","flaggedWatched":1}}}},
        "bad/id":{{"_id":"bad/id","name":"X","type":"movie","state":{{"flaggedWatched":1}}}}}}}}"#
    );
    let streams = r#"{"items":[[{"metaId":"tt1","videoId":"tt1"},{"stream":{"url":"https://s.example/1.mp4"}}],
                              [{"metaId":"tt2","videoId":"tt2:1:2"},{"stream":{"url":"https://s.example/wrong-episode.mp4"}}],
                              [{"metaId":"tt2","videoId":"tt2:1:3"},{"stream":{"infoHash":"0123456789abcdef0123456789abcdef01234567","fileIdx":2}}],
                              [{"metaId":"tt3","videoId":"tt3"},{"stream":{"url":"javascript:alert(1)"}}]]}"#;
    [("profile", profile), ("library_recent", library), ("streams", streams.to_string())]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[test]
fn bitfield_counts_watched_episodes() {
    assert_eq!(watched_episodes("tt0:not-base64"), 0);
    assert_eq!(watched_episodes("garbage"), 0);
    let s = storage();
    let lib = &s["library_recent"];
    let field = lib.split(r#""watched":""#).nth(1).unwrap().split('"').next().unwrap();
    assert_eq!(watched_episodes(field), 3);
}

#[test]
fn bitfield_maps_to_episode_ids() {
    let s = storage();
    let field = s["library_recent"].split(r#""watched":""#).nth(1).unwrap().split('"').next().unwrap().to_string();
    let ep = |id: &str, season, episode| Video { id: id.into(), title: None, season: Some(season), episode: Some(episode), released: None, thumbnail: None };
    let videos = [ep("tt2:1:3", 1, 3), ep("tt2:1:1", 1, 1), ep("tt2:1:2", 1, 2), ep("tt2:1:4", 1, 4), ep("tt2:1:5", 1, 5)];
    // Anchor `tt2:1:1` with length 5 puts it at bit 4, so the field is shifted by 4: only bit 4 (= 1x1) maps.
    assert_eq!(watched_videos(&field, &videos), ["tt2:1:1"]);
    // Anchored on the 5th video, the bits map one to one: 1x1, 1x3, 1x5.
    let field5 = field.replacen("tt2:1:1:", "tt2:1:5:", 1);
    assert_eq!(watched_videos(&field5, &videos), ["tt2:1:1", "tt2:1:3", "tt2:1:5"]);
    // Like stremio-core, a special (season 0) sorts after regular episodes, so it shifts nothing.
    let mut with_special = videos.to_vec();
    with_special.push(ep("tt2:0:1", 0, 1));
    assert_eq!(watched_videos(&field5, &with_special), ["tt2:1:1", "tt2:1:3", "tt2:1:5"]);
    // Anchored on the special as 6th video: no shift, so regular episodes keep their bits.
    let field_sp = field.replacen("tt2:1:1:5:", "tt2:0:1:6:", 1);
    assert_eq!(watched_videos(&field_sp, &with_special), ["tt2:1:1", "tt2:1:3", "tt2:1:5"]);
    assert!(watched_videos(&field.replacen("tt2:1:1:", "tt9:1:1:", 1), &videos).is_empty(), "unknown anchor");
    assert!(watched_videos("garbage", &videos).is_empty());
}

#[test]
fn parses_and_saves_without_clobbering_newer_progress() {
    let mut imp = parse(&storage()).unwrap();
    assert_eq!(imp.addons.len(), 1, "file: transport URL is rejected");
    assert_eq!(imp.progress.len(), 2);
    imp.progress.sort_by(|a, b| a.0.key.cmp(&b.0.key));
    let ep = &imp.progress[1].0;
    assert_eq!((ep.key.as_str(), ep.source.as_str()), ("series:tt2:1:3", "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&so=2"));
    let p = &imp.progress[0].0;
    assert_eq!((p.key.as_str(), p.source.as_str(), p.position, p.duration), ("movie:tt1", "https://s.example/1.mp4", 61.5, 120.0));
    let mut w: Vec<_> = imp.watched.iter().map(|(w, _)| (w.key.as_str(), w.episodes, w.poster.is_some())).collect();
    w.sort();
    assert_eq!(w, [("movie:tt3", 0, false), ("series:tt2", 3, false)]);

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("a.db")).unwrap();
    imp.save(&db).unwrap();
    imp.save(&db).unwrap(); // re-import is idempotent
    assert_eq!(db.addons().unwrap().len(), 1);
    let mut saved: Vec<_> = db.saved_titles().unwrap().into_iter().map(|t| t.key).collect();
    saved.sort();
    assert_eq!(saved, ["movie:tt1", "movie:tt3", "series:tt2"]);
    assert!(db.setting("bitfield:series:tt2").unwrap().is_some_and(|f| f.starts_with("tt2:1:1:5:")));
    assert_eq!(db.recent_progress(10).unwrap().len(), 2);
    // Newest watched first.
    assert_eq!(db.watched(10).unwrap().iter().map(|w| w.key.as_str()).collect::<Vec<_>>(), ["series:tt2", "movie:tt3"]);
    // Local progress saved now is newer than the 2026-01-02 import and survives a re-import.
    db.save_progress(&stremflix_rs::db::Progress { position: 99.0, ..p.clone() }).unwrap();
    imp.save(&db).unwrap();
    assert_eq!(db.progress("movie:tt1").unwrap().unwrap().position, 99.0);
}

#[test]
fn account_sync_imports_addons_library_and_saved_titles() {
    let addons = serde_json::json!({"addons": [
        {"transportUrl": "https://a.example/manifest.json", "manifest": {"id": "a", "version": "1", "name": "A", "resources": ["stream"], "types": ["movie"]}},
        {"transportUrl": "file:///etc/manifest.json", "manifest": {"id": "b", "version": "1", "name": "B", "resources": [], "types": []}}]});
    let item = |id: &str, extra: serde_json::Value| {
        let mut v = serde_json::json!({"_id": id, "name": format!("Title {id}"), "type": "movie", "_mtime": "2026-01-01T00:00:00.000Z", "state": {}});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    };
    let library = serde_json::json!([
        item("tt1", serde_json::json!({})),
        item("tt2", serde_json::json!({"removed": true, "state": {"flaggedWatched": 1}})),
        item("tt3", serde_json::json!({"temp": true})),
        item("tt4", serde_json::json!({"type": "other", "state": {"flaggedWatched": 1}})),
        item("bad/id", serde_json::json!({}))]);
    let imp = from_account(&addons, &library).unwrap();
    assert_eq!(imp.addons.len(), 1, "file: transport URL is rejected");
    assert_eq!(imp.saved.iter().map(|(t, _)| t.key.as_str()).collect::<Vec<_>>(), ["movie:tt1"], "removed, temp, other and invalid items aren't saved");
    assert_eq!(imp.watched.iter().map(|(w, _)| w.key.as_str()).collect::<Vec<_>>(), ["movie:tt2"], "removed items keep their history");
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("a.db")).unwrap();
    imp.save(&db).unwrap();
    imp.save(&db).unwrap(); // a re-sync is idempotent
    assert_eq!(db.saved_titles().unwrap().len(), 1);
    assert_eq!(db.addons().unwrap().len(), 1);
    db.remove_saved_title("movie:tt1").unwrap();
    db.set_watched("movie:tt2", false).unwrap();
    imp.save(&db).unwrap();
    assert!(db.saved_titles().unwrap().is_empty(), "a title removed here stays removed after a re-sync");
    assert!(db.watched(10).unwrap().is_empty(), "a title unwatched here stays unwatched after a re-sync");
    // Added back here: later syncs update it again.
    db.add_saved_title(&imp.saved[0].0).unwrap();
    db.set_watched("movie:tt2", true).unwrap();
    imp.save(&db).unwrap();
    assert_eq!(db.saved_titles().unwrap().len(), 1);
    assert_eq!(db.watched(10).unwrap().len(), 1);
    // An addon removed here isn't reinstalled.
    db.remove_addon("https://a.example/manifest.json").unwrap();
    imp.save(&db).unwrap();
    assert!(db.addons().unwrap().is_empty());
    assert!(from_account(&serde_json::json!({}), &library).is_err(), "unexpected addon shape");
    assert!(from_account(&addons, &serde_json::json!({"items": []})).is_err(), "unexpected library shape");
    assert!(from_account(&addons, &serde_json::json!([{"id": "tt1"}])).is_err(), "unexpected library item shape");
    assert!(from_account(&addons, &serde_json::json!([])).is_ok(), "an empty library is fine");
}

#[test]
fn unwatched_episode_survives_bitfield() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("a.db")).unwrap();
    db.set_watched("series:tt2:1:1", false).unwrap();
    let keys = ["series:tt2:1:1".to_string(), "series:tt2:1:2".to_string()];
    assert_eq!(db.apply_watched_bitfield("bitfield:series:tt2", &keys).unwrap(), ["series:tt2:1:2"]);
    assert!(!db.watched_keys().unwrap().contains("series:tt2:1:1"));

    // Unwatching an episode drops its imported series from Watch It Again, also across syncs.
    let imp = Import { watched: vec![(Watched { key: "series:tt2".into(), title: "Show".into(), poster: None, episodes: 2 }, "2026-01-01T00:00:00Z".into())], ..Default::default() };
    imp.save(&db).unwrap();
    db.set_watched("series:tt2:1:2", false).unwrap();
    assert!(db.watched(10).unwrap().is_empty());
    imp.save(&db).unwrap();
    assert!(db.watched(10).unwrap().is_empty(), "a sync doesn't bring back a series unwatched here");
    // Watching an episode again lets the next sync restore it; a similar-prefix series isn't affected.
    db.set_watched("series:tt22:1:1", true).unwrap();
    imp.save(&db).unwrap();
    assert!(db.watched(10).unwrap().is_empty());
    db.set_watched("series:tt2:1:2", true).unwrap();
    imp.save(&db).unwrap();
    assert_eq!(db.watched(10).unwrap().len(), 1);
}

#[test]
fn imported_saved_titles_keep_stremio_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("a.db")).unwrap();
    let t = |id: &str| stremflix_rs::db::SavedTitle { key: format!("movie:{id}"), title: id.into(), kind: "movie".into(), poster: None };
    let imp = Import { saved: vec![(t("old"), "2024-01-01T00:00:00Z".into()), (t("new"), "2025-01-01T00:00:00Z".into())], ..Default::default() };
    imp.save(&db).unwrap();
    assert_eq!(db.saved_titles().unwrap().iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["new", "old"]);
}

#[test]
fn api_errors_are_reported() {
    use stremflix_rs::addon::api_result;
    assert_eq!(api_result(br#"{"result":{"authKey":"k"}}"#).unwrap()["authKey"], "k");
    let e = api_result(br#"{"error":{"message":"Wrong passphrase","code":2}}"#).unwrap_err().to_string();
    assert!(e.contains("Wrong passphrase"), "{e}");
    assert!(api_result(br#"{"result":null}"#).is_err());
    assert!(api_result(b"<html>").is_err());
}

#[test]
fn reads_webkit_sqlite_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("LocalStorage/localstorage.sqlite3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB NOT NULL ON CONFLICT FAIL)").unwrap();
    for (k, v) in storage() {
        conn.execute("INSERT INTO ItemTable VALUES (?1, ?2)", rusqlite::params![k, utf16(&v)]).unwrap();
    }
    // Left open: uncheckpointed WAL data must be read too, like with a running Stremio.
    assert_eq!(read_storage(&path).unwrap(), storage());
    let (found, imp) = import(Some(dir.path())).unwrap();
    assert_eq!(found, path);
    assert_eq!(imp.progress.len(), 2);
    drop(conn);
}

#[test]
fn reads_chromium_leveldb_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Default/Local Storage/leveldb");
    {
        let mut db = rusty_leveldb::DB::open(&path, rusty_leveldb::Options::default()).unwrap();
        let key = |origin: &str, k: &str| [format!("_{origin}\0\x01{k}").into_bytes()].concat();
        db.put(&key("https://other.example", "profile"), b"\x01{}").unwrap(); // another origin is ignored
        db.put(b"VERSION", b"1").unwrap();
        for (k, v) in storage() {
            db.put(&key("https://web.stremio.com", &k), &[&[0u8][..], &utf16(&v)].concat()).unwrap();
        }
        db.flush().unwrap();
    }
    assert_eq!(read_storage(&path).unwrap(), storage());
    let (found, imp) = import(Some(dir.path())).unwrap();
    assert_eq!(found, path);
    assert_eq!(imp.watched.len(), 2);
}

#[test]
fn manual_path_named_like_a_cache_is_searched() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("stremio-cache-backup");
    let path = root.join("localstorage.sqlite3");
    std::fs::create_dir_all(&root).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE ItemTable (key TEXT, value BLOB)").unwrap();
    for (k, v) in storage() {
        conn.execute("INSERT INTO ItemTable VALUES (?1, ?2)", rusqlite::params![k, utf16(&v)]).unwrap();
    }
    assert_eq!(import(Some(&root)).unwrap().0, path);
}

#[test]
fn missing_storage_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(import(Some(dir.path())).is_err());
}

/// Manual check against this machine's Stremio: `cargo test --test stremio_import -- --ignored --nocapture`.
#[test]
#[ignore]
fn local_install() {
    let (path, imp) = import(None).unwrap();
    println!("{}: {} addons, {} in progress, {} watched", path.display(), imp.addons.len(), imp.progress.len(), imp.watched.len());
    for (p, at) in imp.progress.iter().take(5) {
        println!("  progress {} {:?} {:.0}/{:.0}s {at}", p.key, p.title, p.position, p.duration);
    }
    for (w, _) in imp.watched.iter().take(5) {
        println!("  watched {} {:?} episodes={}", w.key, w.title, w.episodes);
    }
    let dir = tempfile::tempdir().unwrap();
    imp.save(&Db::open(&dir.path().join("a.db")).unwrap()).unwrap();
}
