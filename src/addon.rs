//! Stremio addon protocol: types, validation/sanitization, and an HTTP client.
//! All addon output is untrusted: every response goes through `parse_*`, which
//! never panics, rejects structurally invalid data, and drops invalid entries.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;

/// Upper bound on any addon response body.
pub const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_TEXT: usize = 300;
const MAX_LONG_TEXT: usize = 4000;
const MAX_ITEMS: usize = 500;
/// Long-running series have well over `MAX_ITEMS` episodes.
const MAX_VIDEOS: usize = 5000;
/// Cap on genres, cast, directors and links of a meta.
const MAX_TAGS: usize = 30;
/// Max options kept per catalog extra (e.g. genres).
const MAX_OPTIONS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Resource {
    Name(String),
    Full {
        name: String,
        #[serde(default)]
        types: Vec<String>,
        #[serde(default, rename = "idPrefixes")]
        id_prefixes: Option<Vec<String>>,
    },
}

impl Resource {
    pub fn name(&self) -> &str {
        match self {
            Resource::Name(n) | Resource::Full { name: n, .. } => n,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub extra: Vec<ExtraDef>,
    /// Legacy form of `extra`.
    #[serde(default, rename = "extraSupported")]
    pub extra_supported: Vec<String>,
    #[serde(default, rename = "extraRequired")]
    pub extra_required: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtraDef {
    pub name: String,
    #[serde(default, rename = "isRequired")]
    pub is_required: bool,
    /// Allowed values (e.g. genres); non-string entries are dropped.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub options: Vec<String>,
}

impl CatalogDef {
    pub fn supports(&self, extra: &str) -> bool {
        self.extra.iter().any(|e| e.name == extra) || self.extra_supported.iter().chain(&self.extra_required).any(|e| e == extra)
    }

    /// Supports search and needs no other required extra (e.g. a required genre we can't supply).
    pub fn searchable(&self) -> bool {
        self.supports("search")
            && self.extra.iter().all(|e| !e.is_required || e.name == "search")
            && self.extra_required.iter().all(|e| e == "search")
    }

    /// True for catalogs that can't be browsed without an extra arg (e.g. search-only catalogs).
    pub fn requires_extra(&self) -> bool {
        self.extra.iter().any(|e| e.is_required) || !self.extra_required.is_empty()
    }

    /// Options of the `genre` extra (empty for legacy `extraSupported` catalogs, which list none).
    pub fn genres(&self) -> &[String] {
        self.extra.iter().find(|e| e.name == "genre").map_or(&[], |e| &e.options)
    }

    pub fn requires_genre(&self) -> bool {
        self.extra.iter().any(|e| e.is_required && e.name == "genre") || self.extra_required.iter().any(|e| e == "genre")
    }

    /// Browsable on Discover: no required extra other than a genre we have options for.
    pub fn discoverable(&self) -> bool {
        self.extra.iter().all(|e| !e.is_required || e.name == "genre")
            && self.extra_required.iter().all(|e| e == "genre")
            && (!self.requires_genre() || !self.genres().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub resources: Vec<Resource>,
    pub types: Vec<String>,
    #[serde(default)]
    pub catalogs: Vec<CatalogDef>,
    #[serde(default, rename = "idPrefixes")]
    pub id_prefixes: Option<Vec<String>>,
    #[serde(default, rename = "behaviorHints", deserialize_with = "lenient_hints")]
    pub behavior_hints: ManifestHints,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ManifestHints {
    #[serde(default)]
    pub configurable: bool,
    #[serde(default, rename = "configurationRequired")]
    pub configuration_required: bool,
}

/// Malformed hints (wrong type, non-bool flags) degrade to false instead of failing the manifest.
fn lenient_hints<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<ManifestHints, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    let flag = |k: &str| v.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
    Ok(ManifestHints { configurable: flag("configurable"), configuration_required: flag("configurationRequired") })
}

impl Manifest {
    pub fn has_resource(&self, name: &str) -> bool {
        self.resources.iter().any(|r| r.name() == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetaPreview {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub poster: Option<String>,
    /// Wide backdrop image, used for the hero banner.
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default, deserialize_with = "lenient_text")]
    pub poster: Option<String>,
    /// Wide backdrop image shown behind the detail header.
    #[serde(default, deserialize_with = "lenient_text")]
    pub background: Option<String>,
    /// Title artwork shown in place of the name.
    #[serde(default, deserialize_with = "lenient_text")]
    pub logo: Option<String>,
    #[serde(default, deserialize_with = "lenient_text")]
    pub description: Option<String>,
    #[serde(default, rename = "releaseInfo", deserialize_with = "lenient_text")]
    pub release_info: Option<String>,
    /// e.g. "118 min".
    #[serde(default, deserialize_with = "lenient_text")]
    pub runtime: Option<String>,
    #[serde(default, rename = "imdbRating", deserialize_with = "lenient_text")]
    pub imdb_rating: Option<String>,
    /// Also filled from the legacy `genre` field.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub genres: Vec<String>,
    #[serde(default, deserialize_with = "lenient_vec", skip_serializing)]
    genre: Vec<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub cast: Vec<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub director: Vec<String>,
    /// Only links with an http(s) URL are kept (Stremio's own `stremio://` links are dropped).
    #[serde(default, deserialize_with = "lenient_vec")]
    pub links: Vec<Link>,
    /// Episodes of a series; malformed entries are dropped.
    #[serde(default, deserialize_with = "lenient_vec")]
    pub videos: Vec<Video>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub name: String,
    #[serde(default)]
    pub category: String,
    pub url: String,
}

/// One video of a meta, e.g. a series episode. Its `id` (often `tt…:S:E`) is what streams are requested for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Video {
    pub id: String,
    #[serde(default, alias = "name")]
    pub title: Option<String>,
    #[serde(default)]
    pub season: Option<u32>,
    #[serde(default)]
    pub episode: Option<u32>,
    /// ISO 8601 release date.
    #[serde(default)]
    pub released: Option<String>,
    /// Episode still image URL.
    #[serde(default)]
    pub thumbnail: Option<String>,
}

fn lenient_vec<'de, D: serde::Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    let items = v.as_array().map(|a| a.as_slice()).unwrap_or_default();
    Ok(items.iter().take(MAX_VIDEOS).filter_map(|v| T::deserialize(v).ok()).collect())
}

/// A string (or number, e.g. `"imdbRating": 7.8`); anything else becomes `None` instead of failing the meta.
fn lenient_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Stream {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "infoHash")]
    pub info_hash: Option<String>,
    #[serde(default, rename = "fileIdx")]
    pub file_idx: Option<usize>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// e.g. "tracker:<url>", "dht:<hash>"; "peer:<host:port>" is a local extension.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Page to open in the browser instead of playing; only http(s) is kept.
    #[serde(default, rename = "externalUrl")]
    pub external_url: Option<String>,
    /// YouTube video id, played through mpv's yt-dlp hook.
    #[serde(default, rename = "ytId")]
    pub yt_id: Option<String>,
    #[serde(default, rename = "behaviorHints", deserialize_with = "behavior_hints", skip_serializing)]
    pub hints: BehaviorHints,
    /// Name of the addon that served the stream (set by the client, not the addon).
    #[serde(skip)]
    pub addon: String,
}

/// The parts of a stream's `behaviorHints` this client uses.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BehaviorHints {
    /// `bingeGroup`: streams sharing one come from the same source/quality, so the next episode can autoplay it.
    pub binge_group: Option<String>,
    /// `proxyHeaders.request`: headers the stream URL needs (e.g. a debrid User-Agent), each passing [`valid_header`].
    pub request_headers: Vec<(String, String)>,
    pub filename: Option<String>,
    /// `videoSize` in bytes.
    pub video_size: Option<u64>,
}

const MAX_HEADERS: usize = 32;

/// Addon request header check: the name is an RFC 7230 token, the value printable ASCII (so no CR/LF/NUL
/// header injection), both length-capped, and never a header that frames or routes the request.
pub fn valid_header(name: &str, value: &str) -> bool {
    const RESERVED: [&str; 10] = ["host", "content-length", "transfer-encoding", "connection", "te", "upgrade", "trailer", "keep-alive", "proxy-connection", "range"];
    let tchar = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    (1..=128).contains(&name.len())
        && name.bytes().all(tchar)
        && !RESERVED.iter().any(|r| name.eq_ignore_ascii_case(r))
        && value.len() <= MAX_LONG_TEXT
        && value.bytes().all(|b| b == b'\t' || (b' '..=b'~').contains(&b))
}

/// Typed fields of `behaviorHints`; anything malformed is ignored, never fatal.
fn behavior_hints<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BehaviorHints, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    let headers = v.pointer("/proxyHeaders/request").and_then(|h| h.as_object()).into_iter().flatten();
    Ok(BehaviorHints {
        binge_group: text("bingeGroup"),
        request_headers: headers
            .filter_map(|(k, x)| Some((k.clone(), x.as_str()?.trim().to_string())))
            .filter(|(k, x)| valid_header(k, x))
            .take(MAX_HEADERS)
            .collect(),
        filename: text("filename"),
        video_size: v.get("videoSize").and_then(|x| x.as_u64()),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subtitle {
    #[serde(default)]
    pub id: Option<String>,
    pub url: String,
    pub lang: String,
}

/// What the player should open for a stream.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayTarget {
    Http(Url),
    Magnet(String, Option<usize>),
}

impl Stream {
    /// Playable source string (http URL or magnet with BEP 53 file index).
    pub fn source(&self) -> Option<String> {
        Some(match self.target()? {
            PlayTarget::Http(u) => u.to_string(),
            PlayTarget::Magnet(m, Some(idx)) => format!("{m}&so={idx}"),
            PlayTarget::Magnet(m, None) => m,
        })
    }

    pub fn target(&self) -> Option<PlayTarget> {
        // An unplayable `url` falls back to `infoHash`, then `ytId`.
        if let Some(u) = &self.url {
            let t = if u.starts_with("magnet:") { parse_magnet_hash(u).map(|_| PlayTarget::Magnet(u.clone(), self.file_idx)) } else { safe_http_url(u).map(PlayTarget::Http) };
            if t.is_some() {
                return t;
            }
        }
        let Some(hash) = self.info_hash.as_deref().filter(|h| is_info_hash(h)) else {
            return self.yt_id.as_deref().and_then(youtube_url).map(PlayTarget::Http);
        };
        let mut magnet = format!("magnet:?xt=urn:btih:{}", hash.to_ascii_lowercase());
        for s in &self.sources {
            if let Some(tr) = s.strip_prefix("tracker:").and_then(safe_http_url) {
                magnet.push_str("&tr=");
                magnet.push_str(&url::form_urlencoded::byte_serialize(tr.as_str().as_bytes()).collect::<String>());
            } else if let Some(p) = s.strip_prefix("peer:").and_then(|p| p.parse::<std::net::SocketAddr>().ok()) {
                magnet.push_str(&format!("&x.pe={p}"));
            }
        }
        Some(PlayTarget::Magnet(magnet, self.file_idx))
    }
}

// ---------- sanitization ----------

/// Strip control characters (keeps newlines/tabs), trim, and cap length.
pub fn sanitize_text(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .filter(|c| !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Only plain http/https URLs with a host are allowed (no file:, javascript:, data:, ...).
pub fn safe_http_url(s: &str) -> Option<Url> {
    let u = Url::parse(s.trim()).ok()?;
    (matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())).then_some(u)
}

/// Watch URL of a YouTube video id (11 chars of `[A-Za-z0-9_-]`); `None` for anything else.
pub fn youtube_url(id: &str) -> Option<Url> {
    (id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        .then(|| Url::parse(&format!("https://www.youtube.com/watch?v={id}")).unwrap())
}

/// True only for the exact URL [`youtube_url`] builds, so yt-dlp never runs on arbitrary addon URLs.
pub fn is_youtube(source: &str) -> bool {
    source.strip_prefix("https://www.youtube.com/watch?v=").and_then(youtube_url).is_some_and(|u| u.as_str() == source)
}

pub fn is_info_hash(h: &str) -> bool {
    h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Info hash of a magnet: 40 hex digits (lowercased) or the 32-character base32 form (uppercase, as librqbit reads it).
pub fn parse_magnet_hash(m: &str) -> Option<String> {
    let u = Url::parse(m).ok()?;
    if u.scheme() != "magnet" {
        return None;
    }
    let (_, v) = u.query_pairs().find(|(k, v)| k == "xt" && v.starts_with("urn:btih:"))?;
    let h = &v["urn:btih:".len()..];
    let base32 = h.len() == 32 && h.bytes().all(|b| matches!(b, b'A'..=b'Z' | b'2'..=b'7'));
    if is_info_hash(h) { Some(h.to_ascii_lowercase()) } else { base32.then(|| h.to_string()) }
}

/// Peer hints (`x.pe=host:port`) embedded in a magnet link.
pub fn magnet_peers(m: &str) -> Vec<std::net::SocketAddr> {
    Url::parse(m)
        .map(|u| u.query_pairs().filter(|(k, _)| k == "x.pe").filter_map(|(_, v)| v.parse().ok()).collect())
        .unwrap_or_default()
}

fn opt_text(s: &mut Option<String>, max: usize) {
    *s = s.as_deref().map(|v| sanitize_text(v, max)).filter(|v| !v.is_empty());
}

fn opt_url(s: &mut Option<String>) {
    *s = s.as_deref().and_then(safe_http_url).map(String::from);
}

pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_TEXT && !s.chars().any(|c| c.is_control() || c == '/')
}

fn from_json<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    ensure!(body.len() <= MAX_BODY, "response too large ({} bytes)", body.len());
    serde_json::from_slice(body).context("malformed addon response")
}

// ---------- parsers ----------

pub fn parse_manifest(body: &[u8]) -> Result<Manifest> {
    let mut m: Manifest = from_json(body)?;
    ensure!(valid_id(&m.id), "manifest: invalid id");
    m.name = sanitize_text(&m.name, MAX_TEXT);
    m.version = sanitize_text(&m.version, 64);
    ensure!(!m.name.is_empty(), "manifest: empty name");
    ensure!(!m.version.is_empty(), "manifest: empty version");
    ensure!(!m.resources.is_empty(), "manifest: no resources");
    ensure!(!m.types.is_empty(), "manifest: no types");
    opt_text(&mut m.description, MAX_LONG_TEXT);
    m.catalogs.retain(|c| valid_id(&c.id) && valid_id(&c.kind));
    m.catalogs.truncate(100);
    for c in &mut m.catalogs {
        opt_text(&mut c.name, MAX_TEXT);
        for e in &mut c.extra {
            e.options = e.options.iter().map(|o| sanitize_text(o, 64)).filter(|o| !o.is_empty()).take(MAX_OPTIONS).collect();
        }
    }
    Ok(m)
}

fn clean_preview(mut p: MetaPreview) -> Option<MetaPreview> {
    if !valid_id(&p.id) || !valid_id(&p.kind) {
        return None;
    }
    p.name = sanitize_text(&p.name, MAX_TEXT);
    opt_url(&mut p.poster);
    opt_url(&mut p.background);
    opt_text(&mut p.description, MAX_LONG_TEXT);
    (!p.name.is_empty()).then_some(p)
}

pub fn parse_catalog(body: &[u8]) -> Result<Vec<MetaPreview>> {
    #[derive(Deserialize)]
    struct R {
        metas: Vec<serde_json::Value>,
    }
    let r: R = from_json(body)?;
    // Drop individual malformed entries instead of failing the whole catalog.
    Ok(r.metas
        .into_iter()
        .take(MAX_ITEMS)
        .filter_map(|v| serde_json::from_value(v).ok())
        .filter_map(clean_preview)
        .collect())
}

pub fn parse_meta(body: &[u8]) -> Result<Meta> {
    #[derive(Deserialize)]
    struct R {
        meta: Meta,
    }
    let mut m = from_json::<R>(body)?.meta;
    ensure!(valid_id(&m.id) && valid_id(&m.kind), "meta: invalid id/type");
    m.name = sanitize_text(&m.name, MAX_TEXT);
    ensure!(!m.name.is_empty(), "meta: empty name");
    opt_url(&mut m.poster);
    opt_url(&mut m.background);
    opt_url(&mut m.logo);
    opt_text(&mut m.description, MAX_LONG_TEXT);
    opt_text(&mut m.release_info, 64);
    opt_text(&mut m.runtime, 64);
    opt_text(&mut m.imdb_rating, 16);
    if m.genres.is_empty() {
        m.genres = std::mem::take(&mut m.genre);
    }
    m.genre.clear();
    for list in [&mut m.genres, &mut m.cast, &mut m.director] {
        *list = list.iter().map(|s| sanitize_text(s, MAX_TEXT)).filter(|s| !s.is_empty()).take(MAX_TAGS).collect();
    }
    m.links = std::mem::take(&mut m.links)
        .into_iter()
        .filter_map(|mut l| {
            l.url = safe_http_url(&l.url)?.into();
            l.name = sanitize_text(&l.name, MAX_TEXT);
            l.category = sanitize_text(&l.category, 64);
            (!l.name.is_empty()).then_some(l)
        })
        .take(MAX_TAGS)
        .collect();
    m.videos.retain(|v| valid_id(&v.id));
    for v in &mut m.videos {
        opt_text(&mut v.title, MAX_TEXT);
        opt_text(&mut v.released, 64);
        opt_url(&mut v.thumbnail);
    }
    Ok(m)
}

pub fn parse_streams(body: &[u8]) -> Result<Vec<Stream>> {
    #[derive(Deserialize)]
    struct R {
        streams: Vec<serde_json::Value>,
    }
    let r: R = from_json(body)?;
    Ok(r.streams
        .into_iter()
        .take(MAX_ITEMS)
        .filter_map(|v| serde_json::from_value::<Stream>(v).ok())
        .filter_map(|mut s| {
            opt_text(&mut s.name, MAX_TEXT);
            opt_text(&mut s.title, MAX_LONG_TEXT);
            opt_text(&mut s.description, MAX_LONG_TEXT);
            opt_text(&mut s.hints.binge_group, MAX_TEXT);
            // Shown on one line of the stream row.
            s.hints.filename = s.hints.filename.map(|f| f.replace(['\n', '\t'], " "));
            opt_text(&mut s.hints.filename, MAX_TEXT);
            opt_url(&mut s.external_url);
            (s.target().is_some() || s.external_url.is_some()).then_some(s)
        })
        .collect())
}

pub fn parse_subtitles(body: &[u8]) -> Result<Vec<Subtitle>> {
    #[derive(Deserialize)]
    struct R {
        subtitles: Vec<serde_json::Value>,
    }
    let r: R = from_json(body)?;
    Ok(r.subtitles
        .into_iter()
        .take(MAX_ITEMS)
        .filter_map(|v| serde_json::from_value::<Subtitle>(v).ok())
        .filter_map(|mut s| {
            s.url = safe_http_url(&s.url)?.into();
            s.lang = sanitize_text(&s.lang, 16);
            opt_text(&mut s.id, MAX_TEXT);
            Some(s)
        })
        .collect())
}

/// Largest poster side accepted; GPU texture limits can be as low as 2048 and egui_glow asserts on them.
pub const MAX_POSTER_SIDE: u32 = 2048;

/// Reject posters that aren't a decodable image or whose dimensions (not byte size) are too big,
/// since a few-KB image can declare huge dimensions.
pub fn check_poster(body: &[u8]) -> Result<()> {
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(body)).with_guessed_format()?.into_dimensions()?;
    ensure!(w > 0 && h > 0 && w <= MAX_POSTER_SIDE && h <= MAX_POSTER_SIDE, "poster too large: {w}x{h}");
    Ok(())
}

#[cfg(test)]
mod configure_tests {
    use super::*;

    #[test]
    fn configure_url_derivation() {
        assert_eq!(configure_url("https://a.example/x/manifest.json").unwrap().as_str(), "https://a.example/x/configure");
        assert_eq!(configure_url("http://127.0.0.1:7070/manifest.json").unwrap().as_str(), "http://127.0.0.1:7070/configure");
        assert!(configure_url("stremio://a.example/manifest.json").is_err());
        assert!(configure_url("https://a.example/other.json").is_err());
    }

    #[test]
    fn behavior_hints_tolerant() {
        let m = |h: &str| {
            let body = format!(r#"{{"id":"a","version":"1","name":"n","resources":["catalog"],"types":["movie"]{h}}}"#);
            parse_manifest(body.as_bytes()).unwrap().behavior_hints
        };
        assert_eq!(m(""), ManifestHints::default());
        let h = m(r#","behaviorHints":{"configurable":true,"configurationRequired":true}"#);
        assert!(h.configurable && h.configuration_required);
        assert_eq!(m(r#","behaviorHints":"junk""#), ManifestHints::default());
        assert_eq!(m(r#","behaviorHints":{"configurable":"yes"}"#), ManifestHints::default());
    }
}

// ---------- client ----------

/// Base URL of an addon given its transport (manifest) URL.
pub fn addon_base(transport_url: &str) -> Result<Url> {
    let u = safe_http_url(transport_url).context("addon URL must be http(s)")?;
    let s = u.as_str();
    let Some(base) = s.strip_suffix("/manifest.json") else { bail!("addon URL must end with /manifest.json") };
    Ok(Url::parse(&format!("{base}/"))?)
}

/// `<base>/configure` page of a configurable addon (http(s) only).
pub fn configure_url(transport_url: &str) -> Result<Url> {
    Ok(addon_base(transport_url)?.join("configure")?)
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
}

/// Relative catalog URL; `skip > 0` adds the Stremio `skip` extra.
pub fn catalog_path(kind: &str, id: &str, skip: usize) -> String {
    match skip {
        0 => format!("catalog/{}/{}.json", enc(kind), enc(id)),
        n => format!("catalog/{}/{}/skip={n}.json", enc(kind), enc(id)),
    }
}

/// Relative catalog URL, with an optional `genre` extra (`catalog/<type>/<id>/genre=<value>.json`).
pub fn discover_path(kind: &str, id: &str, genre: Option<&str>) -> String {
    match genre {
        None => format!("catalog/{}/{}.json", enc(kind), enc(id)),
        Some(g) => format!("catalog/{}/{}/genre={}.json", enc(kind), enc(id), enc(g)),
    }
}

#[derive(Clone)]
pub struct AddonClient {
    http: reqwest::Client,
}

impl Default for AddonClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AddonClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .expect("reqwest client");
        Self { http }
    }

    async fn get(&self, url: Url) -> Result<Vec<u8>> {
        let mut resp = self.http.get(url.clone()).send().await.with_context(|| format!("GET {url}"))?;
        ensure!(resp.status().is_success(), "GET {url}: HTTP {}", resp.status());
        ensure!(resp.content_length().unwrap_or(0) <= MAX_BODY as u64, "response too large");
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            body.extend_from_slice(&chunk);
            ensure!(body.len() <= MAX_BODY, "response too large");
        }
        Ok(body)
    }

    pub async fn fetch_manifest(&self, transport_url: &str) -> Result<Manifest> {
        addon_base(transport_url)?;
        parse_manifest(&self.get(safe_http_url(transport_url).unwrap()).await?)
    }

    fn resource_url(transport_url: &str, resource: &str, kind: &str, id: &str) -> Result<Url> {
        Ok(addon_base(transport_url)?.join(&format!("{resource}/{}/{}.json", enc(kind), enc(id)))?)
    }

    /// One catalog page; `skip` is the number of items already fetched (0 = first page).
    pub async fn catalog(&self, transport_url: &str, kind: &str, id: &str, skip: usize) -> Result<Vec<MetaPreview>> {
        parse_catalog(&self.get(addon_base(transport_url)?.join(&catalog_path(kind, id, skip))?).await?)
    }

    /// First page of a catalog, filtered by `genre` if given.
    pub async fn discover(&self, transport_url: &str, kind: &str, id: &str, genre: Option<&str>) -> Result<Vec<MetaPreview>> {
        parse_catalog(&self.get(addon_base(transport_url)?.join(&discover_path(kind, id, genre))?).await?)
    }

    pub async fn search(&self, transport_url: &str, kind: &str, id: &str, query: &str) -> Result<Vec<MetaPreview>> {
        let path = format!("catalog/{}/{}/search={}.json", enc(kind), enc(id), enc(query));
        parse_catalog(&self.get(addon_base(transport_url)?.join(&path)?).await?)
    }

    /// Raw image bytes for a poster URL, capped at `MAX_BODY`.
    pub async fn poster(&self, url: &str) -> Result<Vec<u8>> {
        let body = self.get(safe_http_url(url).context("poster URL must be http(s)")?).await?;
        check_poster(&body)?;
        Ok(body)
    }

    pub async fn meta(&self, transport_url: &str, kind: &str, id: &str) -> Result<Meta> {
        parse_meta(&self.get(Self::resource_url(transport_url, "meta", kind, id)?).await?)
    }

    pub async fn streams(&self, transport_url: &str, kind: &str, id: &str) -> Result<Vec<Stream>> {
        parse_streams(&self.get(Self::resource_url(transport_url, "stream", kind, id)?).await?)
    }

    pub async fn subtitles(&self, transport_url: &str, kind: &str, id: &str) -> Result<Vec<Subtitle>> {
        parse_subtitles(&self.get(Self::resource_url(transport_url, "subtitles", kind, id)?).await?)
    }
}
