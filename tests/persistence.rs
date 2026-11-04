use stremflix_rs::addon::parse_manifest;
use stremflix_rs::db::{Db, Download, Progress, SavedTitle, Watched};
use stremflix_rs::library::LibraryItem;

#[test]
fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/app.db");
    let manifest = parse_manifest(
        br#"{"id":"a","version":"1","name":"A","resources":["stream"],"types":["movie"]}"#,
    )
    .unwrap();
    let item = LibraryItem { path: "/videos/a.mp4".into(), title: "a".into() };
    let prog = Progress { key: "movie:tt1".into(), title: "One".into(), source: "https://x.example/v.mp4".into(), position: 42.5, duration: 120.0, poster: Some("https://x.example/p.jpg".into()) };
    {
        let db = Db::open(&path).unwrap();
        db.add_addon("https://x.example/manifest.json", &manifest).unwrap();
        db.add_library_folder("/videos".as_ref(), &[item.clone()]).unwrap();
        db.set_setting("theme", "dark").unwrap();
        db.save_progress(&prog).unwrap();
        db.save_progress(&Progress { position: 50.0, ..prog.clone() }).unwrap(); // upsert
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.addons().unwrap(), vec![("https://x.example/manifest.json".to_string(), manifest)]);
    assert_eq!(db.library_items().unwrap(), vec![item]);
    assert_eq!(db.setting("theme").unwrap().as_deref(), Some("dark"));
    assert_eq!(db.setting("missing").unwrap(), None);
    assert_eq!(db.progress("movie:tt1").unwrap().unwrap(), Progress { position: 50.0, ..prog });
    assert_eq!(db.recent_progress(10).unwrap().len(), 1);
    db.save_progress(&Progress { key: "movie:tt2".into(), title: "Two".into(), source: "s".into(), position: 1.0, duration: 0.0, poster: None }).unwrap();
    db.remove_progress("movie:tt1").unwrap();
    db.remove_progress("movie:missing").unwrap(); // no-op
    assert_eq!(db.recent_progress(10).unwrap().iter().map(|p| p.key.as_str()).collect::<Vec<_>>(), ["movie:tt2"]);
    assert!(db.watched_keys().unwrap().is_empty()); // removing is not marking watched

    db.remove_addon("https://x.example/manifest.json").unwrap();
    drop(db);
    assert!(Db::open(&path).unwrap().addons().unwrap().is_empty());
}

#[test]
fn saved_titles_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let movie = SavedTitle { key: "movie:tt1".into(), title: "One".into(), kind: "movie".into(), poster: Some("https://x.example/p.jpg".into()) };
    let show = SavedTitle { key: "series:tt2".into(), title: "Two".into(), kind: "series".into(), poster: None };
    {
        let db = Db::open(&path).unwrap();
        db.add_saved_title(&movie).unwrap();
        db.add_saved_title(&show).unwrap();
        // Re-adding keeps one row and does not push it below newer entries.
        db.add_saved_title(&SavedTitle { title: "One (2020)".into(), ..movie.clone() }).unwrap();
    }
    let db = Db::open(&path).unwrap();
    let saved = db.saved_titles().unwrap();
    assert_eq!(saved, vec![show.clone(), SavedTitle { title: "One (2020)".into(), ..movie.clone() }]);
    // A malformed key or type is skipped instead of breaking the list.
    db.add_saved_title(&SavedTitle { key: "bad/key".into(), title: "Bad".into(), kind: "movie".into(), poster: None }).unwrap();
    assert_eq!(db.saved_titles().unwrap().len(), 2);
    assert_eq!(show.preview().map(|m| (m.kind, m.id, m.name)), Some(("series".to_string(), "tt2".to_string(), "Two".to_string())));
    assert_eq!(SavedTitle { key: "nocolon".into(), ..show.clone() }.preview(), None);
    db.remove_saved_title("movie:tt1").unwrap();
    assert_eq!(db.saved_titles().unwrap(), vec![show]);
}

#[test]
fn migrates_progress_without_poster() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE progress (key TEXT PRIMARY KEY, title TEXT NOT NULL, source TEXT NOT NULL,
                 position REAL NOT NULL, duration REAL NOT NULL, updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
             INSERT INTO progress (key, title, source, position, duration) VALUES ('k', 'T', 's', 1.0, 2.0);",
        )
        .unwrap();
    let db = Db::open(&path).unwrap();
    assert_eq!(db.progress("k").unwrap().unwrap().poster, None);
    db.set_progress_poster("k", "https://x.example/p.jpg").unwrap();
    assert_eq!(db.progress("k").unwrap().unwrap().poster.as_deref(), Some("https://x.example/p.jpg"));
    Db::open(&path).unwrap(); // reopening with the column present still works
}

#[test]
fn watched_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("w.db")).unwrap();
    let key = "series:tt1:1:2";
    let prog = Progress { key: key.into(), title: "T".into(), source: "s".into(), position: 10.0, duration: 100.0, poster: None };
    db.save_progress(&prog).unwrap();
    assert!(db.watched_keys().unwrap().is_empty());

    db.set_watched(key, true).unwrap();
    assert!(db.watched_keys().unwrap().contains(key));
    assert!(db.progress(key).unwrap().is_none()); // left Continue Watching
    assert!(db.watched(10).unwrap().is_empty()); // not a Watch It Again title

    db.set_watched(key, false).unwrap();
    assert!(db.watched_keys().unwrap().is_empty());

    // An imported movie counts as watched, and can be unmarked.
    let w = Watched { key: "movie:tt9".into(), title: "M".into(), poster: None, episodes: 0 };
    db.save_watched(&w, "2024-01-01T00:00:00Z").unwrap();
    assert!(db.watched_keys().unwrap().contains("movie:tt9"));
    db.set_watched("movie:tt9", false).unwrap();
    assert!(db.watched_keys().unwrap().is_empty());
}

#[test]
fn addon_order_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let m = parse_manifest(br#"{"id":"a","version":"1","name":"A","resources":["stream"],"types":["movie"]}"#).unwrap();
    let urls = |db: &Db| db.addons().unwrap().into_iter().map(|(u, _)| u).collect::<Vec<_>>();
    let u = |n: &str| format!("https://{n}.example/manifest.json");
    {
        let db = Db::open(&path).unwrap();
        for n in ["a", "b", "c"] {
            db.add_addon(&u(n), &m).unwrap();
        }
        assert_eq!(urls(&db), [u("a"), u("b"), u("c")], "install order");
        db.reorder_addons(&[u("c"), u("a"), u("b")]).unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(urls(&db), [u("c"), u("a"), u("b")]);
    db.add_addon(&u("d"), &m).unwrap();
    assert_eq!(urls(&db).last(), Some(&u("d")), "new addons go last");
    db.add_addon(&u("c"), &m).unwrap();
    assert_eq!(urls(&db)[0], u("c"), "reinstall keeps its place");
    drop(db);
    // A database from before the position column: rows keep install order and new ones go last.
    let old = dir.path().join("old.db");
    let conn = rusqlite::Connection::open(&old).unwrap();
    conn.execute_batch("CREATE TABLE addons (transport_url TEXT PRIMARY KEY, manifest TEXT NOT NULL, installed_at INTEGER NOT NULL DEFAULT (unixepoch()));").unwrap();
    let json = serde_json::to_string(&m).unwrap();
    for (n, at) in [("b", 2), ("a", 1)] {
        conn.execute("INSERT INTO addons VALUES (?1, ?2, ?3)", rusqlite::params![u(n), json, at]).unwrap();
    }
    drop(conn);
    let db = Db::open(&old).unwrap();
    db.add_addon(&u("c"), &m).unwrap();
    assert_eq!(urls(&db), [u("a"), u("b"), u("c")]);
}

#[test]
fn downloads_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.db");
    let ep = Download { path: "/d/Show S1E2.mkv".into(), kind: "series".into(), meta_id: "tt2".into(), video_id: "tt2:1:2".into(), title: "Show S1E2".into() };
    let movie = Download { path: "/d/One.mp4".into(), kind: "movie".into(), meta_id: "tt1".into(), video_id: "tt1".into(), title: "One".into() };
    {
        let db = Db::open(&path).unwrap();
        db.add_download(&ep).unwrap();
        db.add_download(&movie).unwrap();
    }
    let db = Db::open(&path).unwrap();
    assert_eq!(db.downloads().unwrap(), [movie.clone(), ep.clone()], "newest first");
    assert_eq!((ep.key(), movie.key()), ("series:tt2:1:2".to_string(), "movie:tt1".to_string()));
    let prog = |key: String, source: &str| Progress { key, title: "t".into(), source: source.into(), position: 1.0, duration: 2.0, poster: None };
    db.save_progress(&prog(movie.key(), "/d/One.mp4")).unwrap();
    db.save_progress(&prog(ep.key(), "https://x/ep.mkv")).unwrap();
    db.remove_download(&movie).unwrap();
    db.remove_download(&ep).unwrap();
    assert_eq!(db.downloads().unwrap(), []);
    assert_eq!(db.progress(&movie.key()).unwrap(), None, "resume point on the deleted file is dropped");
    assert!(db.progress(&ep.key()).unwrap().is_some(), "a streamed resume point is kept");
}

#[cfg(unix)]
#[test]
fn db_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.db");
    drop(Db::open(&path).unwrap());
    // A journal left world-readable by an older version is fixed on open.
    let wal = dir.path().join("a.db-wal");
    std::fs::write(&wal, b"").unwrap();
    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o644)).unwrap();
    let _db = Db::open(&path).unwrap(); // kept open: SQLite deletes the WAL on the last close
    for f in [&path, &wal] {
        assert_eq!(std::fs::metadata(f).unwrap().permissions().mode() & 0o777, 0o600, "{}", f.display());
    }
}
