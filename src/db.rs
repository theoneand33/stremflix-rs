//! SQLite persistence: addons, library folders/items, settings, watch progress.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

use crate::addon::{Manifest, parse_manifest};
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

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS addons (transport_url TEXT PRIMARY KEY, manifest TEXT NOT NULL, installed_at INTEGER NOT NULL DEFAULT (unixepoch()));
             CREATE TABLE IF NOT EXISTS library_folders (path TEXT PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS library_items (path TEXT PRIMARY KEY, folder TEXT NOT NULL, title TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS progress (key TEXT PRIMARY KEY, title TEXT NOT NULL, source TEXT NOT NULL,
                 position REAL NOT NULL, duration REAL NOT NULL, updated_at INTEGER NOT NULL DEFAULT (unixepoch()));",
        )?;
        // Migrate databases created before posters were stored; fails harmlessly if the column exists.
        let _ = conn.execute("ALTER TABLE progress ADD COLUMN poster TEXT", []);
        Ok(Self { conn })
    }

    pub fn add_addon(&self, transport_url: &str, manifest: &Manifest) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO addons (transport_url, manifest) VALUES (?1, ?2)",
            params![transport_url, serde_json::to_string(manifest)?],
        )?;
        Ok(())
    }

    pub fn remove_addon(&self, transport_url: &str) -> Result<()> {
        self.conn.execute("DELETE FROM addons WHERE transport_url = ?1", [transport_url])?;
        Ok(())
    }

    /// Installed addons; stored manifests are re-validated and corrupt rows skipped.
    pub fn addons(&self) -> Result<Vec<(String, Manifest)>> {
        let mut st = self.conn.prepare("SELECT transport_url, manifest FROM addons ORDER BY installed_at, transport_url")?;
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

    pub fn library_folders(&self) -> Result<Vec<PathBuf>> {
        let mut st = self.conn.prepare("SELECT path FROM library_folders ORDER BY path")?;
        Ok(st.query_map([], |r| r.get::<_, String>(0))?.filter_map(|r| r.ok()).map(PathBuf::from).collect())
    }

    pub fn library_items(&self) -> Result<Vec<LibraryItem>> {
        let mut st = self.conn.prepare("SELECT path, title FROM library_items ORDER BY path")?;
        let rows = st.query_map([], |r| {
            Ok(LibraryItem { path: PathBuf::from(r.get::<_, String>(0)?), title: r.get(1)? })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", [key, value])?;
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

    pub fn progress(&self, key: &str) -> Result<Option<Progress>> {
        Ok(self
            .conn
            .query_row("SELECT key, title, source, position, duration, poster FROM progress WHERE key = ?1", [key], row_progress)
            .optional()?)
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
