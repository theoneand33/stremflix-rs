//! SQLite persistence: addons, library folders/items, saved titles and lists, settings, watch progress.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

use crate::addon::{Manifest, MetaPreview, addon_base, parse_magnet_hash, parse_manifest, safe_http_url, sanitize_text, valid_id};
use crate::download::State;
use crate::library::LibraryItem;
use serde_json::{Value, json};

/// An unfinished download: where it comes from and what a resume needs.
#[derive(Debug, PartialEq)]
pub struct QueuedDownload {
    pub d: Download,
    /// http URL or magnet.
    pub source: String,
    pub headers: Vec<(String, String)>,
    pub state: State,
    /// ETag or Last-Modified of the file when it started; empty if unknown.
    pub validator: String,
    pub total: u64,
}

/// Settings an export carries. Not secrets (`stremio_auth`), local paths, or `external_player` (a command to run).
const EXPORT_SETTINGS: [&str; 9] = ["autoplay_next", "hwdec", "sub_style", "sub_off", "torrent_cache", "torrent_profile", "default_speed", "alang", "slang"];
const EXPORT_FORMAT: &str = "stremflix-rs-export";
pub const MAX_IMPORT_BYTES: u64 = 16 << 20;

/// A progress row is kept only if it resumes a web URL or a magnet with a real info hash, never a local path.
fn resumable_source(key: &str, source: &str) -> bool {
    !key.starts_with("file:") && (safe_http_url(source).is_some() || parse_magnet_hash(source).is_some())
}

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
    /// Release year and IMDb rating, when known (older rows and imports: `None` until the detail page loads).
    pub year: Option<i32>,
    pub rating: Option<f64>,
    /// Unix time of the latest progress or watched mark on the title or any of its episodes; read-only, set by `saved_titles`.
    pub last_watched: Option<i64>,
}

/// A user list ("collection") of saved titles.
#[derive(Debug, Clone, PartialEq)]
pub struct List {
    pub id: i64,
    pub name: String,
    /// Saved-title keys in the list.
    pub keys: std::collections::HashSet<String>,
}

/// Trimmed list name, or an error unless it is 1–60 characters.
fn list_name(name: &str) -> Result<&str> {
    let n = name.trim();
    anyhow::ensure!((1..=60).contains(&n.chars().count()), "list names are 1–60 characters");
    Ok(n)
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
            let mut f = path.as_os_str().to_os_string();
            f.push(ext);
            // Some filesystems (FAT, network shares) have no Unix modes: not a reason to refuse to start.
            if let Err(e) = std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                && e.kind() != std::io::ErrorKind::NotFound
            {
                crate::elog!("making {} owner-only: {e}", PathBuf::from(f).display());
            }
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL; -- safe with WAL; a crash loses at most the last commit, and progress saves every second
             PRAGMA foreign_keys=ON;
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
                 video_id TEXT NOT NULL, title TEXT NOT NULL, added_at INTEGER NOT NULL DEFAULT (unixepoch()));
             CREATE TABLE IF NOT EXISTS lists (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                 created INTEGER NOT NULL DEFAULT (unixepoch()));
             CREATE TABLE IF NOT EXISTS list_items (list_id INTEGER NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
                 title_key TEXT NOT NULL, added INTEGER NOT NULL DEFAULT (unixepoch()), PRIMARY KEY (list_id, title_key));",
        )?;
        // Migrate databases created before posters were stored; fails harmlessly if the column exists.
        let _ = conn.execute("ALTER TABLE progress ADD COLUMN poster TEXT", []);
        // Library sorts by year and rating; older rows stay NULL and sort last.
        let _ = conn.execute("ALTER TABLE saved_titles ADD COLUMN year INTEGER", []);
        let _ = conn.execute("ALTER TABLE saved_titles ADD COLUMN rating REAL", []);
        // Addon priority (streams and catalogs follow it); older rows share 0 and keep install order.
        let _ = conn.execute("ALTER TABLE addons ADD COLUMN position INTEGER NOT NULL DEFAULT 0", []);
        // Download queue: finished rows keep the default 'done'; queued, active, paused and failed ones carry what a resume needs.
        for col in ["status TEXT NOT NULL DEFAULT 'done'", "source TEXT NOT NULL DEFAULT ''", "headers TEXT NOT NULL DEFAULT '[]'", "validator TEXT NOT NULL DEFAULT ''", "total INTEGER NOT NULL DEFAULT 0"] {
            let _ = conn.execute(&format!("ALTER TABLE downloads ADD COLUMN {col}"), []);
        }
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

    /// Paths of the folders added to the local library.
    pub fn library_folders(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT path FROM library_folders ORDER BY path")?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Forget a folder and its scanned items. Files on disk and their saved progress are untouched.
    pub fn remove_library_folder(&self, folder: &str) -> Result<()> {
        self.transaction(|| {
            self.conn.execute("DELETE FROM library_items WHERE folder = ?1", [folder])?;
            self.conn.execute("DELETE FROM library_folders WHERE path = ?1", [folder])?;
            Ok(())
        })
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
            "INSERT INTO saved_titles (key, title, kind, poster, year, rating) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(key) DO UPDATE SET title = excluded.title, poster = coalesce(excluded.poster, poster),
                 year = coalesce(excluded.year, year), rating = coalesce(excluded.rating, rating)",
            params![t.key, t.title, t.kind, t.poster, t.year, t.rating],
        )?;
        Ok(())
    }

    /// Fill the year and rating of a saved title (a no-op if it isn't saved); keeps stored values when `None`.
    pub fn set_saved_facts(&self, key: &str, year: Option<i32>, rating: Option<f64>) -> Result<()> {
        self.conn.execute(
            "UPDATE saved_titles SET year = coalesce(?2, year), rating = coalesce(?3, rating) WHERE key = ?1",
            params![key, year, rating],
        )?;
        Ok(())
    }

    /// A title from a Stremio import, dated when it was added there (ISO 8601 `at`) so the "Added" order follows Stremio.
    pub fn import_saved_title(&self, t: &SavedTitle, at: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO saved_titles (key, title, kind, poster, saved_at) VALUES (?1, ?2, ?3, ?4, coalesce(unixepoch(?5), unixepoch()))
             ON CONFLICT(key) DO UPDATE SET title = excluded.title, poster = coalesce(excluded.poster, poster)",
            params![t.key, t.title, t.kind, t.poster, at],
        )?;
        Ok(())
    }

    /// Remembered (`unsaved:<key>`), so an account sync or import doesn't bring it back.
    pub fn remove_saved_title(&self, key: &str) -> Result<()> {
        self.transaction(|| {
            self.conn.execute("DELETE FROM saved_titles WHERE key = ?1", [key])?;
            self.conn.execute("DELETE FROM list_items WHERE title_key = ?1", [key])?;
            self.set_setting(&format!("unsaved:{key}"), "1")
        })
    }

    /// Saved titles, newest first; rows with a malformed key or type are skipped.
    pub fn saved_titles(&self) -> Result<Vec<SavedTitle>> {
        // rowid, not key, breaks same-second ties so the order matches insertion order.
        let mut st = self.conn.prepare(
            "SELECT key, title, kind, poster, year, rating,
                 max(coalesce((SELECT max(updated_at) FROM progress WHERE key = s.key OR substr(key, 1, length(s.key) + 1) = s.key || ':'), 0),
                     coalesce((SELECT max(watched_at) FROM watched WHERE key = s.key), 0))
             FROM saved_titles s ORDER BY saved_at DESC, rowid DESC",
        )?;
        let rows = st.query_map([], |r| {
            let at: i64 = r.get(6)?;
            Ok(SavedTitle {
                key: r.get(0)?,
                title: r.get(1)?,
                kind: r.get(2)?,
                poster: r.get(3)?,
                year: r.get(4)?,
                rating: r.get(5)?,
                last_watched: (at > 0).then_some(at),
            })
        })?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter(|t| valid_id(&t.key) && valid_id(&t.kind) && !t.title.trim().is_empty())
            .collect())
    }

    /// New list; the name is trimmed, 1–60 characters, and unique ignoring case.
    pub fn create_list(&self, name: &str) -> Result<i64> {
        self.conn.execute("INSERT INTO lists (name) VALUES (?1)", [list_name(name)?])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_list(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute("UPDATE lists SET name = ?2 WHERE id = ?1", params![id, list_name(name)?])?;
        Ok(())
    }

    /// Its items go with it (cascade); the titles stay saved.
    pub fn delete_list(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM lists WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Add `t` to list `id` (saving it too if needed), or take it out of the list.
    pub fn set_in_list(&self, id: i64, t: &SavedTitle, on: bool) -> Result<()> {
        if !on {
            self.conn.execute("DELETE FROM list_items WHERE list_id = ?1 AND title_key = ?2", params![id, t.key])?;
            return Ok(());
        }
        self.transaction(|| {
            self.add_saved_title(t)?;
            self.conn.execute("INSERT OR IGNORE INTO list_items (list_id, title_key) VALUES (?1, ?2)", params![id, t.key])?;
            Ok(())
        })
    }

    /// Lists by name (case-insensitive), with their title keys.
    pub fn lists(&self) -> Result<Vec<List>> {
        let mut st = self.conn.prepare("SELECT id, name FROM lists ORDER BY name")?;
        let mut lists: Vec<List> =
            st.query_map([], |r| Ok(List { id: r.get(0)?, name: r.get(1)?, keys: Default::default() }))?.collect::<rusqlite::Result<_>>()?;
        let mut st = self.conn.prepare("SELECT list_id, title_key FROM list_items")?;
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, key) = r?;
            if let Some(l) = lists.iter_mut().find(|l| l.id == id) {
                l.keys.insert(key);
            }
        }
        Ok(lists)
    }

    pub fn add_download(&self, d: &Download) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO downloads (path, kind, meta_id, video_id, title) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![d.path.to_string_lossy(), d.kind, d.meta_id, d.video_id, d.title],
        )?;
        Ok(())
    }

    /// A download waiting, running or stopped: `source` (URL or magnet) and request `headers` let it resume after a restart.
    pub fn queue_download(&self, d: &Download, source: &str, headers: &[(String, String)], state: State) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO downloads (path, kind, meta_id, video_id, title, status, source, headers) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![d.path.to_string_lossy(), d.kind, d.meta_id, d.video_id, d.title, state.as_str(), source, serde_json::to_string(headers)?],
        )?;
        Ok(())
    }

    pub fn set_download_state(&self, d: &Download, state: State) -> Result<()> {
        self.conn.execute("UPDATE downloads SET status = ?1 WHERE path = ?2", params![state.as_str(), d.path.to_string_lossy()])?;
        Ok(())
    }

    /// The file's validator and size seen when a transfer started; a resume is only safe if they still match.
    pub fn set_download_meta(&self, d: &Download, validator: &str, total: u64) -> Result<()> {
        self.conn.execute("UPDATE downloads SET validator = ?1, total = ?2 WHERE path = ?3", params![validator, total as i64, d.path.to_string_lossy()])?;
        Ok(())
    }

    /// Unfinished downloads, oldest first.
    pub fn download_queue(&self) -> Result<Vec<QueuedDownload>> {
        let mut st = self.conn.prepare("SELECT path, kind, meta_id, video_id, title, source, headers, status, validator, total FROM downloads WHERE status != 'done' ORDER BY added_at, rowid")?;
        let rows = st.query_map([], |r| {
            let d = Download { path: PathBuf::from(r.get::<_, String>(0)?), kind: r.get(1)?, meta_id: r.get(2)?, video_id: r.get(3)?, title: r.get(4)? };
            let headers = serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default();
            Ok(QueuedDownload { d, source: r.get(5)?, headers, state: State::from_db(&r.get::<_, String>(7)?), validator: r.get(8)?, total: r.get::<_, i64>(9)? as u64 })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
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
        let mut st = self.conn.prepare("SELECT path, kind, meta_id, video_id, title FROM downloads WHERE status = 'done' ORDER BY added_at DESC, rowid DESC")?;
        let rows = st.query_map([], |r| {
            Ok(Download { path: PathBuf::from(r.get::<_, String>(0)?), kind: r.get(1)?, meta_id: r.get(2)?, video_id: r.get(3)?, title: r.get(4)? })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// All user data as one versioned JSON document: addons (in priority order), saved titles, progress,
    /// watched marks and settings. No account key, no download or other local file paths.
    pub fn export_json(&self) -> Result<String> {
        let rows = |sql: &str, f: fn(&rusqlite::Row) -> rusqlite::Result<Value>| -> Result<Vec<Value>> {
            Ok(self.conn.prepare(sql)?.query_map([], f)?.collect::<rusqlite::Result<_>>()?)
        };
        let addons: Vec<Value> = self.addons()?.into_iter().map(|(url, m)| json!({"url": url, "manifest": m})).collect();
        let saved = rows("SELECT key, title, kind, poster, saved_at FROM saved_titles ORDER BY saved_at, rowid", |r| {
            Ok(json!({"key": r.get::<_, String>(0)?, "title": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?, "poster": r.get::<_, Option<String>>(3)?, "saved_at": r.get::<_, i64>(4)?}))
        })?;
        // Same rule as import: local-file rows (download resume points, `file:` keys) are left out.
        let mut progress = rows("SELECT key, title, source, position, duration, poster, updated_at FROM progress WHERE source NOT IN (SELECT path FROM downloads) ORDER BY updated_at", |r| {
            Ok(json!({"key": r.get::<_, String>(0)?, "title": r.get::<_, String>(1)?, "source": r.get::<_, String>(2)?, "position": r.get::<_, f64>(3)?,
                "duration": r.get::<_, f64>(4)?, "poster": r.get::<_, Option<String>>(5)?, "updated_at": r.get::<_, i64>(6)?}))
        })?;
        progress.retain(|p| resumable_source(p["key"].as_str().unwrap_or_default(), p["source"].as_str().unwrap_or_default()));
        let watched = rows("SELECT key, title, poster, episodes, watched_at FROM watched ORDER BY watched_at", |r| {
            Ok(json!({"key": r.get::<_, String>(0)?, "title": r.get::<_, String>(1)?, "poster": r.get::<_, Option<String>>(2)?, "episodes": r.get::<_, u32>(3)?, "watched_at": r.get::<_, i64>(4)?}))
        })?;
        let marks = rows("SELECT substr(key, 9) FROM settings WHERE key LIKE 'watched:%' ORDER BY key", |r| Ok(Value::String(r.get(0)?)))?;
        let mut settings = serde_json::Map::new();
        for k in EXPORT_SETTINGS {
            if let Some(v) = self.setting(k)? {
                settings.insert(k.into(), v.into());
            }
        }
        Ok(serde_json::to_string_pretty(&json!({"format": EXPORT_FORMAT, "version": 1, "addons": addons, "saved": saved, "progress": progress,
            "watched": watched, "watched_marks": marks, "settings": settings}))?)
    }

    /// Merge an export into this database in one transaction; nothing is deleted and older progress never
    /// replaces newer. The file is untrusted: invalid entries are dropped. Returns a summary of what was merged.
    pub fn import_json(&self, text: &str) -> Result<String> {
        anyhow::ensure!(text.len() as u64 <= MAX_IMPORT_BYTES, "file is over 16 MiB");
        let doc: Value = serde_json::from_str(text).map_err(|e| anyhow::anyhow!("not valid JSON: {e}"))?;
        anyhow::ensure!(doc["format"] == EXPORT_FORMAT, "not a stremflix-rs export");
        anyhow::ensure!(doc["version"] == 1, "unsupported export version {}", doc["version"]);
        let list = |k: &str| doc[k].as_array().map(Vec::as_slice).unwrap_or_default();
        let text = |v: &Value, k: &str| v[k].as_str().map(str::to_owned);
        // Titles are shown as is, so they get the same cleaning and cap as addon text.
        let title = |v: &Value| text(v, "title").map(|t| sanitize_text(&t, 300));
        let poster = |v: &Value| text(v, "poster").filter(|p| p.len() <= 2048 && safe_http_url(p).is_some());
        let int = |v: &Value, k: &str| v[k].as_i64().unwrap_or(0).max(0);
        let (mut addons, mut saved, mut progress, mut watched, mut settings) = (0, 0, 0, 0, 0);
        let total = list("addons").len() + list("saved").len() + list("progress").len() + list("watched").len() + list("watched_marks").len();
        self.transaction(|| {
            for a in list("addons") {
                let (Some(url), Ok(m)) = (text(a, "url"), parse_manifest(&serde_json::to_vec(&a["manifest"])?)) else { continue };
                if addon_base(&url).is_ok() {
                    self.add_addon(&url, &m)?;
                    addons += 1;
                }
            }
            for t in list("saved") {
                let (Some(key), Some(title), Some(kind)) = (text(t, "key"), title(t), text(t, "kind")) else { continue };
                if valid_id(&key) && valid_id(&kind) && !title.trim().is_empty() {
                    self.conn.execute(
                        "INSERT INTO saved_titles (key, title, kind, poster, saved_at) VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(key) DO UPDATE SET title = excluded.title, poster = coalesce(excluded.poster, poster)",
                        params![key, title, kind, poster(t), int(t, "saved_at")],
                    )?;
                    saved += 1;
                }
            }
            for p in list("progress") {
                let (Some(key), Some(title), Some(source)) = (text(p, "key"), title(p), text(p, "source")) else { continue };
                let (pos, dur) = (p["position"].as_f64().unwrap_or(-1.0), p["duration"].as_f64().unwrap_or(-1.0));
                if valid_id(&key) && resumable_source(&key, &source) && pos.is_finite() && dur.is_finite() && pos >= 0.0 && dur >= 0.0 {
                    self.conn.execute(
                        "INSERT INTO progress (key, title, source, position, duration, poster, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(key) DO UPDATE SET title = excluded.title, source = excluded.source, position = excluded.position,
                             duration = excluded.duration, poster = coalesce(excluded.poster, poster), updated_at = excluded.updated_at
                         WHERE excluded.updated_at > progress.updated_at",
                        params![key, title, source, pos, dur, poster(p), int(p, "updated_at")],
                    )?;
                    progress += 1;
                }
            }
            for w in list("watched") {
                let (Some(key), Some(title)) = (text(w, "key"), title(w)) else { continue };
                if valid_id(&key) && !title.trim().is_empty() {
                    self.conn.execute(
                        "INSERT INTO watched (key, title, poster, episodes, watched_at) VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(key) DO UPDATE SET title = excluded.title, poster = coalesce(excluded.poster, poster),
                             episodes = excluded.episodes, watched_at = excluded.watched_at WHERE excluded.watched_at > watched.watched_at",
                        params![key, title, poster(w), int(w, "episodes").min(u32::MAX as i64), int(w, "watched_at")],
                    )?;
                    watched += 1;
                }
            }
            for k in list("watched_marks").iter().filter_map(Value::as_str).filter(|k| valid_id(k)) {
                self.set_setting(&format!("watched:{k}"), "1")?;
                watched += 1;
            }
            for k in EXPORT_SETTINGS {
                if let Some(v) = doc["settings"][k].as_str().filter(|v| v.len() <= 256) {
                    self.set_setting(k, v)?;
                    settings += 1;
                }
            }
            Ok(())
        })?;
        let merged = addons + saved + progress + watched;
        Ok(format!("Imported {addons} addons, {saved} saved titles, {progress} progress, {watched} watched, {settings} settings; skipped {} invalid", total - merged.min(total)))
    }

    /// Write [`Db::export_json`] to `dir/stremflix-rs-<unix time>.json` and return the path.
    pub fn export_file(&self, dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let path = dir.join(format!("stremflix-rs-{now}.json"));
        std::fs::write(&path, self.export_json()?)?;
        Ok(path)
    }

    /// Read an export file (at most 16 MiB) and merge it with [`Db::import_json`].
    pub fn import_file(&self, path: &Path) -> Result<String> {
        use std::io::Read;
        let mut text = String::new();
        std::fs::File::open(path)?.take(MAX_IMPORT_BYTES + 1).read_to_string(&mut text)?;
        self.import_json(&text)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    /// Mark this run as live; true if the previous run never cleared the mark (crash, kill, power loss).
    pub fn begin_session(&self) -> Result<bool> {
        let crashed = self.setting("running")?.is_some();
        self.set_setting("running", "1")?;
        Ok(crashed)
    }

    pub fn end_session(&self) -> Result<()> {
        self.remove_setting("running")
    }

    /// `PRAGMA quick_check`: None when the database is intact, else the first problem it reports.
    pub fn quick_check(&self) -> Result<Option<String>> {
        let r: String = self.conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        Ok((r != "ok").then_some(r))
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
    /// Remembered (`unprogress:<key>` = unix time), so an account sync doesn't bring back a resume point that is not newer.
    pub fn remove_progress(&self, key: &str) -> Result<()> {
        self.transaction(|| {
            self.conn.execute("DELETE FROM progress WHERE key = ?1", [key])?;
            self.mark_unprogress(key)
        })
    }

    fn mark_unprogress(&self, key: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('unprogress:' || ?1, CAST(unixepoch() AS TEXT))", [key])?;
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
        // Removed or finished here at or after `at`: stays gone.
        // shortcut: compares this machine's clock with the account's `_mtime`, so clock skew can revive or hide a copy; upgrade if the API ever gives a server time.
        let gone: Option<i64> = self.conn.query_row("SELECT CAST(value AS INTEGER) FROM settings WHERE key = 'unprogress:' || ?1", [&p.key], |r| r.get(0)).optional()?;
        let when: Option<i64> = self.conn.query_row("SELECT unixepoch(?1)", [at], |r| r.get(0))?;
        if gone.is_some_and(|g| when.unwrap_or(0) <= g) {
            return Ok(());
        }
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
    /// Unwatching an episode also drops its imported series (`<series key>:...`) from Watch It Again,
    /// as its imported count no longer holds; watching one of its episodes lets a sync bring it back.
    pub fn set_watched(&self, key: &str, on: bool) -> Result<()> {
        self.set_watched_many(&[key.to_string()], on)
    }

    /// [`Db::set_watched`] for many keys (a season or series of episodes), in one transaction.
    pub fn set_watched_many(&self, keys: &[String], on: bool) -> Result<()> {
        self.transaction(|| keys.iter().try_for_each(|k| self.set_watched_one(k, on)))
    }

    fn set_watched_one(&self, key: &str, on: bool) -> Result<()> {
        let k = format!("watched:{key}");
        {
            if on {
                self.set_setting(&k, "1")?;
                self.conn.execute("DELETE FROM progress WHERE key = ?1", [key])?;
                self.mark_unprogress(key)?;
                self.conn.execute("DELETE FROM settings WHERE key = ?1", [format!("unwatched:{key}")])?;
                // The series' tombstone, `unwatched:<series key>` (10-char prefix).
                self.conn.execute("DELETE FROM settings WHERE key LIKE 'unwatched:%' AND substr(?1, 1, length(key) - 9) = substr(key, 11) || ':'", [key])?;
            } else {
                self.conn.execute("DELETE FROM settings WHERE key = ?1", [&k])?;
                self.set_setting(&format!("unwatched:{key}"), "1")?;
                // Also clears a title imported from Stremio, or the series an episode belongs to.
                let parent = "(key = ?1 OR substr(?1, 1, length(key) + 1) = key || ':')";
                self.conn.execute(&format!("INSERT OR REPLACE INTO settings (key, value) SELECT 'unwatched:' || key, '1' FROM watched WHERE {parent}"), [key])?;
                self.conn.execute(&format!("DELETE FROM watched WHERE {parent}"), [key])?;
            }
            Ok(())
        }
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

    /// All resume points and when each was saved, in Stremio's ISO 8601 form (`2026-01-02T03:04:05.000Z`).
    pub fn progress_times(&self) -> Result<Vec<(Progress, String)>> {
        let mut st = self.conn.prepare(
            "SELECT key, title, source, position, duration, poster, strftime('%Y-%m-%dT%H:%M:%fZ', updated_at, 'unixepoch') FROM progress",
        )?;
        Ok(st.query_map([], |r| Ok((row_progress(r)?, r.get(6)?)))?.filter_map(|r| r.ok()).collect())
    }

    /// The current time, in the same form as [`Db::progress_times`].
    pub fn now_iso(&self) -> Result<String> {
        Ok(self.conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |r| r.get(0))?)
    }
}

fn row_progress(r: &rusqlite::Row) -> rusqlite::Result<Progress> {
    Ok(Progress { key: r.get(0)?, title: r.get(1)?, source: r.get(2)?, position: r.get(3)?, duration: r.get(4)?, poster: r.get(5)? })
}

const HISTORY_MAX: usize = 10;
const HISTORY_QUERY_MAX: usize = 100;

/// Puts `q` first in `h`: trimmed, capped in length, deduped case-insensitively, list capped.
pub fn push_history(h: &mut Vec<String>, q: &str) {
    let q: String = q.trim().chars().take(HISTORY_QUERY_MAX).collect();
    if q.is_empty() {
        return;
    }
    h.retain(|x| x.to_lowercase() != q.to_lowercase());
    h.insert(0, q);
    h.truncate(HISTORY_MAX);
}

pub fn encode_history(h: &[String]) -> String {
    h.join("\n")
}

pub fn decode_history(s: &str) -> Vec<String> {
    let mut h = Vec::new();
    for l in s.lines().rev() {
        push_history(&mut h, l);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unclean_exit_detected_and_cleared() {
        let dir = std::env::temp_dir().join(format!("stremflix-session-{}", std::process::id()));
        let path = dir.join("t.db");
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&path).unwrap();
        assert!(!db.begin_session().unwrap());
        assert_eq!(db.quick_check().unwrap(), None);
        drop(db); // no end_session: simulates a crash
        let db = Db::open(&path).unwrap();
        assert!(db.begin_session().unwrap());
        db.end_session().unwrap();
        assert!(!db.begin_session().unwrap());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_push_dedupe_cap_roundtrip() {
        let mut h = Vec::new();
        push_history(&mut h, "  Matrix ");
        push_history(&mut h, "   ");
        push_history(&mut h, "dune");
        push_history(&mut h, "MATRIX");
        assert_eq!(h, ["MATRIX", "dune"]);
        for i in 0..20 {
            push_history(&mut h, &format!("q{i}"));
        }
        assert_eq!(h.len(), HISTORY_MAX);
        assert_eq!(h[0], "q19");
        push_history(&mut h, &"x".repeat(500));
        assert_eq!(h[0].len(), HISTORY_QUERY_MAX);
        assert_eq!(decode_history(&encode_history(&h)), h);
        assert!(decode_history("").is_empty());
    }
}
