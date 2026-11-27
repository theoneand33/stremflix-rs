//! Stremflix-rs desktop GUI (egui/eframe). Network and torrent work runs on a tokio
//! runtime; results come back to the UI thread over a channel.

use eframe::egui;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use egui::{Align, Color32, Layout, RichText, Sense, UiBuilder, pos2, vec2};
use stremflix_rs::addon::{AddonClient, Skip, SkipKind, AddonEntry, CatalogDef, DeepLink, Manifest, Meta, MetaPreview, PlayTarget, Stream, Video, flags_to_codes, parse_magnet_hash, sanitize_text};
use stremflix_rs::{api, instance, key};
use stremflix_rs::settings::{self, Setting, SEEK_STEP_RANGE, clamp_scale};
use stremflix_rs::db::{Db, Download, List, Progress, SavedTitle, Watched};
use stremflix_rs::download::{self, State};
use stremflix_rs::library::{LibraryItem, is_media, scan};
use eframe::glow;
use stremflix_rs::profile;
use stremflix_rs::player::{self, chapter_skips, GlProc, Player, ASPECTS, SPEEDS, VOLUME_MAX, SUB_COLORS, SUB_OUTLINES, ZOOM_RANGE, SubStyle, hwdec_value, prefer_lang, valid_langs, zoom_value};
use stremflix_rs::stremio_import::{self, Import};
use stremflix_rs::torrent::{NetStats, Profile, SeedLimit, TorrentEngine, fmt_rate};
use stremflix_rs::tray::{self, Tray};

mod app;
mod widgets;
use app::{catalog::*, cli::*, episodes::*, geo::*, playback::*, posters::*, saved::*, streams::*, video::*};
use widgets::*;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Home,
    Discover,
    Addons,
    Library,
    Settings,
}

impl Tab {
    fn name(self) -> &'static str {
        match self { Tab::Home => "home", Tab::Discover => "discover", Tab::Addons => "addons", Tab::Library => "library", Tab::Settings => "settings" }
    }

    fn from_name(s: &str) -> Option<Tab> {
        [Tab::Home, Tab::Discover, Tab::Addons, Tab::Library, Tab::Settings].into_iter().find(|t| t.name() == s)
    }
}

/// Tab shown at the last exit; missing or unknown is Home.
const TAB: Setting<Tab> = Setting::new("tab", || Tab::Home, Tab::from_name, |t| t.name().into());
/// Side margin of every page; a banner bleeds out through it to the window edges.
const PAGE_X: i8 = 40;

/// Speed limit choices in bytes/s; 0 is unlimited.
const SPEED_LIMITS: [(&str, u32); 6] = [("Unlimited", 0), ("500 KB/s", 500_000), ("1 MB/s", 1_000_000), ("2 MB/s", 2_000_000), ("5 MB/s", 5_000_000), ("10 MB/s", 10_000_000)];

/// Torrent cache size choices: label and bytes kept (0 = delete a torrent when its playback stops).
const CACHE_SIZES: [(&str, u64); 5] = [("No cache", 0), ("2 GB", 2_000_000_000), ("5 GB", 5_000_000_000), ("10 GB", 10_000_000_000), ("Unlimited", u64::MAX)];

/// Seeding limit choices (0 = no limit of that kind): upload ratio, and seconds.
const SEED_RATIOS: [(&str, f64); 4] = [("Off", 0.0), ("1.0", 1.0), ("2.0", 2.0), ("5.0", 5.0)];
const SEED_TIMES: [(&str, u64); 4] = [("Off", 0), ("1 hour", 3600), ("6 hours", 21600), ("24 hours", 86400)];

/// What to do once a series' episodes arrive.
enum OnEpisodes {
    /// Show this video's streams (Continue Watching card, episode link).
    Pick(String),
    /// Resume this saved episode with its stream (Continue Watching play button).
    Resume(Progress),
    /// Resume the series' episode in progress, else start the next one (card play button).
    Next,
}

enum Msg {
    Error(String),
    /// No addon listed series `id`'s episodes: why, and the navigation it was for.
    NoEpisodes(String, String, u64),
    /// Play key and its TheIntroDB spans.
    IntroDb(String, Vec<Skip>),
    Installed(String, Manifest, Option<String>),
    /// Manifests fetched again on locking a profile, for addons stored before the `adult` flag was kept.
    Rated(Vec<(String, Manifest)>),
    /// Fresh manifest (or error) per installed addon URL, from "Check for updates".
    UpdateCheck(Vec<(String, Result<Manifest, String>)>),
    /// Query, the catalog searched (addon URL, type, catalog id), and its results or error.
    Search(String, (String, String, String), Result<Vec<MetaPreview>, String>),
    /// Row source (addon URL, type, catalog id) and its items or error.
    Row((String, String, String), Result<Vec<MetaPreview>, String>),
    /// Row source, the `skip` it was requested with, and the next page or error.
    More((String, String, String), usize, Result<Vec<MetaPreview>, String>),
    /// Addon catalog source (addon URL, type, catalog id) and the addons it lists, or an error.
    AddonCatalog(AddonCatalogSrc, Result<Vec<AddonEntry>, String>),
    /// Discover grid source and its items or error.
    Discover(DiscoverSrc, Result<Vec<MetaPreview>, String>),
    /// Next page of the Discover grid: selection, the `skip` it was requested with, items.
    DiscoverMore(DiscoverSrc, usize, Result<Vec<MetaPreview>, String>),
    /// meta, episode (series only), streams, subtitle URLs, what to autoplay on arrival, and the `nav` it was requested at
    Streams(MetaPreview, Option<Video>, Vec<Stream>, Subs, Autoplay, u64),
    /// Next episode's id and its streams and subtitles, fetched ahead near the current one's end (`None` on failure).
    Preloaded(String, Option<(Vec<Stream>, Subs)>),
    /// series, its full meta (with episodes), and the `nav` it was requested at
    Episodes(MetaPreview, Meta, u64),
    /// Full meta of a movie (genres, cast, ...) for the detail page.
    Details(Meta),
    /// `new_eps_gen`, a saved series and its recently released episodes.
    NewEpisodes(u64, MetaPreview, Vec<Video>),
    /// Cache key, the scale its size was chosen for, and the decoded image (`None` on failure).
    Poster(String, f32, Option<egui::ColorImage>),
    /// key, title, resumable source, playable URL/path, subtitle URLs, torrent id, sleep timer carried over, the `nav` it was requested at
    Play(String, String, String, String, Subs, Option<usize>, Option<Sleep>, u64),
    /// key, title, magnet, subtitle URLs, the media files `(index, name, size)` to choose from, the `nav` it was requested at
    PickFile(String, String, String, Subs, Vec<MediaFile>, u64),
    /// progress key, poster URL found for a Continue Watching entry
    RecentPoster(String, String),
    /// Stremio storage that was read, and what it holds.
    Imported(Import),
    /// Stremio account email, and its auth key or the sign-in error.
    SignedIn(String, Result<String, String>),
    /// Account auth key a sync ran with, and what it pulled or the error.
    Synced(String, Result<Import, String>),
    /// Account auth key an upload ran with, and its summary or the error.
    Uploaded(String, Result<String, String>),
    /// Library folder and its scanned media files.
    /// Folder, whether it was a rescan of a listed folder (dropped if the folder was removed meanwhile), items.
    Scanned(PathBuf, bool, Result<Vec<LibraryItem>, String>),
    /// Release page of a newer build and its download for this platform, if any.
    Update(Option<(String, Option<String>)>),
    /// The self-update finished installing, or failed.
    UpdateInstalled(Result<(), String>),
    /// A `stremio://` link from the command line, a later launch, or macOS.
    Link(String),
    /// A running download's progress or end.
    Download(download::Event),
    /// A tray menu item or icon click.
    Tray(tray::Action),
    /// Progress key and the subtitles every addon offered for it.
    SubSearch(String, Vec<SubFound>),
    /// A saved subtitle's path, or why saving failed.
    SubSaved(Result<PathBuf, String>),
    /// A media key or OS media overlay button.
    Media(souvlaki::MediaControlEvent),
    /// A local API request and where its reply goes.
    Api(api::Call, Sender<api::Reply>),
}

/// Content types that can have their own addon order.
const ADDON_ORDER_KINDS: [&str; 2] = ["movie", "series"];

/// Continue Watching and Watch It Again lengths.
const RECENT_SHOWN: usize = 20;
const WATCHED_SHOWN: usize = 50;

/// Player popover opened from its control-row button.
#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Audio,
    Subtitles,
    Playback,
}

/// An update must keep the installed addon's id, or it would replace a different addon.
fn same_addon(installed: &Manifest, fetched: &Manifest) -> bool {
    installed.id == fetched.id
}

/// Health label for a failed addon check, classified from the typed error: only a transport
/// failure is "unreachable"; HTTP status and manifest parse errors mean the host answered.
fn health_error(e: &anyhow::Error) -> String {
    match e.chain().find_map(|c| c.downcast_ref::<reqwest::Error>()) {
        Some(r) if r.is_timeout() => "Timed out".into(),
        Some(_) => format!("Unreachable: {}", e.root_cause()),
        None => format!("Error: {}", e.root_cause()),
    }
}

/// Poster art rarely has readable text at card size, so the tooltip leads with the title.
/// Rating badge on the poster, and the title and year under it.
fn meta_caption(ui: &egui::Ui, r: &egui::Response, rect: egui::Rect, m: &MetaPreview) {
    if let Some(rating) = &m.imdb_rating {
        card_badge(ui, rect, rating);
    }
    card_caption(ui, r, &m.name, m.release_info.as_deref().unwrap_or(""));
}

fn card_tip(m: &MetaPreview) -> String {
    m.description.as_ref().map_or_else(|| m.name.clone(), |d| format!("{}\n\n{d}", m.name))
}

fn stream_row(ui: &mut egui::Ui, n: usize, s: &Stream, reserve: f32) -> egui::Response {
    let mut parts = [&s.name, &s.title, &s.description].into_iter().flatten().map(|x| flags_to_codes(x.trim())).filter(|x| !x.is_empty());
    let head = parts.next().map_or("Stream".into(), |h| h.replace('\n', " "));
    let parts: Vec<String> = parts.collect();
    // Torrent addons already describe the file (name, size) in their text; don't repeat it.
    let stem = s.hints.filename.as_deref().map(|f| f.rsplit_once('.').map_or(f, |x| x.0)).filter(|f| !f.is_empty());
    let described = stem.is_some_and(|f| parts.iter().any(|p| p.contains(&flags_to_codes(f))));
    let file = if described { String::new() } else { [s.hints.filename.clone(), s.hints.video_size.map(fmt_size)].into_iter().flatten().collect::<Vec<_>>().join("  ·  ") };
    let external = (s.target().is_none()).then_some("Opens in your browser");
    let not_web_ready = s.hints.not_web_ready.then_some("Not web-ready (plays directly here)");
    let details: Vec<&str> = parts.iter().map(String::as_str).chain(Some(file.as_str()).filter(|f| !f.is_empty())).chain(not_web_ready).chain(external).collect();
    list_row(ui, n, head, details.join("\n"), reserve)
}

/// Byte count as decimal MB/GB, like addons label sizes.
fn fmt_size(b: u64) -> String {
    if b >= 1_000_000_000 { format!("{:.2} GB", b as f64 / 1e9) } else { format!("{:.0} MB", b as f64 / 1e6) }
}

/// Episode row: number, title, release date.
fn episode_row(ui: &mut egui::Ui, v: &Video, watched: bool, today: &str) -> egui::Response {
    let n = v.episode.unwrap_or(0) as usize;
    let head = v.title.clone().unwrap_or_else(|| format!("Episode {n}"));
    let date = episode_date(v.released.as_deref(), today);
    list_row(ui, n, if watched { format!("{head}  {}", icons::CHECK) } else { head }, date, EPISODE_BUTTON.x + 12.0)
}

/// Size of the "Mark watched" pill overlaid on episode rows, left of the play circle.
const EPISODE_BUTTON: egui::Vec2 = egui::Vec2::new(150.0, 32.0);

/// OS media controls, sending their events to the UI thread; `None` (logged) where unavailable.
fn media_controls(cc: &eframe::CreationContext, tx: Sender<Msg>) -> Option<souvlaki::MediaControls> {
    #[cfg(windows)]
    let hwnd = match raw_window_handle::HasWindowHandle::window_handle(cc).map(|h| h.as_raw()) {
        Ok(raw_window_handle::RawWindowHandle::Win32(h)) => Some(h.hwnd.get() as *mut std::ffi::c_void),
        _ => {
            stremflix_rs::elog!("media controls: no window handle");
            return None;
        }
    };
    #[cfg(not(windows))]
    let hwnd = None;
    let config = souvlaki::PlatformConfig { display_name: "Stremflix-rs", dbus_name: "stremflix_rs", hwnd };
    let mut controls = souvlaki::MediaControls::new(config).inspect_err(|e| stremflix_rs::elog!("media controls: {e:?}")).ok()?;
    let ctx = cc.egui_ctx.clone();
    // shortcut: without a D-Bus session souvlaki's MPRIS thread panics (logged on stderr) and the app runs on without controls; pre-check the bus if that noise matters.
    let attached = controls.attach(move |e| {
        let _ = tx.send(Msg::Media(e));
        ctx.request_repaint();
    });
    attached.inspect_err(|e| stremflix_rs::elog!("media controls: {e:?}")).ok().map(|()| controls)
}

struct Playing {
    player: Player,
    key: String,
    title: String,
    source: String,
    /// What mpv opened: `source`, or the local torrent stream URL.
    url: String,
    /// Torrent being streamed, if any.
    torrent: Option<usize>,
    pos: f64,
    dur: f64,
    poster: Option<String>,
    /// Controls stay visible until this time (seconds, egui clock).
    osd_until: f64,
    /// Open player popover, if any.
    menu: Option<Menu>,
    /// Seek-bar position while dragging, in seconds.
    scrub: Option<f64>,
    /// Seek-bar previews (none for torrents, whose random reads would compete with playback, and YouTube) and the last one shown.
    thumbs: Option<player::Thumbs>,
    thumb: Option<egui::TextureHandle>,
    /// Video held down: playing at 2x until release.
    fast: bool,
    /// Press start (egui clock) of a press that began on the video, while the button stays down.
    hold: Option<f64>,
    /// The last click on the video was a hold release, so the next one must not pair with it into a double-click.
    after_hold: bool,
    /// Wheel scroll not yet turned into volume steps (points).
    wheel: f32,
    /// Volume the user asked for; the controls draw this because mpv's reported value lags behind.
    volume: f64,
    /// The up-next card was dismissed.
    next_dismissed: bool,
    /// Addon-marked spans of the playing stream (`behaviorHints.skip`).
    skips: Vec<Skip>,
    /// TheIntroDB spans; `None` until requested.
    db_skips: Option<Vec<Skip>>,
    /// End of the span whose skip button was pressed; hidden until the playhead leaves it.
    skip_done: Option<f64>,
    /// Stats overlay shown; `net` is its torrent line, refreshed by a task about once per second (`net_next`, egui clock).
    stats: bool,
    net: Arc<std::sync::Mutex<Option<NetStats>>>,
    net_next: f64,
    search: SubSearch,
    /// Per-session picture scaling: aspect override index into `ASPECTS`, fill (crop) instead of fit, zoom in tenths.
    aspect: usize,
    fill: bool,
    /// A-B loop points in seconds, as set from the menu.
    ab: [Option<f64>; 2],
    /// Repeat the current file.
    repeat: bool,
    zoom: i32,
    /// Screen/idle inhibit, attempted once per unpaused stretch (inner `None`: unavailable); embedded mpv can't do it itself.
    awake: Option<Option<keepawake::KeepAwake>>,
    /// Video track to restore once the window is shown again; video is off while hidden.
    hidden_vid: Option<String>,
    /// Paused by the pause-on-minimize setting, so only then resume on restore.
    auto_pause: AutoPause,
}

impl Drop for Playing {
    fn drop(&mut self) {
        release_awake(self.awake.take());
    }
}

struct App {
    sub_style: SubStyle,
    rt: tokio::runtime::Runtime,
    db: Db,
    /// The active profile's folder: `root` for the default profile.
    data_dir: PathBuf,
    /// The app's data folder, which holds every profile.
    root: PathBuf,
    /// Active profile; empty is the default one.
    profile: String,
    /// Profile picked in Settings for the next launch, and the name typed for a new one.
    profile_next: String,
    profile_new: String,
    /// PIN of a child-safe profile; empty when it is not locked.
    // shortcut: kept as typed in the profile's database, which anyone who can read it can also edit; hash it if the lock must resist that.
    child_pin: String,
    pin_entry: String,
    client: AddonClient,
    torrent: Arc<tokio::sync::OnceCell<Arc<TorrentEngine>>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    tab: Tab,
    addons: Vec<(String, Manifest)>,
    addon_url: String,
    /// Last "Check for updates" result per addon URL.
    addon_updates: Vec<(String, Result<Manifest, String>)>,
    checking_updates: bool,
    /// Addon being reconfigured (old transport URL) and the pasted result URL.
    configuring: Option<(String, String)>,
    /// Addon catalog picked in the Addons tab (addon URL, type, catalog id) and its addons (`None` while loading).
    browse: Option<(AddonCatalogSrc, Option<Vec<AddonEntry>>)>,
    rows: Vec<Row>,
    /// Set while the search results page is up (it lives on Home, under any open title).
    search_page: Option<SearchPage>,
    discover: Discover,
    selected: Option<(MetaPreview, Vec<Stream>, Subs)>,
    /// Episodes of the selected series (in `sort_episodes` order), the season shown,
    /// and the episode whose streams are in `selected`.
    episodes: Vec<Video>,
    season: Option<u32>,
    episode: Option<Video>,
    /// Series id whose episodes are loading, and what to do once they arrive.
    pending: Option<(String, OnEpisodes)>,
    /// Bumped by every navigation (open a title or episode, play, Back); replies for an older one are dropped.
    nav: u64,
    /// `nav` and title of a title or episode whose streams or episodes were asked for and have not arrived.
    opening: Option<(u64, String)>,
    /// Full meta of the selected title, if an addon served one; checked against `selected` by id.
    details: Option<Meta>,
    /// Type and id of the movie whose details were last requested; older replies are dropped.
    details_want: (String, String),
    scroll_top: bool,
    /// Scroll the stream picker into view on the next frame.
    show_streams: bool,
    magnet: String,
    search: String,
    /// Recent submitted queries, newest first; persisted in settings as `search_history`.
    search_history: Vec<String>,
    /// The search palette is up.
    search_open: bool,
    /// Fetched images by URL, or `CARD_KEY` + URL for card-sized ones; a missing key has not been requested.
    posters: HashMap<String, Poster>,
    /// Frame being drawn, stamped on the posters it uses.
    poster_frame: u64,
    /// Highest scale so far: card posters are sized for it, so zooming back in stays sharp.
    poster_ppp: f32,
    /// Limits concurrent poster decodes; a screenful arrives at once.
    poster_slots: Arc<tokio::sync::Semaphore>,
    folder: String,
    /// Preferred audio / subtitle languages (mpv `alang` / `slang`), as edited; saved to the settings table only when valid.
    langs: [String; 2],
    /// Custom User-Agent as edited; saved and applied only when valid.
    user_agent: String,
    library: Vec<LibraryItem>,
    library_folders: Vec<String>,
    /// Titles saved with "Add to library", newest first.
    saved: Vec<SavedTitle>,
    /// User lists of saved titles; `lib_list` is the one shown in Library (`None` = all saved).
    lists: Vec<List>,
    lib_list: Option<i64>,
    /// Name typed for a new or renamed list.
    list_name: String,
    /// Saved series with episodes released in the last `NEW_EPISODE_DAYS` days (watched ones are hidden when drawn).
    new_episodes: Vec<(MetaPreview, Vec<Video>)>,
    /// Saved series with episodes releasing within `CALENDAR_DAYS` days, for the Library calendar.
    upcoming: Vec<(MetaPreview, Vec<Video>)>,
    /// Bumped on each `build_rows`; stale `Msg::NewEpisodes` replies are dropped.
    new_eps_gen: u64,
    /// "Notify about new episodes" setting, the keys already notified, the last check, and the in-app banner.
    notify_eps: bool,
    notified: Vec<String>,
    notify_check: Instant,
    notice: Option<String>,
    /// Finished downloads, newest first.
    downloads: Vec<Download>,
    queue: download::Queue,
    lib_sort: LibSort,
    lib_kind: LibKind,
    /// Title filter of the Library tab.
    lib_query: String,
    recent: Vec<Progress>,
    watched: Vec<Watched>,
    /// Keys (`<type>:<id>`) of movies and episodes marked watched.
    watched_keys: std::collections::HashSet<String>,
    /// Stremio data path to import from; empty = auto-detect.
    import_path: String,
    /// Stremio account sign-in form, and the signed-in email (set while a `stremio_auth` key is stored).
    login: (String, String),
    account: Option<String>,
    /// A sign-in or sync is running.
    account_busy: bool,
    /// "Upload to account" was clicked and waits for confirmation.
    confirm_upload: bool,
    /// Result of the last upload.
    account_note: Option<String>,
    /// Addon whose details are shown in the Addons tab.
    addon_details: Option<String>,
    /// Torrent download folder as edited (empty = in the data directory); saved when valid.
    torrent_dir: String,
    torrent_cache: u64,
    torrent_profile: Profile,
    /// Proxy type ("", "http", "socks5"), host:port, username, password as saved; applied at startup.
    proxy: [String; 4],
    /// (download, upload) in bytes/s; 0 is unlimited.
    torrent_limits: (u32, u32),
    torrent_seed: SeedLimit,
    /// Torrent row in the manager awaiting a delete confirmation.
    confirm_delete: Option<usize>,
    /// Downloaded file in Library awaiting a delete confirmation.
    confirm_delete_dl: Option<PathBuf>,
    /// Addon (transport URL) in Addons awaiting an uninstall confirmation.
    confirm_uninstall: Option<String>,
    /// A widget had keyboard focus last frame: egui drops focus on Escape before `ui` runs, so that Escape must not also go back.
    had_focus: bool,
    /// External player command, e.g. `vlc` or `mpv --fs`.
    external_player: String,
    /// Settings > Data: path typed for an import, and the last export/import result.
    data_import: String,
    data_msg: Option<String>,
    /// Settings > Diagnostics: where the last export went.
    diag_msg: Option<String>,
    /// mpv hardware decoding.
    hwdec: bool,
    /// Custom Home row order and hidden rows (row keys).
    home_order: Vec<String>,
    /// Addon priority per content type (movie, series): addon URLs, first = highest; a type without one follows the global order.
    addon_orders: HashMap<String, Vec<String>>,
    /// Type whose order the Addons tab edits; empty = the global order.
    addon_order_kind: String,
    home_hidden: Vec<String>,
    /// Every Home row as (key, title) in display order, hidden ones included.
    home_all: Vec<(String, String)>,
    light: bool,
    /// Index into `ACCENTS`.
    accent: usize,
    /// Play the next episode when one ends.
    autoplay_next: bool,
    introdb: bool,
    /// Closing the window hides it to the tray instead of quitting.
    tray_on_close: bool,
    /// Pause playback while the window is minimized or hidden.
    pause_on_minimize: bool,
    tray: Option<Tray>,
    /// Window hidden to the tray.
    hidden: bool,
    /// Quit was picked in the tray: let the close through.
    quit: bool,
    ui_scale: f32,
    /// Playback speed every video starts at.
    default_speed: f64,
    /// Player volume (0-130) every video starts at; saved when the user changes it.
    volume: f64,
    /// Arrow-key seek in seconds: plain and with Shift.
    seek_steps: [u32; 2],
    /// Last error, shown in a dismissible red line until cleared.
    error: Option<String>,
    /// A multi-file torrent waiting for the user to pick a file: key, title, magnet, subtitles, files, selected index.
    file_pick: Option<(String, String, String, Subs, Vec<MediaFile>, usize)>,
    /// Release page of a newer build and its download for this platform, shown until dismissed.
    update: Option<(String, Option<String>)>,
    /// A self-update is downloading and installing.
    installing: bool,
    /// Nav pill animation: tab it's heading to, start rect, target rect, start time.
    pill: Option<(Tab, egui::Rect, egui::Rect, f64)>,
    player: Option<Playing>,
    last_poll: Instant,
    /// Survives autoplay-next (it rides `Autoplay::Next` and `Msg::Play`), cleared when the player is left.
    sleep: Option<Sleep>,
    /// Next episode id and its preloaded streams and subtitles (`None` while fetching or after a failure); cleared with the player.
    preload: Option<(String, Option<(Vec<Stream>, Subs)>)>,
    /// The sleep timer paused playback: `Some(false)` until mpv reports paused, then `Some(true)` until resumed.
    /// Autoplay must not fire while set (see `timer_pause_step`).
    timer_paused: Option<bool>,
    /// GL function loader of eframe's context, for libmpv's OpenGL renderer.
    gl_proc: Option<GlProc>,
    window: Geo,
    video: Option<VideoSurface>,
    media: Option<souvlaki::MediaControls>,
    /// Title, paused and duration (s) last published to the media controls.
    media_shown: Option<(String, bool, u64)>,
}

fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("STREMFLIX_RS_DATA") {
        return d.into();
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| cfg!(windows).then(|| std::env::var_os("APPDATA").map(PathBuf::from)).flatten())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("stremflix-rs")
}

/// Password field for a child-safe PIN and its button; true when pressed (or Enter) with a PIN typed.
fn pin_row(ui: &mut egui::Ui, pin: &mut String, action: &str) -> bool {
    field_row(ui, "Child-safe PIN", |ui, l| {
        let r = ui.add(egui::TextEdit::singleline(pin).password(true).desired_width(200.0)).labelled_by(l);
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        (small_button(ui, action).clicked() || enter) && !pin.is_empty()
    })
}

fn wayland() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Show `e` in the UI and on stderr (headless runs, CI).
fn report(slot: &mut Option<String>, e: impl std::fmt::Display) {
    stremflix_rs::elog!("error: {e:#}");
    *slot = Some(format!("{e:#}"));
}

/// A deep-linked title only knows its id until its meta arrives: take the name and artwork from `d`.
fn fill_deep_linked(sel: &mut MetaPreview, d: &Meta) {
    if sel.name == sel.id && (&sel.kind, &sel.id) == (&d.kind, &d.id) {
        sel.name = d.name.clone();
        (sel.poster, sel.background, sel.description) = (d.poster.clone(), d.background.clone(), d.description.clone());
    }
}

impl App {
    fn new(root: PathBuf) -> anyhow::Result<Self> {
        let profile = profile::active(&root);
        let data_dir = profile::dir(&root, &profile);
        let db = Db::open(&data_dir.join("stremflix-rs.db"))?;
        let queue = download::Queue::load(&db, &data_dir)?;
        let search_history = settings::SEARCH_HISTORY.get(&db)?;
        let tab = TAB.get(&db)?;
        let (tx, rx) = channel();
        let ua = settings::USER_AGENT.get(&db)?;
        stremflix_rs::addon::set_user_agent(&stremflix_rs::addon::valid_user_agent(&ua).unwrap_or_default());
        let mut app = Self {
            sub_style: settings::SUB_STYLE.get(&db)?,
            rt: tokio::runtime::Runtime::new()?,
            addons: Vec::new(),
            library: db.library_items()?,
            library_folders: db.library_folders()?,
            saved: db.saved_titles()?,
            lists: db.lists()?,
            lib_list: None,
            list_name: String::new(),
            new_episodes: Vec::new(),
            upcoming: Vec::new(),
            new_eps_gen: 0,
            notify_eps: settings::NOTIFY_EPS.get(&db)?,
            notified: settings::NOTIFIED_EPS.get(&db)?,
            notify_check: Instant::now(),
            notice: None,
            downloads: db.downloads()?,
            queue,
            lib_sort: LibSort::Added,
            lib_kind: LibKind::All,
            lib_query: String::new(),
            recent: Vec::new(),
            watched: Vec::new(),
            watched_keys: Default::default(),
            import_path: String::new(),
            login: Default::default(),
            account: match settings::STREMIO_AUTH.get(&db)? {
                Some(_) => Some(Some(settings::STREMIO_EMAIL.get(&db)?).filter(|e| !e.is_empty()).unwrap_or_else(|| "a Stremio account".into())),
                None => None,
            },
            account_busy: false,
            confirm_upload: false,
            account_note: None,
            addon_details: None,
            torrent_dir: settings::TORRENT_DIR.get(&db)?,
            torrent_cache: settings::TORRENT_CACHE.get(&db)?,
            torrent_profile: settings::TORRENT_PROFILE.get(&db)?,
            proxy: [settings::PROXY[0].get(&db)?, settings::PROXY[1].get(&db)?, settings::PROXY[2].get(&db)?, settings::PROXY[3].get(&db)?],
            torrent_limits: (settings::TORRENT_DOWN_LIMIT.get(&db)?, settings::TORRENT_UP_LIMIT.get(&db)?),
            torrent_seed: SeedLimit { ratio: settings::SEED_RATIO.get(&db)?, secs: settings::SEED_SECS.get(&db)? },
            confirm_delete: None,
            confirm_delete_dl: None,
            confirm_uninstall: None,
            had_focus: false,
            data_import: String::new(),
            data_msg: None,
            diag_msg: None,
            external_player: settings::EXTERNAL_PLAYER.get(&db)?,
            hwdec: settings::HWDEC.get(&db)?,
            home_order: settings::HOME_ORDER.get(&db)?,
            addon_orders: ADDON_ORDER_KINDS.iter().map(|k| Ok((k.to_string(), settings::ADDON_ORDER.get_at(&db, k)?))).collect::<anyhow::Result<_>>()?,
            addon_order_kind: String::new(),
            home_hidden: settings::HOME_HIDDEN.get(&db)?,
            home_all: Vec::new(),
            light: settings::THEME_LIGHT.get(&db)?,
            accent: accent_index(&settings::ACCENT.get(&db)?),
            autoplay_next: settings::AUTOPLAY_NEXT.get(&db)?,
            introdb: settings::INTRODB.get(&db)?,
            tray_on_close: settings::TRAY_ON_CLOSE.get(&db)?,
            pause_on_minimize: settings::PAUSE_ON_MINIMIZE.get(&db)?,
            tray: None,
            hidden: false,
            quit: false,
            volume: settings::VOLUME.get(&db)?,
            ui_scale: settings::UI_SCALE.get(&db)?,
            default_speed: settings::DEFAULT_SPEED.get(&db)?,
            seek_steps: [settings::SEEK_STEP.get(&db)?, settings::SEEK_STEP_SHIFT.get(&db)?],
            error: None,
            file_pick: None,
            update: None,
            installing: false,
            folder: settings::LAST_FOLDER.get(&db)?,
            langs: [settings::ALANG.get(&db)?, settings::SLANG.get(&db)?],
            user_agent: ua,
            child_pin: settings::CHILD_PIN.get(&db)?,
            pin_entry: String::new(),
            profile_next: profile.clone(),
            profile_new: String::new(),
            profile,
            root,
            db,
            data_dir,
            client: AddonClient::new(),
            torrent: Default::default(),
            tx,
            rx,
            tab,
            addon_url: String::new(),
            addon_updates: Vec::new(),
            checking_updates: false,
            configuring: None,
            browse: None,
            rows: Vec::new(),
            search_page: None,
            discover: Discover::default(),
            selected: None,
            episodes: Vec::new(),
            season: None,
            episode: None,
            pending: None,
            nav: 0,
            opening: None,
            details: None,
            details_want: Default::default(),
            scroll_top: false,
            show_streams: false,
            magnet: String::new(),
            search: String::new(),
            search_history,
            search_open: false,
            posters: HashMap::new(),
            poster_frame: 0,
            poster_ppp: 0.0,
            poster_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            player: None,
            pill: None,
            last_poll: Instant::now(),
            sleep: None,
            preload: None,
            timer_paused: None,
            gl_proc: None,
            window: Geo::default(),
            video: None,
            media: None,
            media_shown: None,
        };
        app.reload_addons();
        app.reload_history();
        if app.addons.is_empty() && !app.locked() {
            app.tab = Tab::Addons;
        }
        // Neither check may block startup: a failure here only skips the notice.
        match app.db.quick_check() {
            Ok(Some(e)) => report(&mut app.error, format!("database check failed: {e}")),
            Ok(None) => {}
            Err(e) => eprintln!("database check: {e:#}"),
        }
        if app.db.begin_session().unwrap_or(false) {
            app.notice = Some("Stremflix did not exit cleanly last time. Playback progress is saved every second.".into());
        }
        let [kind, addr, user, pass] = &app.proxy;
        match stremflix_rs::addon::proxy_url(kind, addr, user, pass) {
            Ok(Some(p)) => {
                let _ = stremflix_rs::addon::PROXY.set(p);
            }
            Ok(None) => {}
            Err(e) => report(&mut app.error, format!("proxy setting ignored: {e}")),
        }
        Ok(app)
    }

    /// Auth key of the signed-in Stremio account.
    fn account_key(&self) -> Option<String> {
        settings::STREMIO_AUTH.get(&self.db).ok().flatten()
    }

    fn spawn<F: Future<Output = anyhow::Result<Msg>> + Send + 'static>(&self, ctx: &egui::Context, f: F) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(f.await.unwrap_or_else(|e| Msg::Error(format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    /// A child-safe profile: no Addons tab, no settings but the unlock, no adult addons, no new addons.
    fn locked(&self) -> bool {
        !self.child_pin.is_empty()
    }

    fn child_filter(&mut self) {
        if self.locked() {
            // An addon whose stored row predates the flag stays hidden until its manifest is fetched again (`Msg::Rated`).
            let Ok(unrated) = self.db.unrated_addons() else { return self.addons.clear() };
            self.addons.retain(|(u, m)| !m.behavior_hints.adult && !unrated.contains(u));
        }
    }

    fn handle(&mut self, ctx: &egui::Context, msg: Msg) {
        match msg {
            Msg::Error(e) => {
                self.opening = None;
                report(&mut self.error, e);
            }
            // A resume still plays its saved stream, only without the next episode to autoplay.
            Msg::NoEpisodes(id, why, nav) => {
                let resume = nav == self.nav && matches!(&self.pending, Some((p, OnEpisodes::Resume(_))) if *p == id);
                match self.pending.take_if(|_| resume) {
                    Some((_, OnEpisodes::Resume(p))) => {
                        self.opening = None;
                        self.play(ctx, p.key, p.title, p.source, vec![]);
                    }
                    _ => self.handle(ctx, Msg::Error(why)),
                }
            }
            // Only while still locked: nothing can have been removed since the fetch began.
            Msg::Rated(found) if self.locked() => {
                for (u, m) in &found {
                    if let Err(e) = self.db.add_addon(u, m) {
                        report(&mut self.error, format!("saving addon: {e:#}"));
                    }
                }
                self.reload_addons();
                self.build_rows(ctx);
            }
            Msg::Rated(_) => {}
            Msg::Installed(..) if self.locked() => report(&mut self.error, "this profile is child-safe: unlock it in Settings to add addons"),
            Msg::Installed(url, m, replaces) => {
                let saved = self.db.add_addon(&url, &m);
                if let Err(e) = &saved {
                    report(&mut self.error, format!("saving addon: {e:#}"));
                }
                // Keep the addon's priority: an update stays in place, a reconfigured one takes the old URL's place, a new one goes last.
                let is_slot = |u: &String| *u == url || replaces.as_ref() == Some(u);
                let slot = self.addons.iter().position(|(u, _)| is_slot(u)).unwrap_or(self.addons.len());
                self.addons.retain(|(u, _)| !is_slot(u));
                if let Some(old) = replaces {
                    if old != url && saved.is_ok() {
                        let _ = self.db.remove_addon(&old);
                    }
                    self.configuring = None;
                }
                // An install from Browse addons keeps whatever is typed in the URL box.
                if self.addon_url.trim() == url {
                    self.addon_url.clear();
                }
                self.addons.insert(slot, (url, m));
                if saved.is_ok() {
                    let urls: Vec<String> = self.addons.iter().map(|(u, _)| u.clone()).collect();
                    if let Err(e) = self.db.reorder_addons(&urls) {
                        report(&mut self.error, format!("saving addon order: {e:#}"));
                    }
                }
                self.build_rows(ctx);
                self.fetch_recent_posters(ctx);
            }
            Msg::IntroDb(key, skips) => {
                if let Some(p) = self.player.as_mut().filter(|p| p.key == key) {
                    p.db_skips = Some(skips);
                }
            }
            Msg::UpdateCheck(res) => {
                self.checking_updates = false;
                self.addon_updates = res;
            }
            Msg::Search(query, src, res) => {
                // Ignore replies for a search that was replaced or closed since the request.
                let Some(page) = self.search_page.as_mut().filter(|p| p.query == query) else { return };
                let Some(row) = page.rows.iter_mut().find(|r| r.src == src && r.page.items.is_none()) else { return };
                row.page.first(res.unwrap_or_else(|e| {
                    page.failed.push(e);
                    Vec::new()
                }));
                if page.failed.len() == page.rows.len() {
                    report(&mut self.error, format!("search failed: {}", page.failed.join("; ")));
                }
            }
            Msg::Row(src, res) => {
                let Some(row) = self.rows.iter_mut().find(|r| r.src == src) else { return };
                // A failure stays on its row (with a Retry), not in the page-wide error banner.
                let (items, error) = match res {
                    Ok(items) => (items, None),
                    Err(e) => (Vec::new(), Some(e)),
                };
                row.page.first(items);
                row.error = error;
            }
            Msg::More(src, skip, res) => {
                let Some(row) = self.rows.iter_mut().find(|r| r.src == src) else { return };
                if let Some(Err(e)) = row.page.on_page(skip, res) {
                    report(&mut self.error, format!("loading more of {}: {e:#}", row.title));
                }
            }
            Msg::AddonCatalog(src, res) => {
                // Ignore replies for a catalog the user has since switched away from.
                if self.browse.as_ref().is_some_and(|(cur, _)| *cur == src) {
                    match res {
                        Ok(items) => self.browse = Some((src, Some(items))),
                        // Deselect, so picking the catalog again retries.
                        Err(e) => {
                            self.browse = None;
                            report(&mut self.error, format!("loading addon catalog {}: {e:#}", sanitize_text(&src.2, 300)));
                        }
                    }
                }
            }
            Msg::Discover(src, res) => {
                // Ignore replies for a selection the user has since changed.
                if self.discover.src.as_ref() == Some(&src) {
                    let items = res.unwrap_or_else(|e| {
                        report(&mut self.error, format!("loading catalog: {e:#}"));
                        Vec::new()
                    });
                    self.discover.page.first(items);
                }
            }
            Msg::DiscoverMore(src, skip, res) => {
                // Ignore replies for a selection the user has since changed.
                if self.discover.src.as_ref() == Some(&src)
                    && let Some(Err(e)) = self.discover.page.on_page(skip, res)
                {
                    report(&mut self.error, format!("loading more of the catalog: {e:#}"));
                }
            }
            Msg::Streams(meta, episode, streams, subs, autoplay, nav) => {
                // Stale reply for a title the user has since left.
                if nav != self.nav {
                    return;
                }
                self.opening = None;
                if episode.is_none() {
                    self.episodes.clear();
                    self.scroll_top = true;
                }
                let (src, sleep) = match &autoplay {
                    Autoplay::Off => (None, None),
                    Autoplay::Only => (only_source(&streams), None),
                    Autoplay::Top(binge) => (binge.as_ref().and_then(|g| binge_source(&streams, g)).or_else(|| streams.iter().find_map(Stream::source)), None),
                    Autoplay::Next(binge, sleep) => (binge.as_ref().and_then(|g| binge_source(&streams, g)).or_else(|| only_source(&streams)), *sleep),
                };
                (self.selected, self.episode) = (Some((meta, streams, subs.clone())), episode);
                // Details may have arrived first; a deep-linked title only knew its id.
                if let (Some((sel, ..)), Some(d)) = (self.selected.as_mut(), &self.details) {
                    fill_deep_linked(sel, d);
                }
                // `Only` autoplays when there's no choice to make; otherwise show the stream picker.
                if autoplay != Autoplay::Off {
                    match (src, self.play_key()) {
                        (Some(src), Some((key, title))) => self.start_play(ctx, key, title, src, subs, sleep),
                        _ => self.show_streams = true,
                    }
                }
            }
            // A failure stays `None`, so it isn't retried and play_next fetches as before.
            Msg::Preloaded(id, found) => {
                if let (Some((want, slot)), Some(found)) = (&mut self.preload, found)
                    && *want == id
                {
                    *slot = Some(found);
                }
            }
            Msg::Episodes(mut meta, mut m, nav) => {
                if nav != self.nav {
                    return;
                }
                self.opening = None;
                fill_deep_linked(&mut meta, &m); // before the rename below, which the name guard depends on
                let mut videos = std::mem::take(&mut m.videos);
                // A Continue Watching entry only knows the episode title.
                let meta = MetaPreview { name: m.name.clone(), poster: meta.poster.or(m.poster.clone()), ..meta };
                self.backfill_saved_facts(&m);
                self.details = Some(m);
                sort_episodes(&mut videos);
                self.apply_imported_watched(&meta, &videos);
                let key = |v: &Video| key::item(&meta.kind, &v.id);
                // Open on the season of the episode in progress, else of the next unwatched one.
                let resume = self.recent.iter().find_map(|p| videos.iter().find(|v| key(v) == p.key));
                self.season = resume.or_else(|| start_episode(&videos, |v| self.watched_keys.contains(&key(v)))).and_then(|v| v.season);
                (self.episodes, self.episode) = (videos, None);
                self.selected = Some((meta.clone(), Vec::new(), Vec::new()));
                self.scroll_top = true;
                let Some((_, show)) = self.pending.take().filter(|(id, _)| *id == meta.id) else { return };
                let pick = matches!(show, OnEpisodes::Pick(_));
                let v = match show {
                    OnEpisodes::Pick(id) => self.episodes.iter().find(|v| v.id == id),
                    show => {
                        // Resume the chosen entry, else the series' episode in progress, with its stream; else start the next one.
                        let resume = match show {
                            OnEpisodes::Resume(p) => Some(p),
                            _ => self.recent.iter().find(|p| self.episodes.iter().any(|v| key(v) == p.key)).cloned(),
                        };
                        if let Some(p) = resume {
                            // Select the episode so the player offers the next one.
                            self.episode = self.episodes.iter().find(|v| key(v) == p.key).cloned();
                            self.season = self.episode.as_ref().and_then(|v| v.season);
                            return self.play(ctx, p.key, p.title, p.source, vec![]);
                        }
                        start_episode(&self.episodes, |v| self.watched_keys.contains(&key(v)))
                    }
                };
                let Some(v) = v.cloned() else { return };
                self.season = v.season;
                let autoplay = if pick { Autoplay::Off } else { Autoplay::Top(self.saved_binge(&meta)) };
                self.open_streams(ctx, meta, Some(v), autoplay);
            }
            Msg::Tray(tray::Action::Toggle) => self.show_window(ctx, self.hidden),
            Msg::Tray(tray::Action::PlayPause) => {
                if let Some(Err(e)) = self.player.as_ref().map(|p| p.player.command(&["cycle", "pause"])) {
                    report(&mut self.error, e);
                }
            }
            Msg::Tray(tray::Action::Quit) => {
                self.quit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Msg::Link(s) => {
                let link = stremflix_rs::addon::parse_deep_link(&s);
                if s.is_empty() {
                    self.show_window(ctx, true);
                } else if link.is_none() && !is_open_input(&s) {
                    report(&mut self.error, format!("unsupported link: {}", sanitize_text(&s, 200)));
                } else {
                    self.show_window(ctx, true);
                    self.leave_player(ctx);
                    match link {
                        Some(link) => self.open_link(ctx, link),
                        None if is_torrent_source(&s) => self.play_torrent(ctx, &s),
                        None => {
                            let name = std::path::Path::new(&s).file_stem().map_or("Video".into(), |n| n.to_string_lossy().into_owned());
                            self.play(ctx, key::file(&s), name, s, vec![]);
                        }
                    }
                }
            }
            Msg::NewEpisodes(epoch, meta, mut videos) => {
                if epoch == self.new_eps_gen {
                    sort_episodes(&mut videos);
                    self.apply_imported_watched(&meta, &videos);
                    let today = unix_days();
                    self.upcoming.retain(|(m, _)| m.id != meta.id);
                    let ahead = recent_episodes(videos.clone(), &date_string(today), &date_string(today + CALENDAR_DAYS));
                    if self.saved.iter().any(|t| t.key == key::item("series", &meta.id)) && !ahead.is_empty() {
                        self.upcoming.push((meta.clone(), ahead));
                    }
                    let videos = recent_episodes(videos, &date_string(today - (NEW_EPISODE_DAYS - 1)), &date_string(today));
                    if self.saved.iter().any(|t| t.key == key::item("series", &meta.id)) {
                        // Also with no recent episode: seeds the series' marker so its next one notifies.
                        let ids: Vec<&str> = videos.iter().map(|v| v.id.as_str()).collect();
                        let saved_ids: Vec<&str> = self.saved.iter().filter_map(|t| key::id_of(&t.key, "series")).collect();
                        prune_unsaved(&mut self.notified, &saved_ids);
                        let fresh = fresh_episodes(&mut self.notified, &meta.id, &ids);
                        if let Err(e) = settings::NOTIFIED_EPS.set(&self.db, &self.notified) {
                            report(&mut self.error, format!("saving notified episodes: {e:#}"));
                        }
                        let labels: Vec<String> = videos
                            .iter()
                            .filter(|v| fresh.contains(&v.id) && !self.watched_keys.contains(&key::item(&meta.kind, &v.id)))
                            .map(|v| format!("S{}E{}", v.season.unwrap_or(0), v.episode.unwrap_or(0)))
                            .collect();
                        if self.notify_eps && !labels.is_empty() {
                            let body = format!("New: {}", labels.join(", "));
                            self.notice = Some(format!("{}: {}", meta.name, body));
                            os_notify(meta.name.clone(), body);
                        }
                        upsert_by_id(&mut self.new_episodes, meta, videos);
                    }
                }
            }
            Msg::Details(m) => {
                if self.details_want == (m.kind.clone(), m.id.clone()) {
                    if let Some((sel, ..)) = self.selected.as_mut() {
                        fill_deep_linked(sel, &m);
                    }
                    self.backfill_saved_facts(&m);
                    self.details = Some(m);
                }
            }
            Msg::RecentPoster(key, url) => {
                if let Err(e) = self.db.set_progress_poster(&key, &url) {
                    report(&mut self.error, format!("saving poster: {e:#}"));
                }
                for p in self.recent.iter_mut().filter(|p| p.key == key) {
                    p.poster = Some(url.clone());
                }
            }
            // Dropped if signed out (or into another account) while it ran.
            Msg::Synced(key, imp) => {
                if self.account_key().as_ref() == Some(&key) {
                    // Only for the current key: a stale sync mustn't clear a newer sign-in's flag.
                    self.account_busy = false;
                    match imp {
                        Ok(imp) => self.handle(ctx, Msg::Imported(imp)),
                        Err(e) => report(&mut self.error, e),
                    }
                }
            }
            Msg::Uploaded(key, r) => {
                if self.account_key().as_ref() == Some(&key) {
                    self.account_busy = false;
                    match r {
                        Ok(note) => self.account_note = Some(note),
                        Err(e) => report(&mut self.error, e),
                    }
                }
            }
            // A sync or import that was running when the profile was locked.
            Msg::Imported(_) if self.locked() => {}
            Msg::Imported(imp) => {
                if let Err(e) = imp.save(&self.db) {
                    report(&mut self.error, format!("saving import: {e:#}"));
                    return;
                }
                self.reload_addons();
                self.saved = self.db.saved_titles().unwrap_or_default();
                self.reload_history();
                self.build_rows(ctx);
                self.fetch_recent_posters(ctx);
            }
            Msg::SignedIn(email, key) => {
                self.account_busy = false;
                let key = match key {
                    Ok(k) => k,
                    Err(e) => return report(&mut self.error, e),
                };
                let saved = self.db.transaction(|| {
                    settings::STREMIO_AUTH.set(&self.db, &Some(key))?;
                    settings::STREMIO_EMAIL.set(&self.db, &email)
                });
                match saved {
                    Ok(()) => {
                        self.account = Some(email);
                        self.login = Default::default();
                        self.sync_account(ctx);
                    }
                    Err(e) => report(&mut self.error, format!("saving sign-in: {e:#}")),
                }
            }
            Msg::Update(url) => self.update = url,
            Msg::UpdateInstalled(res) => {
                self.installing = false;
                match res {
                    Ok(()) => {
                        self.update = None;
                        self.notice = Some("Update installed. Restart Stremflix-rs to use it.".into());
                    }
                    Err(e) => self.error = Some(format!("Update failed: {e}")),
                }
            }
            Msg::SubSearch(key, found) => {
                if let Some(p) = self.player.as_mut().filter(|p| p.key == key) {
                    p.search.busy = false;
                    p.search.results = found;
                }
            }
            Msg::SubSaved(res) => {
                if let Some(p) = &mut self.player {
                    p.search.status = match res {
                        Ok(path) => format!("Saved {}", path.display()),
                        Err(e) => format!("Save failed: {e}"),
                    };
                }
            }
            Msg::Media(e) => self.media_event(ctx, e),
            Msg::Api(call, reply) => self.api_call(ctx, call, reply),
            Msg::Download(e) => {
                let step = self.queue.on_event(&self.db, e);
                self.apply_downloads(ctx, step);
            }
            Msg::Scanned(dir, rescan, res) => {
                if rescan && !self.library_folders.contains(&dir.to_string_lossy().into_owned()) {
                    return;
                }
                let saved = res.map_err(anyhow::Error::msg).and_then(|items| {
                    self.db.add_library_folder(&dir, &items)?;
                    settings::LAST_FOLDER.set(&self.db, &dir.to_string_lossy().into_owned())?;
                    Ok(items.len())
                });
                match saved {
                    Ok(_) => self.reload_library(),
                    Err(e) => report(&mut self.error, e),
                }
            }
            Msg::Poster(key, ppp, image) => {
                // Sized for a scale we have since passed (zoom, new monitor): drop it, keep drawing the old texture, and let the next draw ask again.
                if key.starts_with(CARD_KEY) && ppp != self.poster_ppp.max(1.0) {
                    if let Some(p) = self.posters.get_mut(&key).filter(|p| p.asked != self.poster_ppp.max(1.0)) {
                        p.loading = false;
                    }
                    return;
                }
                // A failed resize keeps the old texture, which is still a fine picture.
                let (old, old_bytes) = self.posters.remove(&key).map_or((None, 0), |p| (p.tex, p.bytes));
                let failed = image.is_none() && old.is_none();
                // The kept texture still occupies memory, so it still counts against the budget.
                let bytes = image.as_ref().map_or(old_bytes, |i| i.pixels.len() * 4);
                let tex = image.map(|i| ctx.load_texture(&key, i, egui::TextureOptions::LINEAR)).or(old);
                self.posters.insert(key, Poster { tex, failed, bytes, loading: false, asked: ppp, used: Cell::new(self.poster_frame) });
                // Cheap check first: building the list clones every key, and a screenful of replies arrives at once.
                if self.posters.len() > POSTER_CACHE || self.posters.values().map(|p| p.bytes).sum::<usize>() > POSTER_BYTES {
                    let entries = self.posters.iter().map(|(k, p)| (p.used.get(), p.bytes, k.clone())).collect();
                    for k in posters_to_evict(entries, self.poster_frame, POSTER_CACHE, POSTER_BYTES) {
                        self.posters.remove(&k);
                    }
                }
            }
            Msg::PickFile(key, title, source, subs, files, nav) => {
                if nav == self.nav {
                    let first = files[0].0;
                    self.file_pick = Some((key, title, source, subs, files, first));
                } else {
                    self.release_torrents(self.player.as_ref().and_then(|p| p.torrent));
                }
            }
            Msg::Play(key, title, source, url, subs, torrent, sleep, nav) => {
                if nav != self.nav {
                    // A torrent resolved after the user moved on: drop it, keeping only what is playing.
                    if torrent.is_some() {
                        self.release_torrents(self.player.as_ref().and_then(|p| p.torrent));
                    }
                    return;
                }
                self.end_playback();
                self.sleep = sleep;
                self.release_torrents(torrent);
                let prev = self.db.progress(&key).ok().flatten();
                let start = match &prev {
                    Some(p) if p.position > 3.0 && (p.duration <= 0.0 || p.position < p.duration - 5.0) => p.position,
                    _ => 0.0,
                };
                // Poster of the opened title (the series, for an episode), else the one saved earlier for this key.
                let poster = (self.selected.as_ref())
                    .filter(|_| self.play_key().is_some_and(|(k, _)| k == key))
                    .and_then(|(m, ..)| m.poster.clone())
                    .or(prev.and_then(|p| p.poster));
                let wake = ctx.clone();
                let Some(gl_proc) = self.gl_proc.clone() else {
                    report(&mut self.error, "no OpenGL context for the player");
                    return;
                };
                let skips = (self.selected.as_ref())
                    .and_then(|(_, streams, _)| streams.iter().find(|s| s.source().as_ref() == Some(&source)))
                    .map(|s| s.hints.skip.clone())
                    .unwrap_or_default();
                let lang = |s: &Setting<String>| s.get(&self.db).unwrap_or_default();
                let subs_off = settings::subs_off(&self.db).unwrap_or(true);
                let headers = self.selected.as_ref().map(|(_, streams, _)| request_headers(streams, &source, &url)).unwrap_or_default();
                let demuxer = self.selected.as_ref().and_then(|(_, streams, _)| own_stream(streams, &source, &url)).and_then(|s| s.hints.demuxer);
                self.remember_episode_stream(&key, &source);
                let (alang, slang) = (lang(&settings::ALANG), lang(&settings::SLANG));
                let launch = player::Launch { title: &title, start, subtitles: &subs, langs: [&alang, &slang], subs_off, headers: &headers, demuxer, hwdec: self.hwdec, speed: self.default_speed, volume: self.volume, sub_style: self.sub_style };
                match Player::launch(&url, &launch, gl_proc, Box::new(move || wake.request_repaint())) {
                    Ok(player) => {
                        let wake = ctx.clone();
                        let thumbs = (torrent.is_none() && !stremflix_rs::addon::is_youtube(&url)).then(|| player::Thumbs::new(&url, &headers, demuxer, Box::new(move || wake.request_repaint())));
                        let p = Playing { player, key, title, source, url, torrent, pos: start, dur: 0.0, poster, osd_until: ctx.input(|i| i.time) + 3.0, menu: None, scrub: None, thumbs, thumb: None, fast: false, hold: None, after_hold: false, wheel: 0.0, volume: self.volume, next_dismissed: false, aspect: 0, fill: false, ab: [None; 2], repeat: false, zoom: 0, search: SubSearch::default(), skips, db_skips: None, skip_done: None, stats: false, net: Default::default(), net_next: 0.0, awake: None, hidden_vid: None, auto_pause: AutoPause::Off };
                        self.player = Some(p);
                        // Force a fresh render so the previous video's last frame never flashes.
                        if let Some(v) = &mut self.video {
                            v.invalidate();
                        }
                    }
                    Err(e) => {
                        report(&mut self.error, e);
                        self.release_torrents(None);
                    }
                }
                ctx.request_repaint();
            }
        }
    }

    fn apply_imported_watched(&mut self, series: &MetaPreview, videos: &[Video]) {
        match stremio_import::apply_watched(&self.db, &series.kind, &series.id, videos) {
            Ok(marked) => self.watched_keys.extend(marked),
            Err(e) => report(&mut self.error, format!("saving watched episodes: {e:#}")),
        }
    }

    /// Re-read Continue Watching, Watch It Again and the watched marks after any of them changed.
    fn reload_history(&mut self) {
        self.recent = self.db.recent_progress(RECENT_SHOWN).unwrap_or_default();
        self.watched = self.db.watched(WATCHED_SHOWN).unwrap_or_default();
        self.watched_keys = self.db.watched_keys().unwrap_or_default();
    }

    /// Re-read installed addons, hiding what a child-safe profile must not see.
    fn reload_addons(&mut self) {
        self.addons = self.db.addons().unwrap_or_default();
        self.child_filter();
    }

    fn save_progress(&mut self) {
        // Trailers never enter Continue Watching or the watched list.
        if let Some(p) = self.player.as_ref().filter(|p| !p.key.starts_with(TRAILER)) {
            let prog = Progress { key: p.key.clone(), title: p.title.clone(), source: p.source.clone(), position: p.pos, duration: p.dur, poster: p.poster.clone() };
            self.record_progress(prog);
        }
    }

    /// Past 90% the title counts as watched and loses its resume point; before that its position is saved.
    fn record_progress(&mut self, p: Progress) {
        if p.duration > 0.0 && p.position >= p.duration * 0.9 {
            // Also for a rewatch of a watched title, so its resume point leaves Continue Watching.
            if !self.watched_keys.contains(&p.key) || self.recent.iter().any(|r| r.key == p.key) {
                self.set_watched(&p.key, true);
            }
        } else if p.position > 0.0 {
            if let Err(e) = self.db.save_progress(&p) {
                report(&mut self.error, format!("saving progress: {e:#}"));
            }
            self.reload_history();
        }
    }

    fn set_watched(&mut self, key: &str, on: bool) {
        self.set_watched_many(&[key.to_string()], on);
    }

    fn set_watched_many(&mut self, keys: &[String], on: bool) {
        match self.db.set_watched_many(keys, on) {
            Ok(()) => self.reload_history(),
            Err(e) => report(&mut self.error, format!("saving watched state: {e:#}")),
        }
    }

    /// Shows and focuses the window, or hides it to the tray.
    fn show_window(&mut self, ctx: &egui::Context, show: bool) {
        self.hidden = !show;
        // shortcut: Wayland can't hide a window (winit ignores Visible), so it minimizes there instead.
        if !show && wayland() {
            return ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(show));
        if show {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    /// Answer a local API request (API.md). Search replies from a task once every catalog answered.
    fn api_call(&mut self, ctx: &egui::Context, call: api::Call, reply: Sender<api::Reply>) {
        let r = match call {
            api::Call::Continue => Ok(serde_json::Value::from_iter(self.recent.iter().map(|p| {
                let (kind, id) = key::split(&p.key).unwrap_or_default();
                serde_json::json!({ "type": kind, "id": id, "title": p.title, "poster": p.poster, "position": p.position, "duration": p.duration })
            }))),
            api::Call::Search(q) => {
                let srcs: Vec<_> = self.search_catalogs().into_iter().map(|(_, src)| src).collect();
                if srcs.is_empty() {
                    Err((400, "no installed addon supports search".into()))
                } else {
                    let client = self.client.clone();
                    self.rt.spawn(async move {
                        let tasks: Vec<_> = srcs
                            .into_iter()
                            .map(|(u, k, c)| {
                                let (client, q) = (client.clone(), q.clone());
                                tokio::spawn(async move { client.search(&u, &k, &c, &q).await })
                            })
                            .collect();
                        // Addon order, first of each title; failed catalogs are skipped like the search page's.
                        let (mut seen, mut out) = (std::collections::HashSet::new(), Vec::new());
                        for t in tasks {
                            if let Ok(Ok(items)) = t.await {
                                out.extend(items.into_iter().filter(|m| seen.insert((m.kind.clone(), m.id.clone()))));
                            }
                        }
                        let _ = reply.send(serde_json::to_value(out).map_err(|e| (503, e.to_string())));
                    });
                    return;
                }
            }
            api::Call::Play { kind, id, name } => {
                let key = key::item(&kind, &id);
                let title = self.recent.iter().find(|p| p.key == key).map(|p| p.title.clone());
                // Torrents and files only resume: a path or magnet from the API alone is never opened.
                if title.is_none() && key::is_local(&kind) {
                    Err((404, "not in Continue Watching".into()))
                } else {
                    self.show_window(ctx, true);
                    self.leave_player(ctx);
                    // Saved progress keeps this name until the addon's meta renames it.
                    let name = title.or(name).unwrap_or_else(|| id.clone());
                    self.quick_play(ctx, MetaPreview { id, kind, name, ..Default::default() });
                    Ok(serde_json::json!({ "ok": true }))
                }
            }
        };
        let _ = reply.send(r);
    }

    /// Look up posters for Continue Watching entries saved without one (older history).
    /// Torrent/file keys never match an addon type, so they are skipped.
    fn fetch_recent_posters(&self, ctx: &egui::Context) {
        for p in self.recent.iter().filter(|p| p.poster.is_none()) {
            let Some((kind, id)) = key::addon_item(&p.key) else { continue };
            let addons = self.addons_for("meta", kind, id);
            if addons.is_empty() {
                continue;
            }
            let (client, tx, ctx) = (self.client.clone(), self.tx.clone(), ctx.clone());
            let (key, kind, id) = (p.key.clone(), kind.to_owned(), id.to_owned());
            // Not `spawn`: a missing poster is not worth an error.
            self.rt.spawn(async move {
                if let Ok(Meta { poster: Some(poster), .. }) = client.first_meta(&addons, &kind, &id, |m| m.poster.is_some()).await {
                    let _ = tx.send(Msg::RecentPoster(key, poster));
                    ctx.request_repaint();
                }
            });
        }
    }

}

impl eframe::App for App {
    /// Runs before each `ui`, and alone while the window is hidden, so messages, tray clicks and playback keep going.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(ctx, msg);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.tray_on_close && !self.quit && self.tray.as_ref().is_some_and(Tray::is_up) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_window(ctx, false);
        }
        // Also while hidden to the tray: `logic` runs on this wake-up.
        if self.notify_check.elapsed() >= NOTIFY_EVERY {
            self.notify_check = Instant::now();
            self.fetch_new_episodes(ctx);
        }
        // Wake for the next check; with a player (or sleep timer) running, the 500 ms tick below is sooner.
        ctx.request_repaint_after(NOTIFY_EVERY.saturating_sub(self.notify_check.elapsed()));
        self.poll_player(ctx);
        self.publish_media();
        // Hidden, `ui` doesn't run and mpv's frames aren't rendered, which stalls its core: play audio only meanwhile.
        // A minimized Wayland window is no different from one minimized by hand, so it keeps its video.
        if let Some(p) = &mut self.player
            && (self.hidden && !wayland()) != p.hidden_vid.is_some()
        {
            let v = p.hidden_vid.take().unwrap_or_else(|| {
                p.hidden_vid = Some(p.player.state.tracks.iter().find(|t| t.kind == "video" && t.selected).map_or("auto".into(), |t| t.id.to_string()));
                "no".into()
            });
            if let Err(e) = p.player.command(&["set", "vid", &v]) {
                report(&mut self.error, e);
            }
        }
        let away = self.hidden || ctx.input(|i| i.viewport().minimized) == Some(true);
        if let Some(p) = &mut self.player {
            let (pause, auto) = pause_step(self.pause_on_minimize, away, p.player.state.paused, p.auto_pause);
            p.auto_pause = auto;
            if let Some(pause) = pause
                && let Err(e) = p.player.command(&["set", "pause", if pause { "yes" } else { "no" }])
            {
                report(&mut self.error, e);
            }
        }
        if self.player.is_some() || self.sleep.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poster_frame = ctx.cumulative_frame_nr();
        // Card posters sized for a lower scale (zoom, or a new monitor) are refetched sharp as they are drawn.
        // From the zoom the UI will settle on (the clamp below lands next pass), floored at 1 and rounded up to half steps so a zoom sweep refetches a few times, not once per step.
        let want_ppp = ((ctx.pixels_per_point() / ctx.zoom_factor() * clamp_scale(ctx.zoom_factor())).max(1.0) * 2.0).ceil() / 2.0;
        if want_ppp > self.poster_ppp {
            self.poster_ppp = want_ppp;
        }
        // egui's built-in zoom keys change the factor too: bound it, then persist any change.
        let zoom = clamp_scale(ctx.zoom_factor());
        if zoom != ctx.zoom_factor() {
            ctx.set_zoom_factor(zoom);
        }
        if zoom != self.ui_scale {
            self.ui_scale = zoom;
            if let Err(e) = settings::UI_SCALE.set(&self.db, &zoom) {
                report(&mut self.error, format!("saving ui_scale: {e:#}"));
            }
        }
        ctx.input(|i| self.window.track(i.viewport()));
        // Load libmpv off the UI thread once the first frames are up: loading it earlier delayed them by ~140 ms.
        if ctx.cumulative_frame_nr() == 2 {
            std::thread::spawn(stremflix_rs::player::load_mpv);
        }
        // A dropped .torrent or media file, or a magnet pasted with no text field focused, plays at once.
        // Window systems deliver dropped files only, not dropped text, so magnets come in by paste.
        let (dropped, pasted) = ctx.input(|i| {
            let file = i.raw.dropped_files.iter().filter_map(|f| f.path().to_str().map(String::from)).find(|p| is_torrent_file(p) || is_media(p.as_ref()));
            (file, i.events.iter().find_map(|e| if let egui::Event::Paste(t) = e { Some(t.trim().to_string()) } else { None }))
        });
        let pasted = pasted.filter(|t| parse_magnet_hash(t).is_some() && ctx.memory(|m| m.focused().is_none()));
        match dropped.or(pasted) {
            Some(t) if is_torrent_source(&t) => self.play_torrent(&ctx, &t),
            // A media file opens like one given on the command line.
            Some(t) => self.handle(&ctx, Msg::Link(t)),
            None => {}
        }
        if self.player.is_some() {
            // The player has its own keys; don't carry a stale focus into the first page shown after it.
            self.had_focus = false;
            self.ui_player(ui, frame);
            return;
        }

        // Not a key repeat: holding Escape to leave the player must not also close the page under it. An open dropdown takes Escape for itself.
        let esc = |e: &egui::Event| matches!(e, egui::Event::Key { key: egui::Key::Escape, pressed: true, repeat: false, .. });
        let back = ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Extra1) || (!self.had_focus && i.events.iter().any(esc)));
        if back && !self.search_open && !egui::Popup::is_any_open(&ctx) {
            self.go_back();
        }
        // The search page lives on Home; any other tab (also set by links) closes it.
        if self.tab != Tab::Home {
            self.search_page = None;
        }
        self.leave_locked_tab();
        let full = ui.max_rect();
        // A page with a banner runs it under the floating header; the others start below it.
        let banner = self.tab == Tab::Home && (self.search_page.is_none() || self.selected.is_some()) && self.featured().is_some();
        egui::Frame::central_panel(ui.style()).inner_margin(0).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            let mut chosen = None;
            if let Some((_, title, _, _, files, sel)) = &mut self.file_pick {
                let mut open = true;
                egui::Window::new(format!("Choose a file: {}", sanitize_text(title, 80))).open(&mut open).collapsible(false).show(&ctx, |ui| {
                    // Scrolls: a season pack's file list would run past the window and take Play with it.
                    egui::ScrollArea::vertical().max_height((ui.ctx().content_rect().height() - 160.0).max(80.0)).show(ui, |ui| {
                        for (i, name, len) in files.iter() {
                            ui.radio_value(sel, *i, format!("{}  ({})", sanitize_text(name, 120), fmt_size(*len)));
                        }
                    });
                    if primary_button(ui, "Play").clicked() {
                        chosen = Some(*sel);
                    }
                });
                if !open {
                    self.clear_file_pick();
                }
            }
            if let (Some(idx), Some((key, title, source, subs, _, _))) = (chosen, self.file_pick.take()) {
                self.play(&ctx, key, title, format!("{source}&so={idx}"), subs);
            }
            // No shrinking: a page of narrow rows would pull the scroll bar in from the window edge.
            // Own offset per tab and for the detail and search pages, so one page's scroll never carries into another.
            let page = egui::ScrollArea::vertical().id_salt((self.tab.name(), self.tab == Tab::Home && self.selected.is_some(), self.search_page.is_some())).auto_shrink(false).show(ui, |ui| {
                egui::Frame::NONE.inner_margin(egui::Margin { left: PAGE_X, right: PAGE_X, top: 0, bottom: 32 }).show(ui, |ui| {
                    if !banner {
                        ui.add_space(HEADER_H + 8.0);
                    }
                    match self.tab {
                        Tab::Home => self.ui_home(ui),
                        Tab::Discover => self.ui_discover(ui),
                        Tab::Addons => self.ui_addons(ui),
                        Tab::Library => self.ui_library(ui),
                        Tab::Settings => self.ui_settings(ui),
                    }
                });
            });
            // After the page, so the header paints over it and takes the clicks.
            self.ui_header(ui, full, page.state.clamped_offset().y);
        });
        self.ui_toasts(&ctx);
        self.ui_search_palette(&ctx);
        self.had_focus = ctx.memory(|m| m.focused().is_some());
    }

    fn on_exit(&mut self, _gl: Option<&glow::Context>) {
        let _ = WINDOW.set(&self.db, &self.window);
        let _ = TAB.set(&self.db, &self.tab);
        self.stop_player();
        let _ = self.db.end_session();
    }
}

fn main() -> eframe::Result {
    // A launch without an argument just focuses the open window.
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let arg = match parse_cli(&args) {
        Cli::Open(a) => a,
        Cli::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        Cli::Bad(e) => {
            eprintln!("error: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let dir = data_dir();
    if instance::forward(&dir, arg.as_deref().unwrap_or("")) {
        return Ok(());
    }
    stremflix_rs::diag::init(&dir);
    let mut app = App::new(dir.clone()).expect("failed to initialise app data");
    let ctx_cell: Arc<std::sync::OnceLock<egui::Context>> = Default::default();
    let sink = {
        let (tx, cell) = (app.tx.clone(), ctx_cell.clone());
        move |link: String| {
            let _ = tx.send(Msg::Link(link));
            if let Some(c) = cell.get() {
                c.request_repaint();
            }
        }
    };
    if let Some(a) = arg {
        sink(a);
    }
    if let Err(e) = instance::listen(&dir, sink.clone()) {
        stremflix_rs::elog!("error: links from later launches won't reach this window: {e}");
    }
    let api_addr = std::env::var("STREMFLIX_RS_API").unwrap_or_else(|_| api::DEFAULT_ADDR.into());
    if api_addr != "off" {
        let (tx, cell) = (app.tx.clone(), ctx_cell.clone());
        let on_call = move |call| {
            let (reply, got) = channel();
            let _ = tx.send(Msg::Api(call, reply));
            if let Some(c) = cell.get() {
                c.request_repaint();
            }
            got.recv_timeout(api::WAIT).unwrap_or_else(|_| Err((503, "the app did not answer".into())))
        };
        match api::serve(&api_addr, on_call) {
            Ok(a) => stremflix_rs::elog!("local API on http://{a}/api/"),
            Err(e) => stremflix_rs::elog!("error: local API on {api_addr} is off: {e}"),
        }
    }
    #[cfg(windows)]
    std::thread::spawn(instance::register_scheme);
    app.window = WINDOW.get(&app.db).unwrap_or_default();
    let mut vp = egui::ViewportBuilder::default().with_inner_size(app.window.size).with_maximized(app.window.maximized);
    if let Some(p) = app.window.pos {
        vp = vp.with_position(p);
    }
    let opts = eframe::NativeOptions {
        viewport: vp
            .with_title("Stremflix-rs")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).expect("bundled icon is a valid PNG")),
        ..Default::default()
    };
    let creator: eframe::AppCreator = Box::new(move |cc| {
        install_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, app.light, ACCENTS[app.accent].1);
        cc.egui_ctx.set_zoom_factor(app.ui_scale);
        app.gl_proc = cc.get_proc_address.clone();
        app.media = media_controls(cc, app.tx.clone());
        app.build_rows(&cc.egui_ctx);
        app.fetch_recent_posters(&cc.egui_ctx);
        app.sync_account(&cc.egui_ctx);
        let client = app.client.clone();
        // A failed check (offline, rate limit) only logs: it shouldn't show as an error.
        app.spawn(&cc.egui_ctx, async move {
            stremflix_rs::update::cleanup();
            Ok(Msg::Update(stremflix_rs::update::check(&client).await.unwrap_or_else(|e| {
                stremflix_rs::elog!("update check: {e:#}");
                None
            })))
        });
        let (tx, ctx) = (app.tx.clone(), cc.egui_ctx.clone());
        app.tray = tray::spawn(&app.rt, include_bytes!("../assets/icon.png"), move |a| {
            let _ = tx.send(Msg::Tray(a));
            ctx.request_repaint();
        });
        // SIGTERM (logout, shutdown) and Ctrl+C quit like the tray's Quit, so the session ends cleanly and the window is saved.
        #[cfg(unix)]
        {
            let (tx, ctx) = (app.tx.clone(), cc.egui_ctx.clone());
            app.rt.spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
                let mut next = async || tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} };
                next().await;
                let _ = tx.send(Msg::Tray(tray::Action::Quit));
                ctx.request_repaint();
                // A second signal means the UI did not get to it.
                next().await;
                std::process::exit(1);
            });
        }
        let _ = ctx_cell.set(cc.egui_ctx.clone());
        Ok(Box::new(app))
    });
    #[cfg(not(target_os = "macos"))]
    return eframe::run_native("Stremflix-rs", opts, creator);
    // macOS sends links as Apple events, not arguments; the hook must exist before the loop runs.
    #[cfg(target_os = "macos")]
    {
        let event_loop = winit::event_loop::EventLoop::<eframe::UserEvent>::with_user_event().build()?;
        mac_links::install(Box::new(sink));
        let mut winit_app = eframe::create_native("Stremflix-rs", opts, creator, &event_loop);
        event_loop.run_app(&mut winit_app)?;
        // run_native exits the process here; don't wait on the runtime and torrent threads.
        std::process::exit(0)
    }
}

/// Adds `application:openURLs:` to winit's app delegate (winit 0.30 installs its own and exposes no URL events).
#[cfg(target_os = "macos")]
mod mac_links {
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{msg_send, sel};
    use std::ffi::{CStr, c_char};
    use std::sync::OnceLock;

    static SINK: OnceLock<Box<dyn Fn(String) + Send + Sync>> = OnceLock::new();

    type OpenUrls = extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, &AnyObject);

    extern "C-unwind" fn open_urls(_this: &AnyObject, _cmd: Sel, _app: &AnyObject, urls: &AnyObject) {
        let Some(sink) = SINK.get() else { return };
        // SAFETY: AppKit passes an NSArray<NSURL>; nil replies read as 0 / null.
        unsafe {
            let n: usize = msg_send![urls, count];
            for i in 0..n {
                let url: *mut AnyObject = msg_send![urls, objectAtIndex: i];
                let s: *mut AnyObject = msg_send![url, absoluteString];
                let p: *const c_char = msg_send![s, UTF8String];
                if !p.is_null() {
                    sink(CStr::from_ptr(p).to_string_lossy().into_owned());
                }
            }
        }
    }

    /// Call after the event loop is built (winit registers its delegate class then) and before it runs.
    pub fn install(sink: Box<dyn Fn(String) + Send + Sync>) {
        let _ = SINK.set(sink);
        let Some(cls) = AnyClass::get(c"WinitApplicationDelegate") else {
            return stremflix_rs::elog!("error: stremio:// links unavailable: winit's app delegate not found");
        };
        // SAFETY: the signature matches the "v@:@@" encoding of -application:openURLs:.
        let added = unsafe {
            let imp = std::mem::transmute::<OpenUrls, Imp>(open_urls);
            objc2::ffi::class_addMethod(std::ptr::from_ref(cls).cast_mut(), sel!(application:openURLs:), imp, c"v@:@@".as_ptr())
        };
        if !added.as_bool() {
            return stremflix_rs::elog!("error: stremio:// links unavailable: delegate already has application:openURLs:");
        }
        // AppKit can cache which selectors a delegate answers when it is set: set it again so the new one counts.
        // SAFETY: plain messages to the shared NSApplication on the main thread.
        unsafe {
            let app: *mut AnyObject = msg_send![objc2::class!(NSApplication), sharedApplication];
            let delegate: *mut AnyObject = msg_send![app, delegate];
            let () = msg_send![app, setDelegate: std::ptr::null_mut::<AnyObject>()];
            let () = msg_send![app, setDelegate: delegate];
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_addon_labels() {
        assert_eq!((fmt_size(734_003_200), fmt_size(2_345_000_000)), ("734 MB".to_string(), "2.35 GB".to_string()));
    }

    #[test]
    fn tab_name_round_trip() {
        for t in [Tab::Home, Tab::Discover, Tab::Addons, Tab::Library, Tab::Settings] {
            assert!(Tab::from_name(t.name()) == Some(t));
        }
        let (_d, app) = test_app();
        assert!(TAB.get(&app.db).unwrap() == Tab::Home);
        app.db.set_setting(TAB.key, "bogus").unwrap();
        assert!(TAB.get(&app.db).unwrap() == Tab::Home);
    }

    #[test]
    fn health_error_classifies_by_type() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (refused, t) = rt.block_on(async {
            let c = reqwest::Client::new();
            // A listener that never answers makes the deadline fire, not a refusal.
            let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/", silent.local_addr().unwrap());
            (c.get("http://127.0.0.1:1/").send().await.unwrap_err(), c.get(url).timeout(std::time::Duration::from_millis(50)).send().await.unwrap_err())
        });
        // Port 1 on localhost refuses at once: a transport error that is not a timeout.
        assert_eq!(health_error(&anyhow::Error::new(refused)).split(':').next(), Some("Unreachable"));
        assert!(health_error(&anyhow::anyhow!("HTTP 404 Not Found")).starts_with("Error: HTTP 404"));
        assert!(t.is_timeout());
        assert_eq!(health_error(&anyhow::Error::new(t).context("GET x")), "Timed out");
    }

    fn test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().into()).unwrap();
        (dir, app)
    }

    /// One frame of the header and search palette with `events` as input.
    fn header_frame(app: &mut App, ctx: &egui::Context, events: Vec<egui::Event>) {
        let input = egui::RawInput { events, screen_rect: Some(egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 700.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            let full = ui.max_rect();
            app.ui_header(ui, full, 0.0);
            app.ui_search_palette(ui.ctx());
        });
        out.textures_delta.clear();
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    #[test]
    fn slash_opens_an_empty_search_palette_and_escape_closes_it() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        app.search = "old query".into();
        header_frame(&mut app, &ctx, vec![key(egui::Key::Slash)]);
        assert!(app.search_open);
        assert!(app.search.is_empty(), "opens empty so recent searches show");
        header_frame(&mut app, &ctx, vec![]);
        header_frame(&mut app, &ctx, vec![key(egui::Key::Escape)]);
        assert!(!app.search_open);
    }

    #[test]
    fn enter_in_the_search_palette_runs_only_a_real_query() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        header_frame(&mut app, &ctx, vec![key(egui::Key::Slash)]);
        header_frame(&mut app, &ctx, vec![]); // the field takes focus
        header_frame(&mut app, &ctx, vec![key(egui::Key::Enter)]);
        assert!(app.search_open, "blank query: nothing to run, palette stays");
        header_frame(&mut app, &ctx, vec![egui::Event::Text("pattern".into())]);
        header_frame(&mut app, &ctx, vec![key(egui::Key::Enter)]);
        assert!(!app.search_open);
        assert_eq!(app.search_history.first().map(String::as_str), Some("pattern"));
    }

    #[test]
    fn locked_profile_leaves_the_addons_page() {
        let (_dir, mut app) = test_app();
        app.tab = Tab::Addons;
        app.leave_locked_tab();
        assert!(app.tab == Tab::Addons, "unlocked: stays");
        app.child_pin = "1234".into();
        app.leave_locked_tab();
        assert!(app.tab == Tab::Home);
    }

    #[test]
    fn library_controls_have_accessible_names() {
        use egui::accesskit::Role;
        let (_dir, mut app) = test_app();
        app.lib_query = "x".into(); // shows the clear-filter button
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut out = ctx.run_ui(Default::default(), |ui| app.ui_library(ui));
        out.textures_delta.clear();
        let nodes = out.platform_output.accesskit_update.unwrap().nodes;
        let named = |n: &egui::accesskit::Node| !n.labelled_by().is_empty() || n.label().is_some_and(|l| l.chars().any(char::is_alphanumeric));
        let unnamed: Vec<_> = nodes.iter().filter(|(_, n)| matches!(n.role(), Role::Button | Role::TextInput) && !named(n)).map(|(_, n)| (n.role(), n.label().map(str::to_owned))).collect();
        assert!(unnamed.is_empty(), "unnamed controls: {unnamed:?}");
        assert!(nodes.iter().any(|(_, n)| n.label() == Some("Clear filter")));
    }

    #[test]
    fn resuming_an_episode_selects_it_so_the_next_can_autoplay() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let prog = |e: u32| Progress { key: format!("series:tt1:1:{e}"), title: format!("E{e}"), source: format!("https://x.example/{e}"), position: 5.0, duration: 60.0, poster: None };
        // E3 was watched last; resuming the older E1 must still play E1.
        app.recent = vec![prog(3), prog(1)];
        app.resume(&ctx, prog(1));
        let series = stremflix_rs::addon::parse_meta(br#"{"meta":{"id":"tt1","type":"series","name":"S","videos":[{"id":"tt1:1:1","season":1,"episode":1},{"id":"tt1:1:2","season":1,"episode":2},{"id":"tt1:1:3","season":1,"episode":3}]}}"#).unwrap();
        let meta = MetaPreview { id: "tt1".into(), kind: "series".into(), name: "S".into(), ..Default::default() };
        app.handle(&ctx, Msg::Episodes(meta, series, app.nav));
        assert_eq!(app.episode.as_ref().map(|v| v.id.as_str()), Some("tt1:1:1"));
        assert_eq!(app.play_key().map(|(k, _)| k).as_deref(), Some("series:tt1:1:1"));
        assert_eq!(next_episode(&app.episodes, "tt1:1:1").map(|v| v.id.as_str()), Some("tt1:1:2"));
        // Its saved stream plays (the meta request's own reply may be queued too).
        assert!(app.rx.try_iter().any(|m| matches!(m, Msg::Play(k, _, src, ..) if k == "series:tt1:1:1" && src == "https://x.example/1")));
    }

    #[test]
    fn resume_plays_its_stream_when_no_addon_lists_the_series() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let p = Progress { key: "series:tt1:1:1".into(), title: "E1".into(), source: "https://x.example/1".into(), position: 5.0, duration: 60.0, poster: None };
        // Through the API's play, like the Continue Watching play button.
        app.recent = vec![p.clone()];
        let (tx, _rx) = channel();
        app.handle(&ctx, Msg::Api(api::Call::Play { kind: "series".into(), id: "tt1:1:1".into(), name: None }, tx));
        assert!(matches!(&app.pending, Some((id, OnEpisodes::Resume(r))) if id == "tt1" && *r == p));
        app.handle(&ctx, Msg::NoEpisodes("tt1".into(), "no episodes for E1".into(), app.nav));
        assert!(app.pending.is_none() && app.error.is_none(), "{:?}", app.error);
        assert!(app.rx.try_iter().any(|m| matches!(m, Msg::Play(k, ..) if k == "series:tt1:1:1")));
        // A stale failure leaves the current request alone; opening a series page that fails still reports the error.
        app.pending = Some(("tt3".into(), OnEpisodes::Next));
        app.handle(&ctx, Msg::NoEpisodes("tt3".into(), "no episodes for old".into(), app.nav - 1));
        app.handle(&ctx, Msg::NoEpisodes("tt2".into(), "no episodes for S".into(), app.nav));
        assert!(matches!(&app.pending, Some((id, OnEpisodes::Next)) if id == "tt3"));
        assert!(app.error.is_some());
    }

    #[test]
    fn stale_replies_are_dropped() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let m = |id: &str| MetaPreview { id: id.into(), kind: "movie".into(), name: id.into(), ..Default::default() };
        app.nav = 2;
        app.handle(&ctx, Msg::Streams(m("a"), None, vec![], vec![], Autoplay::Off, 1));
        assert!(app.selected.is_none(), "reply for an older navigation");
        let series = stremflix_rs::addon::parse_meta(br#"{"meta":{"id":"s","type":"series","name":"S","videos":[{"id":"s:1:1","season":1,"episode":1}]}}"#).unwrap();
        app.handle(&ctx, Msg::Episodes(MetaPreview { kind: "series".into(), ..m("s") }, series, 1));
        assert!(app.selected.is_none() && app.episodes.is_empty());
        app.handle(&ctx, Msg::Play("movie:a".into(), "a".into(), "https://x.example/a".into(), "https://x.example/a".into(), vec![], None, None, 1));
        assert!(app.player.is_none());
        app.handle(&ctx, Msg::Streams(m("b"), None, vec![], vec![], Autoplay::Off, 2));
        assert_eq!(app.selected.as_ref().map(|(m, ..)| m.id.as_str()), Some("b"), "current reply applies");
    }

    #[test]
    fn a_rescan_reply_for_a_removed_folder_is_dropped() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let item = LibraryItem { path: "/a/1.mp4".into(), title: "1".into() };
        app.handle(&ctx, Msg::Scanned("/a".into(), true, Ok(vec![item.clone()])));
        assert!(app.library.is_empty() && app.library_folders.is_empty(), "folder was removed before the reply");
        app.handle(&ctx, Msg::Scanned("/a".into(), false, Ok(vec![item])));
        assert_eq!(app.library_folders, ["/a"], "adding a folder still works");
    }

    #[test]
    fn unaired_episodes_say_when_they_air() {
        assert_eq!(episode_date(Some("2026-11-20T00:00:00.000Z"), "2026-11-13"), "Airs 2026-11-20");
        assert_eq!(episode_date(Some("2026-11-13T23:00:00.000Z"), "2026-11-13"), "2026-11-13", "today has aired");
        assert_eq!(episode_date(Some("2019-01-02"), "2026-11-13"), "2019-01-02");
        assert_eq!(episode_date(None, "2026-11-13"), "");
    }

    #[test]
    fn a_title_is_loading_until_its_reply_or_another_navigation() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let m = MetaPreview { id: "a".into(), kind: "movie".into(), name: "A".into(), ..Default::default() };
        app.open_streams(&ctx, m.clone(), None, Autoplay::Off);
        assert_eq!(app.loading(), Some("A"));
        app.handle(&ctx, Msg::Streams(m.clone(), None, vec![], vec![], Autoplay::Off, app.nav));
        assert_eq!(app.loading(), None);
        app.open_streams(&ctx, m.clone(), None, Autoplay::Off);
        app.close_details();
        assert_eq!(app.loading(), None, "Back left the request behind");
        app.open_streams(&ctx, m, None, Autoplay::Off);
        app.handle(&ctx, Msg::Error("boom".into()));
        assert_eq!(app.loading(), None);
    }

    #[test]
    fn home_on_home_leaves_the_title_page() {
        let (_dir, mut app) = test_app();
        let m = MetaPreview { id: "a".into(), kind: "movie".into(), name: "a".into(), ..Default::default() };
        app.selected = Some((m, vec![], vec![]));
        app.tab = Tab::Library;
        app.go_tab(Tab::Home);
        assert!(app.selected.is_some(), "coming back from another tab returns to the title");
        app.go_tab(Tab::Home);
        assert!(app.selected.is_none());
    }

    #[test]
    fn back_closes_the_title_then_the_search_page() {
        let (_dir, mut app) = test_app();
        let m = MetaPreview { id: "a".into(), kind: "movie".into(), name: "a".into(), ..Default::default() };
        app.tab = Tab::Home;
        app.selected = Some((m, vec![], vec![]));
        app.search_page = Some(SearchPage { query: "q".into(), rows: vec![], failed: vec![] });
        app.go_back();
        assert!(app.selected.is_none() && app.search_page.is_some());
        app.go_back();
        assert!(app.search_page.is_none());
    }

    #[test]
    fn resuming_keeps_the_saved_binge_group() {
        let (_dir, mut app) = test_app();
        let m = MetaPreview { id: "s".into(), kind: "series".into(), name: "S".into(), ..Default::default() };
        let v: Video = serde_json::from_str(r#"{"id":"s:1:1","season":1,"episode":1}"#).unwrap();
        let hd = Stream { url: Some("https://x.example/a".into()), hints: stremflix_rs::addon::BehaviorHints { binge_group: Some("hd".into()), ..Default::default() }, ..Default::default() };
        let plain = Stream { url: Some("https://x.example/b".into()), ..Default::default() };
        app.episode = Some(v);
        app.selected = Some((m.clone(), vec![hd, plain], vec![]));
        app.remember_episode_stream("series:s:1:1", "https://x.example/a");
        assert_eq!(app.saved_binge(&m).as_deref(), Some("hd"));
        app.selected = Some((m.clone(), vec![], vec![]));
        app.remember_episode_stream("series:s:1:1", "https://x.example/a");
        assert_eq!(app.saved_binge(&m).as_deref(), Some("hd"), "resume has no stream list");
        app.selected.as_mut().unwrap().1 = vec![Stream { url: Some("https://x.example/b".into()), ..Default::default() }];
        app.remember_episode_stream("series:s:1:1", "https://x.example/b");
        assert_eq!(app.saved_binge(&m), None, "a known stream without a group clears it");
    }

    #[test]
    fn sleep_timer_rides_the_autoplay_chain_only() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let m = MetaPreview { id: "a".into(), kind: "movie".into(), name: "a".into(), ..Default::default() };
        let one = Stream { url: Some("https://x.example/a".into()), ..Default::default() };
        let sleep = (Instant::now() + Duration::from_secs(600), 30);
        let played = |app: &mut App| app.rx.try_iter().find_map(|m| if let Msg::Play(.., s, _) = m { Some(s) } else { None });
        // The next episode's play request carries the timer; a stream picked any other way doesn't.
        app.nav = 3;
        app.handle(&ctx, Msg::Streams(m.clone(), None, vec![one.clone()], vec![], Autoplay::Next(None, Some(sleep)), 3));
        assert_eq!(played(&mut app), Some(Some(sleep)));
        let nav = app.nav;
        app.handle(&ctx, Msg::Streams(m.clone(), None, vec![one.clone()], vec![], Autoplay::Only, nav));
        assert_eq!(played(&mut app), Some(None));
        app.play(&ctx, "movie:a".into(), "a".into(), "https://x.example/a".into(), vec![]);
        assert_eq!(played(&mut app), Some(None));
        // No stream to autoplay: the picker shows and nothing carries the timer.
        let nav = app.nav;
        app.handle(&ctx, Msg::Streams(m, None, vec![], vec![], Autoplay::Next(None, Some(sleep)), nav));
        assert!(app.show_streams && played(&mut app).is_none());
    }

    #[test]
    fn preload_keeps_only_the_awaited_episode() {
        assert!(!preload_due(10.0, 50.0), "short or still-loading stream");
        assert!(!preload_due(1000.0, 1200.0));
        assert!(preload_due(1080.0, 1200.0));
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let one = Stream { url: Some("https://x.example/e2".into()), ..Default::default() };
        app.handle(&ctx, Msg::Preloaded("e2".into(), Some((vec![one.clone()], vec![]))));
        assert!(app.preload.is_none(), "nothing awaited: dropped");
        app.preload = Some(("e2".into(), None));
        app.handle(&ctx, Msg::Preloaded("e3".into(), Some((vec![one.clone()], vec![]))));
        assert!(app.preload.as_ref().is_some_and(|(id, f)| id == "e2" && f.is_none()), "another episode's reply is ignored");
        app.handle(&ctx, Msg::Preloaded("e2".into(), Some((vec![one], vec![]))));
        assert!(app.preload.as_ref().is_some_and(|(_, f)| f.as_ref().is_some_and(|(s, _)| s.len() == 1)));
        app.preload = Some(("e2".into(), None));
        app.handle(&ctx, Msg::Preloaded("e2".into(), None));
        assert!(app.preload.as_ref().is_some_and(|(_, f)| f.is_none()), "a failure stays marked, so it isn't retried");
    }

    #[test]
    fn timer_pause_blocks_autoplay_until_resumed() {
        use AutoPause::*;
        assert!(autoplay_due(true, None, Off, true));
        assert!(!autoplay_due(true, Some(false), Off, true), "pause not yet confirmed");
        assert!(!autoplay_due(true, Some(true), Off, true));
        assert!(!autoplay_due(false, None, Off, true));
        assert!(!autoplay_due(true, None, Held, true), "our minimize pause holds");
        assert!(autoplay_due(true, None, Yielded, true), "user took over while minimized");
        let s = timer_pause_step(Some(false), false);
        assert_eq!(s, Some(false), "mpv has not reported the pause yet");
        let s = timer_pause_step(s, true);
        assert_eq!(s, Some(true));
        assert_eq!(timer_pause_step(s, false), None, "user resumed");
    }

    #[test]
    fn search_replies_fill_their_rows() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let src = |id: &str| ("https://a.example/manifest.json".to_string(), "movie".to_string(), id.to_string());
        let row = |id: &str| Row { title: id.into(), src: src(id), page: Paged::default(), error: None };
        let hit = MetaPreview { id: "x".into(), kind: "movie".into(), name: "x".into(), ..Default::default() };
        app.search_page = Some(SearchPage { query: "new".into(), rows: vec![row("a"), row("b")], failed: vec![] });
        app.handle(&ctx, Msg::Search("old".into(), src("a"), Ok(vec![hit.clone()])));
        assert!(app.search_page.as_ref().unwrap().rows[0].page.items.is_none(), "reply for a replaced search");
        app.handle(&ctx, Msg::Search("new".into(), src("a"), Ok(vec![hit])));
        app.handle(&ctx, Msg::Search("new".into(), src("b"), Err("boom".into())));
        let page = app.search_page.as_ref().unwrap();
        assert_eq!(page.rows[0].page.items.as_ref().map(Vec::len), Some(1));
        assert!(page.rows[1].page.items.as_ref().is_some_and(Vec::is_empty) && app.error.is_none(), "one failed catalog is not an error");
        app.search_page = Some(SearchPage { query: "new".into(), rows: vec![row("a")], failed: vec![] });
        app.handle(&ctx, Msg::Search("new".into(), src("a"), Err("boom".into())));
        assert!(app.error.as_deref().is_some_and(|e| e.contains("boom")), "every catalog failed");
    }

    #[test]
    fn failed_home_row_keeps_its_error_until_a_reply_succeeds() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let src = ("https://a.example/manifest.json".to_string(), "movie".to_string(), "c".to_string());
        app.rows = vec![Row { title: "c".into(), src: src.clone(), page: Paged::default(), error: None }];
        app.handle(&ctx, Msg::Row(src.clone(), Err("boom".into())));
        assert_eq!(app.rows[0].error.as_deref(), Some("boom"));
        assert!(app.error.is_none(), "one failed row is not a page-wide error");
        let hit = MetaPreview { id: "x".into(), kind: "movie".into(), name: "x".into(), ..Default::default() };
        app.handle(&ctx, Msg::Row(src, Ok(vec![hit])));
        assert!(app.rows[0].error.is_none() && app.rows[0].page.fetched == 1);
    }

    #[test]
    fn discover_pages_append_until_one_adds_nothing() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let m = |id: &str| MetaPreview { id: id.into(), kind: "movie".into(), name: id.into(), ..Default::default() };
        let src: DiscoverSrc = ("https://a.example/manifest.json".into(), "movie".into(), "c".into(), vec![]);
        app.discover.src = Some(src.clone());
        app.handle(&ctx, Msg::Discover(src.clone(), Ok(vec![m("a"), m("b")])));
        app.handle(&ctx, Msg::DiscoverMore(src.clone(), 2, Ok(vec![m("c")])));
        assert_eq!(app.discover.page.items.as_ref().map(Vec::len), Some(2), "no page was asked for");
        app.discover.page.loading_more = true;
        app.handle(&ctx, Msg::DiscoverMore(src.clone(), 2, Ok(vec![m("b"), m("c")])));
        assert_eq!(app.discover.page.items.as_ref().map(Vec::len), Some(3), "the repeated title is skipped");
        assert_eq!((app.discover.page.fetched, app.discover.page.loading_more, app.discover.page.exhausted), (4, false, false));
        app.discover.page.loading_more = true;
        app.handle(&ctx, Msg::DiscoverMore(src, 4, Ok(vec![m("c")])));
        assert!(app.discover.page.exhausted);
    }

    #[test]
    fn addon_catalog_replies() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let src = |id: &str| ("https://a.example/manifest.json".to_string(), "all".to_string(), id.to_string());
        app.browse = Some((src("community"), None));
        app.handle(&ctx, Msg::AddonCatalog(src("official"), Ok(vec![])));
        assert_eq!(app.browse, Some((src("community"), None)), "reply for another catalog");
        app.handle(&ctx, Msg::AddonCatalog(src("community"), Ok(vec![])));
        assert_eq!(app.browse, Some((src("community"), Some(vec![]))));
        app.browse = Some((src("community"), None));
        app.handle(&ctx, Msg::AddonCatalog(src("community"), Err("boom".into())));
        assert!(app.browse.is_none() && app.error.is_some(), "a failed load deselects so it can be retried");
    }

    #[test]
    fn rewatch_past_90_percent_leaves_continue_watching() {
        let (_dir, mut app) = test_app();
        app.set_watched("movie:tt1", true);
        let p = |position| Progress { key: "movie:tt1".into(), title: "T".into(), source: "https://x.example/v".into(), position, duration: 100.0, poster: None };
        app.record_progress(p(50.0));
        assert_eq!(app.recent.len(), 1, "a rewatch keeps a resume point");
        app.record_progress(p(95.0));
        assert!(app.recent.is_empty() && app.db.progress("movie:tt1").unwrap().is_none());
    }


    #[test]
    fn version_compare() {
        use stremflix_rs::addon::is_newer;
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("2", "1.9"), "short addon versions are padded");
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(is_newer("1.1", "1.0-beta"));
        assert!(!is_newer("1.2.3.4", "1.0.0"), "four parts isn't semver");
        use stremflix_rs::addon::is_newer_addon;
        assert!(is_newer_addon("1.0.0.1", "1.0.0") && is_newer_addon("2024.02.01", "2024.01.15"), "non-semver addon versions compare numerically");
        assert!(is_newer_addon("1.0.0", "1.0.0-beta") && !is_newer_addon("1.0.0.0", "1.0") && !is_newer_addon("x", "1.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
        assert!(is_newer("v1.1.0", "1.0.0"));
        assert!(!is_newer("1.0.0-beta", "1.0.0"), "a pre-release is older than its release");
        assert!(is_newer("1.1.0-beta", "1.0.0"));
        assert!(is_newer("v1.0.0", "v1.0.0-beta"), "the final release is newer than its beta");
        assert!(is_newer("1.0.0-beta.2", "1.0.0-beta.1"));
        assert!(is_newer("1.0.0-beta.10", "1.0.0-beta.2"), "numeric pre-release parts compare as numbers");
        assert!(is_newer("1.0.0-rc.1", "1.0.0-beta.9"));
        assert!(!is_newer("1.0.0-beta.1", "1.0.0-beta.2"));
        assert!(is_newer("1.0.1+build5", "1.0.0"));
        assert!(!is_newer("1.0.0-beta", "1.0.0-beta"));
        assert!(!is_newer("nightly", "1.0.0"), "non-numeric is never offered");
        assert!(!is_newer("1.0.0", "nightly"));
    }

    #[test]
    fn update_keeps_addon_id() {
        let m = |id: &str| stremflix_rs::addon::parse_manifest(format!(r#"{{"id":"{id}","version":"1","name":"A","resources":["stream"],"types":["movie"]}}"#).as_bytes()).unwrap();
        assert!(same_addon(&m("a"), &m("a")));
        assert!(!same_addon(&m("a"), &m("b")));
    }

    #[test]
    fn reinstall_keeps_addon_priority() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let m = stremflix_rs::addon::parse_manifest(br#"{"id":"a","version":"1","name":"A","resources":["stream"],"types":["movie"]}"#).unwrap();
        let u = |n: &str| format!("https://{n}.example/manifest.json");
        for n in ["a", "b", "c"] {
            app.handle(&ctx, Msg::Installed(u(n), m.clone(), None));
        }
        let order = |app: &App| app.addons.iter().map(|(x, _)| x.clone()).collect::<Vec<_>>();
        app.handle(&ctx, Msg::Installed(u("a"), m.clone(), None));
        assert_eq!(order(&app), [u("a"), u("b"), u("c")], "update stays in place");
        app.handle(&ctx, Msg::Installed(u("b2"), m.clone(), Some(u("b"))));
        assert_eq!(order(&app), [u("a"), u("b2"), u("c")], "reconfigured addon takes the old place");
        assert_eq!(app.db.addons().unwrap().into_iter().map(|(x, _)| x).collect::<Vec<_>>(), order(&app), "saved");
    }

    #[test]
    fn child_safe_profile_hides_adult_addons_and_refuses_new_ones() {
        let dir = tempfile::tempdir().unwrap();
        profile::switch(dir.path(), "Kids").unwrap();
        let mut app = App::new(dir.path().into()).unwrap();
        assert_eq!(app.data_dir, dir.path().join("profiles").join("Kids"));
        let ctx = egui::Context::default();
        let m = |hints: &str| stremflix_rs::addon::parse_manifest(format!(r#"{{"id":"a","version":"1","name":"A","resources":["stream"],"types":["movie"]{hints}}}"#).as_bytes()).unwrap();
        let u = |n: &str| format!("https://{n}.example/manifest.json");
        app.handle(&ctx, Msg::Installed(u("ok"), m(""), None));
        app.handle(&ctx, Msg::Installed(u("adult"), m(r#","behaviorHints":{"adult":true}"#), None));
        let urls = |app: &App| app.addons.iter().map(|(x, _)| x.clone()).collect::<Vec<_>>();
        let nav = app.nav;
        app.selected = Some((serde_json::from_str(r#"{"id":"tt1","type":"movie","name":"Open"}"#).unwrap(), vec![], vec![]));
        app.set_child_pin(&ctx, "1234".into());
        assert_eq!(urls(&app), [u("ok")]);
        assert!(app.selected.is_none() && app.nav > nav, "an open title closes and in-flight replies are dropped");
        app.handle(&ctx, Msg::Installed(u("new"), m(""), None));
        app.handle(&ctx, Msg::Imported(Import { addons: vec![(u("imported"), m(""))], progress: vec![], watched: vec![], bitfields: vec![], saved: vec![] }));
        assert_eq!((urls(&app), app.db.addons().unwrap().len()), (vec![u("ok")], 2), "nothing installed while locked");
        // The lock is the profile's own and survives a restart; the default profile has neither it nor the addons.
        drop(app);
        let mut app = App::new(dir.path().into()).unwrap();
        assert!(app.locked() && urls(&app) == [u("ok")]);
        app.set_child_pin(&ctx, String::new());
        assert_eq!(urls(&app), [u("ok"), u("adult")]);
        drop(app);
        profile::switch(dir.path(), "").unwrap();
        let app = App::new(dir.path().into()).unwrap();
        assert!(!app.locked() && app.addons.is_empty());
    }

    #[test]
    fn deep_link_title_gets_its_name_in_either_order() {
        let ctx = egui::Context::default();
        let meta = |id: &str| stremflix_rs::addon::parse_meta(format!(r#"{{"meta":{{"id":"{id}","type":"movie","name":"Name {id}","description":"About {id}"}}}}"#).as_bytes()).unwrap();
        let name = |app: &App| app.selected.as_ref().map(|(m, ..)| m.name.clone());
        let preview = |id: &str| MetaPreview { id: id.into(), kind: "movie".into(), name: id.into(), ..Default::default() };
        // Details first, then streams.
        let (_d, mut app) = test_app();
        app.nav = 1;
        app.details_want = ("movie".into(), "tt1".into());
        app.handle(&ctx, Msg::Details(meta("tt1")));
        app.handle(&ctx, Msg::Streams(preview("tt1"), None, vec![], vec![], Autoplay::Off, 1));
        assert_eq!(name(&app).as_deref(), Some("Name tt1"));
        // Streams first, then details; details of another title never rename the selection.
        let (_d, mut app) = test_app();
        app.nav = 1;
        app.handle(&ctx, Msg::Streams(preview("tt1"), None, vec![], vec![], Autoplay::Off, 1));
        app.details_want = ("movie".into(), "tt2".into());
        app.handle(&ctx, Msg::Details(meta("tt2")));
        assert_eq!(name(&app).as_deref(), Some("tt1"));
        app.details_want = ("movie".into(), "tt1".into());
        app.handle(&ctx, Msg::Details(meta("tt1")));
        assert_eq!(name(&app).as_deref(), Some("Name tt1"));
        assert_eq!(app.selected.as_ref().unwrap().0.description.as_deref(), Some("About tt1"));
    }

    #[test]
    fn link_message_prefills_addon_url() {
        let (_d, mut app) = test_app();
        app.handle(&egui::Context::default(), Msg::Link("stremio://example.com/manifest.json".into()));
        assert!(app.tab == Tab::Addons && app.addon_url == "https://example.com/manifest.json");
    }

    #[test]
    fn api_calls() {
        let (_d, mut app) = test_app();
        let ctx = egui::Context::default();
        let call = |app: &mut App, c| {
            let (tx, rx) = channel();
            app.handle(&ctx, Msg::Api(c, tx));
            rx.try_recv().unwrap()
        };
        app.recent = vec![Progress { key: "series:tt1:1:2".into(), title: "Ep".into(), source: "http://x/v".into(), position: 5.0, duration: 60.0, poster: Some("http://x/p.jpg".into()) }];
        let list = call(&mut app, api::Call::Continue).unwrap();
        assert_eq!(list, serde_json::json!([{"type": "series", "id": "tt1:1:2", "title": "Ep", "poster": "http://x/p.jpg", "position": 5.0, "duration": 60.0}]));
        assert_eq!(call(&mut app, api::Call::Search("x".into())).unwrap_err().0, 400);
        // A file or magnet the app never played is not opened.
        assert_eq!(call(&mut app, api::Call::Play { kind: "file".into(), id: "/etc/passwd".into(), name: None }).unwrap_err().0, 404);
        // A movie not in progress opens its streams to autoplay the top one.
        assert!(call(&mut app, api::Call::Play { kind: "movie".into(), id: "tt9".into(), name: Some("Nine".into()) }).is_ok());
        assert_eq!(app.loading(), Some("Nine"));
        // A file in Continue Watching resumes.
        app.recent.push(Progress { key: "file:/v/a.mkv".into(), title: "a".into(), source: "/v/a.mkv".into(), position: 0.0, duration: 0.0, poster: None });
        assert!(call(&mut app, api::Call::Play { kind: "file".into(), id: "/v/a.mkv".into(), name: None }).is_ok());
        // Search merges every searchable catalog in addon order, one entry per title.
        let base = test_addon::spawn(test_addon::Config { media_dir: None, torrent: None }).unwrap();
        let m = app.rt.block_on(app.client.fetch_manifest(&format!("{base}/manifest.json"))).unwrap();
        app.addons = vec![(format!("{base}/manifest.json"), m.clone()), (format!("{base}/manifest.json?again"), m)];
        let (tx, rx) = channel();
        app.handle(&ctx, Msg::Api(api::Call::Search("test".into()), tx));
        let found = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(found.as_array().map(|a| a.iter().map(|m| m["id"].as_str()).collect::<Vec<_>>()), Some(vec![Some(test_addon::META_ID)]));
    }

}
