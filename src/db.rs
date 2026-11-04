//! SQLite persistence: addons, library folders/items, saved titles, settings, watch progress.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

use crate::addon::{Manifest, MetaPreview, parse_manifest, valid_id};
use crate::library::LibraryItem;

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub key: String,
    pub title: String,
    /// What to reopen: http URL, magnet, or local file path.
    pub source: String,
    pub position: f64,
    pub duration: f64,
    /// Poster URL for the Continue Watching card, if known.
    pub poster: Option<String>,
}

/// A title marked watched in Stremio (Watch It Again row).
#[derive(Debug, Clone, PartialEq)]
pub struct Watched {
    /// `<type>:<id>`, like addon progress keys.
    pub key: String,
    pub title: String,
    pub poster: Option<String>,
    /// Watched episode count for series; 0 for movies.
    pub episodes: u32,
}

/// A title saved with "Add to library".
#[derive(Debug, Clone, PartialEq)]
pub struct SavedTitle {
    /// `<type>:<id>`, like addon progress keys.
    pub key: String,
    pub title: String,
    pub kind: String,
    pub poster: Option<String>,
}

/// A stream saved for offline playback.
#[derive(Debug, Clone, PartialEq)]
pub struct Download {
    pub path: PathBuf,
    pub kind: String,
    pub meta_id: String,
    /// Episode id for series; the meta id for movies.
    pub video_id: String,
    pub title: String,
}

impl Download {
    /// Progress key, the same as when streaming, so the title resumes either way.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind, self.video_id)
    }
}

impl SavedTitle {
    /// Detail-page view of the saved title; `None` if the key has no `<type>:<id>` shape.
    pub fn preview(&self) -> Option<MetaPreview> {
        let (kind, id) = self.key.split_once(':')?;
        Some(MetaPreview { id: id.into(), kind: kind.into(), name: self.title.clone(), poster: self.poster.clone(), background: None, description: None })
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // Owner-only: it holds the Stremio account's auth key. New `-wal`/`-shm` files get the DB's mode;
        // ones left by an older version are fixed here.
        #[cfg(unix)]
        for ext in ["", "-wal", "-shm"] {
            let f = PathBuf::from(format!("{}{ext}", path.display()));
            if f.exists() {
                std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            }
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL; -- safe with WAL; a crash loses at most the last commit, and progress saves every second
             CREATE TABLE IF NOT EXISTS addons (transport_url TEXT PRIMARY KEY, manifest TEXT NOT NULL, installed_at INTEGER NOT NULL DEFAULT (unixepoch()));
             CREATE TABLE IF NOT EXISTS library_folders (path TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS library_items (path TEXT PRIMARY KEY, folder TEXT NOT NULL, title TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS progress (key TEXT PRIMARY KEY, title TEXT NOT NULL, source TEXT NOT NULL,
                 position REAL NOT NULL, duration REAL NOT NULL, updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
             CREATE TABLE IF NOT EXISTS watched (key TEXT PRIMARY KEY, title TEXT NOT NULL, poster TEXT,
                 episodes INTEGER NOT NULL, watched_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS saved_titles (key TEXT PRIMARY KEY, title TEXT NOT NULL, kind TEXT NOT NULL,
                 poster TEXT, saved_at INTEGER NOT NULL DEFAULT (unixepoch()));
             CREATE TABLE IF NOT EXISTS downloads (path TEXT PRIMARY KEY, kind TEXT NOT NULL, meta_id TEXT NOT NULL,
                 video_id TEXT NOT NULL, title TEXT NOT NULL, added_at INTEGER NOT NULL DEFAULT (unixepoch()));",
        )?;
        // Migrate databases created before posters were stored; fails harmlessly if the column exists.
        let _ = conn.execute("ALTER TABLE progress ADD COLUMN poster TEXT", []);
        // Addon priority (streams and catalogs follow it); older rows share 0 and keep install order.
        let _ = conn.execute("ALTER TABLE addons ADD COLUMN position INTEGER NOT NULL DEFAULT 0", []);
        Ok(Self { conn })
    }

    /// Run `f` in one transaction: all of it is saved, or none (and one fsync instead of many).
    pub fn transaction(&self, f: impl FnOnce() -> Result<()>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        f()?;
        tx.commit()?;
        Ok(())
    }

    /// Also lets Stremio imports add it again (see [`Db::remove_addon`]).
    pub fn add_addon(&self, transport_url: &str, manifest: &Manifest) -> Result<()> {
        self.conn.execute("DELETE FROM settings WHERE key = ?1", [format!("unaddon:{transport_url}")])?;
        self.conn.execute(
            // A reinstall keeps its priority; a new addon goes last.
            "INSERT INTO addons (transport_url, manifest, position) VALUES (?1, ?2, (SELECT coalesce(max(position), 0) + 1 FROM addons))
             ON CONFLICT(transport_url) DO UPDATE SET manifest = excluded.manifest",
            params![transport_url, serde_json::to_string(manifest)?],
        )?;
        Ok(())
    }

    /// Remembered (`unaddon:<url>`), so an account sync or import doesn't reinstall it.
    pub fn remove_addon(&self, transport_url: &str) -> Result<()> {
        self.transaction(|| {
            self.conn.execute("DELETE FROM addons WHERE transport_url = ?1", [transport_url])?;
            self.set_setting(&format!("unaddon:{transport_url}"), "1")
        })
    }

    /// Store `urls` (installed addon transport URLs) as the addon priority order, first = highest.
    pub fn reorder_addons(&self, urls: &[String]) -> Result<()> {
        self.transaction(|| {
            for (i, u) in urls.iter().enumerate() {
                self.conn.execute("UPDATE addons SET position = ?2 WHERE transport_url = ?1", params![u, i as i64])?;
            }
            Ok(())
        })
    }

    /// Installed addons in priority order; stored manifests are re-validated and corrupt rows skipped.
    pub fn addons(&self) -> Result<Vec<(String, Manifest)>> {
        let mut st = self.conn.prepare("SELECT transport_url, manifest FROM addons ORDER BY position, installed_at, transport_url")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).filter_map(|(u, m)| Some((u, parse_manifest(m.as_bytes()).ok()?))).collect())
    }

    pub fn add_library_folder(&self, folder: &Path, items: &[LibraryItem]) -> Result<()> {
        let f = folder.to_string_lossy();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT OR IGNORE INTO library_folders (path) VALUES (?1)", [&f])?;
        tx.execute("DELETE FROM library_items WHERE folder = ?1", [&f])?;
        for it in items {
            tx.execute(
                "INSERT OR REPLACE INTO library_items (path, folder, title) VALUES (?1, ?2, ?3)",
                params![it.path.to_string_lossy(), f, it.title],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn library_items(&self) -> Result<Vec<LibraryItem>> {
        let mut st = self.conn.prepare("SELECT path, title FROM library_items ORDER BY path")?;
        let rows = st.query_map([], |r| {
            Ok(LibraryItem { path: PathBuf::from(r.get::<_, String>(0)?), title: r.get(1)? })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Re-adding a saved title keeps its original `saved_at`, so the library order is stable.
    /// It also lets Stremio imports add it again (see [`Db::remove_saved_title`]).
    pub fn add_saved_title(&self, t: &SavedTitle) -> Result<()> {
        self.conn.execute("DELETE FROM settings WHERE key = ?1", [format!("unsaved:{}", t.key)])?;
        self.conn.execute(
            "INSERT INTO saved_titles (key, title, kind, poster) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET title = excluded.title, poster = coalesce(excluded.poster, poster)",
            params![t.key, t.title, t.kind, t.poster],
        )?;
        Ok(())
    }

    /// Remembered (`unsaved:<key>`), so an account sync or import doesn't bring it back.
    pub fn remove_saved_title(&self, key: &str) -> Result<()> {
        self.transaction(|| {
            self.conn.execute("DELETE FROM saved_titles WHERE key = ?1", [key])?;
            self.set_setting(&format!("unsaved:{key}"), "1")
        })
    }

    /// Saved titles, newest first; rows with a malformed key or type are skipped.
    pub fn saved_titles(&self) -> Result<Vec<SavedTitle>> {
        // rowid, not key, breaks same-second ties so the order matches insertion order.
        let mut st = self.conn.prepare("SELECT key, title, kind, poster FROM saved_titles ORDER BY saved_at DESC, rowid DESC")?;
        let rows = st.query_map([], |r| {
            Ok(SavedTitle { key: r.get(0)?, title: r.get(1)?, kind: r.get(2)?, poster: r.get(3)? })
        })?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter(|t| valid_id(&t.key) && valid_id(&t.kind) && !t.title.trim().is_empty())
            .collect())
    }

    pub fn add_download(&self, d: &Download) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO downloads (path, kind, meta_id, video_id, title) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![d.path.to_string_lossy(), d.kind, d.meta_id, d.video_id, d.title],
        )?;
        Ok(())
    }

    /// Also drops its resume point if that plays the file, so Continue Watching doesn't open a missing file.
    pub fn remove_download(&self, d: &Download) -> Result<()> {
        let path = d.path.to_string_lossy();
        self.transaction(|| {
            self.conn.execute("DELETE FROM downloads WHERE path = ?1", [&path])?;
            self.conn.execute("DELETE FROM progress WHERE key = ?1 AND source = ?2", [d.key().as_str(), &path])?;
            Ok(())
        })
    }

    /// Downloads, newest first.
    pub fn downloads(&self) -> Result<Vec<Download>> {
        let mut st = self.conn.prepare("SELECT path, kind, meta_id, video_id, title FROM downloads ORDER BY added_at DESC, rowid DESC")?;
        let rows = st.query_map([], |r| {
            Ok(Download { path: PathBuf::from(r.get::<_, String>(0)?), kind: r.get(1)?, meta_id: r.get(2)?, video_id: r.get(3)?, title: r.get(4)? })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    pub fn remove_setting(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn save_progress(&self, p: &Progress) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO progress (key, title, source, position, duration, poster, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch())",
            params![p.key, p.title, p.source, p.position, p.duration, p.poster],
        )?;
        Ok(())
    }

    /// Set the poster without bumping `updated_at`, so Continue Watching order is kept.
    pub fn set_progress_poster(&self, key: &str, poster: &str) -> Result<()> {
        self.conn.execute("UPDATE progress SET poster = ?2 WHERE key = ?1", [key, poster])?;
        Ok(())
    }

    /// Drop a resume point without marking it watched (removes it from Continue Watching).
    pub fn remove_progress(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM progress WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn progress(&self, key: &str) -> Result<Option<Progress>> {
        Ok(self
            .conn
            .query_row("SELECT key, title, source, position, duration, poster FROM progress WHERE key = ?1", [key], row_progress)
            .optional()?)
    }

    /// Imported progress keeps its original time (ISO 8601 `at`) and never overwrites newer local progress.
    pub fn import_progress(&self, p: &Progress, at: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO progress (key, title, source, position, duration, poster, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, coalesce(unixepoch(?7), 0))
             ON CONFLICT(key) DO UPDATE SET title = excluded.title, source = excluded.source, position = excluded.position,
                 duration = excluded.duration, poster = coalesce(excluded.poster, poster), updated_at = excluded.updated_at
             WHERE excluded.updated_at > progress.updated_at",
            params![p.key, p.title, p.source, p.position, p.duration, p.poster, at],
        )?;
        Ok(())
    }

    /// `at` is when it was last watched (ISO 8601), used for ordering.
    pub fn save_watched(&self, w: &Watched, at: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO watched (key, title, poster, episodes, watched_at) VALUES (?1, ?2, ?3, ?4, coalesce(unixepoch(?5), 0))",
            params![w.key, w.title, w.poster, w.episodes, at],
        )?;
        Ok(())
    }

    pub fn watched(&self, limit: usize) -> Result<Vec<Watched>> {
        let mut st = self.conn.prepare("SELECT key, title, poster, episodes FROM watched ORDER BY watched_at DESC, key LIMIT ?1")?;
        let rows = st.query_map([limit as i64], |r| Ok(Watched { key: r.get(0)?, title: r.get(1)?, poster: r.get(2)?, episodes: r.get(3)? }))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Mark a movie or episode (`<type>:<id>`) watched or not. Watching also drops its resume point,
    /// so it leaves Continue Watching. Kept in `settings` so the Watch It Again row stays Stremio-only.
    /// Unwatching is remembered (`unwatched:<key>`), so an account sync or import doesn't undo it.
    pub fn set_watched(&self, key: &str, on: bool) -> Result<()> {
        let k = format!("watched:{key}");
        self.transaction(|| {
            if on {
                self.set_setting(&k, "1")?;
                self.conn.execute("DELETE FROM progress WHERE key = ?1", [key])?;
                self.conn.execute("DELETE FROM settings WHERE key = ?1", [format!("unwatched:{key}")])?;
            } else {
                self.conn.execute("DELETE FROM settings WHERE key = ?1", [&k])?;
                self.set_setting(&format!("unwatched:{key}"), "1")?;
                // Also clears a title imported from Stremio.
                self.conn.execute("DELETE FROM watched WHERE key = ?1", [key])?;
            }
            Ok(())
        })
    }

    /// Mark episodes `keys` watched, except ones unwatched here, and drop the imported bitfield setting
    /// they came from, in one transaction. Returns the keys marked.
    pub fn apply_watched_bitfield(&self, bitfield_key: &str, keys: &[String]) -> Result<Vec<String>> {
        let mut marked = Vec::new();
        self.transaction(|| {
            for k in keys {
                if self.setting(&format!("unwatched:{k}"))?.is_none() {
                    self.set_setting(&format!("watched:{k}"), "1")?;
                    marked.push(k.clone());
                }
            }
            self.conn.execute("DELETE FROM settings WHERE key = ?1", [bitfield_key])?;
            Ok(())
        })?;
        Ok(marked)
    }

    /// Keys marked watched here or imported from Stremio (a movie imported as watched counts).
    pub fn watched_keys(&self) -> Result<std::collections::HashSet<String>> {
        let mut st = self.conn.prepare(
            "SELECT substr(key, 9) FROM settings WHERE key LIKE 'watched:%' UNION SELECT key FROM watched",
        )?;
        Ok(st.query_map([], |r| r.get::<_, String>(0))?.filter_map(|r| r.ok()).collect())
    }

    pub fn recent_progress(&self, limit: usize) -> Result<Vec<Progress>> {
        let mut st = self.conn.prepare(
            "SELECT key, title, source, position, duration, poster FROM progress ORDER BY updated_at DESC, key LIMIT ?1",
        )?;
        Ok(st.query_map([limit as i64], row_progress)?.filter_map(|r| r.ok()).collect())
    }
}

fn row_progress(r: &rusqlite::Row) -> rusqlite::Result<Progress> {
    Ok(Progress { key: r.get(0)?, title: r.get(1)?, source: r.get(2)?, position: r.get(3)?, duration: r.get(4)?, poster: r.get(5)? })
}
