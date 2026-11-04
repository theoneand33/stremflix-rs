//! Import addons, library titles, watch progress and watched titles from a local Stremio installation
//! or (via [`from_account`]) a Stremio account.
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

use crate::addon::{Manifest, Stream, Video, addon_base, parse_manifest, safe_http_url, sanitize_text, valid_id};
use crate::db::{Db, Progress, SavedTitle, Watched};

#[derive(Debug, Default)]
pub struct Import {
    pub addons: Vec<(String, Manifest)>,
    /// Progress and when it was last watched (ISO 8601).
    pub progress: Vec<(Progress, String)>,
    pub watched: Vec<(Watched, String)>,
    /// Series key (`<type>:<id>`) and its raw watched-episodes bitfield, mapped to episodes once their list loads.
    pub bitfields: Vec<(String, String)>,
    /// Library titles (not removed, not history-only), saved with "Add to library", and when they were added (ISO 8601).
    pub saved: Vec<(SavedTitle, String)>,
}

impl Import {
    /// Save into the app DB. Already installed addons are kept as they are.
    pub fn save(&self, db: &Db) -> Result<()> {
        db.transaction(|| self.save_rows(db))
    }

    fn save_rows(&self, db: &Db) -> Result<()> {
        let have: Vec<String> = db.addons()?.into_iter().map(|(u, _)| u).collect();
        for (u, m) in self.addons.iter().filter(|(u, _)| !have.contains(u)) {
            // Not one removed here.
            if db.setting(&format!("unaddon:{u}"))?.is_none() {
                db.add_addon(u, m)?;
            }
        }
        for (p, at) in &self.progress {
            db.import_progress(p, at)?;
        }
        // Titles unwatched or removed here stay that way.
        let kept = |kind: &str, key: &str| db.setting(&format!("{kind}:{key}")).map(|v| v.is_none());
        for (w, at) in &self.watched {
            if kept("unwatched", &w.key)? {
                db.save_watched(w, at)?;
            }
        }
        // Episodes unwatched here are skipped when the bitfield is applied (`Db::apply_watched_bitfield`).
        for (key, field) in &self.bitfields {
            db.set_setting(&format!("bitfield:{key}"), field)?;
        }
        for (t, at) in &self.saved {
            if kept("unsaved", &t.key)? {
                db.import_saved_title(t, at)?;
            }
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
                return name.contains("stremio") && !name.contains("stremflix-rs") || name.contains("smart code");
            }
            // Skip video caches and downloads: large and never hold localStorage. Never skip the root itself.
            e.depth() == 0 || !(e.file_type().is_dir() && (name.contains("cache") || name.contains("server") || name == "downloads"))
        });
        found.extend(walk.filter_map(|e| e.ok()).map(|e| e.into_path()).filter(|p| is_storage(p)));
    }
    // XDG_DATA_HOME is usually ~/.local/share, already a root.
    found.sort();
    found.dedup();
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

/// Copy `src` (a file plus its SQLite `-wal`/`-shm`, or a LevelDB folder without its LOCK) into `dir`.
fn copy_to(src: &Path, dir: &Path) -> Result<PathBuf> {
    if src.is_dir() {
        for e in std::fs::read_dir(src)? {
            let e = e?;
            if e.file_type()?.is_file() && e.file_name() != "LOCK" {
                std::fs::copy(e.path(), dir.join(e.file_name()))?;
            }
        }
        return Ok(dir.to_path_buf());
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
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!("stremflix-rs-import-{}-{nanos}", std::process::id()));
    // Owner-only and must not exist yet: the copy holds Stremio's auth key.
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(&dir)?;
    let res = copy_to(src, &dir).and_then(|copy| if src.is_dir() { read_leveldb(&copy) } else { read_sqlite(&copy) });
    let _ = std::fs::remove_dir_all(&dir);
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
    /// When it was added to the library.
    #[serde(rename = "_ctime")]
    ctime: Option<String>,
    state: ItemState,
    #[serde(default)]
    removed: bool,
    /// Only watched, never added to the library.
    #[serde(default)]
    temp: bool,
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

/// Anchor video id, anchor length and bits (LSB first) of a stremio-core bitfield string.
fn decode_bitfield(field: &str) -> Option<(&str, usize, Vec<u8>)> {
    let mut parts = field.rsplitn(3, ':');
    let (b64, len, anchor) = (parts.next()?, parts.next()?.parse::<usize>().ok()?, parts.next()?);
    let z = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let mut bits = Vec::new();
    // Cap: the bitfield is one bit per episode, so even huge shows stay tiny.
    flate2::read::ZlibDecoder::new(&z[..]).take(1 << 20).read_to_end(&mut bits).ok()?;
    Some((anchor, len, bits))
}

fn bit(bits: &[u8], i: usize) -> bool {
    bits.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 == 1)
}

/// Number of watched episodes in a stremio-core bitfield string.
pub fn watched_episodes(field: &str) -> u32 {
    decode_bitfield(field).map_or(0, |(_, len, bits)| (0..len).filter(|&i| bit(&bits, i)).count() as u32)
}

/// Ids of `videos` marked watched in a stremio-core bitfield. Like stremio-core, bits follow the videos
/// sorted by (season, episode) with specials (season 0) last, then unnumbered videos by release date,
/// re-aligned on the anchor; an unknown anchor marks nothing.
pub fn watched_videos(field: &str, videos: &[Video]) -> Vec<String> {
    let Some((anchor, len, bits)) = decode_bitfield(field) else { return Vec::new() };
    let mut ids: Vec<&Video> = videos.iter().collect();
    ids.sort_by_key(|v| (v.season.zip(v.episode).map(|(s, e)| (s == 0, s, e)).ok_or(()), v.released.is_none(), v.released.as_deref()));
    ids.dedup_by(|a, b| a.id == b.id);
    let Some(at) = ids.iter().position(|v| v.id == anchor) else { return Vec::new() };
    // Bit `i + offset` of the stored field is video `i` now (episodes may have been added since).
    let offset = len as isize - at as isize - 1;
    ids.iter()
        .enumerate()
        .filter(|&(i, _)| usize::try_from(i as isize + offset).is_ok_and(|j| j < len && bit(&bits, j)))
        .map(|(_, v)| v.id.clone())
        .collect()
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
                // Like stremio-core's library and Continue Watching: `other` items are skipped.
                && it.kind != "other"
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
        if !it.removed && !it.temp {
            let added = it.ctime.clone().unwrap_or(it.mtime.clone());
            imp.saved.push((SavedTitle { key: key.clone(), title: title.clone(), kind: it.kind.clone(), poster: poster.clone() }, added));
        }
        let video = s.video_id.clone().unwrap_or(it.id.clone());
        if s.time_offset > 0
            && let Some(source) = streams.get(&(it.id.clone(), video.clone()))
            && valid_id(&video)
        {
            let p = Progress {
                // `<type>:<video id>` like local progress, so a series' episode resumes on its own.
                key: format!("{}:{video}", it.kind),
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
            if episodes > 0 {
                imp.bitfields.push((key.clone(), s.watched.clone().unwrap_or_default()));
            }
            imp.watched.push((Watched { key, title, poster, episodes }, at));
        }
    }
    Ok(imp)
}

/// Build an import from Stremio account API results: `addonCollectionGet` and `datastoreGet` of `libraryItem`.
/// Shaped like localStorage and parsed by [`parse`], so it gets the same validation.
pub fn from_account(addons: &Value, library: &Value) -> Result<Import> {
    // An unexpected shape must fail, not sync as empty.
    if !addons["addons"].is_array() || !library.is_array() {
        bail!("Stremio API: unexpected addon or library response");
    }
    let list = library.as_array().into_iter().flatten();
    let items: serde_json::Map<String, Value> = list.filter_map(|it| Some((it["_id"].as_str()?.to_string(), it.clone()))).collect();
    if items.is_empty() && library.as_array().is_some_and(|l| !l.is_empty()) {
        bail!("Stremio API: unexpected library item shape");
    }
    let kv = HashMap::from([
        ("profile".to_string(), serde_json::json!({ "addons": addons["addons"] }).to_string()),
        ("library".to_string(), serde_json::json!({ "items": items }).to_string()),
    ]);
    parse(&kv)
}

/// Local state to upload to a Stremio account, read from the DB before the requests run.
#[derive(Debug, Default)]
pub struct Upload {
    pub addons: Vec<(String, Manifest)>,
    pub saved: Vec<SavedTitle>,
    /// Resume points and when each was saved (ISO 8601).
    pub progress: Vec<(Progress, String)>,
    /// Keys marked watched here or imported (see [`Db::watched_keys`]).
    pub watched: std::collections::HashSet<String>,
    /// `_mtime` of every changed item (ISO 8601).
    pub now: String,
}

impl Upload {
    pub fn read(db: &Db) -> Result<Self> {
        let (addons, saved, progress) = (db.addons()?, db.saved_titles()?, db.progress_times()?);
        Ok(Upload { addons, saved, progress, watched: db.watched_keys()?, now: db.now_iso()? })
    }
}

/// `addons` for `addonCollectionSet`: local addons in local order as `{transportUrl, transportName, manifest, flags}`,
/// then the account's addons missing here, unchanged (they are never deleted). A local addon keeps the
/// account's flags and the manifest fields this app doesn't store (logo, ...).
pub fn addon_collection(local: &[(String, Manifest)], remote: &Value) -> Result<Vec<Value>> {
    let remote = remote["addons"].as_array().context("Stremio API: unexpected addon response")?;
    let find = |u: &str| remote.iter().find(|a| a["transportUrl"] == u);
    let mut out = Vec::new();
    for (url, m) in local {
        let old = find(url);
        let mut manifest = old.map(|a| a["manifest"].clone()).filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
        if let (Some(o), Value::Object(l)) = (manifest.as_object_mut(), serde_json::to_value(m)?) {
            o.extend(l);
        }
        let flags = old.map(|a| a["flags"].clone()).filter(Value::is_object);
        out.push(serde_json::json!({
            "transportUrl": url, "transportName": "", "manifest": manifest,
            "flags": flags.unwrap_or_else(|| serde_json::json!({"official": false, "protected": false})),
        }));
    }
    let is_local = |a: &Value| local.iter().any(|(u, _)| a["transportUrl"] == u.as_str());
    out.extend(remote.iter().filter(|a| !is_local(a)).cloned());
    Ok(out)
}

/// A new history-only `libraryItem` in stremio-core's full shape, which its clients need to read it.
fn new_item(kind: &str, id: &str, name: &str, poster: &Option<String>, now: &str) -> Value {
    serde_json::json!({
        "_id": id, "name": name, "type": kind, "poster": poster, "posterShape": "poster",
        "removed": true, "temp": true, "_ctime": now, "_mtime": now,
        "state": {"lastWatched": null, "timeWatched": 0, "timeOffset": 0, "overallTimeWatched": 0, "timesWatched": 0,
                  "flaggedWatched": 0, "duration": 0, "video_id": null, "watched": null, "noNotif": false},
        "behaviorHints": {"defaultVideoId": null, "featuredVideoId": null, "hasScheduledVideos": false},
    })
}

/// `changes` for `datastorePut` on `libraryItem`: only items that differ from the account's `remote` items,
/// each with `_mtime` = `now`. Account-only items aren't sent, so nothing is deleted there.
/// Progress newer than the account's `lastWatched` wins; saved titles are un-removed; movies watched here are flagged.
/// shortcut: episodes watched here aren't written to the series' `watched` bitfield; upgrade when one is built locally.
pub fn library_changes(up: &Upload, remote: &Value) -> Vec<Value> {
    let mut items: std::collections::BTreeMap<String, Value> = remote
        .as_array()
        .into_iter()
        .flatten()
        .filter(|it| it["state"].is_object())
        .filter_map(|it| Some((format!("{}:{}", it["type"].as_str()?, it["_id"].as_str()?), it.clone())))
        .collect();
    let mut changed = std::collections::BTreeSet::new();
    for t in &up.saved {
        let Some(id) = t.key.strip_prefix(&format!("{}:", t.kind)) else { continue };
        let it = items.entry(t.key.clone()).or_insert_with(|| new_item(&t.kind, id, &t.title, &t.poster, &up.now));
        if it["removed"] != false || it["temp"] != false {
            (it["removed"], it["temp"]) = (false.into(), false.into());
            changed.insert(t.key.clone());
        }
    }
    for (p, at) in &up.progress {
        // The title it belongs to: an episode's key is `<series key>:<season>:<episode>`.
        let known = items.keys().filter(|k| p.key == **k || p.key.starts_with(&format!("{k}:"))).max_by_key(|k| k.len()).cloned();
        let key = match known {
            Some(k) => k,
            // An episode's progress title is the episode's, so only movies are added from progress.
            None if p.key.strip_prefix("movie:").is_some_and(valid_id) => {
                items.insert(p.key.clone(), new_item("movie", &p.key[6..], &p.title, &p.poster, &up.now));
                p.key.clone()
            }
            None => continue,
        };
        let it = items.get_mut(&key).expect("found or inserted above");
        let Some(video) = p.key.strip_prefix(&format!("{}:", it["type"].as_str().unwrap_or_default())) else { continue };
        let video = video.to_string();
        let s = &mut it["state"];
        if p.position <= 0.0 || s["lastWatched"].as_str().is_some_and(|l| l >= at.as_str()) {
            continue;
        }
        s["timeOffset"] = ((p.position * 1000.0) as u64).into();
        s["duration"] = ((p.duration * 1000.0) as u64).into();
        s["lastWatched"] = at.as_str().into();
        s["video_id"] = video.into();
        changed.insert(key);
    }
    for k in up.watched.iter().filter(|k| k.starts_with("movie:")) {
        let Some(s) = items.get_mut(k).map(|it| &mut it["state"]) else { continue };
        if s["flaggedWatched"].as_u64().unwrap_or(0) == 0 && s["timesWatched"].as_u64().unwrap_or(0) == 0 {
            (s["flaggedWatched"], s["timesWatched"]) = (1.into(), 1.into());
            changed.insert(k.clone());
        }
    }
    changed
        .into_iter()
        .filter_map(|k| items.remove(&k))
        .map(|mut it| {
            it["_mtime"] = up.now.as_str().into();
            it
        })
        .collect()
}
