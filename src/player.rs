//! Playback backend: libmpv embedded in-process. mpv renders through its OpenGL render
//! API into a framebuffer the GUI owns (GPU scaling, hardware decoding via `hwdec`),
//! so the video is a texture under the GUI's own controls.
//! `STREMFLIX_RS_MPV_ARGS` adds mpv options (e.g. `--ao=null` for headless CI).
//! mpv's JSON IPC server stays on at [`ipc_socket_path`] so tools can query player state.

use anyhow::{Result, bail};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct GlInitParams {
    get_proc_address: extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    ctx: *mut c_void,
}

#[repr(C)]
struct GlFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

/// Resolves GL function names in the GUI's current GL context.
pub type GlProc = std::sync::Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>;

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

// libmpv is loaded at runtime, not linked: its ~340 shared libraries took ~150 ms to load before `main`.
// Each entry becomes a field holding the symbol and a same-named wrapper, so call sites stay plain calls.
macro_rules! mpv_api {
    ($(fn $name:ident($($a:ident: $t:ty),*) $(-> $r:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct Mpv {
            $($name: unsafe extern "C" fn($($t),*) $(-> $r)?,)*
            _lib: libloading::Library,
        }
        impl Mpv {
            unsafe fn load(lib: libloading::Library) -> Result<Self, libloading::Error> {
                Ok(Self { $($name: unsafe { *lib.get(concat!(stringify!($name), "\0").as_bytes())? },)* _lib: lib })
            }
        }
        $(unsafe fn $name($($a: $t),*) $(-> $r)? { unsafe { (api().$name)($($a),*) } })*
    };
}

mpv_api! {
    fn mpv_create() -> *mut c_void;
    fn mpv_initialize(h: *mut c_void) -> c_int;
    fn mpv_set_option_string(h: *mut c_void, name: *const c_char, data: *const c_char) -> c_int;
    fn mpv_command_async(h: *mut c_void, reply: u64, args: *const *const c_char) -> c_int;
    fn mpv_observe_property(h: *mut c_void, reply: u64, name: *const c_char, format: c_int) -> c_int;
    fn mpv_set_wakeup_callback(h: *mut c_void, cb: Option<extern "C" fn(*mut c_void)>, data: *mut c_void);
    fn mpv_wait_event(h: *mut c_void, timeout: f64) -> *mut MpvEvent;
    fn mpv_error_string(err: c_int) -> *const c_char;
    fn mpv_terminate_destroy(h: *mut c_void);
    fn mpv_render_context_create(ctx: *mut *mut c_void, h: *mut c_void, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_set_update_callback(ctx: *mut c_void, cb: Option<extern "C" fn(*mut c_void)>, data: *mut c_void);
    fn mpv_render_context_update(ctx: *mut c_void) -> u64;
    fn mpv_render_context_render(ctx: *mut c_void, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_free(ctx: *mut c_void);
}

/// File names tried in order; the platform loader searches its usual paths (the exe's folder first on Windows).
const MPV_LIBS: &[&str] = if cfg!(windows) {
    &["libmpv-2.dll", "mpv-2.dll"]
} else if cfg!(target_os = "macos") {
    // Homebrew's `opt/mpv` path is what the linked binary used; the `lib` symlinks can be missing (e.g. cached Cellar).
    &["libmpv.2.dylib", "/opt/homebrew/opt/mpv/lib/libmpv.2.dylib", "/usr/local/opt/mpv/lib/libmpv.2.dylib", "/opt/homebrew/lib/libmpv.2.dylib", "/usr/local/lib/libmpv.2.dylib", "/opt/local/lib/libmpv.2.dylib"]
} else {
    &["libmpv.so.2", "libmpv.so"]
};

static MPV: std::sync::OnceLock<Result<Mpv, String>> = std::sync::OnceLock::new();

/// Loads libmpv once; later calls return the same result (a missing libmpv needs an app restart once installed). Call it early on a background thread so the first play doesn't wait.
pub fn load_mpv() -> Result<(), String> {
    MPV.get_or_init(|| {
        let mut errs = Vec::new();
        for name in MPV_LIBS {
            // SAFETY: libmpv's initializers are safe to run on any thread.
            match unsafe { libloading::Library::new(name) } {
                Ok(lib) => match unsafe { Mpv::load(lib) } {
                    Ok(api) => return Ok(api),
                    Err(e) => errs.push(format!("{name}: {e}")),
                },
                Err(e) => errs.push(e.to_string()),
            }
        }
        Err(format!("libmpv not found: {}", errs.join("; ")))
    })
    .as_ref()
    .map(|_| ())
    .map_err(Clone::clone)
}

/// Only reached through a `Player`, which `launch` creates after `load_mpv` succeeded.
fn api() -> &'static Mpv {
    MPV.get().and_then(|r| r.as_ref().ok()).expect("libmpv loaded before use")
}

const EVENT_SHUTDOWN: c_int = 1;
const EVENT_END_FILE: c_int = 7;
const EVENT_FILE_LOADED: c_int = 8;
const EVENT_PROPERTY_CHANGE: c_int = 22;
const END_FILE_REASON_ERROR: c_int = 4;
const FORMAT_STRING: c_int = 1;
const FORMAT_FLAG: c_int = 3;
const FORMAT_INT64: c_int = 4;
const FORMAT_DOUBLE: c_int = 5;

/// Observed properties, in `reply_userdata` order.
const PROPS: [(&str, c_int); 11] = [
    ("time-pos", FORMAT_DOUBLE),
    ("duration", FORMAT_DOUBLE),
    ("pause", FORMAT_FLAG),
    ("volume", FORMAT_DOUBLE),
    ("mute", FORMAT_FLAG),
    ("paused-for-cache", FORMAT_FLAG),
    ("dwidth", FORMAT_INT64),
    ("dheight", FORMAT_INT64),
    ("track-list", FORMAT_STRING),
    ("speed", FORMAT_DOUBLE),
    ("seeking", FORMAT_FLAG),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub id: i64,
    /// "audio", "video" or "sub".
    pub kind: String,
    pub label: String,
    pub lang: Option<String>,
    pub selected: bool,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub pos: f64,
    pub dur: f64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    pub buffering: bool,
    pub seeking: bool,
    /// Display size of the video in pixels; 0 until known.
    pub video: [usize; 2],
    pub tracks: Vec<Track>,
    /// Playback speed multiplier; 0 until mpv reports it.
    pub speed: f64,
    /// Set when the file failed to open/play.
    pub error: Option<String>,
}

/// Subtitle appearance chosen in the player menu; persisted as one setting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubStyle {
    pub scale: f64,
    /// Index into [`SUB_COLORS`].
    pub color: usize,
    pub background: bool,
    /// Seconds; positive shows subtitles later.
    pub delay: f64,
    /// Seconds; positive plays audio later (mpv `audio-delay`).
    pub audio_delay: f64,
}

/// Playback speeds offered in the player menu.
pub const SPEEDS: [f64; 7] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// Aspect-ratio overrides: label and mpv `video-aspect-override` value (`no` keeps the file's own).
pub const ASPECTS: [(&str, &str); 5] = [("Auto", "no"), ("16:9", "16:9"), ("4:3", "4:3"), ("21:9", "21:9"), ("1:1", "1:1")];

/// Zoom range in tenths of mpv's log2 `video-zoom` (-1 halves, +1 doubles the picture).
pub const ZOOM_RANGE: std::ops::RangeInclusive<i32> = -10..=10;

/// mpv `video-zoom` value for a zoom in tenths.
pub fn zoom_value(tenths: i32) -> String {
    (tenths.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end()) as f64 / 10.0).to_string()
}

/// mpv `hwdec` value: GPU decoders with zero-copy OpenGL interop only, or software decoding.
/// Copy-back decoders (e.g. vulkan-copy) corrupted frames in testing.
pub fn hwdec_value(on: bool) -> &'static str {
    if on { "nvdec,vaapi,videotoolbox,d3d11va" } else { "no" }
}

/// Preset text colors: label and mpv `sub-color` value.
pub const SUB_COLORS: [(&str, &str); 4] = [("White", "#FFFFFF"), ("Yellow", "#FFFF00"), ("Green", "#00FF00"), ("Cyan", "#00FFFF")];

impl Default for SubStyle {
    fn default() -> Self {
        Self { scale: 1.0, color: 0, background: false, delay: 0.0, audio_delay: 0.0 }
    }
}

impl SubStyle {
    /// Forces every field into its valid range (also guards NaN and stale stored values).
    pub fn clamped(self) -> Self {
        let n = |v: f64, lo: f64, hi: f64, d: f64| if v.is_finite() { v.clamp(lo, hi) } else { d };
        let t = |v: f64| (n(v, -60.0, 60.0, 0.0) * 10.0).round() / 10.0;
        Self { scale: n(self.scale, 0.5, 3.0, 1.0), color: self.color.min(SUB_COLORS.len() - 1), background: self.background, delay: t(self.delay), audio_delay: t(self.audio_delay) }
    }

    /// mpv properties to `set`. `sub-border-style` makes `sub-back-color` show as a box.
    pub fn props(&self) -> [(&'static str, String); 6] {
        let s = self.clamped();
        [
            ("sub-scale", format!("{:.2}", s.scale)),
            ("sub-color", SUB_COLORS[s.color].1.into()),
            ("sub-border-style", if s.background { "background-box" } else { "outline-and-shadow" }.into()),
            ("sub-back-color", "#99000000".into()),
            ("sub-delay", format!("{:.1}", s.delay)),
            ("audio-delay", format!("{:.1}", s.audio_delay)),
        ]
    }

    pub fn encode(&self) -> String {
        format!("{},{},{},{},{}", self.scale, self.color, self.background as u8, self.delay, self.audio_delay)
    }

    /// Parses [`encode`](Self::encode) output; any malformed value falls back to the default.
    pub fn decode(s: &str) -> Self {
        let p: Vec<&str> = s.split(',').collect();
        let (Some(Ok(scale)), Some(Ok(color)), Some(bg), Some(Ok(delay))) = (p.first().map(|v| v.parse()), p.get(1).map(|v| v.parse()), p.get(2), p.get(3).map(|v| v.parse())) else { return Self::default() };
        // Older values have no 5th field.
        let audio_delay = p.get(4).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        Self { scale, color, background: *bg == "1", delay, audio_delay }.clamped()
    }
}

pub struct Player {
    mpv: *mut c_void,
    render: *mut c_void,
    /// Repaint callback handed to mpv; freed after the render context.
    wake: *mut Box<dyn Fn() + Send + Sync>,
    /// Handed to mpv as the `get_proc_address` context; boxed so its address is stable.
    gl_proc: Box<GlProc>,
    /// Addon subtitles as (url, lang).
    subs: Vec<(String, String)>,
    slang: String,
    source: String,
    /// The file is open (FILE_LOADED seen since the last `loadfile`).
    loaded: bool,
    /// Subtitles off (last choice was "Off"); kept in sync by the menu so a reload honors it.
    pub subs_off: bool,
    alive: bool,
    pub state: State,
}

/// Fixed, documented socket path so external tools can query player state.
pub fn ipc_socket_path() -> PathBuf {
    std::env::var_os("STREMFLIX_RS_IPC")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                PathBuf::from(r"\\.\pipe\stremflix-rs-mpv")
            } else {
                std::env::temp_dir().join("stremflix-rs-mpv.sock")
            }
        })
}

fn cstr(s: &str) -> CString {
    // Interior NULs can't be passed to C; addon text is sanitized but stay safe anyway.
    CString::new(s.replace('\0', "")).unwrap()
}

fn err_text(code: c_int) -> String {
    unsafe { CStr::from_ptr(mpv_error_string(code)) }.to_string_lossy().into_owned()
}

extern "C" fn gl_proc_address(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    let f = unsafe { &*(ctx as *const GlProc) };
    f(unsafe { CStr::from_ptr(name) }) as *mut c_void
}

extern "C" fn on_update(data: *mut c_void) {
    let wake = unsafe { &*(data as *const Box<dyn Fn() + Send + Sync>) };
    wake();
}

/// `--key=value` / `--flag` / `--no-flag` command-line style args to (option, value) pairs.
pub fn parse_args(s: &str) -> Vec<(String, String)> {
    s.split_whitespace()
        .filter_map(|a| a.strip_prefix("--"))
        .map(|a| match a.split_once('=') {
            Some((k, v)) => (k.into(), v.into()),
            None => match a.strip_prefix("no-") {
                Some(k) => (k.into(), "no".into()),
                None => (a.into(), "yes".into()),
            },
        })
        .collect()
}

/// HTTP request headers as mpv options: User-Agent as the plain-string `user-agent`, the rest as the
/// comma-separated `http-header-fields` list. mpv reads `\,` as a literal comma, but a value's own
/// backslash would make that ambiguous, so such headers are dropped.
pub fn header_options(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut opts = Vec::new();
    let mut fields = Vec::new();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("user-agent") {
            opts.push(("user-agent".to_string(), v.clone()));
        } else if !v.contains('\\') {
            fields.push(format!("{k}: {}", v.replace(',', "\\,")));
        }
    }
    if !fields.is_empty() {
        opts.push(("http-header-fields".to_string(), fields.join(",")));
    }
    opts
}

/// mpv's `track-list` JSON to tracks; unknown shapes yield an empty list.
pub fn parse_tracks(json: &str) -> Vec<Track> {
    let Ok(serde_json::Value::Array(list)) = serde_json::from_str(json) else { return Vec::new() };
    list.iter()
        .filter_map(|t| {
            let id = t["id"].as_i64()?;
            let kind = t["type"].as_str()?.to_owned();
            let (title, lang) = (t["title"].as_str(), t["lang"].as_str());
            let label = match (title, lang) {
                (Some(t), Some(l)) => format!("{t} ({l})"),
                (Some(x), None) | (None, Some(x)) => x.to_owned(),
                (None, None) => format!("Track {id}"),
            };
            Some(Track { id, kind, label, lang: lang.map(str::to_owned), selected: t["selected"].as_bool().unwrap_or(false) })
        })
        .collect()
}

/// Validates a language preference for mpv's `alang`/`slang`: up to 8 comma-separated
/// tokens like `en`, `eng`, `pt-BR`. Returns the normalized list (empty clears the preference).
pub fn valid_langs(s: &str) -> Option<String> {
    let tokens: Vec<&str> = s.split(',').map(str::trim).filter(|t| !t.is_empty()).collect();
    let ok = |t: &str| {
        let mut parts = t.split('-');
        let head = parts.next().unwrap_or("");
        (2..=3).contains(&head.len()) && head.bytes().all(|b| b.is_ascii_alphabetic()) && parts.all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
    };
    (tokens.len() <= 8 && tokens.iter().all(|t| ok(t))).then(|| tokens.join(","))
}

/// `slang` with `lang` moved to the front (case-insensitive dedupe, max 8); None if the result isn't a valid preference.
pub fn prefer_lang(slang: &str, lang: &str) -> Option<String> {
    let lang = lang.trim();
    if lang.is_empty() {
        return None;
    }
    let rest = valid_langs(slang).unwrap_or_default();
    let list: Vec<&str> = std::iter::once(lang).chain(rest.split(',').filter(|x| !x.is_empty() && !x.eq_ignore_ascii_case(lang))).take(8).collect();
    valid_langs(&list.join(","))
}

/// Index of the first addon subtitle whose language matches a `slang` token, in preference order.
/// Compares primary subtags case-insensitively; `en` and `eng` are different tokens, list both.
fn pick_sub(subs: &[(String, String)], slang: &str) -> Option<usize> {
    let primary = |l: &str| l.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    slang.split(',').map(primary).find_map(|want| subs.iter().position(|(_, l)| primary(l) == want))
}

/// Subtitle files beside a local video as `(path, lang)`: `<stem>.<ext>` or `<stem>.<lang>.<ext>`, case-insensitive.
/// Reads one directory listing, no recursion. `lang` is empty unless the tag is 2-3 letters.
/// Returns nothing for URLs and paths that are not existing files.
pub fn sidecar_subs(video: &Path) -> Vec<(String, String)> {
    if video.to_string_lossy().contains("://") || !video.is_file() {
        return vec![];
    }
    let (Some(dir), Some(stem)) = (video.parent(), video.file_stem().and_then(|s| s.to_str())) else { return vec![] };
    let stem = stem.to_lowercase();
    let mut out: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| {
        let name = e.file_name().into_string().ok()?.to_lowercase();
        let (base, ext) = name.rsplit_once('.')?;
        if !["srt", "ass", "ssa", "vtt", "sub"].contains(&ext) {
            return None;
        }
        let tag = if base == stem { "" } else { base.strip_prefix(&stem)?.strip_prefix('.')? };
        let lang = if (2..=3).contains(&tag.len()) && tag.bytes().all(|b| b.is_ascii_lowercase()) { tag } else { "" };
        Some((e.path().to_string_lossy().into_owned(), lang.to_string()))
    }).collect();
    out.sort();
    out
}

impl Player {
    /// `source` is a URL or local path, passed to `loadfile` as a plain argument (never parsed as options).
    /// `wake` is called from mpv's thread whenever a new frame is ready.
    /// `headers` (validated addon request headers) go to this instance only; each launch is a fresh mpv,
    /// so they never leak to the next stream or host.
    /// yt-dlp runs only for the exact YouTube URL built from a validated `ytId`; `hwdec` toggles hardware decoding.
    /// Must be called, rendered and dropped with the GUI's GL context current.
    #[allow(clippy::too_many_arguments)] // player setup options; bundling them adds no clarity
    pub fn launch(source: &str, title: &str, start: f64, subtitles: &[(String, String)], [alang, slang]: [&str; 2], subs_off: bool, headers: &[(String, String)], hwdec: bool, gl_proc: GlProc, wake: Box<dyn Fn() + Send + Sync>) -> Result<Self> {
        load_mpv().map_err(anyhow::Error::msg)?;
        let mpv = unsafe { mpv_create() };
        if mpv.is_null() {
            bail!("failed to create mpv instance");
        }
        let mut p = Self { mpv, render: std::ptr::null_mut(), wake: Box::into_raw(Box::new(wake)), gl_proc: Box::new(gl_proc), subs: subtitles.to_vec(), slang: slang.to_string(), subs_off, source: source.to_string(), loaded: false, alive: true, state: State::default() };
        let socket = ipc_socket_path();
        let _ = std::fs::remove_file(&socket);
        let mut opts: Vec<(String, String)> = [
            ("vo", "libmpv"),
            ("input-ipc-server", &*socket.to_string_lossy()),
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("ytdl", if crate::addon::is_youtube(source) { "yes" } else { "no" }),
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("hwdec", hwdec_value(hwdec)),
            ("force-media-title", title),
            ("start", &format!("{start:.1}")),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        for (k, v) in [("alang", alang), ("slang", slang)] {
            if !v.is_empty() {
                opts.push((k.into(), v.into()));
            }
        }
        if subs_off {
            opts.push(("sid".into(), "no".into()));
        }
        opts.extend(header_options(headers));
        opts.extend(parse_args(&std::env::var("STREMFLIX_RS_MPV_ARGS").unwrap_or_default()));
        for (k, v) in &opts {
            let rc = unsafe { mpv_set_option_string(mpv, cstr(k).as_ptr(), cstr(v).as_ptr()) };
            if rc < 0 {
                bail!("mpv option {k}={v}: {}", err_text(rc));
            }
        }
        let rc = unsafe { mpv_initialize(mpv) };
        if rc < 0 {
            bail!("mpv init: {}", err_text(rc));
        }
        let mut init = GlInitParams { get_proc_address: gl_proc_address, ctx: &*p.gl_proc as *const GlProc as *mut c_void };
        let mut params = [
            RenderParam { kind: 1, data: c"opengl".as_ptr() as *mut c_void },
            RenderParam { kind: 2, data: &mut init as *mut GlInitParams as *mut c_void },
            RenderParam { kind: 0, data: std::ptr::null_mut() },
        ];
        let rc = unsafe { mpv_render_context_create(&mut p.render, mpv, params.as_mut_ptr()) };
        if rc < 0 {
            bail!("mpv render context: {}", err_text(rc));
        }
        unsafe { mpv_render_context_set_update_callback(p.render, Some(on_update), p.wake as *mut c_void) };
        // Also wake on events (pause, volume, tracks), not only on new frames, so a paused UI updates at once.
        unsafe { mpv_set_wakeup_callback(mpv, Some(on_update), p.wake as *mut c_void) };
        for (i, (name, fmt)) in PROPS.iter().enumerate() {
            unsafe { mpv_observe_property(mpv, i as u64, cstr(name).as_ptr(), *fmt) };
        }
        p.command(&["loadfile", source])?;
        Ok(p)
    }

    /// Runs an mpv command without blocking the UI thread.
    pub fn command(&self, args: &[&str]) -> Result<()> {
        let owned: Vec<CString> = args.iter().map(|a| cstr(a)).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let rc = unsafe { mpv_command_async(self.mpv, 0, ptrs.as_ptr()) };
        if rc < 0 {
            bail!("mpv {}: {}", args[0], err_text(rc));
        }
        Ok(())
    }

    /// Seeks to `to` seconds, or by `to` if `relative`. mpv runs a seek only after the stream read in
    /// flight returns, and a torrent read waits for its piece, so while the file is still opening,
    /// buffering or seeking, this reopens it at the target instead: that cancels the stalled read.
    pub fn seek(&mut self, to: f64, relative: bool) -> Result<()> {
        if self.loaded && !self.state.buffering && !self.state.seeking {
            return self.command(&["seek", &format!("{to:.1}"), if relative { "relative+exact" } else { "absolute" }]);
        }
        let mut t = if relative { self.state.pos + to } else { to }.max(0.0);
        if self.state.dur > 0.0 {
            t = t.min(self.state.dur);
        }
        // `start` then `loadfile`, not loadfile's per-file options: their position changed across mpv versions.
        self.command(&["set", "start", &format!("{t:.1}")])?;
        self.command(&["loadfile", &self.source, "replace"])?;
        self.loaded = false;
        // Shown at once, and the base for further skips before the reopened file reports its position.
        self.state.pos = t;
        Ok(())
    }

    /// Sets the subtitle style properties (async, like every other command).
    pub fn apply_sub_style(&self, st: &SubStyle) -> Result<()> {
        for (k, v) in st.props() {
            self.command(&["set", k, &v])?;
        }
        Ok(())
    }

    /// Drains mpv events into [`State`]. Returns false once mpv has shut down.
    pub fn poll(&mut self) -> bool {
        while self.alive {
            let ev = unsafe { &*mpv_wait_event(self.mpv, 0.0) };
            match ev.event_id {
                0 => break,
                EVENT_SHUTDOWN => self.alive = false,
                EVENT_FILE_LOADED => {
                    // Addon subtitles: none if off, else the one matching `slang`; without a preference, the first (like `--sub-file`).
                    // Kept, not taken: a reload from `seek` opens the file again.
                    self.loaded = true;
                    let pick = if self.subs_off { None } else if self.slang.is_empty() { Some(0) } else { pick_sub(&self.subs, &self.slang) };
                    for (i, (url, lang)) in self.subs.iter().enumerate() {
                        let _ = self.command(&["sub-add", url, if Some(i) == pick { "select" } else { "auto" }, "", lang]);
                    }
                }
                EVENT_END_FILE => {
                    let end = unsafe { &*(ev.data as *const [c_int; 2]) };
                    if end[0] == END_FILE_REASON_ERROR {
                        self.state.error = Some(format!("playback failed: {}", err_text(end[1])));
                    }
                }
                EVENT_PROPERTY_CHANGE => self.on_property(ev.reply_userdata as usize, unsafe { &*(ev.data as *const MpvEventProperty) }),
                _ => {}
            }
        }
        self.alive
    }

    fn on_property(&mut self, idx: usize, p: &MpvEventProperty) {
        let s = &mut self.state;
        if p.data.is_null() {
            // Property unavailable (e.g. no file yet): keep numbers, clear what is file-specific.
            if idx == 8 {
                s.tracks.clear();
            }
            return;
        }
        unsafe {
            let f = || *(p.data as *const f64);
            let flag = || *(p.data as *const c_int) != 0;
            let int = || (*(p.data as *const i64)).max(0) as usize;
            match idx {
                0 => s.pos = f(),
                1 => s.dur = f(),
                2 => s.paused = flag(),
                3 => s.volume = f(),
                4 => s.muted = flag(),
                5 => s.buffering = flag(),
                6 => s.video[0] = int(),
                7 => s.video[1] = int(),
                8 => s.tracks = parse_tracks(&CStr::from_ptr(*(p.data as *const *const c_char)).to_string_lossy()),
                9 => s.speed = f(),
                10 => s.seeking = flag(),
                _ => {}
            }
        }
    }

    /// Current playback position and duration in seconds, if known.
    pub fn position(&self) -> Option<(f64, f64)> {
        (self.state.pos > 0.0).then_some((self.state.pos, self.state.dur))
    }

    /// Renders the current frame into GL framebuffer `fbo` (`size` pixels) when mpv has a new
    /// one, or always when `force` is set (e.g. after a resize). Returns whether it rendered.
    pub fn render(&mut self, fbo: u32, size: [usize; 2], force: bool) -> bool {
        let fresh = unsafe { mpv_render_context_update(self.render) } & 1 != 0;
        if !(fresh || force) || size[0] == 0 || size[1] == 0 {
            return false;
        }
        let mut target = GlFbo { fbo: fbo as c_int, w: size[0] as c_int, h: size[1] as c_int, internal_format: 0 };
        // No flip: mpv's unflipped output already has the top row where egui samples v = 0.
        let mut flip: c_int = 0;
        let mut params = [
            RenderParam { kind: 3, data: &mut target as *mut GlFbo as *mut c_void },
            RenderParam { kind: 4, data: &mut flip as *mut c_int as *mut c_void },
            RenderParam { kind: 0, data: std::ptr::null_mut() },
        ];
        unsafe { mpv_render_context_render(self.render, params.as_mut_ptr()) >= 0 }
    }

    pub fn stop(&mut self) {
        if self.mpv.is_null() {
            return;
        }
        unsafe {
            if !self.render.is_null() {
                mpv_render_context_free(self.render);
            }
            mpv_terminate_destroy(self.mpv);
            drop(Box::from_raw(self.wake));
        }
        self.mpv = std::ptr::null_mut();
        let _ = std::fs::remove_file(ipc_socket_path());
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_matching() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["Movie.mkv", "movie.SRT", "Movie.en.ass", "Movie.pt-BR.vtt", "Movie2.srt", "Other.srt", "Movie.txt"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        let got: Vec<_> = sidecar_subs(&dir.path().join("Movie.mkv")).into_iter().map(|(p, l)| (Path::new(&p).file_name().unwrap().to_string_lossy().into_owned(), l)).collect();
        assert_eq!(got, [("Movie.en.ass", "en"), ("Movie.pt-BR.vtt", ""), ("movie.SRT", "")].map(|(p, l)| (p.to_string(), l.to_string())));
        assert!(sidecar_subs(Path::new("https://x/Movie.mkv")).is_empty());
        assert!(sidecar_subs(&dir.path().join("Missing.mkv")).is_empty());
    }

    #[test]
    fn video_scaling_values() {
        assert_eq!(zoom_value(0), "0");
        assert_eq!(zoom_value(3), "0.3");
        assert_eq!(zoom_value(-99), "-1");
        assert_eq!(zoom_value(99), "1");
        assert_eq!(ASPECTS[0].1, "no");
    }

    #[test]
    fn libmpv_loads_with_every_symbol() {
        load_mpv().unwrap();
        assert!(!unsafe { CStr::from_ptr(mpv_error_string(0)) }.to_bytes().is_empty());
    }

    #[test]
    fn language_validation() {
        assert_eq!(valid_langs(" en, eng ,pt-BR"), Some("en,eng,pt-BR".into()));
        assert_eq!(valid_langs(""), Some(String::new()));
        for bad in ["e", "english", "en;fr", "en-", "en-x", "1a", "en,,e1", "a,b,c,d,e,f,g,h,i"] {
            assert_eq!(valid_langs(bad), None, "{bad}");
        }
        assert!(valid_langs("en,fr,de,es,it,pt,ja,ko").is_some());
        assert!(valid_langs("en,fr,de,es,it,pt,ja,ko,ru").is_none());
    }

    #[test]
    fn prefer_lang_moves_choice_first() {
        assert_eq!(prefer_lang("fr,EN,de", "en").as_deref(), Some("en,fr,de"));
        assert_eq!(prefer_lang("", "pt-BR").as_deref(), Some("pt-BR"));
        assert_eq!(prefer_lang("bad!", "en").as_deref(), Some("en"));
        assert_eq!(prefer_lang("fr,de,es,it,pt,ja,ko,ru", "en").as_deref(), Some("en,fr,de,es,it,pt,ja,ko"));
        assert_eq!(prefer_lang("en", "English"), None);
        assert_eq!(prefer_lang("en", " "), None);
    }

    #[test]
    fn slang_picks_by_preference_order() {
        let s = |l: &str| (String::new(), l.to_string());
        let subs = [s("fre"), s("ENG"), s("pt-BR")];
        assert_eq!(pick_sub(&subs, "pt,eng"), Some(2));
        assert_eq!(pick_sub(&subs, "en,eng"), Some(1));
        assert_eq!(pick_sub(&subs, "de"), None);
    }

    #[test]
    fn default_ipc_path_per_platform() {
        if std::env::var_os("STREMFLIX_RS_IPC").is_some() {
            return;
        }
        let p = ipc_socket_path();
        if cfg!(windows) {
            assert_eq!(p, PathBuf::from(r"\\.\pipe\stremflix-rs-mpv"));
        } else {
            assert_eq!(p, std::env::temp_dir().join("stremflix-rs-mpv.sock"));
        }
    }

    #[test]
    fn parses_cli_style_args() {
        let a = parse_args("--ao=null --no-audio-display --fs junk");
        assert_eq!(a, [("ao".into(), "null".into()), ("audio-display".into(), "no".into()), ("fs".into(), "yes".into())]);
    }

    #[test]
    fn sub_style_clamps_and_roundtrips() {
        let s = SubStyle { scale: 9.0, color: 99, background: true, delay: -500.0, audio_delay: 500.0 }.clamped();
        assert_eq!((s.scale, s.color, s.delay, s.audio_delay), (3.0, SUB_COLORS.len() - 1, -60.0, 60.0));
        assert_eq!(SubStyle::decode("1,0,0,0.5,-0.3").audio_delay, -0.3);
        assert_eq!(SubStyle::decode("1,0,0,0.5").audio_delay, 0.0);
        assert_eq!(SubStyle { audio_delay: 0.3, ..Default::default() }.props()[5], ("audio-delay", "0.3".into()));
        assert_eq!(SubStyle::decode(&s.encode()), s);
        assert_eq!(SubStyle::decode("junk"), SubStyle::default());
        assert_eq!(SubStyle::decode("NaN,0,0,0").scale, 1.0);
        assert_eq!(SubStyle { background: true, ..Default::default() }.props()[2].1, "background-box");
    }

    #[test]
    fn headers_to_mpv_options() {
        let h = |k: &str, v: &str| (k.to_string(), v.to_string());
        let o = header_options(&[h("User-Agent", "Moz (KHTML, like Gecko)"), h("Referer", "https://a.example/"), h("Accept", "a, b"), h("X-Bad", r"c\")]);
        assert_eq!(o, [h("user-agent", "Moz (KHTML, like Gecko)"), h("http-header-fields", r"Referer: https://a.example/,Accept: a\, b")]);
        assert!(header_options(&[]).is_empty());
    }

    #[test]
    fn parses_track_list() {
        let t = parse_tracks(r#"[{"id":1,"type":"sub","title":"English","lang":"en","selected":true},{"id":2,"type":"audio"},{"type":"sub"}]"#);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].label.as_str(), t[0].lang.as_deref(), t[0].selected), ("English (en)", Some("en"), true));
        assert_eq!(t[1].label, "Track 2");
        assert!(parse_tracks("not json").is_empty());
    }
}
