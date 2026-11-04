//! Stremio addon protocol: types, validation/sanitization, and an HTTP client.
//! All addon output is untrusted: every response goes through `parse_*`, which
//! never panics, rejects structurally invalid data, and drops invalid entries.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;

/// Upper bound on any addon response body.
pub const MAX_BODY: usize = 4 * 1024 * 1024;
/// Stremio account API. A whole library can be far larger than an addon response.
const STREMIO_API: &str = "https://api.strem.io/api/";
const MAX_API_BODY: usize = 64 * 1024 * 1024;
const MAX_TEXT: usize = 300;
const MAX_LONG_TEXT: usize = 4000;
const MAX_ITEMS: usize = 500;
/// Long-running series have well over `MAX_ITEMS` episodes.
const MAX_VIDEOS: usize = 5000;
/// Cap on genres, cast, directors, links and trailers of a meta.
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

    /// Extras the user can pick a value for (`genre`, `year`, `sort`, ...): those declaring options,
    /// minus `skip`/`search` (handled elsewhere).
    pub fn pickers(&self) -> impl Iterator<Item = &ExtraDef> {
        self.extra.iter().filter(|e| !e.options.is_empty() && !matches!(e.name.as_str(), "skip" | "search"))
    }

    /// Required by the `extra` list or the legacy `extraRequired` field.
    pub fn is_required(&self, name: &str) -> bool {
        self.extra.iter().any(|e| e.name == name && e.is_required) || self.extra_required.iter().any(|n| n == name)
    }

    /// `current` kept where still a valid option, plus the first option of each required picker.
    pub fn valid_extras(&self, current: &[(String, String)]) -> Vec<(String, String)> {
        self.pickers()
            .filter_map(|e| {
                let kept = current.iter().find(|(n, v)| *n == e.name && e.options.contains(v));
                kept.cloned().or_else(|| self.is_required(&e.name).then(|| (e.name.clone(), e.options[0].clone())))
            })
            .collect()
    }

    /// Browsable on Discover: every required extra (either source) has a picker to fill it.
    pub fn discoverable(&self) -> bool {
        let required = self.extra.iter().filter(|e| e.is_required).map(|e| &e.name).chain(&self.extra_required);
        required.into_iter().all(|n| self.pickers().any(|p| p.name == *n))
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
    /// Catalogs of other addons, served by the `addon_catalog` resource; malformed entries are dropped.
    #[serde(default, rename = "addonCatalogs", deserialize_with = "lenient_vec")]
    pub addon_catalogs: Vec<CatalogDef>,
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

/// Whether `new` is strictly newer than `old`. Compares the numeric dot parts of the core;
/// a `-` or `+` suffix is ignored, and non-numeric versions are never offered.
pub fn is_newer(new: &str, old: &str) -> bool {
    let core = |v: &str| {
        let v = v.trim().trim_start_matches('v');
        v.split(['-', '+']).next()?.split('.').map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<_>>>()
    };
    match (core(new), core(old)) {
        (Some(mut a), Some(mut b)) => {
            let n = a.len().max(b.len());
            a.resize(n, 0);
            b.resize(n, 0);
            a > b
        }
        _ => false,
    }
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
    /// YouTube trailers (`trailerStreams`, else legacy `trailers`); each has a valid `ytId` and nothing else to play.
    #[serde(default, rename = "trailerStreams", deserialize_with = "lenient_vec")]
    pub trailers: Vec<Stream>,
    #[serde(default, rename = "trailers", deserialize_with = "lenient_vec", skip_serializing)]
    legacy_trailers: Vec<LegacyTrailer>,
}

/// Legacy `trailers` entry: `{"source": "<ytId>", "type": "Trailer"}`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
struct LegacyTrailer {
    source: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
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
    /// `videoHash`: OpenSubtitles hash of the file (16 hex digits).
    pub video_hash: Option<String>,
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
        video_hash: text("videoHash").filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit())).map(|h| h.to_ascii_lowercase()),
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

/// A `stremio://` link from the OS (browser, other apps).
#[derive(Debug, PartialEq)]
pub enum DeepLink {
    /// `stremio://<host>/…/manifest.json`: the addon's https manifest URL (http for loopback hosts).
    Addon(Url),
    /// `stremio:///detail/<type>/<id>[/<video id>]`, segments percent-decoded.
    Detail { kind: String, id: String, video: Option<String> },
}

/// Parse a `stremio://` link; `None` for anything else or invalid parts.
pub fn parse_deep_link(s: &str) -> Option<DeepLink> {
    let s = s.trim();
    // Schemes are case-insensitive.
    let rest = s.get(..10).filter(|p| p.eq_ignore_ascii_case("stremio://")).map(|_| &s[10..])?;
    if let Some(path) = rest.strip_prefix('/') {
        let seg: Vec<String> = path.split(['?', '#']).next()?.split('/').map(|p| percent_encoding::percent_decode_str(p).decode_utf8_lossy().into_owned()).collect();
        return match seg.as_slice() {
            [d, kind, id, video @ ..] if d == "detail" && video.len() <= 1 && [kind, id].into_iter().chain(video).all(|p| valid_id(p) && sanitize_text(p, MAX_TEXT) == *p) => {
                Some(DeepLink::Detail { kind: kind.clone(), id: id.clone(), video: video.first().cloned() })
            }
            _ => None,
        };
    }
    let u = safe_http_url(&format!("https://{rest}"))?;
    if !u.path().ends_with("/manifest.json") || u.query().is_some() || u.fragment().is_some() {
        return None;
    }
    // Local addons (the SDK's own landing page links) serve plain http.
    let local = match u.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    safe_http_url(&format!("{}://{rest}", if local { "http" } else { "https" })).map(DeepLink::Addon)
}

fn from_json<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    ensure!(body.len() <= MAX_BODY, "response too large ({} bytes)", body.len());
    serde_json::from_slice(body).context("malformed addon response")
}

// ---------- parsers ----------

/// `result` of a Stremio API response body, or an error with its (sanitized) message.
pub fn api_result(body: &[u8]) -> Result<serde_json::Value> {
    let mut v: serde_json::Value = serde_json::from_slice(body).context("Stremio API: invalid response")?;
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        bail!("Stremio API: {}", sanitize_text(e["message"].as_str().unwrap_or("request failed"), 200));
    }
    v.get_mut("result").map(serde_json::Value::take).filter(|r| !r.is_null()).context("Stremio API: no result")
}

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
    m.addon_catalogs.retain(|c| valid_id(&c.id) && valid_id(&c.kind));
    m.addon_catalogs.truncate(100);
    for c in &mut m.addon_catalogs {
        opt_text(&mut c.name, MAX_TEXT);
    }
    for c in &mut m.catalogs {
        opt_text(&mut c.name, MAX_TEXT);
        for e in &mut c.extra {
            e.options = e.options.iter().map(|o| sanitize_text(o, 64)).filter(|o| !o.is_empty()).take(MAX_OPTIONS).collect();
        }
    }
    Ok(m)
}

/// One installable addon listed by an `addon_catalog` resource.
#[derive(Debug, Clone, PartialEq)]
pub struct AddonEntry {
    pub transport_url: String,
    pub manifest: Manifest,
}

/// `{"addons": [{transportUrl, manifest}]}`; entries with a non-http(s) URL or an invalid manifest are dropped.
pub fn parse_addon_catalog(body: &[u8]) -> Result<Vec<AddonEntry>> {
    #[derive(Deserialize)]
    struct R {
        addons: Vec<serde_json::Value>,
    }
    let r: R = from_json(body)?;
    Ok(r.addons
        .into_iter()
        .take(MAX_ITEMS)
        .filter_map(|v| {
            let url = v.get("transportUrl")?.as_str()?;
            addon_base(url).ok()?;
            let manifest = parse_manifest(&serde_json::to_vec(v.get("manifest")?).ok()?).ok()?;
            // Normalized form: the URL parser drops tabs and newlines, so they never reach the UI or the DB.
            Some(AddonEntry { transport_url: safe_http_url(url)?.into(), manifest })
        })
        .collect())
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
    // Only the YouTube id is kept, so a trailer can never play an arbitrary URL or torrent.
    let keep = |list: Vec<Stream>| -> Vec<Stream> {
        list.into_iter()
            .filter(|t| t.yt_id.as_deref().and_then(youtube_url).is_some())
            .map(|t| Stream { title: t.title.map(|x| sanitize_text(&x, MAX_TEXT)).filter(|x| !x.is_empty()), yt_id: t.yt_id, ..Default::default() })
            .take(MAX_TAGS)
            .collect()
    };
    m.trailers = keep(std::mem::take(&mut m.trailers));
    if m.trailers.is_empty() {
        m.trailers = keep(m.legacy_trailers.drain(..).map(|t| Stream { yt_id: Some(t.source), title: t.kind, ..Default::default() }).collect());
    }
    m.legacy_trailers.clear();
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
    fn pickers_exclude_skip_and_search() {
        let body = r#"{"id":"a","version":"1","name":"n","resources":["catalog"],"types":["movie"],"catalogs":[{"type":"movie","id":"c","extra":[
            {"name":"genre","options":["A"]},{"name":"skip","options":["0","100"]},{"name":"search","options":["x"]}]}]}"#;
        let m = parse_manifest(body.as_bytes()).unwrap();
        let names: Vec<_> = m.catalogs[0].pickers().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["genre"]);
    }

    #[test]
    fn required_extras_need_a_picker() {
        let cat = |c: &str| parse_manifest(format!(r#"{{"id":"a","version":"1","name":"n","resources":["catalog"],"types":["movie"],"catalogs":[{c}]}}"#).as_bytes()).unwrap().catalogs.remove(0);
        // Legacy `extraRequired` plus options in `extra`: browsable, defaulting to the first option.
        let c = cat(r#"{"type":"movie","id":"t","extra":[{"name":"genre","options":["Action","Drama"]}],"extraRequired":["genre"]}"#);
        assert!(c.discoverable());
        assert_eq!(c.valid_extras(&[]), [("genre".to_string(), "Action".to_string())]);
        // Required names without a picker (no options, or `skip`/`search`) stay hidden.
        assert!(!cat(r#"{"type":"movie","id":"t","extraRequired":["genre"]}"#).discoverable());
        assert!(!cat(r#"{"type":"movie","id":"t","extra":[{"name":"skip","isRequired":true,"options":["0"]}]}"#).discoverable());
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

/// Bytes the OpenSubtitles hash reads from each end of the file.
pub const HASH_CHUNK: usize = 64 * 1024;

/// OpenSubtitles hash: file size plus the little-endian u64 words of its first and last 64 KiB, wrapping.
/// For files under 128 KiB the two chunks overlap, as in the reference algorithm; files under 64 KiB get no hash.
pub fn opensubtitles_hash(size: u64, head: &[u8], tail: &[u8]) -> String {
    let sum = |b: &[u8]| b.as_chunks::<8>().0.iter().fold(0u64, |a, w| a.wrapping_add(u64::from_le_bytes(*w)));
    format!("{:016x}", size.wrapping_add(sum(head)).wrapping_add(sum(tail)))
}

/// Subtitles path with Stremio extras (`subtitles/<type>/<id>/videoHash=..&videoSize=...json`).
pub fn subtitles_path(kind: &str, id: &str, extra: &[(&str, String)]) -> String {
    let extra: Vec<String> = extra.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect();
    match extra.is_empty() {
        true => format!("subtitles/{}/{}.json", enc(kind), enc(id)),
        false => format!("subtitles/{}/{}/{}.json", enc(kind), enc(id), extra.join("&")),
    }
}

/// Relative catalog URL; `skip > 0` adds the Stremio `skip` extra.
pub fn catalog_path(kind: &str, id: &str, skip: usize) -> String {
    match skip {
        0 => format!("catalog/{}/{}.json", enc(kind), enc(id)),
        n => format!("catalog/{}/{}/skip={n}.json", enc(kind), enc(id)),
    }
}

/// Relative catalog URL with extras (`catalog/<type>/<id>/<name>=<value>&....json`).
pub fn discover_path(kind: &str, id: &str, extras: &[(String, String)]) -> String {
    if extras.is_empty() {
        return format!("catalog/{}/{}.json", enc(kind), enc(id));
    }
    let extra: Vec<String> = extras.iter().map(|(n, v)| format!("{}={}", enc(n), enc(v))).collect();
    format!("catalog/{}/{}/{}.json", enc(kind), enc(id), extra.join("&"))
}

/// Release this binary was built as (`nightly` or `vX.Y.Z`) and its commit; set by release.yml, absent in source builds.
pub const RELEASE: Option<&str> = option_env!("STREMFLIX_RS_RELEASE");
pub const COMMIT: Option<&str> = option_env!("STREMFLIX_RS_COMMIT");
/// `owner/repo` the build was released from (forks check their own releases).
const REPO: &str = match option_env!("STREMFLIX_RS_REPO") {
    Some(r) => r,
    None => "theoneand33/stremflix-rs",
};

/// Page of a newer release on `release`'s channel, given that channel's GitHub release JSON
/// (`nightly`: any other commit; tagged: a newer latest tag).
pub fn newer_release(release: &str, commit: &str, body: &[u8]) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Rel {
        tag_name: String,
        target_commitish: String,
        html_url: String,
    }
    let r: Rel = serde_json::from_slice(body)?;
    // shortcut: pre-release suffixes are ignored when ordering tags (release.yml keeps `-` tags out of /latest), so a beta user is offered neither its own final nor later betas; upgrade if that matters.
    let core = |t: &str| t.trim_start_matches('v').split('-').next().unwrap_or("").split('.').map(|n| n.parse::<u64>().ok()).collect::<Option<Vec<_>>>();
    let newer = if release == "nightly" {
        // target_commitish can also be a branch name; only a full SHA is comparable
        is_info_hash(&r.target_commitish) && r.target_commitish != commit
    } else {
        match (core(&r.tag_name), core(release)) {
            (Some(latest), Some(cur)) => latest > cur,
            _ => r.tag_name != release,
        }
    };
    Ok(newer.then(|| safe_http_url(&r.html_url).map(String::from)).flatten())
}

#[derive(Clone)]
pub struct AddonClient {
    /// Built on first request, off the UI thread: building it loads the system TLS roots (~4 ms at startup).
    http: std::sync::Arc<std::sync::OnceLock<Result<reqwest::Client, String>>>,
}

impl Default for AddonClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AddonClient {
    pub fn new() -> Self {
        Self { http: Default::default() }
    }

    fn http(&self) -> Result<&reqwest::Client> {
        let http = self.http.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::limited(3))
                .user_agent("stremflix-rs") // GitHub's API rejects requests without one
                .build()
                .map_err(|e| format!("HTTP client: {e}"))
        });
        http.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn get(&self, url: Url) -> Result<Vec<u8>> {
        let mut resp = self.http()?.get(url.clone()).send().await.with_context(|| format!("GET {url}"))?;
        ensure!(resp.status().is_success(), "HTTP {}", resp.status());
        ensure!(resp.content_length().unwrap_or(0) <= MAX_BODY as u64, "response too large");
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            body.extend_from_slice(&chunk);
            ensure!(body.len() <= MAX_BODY, "response too large");
        }
        Ok(body)
    }

    /// Release page of a newer build than this one, or `None` (also for source builds).
    pub async fn check_update(&self) -> Result<Option<String>> {
        let (Some(release), Some(commit)) = (RELEASE, COMMIT) else { return Ok(None) };
        let url = Url::parse(&format!("https://api.github.com/repos/{REPO}/releases/"))?.join(if release == "nightly" { "tags/nightly" } else { "latest" })?;
        newer_release(release, commit, &self.get(url).await?)
    }

    /// POST `body` to Stremio API `method`; its `result`, or its `error` message.
    async fn stremio_api(&self, method: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{STREMIO_API}{method}");
        // A whole library takes longer than the client's 15 s default.
        let req = self.http()?.post(&url).json(&body).timeout(std::time::Duration::from_secs(120));
        let mut resp = req.send().await.with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        // Errors come back as JSON `{"error": ...}`, so the body is read whatever the status.
        let mut out = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            out.extend_from_slice(&chunk);
            ensure!(out.len() <= MAX_API_BODY, "Stremio API: response too large");
        }
        api_result(&out).with_context(|| format!("HTTP {status}"))
    }

    /// Sign in to a Stremio account; returns its auth key.
    pub async fn stremio_login(&self, email: &str, password: &str) -> Result<String> {
        let r = self.stremio_api("login", serde_json::json!({"type": "Login", "email": email, "password": password, "facebook": false})).await?;
        r["authKey"].as_str().filter(|k| !k.is_empty()).map(String::from).context("Stremio API: no auth key")
    }

    pub async fn stremio_logout(&self, auth_key: &str) -> Result<()> {
        self.stremio_api("logout", serde_json::json!({"type": "Logout", "authKey": auth_key})).await.map(drop)
    }

    /// The account's addons and library, validated like a local Stremio import.
    pub async fn stremio_pull(&self, auth_key: &str) -> Result<crate::stremio_import::Import> {
        let addons = self.stremio_api("addonCollectionGet", serde_json::json!({"type": "AddonCollectionGet", "authKey": auth_key, "update": true})).await?;
        let library = self.stremio_api("datastoreGet", serde_json::json!({"authKey": auth_key, "collection": "libraryItem", "ids": [], "all": true})).await?;
        crate::stremio_import::from_account(&addons, &library)
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

    /// First page of a catalog, with the given extras (genre, year, sort, ...).
    pub async fn discover(&self, transport_url: &str, kind: &str, id: &str, extras: &[(String, String)]) -> Result<Vec<MetaPreview>> {
        parse_catalog(&self.get(addon_base(transport_url)?.join(&discover_path(kind, id, extras))?).await?)
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

    pub async fn addon_catalog(&self, transport_url: &str, kind: &str, id: &str) -> Result<Vec<AddonEntry>> {
        parse_addon_catalog(&self.get(Self::resource_url(transport_url, "addon_catalog", kind, id)?).await?)
    }

    pub async fn meta(&self, transport_url: &str, kind: &str, id: &str) -> Result<Meta> {
        parse_meta(&self.get(Self::resource_url(transport_url, "meta", kind, id)?).await?)
    }

    pub async fn streams(&self, transport_url: &str, kind: &str, id: &str) -> Result<Vec<Stream>> {
        parse_streams(&self.get(Self::resource_url(transport_url, "stream", kind, id)?).await?)
    }

    /// `extra`: Stremio subtitle extras such as `videoHash`, `videoSize` and `filename`.
    pub async fn subtitles(&self, transport_url: &str, kind: &str, id: &str, extra: &[(&str, String)]) -> Result<Vec<Subtitle>> {
        parse_subtitles(&self.get(addon_base(transport_url)?.join(&subtitles_path(kind, id, extra))?).await?)
    }

    /// Up to `HASH_CHUNK` bytes of `url` from `start`, and the file size from `Content-Range` (or a 200's length).
    async fn chunk_at(&self, url: &Url, headers: &[(String, String)], start: u64) -> Result<(Vec<u8>, u64)> {
        let mut req = self.http()?.get(url.clone()).header("Range", format!("bytes={start}-{}", start + HASH_CHUNK as u64 - 1));
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let mut resp = req.send().await?;
        ensure!(resp.status().is_success(), "HTTP {}", resp.status());
        // `bytes <first>-<last>/<total>`; a slice from elsewhere would hash wrong.
        let range = resp.headers().get("content-range").and_then(|v| v.to_str().ok()?.strip_prefix("bytes ").map(String::from));
        let range = range.and_then(|r| {
            let (span, total) = r.split_once('/')?;
            Some((span.split('-').next()?.parse::<u64>().ok()?, total.parse::<u64>().ok()?))
        });
        let size = match (resp.status().as_u16(), range) {
            (206, Some((first, t))) if first == start => t,
            // No range support: the head still reads, but the tail request then lands in `bail!` below.
            (200, _) if start == 0 => resp.content_length().context("no length")?,
            _ => bail!("no range support"),
        };
        let mut body = Vec::new();
        while body.len() < HASH_CHUNK
            && let Some(c) = resp.chunk().await?
        {
            body.extend_from_slice(&c[..c.len().min(HASH_CHUNK - body.len())]);
        }
        Ok((body, size))
    }

    /// OpenSubtitles hash and size of the video at `url`, read with two range requests.
    pub async fn video_hash(&self, url: &str, headers: &[(String, String)]) -> Result<(String, u64)> {
        let url = safe_http_url(url).context("not an http(s) URL")?;
        let (head, size) = self.chunk_at(&url, headers, 0).await?;
        ensure!(size >= HASH_CHUNK as u64 && head.len() == HASH_CHUNK, "file too small to hash");
        let (tail, _) = self.chunk_at(&url, headers, size - HASH_CHUNK as u64).await?;
        ensure!(tail.len() == HASH_CHUNK, "short read");
        Ok((opensubtitles_hash(size, &head, &tail), size))
    }
}
