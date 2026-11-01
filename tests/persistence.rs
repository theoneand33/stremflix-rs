use stremio_lite::addon::parse_manifest;
use stremio_lite::db::{Db, Progress};
use stremio_lite::library::LibraryItem;

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
    assert_eq!(db.library_folders().unwrap(), vec![std::path::PathBuf::from("/videos")]);
    assert_eq!(db.library_items().unwrap(), vec![item]);
    assert_eq!(db.setting("theme").unwrap().as_deref(), Some("dark"));
    assert_eq!(db.setting("missing").unwrap(), None);
    assert_eq!(db.progress("movie:tt1").unwrap().unwrap(), Progress { position: 50.0, ..prog });
    assert_eq!(db.recent_progress(10).unwrap().len(), 1);

    db.remove_addon("https://x.example/manifest.json").unwrap();
    drop(db);
    assert!(Db::open(&path).unwrap().addons().unwrap().is_empty());
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
