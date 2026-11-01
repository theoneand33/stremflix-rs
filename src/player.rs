//! Playback backend: libmpv embedded in-process. Frames come from mpv's software
//! render API so the GUI can draw them as a texture under its own controls.
//! `STREMIO_LITE_MPV_ARGS` adds mpv options (e.g. `--ao=null` for headless CI).
//! mpv's JSON IPC server stays on at [`ipc_socket_path`] so tools can query player state.

use anyhow::{Result, bail};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;

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
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

#[link(name = "mpv")]
unsafe extern "C" {
    fn mpv_create() -> *mut c_void;
    fn mpv_initialize(h: *mut c_void) -> c_int;
    fn mpv_set_option_string(h: *mut c_void, name: *const c_char, data: *const c_char) -> c_int;
    fn mpv_command_async(h: *mut c_void, reply: u64, args: *const *const c_char) -> c_int;
    fn mpv_observe_property(h: *mut c_void, reply: u64, name: *const c_char, format: c_int) -> c_int;
    fn mpv_wait_event(h: *mut c_void, timeout: f64) -> *mut MpvEvent;
    fn mpv_error_string(err: c_int) -> *const c_char;
    fn mpv_terminate_destroy(h: *mut c_void);
    fn mpv_render_context_create(ctx: *mut *mut c_void, h: *mut c_void, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_set_update_callback(ctx: *mut c_void, cb: Option<extern "C" fn(*mut c_void)>, data: *mut c_void);
    fn mpv_render_context_update(ctx: *mut c_void) -> u64;
    fn mpv_render_context_render(ctx: *mut c_void, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_free(ctx: *mut c_void);
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
const PROPS: [(&str, c_int); 9] = [
    ("time-pos", FORMAT_DOUBLE),
    ("duration", FORMAT_DOUBLE),
    ("pause", FORMAT_FLAG),
    ("volume", FORMAT_DOUBLE),
    ("mute", FORMAT_FLAG),
    ("paused-for-cache", FORMAT_FLAG),
    ("dwidth", FORMAT_INT64),
    ("dheight", FORMAT_INT64),
    ("track-list", FORMAT_STRING),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub id: i64,
    /// "audio", "video" or "sub".
    pub kind: String,
    pub label: String,
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
    /// Display size of the video in pixels; 0 until known.
    pub video: [usize; 2],
    pub tracks: Vec<Track>,
    /// Set when the file failed to open/play.
    pub error: Option<String>,
}

pub struct Player {
    mpv: *mut c_void,
    render: *mut c_void,
    /// Repaint callback handed to mpv; freed after the render context.
    wake: *mut Box<dyn Fn() + Send + Sync>,
    subs: Vec<String>,
    alive: bool,
    pub state: State,
}

/// Fixed, documented socket path so external tools can query player state.
pub fn ipc_socket_path() -> PathBuf {
    std::env::var_os("STREMIO_LITE_IPC")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("stremio-lite-mpv.sock"))
}

fn cstr(s: &str) -> CString {
    // Interior NULs can't be passed to C; addon text is sanitized but stay safe anyway.
    CString::new(s.replace('\0', "")).unwrap()
}

fn err_text(code: c_int) -> String {
    unsafe { CStr::from_ptr(mpv_error_string(code)) }.to_string_lossy().into_owned()
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
            Some(Track { id, kind, label, selected: t["selected"].as_bool().unwrap_or(false) })
        })
        .collect()
}

impl Player {
    /// `source` is a URL or local path, passed to `loadfile` as a plain argument (never parsed as options).
    /// `wake` is called from mpv's thread whenever a new frame is ready.
    pub fn launch(source: &str, title: &str, start: f64, subtitles: &[String], wake: Box<dyn Fn() + Send + Sync>) -> Result<Self> {
        let mpv = unsafe { mpv_create() };
        if mpv.is_null() {
            bail!("failed to create mpv instance");
        }
        let mut p = Self { mpv, render: std::ptr::null_mut(), wake: Box::into_raw(Box::new(wake)), subs: subtitles.to_vec(), alive: true, state: State::default() };
        let socket = ipc_socket_path();
        let _ = std::fs::remove_file(&socket);
        let mut opts: Vec<(String, String)> = [
            ("vo", "libmpv"),
            ("input-ipc-server", &*socket.to_string_lossy()),
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("ytdl", "no"),
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("hwdec", "auto-safe"),
            ("force-media-title", title),
            ("start", &format!("{start:.1}")),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        opts.extend(parse_args(&std::env::var("STREMIO_LITE_MPV_ARGS").unwrap_or_default()));
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
        let sw = c"sw";
        let mut params = [RenderParam { kind: 1, data: sw.as_ptr() as *mut c_void }, RenderParam { kind: 0, data: std::ptr::null_mut() }];
        let rc = unsafe { mpv_render_context_create(&mut p.render, mpv, params.as_mut_ptr()) };
        if rc < 0 {
            bail!("mpv render context: {}", err_text(rc));
        }
        unsafe { mpv_render_context_set_update_callback(p.render, Some(on_update), p.wake as *mut c_void) };
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

    /// Drains mpv events into [`State`]. Returns false once mpv has shut down.
    pub fn poll(&mut self) -> bool {
        while self.alive {
            let ev = unsafe { &*mpv_wait_event(self.mpv, 0.0) };
            match ev.event_id {
                0 => break,
                EVENT_SHUTDOWN => self.alive = false,
                EVENT_FILE_LOADED => {
                    // Addon subtitles: the first one is selected, like `--sub-file`.
                    for (i, s) in std::mem::take(&mut self.subs).iter().enumerate() {
                        let _ = self.command(&["sub-add", s, if i == 0 { "select" } else { "auto" }]);
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
                _ => {}
            }
        }
    }

    /// Current playback position and duration in seconds, if known.
    pub fn position(&self) -> Option<(f64, f64)> {
        (self.state.pos > 0.0).then_some((self.state.pos, self.state.dur))
    }

    /// Renders the current frame into `out` (RGBA, `size` pixels) when mpv has a new one,
    /// or always when `force` is set (e.g. after a resize). Returns whether `out` changed.
    pub fn render(&mut self, size: [usize; 2], force: bool, out: &mut [u8]) -> bool {
        assert_eq!(out.len(), size[0] * size[1] * 4);
        let fresh = unsafe { mpv_render_context_update(self.render) } & 1 != 0;
        if !(fresh || force) || size[0] == 0 || size[1] == 0 {
            return false;
        }
        let mut dims = [size[0] as c_int, size[1] as c_int];
        let mut stride = size[0] * 4;
        let fmt = c"rgb0";
        let mut params = [
            RenderParam { kind: 1, data: c"sw".as_ptr() as *mut c_void },
            RenderParam { kind: 17, data: dims.as_mut_ptr() as *mut c_void },
            RenderParam { kind: 18, data: fmt.as_ptr() as *mut c_void },
            RenderParam { kind: 19, data: &mut stride as *mut usize as *mut c_void },
            RenderParam { kind: 20, data: out.as_mut_ptr() as *mut c_void },
            RenderParam { kind: 0, data: std::ptr::null_mut() },
        ];
        if unsafe { mpv_render_context_render(self.render, params.as_mut_ptr()) } < 0 {
            return false;
        }
        // "rgb0" leaves the 4th byte undefined; make it opaque.
        out.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
        true
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
    fn parses_cli_style_args() {
        let a = parse_args("--ao=null --no-audio-display --fs junk");
        assert_eq!(a, [("ao".into(), "null".into()), ("audio-display".into(), "no".into()), ("fs".into(), "yes".into())]);
    }

    #[test]
    fn parses_track_list() {
        let t = parse_tracks(r#"[{"id":1,"type":"sub","title":"English","lang":"en","selected":true},{"id":2,"type":"audio"},{"type":"sub"}]"#);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].label.as_str(), t[0].selected), ("English (en)", true));
        assert_eq!(t[1].label, "Track 2");
        assert!(parse_tracks("not json").is_empty());
    }
}
