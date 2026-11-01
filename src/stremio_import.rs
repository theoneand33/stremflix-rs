//! Import addons, watch progress and watched titles from a local Stremio installation.
//!
//! Stremio 5 (stremio-web + stremio-core) keeps its state as JSON in the webview's localStorage
//! under the keys `profile` (addons), `library`/`library_recent` (titles + progress) and `streams`
//! (last stream per video). WebKit (Linux, macOS) stores localStorage as a SQLite `ItemTable`;
//! Chromium (Windows WebView2, Stremio 4's QtWebEngine) as a LevelDB. Both are read from a copy,
//! so a running Stremio is never locked or modified.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

use crate::addon::{Manifest, Stream, addon_base, parse_manifest, safe_http_url, sanitize_text, valid_id};
use crate::db::{Db, Progress, Watched};

#[derive(Debug, Default)]
pub struct Import {
    pub addons: Vec<(String, Manifest)>,
    /// Progress and when it was last watched (ISO 8601).
    pub progress: Vec<(Progress, String)>,
    pub watched: Vec<(Watched, String)>,
}

impl Import {
    /// Save into the app DB. Already installed addons are kept as they are.
    pub fn save(&self, db: &Db) -> Result<()> {
        let have: Vec<String> = db.addons()?.into_iter().map(|(u, _)| u).collect();
        for (u, m) in self.addons.iter().filter(|(u, _)| !have.contains(u)) {
            db.add_addon(u, m)?;
        }
        for (p, at) in &self.progress {
            db.import_progress(p, at)?;
        }
        for (w, at) in &self.watched {
            db.save_watched(w, at)?;
        }
        Ok(())
    }
}

/// Find and read Stremio's storage. `path` overrides auto-detection: a storage file, a
/// LevelDB folder, or any folder to search.
pub fn import(path: Option<&Path>) -> Result<(PathBuf, Import)> {
    let found = match path {
        Some(p) => candidates(&[p.to_path_buf()], false),
        None => candidates(&default_roots(), true),
    };
    if found.is_empty() {
        bail!("no Stremio data found{}", path.map(|p| format!(" in {}", p.display())).unwrap_or_default());
    }
    let mut last_err = anyhow!("no Stremio profile found");
    for c in found {
        match read_storage(&c).and_then(|kv| parse(&kv)) {
            Ok(imp) => return Ok((c, imp)),
            Err(e) => last_err = e.context(c.display().to_string()),
        }
    }
    Err(last_err)
}

/// Folders where Stremio (any version) keeps app data on Linux, macOS and Windows.
fn default_roots() -> Vec<PathBuf> {
    let env = |k| std::env::var_os(k).map(PathBuf::from);
    let mut roots = Vec::new();
    if let Some(h) = env("HOME") {
        roots.extend(["Library/WebKit", "Library/Application Support", ".var/app", ".local/share"].map(|d| h.join(d)));
    }
    roots.extend(env("XDG_DATA_HOME"));
    for k in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(d) = env(k) {
            roots.push(d.join("Programs"));
            roots.push(d);
        }
    }
    roots
}

/// localStorage files/folders under `roots`, newest first. With `stremio_only`, only children
/// of a root that look like Stremio's are searched.
fn candidates(roots: &[PathBuf], stremio_only: bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        if is_storage(root) {
            found.push(root.clone());
            continue;
        }
        let walk = WalkDir::new(root).max_depth(8).into_iter().filter_entry(|e| {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if e.depth() == 1 && stremio_only {
                return name.contains("stremio") && !name.contains("stremio-lite") || name.contains("smart code");
            }
            // Skip video caches and downloads: large and never hold localStorage.
            !(e.file_type().is_dir() && (name.contains("cache") || name.contains("server") || name == "downloads"))
        });
        found.extend(walk.filter_map(|e| e.ok()).map(|e| e.into_path()).filter(|p| is_storage(p)));
    }
    let mtime = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    found.sort_by_key(|p| std::cmp::Reverse(mtime(p)));
    found
}

fn is_storage(p: &Path) -> bool {
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    if p.is_file() {
        return name == "localstorage.sqlite3" || name.ends_with(".localstorage");
    }
    p.is_dir() && name == "leveldb" && p.parent().and_then(|q| q.file_name()).is_some_and(|n| n == "Local Storage")
}

/// Copy `src` (a file plus its SQLite `-wal`/`-shm`, or a LevelDB folder without its LOCK) to a fresh temp dir.
fn copy_to_temp(src: &Path) -> Result<PathBuf> {
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!("stremio-lite-import-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    if src.is_dir() {
        for e in std::fs::read_dir(src)? {
            let e = e?;
            if e.file_type()?.is_file() && e.file_name() != "LOCK" {
                std::fs::copy(e.path(), dir.join(e.file_name()))?;
            }
        }
        return Ok(dir);
    }
    let name = src.file_name().context("no file name")?.to_string_lossy().into_owned();
    std::fs::copy(src, dir.join(&name))?;
    for ext in ["-wal", "-shm"] {
        let side = src.with_file_name(format!("{name}{ext}"));
        if side.exists() {
            std::fs::copy(&side, dir.join(format!("{name}{ext}")))?;
        }
    }
    Ok(dir.join(name))
}

/// localStorage key/values of the origin that holds a Stremio `profile`.
pub fn read_storage(src: &Path) -> Result<HashMap<String, String>> {
    let copy = copy_to_temp(src)?;
    let res = if src.is_dir() { read_leveldb(&copy) } else { read_sqlite(&copy) };
    let _ = std::fs::remove_dir_all(if src.is_dir() { &copy } else { copy.parent().unwrap_or(&copy) });
    res
}

fn utf16le(b: &[u8]) -> String {
    String::from_utf16_lossy(&b.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect::<Vec<_>>())
}

fn read_sqlite(path: &Path) -> Result<HashMap<String, String>> {
    use rusqlite::types::ValueRef;
    let conn = rusqlite::Connection::open(path)?;
    let mut st = conn.prepare("SELECT key, value FROM ItemTable")?;
    let mut rows = st.query([])?;
    let mut kv = HashMap::new();
    while let Some(r) = rows.next()? {
        let v = match r.get_ref(1)? {
            ValueRef::Blob(b) => utf16le(b), // WebKit stores values as UTF-16LE blobs
            ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
            _ => continue,
        };
        kv.insert(r.get::<_, String>(0)?, v);
    }
    if !kv.contains_key("profile") {
        bail!("no Stremio profile in this storage");
    }
    Ok(kv)
}

/// Chromium string encoding: a 0 prefix means UTF-16LE, 1 means Latin-1.
fn chromium_str(b: &[u8]) -> Option<String> {
    match b.split_first()? {
        (0, rest) => Some(utf16le(rest)),
        (1, rest) => Some(rest.iter().map(|&c| c as char).collect()),
        _ => None,
    }
}

fn read_leveldb(dir: &Path) -> Result<HashMap<String, String>> {
    use rusty_leveldb::LdbIterator;
    let opts = rusty_leveldb::Options { create_if_missing: false, ..Default::default() };
    let mut db = rusty_leveldb::DB::open(dir, opts).map_err(|e| anyhow!("LevelDB: {e}"))?;
    let mut it = db.new_iter().map_err(|e| anyhow!("LevelDB: {e}"))?;
    // Keys are `_<origin>\0<encoded key>`; group by origin.
    let mut origins: HashMap<String, HashMap<String, String>> = HashMap::new();
    while let Some((k, v)) = it.next() {
        let Some(rest) = k.strip_prefix(b"_") else { continue };
        let Some(nul) = rest.iter().position(|&b| b == 0) else { continue };
        let (Some(key), Some(val)) = (chromium_str(&rest[nul + 1..]), chromium_str(&v)) else { continue };
        origins.entry(String::from_utf8_lossy(&rest[..nul]).into_owned()).or_default().insert(key, val);
    }
    // Other origins (e.g. embedded YouTube) may reuse key names; the app origin has the most keys.
    origins.into_values().filter(|kv| kv.contains_key("profile")).max_by_key(|kv| kv.len()).context("no Stremio profile in this storage")
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "_id")]
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    poster: Option<String>,
    #[serde(rename = "_mtime", default)]
    mtime: String,
    state: ItemState,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ItemState {
    last_watched: Option<String>,
    /// Milliseconds.
    time_offset: u64,
    duration: u64,
    times_watched: u64,
    flagged_watched: u64,
    #[serde(rename = "video_id")] // snake_case in Stremio, unlike the rest
    video_id: Option<String>,
    /// Watched-episodes bitfield: `<anchor video id>:<length>:<base64 zlib bits>`.
    watched: Option<String>,
}

/// Number of watched episodes in a stremio-core bitfield string.
pub fn watched_episodes(field: &str) -> u32 {
    let mut parts = field.rsplitn(3, ':');
    let (Some(b64), Some(len)) = (parts.next(), parts.next().and_then(|l| l.parse::<usize>().ok())) else { return 0 };
    let Ok(z) = base64::engine::general_purpose::STANDARD.decode(b64) else { return 0 };
    let mut bits = Vec::new();
    // Cap: the bitfield is one bit per episode, so even huge shows stay tiny.
    if flate2::read::ZlibDecoder::new(&z[..]).take(1 << 20).read_to_end(&mut bits).is_err() {
        return 0;
    }
    (0..len.min(bits.len() * 8)).filter(|i| bits[i / 8] >> (i % 8) & 1 == 1).count() as u32
}

/// Build an import from Stremio's localStorage values. Addon and stream data came from addons,
/// so it goes through the same validation as live addon responses.
pub fn parse(kv: &HashMap<String, String>) -> Result<Import> {
    let json = |k: &str| kv.get(k).and_then(|v| serde_json::from_str::<Value>(v).ok());
    let profile = json("profile").context("no Stremio profile")?;
    let mut imp = Import::default();
    for a in profile["addons"].as_array().into_iter().flatten() {
        let (Some(url), Ok(m)) = (a["transportUrl"].as_str(), parse_manifest(a["manifest"].to_string().as_bytes())) else { continue };
        if addon_base(url).is_ok() {
            imp.addons.push((url.to_string(), m));
        }
    }
    // Last stream per (meta id, video id): what Continue Watching resumes.
    let mut streams = HashMap::new();
    for pair in json("streams").as_ref().and_then(|s| s["items"].as_array()).into_iter().flatten() {
        let (ids, v) = (&pair[0], &pair[1]["stream"]);
        let (Some(meta), Some(video)) = (ids["metaId"].as_str(), ids["videoId"].as_str()) else { continue };
        if let Some(src) = serde_json::from_value::<Stream>(v.clone()).ok().and_then(|s| s.source()) {
            streams.insert((meta.to_string(), video.to_string()), src);
        }
    }
    // `library_recent` is the newer copy of recently changed items, so it wins.
    let mut items: HashMap<String, Item> = HashMap::new();
    for k in ["library", "library_recent"] {
        for v in json(k).as_ref().and_then(|l| l["items"].as_object()).into_iter().flat_map(|o| o.values()) {
            if let Ok(it) = serde_json::from_value::<Item>(v.clone())
                && valid_id(&it.id)
                && valid_id(&it.kind)
                && items.get(&it.id).is_none_or(|old| old.mtime <= it.mtime)
            {
                items.insert(it.id.clone(), it);
            }
        }
    }
    for it in items.into_values() {
        let (key, title) = (format!("{}:{}", it.kind, it.id), sanitize_text(&it.name, 300));
        let poster = it.poster.as_deref().and_then(safe_http_url).map(String::from);
        let s = &it.state;
        let at = s.last_watched.clone().unwrap_or(it.mtime.clone());
        if title.is_empty() {
            continue;
        }
        let video = s.video_id.clone().unwrap_or(it.id.clone());
        if s.time_offset > 0
            && let Some(source) = streams.get(&(it.id.clone(), video))
        {
            let p = Progress {
                key: key.clone(),
                title: title.clone(),
                source: source.clone(),
                position: s.time_offset as f64 / 1000.0,
                duration: s.duration as f64 / 1000.0,
                poster: poster.clone(),
            };
            imp.progress.push((p, at.clone()));
        }
        let episodes = s.watched.as_deref().map(watched_episodes).unwrap_or(0);
        if episodes > 0 || s.flagged_watched > 0 || s.times_watched > 0 {
            let episodes = if it.kind == "movie" { 0 } else { episodes };
            imp.watched.push((Watched { key, title, poster, episodes }, at));
        }
    }
    Ok(imp)
}
