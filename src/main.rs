//! Stremflix-rs desktop GUI (egui/eframe). Network and torrent work runs on a tokio
//! runtime; results come back to the UI thread over a channel.

use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use egui::{Align, Color32, Layout, RichText, Sense, UiBuilder, pos2, vec2};
use anyhow::Context;
use stremflix_rs::addon::{AddonClient, Skip, SkipKind, AddonEntry, CatalogDef, DeepLink, Manifest, Meta, MetaPreview, PlayTarget, Stream, Video, parse_magnet_hash, sanitize_text};
use stremflix_rs::instance;
use stremflix_rs::db::{self, Db, Download, List, Progress, SavedTitle, Watched};
use stremflix_rs::download::{self, State};
use stremflix_rs::library::{LibraryItem, scan};
use eframe::glow::{self, HasContext};
use stremflix_rs::player::{self, chapter_skips, GlProc, Player, ASPECTS, SPEEDS, SUB_COLORS, SUB_OUTLINES, ZOOM_RANGE, SubStyle, hwdec_value, prefer_lang, valid_langs, zoom_value};
use stremflix_rs::stremio_import::{self, Import};
use stremflix_rs::torrent::{NetStats, Profile, TorrentEngine, fmt_rate};
use stremflix_rs::tray::{self, Tray};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Home,
    Discover,
    Addons,
    Library,
    Settings,
}

/// Speed limit choices in bytes/s; 0 is unlimited.
const SPEED_LIMITS: [(&str, u32); 6] = [("Unlimited", 0), ("500 KB/s", 500_000), ("1 MB/s", 1_000_000), ("2 MB/s", 2_000_000), ("5 MB/s", 5_000_000), ("10 MB/s", 10_000_000)];

/// Torrent cache size choices: label and bytes kept (0 = delete a torrent when its playback stops).
const CACHE_SIZES: [(&str, u64); 5] = [("No cache", 0), ("2 GB", 2_000_000_000), ("5 GB", 5_000_000_000), ("10 GB", 10_000_000_000), ("Unlimited", u64::MAX)];

/// A local `.torrent` file path (not a URL or magnet).
fn is_torrent_file(s: &str) -> bool {
    !s.contains("://") && !s.starts_with("magnet:") && std::path::Path::new(s).extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
}

/// Splits a command line on whitespace; double quotes group words with spaces (e.g. a Windows path).
fn command_words(s: &str) -> Vec<String> {
    let (mut words, mut cur, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in s.chars() {
        match c {
            '"' => (quoted, any) = (!quoted, true),
            c if c.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => (cur.push(c), any = true).1,
        }
    }
    if any {
        words.push(cur);
    }
    words
}

/// Start `command` (program and its arguments) with `url` as the last argument. No shell is involved.
/// A command that is an existing file is the program alone, so an unquoted path with spaces works.
fn open_external(command: &str, url: &str) -> anyhow::Result<()> {
    let command = command.trim();
    let words = if std::path::Path::new(command).is_file() { vec![command.to_string()] } else { command_words(command) };
    let mut words = words.iter();
    let prog = words.next().context("set an external player in Settings")?;
    let mut child = std::process::Command::new(prog).args(words).arg(url).spawn().with_context(|| format!("start {prog}"))?;
    // Reap it when it exits, so it never lingers as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Addon subtitles as (url, lang).
type Subs = Vec<(String, String)>;
/// Media file `(index, name, size)` of a torrent.
type MediaFile = (usize, String, u64);

/// A subtitle an addon offers for the playing item, listed in the player's search panel.
#[derive(Clone)]
struct SubFound {
    addon: String,
    lang: String,
    url: String,
}

/// State of the player's subtitle search panel.
#[derive(Default)]
struct SubSearch {
    open: bool,
    busy: bool,
    filter: String,
    results: Vec<SubFound>,
    status: String,
}

/// Results whose language or addon name contains `q` (case-insensitive, blank matches all).
fn filter_subs<'a>(found: &'a [SubFound], q: &str) -> Vec<&'a SubFound> {
    let q = q.trim().to_lowercase();
    found.iter().filter(|f| f.lang.to_lowercase().contains(&q) || f.addon.to_lowercase().contains(&q)).collect()
}

/// Order of the saved-titles list in the Library tab.
#[derive(PartialEq, Clone, Copy)]
enum LibSort {
    Added,
    Title,
    Watched,
    Rating,
    Year,
}

/// Type filter of the saved-titles list in the Library tab.
#[derive(PartialEq, Clone, Copy)]
enum LibKind {
    All,
    Movie,
    Series,
}

impl LibSort {
    fn label(self) -> &'static str {
        match self {
            Self::Added => "Recently added",
            Self::Title => "Title A–Z",
            Self::Watched => "Recently watched",
            Self::Rating => "Rating",
            Self::Year => "Year",
        }
    }
}

impl LibKind {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All types",
            Self::Movie => "Movies",
            Self::Series => "Shows",
        }
    }

    fn matches(self, kind: &str) -> bool {
        match self {
            Self::All => true,
            Self::Movie => kind == "movie",
            Self::Series => kind == "series",
        }
    }
}

/// Release year from free text like "2010" or "2010–2015".
fn parse_year(s: &str) -> Option<i32> {
    let d: String = s.chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
    d.parse().ok().filter(|y| (1800..=2200).contains(y))
}

/// Year and IMDb rating of a title's details.
fn saved_facts(d: &Meta) -> (Option<i32>, Option<f64>) {
    (d.release_info.as_deref().and_then(parse_year), d.imdb_rating.as_deref().and_then(|r| r.trim().parse::<f64>().ok()).filter(|r| (0.0..=10.0).contains(r)))
}

/// Case-insensitive substring match; a blank query matches everything.
fn title_matches(title: &str, query: &str) -> bool {
    title.to_lowercase().contains(&query.trim().to_lowercase())
}

/// Saved titles of `kind` matching `query`, newest first unless sorted: by title (case-insensitive), or best first with missing values last.
fn saved_view<'a>(items: &'a [SavedTitle], kind: LibKind, sort: LibSort, query: &str) -> Vec<&'a SavedTitle> {
    let mut v: Vec<&SavedTitle> = items.iter().filter(|t| kind.matches(&t.kind) && title_matches(&t.title, query)).collect();
    // Stable sorts keep the "Recently added" order among ties; `None` is the smallest, so it ends up last when reversed.
    match sort {
        LibSort::Added => {}
        LibSort::Title => v.sort_by_key(|t| t.title.to_lowercase()),
        LibSort::Watched => v.sort_by_key(|t| std::cmp::Reverse(t.last_watched)),
        LibSort::Year => v.sort_by_key(|t| std::cmp::Reverse(t.year)),
        LibSort::Rating => v.sort_by(|a, b| b.rating.unwrap_or(f64::NEG_INFINITY).total_cmp(&a.rating.unwrap_or(f64::NEG_INFINITY))),
    }
    v
}

/// What to play when streams arrive: nothing, the only stream, or the top one.
#[derive(Clone, Copy, PartialEq)]
enum Autoplay {
    Off,
    Only,
    Top,
}

enum Msg {
    Error(String),
    Installed(String, Manifest, Option<String>),
    /// Fresh manifest (or error) per installed addon URL, from "Check for updates".
    UpdateCheck(Vec<(String, Result<Manifest, String>)>),
    /// Search title and results.
    Catalog(String, Vec<MetaPreview>),
    /// Row source (addon URL, type, catalog id) and its items or error.
    Row((String, String, String), Result<Vec<MetaPreview>, String>),
    /// Row source, the `skip` it was requested with, and the next page or error.
    More((String, String, String), usize, Result<Vec<MetaPreview>, String>),
    /// Addon catalog source (addon URL, type, catalog id) and the addons it lists, or an error.
    AddonCatalog(AddonCatalogSrc, Result<Vec<AddonEntry>, String>),
    /// Discover grid source and its items or error.
    Discover(DiscoverSrc, Result<Vec<MetaPreview>, String>),
    /// meta, episode (series only), streams, subtitle URLs, what to autoplay on arrival,
    /// the `bingeGroup` to prefer (next episode), and the `nav` it was requested at
    Streams(MetaPreview, Option<Video>, Vec<Stream>, Subs, Autoplay, Option<String>, u64),
    /// series, its full meta (with episodes), and the `nav` it was requested at
    Episodes(MetaPreview, Meta, u64),
    /// Full meta of a movie (genres, cast, ...) for the detail page.
    Details(Meta),
    /// `new_eps_gen`, a saved series and its recently released episodes.
    NewEpisodes(u64, MetaPreview, Vec<Video>),
    /// poster URL, image bytes (empty on failure)
    Poster(String, Vec<u8>),
    /// key, title, resumable source, playable URL/path, subtitle URLs, torrent id, the `nav` it was requested at
    Play(String, String, String, String, Subs, Option<usize>, u64),
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
    Scanned(PathBuf, Result<Vec<LibraryItem>, String>),
    /// Release page of a newer build, if any.
    Update(Option<String>),
    /// A `stremio://` link from the command line, a later launch, or macOS.
    Link(String),
    /// A download's path, run number, bytes got, total and file validator, while it transfers.
    DownloadProgress(PathBuf, u64, u64, u64, String),
    /// A download's path and run number, once it finished, failed or was stopped.
    Downloaded(PathBuf, u64, Result<(), String>),
    /// A tray menu item or icon click.
    Tray(tray::Action),
    /// Progress key and the subtitles every addon offered for it.
    SubSearch(String, Vec<SubFound>),
    /// A saved subtitle's path, or why saving failed.
    SubSaved(Result<PathBuf, String>),
    /// A media key or OS media overlay button.
    Media(souvlaki::MediaControlEvent),
}

/// One horizontal catalog row on the home screen.
struct Row {
    title: String,
    src: (String, String, String),
    /// `None` while loading; empty rows are hidden.
    items: Option<Vec<MetaPreview>>,
    /// Catalog declares the `skip` extra.
    skippable: bool,
    /// Items received from the addon so far (the next `skip`), before de-duplication.
    fetched: usize,
    loading_more: bool,
    /// Last page was empty, had nothing new, or failed.
    exhausted: bool,
}

/// Stable id of a Home row: `<addon url>|<type>|<catalog id>`.
fn row_key(src: &(String, String, String)) -> String {
    format!("{}|{}|{}", src.0, src.1, src.2)
}

/// Indices into `keys` with the keys in `order` first (unknown or repeated ones skipped), the rest in their own order.
fn arrange_rows(keys: &[String], order: &[String]) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for k in order {
        if let Some(i) = keys.iter().position(|x| x == k)
            && !out.contains(&i)
        {
            out.push(i);
        }
    }
    let rest: Vec<usize> = (0..keys.len()).filter(|i| !out.contains(i)).collect();
    out.extend(rest);
    out
}

/// Keys contain `|` and URLs, so settings hold them as a JSON array.
fn encode_keys(keys: &[String]) -> String {
    serde_json::to_string(keys).unwrap_or_default()
}

fn decode_keys(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

/// How far back an episode's release date still counts as new.
const NEW_EPISODE_DAYS: i64 = 14;

/// Most `<series id>:<episode id>` keys kept per series; the oldest of that series go first.
/// A series' own `<series id>:` marker is never evicted, or its next episode would be silently re-seeded.
const NOTIFIED_PER_SERIES: usize = 50;
/// How often saved series are re-checked for new episodes while the app runs.
const NOTIFY_EVERY: Duration = Duration::from_secs(6 * 3600);

/// Drops every notified key whose series is no longer in the saved library, so stale keys don't pile up.
fn prune_unsaved(seen: &mut Vec<String>, saved: &[&str]) {
    seen.retain(|k| saved.iter().any(|id| k.strip_prefix(*id).is_some_and(|r| r.starts_with(':'))));
}

/// Records `eps` of `series` in `seen` and returns those never seen before. A series
/// with no entry yet (first run, or newly saved) is only recorded, so its backlog stays silent.
fn fresh_episodes(seen: &mut Vec<String>, series: &str, eps: &[&str]) -> Vec<String> {
    let marker = format!("{series}:");
    let known = seen.contains(&marker);
    if !known {
        seen.push(marker.clone());
    }
    let mut fresh = Vec::new();
    for ep in eps {
        let k = format!("{series}:{ep}");
        if !seen.contains(&k) {
            seen.push(k);
            if known {
                fresh.push(ep.to_string());
            }
        }
    }
    // Cap only this series' episode keys (never its marker, never other series), but always
    // keep the whole current window so an episode still listed can't be dropped and re-notified.
    let mut excess = seen.iter().filter(|k| k.starts_with(&marker) && **k != marker).count().saturating_sub(NOTIFIED_PER_SERIES.max(eps.len()));
    if excess > 0 {
        seen.retain(|k| {
            let evict = excess > 0 && k.starts_with(&marker) && *k != marker;
            if evict {
                excess -= 1;
            }
            !evict
        });
    }
    fresh
}

/// Replaces any earlier row for the series, so refetches don't duplicate rows; an empty `videos` only removes it.
fn upsert_by_id(list: &mut Vec<(MetaPreview, Vec<Video>)>, meta: MetaPreview, videos: Vec<Video>) {
    list.retain(|(m, _)| m.id != meta.id);
    if !videos.is_empty() {
        list.push((meta, videos));
    }
}

/// Desktop notification; never blocks the UI and a missing notification service only logs.
fn os_notify(title: String, body: String) {
    std::thread::spawn(move || {
        if let Err(e) = notify_rust::Notification::new().appname("Stremflix-rs").summary(&title).body(&body).show() {
            stremflix_rs::elog!("notification: {e}");
        }
    });
}

/// Days since 1970-01-01 (UTC) today.
fn unix_days() -> i64 {
    (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) / 86400) as i64
}

/// `YYYY-MM-DD` of a day count since 1970-01-01 (civil-from-days).
fn date_string(days: i64) -> String {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + i64::from(m <= 2))
}

/// Numbered episodes released on `since..=today` (`YYYY-MM-DD`), in list order.
fn recent_episodes(videos: Vec<Video>, since: &str, today: &str) -> Vec<Video> {
    let day = |v: &Video| v.released.as_deref().and_then(|d| d.get(..10)).map(str::to_owned);
    videos.into_iter().filter(|v| v.episode.is_some() && v.season != Some(0) && day(v).is_some_and(|d| d.as_str() >= since && d.as_str() <= today)).collect()
}

/// How far ahead the Library calendar looks.
const CALENDAR_DAYS: i64 = 30;

/// Calendar entries (series, episode) grouped by release day within `today..=today+CALENDAR_DAYS`, earliest
/// first, headed Today, Tomorrow, then the date. `released` is read as a UTC date (no timezone database);
/// anything that is not `YYYY-MM-DD` is skipped.
fn calendar_groups(entries: &[(MetaPreview, Vec<Video>)], today: i64) -> Vec<(String, Vec<(&MetaPreview, &Video)>)> {
    let well_formed = |d: &str| d.len() == 10 && d.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
    let (from, to) = (date_string(today), date_string(today + CALENDAR_DAYS));
    let mut days: Vec<(&str, &MetaPreview, &Video)> = entries
        .iter()
        .flat_map(|(m, vs)| vs.iter().map(move |v| (m, v)))
        .filter(|(_, v)| v.episode.is_some() && v.season != Some(0))
        .filter_map(|(m, v)| v.released.as_deref().and_then(|d| d.get(..10)).filter(|d| well_formed(d) && *d >= from.as_str() && *d <= to.as_str()).map(|d| (d, m, v)))
        .collect();
    days.sort_by(|a, b| (a.0, &a.1.name, a.2.season, a.2.episode).cmp(&(b.0, &b.1.name, b.2.season, b.2.episode)));
    let mut out: Vec<(String, Vec<(&MetaPreview, &Video)>)> = Vec::new();
    for (d, m, v) in days {
        let head = if d == from { "Today".to_string() } else if d == date_string(today + 1) { "Tomorrow".to_string() } else { d.to_string() };
        match out.last_mut() {
            Some((h, g)) if *h == head => g.push((m, v)),
            _ => out.push((head, vec![(m, v)])),
        }
    }
    out
}

/// Max items kept per catalog row.
const ROW_CAP: usize = 1000;
/// Posters kept in memory (bytes and decoded textures) before the cache is cleared.
const POSTER_CACHE: usize = 1500;

impl Row {
    /// Request the next page when the row is scrolled near its end.
    fn wants_more(&self, near_end: bool) -> bool {
        near_end && self.skippable && !self.loading_more && !self.exhausted && self.items.as_ref().is_some_and(|i| !i.is_empty() && i.len() < ROW_CAP)
    }
}

/// Addon URL, type and id of an addon catalog.
type AddonCatalogSrc = (String, String, String);

/// Addon URL, type, catalog id, picked extras (name, value).
type DiscoverSrc = (String, String, String, Vec<(String, String)>);

/// Discover tab pickers and the grid they loaded.
#[derive(Default)]
struct Discover {
    kind: String,
    /// Addon URL and catalog id.
    cat: (String, String),
    extras: Vec<(String, String)>,
    src: Option<DiscoverSrc>,
    /// `None` while loading.
    items: Option<Vec<MetaPreview>>,
}

const RED: Color32 = Color32::from_rgb(229, 9, 20);
/// Selectable accent colors; the first is Stremio's purple.
const ACCENTS: [(&str, Color32); 4] = [
    ("Purple", Color32::from_rgb(123, 91, 245)),
    ("Blue", Color32::from_rgb(37, 120, 235)),
    ("Green", Color32::from_rgb(30, 160, 90)),
    ("Orange", Color32::from_rgb(230, 120, 20)),
];
static ACCENT_IDX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn accent() -> Color32 {
    ACCENTS[ACCENT_IDX.load(std::sync::atomic::Ordering::Relaxed)].1
}
/// Index of the saved accent name; unknown or missing falls back to the default.
fn accent_index(name: Option<&str>) -> usize {
    ACCENTS.iter().position(|a| Some(a.0) == name).unwrap_or(0)
}
/// Strong text color at `alpha`: a hover/fill wash that reads on both themes.
fn tint(ui: &egui::Ui, alpha: u8) -> Color32 {
    let c = ui.visuals().strong_text_color();
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}
/// Applies the theme and accent to egui; the player menu keeps the dark visuals.
fn apply_theme(ctx: &egui::Context, light: bool) {
    for (theme, is_light) in [(egui::Theme::Dark, false), (egui::Theme::Light, true)] {
        let mut v = if is_light { egui::Visuals::light() } else { egui::Visuals::dark() };
        v.panel_fill = if is_light { Color32::from_gray(245) } else { Color32::from_rgb(20, 20, 20) };
        v.window_fill = v.panel_fill;
        v.extreme_bg_color = if is_light { Color32::WHITE } else { Color32::from_gray(32) };
        v.selection.bg_fill = accent();
        if !is_light {
            // Keep the old dark palette: egui's defaults are dimmer than the grays used before theming.
            v.widgets.noninteractive.fg_stroke.color = Color32::from_gray(200);
            v.weak_text_color = Some(Color32::from_gray(170));
        }
        // Netflix-style flat buttons: no outline or grow on hover, gray that dims when hovered.
        let w = &mut v.widgets;
        let grays = if is_light { [215, 200, 185, 200] } else { [82, 56, 45, 56] };
        for (st, gray) in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open].into_iter().zip(grays) {
            (st.weak_bg_fill, st.bg_stroke, st.corner_radius, st.expansion) = (Color32::from_gray(gray), egui::Stroke::NONE, 4.into(), 0.0);
            st.fg_stroke.color = if is_light { Color32::BLACK } else { Color32::WHITE };
        }
        ctx.set_visuals_of(theme, v);
        ctx.style_mut_of(theme, |s| s.spacing.button_padding = vec2(14.0, 6.0));
    }
    ctx.set_theme(if light { egui::Theme::Light } else { egui::Theme::Dark });
}
const CARD: egui::Vec2 = egui::Vec2::new(150.0, 225.0);

/// Netflix-style card: no border, grows on hover/focus, shrinks while pressed.
/// Returns the click response and the animated rect to paint into.
fn card(ui: &mut egui::Ui, size: egui::Vec2, label: &str) -> (egui::Response, egui::Rect) {
    const GROW: f32 = 1.06;
    // Reserve room for the grown card so the scroll area doesn't clip it.
    let (slot, r) = ui.allocate_exact_size(size * GROW, Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    let target = if r.is_pointer_button_down_on() { 0.96 } else if r.contains_pointer() || r.has_focus() { GROW } else { 1.0 };
    let s = ui.ctx().animate_value_with_time(r.id, target, 0.15);
    (r, egui::Rect::from_center_size(slot.center(), size * s))
}

/// Round play button in the middle of a card, shown while the card is hovered or the button focused.
/// Registered after the card, so it takes the click.
fn card_play(ui: &mut egui::Ui, card: &egui::Response, rect: egui::Rect, label: &str) -> bool {
    let b = egui::Rect::from_center_size(rect.center(), vec2(56.0, 56.0));
    let r = ui.interact(b, card.id.with("play"), Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Play {label}")));
    if card.contains_pointer() || r.has_focus() {
        let p = ui.painter();
        p.circle_filled(b.center(), 28.0, Color32::from_black_alpha(if r.hovered() { 210 } else { 140 }));
        p.circle_stroke(b.center(), 27.0, (2.0, Color32::WHITE));
        // Triangle centroid on the circle center; a font glyph centers on its line box, not its ink.
        let c = b.center();
        p.add(egui::Shape::convex_polygon(vec![c + vec2(-7.0, -11.0), c + vec2(14.0, 0.0), c + vec2(-7.0, 11.0)], Color32::WHITE, egui::Stroke::NONE));
    }
    r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Phosphor Fill weight (same codepoints as Regular); used for play and pause.
fn fill() -> egui::FontFamily {
    egui::FontFamily::Name("Phosphor-Fill".into())
}

/// Phosphor Regular alone. Inter has its own glyphs at some Phosphor codepoints (e.g. ARROW_LEFT), so icons must not go through Inter.
fn phosphor() -> egui::FontFamily {
    egui::FontFamily::Name("Phosphor".into())
}

/// Bold weight of the UI font (egui's `strong()` only brightens the color).
fn bold() -> egui::FontFamily {
    egui::FontFamily::Name("bold".into())
}

/// Phosphor Regular codepoints used by the UI.
mod icons {
    pub const ARROW_LEFT: &str = "\u{E058}";
    pub const ARROWS_IN: &str = "\u{E09A}";
    pub const ARROWS_OUT: &str = "\u{E0A2}";
    pub const CARET_LEFT: &str = "\u{E138}";
    pub const CHECK: &str = "\u{E182}";
    pub const MAGNIFYING_GLASS: &str = "\u{E30C}";
    pub const PAUSE: &str = "\u{E39E}";
    pub const PLAY: &str = "\u{E3D0}";
    pub const PLUS: &str = "\u{E3D4}";
    pub const SKIP_FORWARD: &str = "\u{E5A6}";
    pub const SPEAKER_HIGH: &str = "\u{E44A}";
    pub const SPEAKER_X: &str = "\u{E45C}";
    pub const SUBTITLES: &str = "\u{E1A8}";
    pub const WAVEFORM: &str = "\u{E802}";
    pub const GEAR: &str = "\u{E270}";
    pub const X: &str = "\u{E4F6}";
}

/// Player popover opened from its control-row button.
#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Audio,
    Subtitles,
    Playback,
}

/// Keyboard-focus outline for the popover's custom controls.
fn focus_ring(ui: &egui::Ui, r: &egui::Response, radius: u8) {
    if r.has_focus() {
        ui.painter().rect_stroke(r.rect, radius, egui::Stroke::new(2.0, accent()), egui::StrokeKind::Inside);
    }
}

/// Small caps section heading inside a player popover.
fn menu_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(text.to_uppercase()).size(11.5).family(bold()).color(ui.visuals().weak_text_color()));
    ui.add_space(4.0);
}

/// Full-width popover row: accent check when selected, highlight on hover, long labels truncated.
fn menu_row(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, label));
    focus_ring(ui, &r, 6);
    let p = ui.painter();
    if r.hovered() {
        p.rect_filled(rect, 6, tint(ui, 14));
    }
    if selected {
        p.text(pos2(rect.left() + 14.0, rect.center().y), egui::Align2::LEFT_CENTER, icons::CHECK, egui::FontId::new(16.0, phosphor()), accent());
    }
    let font = egui::FontId::new(15.0, if selected { bold() } else { egui::FontFamily::Proportional });
    let color = if selected { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
    let mut job = egui::text::LayoutJob::simple_singleline(label.to_string(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 50.0);
    let g = p.layout_job(job);
    p.galley(pos2(rect.left() + 40.0, rect.center().y - g.size().y / 2.0), g, color);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Bounds a UI zoom factor to 0.75-2.0 in 0.05 steps; non-finite falls back to 1.0.
fn clamp_scale(z: f32) -> f32 {
    if z.is_finite() { ((z * 20.0).round() / 20.0).clamp(0.75, 2.0) } else { 1.0 }
}

/// Rounded chip, accent-filled when selected.
fn pill(ui: &mut egui::Ui, label: &str, selected: bool, role: egui::Role) -> egui::Response {
    let fg = if selected { Color32::WHITE } else { ui.visuals().strong_text_color() };
    let g = ui.painter().layout_no_wrap(label.to_string(), egui::FontId::new(14.0, if selected { bold() } else { egui::FontFamily::Proportional }), fg);
    let (rect, r) = ui.allocate_exact_size(vec2(g.size().x + 24.0, 30.0), Sense::click());
    r.widget_info(|| egui::WidgetInfo::selected(role, true, selected, label));
    let fill = if selected { accent() } else { tint(ui, if r.hovered() { 30 } else { 16 }) };
    ui.painter().rect_filled(rect, 15, fill);
    focus_ring(ui, &r, 15);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Color dot with an accent ring when selected.
fn swatch(ui: &mut egui::Ui, name: &str, color: Color32, selected: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, name));
    focus_ring(ui, &r, 15);
    if selected || r.hovered() {
        ui.painter().circle_stroke(rect.center(), 13.0, egui::Stroke::new(2.0, if selected { accent() } else { Color32::from_gray(120) }));
    }
    ui.painter().circle_filled(rect.center(), 9.0, color);
    r.on_hover_text(name).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Label on the left, `− value +` on the right; returns -1, 0 or 1.
fn stepper(ui: &mut egui::Ui, label: &str, value: &str) -> i32 {
    let mut step = 0;
    ui.horizontal(|ui| {
        ui.set_min_height(36.0);
        ui.label(RichText::new(label).size(15.0).color(ui.visuals().text_color()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if pill(ui, "+", false, egui::Role::Button).on_hover_text(format!("Increase {label}")).clicked() {
                step = 1;
            }
            ui.add_sized([56.0, 30.0], egui::Label::new(RichText::new(value).size(14.0).color(ui.visuals().strong_text_color())));
            if pill(ui, "−", false, egui::Role::Button).on_hover_text(format!("Decrease {label}")).clicked() {
                step = -1;
            }
        });
    });
    step
}

/// Full-width row with a label and an on/off switch.
fn switch_row(ui: &mut egui::Ui, label: &str, on: &mut bool) -> egui::Response {
    let (rect, mut r) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    if r.clicked() {
        *on = !*on;
        r.mark_changed();
    }
    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, *on, label));
    focus_ring(ui, &r, 6);
    let p = ui.painter();
    if r.hovered() {
        p.rect_filled(rect, 6, tint(ui, 14));
    }
    p.text(pos2(rect.left() + 8.0, rect.center().y), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(15.0), ui.visuals().text_color());
    let t = ui.ctx().animate_bool_with_time(r.id, *on, 0.12);
    let track = egui::Rect::from_center_size(pos2(rect.right() - 26.0, rect.center().y), vec2(36.0, 20.0));
    p.rect_filled(track, 10, Color32::from_gray(80).lerp_to_gamma(accent(), t));
    p.circle_filled(pos2(egui::lerp(track.left() + 10.0..=track.right() - 10.0, t), track.center().y), 7.0, Color32::WHITE);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Inter (OFL) as the UI font: the closest free match to Netflix Sans, which is proprietary.
/// egui's default fonts stay as fallbacks for symbols and emoji.
fn install_fonts(ctx: &egui::Context) {
    let mut f = egui::FontDefinitions::default();
    for (name, bytes) in [("Inter", &include_bytes!("../assets/fonts/Inter-Regular.ttf")[..]), ("Inter-Bold", &include_bytes!("../assets/fonts/Inter-Bold.ttf")[..])] {
        f.font_data.insert(name.into(), Arc::new(egui::FontData::from_static(bytes)));
    }
    // Phosphor icons (MIT). Kept as the last fallback: it also maps a-z for ligatures.
    f.font_data.insert("Phosphor".into(), Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/Phosphor.ttf"))));
    f.families.insert(phosphor(), vec!["Phosphor".into()]);
    f.families.get_mut(&egui::FontFamily::Proportional).unwrap().push("Phosphor".into());
    f.font_data.insert("Phosphor-Fill".into(), Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/Phosphor-Fill.ttf"))));
    f.families.insert(fill(), vec!["Phosphor-Fill".into()]);
    let fallback = f.families[&egui::FontFamily::Proportional].clone();
    f.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(0, "Inter".into());
    f.families.insert(bold(), std::iter::once("Inter-Bold".into()).chain(fallback).collect());
    ctx.set_fonts(f);
}

/// Netflix flat pill: `fill` = [normal, hovered], no outline; fully rounded ends.
fn pill_button(ui: &mut egui::Ui, text: &str, size: f32, min: egui::Vec2, fill: [Color32; 2], fg: Color32) -> egui::Response {
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        for (st, f) in [(&mut w.inactive, fill[0]), (&mut w.hovered, fill[1]), (&mut w.active, fill[1])] {
            (st.weak_bg_fill, st.bg_stroke, st.fg_stroke.color) = (f, egui::Stroke::NONE, fg);
        }
        let rt = |s: &str, family| RichText::new(s).size(size).strong().family(family);
        // A leading icon ("<icon>  Label") is drawn in Phosphor, not Inter; see `phosphor`.
        let button = match text.split_once("  ") {
            Some((i, t)) if !i.is_empty() && i.chars().all(|c| ('\u{E000}'..='\u{F8FF}').contains(&c)) => egui::Button::new((rt(i, phosphor()), rt(t, bold()))),
            _ => egui::Button::new(rt(text, bold())),
        };
        ui.add(button.min_size(min).corner_radius((min.y / 2.0) as u8))
    })
    .inner
}

/// White primary pill (Netflix "Play") and translucent gray secondary pill.
fn white_fill() -> [Color32; 2] {
    [Color32::WHITE, Color32::from_white_alpha(191)]
}
fn gray_fill() -> [Color32; 2] {
    [Color32::from_rgba_unmultiplied(109, 109, 110, 179), Color32::from_rgba_unmultiplied(109, 109, 110, 102)]
}

fn hero_button(ui: &mut egui::Ui, text: &str, fill: [Color32; 2], text_color: Color32) -> egui::Response {
    pill_button(ui, text, 18.0, vec2(130.0, 46.0), fill, text_color)
}

/// An update must keep the installed addon's id, or it would replace a different addon.
fn same_addon(installed: &Manifest, fetched: &Manifest) -> bool {
    installed.id == fetched.id
}

/// Small secondary pill for toolbar actions (Uninstall, Clear, Stop …).
fn small_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    pill_button(ui, text, 14.0, vec2(0.0, 32.0), gray_fill(), Color32::WHITE)
}

/// Small white pill for the main action in a row (Install, Play …).
fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    pill_button(ui, text, 14.0, vec2(0.0, 32.0), white_fill(), Color32::BLACK)
}

/// Labeled text field and its action button on one centered line; fields of consecutive rows start at the same x.
/// True when the button is pressed, or Enter in the field, with non-blank text (any text if `allow_blank`).
/// `tip`: hover text of the button.
fn form_row(ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str, action: &str, tip: Option<&str>, allow_blank: bool) -> bool {
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), Layout::left_to_right(Align::Center), |ui| {
        let l = ui.label(label);
        ui.add_space((150.0 - l.rect.width()).max(0.0));
        let r = ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(400.0)).labelled_by(l.id);
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let b = primary_button(ui, action);
        let b = match tip {
            Some(t) => b.on_hover_text(t),
            None => b,
        };
        (b.clicked() || enter) && (allow_blank || !text.trim().is_empty())
    })
    .inner
}

/// Gray card with centered, wrapped text (used when there's no poster).
fn card_text(ui: &egui::Ui, rect: egui::Rect, text: &str) {
    // Clipped: a very long title would otherwise spill past the card.
    let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    p.rect_filled(rect, 6, Color32::from_gray(40));
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), egui::FontId::proportional(15.0), Color32::WHITE, rect.width() - 16.0);
    job.halign = Align::Center; // each line centered around x = 0
    let g = p.layout_job(job);
    p.galley(pos2(rect.center().x, rect.center().y - g.size().y / 2.0), g, Color32::WHITE);
}

fn stream_row(ui: &mut egui::Ui, n: usize, s: &Stream, reserve: f32) -> egui::Response {
    let mut parts = [&s.name, &s.title, &s.description].into_iter().flatten().map(|x| x.trim()).filter(|x| !x.is_empty());
    let head = parts.next().unwrap_or("Stream").replace('\n', " ");
    let parts: Vec<&str> = parts.collect();
    // Torrent addons already describe the file (name, size) in their text; don't repeat it.
    let stem = s.hints.filename.as_deref().map(|f| f.rsplit_once('.').map_or(f, |x| x.0)).filter(|f| !f.is_empty());
    let described = stem.is_some_and(|f| parts.iter().any(|p| p.contains(f)));
    let file = if described { String::new() } else { [s.hints.filename.clone(), s.hints.video_size.map(fmt_size)].into_iter().flatten().collect::<Vec<_>>().join("  ·  ") };
    let external = (s.target().is_none()).then_some("Opens in your browser");
    let not_web_ready = s.hints.not_web_ready.then_some("Not web-ready (plays directly here)");
    let details: Vec<&str> = parts.into_iter().chain(Some(file.as_str()).filter(|f| !f.is_empty())).chain(not_web_ready).chain(external).collect();
    list_row(ui, n, head, details.join("\n"), reserve)
}

/// Byte count as decimal MB/GB, like addons label sizes.
fn fmt_size(b: u64) -> String {
    if b >= 1_000_000_000 { format!("{:.2} GB", b as f64 / 1e9) } else { format!("{:.0} MB", b as f64 / 1e6) }
}

/// Episode row: number, title, release date.
fn episode_row(ui: &mut egui::Ui, v: &Video, watched: bool) -> egui::Response {
    let n = v.episode.unwrap_or(0) as usize;
    let head = v.title.clone().unwrap_or_else(|| format!("Episode {n}"));
    let date: String = v.released.as_deref().map(|d| d.chars().take(10).collect()).unwrap_or_default();
    list_row(ui, n, if watched { format!("{head}  {}", icons::CHECK) } else { head }, date, EPISODE_BUTTON.x + 12.0)
}

/// Size of the "Mark watched" pill overlaid on episode rows, left of the play circle.
const EPISODE_BUTTON: egui::Vec2 = egui::Vec2::new(150.0, 32.0);

/// Netflix episode-list row: index, bold name, gray details, play circle; highlights on hover.
/// `reserve`: extra width kept free of text on the right (for an overlaid button).
fn list_row(ui: &mut egui::Ui, n: usize, head: String, details: String, reserve: f32) -> egui::Response {
    let (w, text_x, pad) = (ui.available_width().min(960.0), 84.0, 18.0);
    let wrap = (w - text_x - 96.0 - reserve).max(40.0);
    let (strong, weak, panel) = (ui.visuals().strong_text_color(), ui.visuals().weak_text_color(), ui.visuals().panel_fill);
    let p = ui.painter();
    let hg = p.layout(head.clone(), egui::FontId::new(17.0, bold()), strong, wrap);
    let has_details = !details.is_empty();
    let mut job = egui::text::LayoutJob::simple(details, egui::FontId::proportional(14.0), weak, wrap);
    job.sections[0].format.line_height = Some(20.0);
    let dg = p.layout_job(job);
    let block = hg.size().y + if has_details { 8.0 + dg.size().y } else { 0.0 };
    let h = (pad * 2.0 + block).max(88.0);
    let (rect, r) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Play {head}")));
    let t = ui.ctx().animate_bool(r.id, r.hovered() || r.has_focus());
    // Hover wash: strong text at alpha 34 is the old gray 51 on the dark panel.
    let hover = tint(ui, 34);
    let p = ui.painter();
    // Inset so neighbouring rows' highlights never touch.
    p.rect_filled(rect.shrink2(vec2(4.0, 4.0)), 8, hover.gamma_multiply(t));
    let num = p.layout_no_wrap(n.to_string(), egui::FontId::proportional(24.0), strong);
    p.galley(pos2(rect.left() + text_x / 2.0 - num.size().x / 2.0, rect.center().y - num.size().y / 2.0), num, strong);
    let top = rect.center().y - block / 2.0;
    p.galley(pos2(rect.left() + text_x, top), hg.clone(), strong);
    p.galley(pos2(rect.left() + text_x, top + hg.size().y + 8.0), dg, weak);
    // Play button: outlined circle that fills with strong text on hover.
    let c = pos2(rect.right() - 48.0, rect.center().y);
    p.circle(c, 20.0, strong.gamma_multiply(t), (2.0, strong));
    let tri = strong.lerp_to_gamma(panel, t);
    p.add(egui::Shape::convex_polygon(vec![c + vec2(-6.0, -9.0), c + vec2(10.0, 0.0), c + vec2(-6.0, 9.0)], tri, egui::Stroke::NONE));
    r
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "movie" => "Movies",
        "series" => "Shows",
        k => k,
    }
}

fn season_label(season: Option<u32>) -> String {
    match season {
        Some(0) => "Specials".into(),
        Some(n) => format!("Season {n}"),
        None => "Other".into(),
    }
}

/// Episodes in watch order: by season then episode, with specials (season 0), then unnumbered videos, last.
fn sort_episodes(videos: &mut [Video]) {
    videos.sort_by_key(|v| (matches!(v.season, Some(0) | None), v.season.is_none(), v.season, v.episode));
}

/// The episode after `current` in `sort_episodes` order; a regular episode never rolls over into specials or unnumbered videos.
fn next_episode<'a>(episodes: &'a [Video], current: &str) -> Option<&'a Video> {
    let i = episodes.iter().position(|v| v.id == current)?;
    let regular = |v: &Video| v.season.is_some_and(|s| s > 0);
    episodes.get(i + 1).filter(|n| regular(n) || !regular(&episodes[i]))
}

/// Playable source of the first stream in binge group `group` (same addon/quality as the previous episode).
fn binge_source(streams: &[Stream], group: &str) -> Option<String> {
    streams.iter().filter(|s| s.hints.binge_group.as_deref() == Some(group)).find_map(Stream::source)
}

/// Progress-key prefix of trailers, which are never saved to history; `/` keeps it off any `<kind>:<id>` key.
const TRAILER: &str = "trailer/";

/// The addon's `proxyHeaders` for playing `url`, only when it is that addon's own http(s) stream:
/// never for torrents (`url` is then the local torrent server, not `source`) or local files.
fn request_headers(streams: &[Stream], source: &str, url: &str) -> Vec<(String, String)> {
    own_stream(streams, source, url).map(|s| s.hints.request_headers.clone()).unwrap_or_default()
}

/// The stream whose own http(s) URL is `url`, when `url` is also the `source` being played.
fn own_stream<'a>(streams: &'a [Stream], source: &str, url: &str) -> Option<&'a Stream> {
    let own = |s: &&Stream| matches!(s.target(), Some(PlayTarget::Http(u)) if u.as_str() == url);
    (source == url).then(|| streams.iter().find(own)).flatten()
}

/// Series id of a local episode progress key's video id, for history saved before the series id was stored:
/// `tt1:<season>:<episode>` → `tt1`, `kitsu:1:<episode>` → `kitsu:1` (no season). Other ids are returned unchanged.
fn series_id(video: &str) -> &str {
    let num = |x: &str| x.parse::<u32>().is_ok();
    match video.rsplitn(3, ':').collect::<Vec<_>>()[..] {
        [e, s, id] if num(e) && num(s) && id.contains(|c: char| c.is_ascii_digit()) => id,
        [e, ..] if num(e) => &video[..video.len() - e.len() - 1],
        _ => video,
    }
}

/// Episode to start a series at: the one after the last watched (in list order), else the first regular one.
fn start_episode(episodes: &[Video], watched: impl Fn(&Video) -> bool) -> Option<&Video> {
    let first = || episodes.iter().find(|v| v.season.is_some_and(|s| s > 0)).or(episodes.first());
    episodes.iter().rposition(watched).and_then(|i| next_episode(episodes, &episodes[i].id)).or_else(first)
}

/// Cover-fit UVs: crop an image of `size` to `rect`'s aspect ratio, centered.
fn cover_uv(rect: egui::Rect, size: egui::Vec2) -> egui::Rect {
    let (ra, ia) = (rect.aspect_ratio(), size.x / size.y);
    if ia > ra {
        let f = ra / ia;
        egui::Rect::from_min_max(pos2((1.0 - f) / 2.0, 0.0), pos2((1.0 + f) / 2.0, 1.0))
    } else {
        let f = ia / ra;
        egui::Rect::from_min_max(pos2(0.0, (1.0 - f) / 2.0), pos2(1.0, (1.0 + f) / 2.0))
    }
}

/// mpv's `keep-open` pauses on the last frame at end of file.
fn at_end(pos: f64, dur: f64, paused: bool) -> bool {
    paused && dur > 0.0 && pos >= dur - 1.0
}

/// Fraction of the seek bar the cache covers, never behind the playhead.
fn buffered_frac(pos: f64, cached_to: f64, dur: f64) -> f32 {
    if dur > 0.0 { (cached_to.max(pos) / dur).clamp(0.0, 1.0) as f32 } else { 0.0 }
}

/// The playable source when there is exactly one; `None` when there's a choice (or nothing).
fn only_source(streams: &[Stream]) -> Option<String> {
    let mut it = streams.iter().filter_map(Stream::source);
    let first = it.next();
    if it.next().is_some() { None } else { first }
}

/// Inputs of [`hashed_subs`].
struct SubLookup {
    addons: Vec<String>,
    kind: String,
    id: String,
    hints: stremflix_rs::addon::BehaviorHints,
    headers: Vec<(String, String)>,
}

/// HLS/DASH manifest URL: a list of segments, not the video file.
fn is_playlist(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| [".m3u8", ".mpd"].iter().any(|x| u.path().to_ascii_lowercase().ends_with(x)))
}

/// Re-ask subtitle addons with the stream's `videoHash`/`videoSize`/`filename` (the hash from the stream's hints,
/// else read from `url`), so OpenSubtitles can match the exact file. Matches come first, then the earlier `subs`;
/// on timeout or without a hash, `subs` unchanged.
async fn hashed_subs(client: AddonClient, l: SubLookup, url: &str, subs: Subs) -> Subs {
    // Playback waits on this; one shared deadline, so a slow hash or addon cannot hold it up for long
    // and a slow addon does not discard the fast ones' matches.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let ask = async {
        // Without a hash the addons would only repeat the earlier lookup.
        let (hash, size) = match l.hints.video_hash.clone() {
            Some(h) => (h, l.hints.video_size),
            // Playlists and YouTube pages are not the video file, so they have no hash.
            None if stremflix_rs::addon::is_youtube(url) || is_playlist(url) => return Vec::new(),
            None => match tokio::time::timeout_at(deadline, client.video_hash(url, &l.headers)).await {
                Ok(Ok((h, n))) => (h, Some(n)),
                _ => return Vec::new(),
            },
        };
        let extra: Vec<(&str, String)> = [("videoHash", Some(hash)), ("videoSize", size.map(|n| n.to_string())), ("filename", l.hints.filename.clone())]
            .into_iter()
            .filter_map(|(k, v)| Some((k, v?)))
            .collect();
        let ask = |u: String| {
            let (c, k, i, e) = (client.clone(), l.kind.clone(), l.id.clone(), extra.clone());
            async move { tokio::time::timeout_at(deadline, c.subtitles(&u, &k, &i, &e)).await? }
        };
        concurrently(l.addons.clone(), ask).await.into_iter().flatten().flatten().map(|s| (s.url, s.lang)).collect::<Subs>()
    };
    let mut out = ask.await;
    for s in subs {
        if !out.iter().any(|(u, _)| *u == s.0) {
            out.push(s);
        }
    }
    out
}

/// Run `f` for every item at once; results come back in item order (e.g. addon priority).
async fn concurrently<I, T, F>(items: Vec<I>, f: impl Fn(I) -> F) -> Vec<anyhow::Result<T>>
where
    T: Send + 'static,
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
{
    let tasks: Vec<_> = items.into_iter().map(|i| tokio::spawn(f(i))).collect();
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        out.push(t.await.unwrap_or_else(|e| Err(e.into())));
    }
    out
}

/// What an OS media control event does to the player.
#[derive(Debug, PartialEq)]
enum MediaAction {
    Cmd(&'static [&'static str]),
    /// Seconds, and whether relative.
    Seek(f64, bool),
    Next,
    Stop,
}

fn media_action(e: souvlaki::MediaControlEvent) -> Option<MediaAction> {
    use souvlaki::{MediaControlEvent as E, SeekDirection};
    let sign = |d| if d == SeekDirection::Forward { 1.0 } else { -1.0 };
    Some(match e {
        E::Play => MediaAction::Cmd(&["set", "pause", "no"]),
        E::Pause => MediaAction::Cmd(&["set", "pause", "yes"]),
        E::Toggle => MediaAction::Cmd(&["cycle", "pause"]),
        E::Next => MediaAction::Next,
        E::Stop => MediaAction::Stop,
        E::Seek(d) => MediaAction::Seek(10.0 * sign(d), true),
        E::SeekBy(d, by) => MediaAction::Seek(by.as_secs_f64() * sign(d), true),
        E::SetPosition(p) => MediaAction::Seek(p.0.as_secs_f64(), false),
        _ => return None,
    })
}

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
    /// Video held down: playing at 2x until release.
    fast: bool,
    /// Press start (egui clock) of a press that began on the video, while the button stays down.
    hold: Option<f64>,
    /// The last click on the video was a hold release, so the next one must not pair with it into a double-click.
    after_hold: bool,
    /// Wheel scroll not yet turned into volume steps (points).
    wheel: f32,
    /// The up-next card was dismissed.
    next_dismissed: bool,
    /// Addon-marked spans of the playing stream (`behaviorHints.skip`).
    skips: Vec<Skip>,
    /// Stats overlay shown; `net` is its torrent line, refreshed by a task about once per second (`net_next`, egui clock).
    stats: bool,
    net: Arc<std::sync::Mutex<Option<NetStats>>>,
    net_next: f64,
    search: SubSearch,
    /// Per-session picture scaling: aspect override index into `ASPECTS`, fill (crop) instead of fit, zoom in tenths.
    aspect: usize,
    fill: bool,
    zoom: i32,
    /// Screen/idle inhibit, attempted once per unpaused stretch (inner `None`: unavailable); embedded mpv can't do it itself.
    awake: Option<Option<keepawake::KeepAwake>>,
    /// Video track to restore once the window is shown again; video is off while hidden.
    hidden_vid: Option<String>,
}

impl Drop for Playing {
    fn drop(&mut self) {
        release_awake(self.awake.take());
    }
}

/// Inhibits screen dimming, blanking and idle sleep until dropped; `None` when no inhibit service is reachable.
fn keep_awake() -> Option<keepawake::KeepAwake> {
    // Linux: one missing service (ScreenSaver or logind) fails the combined request outright, so fall back to each alone.
    // catch_unwind: a failed attempt drops its half-built guard, whose unwrap can panic (see release_awake).
    [(true, true), (true, false), (false, true)].into_iter().find_map(|(display, idle)| {
        std::panic::catch_unwind(|| keepawake::Builder::default().display(display).idle(idle).reason("Video playback").app_name("Stremflix").app_reverse_domain("io.github.stremflix").create().ok()).ok().flatten()
    })
}

/// keepawake's Linux drop unwraps the D-Bus UnInhibit call, which fails if the screensaver service went away mid-playback.
fn release_awake(awake: Option<Option<keepawake::KeepAwake>>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(awake)));
}

struct App {
    sub_style: SubStyle,
    rt: tokio::runtime::Runtime,
    db: Db,
    data_dir: PathBuf,
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
    search_results: Option<(String, Vec<MetaPreview>)>,
    discover: Discover,
    selected: Option<(MetaPreview, Vec<Stream>, Subs)>,
    /// Episodes of the selected series (in `sort_episodes` order), the season shown,
    /// and the episode whose streams are in `selected`.
    episodes: Vec<Video>,
    season: Option<u32>,
    episode: Option<Video>,
    /// Series id whose episodes are loading, and on arrival the video id whose streams to show
    /// (Continue Watching), or `None` to play the next episode (card play button).
    pending: Option<(String, Option<String>)>,
    /// Bumped by every navigation (open a title or episode, play, Back); replies for an older one are dropped.
    nav: u64,
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
    /// Poster bytes by URL; `None` while loading, empty if the fetch failed.
    posters: HashMap<String, Option<Arc<[u8]>>>,
    folder: String,
    /// Preferred audio / subtitle languages (mpv `alang` / `slang`), as edited; saved to the settings table only when valid.
    langs: [String; 2],
    library: Vec<LibraryItem>,
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
    /// Downloads waiting, running, paused or failed, oldest first.
    queue: Vec<Job>,
    /// Counts transfers, so a stopped one's late messages can be told from the current one's.
    download_runs: u64,
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
    /// External player command, e.g. `vlc` or `mpv --fs`.
    external_player: String,
    /// Settings > Data: path typed for an import, and the last export/import result.
    data_import: String,
    data_msg: Option<String>,
    /// mpv hardware decoding.
    hwdec: bool,
    /// Custom Home row order and hidden rows (row keys).
    home_order: Vec<String>,
    home_hidden: Vec<String>,
    /// Every Home row as (key, title) in display order, hidden ones included.
    home_all: Vec<(String, String)>,
    light: bool,
    /// Play the next episode when one ends.
    autoplay_next: bool,
    /// Closing the window hides it to the tray instead of quitting.
    tray_on_close: bool,
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
    /// Release page of a newer build, shown until dismissed.
    update: Option<String>,
    /// Nav pill animation: tab it's heading to, start rect, target rect, start time.
    pill: Option<(Tab, egui::Rect, egui::Rect, f64)>,
    player: Option<Playing>,
    last_poll: Instant,
    /// GL function loader of eframe's context, for libmpv's OpenGL renderer.
    gl_proc: Option<GlProc>,
    window: Geo,
    /// Video framebuffer, its texture (owned by egui once registered) and pixel size; reused across playbacks.
    video: Option<(glow::Framebuffer, glow::Texture, egui::TextureId, [usize; 2])>,
    media: Option<souvlaki::MediaControls>,
    /// Title, paused and duration (s) last published to the media controls.
    media_shown: Option<(String, bool, u64)>,
}

/// `<data>/screenshots/<title>-<unix secs>.png`, with the title reduced to filename-safe characters.
fn screenshot_path(dir: &std::path::Path, title: &str, secs: u64) -> PathBuf {
    let t: String = title.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).take(60).collect();
    dir.join("screenshots").join(format!("{t}-{secs}.png"))
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

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// A download that is waiting, running, paused or failed.
struct Job {
    d: Download,
    /// http URL or magnet.
    source: String,
    headers: Vec<(String, String)>,
    state: State,
    /// Which transfer this job is on; see `App::download_runs`.
    run: u64,
    got: u64,
    total: u64,
    /// Bytes per second, smoothed.
    speed: f64,
    /// When and at how many bytes the speed was last measured.
    last: (Instant, u64),
    /// Dropping or sending on it stops the transfer.
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    err: Option<String>,
    /// ETag or Last-Modified (or `IMMUTABLE`) of the file as first fetched; Resume restarts if it is empty or changed.
    validator: String,
    /// A transfer task is still alive (a paused one flushes its last write first); no new run starts until it reports.
    running: bool,
}

impl Job {
    fn new(d: Download, source: String, headers: Vec<(String, String)>, state: State, validator: String, total: u64) -> Self {
        let got = std::fs::metadata(download::part(&d.path)).map_or(0, |m| m.len());
        // A torrent stream is fixed by its info hash, so it needs no validator.
        let validator = if source.starts_with("magnet:") { download::IMMUTABLE.into() } else { validator };
        Self { d, source, headers, state, run: 0, got, total, speed: 0.0, last: (Instant::now(), got), cancel: None, err: None, validator, running: false }
    }
}

impl App {
    fn new(data_dir: PathBuf) -> anyhow::Result<Self> {
        let db = Db::open(&data_dir.join("stremflix-rs.db"))?;
        let queue: Vec<Job> = db.download_queue()?.into_iter().map(|q| Job::new(q.d, q.source, q.headers, q.state, q.validator, q.total)).collect();
        download::clear_partial(&data_dir.join("downloads"), &queue.iter().map(|j| j.d.path.clone()).collect::<Vec<_>>());
        let search_history = db.setting("search_history")?.map(|s| db::decode_history(&s)).unwrap_or_default();
        let (tx, rx) = channel();
        ACCENT_IDX.store(accent_index(db.setting("accent")?.as_deref()), std::sync::atomic::Ordering::Relaxed);
        let mut app = Self {
            sub_style: db.setting("sub_style")?.map(|s| SubStyle::decode(&s)).unwrap_or_default(),
            rt: tokio::runtime::Runtime::new()?,
            addons: db.addons()?,
            library: db.library_items()?,
            saved: db.saved_titles()?,
            lists: db.lists()?,
            lib_list: None,
            list_name: String::new(),
            new_episodes: Vec::new(),
            upcoming: Vec::new(),
            new_eps_gen: 0,
            notify_eps: db.setting("notify_eps")?.as_deref() != Some("0"),
            notified: db.setting("notified_eps")?.map(|s| decode_keys(&s)).unwrap_or_default(),
            notify_check: Instant::now(),
            notice: None,
            downloads: db.downloads()?,
            queue,
            download_runs: 0,
            lib_sort: LibSort::Added,
            lib_kind: LibKind::All,
            lib_query: String::new(),
            recent: db.recent_progress(20)?,
            watched: db.watched(50)?,
            watched_keys: db.watched_keys()?,
            import_path: String::new(),
            login: Default::default(),
            account: db.setting("stremio_auth")?.filter(|k| !k.is_empty()).map(|_| db.setting("stremio_email").ok().flatten().filter(|e| !e.is_empty()).unwrap_or_else(|| "a Stremio account".into())),
            account_busy: false,
            confirm_upload: false,
            account_note: None,
            addon_details: None,
            torrent_dir: db.setting("torrent_dir")?.unwrap_or_default(),
            torrent_cache: db.setting("torrent_cache")?.and_then(|v| v.parse().ok()).unwrap_or(0),
            torrent_profile: Profile::from_label(&db.setting("torrent_profile")?.unwrap_or_default()),
            proxy: [db.setting("proxy_kind")?.unwrap_or_default(), db.setting("proxy_addr")?.unwrap_or_default(), db.setting("proxy_user")?.unwrap_or_default(), db.setting("proxy_pass")?.unwrap_or_default()],
            torrent_limits: (
                db.setting("torrent_down_limit")?.and_then(|v| v.parse().ok()).unwrap_or(0),
                db.setting("torrent_up_limit")?.and_then(|v| v.parse().ok()).unwrap_or(0),
            ),
            data_import: String::new(),
            data_msg: None,
            external_player: db.setting("external_player")?.unwrap_or_else(|| "vlc".into()),
            hwdec: db.setting("hwdec")?.as_deref() != Some("0"),
            home_order: decode_keys(&db.setting("home_order")?.unwrap_or_default()),
            home_hidden: decode_keys(&db.setting("home_hidden")?.unwrap_or_default()),
            home_all: Vec::new(),
            light: db.setting("theme")?.as_deref() == Some("light"),
            autoplay_next: db.setting("autoplay_next")?.as_deref() != Some("0"),
            tray_on_close: db.setting("tray_on_close")?.as_deref() == Some("1"),
            tray: None,
            hidden: false,
            quit: false,
            volume: db.setting("volume")?.and_then(|v| v.parse().ok()).filter(|v| (0.0..=130.0).contains(v)).unwrap_or(100.0),
            ui_scale: clamp_scale(db.setting("ui_scale")?.and_then(|v| v.parse().ok()).unwrap_or(1.0)),
            default_speed: db.setting("default_speed")?.and_then(|v| v.parse().ok()).filter(|v| SPEEDS.contains(v)).unwrap_or(1.0),
            seek_steps: [seek_step(db.setting("seek_step")?, 10), seek_step(db.setting("seek_step_shift")?, 30)],
            error: None,
            file_pick: None,
            update: None,
            folder: db.setting("last_folder")?.unwrap_or_default(),
            langs: [db.setting("alang")?.unwrap_or_default(), db.setting("slang")?.unwrap_or_default()],
            db,
            data_dir,
            client: AddonClient::new(),
            torrent: Default::default(),
            tx,
            rx,
            tab: Tab::Home,
            addon_url: String::new(),
            addon_updates: Vec::new(),
            checking_updates: false,
            configuring: None,
            browse: None,
            rows: Vec::new(),
            search_results: None,
            discover: Discover::default(),
            selected: None,
            episodes: Vec::new(),
            season: None,
            episode: None,
            pending: None,
            nav: 0,
            details: None,
            details_want: Default::default(),
            scroll_top: false,
            show_streams: false,
            magnet: String::new(),
            search: String::new(),
            search_history,
            posters: HashMap::new(),
            player: None,
            pill: None,
            last_poll: Instant::now(),
            gl_proc: None,
            window: Geo::default(),
            video: None,
            media: None,
            media_shown: None,
        };
        if app.addons.is_empty() {
            app.tab = Tab::Addons;
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

    fn spawn<F: Future<Output = anyhow::Result<Msg>> + Send + 'static>(&self, ctx: &egui::Context, f: F) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(f.await.unwrap_or_else(|e| Msg::Error(format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    /// Pull the signed-in Stremio account's addons and library into the local DB.
    fn sync_account(&mut self, ctx: &egui::Context) {
        if self.account_busy {
            return;
        }
        let Some(key) = self.db.setting("stremio_auth").ok().flatten().filter(|k| !k.is_empty()) else { return };
        self.account_busy = true;
        let client = self.client.clone();
        self.spawn(ctx, async move {
            let imp = client.stremio_pull(&key).await.map_err(|e| format!("Stremio account sync: {e:#}"));
            Ok(Msg::Synced(key, imp))
        });
    }

    /// Push local addons and library to the signed-in Stremio account.
    fn upload_account(&mut self, ctx: &egui::Context) {
        if self.account_busy {
            return;
        }
        let Some(key) = self.db.setting("stremio_auth").ok().flatten().filter(|k| !k.is_empty()) else { return };
        let up = match stremio_import::Upload::read(&self.db) {
            Ok(up) => up,
            Err(e) => return report(&mut self.error, format!("reading library: {e:#}")),
        };
        (self.account_busy, self.account_note) = (true, None);
        let client = self.client.clone();
        self.spawn(ctx, async move {
            let r = client.stremio_push(&key, &up).await;
            let r = r.map(|(a, l)| format!("Uploaded {a} addons and {l} changed library items.")).map_err(|e| format!("Stremio account upload: {e:#}"));
            Ok(Msg::Uploaded(key, r))
        });
    }

    /// Resolve `source` (http URL, magnet, or local path) and start playback.
    fn play(&mut self, ctx: &egui::Context, key: String, title: String, source: String, subs: Subs) {
        self.clear_file_pick();
        self.nav += 1;
        let nav = self.nav;
        let lookup = self.sub_lookup(&key, &source);
        let torrent = source.starts_with("magnet:") || is_torrent_file(&source);
        let mut subs = subs;
        if !torrent {
            subs.extend(player::sidecar_subs(std::path::Path::new(&source))); // empty for URLs and missing paths
        }
        if !torrent && lookup.is_none() {
            let _ = self.tx.send(Msg::Play(key, title, source.clone(), source, subs, None, nav));
            return;
        }
        let (start, client) = (self.engine(), self.client.clone());
        self.spawn(ctx, async move {
            let (url, id) = match torrent {
                true => {
                    let engine = start.await?;
                    if source.starts_with("magnet:") {
                        let files = engine.media_choices(&source).await?;
                        if !files.is_empty() {
                            return Ok(Msg::PickFile(key, title, source, subs, files, nav));
                        }
                    }
                    let (url, id) = engine.stream_url(&source, None, false).await?;
                    (url, Some(id))
                }
                false => (source.clone(), None),
            };
            let subs = match lookup {
                Some(l) => hashed_subs(client, l, &url, subs).await,
                None => subs,
            };
            Ok(Msg::Play(key, title, source, url, subs, id, nav))
        });
    }

    /// The torrent engine, started on first use.
    fn engine(&self) -> impl Future<Output = anyhow::Result<Arc<TorrentEngine>>> + Send + 'static {
        // A chosen folder gets its own subfolder: the engine clears its folder at start.
        let dir = match self.db.setting("torrent_dir").ok().flatten().filter(|d| !d.is_empty()) {
            Some(d) => PathBuf::from(d).join("stremflix-rs-torrents"),
            None => self.data_dir.join("torrents"),
        };
        let (cell, cache, profile, limits) = (self.torrent.clone(), self.torrent_cache, self.torrent_profile, self.torrent_limits);
        let dht = std::env::var_os("STREMFLIX_RS_NO_DHT").is_none();
        async move { cell.get_or_try_init(|| async move { TorrentEngine::start(dir, dht, cache, profile, limits).await.map(Arc::new) }).await.cloned() }
    }

    /// What a hash-matched subtitle query needs for the selected title's stream `source`;
    /// `None` without subtitle addons or outside the selected title's streams.
    fn sub_lookup(&self, key: &str, source: &str) -> Option<SubLookup> {
        let (m, streams, _) = self.selected.as_ref().filter(|_| self.play_key().is_some_and(|(k, _)| k == key))?;
        let s = streams.iter().find(|s| s.source().as_deref() == Some(source))?;
        let id = self.episode.as_ref().map_or(m.id.clone(), |v| v.id.clone());
        let addons = self.addons_for("subtitles", &m.kind, &id);
        (!addons.is_empty()).then(|| SubLookup { addons, kind: m.kind.clone(), id, hints: s.hints.clone(), headers: request_headers(streams, source, source) })
    }

    /// Play a magnet link or a local `.torrent` file (typed, pasted or dropped).
    fn play_torrent(&mut self, ctx: &egui::Context, input: &str) {
        let input = input.trim();
        if is_torrent_file(input) {
            let name = std::path::Path::new(input).file_stem().map_or("Torrent".into(), |n| n.to_string_lossy().into_owned());
            return self.play(ctx, format!("file:{input}"), name, input.into(), vec![]);
        }
        match parse_magnet_hash(input) {
            Some(h) => self.play(ctx, format!("btih:{h}"), format!("Torrent {}", &h[..8]), input.into(), vec![]),
            None => report(&mut self.error, "not a magnet link or .torrent file"),
        }
    }

    /// Queue stream `source` (http URL or magnet) for the downloads folder as `d`; it starts when a slot is free.
    /// `hint` (a file name or URL path) gives the file extension.
    fn download(&mut self, ctx: &egui::Context, mut d: Download, source: String, headers: Vec<(String, String)>, hint: Option<String>) {
        match download::reserve(&self.data_dir.join("downloads"), &download::file_name(&d.title, hint.as_deref())) {
            Ok(p) => d.path = p,
            Err(e) => return report(&mut self.error, format!("download: {e:#}")),
        }
        if let Err(e) = self.db.queue_download(&d, &source, &headers, State::Queued) {
            drop(std::fs::remove_file(download::part(&d.path)));
            return report(&mut self.error, format!("download: {e:#}"));
        }
        self.queue.push(Job::new(d, source, headers, State::Queued, String::new(), 0));
        self.pump_downloads(ctx);
    }

    /// Start queued downloads while fewer than `MAX_ACTIVE` run.
    fn pump_downloads(&mut self, ctx: &egui::Context) {
        // A stopped job whose task is still flushing keeps its slot and can't restart yet.
        let states: Vec<State> = self.queue.iter().map(|j| if j.running { State::Active } else { j.state }).collect();
        for i in download::to_start(&states, download::MAX_ACTIVE) {
            self.start_download(ctx, i);
        }
    }

    fn set_job_state(&mut self, i: usize, state: State) {
        let j = &mut self.queue[i];
        j.state = state;
        j.speed = 0.0;
        if let Err(e) = self.db.set_download_state(&j.d, state) {
            report(&mut self.error, format!("saving download state: {e:#}"));
        }
    }

    /// Run `queue[i]` in the background, continuing its `.part` file if it has one.
    fn start_download(&mut self, ctx: &egui::Context, i: usize) {
        self.set_job_state(i, State::Active);
        self.download_runs += 1;
        let run = self.download_runs;
        let (cancel, mut cancelled) = tokio::sync::oneshot::channel::<()>();
        let j = &mut self.queue[i];
        (j.run, j.cancel, j.err, j.last, j.running) = (run, Some(cancel), None, (Instant::now(), j.got), true);
        let (path, source, headers, prev) = (j.d.path.clone(), j.source.clone(), j.headers.clone(), (j.validator.clone(), j.total));
        let engine = source.starts_with("magnet:").then(|| self.engine());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.rt.spawn(async move {
            let res = async {
                let (url, headers, pin) = match engine {
                    None => (source, headers, None),
                    Some(engine) => {
                        let engine = engine.await?;
                        // Pinned so playing or stopping another title doesn't pause it mid-download.
                        let (url, id) = engine.stream_url(&source, None, true).await?;
                        (url, vec![], Some((engine, id)))
                    }
                };
                let progress = |got, total, validator: &str| {
                    let _ = tx.send(Msg::DownloadProgress(path.clone(), run, got, total, validator.to_string()));
                    ctx.request_repaint();
                };
                // Pausing or dropping the job sends or drops the sender; fetch flushes its file before returning.
                let res = download::fetch(&url, &headers, &path, (&prev.0, prev.1), &mut cancelled, progress).await;
                // shortcut: a paused torrent is unpinned but keeps its cache copy until the next playback releases it; resume re-adds it.
                if let Some((engine, id)) = pin {
                    engine.unpin(id).await;
                }
                res
            };
            let res = res.await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Downloaded(path, run, res));
            ctx.request_repaint();
        });
    }

    /// Stop `queue[i]`'s transfer (it keeps its `.part` file) and mark it `state`.
    fn stop_download(&mut self, ctx: &egui::Context, i: usize, state: State) {
        if let Some(c) = self.queue[i].cancel.take() {
            let _ = c.send(());
        }
        self.set_job_state(i, state);
        self.pump_downloads(ctx);
    }

    /// Forget `queue[i]`: stop it, delete its `.part` file and its row.
    fn remove_download(&mut self, ctx: &egui::Context, i: usize) {
        let j = self.queue.remove(i);
        if let Some(c) = j.cancel {
            let _ = c.send(());
        }
        drop(std::fs::remove_file(download::part(&j.d.path)));
        if let Err(e) = self.db.remove_download(&j.d) {
            report(&mut self.error, format!("removing download {}: {e:#}", j.d.title));
        }
        self.pump_downloads(ctx);
    }

    /// Stop every torrent but `keep`, so none keeps downloading and seeding. Runs on the UI thread,
    /// so it always finishes before a later `play` adds the next torrent.
    fn release_torrents(&self, keep: Option<usize>) {
        if let Some(engine) = self.torrent.get() {
            // shortcut: blocks the UI up to 2 s while librqbit deletes; fine for a few torrents at a time.
            let _ = self.rt.block_on(tokio::time::timeout(Duration::from_secs(2), engine.release_except(keep)));
        }
    }

    /// Drop a pending file picker and stop its torrent, which nothing else will play.
    fn clear_file_pick(&mut self) {
        if self.file_pick.take().is_some() {
            self.release_torrents(self.player.as_ref().and_then(|p| p.torrent));
        }
    }

    fn handle(&mut self, ctx: &egui::Context, msg: Msg) {
        match msg {
            Msg::Error(e) => report(&mut self.error, e),
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
            Msg::UpdateCheck(res) => {
                self.checking_updates = false;
                self.addon_updates = res;
            }
            Msg::Catalog(name, items) => {
                self.search_results = Some((name, items));
            }
            Msg::Row(src, res) => {
                let Some(row) = self.rows.iter_mut().find(|r| r.src == src) else { return };
                row.items = Some(res.unwrap_or_else(|e| {
                    report(&mut self.error, format!("loading {}: {e:#}", row.title));
                    Vec::new()
                }));
                row.fetched = row.items.as_ref().map_or(0, Vec::len);
            }
            Msg::More(src, skip, res) => {
                // Ignore replies for a row that was rebuilt or advanced since the request.
                let Some(row) = self.rows.iter_mut().find(|r| r.src == src && r.loading_more && r.fetched == skip) else { return };
                row.loading_more = false;
                match res {
                    Ok(page) => {
                        row.fetched += page.len();
                        let items = row.items.get_or_insert_with(Vec::new);
                        let mut seen: std::collections::HashSet<String> = items.iter().map(|m| m.id.clone()).collect();
                        let before = items.len();
                        items.extend(page.into_iter().filter(|m| seen.insert(m.id.clone())).take(ROW_CAP - before));
                        row.exhausted = items.len() == before;
                    }
                    Err(e) => {
                        row.exhausted = true;
                        report(&mut self.error, format!("loading more of {}: {e:#}", row.title));
                    }
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
                    self.discover.items = Some(res.unwrap_or_else(|e| {
                        report(&mut self.error, format!("loading catalog: {e:#}"));
                        Vec::new()
                    }));
                }
            }
            Msg::Streams(meta, episode, streams, subs, autoplay, binge, nav) => {
                // Stale reply for a title the user has since left.
                if nav != self.nav {
                    return;
                }
                if episode.is_none() {
                    self.episodes.clear();
                    self.scroll_top = true;
                }
                let fallback = || if autoplay == Autoplay::Top { streams.iter().find_map(Stream::source) } else { only_source(&streams) };
                let src = binge.and_then(|g| binge_source(&streams, &g)).or_else(fallback);
                (self.selected, self.episode) = (Some((meta, streams, subs.clone())), episode);
                // Details may have arrived first; a deep-linked title only knew its id.
                if let (Some((sel, ..)), Some(d)) = (self.selected.as_mut(), &self.details) {
                    fill_deep_linked(sel, d);
                }
                // `Only` autoplays when there's no choice to make; otherwise show the stream picker.
                if autoplay != Autoplay::Off {
                    match (src, self.play_key()) {
                        (Some(src), Some((key, title))) => self.play(ctx, key, title, src, subs),
                        _ => self.show_streams = true,
                    }
                }
            }
            Msg::Episodes(mut meta, mut m, nav) => {
                if nav != self.nav {
                    return;
                }
                fill_deep_linked(&mut meta, &m); // before the rename below, which the name guard depends on
                let mut videos = std::mem::take(&mut m.videos);
                // A Continue Watching entry only knows the episode title.
                let meta = MetaPreview { name: m.name.clone(), poster: meta.poster.or(m.poster.clone()), ..meta };
                self.backfill_saved_facts(&m);
                self.details = Some(m);
                sort_episodes(&mut videos);
                // Episodes imported as watched from Stremio: saved once as local marks, so they can be unmarked.
                let bk = format!("bitfield:{}:{}", meta.kind, meta.id);
                if let Ok(Some(field)) = self.db.setting(&bk) {
                    let keys: Vec<String> = stremio_import::watched_videos(&field, &videos).into_iter().map(|id| format!("{}:{id}", meta.kind)).collect();
                    // An anchor missing from this addon's list marks nothing; keep the field for another source.
                    if !keys.is_empty() {
                        match self.db.apply_watched_bitfield(&bk, &keys) {
                            Ok(marked) => self.watched_keys.extend(marked),
                            Err(e) => report(&mut self.error, format!("saving watched episodes: {e:#}")),
                        }
                    }
                }
                let key = |v: &Video| format!("{}:{}", meta.kind, v.id);
                // Open on the season of the episode in progress, else of the next unwatched one.
                let resume = self.recent.iter().find_map(|p| videos.iter().find(|v| key(v) == p.key));
                self.season = resume.or_else(|| start_episode(&videos, |v| self.watched_keys.contains(&key(v)))).and_then(|v| v.season);
                (self.episodes, self.episode) = (videos, None);
                self.selected = Some((meta.clone(), Vec::new(), Vec::new()));
                self.scroll_top = true;
                let Some((_, show)) = self.pending.take().filter(|(id, _)| *id == meta.id) else { return };
                let v = match &show {
                    Some(id) => self.episodes.iter().find(|v| v.id == *id),
                    None => {
                        // Resume the episode in progress with its stream, else start the next one.
                        let resume = self.recent.iter().find(|p| self.episodes.iter().any(|v| key(v) == p.key)).cloned();
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
                let binge = self.db.setting(&format!("binge:{}:{}", meta.kind, meta.id)).ok().flatten().filter(|g| !g.is_empty());
                let autoplay = if show.is_some() { Autoplay::Off } else { Autoplay::Top };
                self.open_streams(ctx, meta, Some(v), autoplay, binge);
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
            Msg::Link(s) => match stremflix_rs::addon::parse_deep_link(&s) {
                Some(link) => {
                    self.show_window(ctx, true);
                    // Leave the player like its Back button does, or the link opens behind it.
                    if self.player.is_some() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                        self.stop_player();
                        self.release_torrents(None);
                    }
                    self.open_link(ctx, link);
                }
                None if s.is_empty() => self.show_window(ctx, true),
                None => report(&mut self.error, format!("unsupported link: {}", sanitize_text(&s, 200))),
            },
            Msg::NewEpisodes(epoch, meta, mut videos) => {
                if epoch == self.new_eps_gen {
                    sort_episodes(&mut videos);
                    // Episodes imported as watched from Stremio, as in `Msg::Episodes`.
                    let bk = format!("bitfield:{}:{}", meta.kind, meta.id);
                    if let Ok(Some(field)) = self.db.setting(&bk) {
                        let keys: Vec<String> = stremio_import::watched_videos(&field, &videos).into_iter().map(|id| format!("{}:{id}", meta.kind)).collect();
                        if !keys.is_empty() {
                            match self.db.apply_watched_bitfield(&bk, &keys) {
                                Ok(marked) => self.watched_keys.extend(marked),
                                Err(e) => report(&mut self.error, format!("saving watched episodes: {e:#}")),
                            }
                        }
                    }
                    let today = unix_days();
                    self.upcoming.retain(|(m, _)| m.id != meta.id);
                    let ahead = recent_episodes(videos.clone(), &date_string(today), &date_string(today + CALENDAR_DAYS));
                    if self.saved.iter().any(|t| t.key == format!("series:{}", meta.id)) && !ahead.is_empty() {
                        self.upcoming.push((meta.clone(), ahead));
                    }
                    let videos = recent_episodes(videos, &date_string(today - (NEW_EPISODE_DAYS - 1)), &date_string(today));
                    if self.saved.iter().any(|t| t.key == format!("series:{}", meta.id)) {
                        // Also with no recent episode: seeds the series' marker so its next one notifies.
                        let ids: Vec<&str> = videos.iter().map(|v| v.id.as_str()).collect();
                        let saved_ids: Vec<&str> = self.saved.iter().filter_map(|t| t.key.strip_prefix("series:")).collect();
                        prune_unsaved(&mut self.notified, &saved_ids);
                        let fresh = fresh_episodes(&mut self.notified, &meta.id, &ids);
                        if let Err(e) = self.db.set_setting("notified_eps", &encode_keys(&self.notified)) {
                            report(&mut self.error, format!("saving notified episodes: {e:#}"));
                        }
                        let labels: Vec<String> = videos
                            .iter()
                            .filter(|v| fresh.contains(&v.id) && !self.watched_keys.contains(&format!("{}:{}", meta.kind, v.id)))
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
                if self.db.setting("stremio_auth").ok().flatten().as_deref() == Some(key.as_str()) {
                    // Only for the current key: a stale sync mustn't clear a newer sign-in's flag.
                    self.account_busy = false;
                    match imp {
                        Ok(imp) => self.handle(ctx, Msg::Imported(imp)),
                        Err(e) => report(&mut self.error, e),
                    }
                }
            }
            Msg::Uploaded(key, r) => {
                if self.db.setting("stremio_auth").ok().flatten().as_deref() == Some(key.as_str()) {
                    self.account_busy = false;
                    match r {
                        Ok(note) => self.account_note = Some(note),
                        Err(e) => report(&mut self.error, e),
                    }
                }
            }
            Msg::Imported(imp) => {
                if let Err(e) = imp.save(&self.db) {
                    report(&mut self.error, format!("saving import: {e:#}"));
                    return;
                }
                self.addons = self.db.addons().unwrap_or_default();
                self.saved = self.db.saved_titles().unwrap_or_default();
                self.recent = self.db.recent_progress(20).unwrap_or_default();
                self.watched = self.db.watched(50).unwrap_or_default();
                self.watched_keys = self.db.watched_keys().unwrap_or_default();
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
                    self.db.set_setting("stremio_auth", &key)?;
                    self.db.set_setting("stremio_email", &email)
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
            Msg::DownloadProgress(path, run, got, total, validator) => {
                if let Some(j) = self.queue.iter_mut().find(|j| j.d.path == path && j.run == run && j.state == State::Active) {
                    if (j.total, &j.validator) != (total, &validator) {
                        (j.total, j.validator) = (total, validator);
                        if let Err(e) = self.db.set_download_meta(&j.d, &j.validator, j.total) {
                            report(&mut self.error, format!("saving download state: {e:#}"));
                        }
                    }
                    let dt = j.last.0.elapsed().as_secs_f64();
                    if dt > 0.0 {
                        let rate = got.saturating_sub(j.last.1) as f64 / dt;
                        j.speed = if j.speed == 0.0 { rate } else { j.speed * 0.5 + rate * 0.5 };
                    }
                    (j.got, j.total, j.last) = (got, total, (Instant::now(), got));
                }
            }
            Msg::Downloaded(path, run, res) => {
                let Some(i) = self.queue.iter().position(|j| j.d.path == path && j.run == run) else { return };
                self.queue[i].running = false;
                // A paused download's result only frees its slot; it was flushed, so Resume continues from the `.part` length.
                if self.queue[i].state != State::Active {
                    self.queue[i].got = std::fs::metadata(download::part(&path)).map_or(0, |m| m.len());
                    return self.pump_downloads(ctx);
                }
                match res {
                    // The `.part` stays so Resume can continue it.
                    Err(e) => {
                        self.queue[i].err = Some(e);
                        self.set_job_state(i, State::Failed);
                    }
                    Ok(()) => {
                        let d = self.queue.remove(i).d;
                        // A failed DB update leaves our finished file.
                        match self.db.add_download(&d).inspect_err(|_| drop(std::fs::remove_file(&d.path))) {
                            Ok(()) => self.downloads.insert(0, d),
                            Err(e) => report(&mut self.error, format!("download {}: {e:#}", d.title)),
                        }
                    }
                }
                self.pump_downloads(ctx);
            }
            Msg::Scanned(dir, res) => {
                let saved = res.map_err(anyhow::Error::msg).and_then(|items| {
                    self.db.add_library_folder(&dir, &items)?;
                    self.db.set_setting("last_folder", &dir.to_string_lossy())?;
                    Ok(items.len())
                });
                match saved {
                    Ok(_) => {
                        self.library = self.db.library_items().unwrap_or_default();
                    }
                    Err(e) => report(&mut self.error, e),
                }
            }
            Msg::Poster(url, bytes) => {
                self.posters.insert(url, Some(bytes.into()));
            }
            Msg::PickFile(key, title, source, subs, files, nav) => {
                if nav == self.nav {
                    let first = files[0].0;
                    self.file_pick = Some((key, title, source, subs, files, first));
                } else {
                    self.release_torrents(self.player.as_ref().and_then(|p| p.torrent));
                }
            }
            Msg::Play(key, title, source, url, subs, torrent, nav) => {
                if nav != self.nav {
                    // A torrent resolved after the user moved on: drop it, keeping only what is playing.
                    if torrent.is_some() {
                        self.release_torrents(self.player.as_ref().and_then(|p| p.torrent));
                    }
                    return;
                }
                self.stop_player();
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
                let setting = |k| self.db.setting(k).ok().flatten();
                let lang = |k| setting(k).unwrap_or_default();
                let subs_off = self.subs_off();
                let headers = self.selected.as_ref().map(|(_, streams, _)| request_headers(streams, &source, &url)).unwrap_or_default();
                let demuxer = self.selected.as_ref().and_then(|(_, streams, _)| own_stream(streams, &source, &url)).and_then(|s| s.hints.demuxer);
                // Remember the episode's series (its key holds only the video id) and the stream's binge group
                // (empty if none), so the series' play button and Continue Watching can find them later.
                if let (Some((m, streams, _)), Some(_)) = (&self.selected, &self.episode)
                    && self.play_key().is_some_and(|(k, _)| k == key)
                {
                    let group = streams.iter().find(|s| s.source().as_ref() == Some(&source)).and_then(|s| s.hints.binge_group.clone());
                    let _ = self.db.set_setting(&format!("series:{key}"), &m.id);
                    let _ = self.db.set_setting(&format!("binge:{}:{}", m.kind, m.id), &group.unwrap_or_default());
                }
                match Player::launch(&url, &title, start, &subs, [&lang("alang"), &lang("slang")], subs_off, &headers, demuxer, self.hwdec, gl_proc, Box::new(move || wake.request_repaint())) {
                    Ok(player) => {
                        let _ = player.apply_sub_style(&self.sub_style);
                        if self.default_speed != 1.0 {
                            let _ = player.command(&["set", "speed", &self.default_speed.to_string()]);
                        }
                        if self.volume != 100.0 {
                            let _ = player.command(&["set", "volume", &self.volume.to_string()]);
                        }
                        let p = Playing { player, key, title, source, url, torrent, pos: start, dur: 0.0, poster, osd_until: ctx.input(|i| i.time) + 3.0, menu: None, scrub: None, fast: false, hold: None, after_hold: false, wheel: 0.0, next_dismissed: false, aspect: 0, fill: false, zoom: 0, search: SubSearch::default(), skips, stats: false, net: Default::default(), net_next: 0.0, awake: None, hidden_vid: None };
                        self.player = Some(p);
                        // Force a fresh render so the previous video's last frame never flashes.
                        if let Some(v) = &mut self.video {
                            v.3 = [0, 0];
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
            self.recent = self.db.recent_progress(20).unwrap_or_default();
        }
    }

    fn set_watched(&mut self, key: &str, on: bool) {
        self.set_watched_many(&[key.to_string()], on);
    }

    fn set_watched_many(&mut self, keys: &[String], on: bool) {
        match self.db.set_watched_many(keys, on) {
            Ok(()) => {
                self.watched_keys = self.db.watched_keys().unwrap_or_default();
                self.watched = self.db.watched(50).unwrap_or_default();
                self.recent = self.db.recent_progress(20).unwrap_or_default();
            }
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

    fn stop_player(&mut self) {
        self.save_progress();
        self.player = None;
    }

    /// A media key or OS overlay button; ignored when nothing plays.
    fn media_event(&mut self, ctx: &egui::Context, e: souvlaki::MediaControlEvent) {
        let Some(p) = &mut self.player else { return };
        let res = match media_action(e) {
            Some(MediaAction::Cmd(args)) => p.player.command(args),
            Some(MediaAction::Seek(to, relative)) => p.player.seek(to, relative),
            Some(MediaAction::Next) => {
                if self.playing_episode() {
                    self.play_next(ctx);
                }
                return;
            }
            Some(MediaAction::Stop) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                self.stop_player();
                return self.release_torrents(None);
            }
            None => return,
        };
        if let Err(e) = res {
            report(&mut self.error, e);
        }
    }

    /// Publish the title, duration and play state to the OS media controls when they change.
    fn publish_media(&mut self) {
        let Some(mc) = &mut self.media else { return };
        let shown = self.player.as_ref().map(|p| (p.title.clone(), p.player.state.paused, p.player.state.dur as u64));
        if shown == self.media_shown {
            return;
        }
        // Errors are ignored: without a D-Bus session every call fails, and the failure was logged at startup.
        // shortcut: the position is sent only with these changes, so a seek leaves the OS's interpolated position stale until the next pause/play.
        let _ = match &shown {
            None => mc.set_playback(souvlaki::MediaPlayback::Stopped),
            Some((title, paused, dur)) => {
                let duration = (*dur > 0).then(|| Duration::from_secs(*dur));
                let _ = mc.set_metadata(souvlaki::MediaMetadata { title: Some(title), duration, ..Default::default() });
                let pos = self.player.as_ref().map_or(0.0, |p| p.player.state.pos);
                let progress = Duration::try_from_secs_f64(pos).ok().map(souvlaki::MediaPosition);
                mc.set_playback(if *paused { souvlaki::MediaPlayback::Paused { progress } } else { souvlaki::MediaPlayback::Playing { progress } })
            }
        };
        self.media_shown = shown;
    }

    fn poll_player(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.player else { return };
        if !p.player.poll() || p.player.state.error.is_some() {
            if let Some(e) = p.player.state.error.clone() {
                report(&mut self.error, e);
            }
            self.stop_player();
            self.release_torrents(None);
            return;
        }
        let st = &p.player.state;
        if self.autoplay_next && at_end(st.pos, st.dur, st.paused) && self.playing_episode() {
            return self.play_next(ctx);
        }
        let Some(p) = &mut self.player else { return };
        if self.last_poll.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_poll = Instant::now();
        let paused = p.player.state.paused;
        if paused {
            if p.awake.is_some() {
                release_awake(p.awake.take());
            }
        } else if p.awake.is_none() {
            // Not retried until the next pause/play, so a missing service never blocks the UI thread repeatedly.
            p.awake = Some(keep_awake());
        }
        if let Some((pos, dur)) = p.player.position() {
            (p.pos, p.dur) = (pos, dur);
            self.save_progress();
        }
    }

    /// Nav pill position: eases (cubic out) from where it is to the selected tab.
    fn pill_rect(&mut self, ctx: &egui::Context, target: egui::Rect) -> egui::Rect {
        const DURATION: f64 = 0.35;
        let now = ctx.input(|i| i.time);
        let (tab, from, to, start) = self.pill.get_or_insert((self.tab, target, target, now));
        if *tab != self.tab {
            // Restart from the pill's current spot, so a mid-slide click doesn't jump.
            let t = ((now - *start) / DURATION).clamp(0.0, 1.0) as f32;
            *from = from.lerp_towards(to, egui::emath::easing::cubic_out(t));
            (*tab, *start) = (self.tab, now);
        }
        *to = target; // follows small layout shifts (bold text) without restarting
        let t = ((now - *start) / DURATION).clamp(0.0, 1.0) as f32;
        if t < 1.0 {
            ctx.request_repaint();
        }
        from.lerp_towards(to, egui::emath::easing::cubic_out(t))
    }

    /// Rebuild home rows from installed catalogs: generic Trending/Popular rows first, then the rest.
    fn build_rows(&mut self, ctx: &egui::Context) {
        self.new_eps_gen += 1;
        self.new_episodes.clear();
        self.upcoming.clear();
        let cats: Vec<(&String, &String, &CatalogDef)> = self
            .addons
            .iter()
            .filter(|(_, m)| m.has_resource("catalog"))
            .flat_map(|(u, m)| m.catalogs.iter().filter(|c| !c.requires_extra()).map(move |c| (u, &m.name, c)))
            .collect();
        let text = |c: &CatalogDef| format!("{} {}", c.id, c.name.as_deref().unwrap_or("")).to_lowercase();
        // Cinemeta names its popular catalog "top".
        let matches = |c: &CatalogDef, word: &str| text(c).contains(word) || (word == "popular" && c.id == "top");
        let generic = [
            ("Movies - Trending", "movie", "trending"),
            ("Shows - Trending", "series", "trending"),
            ("Movies - Popular", "movie", "popular"),
            ("Shows - Popular", "series", "popular"),
        ];
        let (mut rows, mut used) = (Vec::new(), Vec::new());
        for (title, kind, word) in generic {
            if let Some(i) = (0..cats.len()).find(|&i| !used.contains(&i) && cats[i].2.kind == kind && matches(cats[i].2, word)) {
                used.push(i);
                rows.push((title.to_string(), i));
            }
        }
        for i in (0..cats.len()).filter(|i| !used.contains(i)) {
            let (_, addon, c) = cats[i];
            let (name, kind) = (c.name.as_deref().unwrap_or(&c.id), kind_label(&c.kind));
            let suffix = if name.to_lowercase().contains(&kind.to_lowercase()) { String::new() } else { format!(" ({kind})") };
            rows.push((format!("{addon} - {name}{suffix}"), i));
        }
        let rows: Vec<Row> = rows
            .into_iter()
            .map(|(title, i)| {
                let (u, _, c) = cats[i];
                Row { title, src: (u.clone(), c.kind.clone(), c.id.clone()), items: None, skippable: c.supports("skip"), fetched: 0, loading_more: false, exhausted: false }
            })
            .collect();
        let keys: Vec<String> = rows.iter().map(|r| row_key(&r.src)).collect();
        let mut rows: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
        let ordered: Vec<Row> = arrange_rows(&keys, &self.home_order).into_iter().filter_map(|i| rows[i].take()).collect();
        self.home_all = ordered.iter().map(|r| (row_key(&r.src), r.title.clone())).collect();
        self.rows = ordered.into_iter().filter(|r| !self.home_hidden.contains(&row_key(&r.src))).collect();
        for src in self.rows.iter().map(|r| r.src.clone()).collect::<Vec<_>>() {
            let client = self.client.clone();
            self.spawn(ctx, async move {
                let res = client.catalog(&src.0, &src.1, &src.2, 0).await.map_err(|e| format!("{e:#}"));
                Ok(Msg::Row(src, res))
            });
        }
        self.fetch_new_episodes(ctx);
    }

    /// Ask a meta addon for the episodes of each saved series and keep those released recently.
    fn fetch_new_episodes(&self, ctx: &egui::Context) {
        // At most 4 meta requests in flight, so a long library doesn't flood the addons.
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        for meta in self.saved.iter().filter(|t| t.kind == "series").filter_map(SavedTitle::preview) {
            let addons = self.addons_for("meta", &meta.kind, &meta.id);
            let (client, tx, ctx, epoch, slots) = (self.client.clone(), self.tx.clone(), ctx.clone(), self.new_eps_gen, slots.clone());
            // Not `spawn`: a series whose meta fails to load just has no row entry.
            self.rt.spawn(async move {
                let Ok(_permit) = slots.acquire_owned().await else { return };
                for u in &addons {
                    // Like `open_meta`: an addon without episodes doesn't end the search.
                    if let Ok(m) = client.meta(u, &meta.kind, &meta.id).await
                        && !m.videos.is_empty()
                    {
                        let _ = tx.send(Msg::NewEpisodes(epoch, meta, m.videos));
                        ctx.request_repaint();
                        return;
                    }
                }
            });
        }
    }

    fn remember_search(&mut self, query: &str) {
        db::push_history(&mut self.search_history, query);
        self.save_search_history();
    }

    fn save_search_history(&mut self) {
        let r = if self.search_history.is_empty() { self.db.remove_setting("search_history") } else { self.db.set_setting("search_history", &db::encode_history(&self.search_history)) };
        if let Err(e) = r {
            report(&mut self.error, e);
        }
    }

    fn search(&mut self, ctx: &egui::Context) {
        let query = self.search.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.remember_search(&query);
        let targets: Vec<(String, String, String)> = self
            .addons
            .iter()
            .filter(|(_, m)| m.has_resource("catalog"))
            .flat_map(|(u, m)| m.catalogs.iter().filter(|c| c.searchable()).map(move |c| (u.clone(), c.kind.clone(), c.id.clone())))
            .collect();
        if targets.is_empty() {
            report(&mut self.error, "no installed addon supports search");
            return;
        }
        let client = self.client.clone();
        self.spawn(ctx, async move {
            let (mut items, mut errors) = (Vec::new(), Vec::new());
            let n = targets.len();
            let ask = |(u, kind, id): (String, String, String)| {
                let (client, query) = (client.clone(), query.clone());
                async move { client.search(&u, &kind, &id, &query).await }
            };
            for res in concurrently(targets, ask).await {
                match res {
                    Ok(ms) => items.extend(ms),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            if errors.len() == n {
                anyhow::bail!("search failed: {}", errors.join("; "));
            }
            Ok(Msg::Catalog(format!("Results for \"{query}\""), items))
        });
    }

    fn fetch_poster(&mut self, ctx: &egui::Context, url: String) {
        if self.posters.len() >= POSTER_CACHE {
            // shortcut: drops every poster at once, not the least recently used; on-screen ones reload next frame.
            self.posters.clear();
            ctx.forget_all_images();
        }
        self.posters.insert(url.clone(), None);
        let client = self.client.clone();
        self.spawn(ctx, async move {
            let bytes = client.poster(&url).await.unwrap_or_default();
            Ok(Msg::Poster(url, bytes))
        });
    }

    /// URLs of installed addons serving `res` for this type and id.
    fn addons_for(&self, res: &str, kind: &str, id: &str) -> Vec<String> {
        let matches = |m: &Manifest| {
            m.has_resource(res) && m.types.iter().any(|t| t == kind) && m.id_prefixes.as_ref().is_none_or(|ps| ps.iter().any(|p| id.starts_with(p.as_str())))
        };
        self.addons.iter().filter(|(_, m)| matches(m)).map(|(u, _)| u.clone()).collect()
    }

    /// Fetch a movie's full meta for the detail page from the first addon that serves it.
    fn fetch_details(&mut self, ctx: &egui::Context, meta: &MetaPreview) {
        let addons = self.addons_for("meta", &meta.kind, &meta.id);
        let (client, tx, ctx) = (self.client.clone(), self.tx.clone(), ctx.clone());
        let (kind, id) = (meta.kind.clone(), meta.id.clone());
        self.details_want = (kind.clone(), id.clone());
        // Not `spawn`: the page works without details, so a failure is not worth an error.
        self.rt.spawn(async move {
            for u in &addons {
                if let Ok(m) = client.meta(u, &kind, &id).await {
                    let _ = tx.send(Msg::Details(m));
                    ctx.request_repaint();
                    return;
                }
            }
        });
    }

    /// Full meta of the selected title, once loaded.
    fn selected_details(&self) -> Option<&Meta> {
        let (m, ..) = self.selected.as_ref()?;
        self.details.as_ref().filter(|d| d.id == m.id && d.kind == m.kind)
    }

    /// Look up posters for Continue Watching entries saved without one (older history).
    /// Torrent/file keys never match an addon type, so they are skipped.
    fn fetch_recent_posters(&self, ctx: &egui::Context) {
        for p in self.recent.iter().filter(|p| p.poster.is_none()) {
            let Some((kind, id)) = p.key.split_once(':') else { continue };
            let addons = self.addons_for("meta", kind, id);
            if addons.is_empty() {
                continue;
            }
            let (client, tx, ctx) = (self.client.clone(), self.tx.clone(), ctx.clone());
            let (key, kind, id) = (p.key.clone(), kind.to_owned(), id.to_owned());
            // Not `spawn`: a missing poster is not worth an error.
            self.rt.spawn(async move {
                for u in &addons {
                    if let Ok(m) = client.meta(u, &kind, &id).await
                        && let Some(poster) = m.poster
                    {
                        let _ = tx.send(Msg::RecentPoster(key, poster));
                        ctx.request_repaint();
                        return;
                    }
                }
            });
        }
    }

    /// Store year and rating on a saved title once its details are known (imports and older rows lack them).
    fn backfill_saved_facts(&mut self, d: &Meta) {
        let key = format!("{}:{}", d.kind, d.id);
        let (year, rating) = saved_facts(d);
        if let Some(t) = self.saved.iter_mut().find(|t| t.key == key && (year.is_some_and(|y| t.year != Some(y)) || rating.is_some_and(|r| t.rating != Some(r)))) {
            (t.year, t.rating) = (year.or(t.year), rating.or(t.rating));
            if let Err(e) = self.db.set_saved_facts(&key, year, rating) {
                report(&mut self.error, format!("saving library: {e:#}"));
            }
        }
    }

    /// Add the selected title to the library, or remove it.
    fn toggle_saved(&mut self, meta: &MetaPreview) {
        let key = format!("{}:{}", meta.kind, meta.id);
        let res = if self.saved.iter().any(|t| t.key == key) {
            self.saved.retain(|t| t.key != key);
            self.lists.iter_mut().for_each(|l| _ = l.keys.remove(&key));
            self.db.remove_saved_title(&key)
        } else {
            let t = SavedTitle { key, title: meta.name.clone(), kind: meta.kind.clone(), poster: meta.poster.clone(), year: None, rating: None, last_watched: None };
            let t = match self.details.as_ref().filter(|d| d.kind == meta.kind && d.id == meta.id).map(saved_facts) {
                Some((year, rating)) => SavedTitle { year, rating, ..t },
                None => t,
            };
            self.saved.insert(0, t.clone());
            self.db.add_saved_title(&t)
        };
        if let Err(e) = res {
            report(&mut self.error, format!("saving library: {e:#}"));
        }
    }

    /// Re-read lists (and saved titles, which joining a list can add) after a list change; `res` errors are shown.
    fn lists_changed(&mut self, res: anyhow::Result<()>) {
        if let Err(e) = res {
            report(&mut self.error, format!("saving list: {e:#}"));
        }
        self.saved = self.db.saved_titles().unwrap_or_default();
        self.lists = self.db.lists().unwrap_or_default();
        if !self.lists.iter().any(|l| Some(l.id) == self.lib_list) {
            self.lib_list = None;
        }
    }

    /// "Add to list" menu: a checkbox per list, plus a new list that gets `meta`.
    fn list_menu(&mut self, ui: &mut egui::Ui, meta: &MetaPreview) {
        let t = SavedTitle { key: format!("{}:{}", meta.kind, meta.id), title: meta.name.clone(), kind: meta.kind.clone(), poster: meta.poster.clone(), year: None, rating: None, last_watched: None };
        ui.menu_button(format!("{}  Add to list", icons::PLUS), |ui| {
            let mut change = None;
            for l in &self.lists {
                let mut on = l.keys.contains(&t.key);
                if ui.checkbox(&mut on, &l.name).changed() {
                    change = Some((l.id, on));
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.list_name).hint_text("New list…").char_limit(60).desired_width(140.0));
                if ui.button("Create").clicked() {
                    match self.db.create_list(&self.list_name) {
                        Ok(id) => {
                            self.list_name.clear();
                            change = Some((id, true));
                        }
                        Err(e) => report(&mut self.error, format!("creating list: {e:#}")),
                    }
                }
            });
            if let Some((id, on)) = change {
                let res = self.db.set_in_list(id, &t, on);
                self.lists_changed(res);
            }
        });
    }

    /// Progress key and player title of the selected movie or episode.
    /// Episodes are keyed `<type>:<video id>` (e.g. `series:tt1:1:2`), so each episode resumes on its own.
    fn play_key(&self) -> Option<(String, String)> {
        let (m, ..) = self.selected.as_ref()?;
        Some(match &self.episode {
            Some(v) => {
                let se = format!("{} S{}E{}", m.name, v.season.unwrap_or(0), v.episode.unwrap_or(0));
                (format!("{}:{}", m.kind, v.id), v.title.as_ref().map_or(se.clone(), |t| format!("{se} - {t}")))
            }
            None => (format!("{}:{}", m.kind, m.id), m.name.clone()),
        })
    }

    /// Whether the player shows the selected series episode (not e.g. a Continue Watching entry).
    fn playing_episode(&self) -> bool {
        self.episode.is_some() && self.player.as_ref().zip(self.play_key()).is_some_and(|(p, (k, _))| p.key == k)
    }

    /// Stop the playing episode and open the next one, autoplaying a stream from the same `bingeGroup`
    /// (else the only stream, else the picker). Without a next episode, just stop.
    fn play_next(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.player else { return };
        if let Some((pos, dur)) = p.player.position() {
            (p.pos, p.dur) = (pos, dur);
        }
        let src = Some(p.source.clone());
        let binge = (self.selected.as_ref()).and_then(|(_, streams, _)| streams.iter().find(|s| s.source() == src)?.hints.binge_group.clone());
        let next = (self.selected.as_ref().zip(self.episode.as_ref()))
            .and_then(|((m, ..), cur)| Some((m.clone(), next_episode(&self.episodes, &cur.id)?.clone())));
        self.stop_player();
        match next {
            Some((m, v)) => self.open_streams(ctx, m, Some(v), Autoplay::Only, binge),
            None => {
                self.release_torrents(None);
            }
        }
    }

    /// Series load their episode list; anything else loads streams for its own id.
    fn open_meta(&mut self, ctx: &egui::Context, meta: MetaPreview, autoplay: bool) {
        // Opening any title cancels a play/show request still waiting for episodes.
        self.pending = None;
        if meta.kind != "series" {
            self.fetch_details(ctx, &meta);
            return self.open_streams(ctx, meta, None, if autoplay { Autoplay::Only } else { Autoplay::Off }, None);
        }
        let client = self.client.clone();
        let addons = self.addons_for("meta", &meta.kind, &meta.id);
        self.clear_file_pick();
        self.nav += 1;
        let nav = self.nav;
        self.spawn(ctx, async move {
            let mut errors = Vec::new();
            for u in &addons {
                match client.meta(u, &meta.kind, &meta.id).await {
                    Ok(m) if !m.videos.is_empty() => return Ok(Msg::Episodes(meta, m, nav)),
                    Ok(_) => {}
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            let why = if errors.is_empty() { "no addon lists them".to_string() } else { errors.join("; ") };
            anyhow::bail!("no episodes for {}: {why}", meta.name)
        });
    }

    /// Card play button: resume the title (or a series' episode in progress) with its last stream,
    /// else play the top stream of the movie or of the series' next episode (its last binge group first).
    fn quick_play(&mut self, ctx: &egui::Context, meta: MetaPreview) {
        let key = format!("{}:{}", meta.kind, meta.id);
        if let Some(p) = self.recent.iter().find(|p| p.key == key).cloned() {
            return self.play(ctx, p.key, p.title, p.source, vec![]);
        }
        self.tab = Tab::Home;
        if meta.kind == "series" {
            let id = meta.id.clone();
            self.open_meta(ctx, meta, false);
            self.pending = Some((id, None));
            return;
        }
        self.fetch_details(ctx, &meta);
        self.open_streams(ctx, meta, None, Autoplay::Top, None);
    }

    /// `stremio://` link: prefill an addon for the user to confirm with Install, or open a title's page.
    fn open_link(&mut self, ctx: &egui::Context, link: DeepLink) {
        match link {
            DeepLink::Addon(url) => {
                // Invalidate in-flight play/streams/episodes replies so they can't act over the Addons tab.
                self.clear_file_pick();
                (self.pending, self.nav) = (None, self.nav + 1);
                (self.tab, self.addon_url) = (Tab::Addons, url.into());
            }
            DeepLink::Detail { kind, id, video } => {
                self.tab = Tab::Home;
                let meta = MetaPreview { name: id.clone(), id, kind, poster: None, background: None, description: None };
                // Only an episode link opens a stream picker; `(id, None)` would autoplay the next episode.
                let pending = video.filter(|_| meta.kind == "series").map(|v| (meta.id.clone(), Some(v)));
                self.open_meta(ctx, meta, false);
                self.pending = pending;
            }
        }
    }

    /// Continue Watching card: the title's page with its stream picker (for a series, the episode's).
    /// Torrents and local files have no page, so they resume.
    fn open_progress(&mut self, ctx: &egui::Context, p: Progress) {
        let Some((kind, id)) = p.key.split_once(':').filter(|(k, _)| !matches!(*k, "btih" | "file")) else {
            return self.play(ctx, p.key, p.title, p.source, vec![]);
        };
        let mut meta = MetaPreview { id: id.into(), kind: kind.into(), name: p.title.clone(), poster: p.poster.clone(), background: None, description: None };
        if kind == "series" {
            let saved = self.db.setting(&format!("series:{}", p.key)).ok().flatten();
            meta.id = saved.unwrap_or_else(|| series_id(id).into());
        }
        let pending = (kind == "series").then(|| (meta.id.clone(), Some(id.to_owned())));
        self.open_meta(ctx, meta, false);
        self.pending = pending;
    }

    /// Fetch streams and subtitles for a movie, or for `episode` of a series.
    /// `binge`: on arrival, autoplay the stream in this binge group if there is one.
    fn open_streams(&mut self, ctx: &egui::Context, meta: MetaPreview, episode: Option<Video>, autoplay: Autoplay, binge: Option<String>) {
        let client = self.client.clone();
        let id = episode.as_ref().map_or(meta.id.clone(), |v| v.id.clone());
        let stream_addons = self.addons_for("stream", &meta.kind, &id);
        let names: Vec<String> = stream_addons.iter().map(|u| self.addons.iter().find(|(x, _)| x == u).map(|(_, m)| m.name.clone()).unwrap_or_default()).collect();
        let sub_addons = self.addons_for("subtitles", &meta.kind, &id);
        self.clear_file_pick();
        self.nav += 1;
        let nav = self.nav;
        self.spawn(ctx, async move {
            let (mut streams, mut subs, mut errors) = (Vec::new(), Vec::new(), Vec::new());
            let (kind, vid) = (meta.kind.clone(), id.clone());
            let ask_streams = |u: String| {
                let (c, k, i) = (client.clone(), kind.clone(), vid.clone());
                async move { c.streams(&u, &k, &i).await }
            };
            let ask_subs = |u: String| {
                let (c, k, i) = (client.clone(), kind.clone(), vid.clone());
                async move { c.subtitles(&u, &k, &i, &[]).await }
            };
            let (stream_res, sub_res) = tokio::join!(concurrently(stream_addons, ask_streams), concurrently(sub_addons, ask_subs));
            // Addon priority order, so the picker groups streams by addon.
            for (res, name) in stream_res.into_iter().zip(names) {
                match res {
                    Ok(s) => streams.extend(s.into_iter().map(|s| Stream { addon: name.clone(), ..s })),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            // Subtitles are optional; a failing subtitle addon must not block playback.
            for s in sub_res.into_iter().flatten() {
                subs.extend(s.into_iter().map(|s| (s.url, s.lang)));
            }
            if streams.is_empty() && !errors.is_empty() {
                anyhow::bail!("no streams: {}", errors.join("; "));
            }
            Ok(Msg::Streams(meta, episode, streams, subs, autoplay, binge, nav))
        });
    }

    /// Poster/background bytes if loaded; queues a fetch on first sight.
    fn image(&self, url: &Option<String>, want: &mut Vec<String>) -> Option<(String, Arc<[u8]>)> {
        let u = url.as_ref()?;
        match self.posters.get(u) {
            Some(Some(b)) if !b.is_empty() => Some((u.clone(), b.clone())),
            None => {
                want.push(u.clone());
                None
            }
            _ => None,
        }
    }

    fn ui_home(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (mut want, mut open, mut play, mut quick, mut resume) = (Vec::new(), None, None, None, None);
        let mut remove: Option<String> = None;
        let featured = match &self.selected {
            Some((m, ..)) => Some(m.clone()),
            None => self.rows.iter().find_map(|r| r.items.as_ref()?.first().cloned()),
        };
        match featured {
            Some(meta) => self.ui_hero(ui, &meta, &mut want, &mut open, &mut play),
            None if self.rows.is_empty() => {
                ui.add_space(40.0);
                ui.label(RichText::new("Install an addon with a catalog in Addons to start browsing.").size(18.0));
            }
            None => {
                ui.add_space(40.0);
                ui.spinner();
            }
        }
        if let Some(d) = self.selected_details()
            && let Some(t) = Self::ui_details(ui, d)
            && let Some(src) = t.source()
        {
            // Not `play`: its `nav` bump would drop the title's still-loading streams and episodes.
            let key = format!("{TRAILER}{}", t.yt_id.clone().unwrap_or_default());
            let _ = self.tx.send(Msg::Play(key, format!("{} - {}", d.name, t.title.as_deref().unwrap_or("Trailer")), src.clone(), src, vec![], None, self.nav));
        }
        let mut episode = None;
        let mut fresh_click: Option<(MetaPreview, String)> = None;
        let mut toggle: Option<(String, bool)> = None;
        let mut bulk: Option<(Vec<String>, bool)> = None;
        let mut save = None;
        // Movies show their streams; a series shows them once an episode is picked.
        if let Some((m, streams, subs)) = &self.selected
            && let Some(key) = self.play_key()
            && (self.episodes.is_empty() || self.episode.is_some())
        {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let r = ui.label(RichText::new("Streams").size(24.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
                if std::mem::take(&mut self.show_streams) {
                    r.scroll_to_me(Some(Align::TOP));
                }
                ui.label(RichText::new(format!("{}  ·  {} subtitle track{}", key.1, subs.len(), if subs.len() == 1 { "" } else { "s" })).size(15.0).color(ui.visuals().weak_text_color()));
                let seen = self.watched_keys.contains(&key.0);
                if small_button(ui, if seen { "Mark unwatched" } else { "Mark watched" }).clicked() {
                    toggle = Some((key.0.clone(), !seen));
                }
            });
            ui.add_space(8.0);
            for (i, s) in streams.iter().enumerate() {
                if (i == 0 || streams[i - 1].addon != s.addon) && !s.addon.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new(&s.addon).size(17.0).family(bold()).color(ui.visuals().weak_text_color()));
                }
                // YouTube needs yt-dlp, which only mpv runs; a playlist GET would save only the manifest.
                let src = s.source().filter(|src| !stremflix_rs::addon::is_youtube(src) && !is_playlist(src));
                let r = stream_row(ui, i + 1, s, if src.is_some() { EPISODE_BUTTON.x + 12.0 } else { 0.0 });
                if let Some(src) = src {
                    // Overlaid like the episode rows' "Mark watched".
                    let b = egui::Rect::from_min_size(pos2(r.rect.right() - 80.0 - EPISODE_BUTTON.x, r.rect.center().y - EPISODE_BUTTON.y / 2.0), EPISODE_BUTTON);
                    let dl = ui.scope_builder(UiBuilder::new().max_rect(b).layout(Layout::centered_and_justified(egui::Direction::LeftToRight)), |ui| small_button(ui, "Download")).inner;
                    ui.advance_cursor_after_rect(r.rect);
                    if dl.clicked() {
                        let video_id = self.episode.as_ref().map_or(&m.id, |v| &v.id).clone();
                        let d = Download { path: PathBuf::new(), kind: m.kind.clone(), meta_id: m.id.clone(), video_id, title: key.1.clone() };
                        let url = match s.target() { Some(PlayTarget::Http(u)) => Some(u), _ => None };
                        let hint = s.hints.filename.clone().or_else(|| url.as_ref().map(|u| u.path().to_string()));
                        let headers = if url.is_some() { s.hints.request_headers.clone() } else { vec![] };
                        save = Some((d, src, headers, hint));
                        continue;
                    }
                }
                if r.clicked() {
                    match (s.source(), &s.external_url) {
                        (Some(src), _) => play = Some((key.0.clone(), key.1.clone(), src, subs.clone())),
                        // `external_url` is http(s) only (checked in `parse_streams`).
                        (None, Some(u)) => ctx.open_url(egui::OpenUrl::new_tab(u)),
                        (None, None) => {}
                    }
                }
            }
        }
        if let Some((meta, ..)) = &self.selected
            && !self.episodes.is_empty()
        {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let r = ui.label(RichText::new("Episodes").size(24.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
                if self.episode.is_none() && std::mem::take(&mut self.show_streams) {
                    r.scroll_to_me(Some(Align::TOP));
                }
                let mut seasons: Vec<Option<u32>> = self.episodes.iter().map(|v| v.season).collect();
                seasons.dedup();
                egui::ComboBox::from_id_salt("season").selected_text(season_label(self.season)).show_ui(ui, |ui| {
                    for s in seasons {
                        ui.selectable_value(&mut self.season, s, season_label(s));
                    }
                });
                for (scope, season) in [("season", Some(self.season)), ("series", None)] {
                    let keys: Vec<String> = self.episodes.iter().filter(|v| season.is_none_or(|s| v.season == s)).map(|v| format!("{}:{}", meta.kind, v.id)).collect();
                    let all = keys.iter().all(|k| self.watched_keys.contains(k));
                    if small_button(ui, &format!("Mark {scope} {}", if all { "unwatched" } else { "watched" })).clicked() {
                        bulk = Some((keys, !all));
                    }
                }
            });
            ui.add_space(8.0);
            for v in self.episodes.iter().filter(|v| v.season == self.season) {
                let k = format!("{}:{}", meta.kind, v.id);
                let seen = self.watched_keys.contains(&k);
                let r = episode_row(ui, v, seen);
                if self.episode.as_ref() == Some(v) {
                    // Same inset and radius as the row's hover fill.
                    ui.painter().rect_stroke(r.rect.shrink(4.0), 8, (2.0, RED), egui::StrokeKind::Inside);
                }
                // Overlaid left of the play circle; the topmost widget wins the click.
                let b = egui::Rect::from_min_size(pos2(r.rect.right() - 80.0 - EPISODE_BUTTON.x, r.rect.center().y - EPISODE_BUTTON.y / 2.0), EPISODE_BUTTON);
                let label = if seen { "Mark unwatched" } else { "Mark watched" };
                let mark = ui.scope_builder(UiBuilder::new().max_rect(b).layout(Layout::centered_and_justified(egui::Direction::LeftToRight)), |ui| small_button(ui, label)).inner;
                // The overlay moved the cursor to the button's bottom; put it back under the row.
                ui.advance_cursor_after_rect(r.rect);
                if mark.clicked() {
                    toggle = Some((k, !seen));
                } else if r.clicked() {
                    episode = Some((meta.clone(), v.clone()));
                }
            }
        }
        if !self.recent.is_empty() {
            Self::row_title(ui, "Continue Watching");
            egui::ScrollArea::horizontal().id_salt("recent").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for p in &self.recent {
                        let (r, rect) = card(ui, CARD, &p.title);
                        self.paint_poster(ui, &r, rect, &p.poster, &p.title, &mut want);
                        // Unknown duration: no total and no progress bar.
                        let at = if p.duration > 0.0 { format!("{} / {}", fmt_time(p.position), fmt_time(p.duration)) } else { fmt_time(p.position) };
                        let r = r.on_hover_text(format!("{} ({at})", p.title));
                        if p.duration > 0.0 {
                            let frac = (p.position / p.duration).clamp(0.0, 1.0) as f32;
                            let bar = egui::Rect::from_min_max(pos2(rect.left() + 10.0, rect.bottom() - 10.0), pos2(rect.right() - 10.0, rect.bottom() - 6.0));
                            ui.painter().rect_filled(bar, 2, Color32::from_gray(90));
                            ui.painter().rect_filled(egui::Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())), 2, RED);
                        }
                        // Corner X, shown like the play button; registered after the card so it takes the click.
                        let x = egui::Rect::from_center_size(rect.right_top() + vec2(-18.0, 18.0), vec2(28.0, 28.0));
                        let xr = ui.interact(x, r.id.with("remove"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Remove from Continue Watching");
                        xr.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Remove {} from Continue Watching", p.title)));
                        if r.contains_pointer() || xr.has_focus() {
                            ui.painter().circle_filled(x.center(), 14.0, Color32::from_black_alpha(if xr.hovered() { 210 } else { 140 }));
                            ui.painter().text(x.center(), egui::Align2::CENTER_CENTER, icons::X, egui::FontId::new(16.0, phosphor()), Color32::WHITE);
                        }
                        if xr.clicked() {
                            remove = Some(p.key.clone());
                        } else if card_play(ui, &r, rect, &p.title) {
                            play = Some((p.key.clone(), p.title.clone(), p.source.clone(), vec![]));
                        } else if r.clicked() {
                            resume = Some(p.clone());
                        }
                    }
                });
            });
        }
        let mut fresh: Vec<(&MetaPreview, &Video)> = self
            .new_episodes
            .iter()
            .filter(|(m, _)| self.saved.iter().any(|t| t.key == format!("series:{}", m.id)))
            .filter_map(|(m, vs)| vs.iter().filter(|v| !self.watched_keys.contains(&format!("{}:{}", m.kind, v.id))).max_by_key(|v| (v.released.as_deref().and_then(|d| d.get(..10)), v.season, v.episode)).map(|v| (m, v)))
            .collect();
        fresh.sort_by(|a, b| b.1.released.cmp(&a.1.released));
        if !fresh.is_empty() {
            Self::row_title(ui, "New Episodes");
            egui::ScrollArea::horizontal().id_salt("new_episodes").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (m, v) in fresh {
                        let label = format!("{} S{}E{}", m.name, v.season.unwrap_or(0), v.episode.unwrap_or(0));
                        let (r, rect) = card(ui, CARD, &label);
                        self.paint_poster(ui, &r, rect, &m.poster, &m.name, &mut want);
                        if r.on_hover_text(label).clicked() {
                            fresh_click = Some((m.clone(), v.id.clone()));
                        }
                    }
                });
            });
        }
        if !self.watched.is_empty() {
            Self::row_title(ui, "Watch It Again");
            egui::ScrollArea::horizontal().id_salt("watched").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for w in &self.watched {
                        let (r, rect) = card(ui, CARD, &w.title);
                        self.paint_poster(ui, &r, rect, &w.poster, &w.title, &mut want);
                        let meta = w.key.split_once(':').map(|(kind, id)| MetaPreview { id: id.into(), kind: kind.into(), name: w.title.clone(), poster: w.poster.clone(), background: None, description: None });
                        if card_play(ui, &r, rect, &w.title) {
                            quick = meta.clone();
                        }
                        let hint = if w.episodes > 0 { format!("{} · {} episodes watched", w.title, w.episodes) } else { format!("{} · Watched", w.title) };
                        if r.on_hover_text(hint).clicked()
                            && let Some(meta) = meta
                        {
                            open = Some((meta, false));
                        }
                    }
                });
            });
        }
        let mut clear = false;
        if let Some((title, items)) = &self.search_results {
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
                clear = small_button(ui, "Clear").clicked();
            });
            if items.is_empty() {
                ui.label("No results.");
            }
            self.ui_cards(ui, "search", items, &mut want, &mut open, &mut quick);
        }
        if clear {
            self.search_results = None;
        }
        let mut more = Vec::new();
        for row in &self.rows {
            match &row.items {
                Some(items) if items.is_empty() => continue,
                Some(items) => {
                    Self::row_title(ui, &row.title);
                    let near_end = self.ui_cards(ui, &row.title, items, &mut want, &mut open, &mut quick);
                    if row.wants_more(near_end) {
                        more.push((row.src.clone(), row.fetched));
                    }
                }
                None => {
                    Self::row_title(ui, &row.title);
                    ui.spinner();
                }
            }
        }
        for (src, skip) in more {
            if let Some(row) = self.rows.iter_mut().find(|r| r.src == src) {
                row.loading_more = true;
            }
            let client = self.client.clone();
            self.spawn(&ctx, async move {
                let res = client.catalog(&src.0, &src.1, &src.2, skip).await.map_err(|e| format!("{e:#}"));
                Ok(Msg::More(src, skip, res))
            });
        }
        want.sort();
        want.dedup();
        for u in want {
            self.fetch_poster(&ctx, u);
        }
        if let Some((k, on)) = toggle {
            self.set_watched(&k, on);
        }
        if let Some((keys, on)) = bulk {
            self.set_watched_many(&keys, on);
        }
        if let Some(k) = remove {
            match self.db.remove_progress(&k) {
                Ok(()) => self.recent = self.db.recent_progress(20).unwrap_or_default(),
                Err(e) => report(&mut self.error, format!("removing from Continue Watching: {e:#}")),
            }
        }
        if let Some((d, src, headers, hint)) = save {
            self.download(&ctx, d, src, headers, hint);
        }
        if let Some((m, autoplay)) = open {
            self.open_meta(&ctx, m, autoplay);
        }
        if let Some((m, v)) = episode {
            self.open_streams(&ctx, m, Some(v), Autoplay::Only, None);
        }
        // Load the series page first, so no other series' episode list stays under it.
        if let Some((m, id)) = fresh_click {
            let sid = m.id.clone();
            self.open_meta(&ctx, m, false);
            self.pending = Some((sid, Some(id)));
        }
        if let Some((k, t, s, subs)) = play {
            self.play(&ctx, k, t, s, subs);
        }
        if let Some(m) = quick {
            self.quick_play(&ctx, m);
        }
        if let Some(p) = resume {
            self.open_progress(&ctx, p);
        }
    }

    /// Pick a type, catalog and its extras (genre, year, sort, ...); the catalog's first page shows as a poster grid.
    fn ui_discover(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let cats: Vec<(&String, &String, &CatalogDef)> = self
            .addons
            .iter()
            .filter(|(_, m)| m.has_resource("catalog"))
            .flat_map(|(u, m)| m.catalogs.iter().filter(|c| c.discoverable()).map(move |c| (u, &m.name, c)))
            .collect();
        if cats.is_empty() {
            ui.add_space(40.0);
            ui.label(RichText::new("Install an addon with a catalog in Addons to start browsing.").size(18.0));
            return;
        }
        let d = &mut self.discover;
        let mut kinds: Vec<&str> = Vec::new();
        for (_, _, c) in &cats {
            if !kinds.contains(&c.kind.as_str()) {
                kinds.push(&c.kind);
            }
        }
        // Keep the pickers valid as addons change: fall back to the first type, catalog and required extras.
        if !kinds.contains(&d.kind.as_str()) {
            d.kind = kinds[0].into();
        }
        let in_kind: Vec<_> = cats.iter().filter(|(_, _, c)| c.kind == d.kind).collect();
        let cur = *in_kind.iter().find(|(u, _, c)| **u == d.cat.0 && c.id == d.cat.1).unwrap_or(&in_kind[0]);
        d.cat = (cur.0.clone(), cur.2.id.clone());
        d.extras = cur.2.valid_extras(&d.extras);
        let src = (d.cat.0.clone(), d.kind.clone(), d.cat.1.clone(), d.extras.clone());
        if d.src.as_ref() != Some(&src) {
            (d.src, d.items) = (Some(src.clone()), None);
            let client = self.client.clone();
            self.spawn(&ctx, async move {
                let res = client.discover(&src.0, &src.1, &src.2, &src.3).await.map_err(|e| format!("{e:#}"));
                Ok(Msg::Discover(src, res))
            });
        }
        // Picker changes apply on the next frame, after the checks above.
        let d = &mut self.discover;
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("discover_kind").selected_text(kind_label(&d.kind)).show_ui(ui, |ui| {
                for k in &kinds {
                    ui.selectable_value(&mut d.kind, k.to_string(), kind_label(k));
                }
            });
            let cat_label = |addon: &str, c: &CatalogDef| format!("{addon} - {}", c.name.as_deref().unwrap_or(&c.id));
            egui::ComboBox::from_id_salt("discover_cat").selected_text(cat_label(cur.1, cur.2)).show_ui(ui, |ui| {
                for (u, addon, c) in &in_kind {
                    ui.selectable_value(&mut d.cat, ((*u).clone(), c.id.clone()), cat_label(addon, c));
                }
            });
            for e in cur.2.pickers() {
                let at = d.extras.iter().position(|(n, _)| *n == e.name);
                let text = at.map_or_else(|| format!("All {}", e.name), |i| d.extras[i].1.clone());
                egui::ComboBox::from_id_salt(("discover_extra", &e.name)).selected_text(text).show_ui(ui, |ui| {
                    let mut pick = None;
                    if !cur.2.is_required(&e.name) && ui.selectable_label(at.is_none(), format!("All {}", e.name)).clicked() {
                        pick = Some(None);
                    }
                    for o in &e.options {
                        if ui.selectable_label(at.is_some_and(|i| d.extras[i].1 == *o), o).clicked() {
                            pick = Some(Some(o.clone()));
                        }
                    }
                    if let Some(p) = pick {
                        d.extras.retain(|(n, _)| *n != e.name);
                        d.extras.extend(p.map(|o| (e.name.clone(), o)));
                    }
                });
            }
        });
        ui.add_space(12.0);
        let (mut want, mut open, mut quick) = (Vec::new(), None, None);
        match &self.discover.items {
            None => {
                ui.spinner();
            }
            Some(items) if items.is_empty() => {
                ui.label("Nothing in this catalog.");
            }
            Some(items) => {
                ui.horizontal_wrapped(|ui| {
                    for m in items {
                        let (r, rect) = card(ui, CARD, &m.name);
                        self.paint_poster(ui, &r, rect, &m.poster, &m.name, &mut want);
                        if card_play(ui, &r, rect, &m.name) {
                            quick = Some(m.clone());
                        }
                        if r.on_hover_text(m.description.as_deref().unwrap_or(&m.name)).clicked() {
                            open = Some(m.clone());
                        }
                    }
                });
            }
        }
        want.sort();
        want.dedup();
        for u in want {
            self.fetch_poster(&ctx, u);
        }
        // The detail view lives on Home.
        if let Some(m) = open {
            self.tab = Tab::Home;
            self.open_meta(&ctx, m, false);
        }
        if let Some(m) = quick {
            self.quick_play(&ctx, m);
        }
    }

    fn row_title(ui: &mut egui::Ui, title: &str) {
        ui.add_space(22.0);
        ui.label(RichText::new(title).size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        ui.add_space(4.0);
    }

    /// Horizontally scrolling poster cards; a click selects the title.
    /// Returns true when one of the last 3 cards is on screen.
    fn ui_cards(&self, ui: &mut egui::Ui, salt: &str, items: &[MetaPreview], want: &mut Vec<String>, open: &mut Option<(MetaPreview, bool)>, quick: &mut Option<MetaPreview>) -> bool {
        let mut near_end = false;
        egui::ScrollArea::horizontal().id_salt(salt).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, m) in items.iter().enumerate() {
                    let (r, rect) = card(ui, CARD, &m.name);
                    // The scroll viewport clips, so a visible third-to-last card means near the end.
                    near_end |= i + 3 >= items.len() && ui.is_rect_visible(r.rect);
                    self.paint_poster(ui, &r, rect, &m.poster, &m.name, want);
                    if card_play(ui, &r, rect, &m.name) {
                        *quick = Some(m.clone());
                    }
                    if r.on_hover_text(m.description.as_deref().unwrap_or(&m.name)).clicked() {
                        *open = Some((m.clone(), false));
                    }
                }
            });
        });
        near_end
    }

    /// Poster image in `rect`, or the title on a gray card until it loads.
    fn paint_poster(&self, ui: &egui::Ui, r: &egui::Response, rect: egui::Rect, poster: &Option<String>, name: &str, want: &mut Vec<String>) {
        // Only fetch posters of cards that are on screen.
        let mut lazy = Vec::new();
        let img = if ui.is_rect_visible(r.rect) { self.image(poster, &mut lazy) } else { None };
        want.extend(lazy);
        match img {
            Some((u, b)) => egui::Image::from_bytes(format!("bytes://{u}"), b).alt_text(name).corner_radius(6).paint_at(ui, rect),
            None => card_text(ui, rect, name),
        }
    }

    /// Big banner: backdrop (or poster at the right), title, description, Play / More info.
    fn ui_hero(
        &mut self,
        ui: &mut egui::Ui,
        meta: &MetaPreview,
        want: &mut Vec<String>,
        open: &mut Option<(MetaPreview, bool)>,
        play: &mut Option<(String, String, String, Subs)>,
    ) {
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, (w * 0.42).clamp(320.0, 560.0)), Sense::hover());
        if std::mem::take(&mut self.scroll_top) {
            ui.scroll_to_rect(rect, Some(Align::TOP));
        }
        ui.painter().rect_filled(rect, 12, Color32::from_gray(28));
        let details = self.selected_details();
        let (background, logo) = (meta.background.clone().or_else(|| details?.background.clone()), details.and_then(|d| d.logo.clone()));
        // Background first; fall back to the poster when it is missing or rejected.
        let bg = self.image(&background, want);
        let poster = if bg.is_none() { self.image(&meta.poster, want) } else { None };
        if let Some((u, b)) = bg {
            let img = egui::Image::from_bytes(format!("bytes://{u}"), b).corner_radius(12);
            if let Ok(egui::load::TexturePoll::Ready { texture }) = img.load_for_size(ui.ctx(), rect.size()) {
                img.uv(cover_uv(rect, texture.size)).paint_at(ui, rect);
                ui.painter().rect_filled(rect, 12, Color32::from_black_alpha(120));
            }
        } else if let Some((u, b)) = poster {
            let pw = rect.height() * 2.0 / 3.0;
            let prect = egui::Rect::from_min_max(pos2(rect.right() - pw, rect.top()), rect.max);
            egui::Image::from_bytes(format!("bytes://{u}"), b).alt_text(&meta.name).corner_radius(12).paint_at(ui, prect);
        }
        let inner = rect.shrink(36.0);
        let col = egui::Rect::from_min_max(inner.min, pos2(inner.left() + (inner.width() * 0.55).min(620.0), inner.bottom()));
        ui.scope_builder(UiBuilder::new().max_rect(col).layout(Layout::bottom_up(Align::LEFT)), |ui| {
            ui.horizontal(|ui| {
                let mut show_streams = false;
                if hero_button(ui, &format!("{}  Play", icons::PLAY), [Color32::WHITE, Color32::from_white_alpha(191)], Color32::BLACK).clicked() {
                    match &self.selected {
                        Some((m, streams, subs)) if m == meta => match (only_source(streams), self.play_key()) {
                            (Some(src), Some((k, t))) => *play = Some((k, t, src, subs.clone())),
                            _ => show_streams = true,
                        },
                        _ => *open = Some((meta.clone(), true)),
                    }
                }
                self.show_streams |= show_streams;
                let gray = |a| Color32::from_rgba_unmultiplied(109, 109, 110, a);
                // The open title's page is already showing; reopening it would reload it and reset the season.
                let is_open = self.selected.as_ref().is_some_and(|(m, ..)| m == meta);
                if !is_open && hero_button(ui, "More info", [gray(179), gray(102)], Color32::WHITE).clicked() {
                    *open = Some((meta.clone(), false));
                }
                let in_lib = self.saved.iter().any(|t| t.key == format!("{}:{}", meta.kind, meta.id));
                let label = if in_lib { format!("{}  In library", icons::CHECK) } else { format!("{}  Add to library", icons::PLUS) };
                if hero_button(ui, &label, [gray(179), gray(102)], Color32::WHITE).clicked() {
                    self.toggle_saved(meta);
                }
                self.list_menu(ui, meta);
            });
            ui.add_space(12.0);
            if let Some(d) = &meta.description {
                let short: String = d.chars().take(240).collect();
                let short = if short.len() < d.len() { format!("{}…", short.trim_end()) } else { short };
                ui.label(RichText::new(short).size(17.0).color(Color32::WHITE));
            }
            ui.add_space(6.0);
            let kind = match meta.kind.as_str() {
                "movie" => "Movie",
                "series" => "Show",
                k => k,
            };
            ui.label(RichText::new(kind).size(15.0).strong().family(bold()).color(Color32::from_gray(220)));
            match self.image(&logo, want) {
                Some((u, b)) => ui.add(egui::Image::from_bytes(format!("bytes://{u}"), b).alt_text(&meta.name).max_height(120.0).max_width(col.width())),
                None => ui.add(egui::Label::new(RichText::new(&meta.name).size(46.0).strong().family(bold()).color(Color32::WHITE)).wrap()),
            };
        });
    }

    /// Detail facts under the banner: year, runtime, rating, genres, cast, director, links, trailers.
    /// Returns the trailer the user clicked.
    fn ui_details<'a>(ui: &mut egui::Ui, d: &'a Meta) -> Option<&'a Stream> {
        ui.add_space(14.0);
        let gray = ui.visuals().weak_text_color();
        let facts: Vec<String> = [d.release_info.clone(), d.runtime.clone(), d.imdb_rating.as_ref().map(|r| format!("IMDb {r}"))]
            .into_iter()
            .flatten()
            .chain((!d.genres.is_empty()).then(|| d.genres.join(", ")))
            .collect();
        if !facts.is_empty() {
            ui.label(RichText::new(facts.join("  ·  ")).size(16.0).color(ui.visuals().strong_text_color()));
        }
        for (name, list) in [("Cast", &d.cast), ("Director", &d.director)] {
            if !list.is_empty() {
                ui.add(egui::Label::new(RichText::new(format!("{name}: {}", list.join(", "))).size(15.0).color(gray)).wrap());
            }
        }
        if !d.links.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for l in &d.links {
                    // `links` holds only http(s) URLs (checked in `parse_meta`).
                    ui.hyperlink_to(&l.name, &l.url).on_hover_text(format!("{}: {}", l.category, l.url));
                }
            });
        }
        let mut clicked = None;
        if !d.trailers.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for t in &d.trailers {
                    if ui.button(format!("▶ {}", t.title.as_deref().unwrap_or("Trailer"))).clicked() {
                        clicked = Some(t);
                    }
                }
            });
        }
        clicked
    }

    /// Full-window Netflix-style player: letterboxed video, controls that fade out after
    /// 3 s without input, red seek bar, ±10 s skips, volume, Audio / Subtitles / Playback panels.
    fn ui_player(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let has_next = self.playing_episode() && self.episode.as_ref().is_some_and(|c| next_episode(&self.episodes, &c.id).is_some());
        let mut next = false;
        // Up-next card: series name, episode label, and its still (else the series background, else poster).
        let mut want = Vec::new();
        let up_next = (self.selected.as_ref().zip(self.episode.as_ref()))
            .filter(|_| has_next)
            .and_then(|((m, ..), cur)| Some((m, next_episode(&self.episodes, &cur.id)?)))
            .map(|(m, v)| {
                let title = v.title.clone().unwrap_or_else(|| format!("Episode {}", v.episode.unwrap_or(0)));
                let label = match (v.season, v.episode) {
                    (Some(s), Some(e)) => format!("{title} (S{s}E{e})"),
                    _ => title,
                };
                // First source whose fetch hasn't failed (failed ones are cached as empty bytes).
                let failed = |u: &String| matches!(self.posters.get(u), Some(Some(b)) if b.is_empty());
                let url = [v.thumbnail.clone(), m.background.clone(), self.selected_details().and_then(|d| d.background.clone()), m.poster.clone()].into_iter().flatten().find(|u| !failed(u));
                (m.name.clone(), label, self.image(&url, &mut want))
            });
        for u in want {
            self.fetch_poster(&ctx, u);
        }
        let Some(p) = &mut self.player else { return };
        let now = ctx.input(|i| i.time);
        let fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let mut cmds: Vec<Vec<String>> = Vec::new();
        let mut cmd = |a: &[&str]| cmds.push(a.iter().map(|s| s.to_string()).collect());
        let (mut back, mut toggle_fs, mut external) = (false, false, false);
        let mut seek: Option<(f64, bool)> = None;
        let (mut search_req, mut sub_act): (bool, Option<(bool, SubFound)>) = (false, None);
        let mut style = self.sub_style;
        ui.painter().rect_filled(rect, 0, Color32::BLACK);

        // Video: mpv renders on the GPU into our framebuffer at on-screen pixel size.
        let [vw, vh] = p.player.state.video;
        if let (true, Some(gl)) = (vw > 0 && vh > 0, frame.gl().cloned()) {
            let fit = (rect.width() / vw as f32).min(rect.height() / vh as f32);
            // Fill renders into the whole window so panscan has something to crop.
            let vrect = if p.fill { rect } else { egui::Rect::from_center_size(rect.center(), vec2(vw as f32, vh as f32) * fit) };
            let px = vrect.size() * ctx.pixels_per_point();
            let size = [(px.x as usize).max(1), (px.y as usize).max(1)];
            let video = match self.video {
                Some(v) => Some(v),
                None => unsafe {
                    match (gl.create_framebuffer(), gl.create_texture()) {
                        (Ok(fbo), Ok(tex)) => Some((fbo, tex, frame.register_native_glow_texture(tex), [0, 0])),
                        (a, b) => {
                            report(&mut self.error, format!("GL setup failed: {:?}", a.err().or(b.err())));
                            None
                        }
                    }
                },
            };
            if let Some((fbo, tex, id, cur)) = video {
                let resized = cur != size;
                unsafe {
                    if resized {
                        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        // The default min filter wants mipmaps; without them the texture samples black.
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                        gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, size[0] as i32, size[1] as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
                        gl.bind_texture(glow::TEXTURE_2D, None);
                    }
                    // egui leaves scissor/blend enabled; mpv draws assuming they are off.
                    gl.disable(glow::SCISSOR_TEST);
                    gl.disable(glow::BLEND);
                    p.player.render(fbo.0.get(), size, resized);
                    // Undo the GL state mpv leaves behind that egui's painter assumes is default.
                    gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, None);
                    gl.use_program(None);
                    gl.bind_vertex_array(None);
                    gl.bind_buffer(glow::ARRAY_BUFFER, None);
                    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                    gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                }
                self.video = Some((fbo, tex, id, size));
                ui.painter().image(id, vrect, egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
        }
        let st = p.player.state.clone();
        if st.buffering || vw == 0 {
            egui::Spinner::new().size(56.0).color(RED).paint_at(ui, egui::Rect::from_center_size(rect.center(), vec2(56.0, 56.0)));
        }

        // Click toggles pause, double-click fullscreen; registered first so controls on top win.
        let bg = ui.interact(rect, ui.scope_id().with("video"), Sense::CLICK);
        // Press and hold the video for 2x speed; the release halves it back and is not a click.
        let was_fast = p.fast;
        // Primary button only; losing focus or the pointer ends the hold in case the release went elsewhere.
        // Space holds the same way; a focused popover control keeps Space for itself.
        let space = ctx.memory(|m| m.focused()).is_none() && ctx.input(|i| i.focused && i.key_down(egui::Key::Space));
        let down = ctx.input(|i| i.pointer.primary_down() && i.focused && i.pointer.has_pointer()) || space;
        // Record the start on the press frame: egui drops the press from `bg` after 0.8 s or 6 px.
        let hold = p.hold.or_else(|| bg.is_pointer_button_down_on().then(|| ctx.input(|i| i.pointer.press_start_time())).flatten()).or(space.then_some(now));
        let long = hold.is_some_and(|t| now - t >= 0.5);
        p.hold = hold.filter(|_| down);
        p.fast = down && long;
        if p.fast != was_fast {
            // shortcut: multiply assumes nothing else changes speed mid-hold; save and restore `speed` once a speed control exists.
            cmd(&["multiply", "speed", if p.fast { "2" } else { "0.5" }]);
        }
        if down {
            ctx.request_repaint_after(Duration::from_millis(100)); // reach the threshold and see the release even when paused
        }
        if p.fast {
            let r = egui::Rect::from_center_size(pos2(rect.center().x, rect.top() + 130.0), vec2(64.0, 32.0));
            ui.painter().rect_filled(r, 16, Color32::from_black_alpha(160));
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, "2x", egui::FontId::new(17.0, bold()), Color32::WHITE);
        }
        if bg.clicked() && !long {
            if p.menu.is_some() { p.menu = None } else { cmd(&["cycle", "pause"]) }
        }
        toggle_fs |= bg.double_clicked() && !p.after_hold;
        if bg.clicked() {
            p.after_hold = long;
        }
        let focused = ctx.memory(|m| m.focused());
        let line = ctx.options(|o| o.input_options.line_scroll_speed); // read outside ctx.input to avoid nested locking
        let mut new_vol = None;
        ctx.input(|i| {
            use egui::Key::*;
            let k = |key| i.key_pressed(key);
            // Space activates a focused popover control, so it must not also toggle pause.
            // A short Space press pauses on release; a hold is 2x speed, like the mouse.
            if (i.key_released(Space) && focused.is_none() && !long) || k(K) { cmd(&["cycle", "pause"]) }
            let step = self.seek_steps[i.modifiers.shift as usize] as f64;
            if k(ArrowLeft) { seek = Some((-step, true)) }
            if k(ArrowRight) { seek = Some((step, true)) }
            if k(ArrowUp) { cmd(&["add", "volume", "5"]); new_vol = Some(st.volume + 5.0) }
            if k(ArrowDown) { cmd(&["add", "volume", "-5"]); new_vol = Some(st.volume - 5.0) }
            if k(M) { cmd(&["cycle", "mute"]) }
            if k(S) && focused.is_none() {
                let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let path = screenshot_path(&self.data_dir, &p.title, secs);
                let _ = path.parent().map(std::fs::create_dir_all);
                cmd(&["screenshot-to-file", &path.to_string_lossy(), "video"]);
            }
            if k(I) && focused.is_none() { p.stats = !p.stats }
            // mpv pauses on its own after a frame step.
            if k(Period) { cmd(&["frame-step"]) }
            if k(Comma) { cmd(&["frame-back-step"]) }
            // Wheel over the bare video only: `hovered` is false under a popover or a control.
            if bg.hovered() && p.menu.is_none() {
                let n = wheel_steps(&mut p.wheel, i.smooth_scroll_delta.y, line);
                if n != 0 { cmd(&["add", "volume", &(n * 5).to_string()]); new_vol = Some(st.volume + f64::from(n * 5)) }
            } else {
                p.wheel = 0.0;
            }
            toggle_fs |= k(F);
            if k(Escape) {
                if fs { toggle_fs = true } else { back = true }
            }
            if i.pointer.delta() != egui::Vec2::ZERO || i.pointer.any_pressed() || !i.events.is_empty() {
                p.osd_until = now + 3.0;
            }
        });
        // Loading keeps the controls up, and they stay 3 s after it ends.
        if st.buffering || vw == 0 {
            p.osd_until = now + 3.0;
        }
        let show = st.paused || p.menu.is_some() || p.scrub.is_some() || now < p.osd_until;
        if now < p.osd_until {
            ctx.request_repaint_after(Duration::from_secs_f64(p.osd_until - now));
        }
        let a = ctx.animate_bool_with_time(ui.scope_id().with("osd"), show, 0.25);
        if !show {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
        if a > 0.01 {
            let paint = ui.painter().clone().with_layer_id(ui.layer_id());
            let white = Color32::WHITE.gamma_multiply(a);
            // Black fades behind the top bar and the bottom controls, so white controls stay readable on bright video.
            let fade = |top: f32, bottom: f32, alpha: [f32; 2]| {
                let r = egui::Rect::from_x_y_ranges(rect.x_range(), top..=bottom);
                let c = |al: f32| Color32::from_black_alpha((al * a) as u8);
                let mut m = egui::Mesh::default();
                for (v, al) in [(r.left_top(), alpha[0]), (r.right_top(), alpha[0]), (r.left_bottom(), alpha[1]), (r.right_bottom(), alpha[1])] {
                    m.colored_vertex(v, c(al));
                }
                m.add_triangle(0, 1, 2);
                m.add_triangle(1, 2, 3);
                paint.add(m);
            };
            fade(rect.top(), rect.top() + 110.0, [170.0, 0.0]);
            fade(rect.bottom() - 170.0, rect.bottom(), [0.0, 200.0]);
            // Icon button: Stremio-style glyph on a rounded highlight that appears on hover.
            let icon = |ui: &mut egui::Ui, c: egui::Pos2, glyph: &str, label: &str| {
                let r = ui.interact(egui::Rect::from_center_size(c, vec2(56.0, 56.0)), ui.scope_id().with(label), Sense::CLICK);
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
                if r.hovered() {
                    paint.rect_filled(r.rect, 8, Color32::from_white_alpha((20.0 * a) as u8));
                }
                paint.text(c, egui::Align2::CENTER_CENTER, glyph, egui::FontId::new(26.0, if glyph == icons::PLAY || glyph == icons::PAUSE { fill() } else { phosphor() }), white);
                r.on_hover_cursor(egui::CursorIcon::PointingHand)
            };
            // Top bar: back chevron + title on the left, full screen on the right.
            back |= icon(ui, rect.min + vec2(44.0, 46.0), icons::CARET_LEFT, "Back to browse").clicked();
            // Truncated with an ellipsis so a long title stops short of the full screen button.
            let mut job = egui::text::LayoutJob::simple_singleline(p.title.clone(), egui::FontId::proportional(17.0), white);
            job.wrap = egui::text::TextWrapping::truncate_at_width((rect.width() - 82.0 - 96.0).max(0.0));
            let title = paint.layout_job(job);
            paint.galley(rect.min + vec2(82.0, 46.0 - title.size().y / 2.0), title, white);
            toggle_fs |= icon(ui, pos2(rect.right() - 52.0, rect.top() + 46.0), if fs { icons::ARROWS_IN } else { icons::ARROWS_OUT }, if fs { "Exit full screen" } else { "Full screen" }).clicked();

            // Seek bar between elapsed and total time: thin track, purple progress and knob; drag or click to seek.
            let font = egui::FontId::proportional(15.0);
            let by = rect.bottom() - 84.0;
            let bar = egui::Rect::from_min_max(pos2(rect.left() + 100.0, by - 12.0), pos2(rect.right() - 100.0, by + 12.0));
            let r = ui.interact(bar, ui.scope_id().with("seek"), Sense::CLICK | Sense::DRAG);
            r.widget_info(|| egui::WidgetInfo::slider(true, st.pos, "Seek"));
            let at = |x: f32| (((x - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64) * st.dur;
            if (r.dragged() || r.drag_started()) && st.dur > 0.0 {
                p.scrub = r.interact_pointer_pos().map(|q| at(q.x));
            }
            if let Some(t) = (r.clicked() || r.drag_stopped()).then(|| r.interact_pointer_pos().map(|q| at(q.x)).or(p.scrub)).flatten() {
                seek = Some((t, false));
            }
            if !r.dragged() {
                p.scrub = None;
            }
            let pos = p.scrub.unwrap_or(st.pos);
            let frac = if st.dur > 0.0 { (pos / st.dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
            let hot = r.hovered() || r.dragged();
            let th = ctx.animate_value_with_time(r.id.with("h"), if hot { 6.0 } else { 4.0 }, 0.1);
            let track = egui::Rect::from_center_size(bar.center(), vec2(bar.width(), th));
            paint.rect_filled(track, 2, Color32::from_gray(128).gamma_multiply(a * 0.5));
            let buf = buffered_frac(pos, st.cached_to, st.dur);
            paint.rect_filled(egui::Rect::from_min_size(track.min, vec2(track.width() * buf, th)), 2, Color32::from_gray(160).gamma_multiply(a * 0.6));
            paint.rect_filled(egui::Rect::from_min_size(track.min, vec2(track.width() * frac, th)), 2, accent().gamma_multiply(a));
            let knob = pos2(track.left() + track.width() * frac, track.center().y);
            paint.circle_filled(knob, if hot { 9.0 } else { 7.0 }, accent().gamma_multiply(a));
            paint.circle_filled(knob, 3.0, white);
            if let Some(h) = r.hover_pos().filter(|_| st.dur > 0.0) {
                paint.text(pos2(h.x, bar.top() - 6.0), egui::Align2::CENTER_BOTTOM, fmt_time(at(h.x)), font.clone(), white);
            }
            paint.text(pos2(rect.left() + 24.0, by), egui::Align2::LEFT_CENTER, fmt_time(pos), font.clone(), white);
            paint.text(pos2(rect.right() - 24.0, by), egui::Align2::RIGHT_CENTER, fmt_time(st.dur), font, white);

            // Control row: play, next episode, volume on the left; audio, subtitles, playback on the right.
            let y = rect.bottom() - 38.0;
            let x = |i: f32| rect.left() + 48.0 + i * 64.0;
            if icon(ui, pos2(x(0.0), y), if st.paused { icons::PLAY } else { icons::PAUSE }, if st.paused { "Play" } else { "Pause" }).clicked() {
                cmd(&["cycle", "pause"]);
            }
            next = has_next && icon(ui, pos2(x(1.0), y), icons::SKIP_FORWARD, "Next episode").clicked();
            let vx = x(if has_next { 2.0 } else { 1.0 });
            let vol = icon(ui, pos2(vx, y), if st.muted || st.volume <= 0.0 { icons::SPEAKER_X } else { icons::SPEAKER_HIGH }, "Mute");
            if vol.clicked() {
                cmd(&["cycle", "mute"]);
            }
            // Volume slider next to the speaker.
            let vbar = egui::Rect::from_min_size(pos2(vx + 36.0, y - 12.0), vec2(150.0, 24.0));
            let vr = ui.interact(vbar, ui.scope_id().with("volume"), Sense::CLICK | Sense::DRAG);
            vr.widget_info(|| egui::WidgetInfo::slider(true, st.volume, "Volume"));
            if let Some(q) = vr.interact_pointer_pos().filter(|_| vr.dragged() || vr.clicked()) {
                let v = ((q.x - vbar.left()) / vbar.width()).clamp(0.0, 1.0) * 100.0;
                cmd(&["set", "volume", &format!("{v:.0}")]);
                new_vol = Some(v.round().into());
                // The slider shows 0 while muted, so setting a volume also unmutes.
                if st.muted {
                    cmd(&["set", "mute", "no"]);
                }
            }
            let vt = egui::Rect::from_center_size(vbar.center(), vec2(vbar.width(), 4.0));
            let vf = if st.muted { 0.0 } else { (st.volume / 100.0).clamp(0.0, 1.0) as f32 };
            paint.rect_filled(vt, 2, Color32::from_gray(128).gamma_multiply(a * 0.5));
            paint.rect_filled(egui::Rect::from_min_size(vt.min, vec2(vt.width() * vf, 4.0)), 2, white);
            paint.circle_filled(pos2(vt.left() + vt.width() * vf, vt.center().y), 7.0, white);
            // Audio, subtitles and playback each open their own popover above the button.
            for (i, (m, glyph, label)) in [(Menu::Audio, icons::WAVEFORM, "Audio"), (Menu::Subtitles, icons::SUBTITLES, "Subtitles"), (Menu::Playback, icons::GEAR, "Playback")].into_iter().enumerate() {
                let bx = rect.right() - 52.0 - (2 - i) as f32 * 64.0;
                let b = icon(ui, pos2(bx, y), glyph, label);
                if p.menu == Some(m) {
                    paint.rect_filled(egui::Rect::from_center_size(pos2(bx, y + 22.0), vec2(18.0, 3.0)), 2, accent().gamma_multiply(a));
                }
                if b.clicked() {
                    p.menu = if p.menu == Some(m) { None } else { Some(m) };
                }
            }

            if let Some(m) = p.menu {
                let i = [Menu::Audio, Menu::Subtitles, Menu::Playback].iter().position(|&x| x == m).unwrap_or(0);
                // Right edge sits 150 px past its button's center, clamped inside the window.
                let right = (rect.right() - 52.0 - (2 - i) as f32 * 64.0 + 150.0).min(rect.right() - 16.0);
                egui::Area::new(ui.scope_id().with(("menu", i))).order(egui::Order::Foreground).pivot(egui::Align2::RIGHT_BOTTOM).fixed_pos(pos2(right, y - 70.0)).show(&ctx, |ui| {
                    egui::Frame::new().fill(Color32::from_rgba_unmultiplied(24, 24, 28, 245)).stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(18))).corner_radius(12).inner_margin(egui::Margin::symmetric(12, 14)).show(ui, |ui| {
                        ui.style_mut().visuals = ui.ctx().style_of(egui::Theme::Dark).visuals.clone();
                        ui.set_width(300.0);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let title = match m { Menu::Audio => "Audio", Menu::Subtitles => "Subtitles", Menu::Playback => "Playback" };
                        ui.label(RichText::new(title).size(18.0).family(bold()).color(Color32::WHITE));
                        if m != Menu::Playback {
                            let (kind, prop) = if m == Menu::Audio { ("audio", "aid") } else { ("sub", "sid") };
                            menu_heading(ui, "Track");
                            let tracks: Vec<_> = st.tracks.iter().filter(|t| t.kind == kind).collect();
                            let off = !tracks.iter().any(|t| t.selected);
                            let entries: Vec<_> = (kind == "sub").then_some((None, "Off", None, off)).into_iter().chain(tracks.iter().map(|t| (Some(t.id), t.label.as_str(), t.lang.as_deref(), t.selected))).collect();
                            if entries.is_empty() {
                                ui.label(RichText::new("No tracks").size(14.0).color(Color32::from_gray(150)));
                            }
                            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                                for (id, label, lang, sel) in entries {
                                    if menu_row(ui, label, sel).clicked() {
                                        let v = id.map_or("no".to_string(), |i| i.to_string());
                                        cmd(&["set", prop, &v]);
                                        // Remember the subtitle choice for the next video: Off, or the picked language first in `slang`.
                                        if kind == "audio" {
                                            // Remember the audio language: the picked track's language first in `alang`.
                                            if let Some(v) = lang.and_then(|l| prefer_lang(&self.langs[0], l)) {
                                                self.langs[0] = v.clone();
                                                if let Err(e) = self.db.set_setting("alang", &v) {
                                                    report(&mut self.error, format!("saving alang: {e:#}"));
                                                }
                                            }
                                        }
                                        if kind == "sub" {
                                            let mut save = vec![("sub_off", if id.is_none() { "1" } else { "0" }.to_string())];
                                            // shortcut: name-only addon languages ("English") fail `valid_langs` and aren't remembered; map names to codes if users hit it.
                                            if let Some(v) = lang.and_then(|l| prefer_lang(&self.langs[1], l)) {
                                                self.langs[1] = v.clone();
                                                save.push(("slang", v));
                                            }
                                            for (k, v) in save {
                                                if let Err(e) = self.db.set_setting(k, &v) {
                                                    report(&mut self.error, format!("saving {k}: {e:#}"));
                                                }
                                            }
                                        }
                                    }
                                }
                            });
                            if kind == "audio" {
                                style.audio_delay += stepper(ui, "Audio delay", &format!("{:+.1}s", style.audio_delay)) as f64 * 0.1;
                                if style.audio_delay != 0.0 && ui.add(egui::Button::new(RichText::new("Reset delay").size(13.0).color(accent())).frame(false)).clicked() {
                                    style.audio_delay = 0.0;
                                }
                            }
                            if kind == "sub" {
                                menu_heading(ui, "Search");
                                let s = &mut p.search;
                                if ui.add(egui::Button::new(RichText::new(if s.open { "Hide search" } else { "Search subtitles" }).size(14.0).color(Color32::WHITE)).fill(Color32::from_white_alpha(16)).corner_radius(8).min_size(vec2(ui.available_width(), 34.0))).clicked() {
                                    s.open = !s.open;
                                    search_req = s.open && s.results.is_empty() && !s.busy;
                                }
                                if s.open {
                                    ui.add(egui::TextEdit::singleline(&mut s.filter).hint_text("Filter by language or addon").desired_width(f32::INFINITY).margin(vec2(8.0, 6.0)));
                                    let hits = filter_subs(&s.results, &s.filter);
                                    let note = if s.busy { "Searching..." } else if hits.is_empty() { "No subtitles found" } else { "" };
                                    if !note.is_empty() {
                                        ui.label(RichText::new(note).size(14.0).color(Color32::from_gray(150)));
                                    }
                                    egui::ScrollArea::vertical().id_salt("sub_search").max_height(200.0).show(ui, |ui| {
                                        // shortcut: first 100 matches only; type a filter to narrow.
                                        for f in hits.into_iter().take(100) {
                                            ui.horizontal(|ui| {
                                                if ui.small_button("Save").on_hover_text("Save next to the video or in downloads").clicked() {
                                                    sub_act = Some((true, f.clone()));
                                                }
                                                if menu_row(ui, &format!("{} · {}", f.lang, f.addon), false).clicked() {
                                                    sub_act = Some((false, f.clone()));
                                                }
                                            });
                                        }
                                    });
                                    if !s.status.is_empty() {
                                        ui.label(RichText::new(&s.status).size(13.0).color(Color32::from_gray(170)));
                                    }
                                }
                                menu_heading(ui, "Style");
                                style.scale = (style.scale * 10.0).round() / 10.0 + stepper(ui, "Size", &format!("{:.1}x", style.scale)) as f64 * 0.1;
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Color").size(15.0).color(Color32::from_gray(210)));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        for (i, (name, hex)) in SUB_COLORS.iter().enumerate().rev() {
                                            if swatch(ui, name, Color32::from_hex(hex).unwrap_or(Color32::WHITE), style.color == i).clicked() {
                                                style.color = i;
                                            }
                                        }
                                    });
                                });
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Outline").size(15.0).color(Color32::from_gray(210)));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        for (i, (name, hex)) in SUB_OUTLINES.iter().enumerate().rev() {
                                            if swatch(ui, name, Color32::from_hex(hex).unwrap_or(Color32::BLACK), style.outline == i).clicked() {
                                                style.outline = i;
                                            }
                                        }
                                    });
                                });
                                style.pos += stepper(ui, "Position", &format!("{}%", style.pos)) as f64 * 5.0;
                                switch_row(ui, "Background box", &mut style.background);
                                style.delay += stepper(ui, "Delay", &format!("{:+.1}s", style.delay)) as f64 * 0.1;
                                if style.delay != 0.0 && ui.add(egui::Button::new(RichText::new("Reset delay").size(13.0).color(accent())).frame(false)).clicked() {
                                    style.delay = 0.0;
                                }
                            }
                            let (i, key) = if kind == "audio" { (0, "alang") } else { (1, "slang") };
                            menu_heading(ui, "Preferred languages");
                            let valid = valid_langs(&self.langs[i]);
                            let r = ui.add(egui::TextEdit::singleline(&mut self.langs[i]).hint_text("en,eng").desired_width(f32::INFINITY).margin(vec2(8.0, 6.0)));
                            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, format!("Preferred {kind} languages")));
                            if let Some(v) = &valid
                                && r.changed()
                                && let Err(e) = self.db.set_setting(key, v)
                            {
                                report(&mut self.error, format!("saving {key}: {e:#}"));
                            }
                            // Typing a subtitle language preference turns subtitles back on after a remembered "Off".
                            if kind == "sub"
                                && r.changed()
                                && valid_langs(&self.langs[1]).is_some_and(|v| !v.is_empty())
                                && let Err(e) = self.db.set_setting("sub_off", "0")
                            {
                                report(&mut self.error, format!("saving sub_off: {e:#}"));
                            }
                            if valid.is_none() {
                                ui.label(RichText::new("Use codes like en,eng,pt-BR (max 8)").size(13.0).color(Color32::from_rgb(230, 90, 90)));
                            }
                        } else {
                            menu_heading(ui, "Speed");
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                                for sp in SPEEDS {
                                    if pill(ui, &format!("{sp}x"), (st.speed - sp).abs() < 0.01, egui::Role::RadioButton).clicked() {
                                        cmd(&["set", "speed", &sp.to_string()]);
                                        // Also the starting speed of the next video, like the Default speed setting.
                                        self.default_speed = sp;
                                        if let Err(e) = self.db.set_setting("default_speed", &sp.to_string()) {
                                            report(&mut self.error, format!("saving default_speed: {e:#}"));
                                        }
                                    }
                                }
                            });
                            menu_heading(ui, "Video");
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                                for (i, (name, v)) in ASPECTS.iter().enumerate() {
                                    if pill(ui, name, p.aspect == i, egui::Role::RadioButton).clicked() {
                                        p.aspect = i;
                                        cmd(&["set", "video-aspect-override", v]);
                                    }
                                }
                            });
                            if switch_row(ui, "Fill screen (crop)", &mut p.fill).changed() {
                                cmd(&["set", "panscan", if p.fill { "1.0" } else { "0.0" }]);
                            }
                            let step = stepper(ui, "Zoom", &if p.zoom == 0 { "0.0".into() } else { format!("{:+.1}", p.zoom as f64 / 10.0) });
                            if step != 0 {
                                p.zoom = (p.zoom + step).clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
                                cmd(&["set", "video-zoom", &zoom_value(p.zoom)]);
                            }
                            if p.zoom != 0 && ui.add(egui::Button::new(RichText::new("Reset zoom").size(13.0).color(accent())).frame(false)).clicked() {
                                p.zoom = 0;
                                cmd(&["set", "video-zoom", "0"]);
                            }
                            if switch_row(ui, "Hardware decoding", &mut self.hwdec).changed() {
                                cmd(&["set", "hwdec", hwdec_value(self.hwdec)]);
                                if let Err(e) = self.db.set_setting("hwdec", if self.hwdec { "1" } else { "0" }) {
                                    report(&mut self.error, format!("saving hwdec: {e:#}"));
                                }
                            }
                            switch_row(ui, "Playback stats (I)", &mut p.stats);
                            ui.add_space(8.0);
                            external =ui.add(egui::Button::new(RichText::new("Open in external player").size(14.0).color(Color32::WHITE)).fill(Color32::from_white_alpha(16)).corner_radius(8).min_size(vec2(ui.available_width(), 34.0))).on_hover_text(format!("Pause here and open in: {}", self.external_player)).clicked();
                        }
                    });
                });
            }
        }
        // Stats overlay: mpv values arrive through observed properties; the torrent line is polled off the UI thread.
        if p.stats {
            if let (Some(id), Some(engine), true) = (p.torrent, self.torrent.get(), now >= p.net_next) {
                p.net_next = now + 1.0;
                let (engine, net, ctx) = (engine.clone(), p.net.clone(), ctx.clone());
                self.rt.spawn(async move {
                    *net.lock().unwrap() = engine.net_stats(id);
                    ctx.request_repaint();
                });
            }
            ctx.request_repaint_after(Duration::from_secs(1));
            let net = *p.net.lock().unwrap();
            let mut rows = vec![
                ("Video", format!("{} {}x{} @ {:.2} fps", if st.codec.is_empty() { "?" } else { &st.codec }, vw, vh, st.fps)),
                ("Hwdec", if st.hwdec.is_empty() { "no".into() } else { st.hwdec.clone() }),
                ("Dropped", format!("{} output, {} decoder", st.dropped[0], st.dropped[1])),
                ("Buffer", format!("{:.1} s", st.cache)),
            ];
            if p.torrent.is_some() {
                rows.push(("Peers", net.map_or("-".into(), |n| n.peers.to_string())));
                rows.push(("Speed", net.map_or("-".into(), |n| format!("{} down, {} up", fmt_rate(n.down), fmt_rate(n.up)))));
            }
            egui::Area::new(ui.scope_id().with("stats")).order(egui::Order::Foreground).fixed_pos(pos2(rect.left() + 16.0, rect.top() + 72.0)).interactable(false).show(&ctx, |ui| {
                egui::Frame::new().fill(Color32::from_rgba_unmultiplied(24, 24, 28, 230)).corner_radius(8).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
                    for (k, v) in rows {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{k:<8}")).size(13.0).monospace().color(Color32::from_gray(150)));
                            ui.label(RichText::new(v).size(13.0).monospace().color(Color32::WHITE));
                        });
                    }
                });
            });
        }
        // Up next: in an episode's last 30 s, a card slides in from the right to play the next one.
        if let Some((series, label, img)) = up_next {
            // Needs a duration over 60 s so a short or still-loading stream doesn't show the card at once.
            let open = !p.next_dismissed && rect.width() >= 362.0 && st.dur > 60.0 && st.dur - st.pos <= 30.0;
            let t = ctx.animate_bool_with_time(ui.scope_id().with("up_next"), open, 0.4);
            if t > 0.0 {
                let ease = 1.0 - (1.0 - t).powi(3);
                let size = vec2(480.0f32.min(rect.width() - 64.0).max(330.0), 160.0);
                let card = egui::Rect::from_min_size(pos2(rect.right() - (size.x + 32.0) * ease, rect.bottom() - 130.0 - size.y), size);
                let paint = ui.painter().clone();
                // Swallows clicks on the card so they don't pause the video; the buttons, registered later, win.
                let _ = ui.interact(card, ui.scope_id().with("up_next_card"), Sense::CLICK);
                paint.rect_filled(card, 12, Color32::from_rgb(22, 22, 26));
                // The buttons need 316 px; the still takes the rest (14 px at the narrowest).
                let thumb = egui::Rect::from_min_size(card.min, vec2((size.x - 316.0).clamp(0.0, size.y * 1.05), size.y));
                let left = egui::CornerRadius { nw: 12, sw: 12, ne: 0, se: 0 };
                paint.rect_filled(thumb, left, Color32::from_gray(40));
                if let (true, Some((u, b))) = (thumb.width() > 0.0, img) {
                    let img = egui::Image::from_bytes(format!("bytes://{u}"), b).corner_radius(left);
                    if let Ok(egui::load::TexturePoll::Ready { texture }) = img.load_for_size(&ctx, thumb.size()) {
                        img.uv(cover_uv(thumb, texture.size)).paint_at(ui, thumb);
                    }
                }
                let x = thumb.right() + 28.0;
                let w = (card.right() - 20.0 - x).max(0.0);
                let line = |sections: &[(&str, egui::FontFamily, Color32)], size: f32, y: f32| {
                    let mut job = egui::text::LayoutJob::default();
                    for (s, family, color) in sections {
                        job.append(s, 0.0, egui::TextFormat::simple(egui::FontId::new(size, family.clone()), *color));
                    }
                    job.wrap = egui::text::TextWrapping::truncate_at_width(w);
                    paint.galley(pos2(x, y), paint.layout_job(job), Color32::WHITE);
                };
                line(&[("Next on ", bold(), Color32::WHITE), (&series, bold(), Color32::from_gray(170))], 17.0, card.top() + 24.0);
                line(&[(&label, egui::FontFamily::Proportional, Color32::from_gray(225))], 16.0, card.top() + 58.0);
                // Buttons: Dismiss (text) and Watch now (outlined pill), bottom-right.
                let by = card.bottom() - 42.0;
                let watch = egui::Rect::from_center_size(pos2(card.right() - 20.0 - 80.0, by), vec2(160.0, 50.0));
                let dismiss = egui::Rect::from_center_size(pos2(watch.left() - 16.0 - 60.0, by), vec2(120.0, 50.0));
                for (r, glyph, family, text, id) in [(dismiss, icons::X, phosphor(), "Dismiss", "up_next_dismiss"), (watch, icons::PLAY, fill(), "Watch now", "up_next_watch")] {
                    let resp = ui.interact(r, ui.scope_id().with(id), Sense::CLICK).on_hover_cursor(egui::CursorIcon::PointingHand);
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, text));
                    let color = if id == "up_next_watch" || resp.hovered() { Color32::WHITE } else { Color32::from_gray(150) };
                    if id == "up_next_watch" {
                        paint.rect_stroke(r, 25, egui::Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Inside);
                    }
                    if resp.hovered() {
                        paint.rect_filled(r, 25, Color32::from_white_alpha(20));
                    }
                    let label = paint.layout_no_wrap(text.into(), egui::FontId::proportional(17.0), color);
                    let left = r.center().x - (label.size().x + 30.0) / 2.0;
                    paint.text(pos2(left + 9.0, by), egui::Align2::CENTER_CENTER, glyph, egui::FontId::new(18.0, family), color);
                    paint.galley(pos2(left + 30.0, by - label.size().y / 2.0), label, color);
                    if resp.clicked() {
                        if id == "up_next_watch" { next = true } else { p.next_dismissed = true }
                    }
                }
            }
        }
        // Skip button while the playhead is in an addon-marked or chapter-named intro/recap/credits span.
        // Credits of a series episode with a next one are left to the up-next card.
        let spans = p.skips.iter().copied().chain(chapter_skips(&st.chapters, st.dur));
        if let Some(sk) = spans.filter(|s| !(has_next && s.kind == SkipKind::Credits)).find(|s| s.start <= st.pos && st.pos < s.end)
            && rect.width() >= 250.0
        {
            let r = egui::Rect::from_min_size(pos2(rect.right() - 32.0 - 200.0, rect.bottom() - 130.0 - 50.0), vec2(200.0, 50.0));
            let paint = ui.painter().clone();
            let resp = ui.interact(r, ui.scope_id().with("skip"), Sense::CLICK).on_hover_cursor(egui::CursorIcon::PointingHand);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, sk.kind.label()));
            paint.rect_filled(r, 25, Color32::from_rgb(22, 22, 26));
            paint.rect_stroke(r, 25, egui::Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Inside);
            if resp.hovered() {
                paint.rect_filled(r, 25, Color32::from_white_alpha(20));
            }
            paint.text(r.center(), egui::Align2::CENTER_CENTER, sk.kind.label(), egui::FontId::proportional(17.0), Color32::WHITE);
            if resp.clicked() {
                seek = Some((sk.end, false));
            }
        }
        let style = style.clamped();
        if style != self.sub_style {
            self.sub_style = style;
            if let Err(e) = self.db.set_setting("sub_style", &style.encode()) {
                report(&mut self.error, e);
            }
            let _ = p.player.apply_sub_style(&style);
        }
        if let Some(v) = new_vol.map(|v| v.clamp(0.0, 130.0)).filter(|v| *v != self.volume) {
            self.volume = v;
            if let Err(e) = self.db.set_setting("volume", &v.to_string()) {
                report(&mut self.error, format!("saving volume: {e:#}"));
            }
        }
        if external {
            match open_external(&self.external_player, &p.url) {
                Ok(()) => cmds.push(vec!["set".into(), "pause".into(), "yes".into()]),
                Err(e) => report(&mut self.error, format!("{e:#}")),
            }
        }
        if let Some((to, relative)) = seek
            && let Err(e) = p.player.seek(to, relative)
        {
            report(&mut self.error, e);
        }
        for c in &cmds {
            let args: Vec<&str> = c.iter().map(String::as_str).collect();
            if let ["set", "sid", v] = args[..] {
                p.player.subs_off = v == "no";
            }
            if let Err(e) = p.player.command(&args) {
                report(&mut self.error, e);
            }
        }
        let (key, source, title) = (p.key.clone(), p.source.clone(), p.title.clone());
        if let Some((save, f)) = sub_act {
            if save {
                // Beside a local or downloaded video, else in the downloads folder.
                let src = std::path::Path::new(&source);
                let dir = src.is_file().then(|| src.parent().map(PathBuf::from)).flatten().unwrap_or_else(|| self.data_dir.join("downloads"));
                let client = self.client.clone();
                p.search.status = "Saving...".into();
                self.spawn(&ctx, async move {
                    let res = async { download::save_subtitle(&dir, &title, &f.lang, &client.subtitle_file(&f.url).await?).await }.await;
                    Ok(Msg::SubSaved(res.map_err(|e| format!("{e:#}"))))
                });
            } else if let Err(e) = p.player.add_sub(&f.url, &f.lang) {
                report(&mut self.error, e);
            }
        }
        if search_req {
            let lookup = self.sub_lookup(&key, &source);
            let names: Vec<(String, String)> = lookup.iter().flat_map(|l| &l.addons).map(|u| (u.clone(), self.addons.iter().find(|(x, _)| x == u).map(|(_, m)| m.name.clone()).unwrap_or_default())).collect();
            if let Some(p) = &mut self.player {
                p.search.busy = lookup.is_some();
                p.search.status = if lookup.is_some() { String::new() } else { "No subtitle addons for this item".into() };
            }
            if let Some(l) = lookup {
                let client = self.client.clone();
                // Like `hashed_subs`: the stream's hash and file name let OpenSubtitles-style addons match the exact file.
                let extra: Vec<(&str, String)> = [("videoHash", l.hints.video_hash.clone()), ("videoSize", l.hints.video_size.map(|n| n.to_string())), ("filename", l.hints.filename.clone())]
                    .into_iter()
                    .filter_map(|(k, v)| Some((k, v?)))
                    .collect();
                self.spawn(&ctx, async move {
                    let ask = |(u, name): (String, String)| {
                        let (c, k, i, e) = (client.clone(), l.kind.clone(), l.id.clone(), extra.clone());
                        async move { Ok(c.subtitles(&u, &k, &i, &e).await?.into_iter().map(|s| SubFound { addon: name.clone(), lang: s.lang, url: s.url }).collect::<Vec<_>>()) }
                    };
                    let mut found: Vec<SubFound> = concurrently(names, ask).await.into_iter().flatten().flatten().collect();
                    let mut seen = std::collections::HashSet::new();
                    found.retain(|f| seen.insert(f.url.clone()));
                    Ok(Msg::SubSearch(key, found))
                });
            }
        }
        if toggle_fs {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
        }
        if back {
            if fs {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
            self.stop_player();
            self.release_torrents(None);
        } else if next {
            self.play_next(&ctx);
        }
    }

    fn ui_addons(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if form_row(ui, "Manifest URL:", &mut self.addon_url, "http://127.0.0.1:7070/manifest.json", "Install", None, false) {
            // A pasted stremio:// link becomes its manifest URL (http for loopback).
            if let Some(stremflix_rs::addon::DeepLink::Addon(u)) = stremflix_rs::addon::parse_deep_link(&self.addon_url) {
                self.addon_url = u.into();
            }
            let (client, url) = (self.client.clone(), self.addon_url.trim().to_string());
            self.spawn(&ctx, async move {
                let m = client.fetch_manifest(&url).await?;
                Ok(Msg::Installed(url, m, None))
            });
        }
        // Blank path = auto-detect.
        if form_row(ui, "Stremio data:", &mut self.import_path, "Auto-detect local Stremio", "Import from Stremio", Some("Import addons, library, watched titles and progress"), true) {
            let path = Some(PathBuf::from(self.import_path.trim())).filter(|p| !p.as_os_str().is_empty());
            self.spawn(&ctx, async move {
                let (_, imp) = tokio::task::spawn_blocking(move || stremio_import::import(path.as_deref())).await??;
                Ok(Msg::Imported(imp))
            });
        }
        if ui.add_enabled(!self.checking_updates && !self.addons.is_empty(), egui::Button::new(if self.checking_updates { "Checking..." } else { "Check for updates" })).clicked() {
            self.checking_updates = true;
            let (client, urls) = (self.client.clone(), self.addons.iter().map(|(u, _)| u.clone()).collect::<Vec<_>>());
            self.spawn(&ctx, async move {
                let mut out = Vec::new();
                // Four at a time keeps a long addon list from opening dozens of connections.
                for chunk in urls.chunks(4) {
                    let res = concurrently(chunk.to_vec(), |u| {
                        let c = client.clone();
                        async move { Ok((u.clone(), c.fetch_manifest(&u).await.map_err(|e| e.root_cause().to_string()))) }
                    })
                    .await;
                    out.extend(res.into_iter().flatten());
                }
                Ok(Msg::UpdateCheck(out))
            });
        }
        ui.separator();
        let mut update = None;
        let mut remove = None;
        let mut configure = None;
        let mut replace = None;
        let mut swap = None;
        let mut details = None;
        let n = self.addons.len();
        for (i, (url, m)) in self.addons.iter().enumerate() {
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), Layout::left_to_right(Align::Center), |ui| {
                // Order is priority: streams and catalogs of higher addons come first.
                if ui.add_enabled_ui(i > 0, |ui| small_button(ui, "Up")).inner.on_hover_text("Higher priority").clicked() {
                    swap = Some(i - 1);
                }
                if ui.add_enabled_ui(i + 1 < n, |ui| small_button(ui, "Down")).inner.on_hover_text("Lower priority").clicked() {
                    swap = Some(i);
                }
                ui.label(format!("{} {} ({})", m.name, m.version, url));
                let open = self.addon_details.as_ref() == Some(url);
                if small_button(ui, if open { "Hide details" } else { "Details" }).clicked() {
                    details = Some((!open).then(|| url.clone()));
                }
                if m.behavior_hints.configurable && small_button(ui, "Configure").clicked() {
                    configure = Some(url.clone());
                }
                if small_button(ui, "Uninstall").clicked() {
                    remove = Some(url.clone());
                }
            });
            match self.addon_updates.iter().find(|(u, _)| u == url).map(|(_, r)| r) {
                Some(Ok(new)) if !same_addon(m, new) => {
                    ui.label(RichText::new("Addon id changed; not offered").weak());
                }
                Some(Ok(new)) if stremflix_rs::addon::is_newer(&new.version, &m.version) => {
                    ui.horizontal(|ui| {
                        ui.label(format!("Update available: {} \u{2192} {}", m.version, new.version));
                        if small_button(ui, "Update").clicked() {
                            update = Some((url.clone(), new.clone()));
                        }
                    });
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(format!("Unreachable: {e}")).weak());
                }
                _ => {}
            }
            if m.behavior_hints.configuration_required {
                ui.label(RichText::new("Needs configuration before use. Press Configure.").weak());
            }
            if self.addon_details.as_ref() == Some(url) {
                Self::ui_addon_details(ui, m);
            }
            if let Some(link) = stremflix_rs::addon::install_link(url) {
                ui.collapsing("Install on phone (QR)", |ui| qr_ui(ui, &link));
            }
            if let Some((old, pasted)) = self.configuring.as_mut().filter(|(o, _)| o == url)
                && form_row(ui, "Configured URL:", pasted, "https://.../manifest.json", "Replace", None, false)
            {
                replace = Some((old.clone(), pasted.trim().to_string()));
            }
        }
        if let Some((url, new)) = update
            && self.addons.iter().any(|(u, m)| *u == url && same_addon(m, &new))
        {
            self.addon_updates.retain(|(u, _)| *u != url);
            self.handle(&ctx, Msg::Installed(url, new, None));
        }
        if let Some((old, new)) = replace {
            let client = self.client.clone();
            self.spawn(&ctx, async move {
                let m = client.fetch_manifest(&new).await?;
                Ok(Msg::Installed(new, m, Some(old)))
            });
        }
        if let Some(u) = configure {
            match stremflix_rs::addon::configure_url(&u) {
                Ok(page) => {
                    ctx.open_url(egui::OpenUrl::new_tab(page.as_str()));
                    self.configuring = Some((u, String::new()));
                }
                Err(e) => report(&mut self.error, e),
            }
        }
        if let Some(d) = details {
            self.addon_details = d;
        }
        if let Some(i) = swap {
            self.addons.swap(i, i + 1);
            let urls: Vec<String> = self.addons.iter().map(|(u, _)| u.clone()).collect();
            if let Err(e) = self.db.reorder_addons(&urls) {
                report(&mut self.error, format!("saving addon order: {e:#}"));
            }
            self.build_rows(&ctx);
        }
        if let Some(u) = remove {
            let _ = self.db.remove_addon(&u);
            self.addons.retain(|(x, _)| *x != u);
            self.build_rows(&ctx);
        }
        self.ui_browse_addons(ui);
    }

    /// Addons listed by the `addon_catalog` resource of installed addons (e.g. Cinemeta's Community catalog), each installable.
    fn ui_browse_addons(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.separator();
        ui.label(RichText::new("Browse addons").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        let label = |addon: &str, c: &CatalogDef| sanitize_text(&format!("{addon} - {} ({})", c.name.as_deref().unwrap_or(&c.id), kind_label(&c.kind)), 400);
        let sources: Vec<(AddonCatalogSrc, String)> = self
            .addons
            .iter()
            .flat_map(|(u, m)| m.addon_catalogs.iter().map(move |c| ((u.clone(), c.kind.clone(), c.id.clone()), label(&m.name, c))))
            .collect();
        if sources.is_empty() {
            ui.label("Install an addon with addon catalogs to browse more addons, e.g. Cinemeta: https://v3-cinemeta.strem.io/manifest.json");
            return;
        }
        let mut pick = self.browse.as_ref().map(|b| b.0.clone()).filter(|s| sources.iter().any(|(x, _)| x == s));
        let shown = pick.as_ref().and_then(|p| sources.iter().find(|(x, _)| x == p)).map_or("Pick a catalog", |(_, l)| l.as_str());
        egui::ComboBox::from_id_salt("addon_catalog").selected_text(shown).show_ui(ui, |ui| {
            for (src, l) in &sources {
                ui.selectable_value(&mut pick, Some(src.clone()), l);
            }
        });
        let Some(src) = pick else {
            self.browse = None;
            return;
        };
        if self.browse.as_ref().map(|b| &b.0) != Some(&src) {
            self.browse = Some((src.clone(), None));
            let client = self.client.clone();
            self.spawn(&ctx, async move {
                let res = client.addon_catalog(&src.0, &src.1, &src.2).await.map_err(|e| format!("{e:#}"));
                Ok(Msg::AddonCatalog(src, res))
            });
        }
        let Some(items) = self.browse.as_ref().and_then(|b| b.1.as_ref()) else {
            ui.spinner();
            return;
        };
        if items.is_empty() {
            ui.label("No addons in this catalog.");
        }
        let mut install = None;
        for a in items {
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), Layout::left_to_right(Align::Center), |ui| {
                // Typed URLs are stored as entered; compare the normalized form, as listed URLs are.
                if self.addons.iter().any(|(u, _)| stremflix_rs::addon::safe_http_url(u).is_some_and(|u| u.as_str() == a.transport_url)) {
                    ui.add_enabled_ui(false, |ui| small_button(ui, "Installed"));
                } else if primary_button(ui, "Install").on_hover_text(&a.transport_url).clicked() {
                    install = Some(a.transport_url.clone());
                }
                ui.label(RichText::new(format!("{} {}", a.manifest.name, a.manifest.version)).strong());
                // The host shows where the addon really lives, since any listed addon can claim any name.
                if let Ok(u) = url::Url::parse(&a.transport_url) {
                    ui.label(RichText::new(u.host_str().unwrap_or_default()).weak());
                }
                if let Some(d) = &a.manifest.description {
                    // One line; the full text is on hover.
                    ui.add(egui::Label::new(RichText::new(d.replace('\n', " ")).weak()).truncate()).on_hover_text(d);
                }
            });
        }
        // The listed manifest is only a preview: install what the addon itself serves.
        if let Some(url) = install {
            let client = self.client.clone();
            self.spawn(&ctx, async move {
                let m = client.fetch_manifest(&url).await?;
                Ok(Msg::Installed(url, m, None))
            });
        }
    }

    /// Manifest facts under an addon row: id, types, resources, catalogs, description.
    fn ui_addon_details(ui: &mut egui::Ui, m: &Manifest) {
        // Types and resource names are not sanitized at parse time; strip control and bidi characters here.
        let list = |v: Vec<&str>| v.into_iter().map(|t| sanitize_text(t, 64)).collect::<Vec<_>>().join(", ");
        let catalogs: Vec<&str> = m.catalogs.iter().map(|c| c.name.as_deref().unwrap_or(&c.id)).collect();
        let gray = ui.visuals().weak_text_color();
        egui::Frame::new().inner_margin(egui::Margin { left: 24, top: 4, bottom: 8, right: 0 }).show(ui, |ui| {
            let facts = [
                ("ID", sanitize_text(&m.id, 300)),
                ("Version", m.version.clone()),
                ("Types", list(m.types.iter().map(String::as_str).collect())),
                ("Resources", list(m.resources.iter().map(|r| r.name()).collect())),
                ("Catalogs", list(catalogs)),
            ];
            for (name, v) in facts.into_iter().filter(|f| !f.1.is_empty()) {
                ui.add(egui::Label::new(RichText::new(format!("{name}: {v}")).color(gray)).wrap());
            }
            if let Some(d) = &m.description {
                ui.add(egui::Label::new(RichText::new(d).color(ui.visuals().strong_text_color())).wrap());
            }
        });
    }

    /// Whether videos start with subtitles off.
    fn subs_off(&self) -> bool {
        let setting = |k| self.db.setting(k).ok().flatten();
        match setting("sub_off").as_deref() {
            Some("0") | Some("") => false, // explicit or legacy "on"
            Some(_) => true,
            None => setting("slang").unwrap_or_default().is_empty(), // fresh install: off; legacy slang: on
        }
    }

    /// Torrent engine and external player options; each is saved as soon as it changes.
    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let mut save: Vec<(&str, String)> = Vec::new();
        ui.add_space(8.0);
        ui.label(RichText::new("Appearance").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        ui.horizontal(|ui| {
            ui.label(RichText::new("Theme").size(15.0).color(ui.visuals().text_color()));
            for (name, light) in [("Dark", false), ("Light", true)] {
                if pill(ui, name, self.light == light, egui::Role::RadioButton).clicked() && self.light != light {
                    self.light = light;
                    save.push(("theme", name.to_lowercase()));
                }
            }
            ui.add_space(12.0);
            ui.label(RichText::new("Accent").size(15.0).color(ui.visuals().text_color()));
            for (i, (name, color)) in ACCENTS.iter().enumerate() {
                let cur = ACCENT_IDX.load(std::sync::atomic::Ordering::Relaxed);
                if swatch(ui, name, *color, cur == i).clicked() && cur != i {
                    ACCENT_IDX.store(i, std::sync::atomic::Ordering::Relaxed);
                    save.push(("accent", name.to_string()));
                }
            }
        });
        ui.scope(|ui| {
            ui.set_max_width(550.0);
            let step = stepper(ui, "UI scale", &format!("{:.0}%", self.ui_scale * 100.0));
            if step != 0 {
                ui.ctx().set_zoom_factor(clamp_scale(self.ui_scale + step as f32 * 0.25));
            }
        });
        ui.label(RichText::new("Also Ctrl and +, Ctrl and −, Ctrl and 0.").size(13.0).color(ui.visuals().weak_text_color()));
        if !save.is_empty() {
            apply_theme(ui.ctx(), self.light);
        }
        ui.add_space(16.0);
        ui.label(RichText::new("Stremio account").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        let ctx = ui.ctx().clone();
        if let Some(email) = self.account.clone() {
            ui.horizontal(|ui| {
                ui.label(format!("Signed in as {email}"));
                let busy = self.account_busy;
                if ui.add_enabled_ui(!busy, |ui| primary_button(ui, "Sync")).inner.on_hover_text("Copy the account's addons and library here").clicked() {
                    self.sync_account(&ctx);
                }
                if ui.add_enabled_ui(!busy, |ui| small_button(ui, "Upload to account")).inner.on_hover_text("Copy addons and library from here to the account").clicked() {
                    self.confirm_upload = true;
                }
                if busy {
                    ui.spinner();
                }
                if small_button(ui, "Sign out").clicked() {
                    let key = self.db.setting("stremio_auth").ok().flatten().unwrap_or_default();
                    let cleared = self.db.transaction(|| ["stremio_auth", "stremio_email"].into_iter().try_for_each(|k| self.db.remove_setting(k)));
                    match cleared {
                        Ok(()) => {
                            // A running sync for this key is dropped when it ends.
                            (self.account, self.account_busy, self.confirm_upload, self.account_note) = (None, false, false, None);
                            let client = self.client.clone();
                            // Best effort: the key is already forgotten locally.
                            self.rt.spawn(async move { client.stremio_logout(&key).await.inspect_err(|e| stremflix_rs::elog!("Stremio sign-out: {e:#}")) });
                        }
                        Err(e) => report(&mut self.error, format!("signing out: {e:#}")),
                    }
                }
            });
            if self.confirm_upload {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Overwrites the account's addon list with this order (addons only on the account are kept, last). Saved titles missing on the account are added (ones it has, even removed, stay as they are), and progress is updated only where it is newer there. Nothing is deleted.").size(13.0).color(Color32::from_gray(170)));
                    if primary_button(ui, "Upload").clicked() {
                        self.confirm_upload = false;
                        self.upload_account(&ctx);
                    }
                    if small_button(ui, "Cancel").clicked() {
                        self.confirm_upload = false;
                    }
                });
            }
            if let Some(note) = &self.account_note {
                ui.label(RichText::new(note).size(13.0).color(Color32::from_gray(170)));
            }
        } else {
            let mut go = false;
            ui.horizontal(|ui| {
                let l = ui.label("Email:");
                ui.add_space((150.0 - l.rect.width()).max(0.0));
                ui.add(egui::TextEdit::singleline(&mut self.login.0).desired_width(260.0)).labelled_by(l.id);
                let l = ui.label("Password:");
                let r = ui.add(egui::TextEdit::singleline(&mut self.login.1).password(true).desired_width(200.0)).labelled_by(l.id);
                go = ui.add_enabled_ui(!self.account_busy, |ui| primary_button(ui, "Sign in")).inner.clicked()
                    || !self.account_busy && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if self.account_busy {
                    ui.spinner();
                }
            });
            ui.label(RichText::new("Copies your addons, library and watched titles from api.strem.io. Settings can upload them back after signing in.").size(13.0).color(ui.visuals().weak_text_color()));
            let email = self.login.0.trim().to_string();
            if go && !email.is_empty() && !self.login.1.is_empty() {
                // Not kept after use, even if signing in fails.
                let (client, password) = (self.client.clone(), std::mem::take(&mut self.login.1));
                self.account_busy = true;
                self.spawn(&ctx, async move {
                    let key = client.stremio_login(&email, &password).await.map_err(|e| format!("Stremio sign-in: {e:#}"));
                    Ok(Msg::SignedIn(email, key))
                });
            }
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Data").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        if small_button(ui, "Export data…").on_hover_text("Write addons, saved titles, progress and settings to a JSON file in the data folder").clicked() {
            match self.db.export_file(&self.data_dir.join("exports")) {
                Ok(p) => self.data_msg = Some(format!("Exported to {}", p.display())),
                Err(e) => report(&mut self.error, format!("exporting: {e:#}")),
            }
        }
        ui.label(RichText::new("The export includes addon URLs, which can hold account tokens. Keep it private.").size(13.0).color(Color32::from_gray(170)));
        if form_row(ui, "Import data…:", &mut self.data_import, "/path/to/stremflix-rs-export.json", "Import", Some("Merges the file into your data; nothing is deleted"), false) {
            match self.db.import_file(std::path::Path::new(self.data_import.trim())) {
                Ok(m) => self.data_msg = Some(format!("{m}. Restart the app to apply.")),
                Err(e) => report(&mut self.error, format!("importing: {e:#}")),
            }
        }
        if let Some(m) = &self.data_msg {
            ui.label(RichText::new(m).size(13.0).color(Color32::from_gray(170)));
        }
        ui.label(RichText::new("Diagnostics").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        let out = self.data_dir.join("diagnostics.txt");
        if ui.button("Export diagnostics").on_hover_text(format!("Version, OS and recent log without secrets, saved to {}", out.display())).clicked() {
            let summary = format!("addons: {}\nsigned in: {}", self.addons.len(), self.account.is_some());
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            let res = o.open(&out).and_then(|mut f| {
                // `mode` only applies to a new file: tighten an older one.
                #[cfg(unix)]
                f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
                std::io::Write::write_all(&mut f, stremflix_rs::diag::bundle(&summary).as_bytes())
            });
            match res {
                Ok(()) => self.data_msg = Some(format!("Diagnostics saved to {}", out.display())),
                Err(e) => report(&mut self.error, format!("diagnostics export: {e}")),
            }
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Torrents").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        let hint = self.data_dir.join("torrents").display().to_string();
        if form_row(ui, "Cache location:", &mut self.torrent_dir, &hint, "Save", Some("Empty: the app's data folder"), true) {
            let d = self.torrent_dir.trim().to_string();
            // Relative paths would depend on the working directory.
            if d.is_empty() || std::path::Path::new(&d).is_absolute() {
                save.push(("torrent_dir", d));
            } else {
                report(&mut self.error, "cache location must be an absolute path");
            }
        }
        ui.horizontal(|ui| {
            let l = ui.label("Cache size:");
            ui.add_space((150.0 - l.rect.width()).max(0.0));
            let label = CACHE_SIZES.iter().find(|c| c.1 == self.torrent_cache).map_or("Custom", |c| c.0);
            egui::ComboBox::from_id_salt("torrent_cache").selected_text(label).show_ui(ui, |ui| {
                for (name, bytes) in CACHE_SIZES {
                    if ui.selectable_value(&mut self.torrent_cache, bytes, name).changed() {
                        save.push(("torrent_cache", bytes.to_string()));
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            let l = ui.label("Connections:");
            ui.add_space((150.0 - l.rect.width()).max(0.0));
            egui::ComboBox::from_id_salt("torrent_profile").selected_text(self.torrent_profile.label()).show_ui(ui, |ui| {
                for p in Profile::ALL {
                    if ui.selectable_value(&mut self.torrent_profile, p, p.label()).changed() {
                        save.push(("torrent_profile", p.label().into()));
                    }
                }
            });
        });
        for (title, key, val) in [("Download limit:", "torrent_down_limit", &mut self.torrent_limits.0), ("Upload limit:", "torrent_up_limit", &mut self.torrent_limits.1)] {
            ui.horizontal(|ui| {
                let l = ui.label(title);
                ui.add_space((150.0 - l.rect.width()).max(0.0));
                let label = SPEED_LIMITS.iter().find(|c| c.1 == *val).map_or("Custom", |c| c.0);
                egui::ComboBox::from_id_salt(key).selected_text(label).show_ui(ui, |ui| {
                    for (name, bps) in SPEED_LIMITS {
                        if ui.selectable_value(val, bps, name).changed() {
                            save.push((key, bps.to_string()));
                        }
                    }
                });
            });
        }
        let note = "Stopped torrents stay on disk up to the cache size, and are cleared when the app starts.";
        ui.label(RichText::new(note).size(13.0).color(ui.visuals().weak_text_color()));
        if self.torrent.get().is_some() {
            ui.label(RichText::new("A torrent has already played: restart the app to apply changes.").size(13.0).color(ui.visuals().weak_text_color()));
        }
        ui.add_space(16.0);
        ui.label(RichText::new("Proxy").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        let kinds = [("", "Off"), ("http", "HTTP"), ("socks5", "SOCKS5")];
        ui.horizontal(|ui| {
            let l = ui.label("Type:");
            ui.add_space((150.0 - l.rect.width()).max(0.0));
            let shown = kinds.iter().find(|k| k.0 == self.proxy[0]).map_or("Off", |k| k.1);
            egui::ComboBox::from_id_salt("proxy_kind").selected_text(shown).show_ui(ui, |ui| {
                for (k, name) in kinds {
                    ui.selectable_value(&mut self.proxy[0], k.to_string(), name);
                }
            });
        });
        let mut apply = false;
        if !self.proxy[0].is_empty() {
            ui.horizontal(|ui| {
                let l = ui.label("Address:");
                ui.add_space((150.0 - l.rect.width()).max(0.0));
                ui.add(egui::TextEdit::singleline(&mut self.proxy[1]).hint_text("host:port").desired_width(260.0)).labelled_by(l.id);
            });
            ui.horizontal(|ui| {
                let l = ui.label("Username:");
                ui.add_space((150.0 - l.rect.width()).max(0.0));
                ui.add(egui::TextEdit::singleline(&mut self.proxy[2]).hint_text("optional").desired_width(160.0)).labelled_by(l.id);
                let l = ui.label("Password:");
                ui.add(egui::TextEdit::singleline(&mut self.proxy[3]).password(true).desired_width(160.0)).labelled_by(l.id);
            });
        }
        let [kind, addr, user, pass] = &self.proxy;
        match stremflix_rs::addon::proxy_url(kind, addr, user, pass) {
            Ok(_) => apply = primary_button(ui, "Save proxy").clicked(),
            Err(e) => {
                ui.label(RichText::new(e).size(13.0).color(Color32::from_rgb(230, 90, 90)));
            }
        }
        if apply {
            for (k, v) in ["proxy_kind", "proxy_addr", "proxy_user", "proxy_pass"].into_iter().zip(&self.proxy) {
                save.push((k, v.clone()));
            }
        }
        let note = "Applies after a restart. HTTP covers addons, downloads, the update check and mpv streams, but not torrents. \
            SOCKS5 covers addons, downloads, the update check and torrent peers and HTTP trackers, but not mpv's direct HTTP streams \
            (mpv has no SOCKS support) or DHT. The password is stored unencrypted in the app's database.";
        ui.label(RichText::new(note).size(13.0).color(ui.visuals().weak_text_color()));
        ui.add_space(16.0);
        ui.label(RichText::new("Player").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        if form_row(ui, "External player:", &mut self.external_player, "vlc", "Save", Some("Command run with the stream URL; no shell"), true) {
            save.push(("external_player", self.external_player.trim().to_string()));
        }
        let on = |b: bool| if b { "1" } else { "0" }.to_string();
        let mut style = self.sub_style;
        ui.scope(|ui| {
            ui.set_max_width(550.0);
            if switch_row(ui, "Play next episode automatically", &mut self.autoplay_next).changed() {
                save.push(("autoplay_next", on(self.autoplay_next)));
            }
            if switch_row(ui, "Hardware decoding", &mut self.hwdec).changed() {
                save.push(("hwdec", on(self.hwdec)));
            }
            if switch_row(ui, "Notify about new episodes", &mut self.notify_eps).changed() {
                save.push(("notify_eps", on(self.notify_eps)));
            }
            if switch_row(ui, "Minimize to tray on close", &mut self.tray_on_close).changed() {
                save.push(("tray_on_close", on(self.tray_on_close)));
            }
            let step = stepper(ui, "Subtitle size", &format!("{:.1}x", style.scale));
            if step != 0 {
                style.scale = (style.scale * 10.0).round() / 10.0 + step as f64 * 0.1;
            }
            ui.horizontal(|ui| {
                ui.label(RichText::new("Subtitle color").size(15.0).color(ui.visuals().weak_text_color()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (i, (name, hex)) in SUB_COLORS.iter().enumerate().rev() {
                        if swatch(ui, name, Color32::from_hex(hex).unwrap_or(Color32::WHITE), style.color == i).clicked() {
                            style.color = i;
                        }
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Subtitle outline").size(15.0).color(ui.visuals().weak_text_color()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (i, (name, hex)) in SUB_OUTLINES.iter().enumerate().rev() {
                        if swatch(ui, name, Color32::from_hex(hex).unwrap_or(Color32::BLACK), style.outline == i).clicked() {
                            style.outline = i;
                        }
                    }
                });
            });
            style.pos += stepper(ui, "Subtitle position", &format!("{}%", style.pos)) as f64 * 5.0;
            switch_row(ui, "Subtitle background box", &mut style.background);
            style.delay += stepper(ui, "Subtitle delay", &format!("{:+.1}s", style.delay)) as f64 * 0.1;
            if style.delay != 0.0 && ui.add(egui::Button::new(RichText::new("Reset delay").size(13.0).color(accent())).frame(false)).clicked() {
                style.delay = 0.0;
            }
            let mut off = self.subs_off();
            if switch_row(ui, "Subtitles off by default", &mut off).changed() {
                save.push(("sub_off", if off { "1" } else { "0" }.into()));
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Default speed").size(15.0).color(ui.visuals().weak_text_color()));
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for sp in SPEEDS {
                    if pill(ui, &format!("{sp}x"), self.default_speed == sp, egui::Role::RadioButton).clicked() {
                        self.default_speed = sp;
                        save.push(("default_speed", sp.to_string()));
                    }
                }
            });
        });
        let style = style.clamped();
        if style != self.sub_style {
            self.sub_style = style;
            save.push(("sub_style", style.encode()));
        }
        ui.horizontal(|ui| {
            for (i, key, label) in [(0, "seek_step", "Arrow-key seek"), (1, "seek_step_shift", "Shift + arrow seek")] {
                ui.label(label);
                if ui.add(egui::DragValue::new(&mut self.seek_steps[i]).range(SEEK_STEP_RANGE).suffix(" s")).changed() {
                    save.push((key, self.seek_steps[i].to_string()));
                }
            }
        });
        ui.add_space(16.0);
        ui.label(RichText::new("Languages").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        for (i, key, label) in [(0, "alang", "Audio:"), (1, "slang", "Subtitles:")] {
            ui.horizontal(|ui| {
                let l = ui.label(label);
                ui.add_space((150.0 - l.rect.width()).max(0.0));
                let r = ui.add(egui::TextEdit::singleline(&mut self.langs[i]).hint_text("en,eng").desired_width(400.0)).labelled_by(l.id);
                match valid_langs(&self.langs[i]) {
                    Some(v) if r.changed() => {
                        // As in the player: a subtitle language preference turns subtitles back on after a remembered "Off".
                        if i == 1 && !v.is_empty() {
                            save.push(("sub_off", String::new()));
                        }
                        save.push((key, v));
                    }
                    None => {
                        ui.label(RichText::new("Use codes like en,eng,pt-BR (max 8)").size(13.0).color(Color32::from_rgb(230, 90, 90)));
                    }
                    _ => {}
                }
            });
        }
        ui.label(RichText::new("Comma-separated, most preferred first. Also editable in the player.").size(13.0).color(Color32::from_gray(170)));
        ui.add_space(16.0);
        ui.label(RichText::new("Customize Home").size(20.0).strong().family(bold()).color(Color32::WHITE));
        let (mut swap, mut toggle, mut reset) = (None, None, false);
        let n = self.home_all.len();
        for (i, (key, title)) in self.home_all.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.add_enabled_ui(i > 0, |ui| small_button(ui, "Up")).inner.clicked() {
                    swap = Some(i - 1);
                }
                if ui.add_enabled_ui(i + 1 < n, |ui| small_button(ui, "Down")).inner.clicked() {
                    swap = Some(i);
                }
                let hidden = self.home_hidden.contains(key);
                if small_button(ui, if hidden { "Show" } else { "Hide" }).clicked() {
                    toggle = Some(key.clone());
                }
                ui.label(if hidden { RichText::new(title).weak() } else { RichText::new(title) });
            });
        }
        if self.home_all.is_empty() {
            ui.label(RichText::new("No catalog rows yet.").weak());
        }
        if small_button(ui, "Reset").on_hover_text("Addon order, nothing hidden").clicked() {
            reset = true;
        }
        if swap.is_some() || toggle.is_some() || reset {
            // Show and Reset need a fetch, so they rebuild. Up, Down and Hide reorder or filter the loaded rows.
            let rebuild = reset || toggle.as_ref().is_some_and(|k| self.home_hidden.contains(k));
            if let Some(i) = swap {
                self.home_all.swap(i, i + 1);
                self.home_order = self.home_all.iter().map(|(k, _)| k.clone()).collect();
            }
            if let Some(k) = toggle {
                match self.home_hidden.iter().position(|x| *x == k) {
                    Some(p) => drop(self.home_hidden.remove(p)),
                    None => self.home_hidden.push(k),
                }
            }
            if reset {
                (self.home_order, self.home_hidden) = (Vec::new(), Vec::new());
            }
            save.push(("home_order", encode_keys(&self.home_order)));
            save.push(("home_hidden", encode_keys(&self.home_hidden)));
            if rebuild {
                self.build_rows(&ctx);
            } else {
                let order = &self.home_all;
                self.rows.sort_by_key(|r| order.iter().position(|(k, _)| *k == row_key(&r.src)));
                self.rows.retain(|r| !self.home_hidden.contains(&row_key(&r.src)));
            }
        }
        ui.label(RichText::new("Comma-separated, most preferred first. Also editable in the player.").size(13.0).color(ui.visuals().weak_text_color()));
        for (k, v) in save {
            if let Err(e) = self.db.set_setting(k, &v) {
                report(&mut self.error, format!("saving {k}: {e:#}"));
            }
        }
    }

    fn ui_library(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (mut want, mut open, mut quick) = (Vec::new(), None, None);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Saved titles").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
            ui.label(RichText::new("Saved titles").size(20.0).strong().family(bold()).color(Color32::WHITE));
            let before = self.lib_sort;
            egui::ComboBox::from_id_salt("lib_sort").selected_text(self.lib_sort.label()).show_ui(ui, |ui| {
                for s in [LibSort::Added, LibSort::Title, LibSort::Watched, LibSort::Rating, LibSort::Year] {
                    ui.selectable_value(&mut self.lib_sort, s, s.label());
                }
            });
            // Watch times are read when loaded, so refresh them when the order is picked.
            if self.lib_sort != before {
                self.saved = self.db.saved_titles().unwrap_or_default();
            }
            egui::ComboBox::from_id_salt("lib_kind").selected_text(self.lib_kind.label()).show_ui(ui, |ui| {
                for k in [LibKind::All, LibKind::Movie, LibKind::Series] {
                    ui.selectable_value(&mut self.lib_kind, k, k.label());
                }
            });
            let list = self.lists.iter().find(|l| Some(l.id) == self.lib_list);
            egui::ComboBox::from_id_salt("lib_list").selected_text(list.map_or("All saved", |l| l.name.as_str())).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.lib_list, None, "All saved");
                for l in &self.lists {
                    ui.selectable_value(&mut self.lib_list, Some(l.id), &l.name);
                }
            });
            ui.add(egui::TextEdit::singleline(&mut self.list_name).hint_text("List name").char_limit(60).desired_width(140.0));
            let mut res = None;
            if ui.button("New list").clicked() {
                res = Some(self.db.create_list(&self.list_name).map(|id| self.lib_list = Some(id)));
            }
            if let Some(id) = self.lib_list {
                if ui.button("Rename").clicked() {
                    res = Some(self.db.rename_list(id, &self.list_name));
                }
                if ui.button("Delete list").on_hover_text("Its titles stay saved").clicked() {
                    res = Some(self.db.delete_list(id));
                }
            }
            if let Some(res) = res {
                if res.is_ok() {
                    self.list_name.clear();
                }
                self.lists_changed(res);
            }
            let edit = ui.add(egui::TextEdit::singleline(&mut self.lib_query).hint_text("Filter by title").desired_width(180.0));
            if (edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape))) || (!self.lib_query.is_empty() && small_button(ui, "x").on_hover_text("Clear filter").clicked()) {
                self.lib_query.clear();
            }
        });
        ui.add_space(4.0);
        let list = self.lists.iter().find(|l| Some(l.id) == self.lib_list);
        let items: Vec<MetaPreview> = saved_view(&self.saved, self.lib_kind, self.lib_sort, &self.lib_query)
            .into_iter()
            .filter(|t| list.is_none_or(|l| l.keys.contains(&t.key)))
            .filter_map(SavedTitle::preview)
            .collect();
        if self.saved.is_empty() {
            ui.label(RichText::new("Nothing saved yet. Open a title and choose \"Add to library\".").color(ui.visuals().weak_text_color()));
        } else if items.is_empty() {
            ui.label(RichText::new(if list.is_some() { "No titles match in this list." } else { "No saved titles match." }).color(ui.visuals().weak_text_color()));
        }
        self.ui_cards(ui, "saved", &items, &mut want, &mut open, &mut quick);
        for u in want {
            self.fetch_poster(&ctx, u);
        }
        if let Some((m, _)) = open {
            // The detail page lives on the Home tab.
            self.tab = Tab::Home;
            self.open_meta(&ctx, m, false);
        }
        if let Some(m) = quick {
            self.quick_play(&ctx, m);
        }
        ui.separator();
        let mut cal_open = None;
        egui::CollapsingHeader::new(RichText::new("Calendar").size(20.0).strong().family(bold())).id_salt("calendar").show(ui, |ui| {
            let live: Vec<_> = self.upcoming.iter().filter(|(m, _)| self.saved.iter().any(|t| t.key == format!("series:{}", m.id))).cloned().collect();
            let groups = calendar_groups(&live, unix_days());
            if groups.is_empty() {
                ui.label(RichText::new("No upcoming episodes for saved series in the next 30 days (or still loading).").color(ui.visuals().weak_text_color()));
            }
            for (head, eps) in groups {
                ui.label(RichText::new(head).strong());
                for (m, v) in eps {
                    let text = format!("{}  S{}E{}  {}", m.name, v.season.unwrap_or(0), v.episode.unwrap_or(0), v.title.as_deref().unwrap_or(""));
                    if ui.link(text).clicked() {
                        cal_open = Some(m.clone());
                    }
                }
            }
        });
        if let Some(m) = cal_open {
            self.tab = Tab::Home;
            self.open_meta(&ctx, m, false);
        }
        ui.separator();
        ui.label(RichText::new("Downloads").size(20.0).strong().family(bold()).color(ui.visuals().strong_text_color()));
        if self.downloads.is_empty() && self.queue.is_empty() {
            ui.label(RichText::new("Nothing downloaded yet. Open a title and choose \"Download\" on a stream.").color(ui.visuals().weak_text_color()));
        }
        // (index, 0 pause, 1 resume, 2 remove)
        let mut act = None;
        for (i, j) in self.queue.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(&j.d.title);
                let (txt, frac) = match j.state {
                    State::Queued => ("Queued".to_string(), 0.0),
                    State::Active if j.total == 0 => ("Starting".to_string(), 0.0),
                    State::Failed => (format!("Failed: {}", j.err.as_deref().unwrap_or("interrupted")), 0.0),
                    s => {
                        let (got, total) = (fmt_size(j.got), fmt_size(j.total));
                        let speed = if s == State::Active { format!(" - {}/s", fmt_size(j.speed as u64)) } else { " - Paused".into() };
                        (format!("{got} / {total}{speed}"), j.got as f32 / j.total.max(1) as f32)
                    }
                };
                ui.add(egui::ProgressBar::new(frac).desired_width(160.0).text(txt));
                match j.state {
                    State::Active | State::Queued if small_button(ui, "Pause").clicked() => act = Some((i, 0)),
                    State::Paused | State::Failed if small_button(ui, "Resume").clicked() => act = Some((i, 1)),
                    _ => {}
                }
                if small_button(ui, if j.state == State::Failed { "Remove" } else { "Cancel" }).clicked() {
                    act = Some((i, 2));
                }
            });
        }
        match act {
            Some((i, 0)) => self.stop_download(&ctx, i, State::Paused),
            Some((i, 1)) => {
                self.set_job_state(i, State::Queued);
                self.pump_downloads(&ctx);
            }
            Some((i, _)) => self.remove_download(&ctx, i),
            None => {}
        }
        let (mut play, mut delete) = (None, None);
        for d in &self.downloads {
            ui.horizontal(|ui| {
                // Stale if the file was removed outside the app; Delete still clears the row.
                let on_disk = d.path.exists();
                let btn = ui.add_enabled_ui(on_disk, |ui| primary_button(ui, &format!("Play {}", d.title))).inner;
                if btn.on_disabled_hover_text("File is missing").clicked() {
                    play = Some(d.clone());
                }
                if small_button(ui, "Delete").clicked() {
                    delete = Some(d.clone());
                }
                ui.label(d.path.display().to_string());
            });
        }
        if let Some(d) = play {
            self.play(&ctx, d.key(), d.title, d.path.to_string_lossy().into_owned(), vec![]);
        }
        if let Some(d) = delete {
            let gone = std::fs::remove_file(&d.path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) });
            match gone.map_err(anyhow::Error::from).and_then(|()| self.db.remove_download(&d)) {
                Ok(()) => {
                    self.downloads.retain(|x| x.path != d.path);
                    self.recent = self.db.recent_progress(20).unwrap_or_default();
                }
                Err(e) => report(&mut self.error, format!("delete {}: {e:#}", d.path.display())),
            }
        }
        ui.separator();
        let tip = "Or drop a .torrent file on the window, or paste a magnet anywhere";
        if form_row(ui, "Torrent:", &mut self.magnet, "magnet:?xt=urn:btih:... or /path/to/file.torrent", "Play torrent", Some(tip), false) {
            let m = self.magnet.clone();
            self.play_torrent(&ctx, &m);
        }
        if form_row(ui, "Folder path:", &mut self.folder, "/path/to/videos", "Add folder", None, false) {
            let dir = PathBuf::from(self.folder.trim());
            // Off the UI thread: a big folder or network share can take minutes.
            self.spawn(&ctx, async move {
                let res = tokio::task::spawn_blocking({
                    let dir = dir.clone();
                    move || scan(&dir).map_err(|e| e.to_string())
                })
                .await?;
                Ok(Msg::Scanned(dir, res))
            });
        }
        ui.separator();
        let mut play = None;
        for it in self.library.iter().filter(|it| title_matches(&it.title, &self.lib_query)) {
            ui.horizontal(|ui| {
                if primary_button(ui, &format!("Play {}", it.title)).clicked() {
                    play = Some(it.clone());
                }
                ui.label(it.path.display().to_string());
            });
        }
        if let Some(it) = play {
            let p = it.path.to_string_lossy().into_owned();
            self.play(&ctx, format!("file:{p}"), it.title, p, vec![]);
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
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // egui's built-in zoom keys change the factor too: bound it, then persist any change.
        let zoom = clamp_scale(ctx.zoom_factor());
        if zoom != ctx.zoom_factor() {
            ctx.set_zoom_factor(zoom);
        }
        if zoom != self.ui_scale {
            self.ui_scale = zoom;
            if let Err(e) = self.db.set_setting("ui_scale", &zoom.to_string()) {
                report(&mut self.error, format!("saving ui_scale: {e:#}"));
            }
        }
        ctx.input(|i| self.window.track(i.viewport()));
        // Load libmpv off the UI thread once the first frames are up: loading it earlier delayed them by ~140 ms.
        if ctx.cumulative_frame_nr() == 2 {
            std::thread::spawn(stremflix_rs::player::load_mpv);
        }
        // A dropped .torrent file, or a magnet pasted with no text field focused, plays at once.
        // Window systems deliver dropped files only, not dropped text, so magnets come in by paste.
        let (dropped, pasted) = ctx.input(|i| {
            let file = i.raw.dropped_files.iter().filter_map(|f| f.path().to_str().map(String::from)).find(|p| is_torrent_file(p));
            (file, i.events.iter().find_map(|e| if let egui::Event::Paste(t) = e { Some(t.trim().to_string()) } else { None }))
        });
        let pasted = pasted.filter(|t| t.starts_with("magnet:") && ctx.memory(|m| m.focused().is_none()));
        if let Some(t) = dropped.or(pasted) {
            self.play_torrent(&ctx, &t);
        }
        if self.player.is_some() {
            self.ui_player(ui, frame);
            return;
        }

        egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(40, 14)).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            // Fixed-height row so the nav and search pills share one center line.
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 44.0), Layout::left_to_right(Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new("STREMFLIX-RS").size(26.0).strong().family(bold()).color(RED)).selectable(false));
                ui.add_space(20.0);
                // Netflix nav: current tab is bold white on a dark gray pill that slides between tabs.
                let pill_slot = ui.painter().add(egui::Shape::Noop);
                let mut sel_rect = None;
                for (tab, name) in [(Tab::Home, "Home"), (Tab::Discover, "Discover"), (Tab::Addons, "Addons"), (Tab::Library, "Library"), (Tab::Settings, "Settings")] {
                    let text = RichText::new(name).size(15.0);
                    let text = if self.tab == tab { text.strong().family(bold()).color(ui.visuals().strong_text_color()) } else { text.color(ui.visuals().weak_text_color()) };
                    // Width of the bold label, so selecting a tab doesn't shift the ones after it.
                    let w = ui.painter().layout_no_wrap(name.into(), egui::FontId::new(15.0, bold()), Color32::WHITE).size().x + 2.0 * ui.spacing().button_padding.x;
                    let r = ui.add(egui::Button::new(text).fill(Color32::TRANSPARENT).min_size(vec2(w, 34.0))).on_hover_cursor(egui::CursorIcon::PointingHand);
                    if self.tab == tab {
                        sel_rect = Some(r.rect);
                    }
                    if r.clicked() {
                        self.tab = tab;
                    }
                }
                if let Some(target) = sel_rect {
                    let rect = self.pill_rect(&ctx, target);
                    ui.painter().set(pill_slot, egui::epaint::RectShape::filled(rect, 17, tint(ui, 34)));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Search in the same dark gray pill; Enter or the icon runs it.
                    // Right-to-left: the field is added first so the icon sits on its left.
                    let bg = ui.painter().add(egui::Shape::Noop);
                    ui.add_space(12.0);
                    let r = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Titles, people, genres").frame(egui::Frame::NONE.inner_margin(egui::Margin { top: 2, ..Default::default() })).desired_width(170.0)); // top margin: optically center the text; width leaves room for five tabs at the 1000 px default size
                    let (icon, b) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                    b.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Search"));
                    ui.painter().text(icon.center(), egui::Align2::CENTER_CENTER, icons::MAGNIFYING_GLASS, egui::FontId::proportional(18.0), ui.visuals().weak_text_color());
                    let b = b.on_hover_text("Search").on_hover_cursor(egui::CursorIcon::PointingHand);
                    let pill = r.rect.union(b.rect);
                    ui.painter().set(bg, egui::epaint::RectShape::filled(egui::Rect::from_center_size(pos2(pill.center().x, icon.center().y), vec2(pill.width() + 24.0, 34.0)), 17, tint(ui, 34)));
                    let r = r.labelled_by(b.id);
                    if b.clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                        self.tab = Tab::Home;
                        self.search(&ctx);
                    }
                    // Recent searches under the empty, focused box. The popup stays up while the pointer is on it,
                    // since pressing it takes focus from the field before the click lands.
                    let id = ui.make_persistent_id("search_history");
                    let over = ui.ctx().data(|d| d.get_temp::<egui::Rect>(id)).is_some_and(|p| ui.input(|i| i.pointer.hover_pos()).is_some_and(|h| p.contains(h)));
                    if (r.has_focus() || over) && self.search.is_empty() && !self.search_history.is_empty() {
                        let (mut pick, mut del, mut clear) = (None, None, false);
                        let area = egui::Area::new(id).order(egui::Order::Foreground).fixed_pos(pos2(pill.left() - 12.0, pill.bottom() + 12.0)).show(ui.ctx(), |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                ui.set_min_width(220.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Recent searches").small().color(ui.visuals().weak_text_color()));
                                    if ui.small_button("Clear").clicked() {
                                        clear = true;
                                    }
                                });
                                for (i, q) in self.search_history.iter().enumerate() {
                                    ui.horizontal(|ui| {
                                        if ui.add(egui::Button::new(q).frame(false)).clicked() {
                                            pick = Some(q.clone());
                                        }
                                        if ui.add(egui::Button::new(icons::X).frame(false)).on_hover_text("Remove").clicked() {
                                            del = Some(i);
                                        }
                                    });
                                }
                            });
                        });
                        ui.ctx().data_mut(|d| d.insert_temp(id, area.response.rect));
                        if clear || del.is_some() {
                            match del {
                                Some(i) => drop(self.search_history.remove(i)),
                                None => self.search_history.clear(),
                            }
                            self.save_search_history();
                        }
                        if let Some(q) = pick {
                            self.search = q;
                            self.tab = Tab::Home;
                            self.search(&ctx);
                        }
                    } else {
                        ui.ctx().data_mut(|d| d.remove::<egui::Rect>(id));
                    }
                });
            });
            let mut chosen = None;
            if let Some((_, title, _, _, files, sel)) = &mut self.file_pick {
                let mut open = true;
                egui::Window::new(format!("Choose a file: {}", sanitize_text(title, 80))).open(&mut open).collapsible(false).show(&ctx, |ui| {
                    for (i, name, len) in files.iter() {
                        ui.radio_value(sel, *i, format!("{}  ({})", sanitize_text(name, 120), fmt_size(*len)));
                    }
                    if ui.button("Play").clicked() {
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
            if let Some(e) = self.error.clone() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Error: {e}")).color(Color32::from_rgb(230, 90, 90)));
                    if ui.small_button("Dismiss").clicked() {
                        self.error = None;
                    }
                });
            }
            if let Some(n) = self.notice.clone() {
                ui.horizontal(|ui| {
                    ui.label(n);
                    if ui.small_button("Dismiss").clicked() {
                        self.notice = None;
                    }
                });
            }
            if let Some(url) = self.update.clone() {
                ui.horizontal(|ui| {
                    ui.label("A new version is available.");
                    ui.hyperlink_to("Download", url);
                    if ui.small_button("Dismiss").clicked() {
                        self.update = None;
                    }
                });
            }
            ui.add_space(8.0);
            // Back lives outside the scroll area, so it stays put while the detail page scrolls.
            if self.selected.is_some() && self.tab == Tab::Home {
                if pill_button(ui, &format!("{}  Back", icons::ARROW_LEFT), 16.0, vec2(0.0, 40.0), white_fill(), Color32::BLACK).on_hover_text("Back to browse").clicked() {
                    (self.selected, self.details) = (None, None);
                    (self.episodes, self.episode) = (Vec::new(), None);
                    self.scroll_top = true;
                    self.clear_file_pick();
                    self.nav += 1;
                }
                ui.add_space(6.0);
            }
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Home => self.ui_home(ui),
                Tab::Discover => self.ui_discover(ui),
                Tab::Addons => self.ui_addons(ui),
                Tab::Library => self.ui_library(ui),
                Tab::Settings => self.ui_settings(ui),
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&glow::Context>) {
        let _ = self.db.set_setting("window", &self.window.to_string());
        self.stop_player();
    }
}

/// Window size/position restored on launch. Default is a maximized 1000x700 window.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Geo {
    size: [f32; 2],
    pos: Option<[f32; 2]>,
    maximized: bool,
}

impl Default for Geo {
    fn default() -> Self {
        Self { size: [1000.0, 700.0], pos: None, maximized: true }
    }
}

impl std::fmt::Display for Geo {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{},{},{}", self.size[0], self.size[1], u8::from(self.maximized))?;
        self.pos.map_or(Ok(()), |p| write!(f, ",{},{}", p[0], p[1]))
    }
}

impl std::str::FromStr for Geo {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        let n: Vec<f32> = s.split(',').map(|v| v.parse().map_err(|_| ())).collect::<Result<_, _>>()?;
        if !n.iter().all(|v| v.is_finite()) || !matches!(n.len(), 3 | 5) || !(200.0..=16384.0).contains(&n[0]) || !(150.0..=16384.0).contains(&n[1]) {
            return Err(());
        }
        // Off-screen positions (e.g. Windows' -32000 minimized spot) drop the position, keep the size.
        let pos = (n.len() == 5 && n[3].abs() <= 10000.0 && n[4].abs() <= 10000.0).then(|| [n[3], n[4]]);
        Ok(Self { size: [n[0], n[1]], pos, maximized: n[2] != 0.0 })
    }
}

impl Geo {
    /// Fullscreen is transient and a maximized rect is not the restore size, so those keep the last normal rect.
    fn track(&mut self, vp: &egui::ViewportInfo) {
        if vp.minimized == Some(true) || vp.fullscreen == Some(true) {
            return;
        }
        // Unknown (None) keeps the last known state.
        if let Some(m) = vp.maximized {
            self.maximized = m;
        }
        if self.maximized {
            return;
        }
        // Clamp to the ranges from_str accepts, or one bad field would discard the whole record on next launch.
        if let Some(r) = vp.inner_rect {
            self.size = [r.width().clamp(200.0, 16384.0), r.height().clamp(150.0, 16384.0)];
        }
        // shortcut: Wayland reports no outer position, so only size is restored there.
        let pos = vp.outer_rect.map(|r| [r.min.x, r.min.y]).filter(|p| p.iter().all(|v| v.abs() <= 10000.0));
        self.pos = pos.or(self.pos);
    }
}

fn main() -> eframe::Result {
    // Only links count: macOS Finder adds `-psn_…`. A launch without one just focuses the open window.
    let arg = std::env::args_os().nth(1).and_then(|a| a.into_string().ok()).filter(|a| a.get(..10).is_some_and(|p| p.eq_ignore_ascii_case("stremio://")));
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
    #[cfg(windows)]
    std::thread::spawn(instance::register_scheme);
    app.window = app.db.setting("window").ok().flatten().and_then(|s| s.parse().ok()).unwrap_or_default();
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
        egui_extras::install_image_loaders(&cc.egui_ctx);
        install_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, app.light);
        cc.egui_ctx.set_zoom_factor(app.ui_scale);
        app.gl_proc = cc.get_proc_address.clone();
        app.media = media_controls(cc, app.tx.clone());
        app.build_rows(&cc.egui_ctx);
        app.fetch_recent_posters(&cc.egui_ctx);
        app.sync_account(&cc.egui_ctx);
        let client = app.client.clone();
        // A failed check (offline, rate limit) only logs: it shouldn't show as an error.
        app.spawn(&cc.egui_ctx, async move {
            Ok(Msg::Update(client.check_update().await.unwrap_or_else(|e| {
                stremflix_rs::elog!("update check: {e:#}");
                None
            })))
        });
        let (tx, ctx) = (app.tx.clone(), cc.egui_ctx.clone());
        app.tray = tray::spawn(&app.rt, include_bytes!("../assets/icon.png"), move |a| {
            let _ = tx.send(Msg::Tray(a));
            ctx.request_repaint();
        });
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

/// Turn wheel scroll (points) into whole volume steps, keeping the remainder; one wheel line is one step.
fn wheel_steps(acc: &mut f32, dy: f32, line: f32) -> i32 {
    if dy * *acc < 0.0 { *acc = 0.0; } // a direction change starts fresh
    *acc += dy;
    let n = (*acc / line).trunc();
    *acc -= n * line;
    n as i32
}

const SEEK_STEP_RANGE: std::ops::RangeInclusive<u32> = 1..=600;

/// Saved seek step in seconds; a missing, unparsable or out-of-range value falls back to `default`.
fn seek_step(v: Option<String>, default: u32) -> u32 {
    v.and_then(|v| v.parse().ok()).filter(|v| SEEK_STEP_RANGE.contains(v)).unwrap_or(default)
}

/// Draw `text` as a QR code of egui rects (no image files); dark on white so phones scan it in any theme.
fn qr_ui(ui: &mut egui::Ui, text: &str) {
    let Ok(code) = qrcode::QrCode::new(text) else {
        ui.label(RichText::new("Link too long for a QR code").weak());
        return;
    };
    let (w, quiet, px) = (code.width(), 2, 4.0);
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat((w + 2 * quiet) as f32 * px), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, Color32::WHITE);
    for (i, c) in code.to_colors().into_iter().enumerate() {
        if c == qrcode::Color::Dark {
            let p = rect.min + vec2(((i % w) + quiet) as f32, ((i / w) + quiet) as f32) * px;
            ui.painter().rect_filled(egui::Rect::from_min_size(p, egui::Vec2::splat(px)), 0.0, Color32::BLACK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_episodes_seeds_dedupes_and_roundtrips() {
        let mut seen = Vec::new();
        // First sight of a series records its backlog silently.
        assert!(fresh_episodes(&mut seen, "tt1", &["tt1:1:1", "tt1:1:2"]).is_empty());
        assert_eq!(fresh_episodes(&mut seen, "tt1", &["tt1:1:1", "tt1:1:2", "tt1:1:3"]), ["tt1:1:3"]);
        // Each episode notifies once.
        assert!(fresh_episodes(&mut seen, "tt1", &["tt1:1:3"]).is_empty());
        // Another series is seeded on its own.
        assert!(fresh_episodes(&mut seen, "tt2", &["tt2:1:1"]).is_empty());
        // Keys with colons and quotes survive the settings encoding.
        seen.push("a\"b:c|d".into());
        assert_eq!(decode_keys(&encode_keys(&seen)), seen);
    }

    #[test]
    fn markers_survive_the_cap_and_other_series_are_untouched() {
        let mut seen = Vec::new();
        fresh_episodes(&mut seen, "tt1", &["e1", "e2"]);
        fresh_episodes(&mut seen, "tt3", &[]);
        // Many batches for one series push it past its per-series cap.
        for batch in 0..5 {
            let eps: Vec<String> = (0..20).map(|i| format!("b{batch}e{i}")).collect();
            let refs: Vec<&str> = eps.iter().map(String::as_str).collect();
            fresh_episodes(&mut seen, "tt3", &refs);
        }
        let tt3: Vec<&String> = seen.iter().filter(|k| k.starts_with("tt3:") && k.as_str() != "tt3:").collect();
        assert_eq!(tt3.len(), NOTIFIED_PER_SERIES, "capped per series");
        assert_eq!(tt3.last().unwrap().as_str(), "tt3:b4e19", "newest kept, oldest dropped");
        // Markers survive the cap pass, and the other series keeps its keys.
        assert!(seen.contains(&"tt3:".to_string()) && seen.contains(&"tt1:".to_string()));
        assert!(seen.contains(&"tt1:e1".to_string()) && seen.contains(&"tt1:e2".to_string()));
        // A series whose marker survived still notifies its next episode instead of re-seeding.
        assert_eq!(fresh_episodes(&mut seen, "tt3", &["new"]), ["new"]);
        // A window larger than the cap is kept whole (no re-notify of episodes still listed).
        let big: Vec<String> = (0..NOTIFIED_PER_SERIES + 5).map(|i| format!("w{i}")).collect();
        let refs: Vec<&str> = big.iter().map(String::as_str).collect();
        fresh_episodes(&mut seen, "tt3", &refs);
        assert!(fresh_episodes(&mut seen, "tt3", &refs).is_empty());
    }

    #[test]
    fn prune_unsaved_drops_only_unsaved_series() {
        let mut seen = vec!["tt1:".into(), "tt1:e1".into(), "tt10:".into(), "tt10:e1".into(), "kitsu:7:".into(), "kitsu:7:e1".into(), "gone:".into(), "gone:e1".into()];
        prune_unsaved(&mut seen, &["tt1", "kitsu:7"]);
        // tt10 is a different series than tt1 even though its id starts with "tt1".
        assert_eq!(seen, ["tt1:", "tt1:e1", "kitsu:7:", "kitsu:7:e1"]);
    }

    #[test]
    fn empty_window_still_seeds_marker() {
        let mut seen = Vec::new();
        assert!(fresh_episodes(&mut seen, "tt1", &[]).is_empty());
        assert_eq!(fresh_episodes(&mut seen, "tt1", &["tt1:1:5"]), ["tt1:1:5"]);
    }

    #[test]
    fn refetch_does_not_duplicate_rows() {
        let meta = |id: &str| MetaPreview { id: id.into(), kind: "series".into(), name: id.into(), poster: None, background: None, description: None };
        let video = |id: &str| Video { id: id.into(), title: None, released: None, season: Some(1), episode: Some(1), thumbnail: None };
        let mut list = Vec::new();
        upsert_by_id(&mut list, meta("a"), vec![video("a1")]);
        upsert_by_id(&mut list, meta("b"), vec![video("b1")]);
        upsert_by_id(&mut list, meta("a"), vec![video("a2")]);
        assert_eq!(list.iter().map(|(m, _)| m.id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(list[1].1[0].id, "a2");
        // An empty refetch (episodes aged out of the window) removes the old row.
        upsert_by_id(&mut list, meta("a"), vec![]);
        assert_eq!(list.iter().map(|(m, _)| m.id.as_str()).collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn wheel_accumulates_to_steps() {
        let mut acc = 0.0;
        // egui's native line_scroll_speed: one real notch is 40 pt.
        assert_eq!(wheel_steps(&mut acc, 40.0, 40.0), 1);
        assert_eq!((0..5).map(|_| wheel_steps(&mut acc, 40.0, 40.0)).sum::<i32>(), 5);
        assert_eq!(wheel_steps(&mut acc, -80.0, 40.0), -2);
        assert_eq!((0..4).map(|_| wheel_steps(&mut acc, 10.0, 40.0)).sum::<i32>(), 1);
        assert_eq!(wheel_steps(&mut acc, 0.0, 40.0), 0);
        // A +30 leftover must not cancel a fresh -40 notch.
        acc = 30.0;
        assert_eq!(wheel_steps(&mut acc, -40.0, 40.0), -1);
    }

    #[test]
    fn accent_name_decodes_with_fallback() {
        assert_eq!(accent_index(Some("Green")), 2);
        assert_eq!(accent_index(Some("nope")), 0);
        assert_eq!(accent_index(None), 0);
    }

    #[test]
    fn calendar_window() {
        let meta = MetaPreview { id: "tt1".into(), kind: "series".into(), name: "Show".into(), poster: None, background: None, description: None };
        let v = |id: &str, d: Option<&str>, season| Video { id: id.into(), title: None, season: Some(season), episode: Some(1), released: d.map(Into::into), thumbnail: None };
        let today = 19_800; // any fixed day
        let (t, tm, far) = (date_string(today), date_string(today + 1), date_string(today + 5));
        let videos = vec![
            v("far", Some(&format!("{far}T00:00:00.000Z")), 1),
            v("tm", Some(&tm), 1),
            v("today", Some(&t), 1),
            v("past", Some(&date_string(today - 1)), 1),
            v("out", Some(&date_string(today + CALENDAR_DAYS + 1)), 1),
            v("bad", Some("2024-0x-10"), 1),
            v("short", Some("2024"), 1),
            v("none", None, 1),
            v("special", Some(&t), 0),
        ];
        let entries = [(meta, videos)];
        let g = calendar_groups(&entries, today);
        let shape: Vec<(&str, Vec<&str>)> = g.iter().map(|(h, e)| (h.as_str(), e.iter().map(|(_, v)| v.id.as_str()).collect())).collect();
        assert_eq!(shape, vec![("Today", vec!["today"]), ("Tomorrow", vec!["tm"]), (far.as_str(), vec!["far"])]);
        assert!(calendar_groups(&[], today).is_empty());
    }

    #[test]
    fn new_episodes_window() {
        assert_eq!(date_string(0), "1970-01-01");
        assert_eq!(date_string(19782), "2024-02-29");
        let v = |id: &str, season, rel: Option<&str>| Video { id: id.into(), title: None, season, episode: Some(1), released: rel.map(Into::into), thumbnail: None };
        let vs = vec![
            v("old", Some(1), Some("2024-01-01T00:00:00.000Z")),
            v("new", Some(1), Some("2024-03-05T00:00:00.000Z")),
            v("future", Some(1), Some("2024-04-01")),
            v("special", Some(0), Some("2024-03-05")),
            v("undated", Some(1), None),
        ];
        let ids: Vec<_> = recent_episodes(vs, "2024-02-20", "2024-03-06").into_iter().map(|v| v.id).collect();
        assert_eq!(ids, ["new"]);
        // Both ends of the window are inclusive; a day outside either is not.
        let edge = vec![v("before", Some(1), Some("2024-02-19")), v("since", Some(1), Some("2024-02-20")), v("today", Some(1), Some("2024-03-06")), v("after", Some(1), Some("2024-03-07"))];
        let ids: Vec<_> = recent_episodes(edge, "2024-02-20", "2024-03-06").into_iter().map(|v| v.id).collect();
        assert_eq!(ids, ["since", "today"]);
    }

    #[test]
    fn buffered_frac_clamps() {
        assert_eq!(buffered_frac(10.0, 50.0, 100.0), 0.5);
        assert_eq!(buffered_frac(10.0, 0.0, 100.0), 0.1);
        assert_eq!(buffered_frac(10.0, 500.0, 100.0), 1.0);
        assert_eq!(buffered_frac(10.0, 50.0, 0.0), 0.0);
    }

    #[test]
    fn clamp_scale_bounds_and_rounds() {
        assert_eq!(clamp_scale(0.1), 0.75);
        assert_eq!(clamp_scale(9.0), 2.0);
        assert_eq!(clamp_scale(1.1000001), 1.1);
        assert_eq!(clamp_scale(f32::NAN), 1.0);
    }

    #[test]
    fn geo_round_trips_and_rejects_junk() {
        let g = Geo { size: [800.0, 600.0], pos: Some([-10.0, 20.0]), maximized: false };
        assert_eq!(g.to_string().parse(), Ok(g));
        assert_eq!(Geo::default().to_string().parse(), Ok(Geo::default()));
        assert!("1,2,3".parse::<Geo>().is_err() && "x".parse::<Geo>().is_err() && "800,600,0,1".parse::<Geo>().is_err());
        assert!("20000,600,0".parse::<Geo>().is_err() && "800,20000,0".parse::<Geo>().is_err());
        assert_eq!("800,600,0,-32000,-32000".parse(), Ok(Geo { size: [800.0, 600.0], pos: None, maximized: false }));
    }

    #[test]
    fn geo_track_keeps_state_and_stays_parseable() {
        let r = |w, h| egui::Rect::from_min_size(egui::pos2(50_000.0, 5.0), egui::vec2(w, h));
        let vp = |maximized, fullscreen, w, h| egui::ViewportInfo { maximized, fullscreen, inner_rect: Some(r(w, h)), outer_rect: Some(r(w, h)), ..Default::default() };
        let mut g = Geo { maximized: true, ..Default::default() };
        g.track(&vp(None, Some(true), 1920.0, 1080.0));
        g.track(&vp(None, None, 1920.0, 1080.0));
        assert!(g.maximized && g.size == [1000.0, 700.0]);
        g.track(&vp(Some(false), None, 10.0, 99999.0));
        assert!(!g.maximized && g.size == [200.0, 16384.0] && g.pos.is_none());
        assert_eq!(g.to_string().parse(), Ok(g));
    }

    #[test]
    fn saved_view_filters_and_sorts() {
        let t = |key: &str, title: &str| SavedTitle {
            key: key.into(),
            title: title.into(),
            kind: key.split_once(':').unwrap().0.into(),
            poster: None,
            year: None,
            rating: None,
            last_watched: None,
        };
        // Newest first, as stored.
        let items = [t("series:tt2", "beta"), t("movie:tt3", "Alpha"), t("movie:tt1", "gamma")];
        let keys = |v: Vec<&SavedTitle>| v.into_iter().map(|t| t.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(saved_view(&items, LibKind::All, LibSort::Added, "")), ["series:tt2", "movie:tt3", "movie:tt1"]);
        assert_eq!(keys(saved_view(&items, LibKind::Movie, LibSort::Added, "")), ["movie:tt3", "movie:tt1"]);
        // Case-insensitive title order, and the filter holds while sorting.
        assert_eq!(keys(saved_view(&items, LibKind::All, LibSort::Title, "")), ["movie:tt3", "series:tt2", "movie:tt1"]);
        assert_eq!(keys(saved_view(&items, LibKind::Series, LibSort::Title, "")), ["series:tt2"]);
        assert!(saved_view(&items, LibKind::Series, LibSort::Added, "").len() == 1);
        // Watched, rating and year: best first, missing last, ties keep the added order.
        let items = [
            SavedTitle { last_watched: Some(5), year: Some(1999), rating: Some(6.0), ..items[0].clone() },
            items[1].clone(),
            SavedTitle { last_watched: Some(9), year: Some(2020), rating: Some(8.5), ..items[2].clone() },
        ];
        let best_first = ["movie:tt1", "series:tt2", "movie:tt3"];
        for sort in [LibSort::Watched, LibSort::Rating, LibSort::Year] {
            assert_eq!(keys(saved_view(&items, LibKind::All, sort, "")), best_first);
        }
        assert_eq!(parse_year("2010–2015"), Some(2010));
        assert_eq!(parse_year("n/a"), None);
        assert_eq!(saved_view(&[], LibKind::All, LibSort::Title, "").len(), 0);
        // Query combines with kind and sort.
        assert_eq!(keys(saved_view(&items, LibKind::All, LibSort::Title, " A ")), ["movie:tt3", "series:tt2", "movie:tt1"]);
        assert_eq!(keys(saved_view(&items, LibKind::Movie, LibSort::Added, "ALPH")), ["movie:tt3"]);
    }

    #[test]
    fn title_match_folds_case() {
        assert!(title_matches("Amélie", "AMÉL"));
        assert!(title_matches("Straße", "straße"));
        assert!(title_matches("anything", "  "));
        assert!(!title_matches("Alpha", "beta"));
    }

    #[test]
    fn media_events_map_to_player_actions() {
        use souvlaki::{MediaControlEvent as E, MediaPosition, SeekDirection::*};
        let s = Duration::from_secs;
        assert_eq!(media_action(E::Toggle), Some(MediaAction::Cmd(&["cycle", "pause"])));
        assert_eq!(media_action(E::Play), Some(MediaAction::Cmd(&["set", "pause", "no"])));
        assert_eq!(media_action(E::Pause), Some(MediaAction::Cmd(&["set", "pause", "yes"])));
        assert_eq!(media_action(E::Next), Some(MediaAction::Next));
        assert_eq!(media_action(E::Stop), Some(MediaAction::Stop));
        assert_eq!(media_action(E::Seek(Backward)), Some(MediaAction::Seek(-10.0, true)));
        assert_eq!(media_action(E::SeekBy(Forward, s(30))), Some(MediaAction::Seek(30.0, true)));
        assert_eq!(media_action(E::SetPosition(MediaPosition(s(95)))), Some(MediaAction::Seek(95.0, false)));
        assert_eq!(media_action(E::Previous), None);
        assert_eq!(media_action(E::Quit), None);
    }

    #[test]
    fn saved_facts_rejects_non_finite_ratings() {
        let rated = |r: &str| serde_json::from_value::<Meta>(serde_json::json!({"id": "tt1", "type": "movie", "name": "x", "imdbRating": r})).unwrap();
        assert_eq!(saved_facts(&rated("NaN")).1, None);
        assert_eq!(saved_facts(&rated("inf")).1, None);
        assert_eq!(saved_facts(&rated("infinity")).1, None);
        assert_eq!(saved_facts(&rated("11")).1, None);
        assert_eq!(saved_facts(&rated("8.5")).1, Some(8.5));
    }

    #[test]
    fn episodes_sort_with_specials_last() {
        let v = |season, episode| Video { id: format!("tt1:{season:?}:{episode}"), title: None, season, episode: Some(episode), released: None, thumbnail: None };
        let mut eps = vec![v(Some(0), 1), v(None, 1), v(Some(2), 1), v(Some(1), 2), v(Some(1), 1)];
        sort_episodes(&mut eps);
        let order: Vec<_> = eps.iter().map(|e| (e.season, e.episode)).collect();
        assert_eq!(order, [(Some(1), Some(1)), (Some(1), Some(2)), (Some(2), Some(1)), (Some(0), Some(1)), (None, Some(1))]);
        assert_eq!((season_label(Some(0)), season_label(Some(3))), ("Specials".to_string(), "Season 3".to_string()));
    }

    #[test]
    fn next_episode_in_watch_order() {
        let v = |season, episode| Video { id: format!("{season:?}:{episode}"), title: None, season, episode: Some(episode), released: None, thumbnail: None };
        let mut eps = vec![v(Some(1), 1), v(Some(1), 2), v(Some(2), 1), v(Some(0), 1), v(Some(0), 2), v(None, 1)];
        sort_episodes(&mut eps);
        let next = |id: &str| next_episode(&eps, id).map(|n| n.id.as_str());
        assert_eq!(next("Some(1):2"), Some("Some(2):1"), "crosses seasons");
        assert_eq!(next("Some(1):1"), Some("Some(1):2"));
        assert_eq!(next("Some(2):1"), None, "finale doesn't roll into specials");
        assert_eq!(next("Some(0):1"), Some("Some(0):2"));
        assert_eq!(next("None:1"), None, "last video");
        assert_eq!(next("missing"), None);
        let start = |seen: &[&str]| start_episode(&eps, |v| seen.contains(&v.id.as_str())).map(|n| n.id.as_str());
        assert_eq!(start(&[]), Some("Some(1):1"), "first regular episode");
        assert_eq!(start(&["Some(1):1", "Some(1):2"]), Some("Some(2):1"), "after the last watched");
        assert_eq!(start(&["Some(2):1"]), Some("Some(1):1"), "finale watched: start over");
    }

    #[test]
    fn series_id_strips_season_and_episode() {
        assert_eq!(series_id("tt1:1:2"), "tt1");
        assert_eq!(series_id("tt1"), "tt1");
        assert_eq!(series_id("kitsu:1234:5"), "kitsu:1234");
        assert_eq!(series_id("x:a:b"), "x:a:b");
    }

    #[test]
    fn row_wants_more_only_when_it_can() {
        let m = |id: &str| MetaPreview { id: id.into(), kind: "movie".into(), name: id.into(), poster: None, background: None, description: None };
        let row = Row { title: String::new(), src: Default::default(), items: Some(vec![m("a")]), skippable: true, fetched: 1, loading_more: false, exhausted: false };
        assert!(row.wants_more(true));
        assert!(!row.wants_more(false), "not near the end");
        assert!(!Row { skippable: false, ..clone_row(&row) }.wants_more(true), "no skip extra");
        assert!(!Row { loading_more: true, ..clone_row(&row) }.wants_more(true), "in flight");
        assert!(!Row { exhausted: true, ..clone_row(&row) }.wants_more(true), "exhausted");
        assert!(!Row { items: None, ..clone_row(&row) }.wants_more(true), "still loading");
        assert!(!Row { items: Some((0..ROW_CAP).map(|i| m(&i.to_string())).collect()), ..clone_row(&row) }.wants_more(true), "capped");
    }

    fn clone_row(r: &Row) -> Row {
        Row { title: String::new(), src: Default::default(), items: r.items.clone(), skippable: r.skippable, fetched: r.fetched, loading_more: r.loading_more, exhausted: r.exhausted }
    }

    #[test]
    fn binge_source_matches_group() {
        let s = |u: &str, g: Option<&str>| Stream { url: Some(u.into()), hints: stremflix_rs::addon::BehaviorHints { binge_group: g.map(String::from), ..Default::default() }, ..Default::default() };
        let streams = [s("https://a/1", None), s("javascript:x", Some("hd")), s("https://a/2", Some("sd")), s("https://a/3", Some("hd"))];
        assert_eq!(binge_source(&streams, "hd").as_deref(), Some("https://a/3"), "skips unplayable");
        assert_eq!(binge_source(&streams, "4k"), None);
        assert!(at_end(99.5, 100.0, true) && !at_end(99.5, 100.0, false) && !at_end(50.0, 100.0, true) && !at_end(0.0, 0.0, true));
    }

    #[test]
    fn request_headers_only_for_own_http_stream() {
        let h = vec![("Referer".to_string(), "https://a.example/".to_string())];
        let s = |u: &str| Stream { url: Some(u.into()), hints: stremflix_rs::addon::BehaviorHints { request_headers: h.clone(), ..Default::default() }, ..Default::default() };
        let magnet = format!("magnet:?xt=urn:btih:{}", "a".repeat(40));
        let streams = [s("https://a.example/v.mp4"), s(&magnet)];
        assert_eq!(request_headers(&streams, "https://a.example/v.mp4", "https://a.example/v.mp4"), h);
        assert!(request_headers(&streams, "https://b.example/v.mp4", "https://b.example/v.mp4").is_empty());
        assert!(request_headers(&streams, &magnet, "http://127.0.0.1:1234/stream").is_empty(), "torrent");
        assert!(request_headers(&streams, "/media/v.mkv", "/media/v.mkv").is_empty(), "local file");
        assert_eq!((fmt_size(734_003_200), fmt_size(2_345_000_000)), ("734 MB".to_string(), "2.35 GB".to_string()));
    }

    fn test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(dir.path().into()).unwrap();
        (dir, app)
    }

    #[test]
    fn stale_replies_are_dropped() {
        let (_dir, mut app) = test_app();
        let ctx = egui::Context::default();
        let m = |id: &str| MetaPreview { id: id.into(), kind: "movie".into(), name: id.into(), poster: None, background: None, description: None };
        app.nav = 2;
        app.handle(&ctx, Msg::Streams(m("a"), None, vec![], vec![], Autoplay::Off, None, 1));
        assert!(app.selected.is_none(), "reply for an older navigation");
        let series = stremflix_rs::addon::parse_meta(br#"{"meta":{"id":"s","type":"series","name":"S","videos":[{"id":"s:1:1","season":1,"episode":1}]}}"#).unwrap();
        app.handle(&ctx, Msg::Episodes(MetaPreview { kind: "series".into(), ..m("s") }, series, 1));
        assert!(app.selected.is_none() && app.episodes.is_empty());
        app.handle(&ctx, Msg::Play("movie:a".into(), "a".into(), "https://x.example/a".into(), "https://x.example/a".into(), vec![], None, 1));
        assert!(app.player.is_none());
        app.handle(&ctx, Msg::Streams(m("b"), None, vec![], vec![], Autoplay::Off, None, 2));
        assert_eq!(app.selected.as_ref().map(|(m, ..)| m.id.as_str()), Some("b"), "current reply applies");
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
    fn filter_subs_matches_lang_or_addon() {
        let f = |addon: &str, lang: &str| SubFound { addon: addon.into(), lang: lang.into(), url: String::new() };
        let all = [f("OpenSubtitles", "eng"), f("Other", "por-BR"), f("Other", "fre")];
        assert_eq!(filter_subs(&all, " ENG ").len(), 1);
        assert_eq!(filter_subs(&all, "other").len(), 2);
        assert_eq!(filter_subs(&all, "").len(), 3);
        assert!(filter_subs(&all, "xx").is_empty());
    }

    #[tokio::test]
    async fn hashed_subs_put_matches_first() {
        let base = test_addon::spawn(test_addon::Config { media_dir: None, torrent: None }).unwrap();
        let hints = stremflix_rs::addon::BehaviorHints { video_hash: Some("8e245d9679d31e12".into()), ..Default::default() };
        let l = |hints| SubLookup { addons: vec![format!("{base}/manifest.json")], kind: "movie".into(), id: test_addon::META_ID.into(), hints, headers: vec![] };
        let test_en = (format!("{base}/subs/test-en.vtt"), "eng".to_string());
        let old = vec![("https://x.example/a.srt".to_string(), "fre".to_string()), test_en.clone()];
        let out = hashed_subs(AddonClient::new(), l(hints.clone()), "/local/file.mkv", old.clone()).await;
        assert_eq!(out, [test_en, old[0].clone()], "match first, deduped by URL");
        let out = hashed_subs(AddonClient::new(), l(Default::default()), "/local/file.mkv", old.clone()).await;
        assert_eq!(out, old, "no hash: unchanged");
        // An addon that never answers times out alone; the fast one's match is kept.
        let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut slow = l(hints);
        slow.addons.insert(0, format!("http://{}/manifest.json", hang.local_addr().unwrap()));
        let out = hashed_subs(AddonClient::new(), slow, "/local/file.mkv", vec![]).await;
        assert_eq!(out, [(format!("{base}/subs/test-en.vtt"), "eng".to_string())]);
    }


    #[tokio::test]
    async fn concurrently_keeps_item_order() {
        let out = concurrently(vec![30u64, 0, 10], |ms| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            anyhow::Ok(ms)
        })
        .await;
        assert_eq!(out.into_iter().map(Result::unwrap).collect::<Vec<_>>(), [30, 0, 10]);
    }

    #[test]
    fn home_rows_order_and_keys() {
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let keys = k(&["a", "b", "c", "d"]);
        assert_eq!(arrange_rows(&keys, &k(&["c", "zz", "a", "c"])), [2, 0, 1, 3], "stale and repeated keys ignored, rest keep priority");
        assert_eq!(arrange_rows(&keys, &[]), [0, 1, 2, 3]);
        let odd = k(&["http://h/m.json|movie|top \"x\"", "b"]);
        assert_eq!(decode_keys(&encode_keys(&odd)), odd);
        assert!(decode_keys("garbage").is_empty());
        assert_eq!(row_key(&("u".into(), "movie".into(), "top".into())), "u|movie|top");
    }

    #[test]
    fn version_compare() {
        use stremflix_rs::addon::is_newer;
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("2", "1.9"));
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
        assert!(is_newer("v1.1.0", "1.0.0"));
        assert!(!is_newer("1.0.0-beta", "1.0.0"), "same core, suffix only");
        assert!(is_newer("1.1.0-beta", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0-beta"));
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
    fn deep_link_title_gets_its_name_in_either_order() {
        let ctx = egui::Context::default();
        let meta = |id: &str| stremflix_rs::addon::parse_meta(format!(r#"{{"meta":{{"id":"{id}","type":"movie","name":"Name {id}","description":"About {id}"}}}}"#).as_bytes()).unwrap();
        let name = |app: &App| app.selected.as_ref().map(|(m, ..)| m.name.clone());
        let preview = |id: &str| MetaPreview { id: id.into(), kind: "movie".into(), name: id.into(), poster: None, background: None, description: None };
        // Details first, then streams.
        let (_d, mut app) = test_app();
        app.nav = 1;
        app.details_want = ("movie".into(), "tt1".into());
        app.handle(&ctx, Msg::Details(meta("tt1")));
        app.handle(&ctx, Msg::Streams(preview("tt1"), None, vec![], vec![], Autoplay::Off, None, 1));
        assert_eq!(name(&app).as_deref(), Some("Name tt1"));
        // Streams first, then details; details of another title never rename the selection.
        let (_d, mut app) = test_app();
        app.nav = 1;
        app.handle(&ctx, Msg::Streams(preview("tt1"), None, vec![], vec![], Autoplay::Off, None, 1));
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
    fn torrent_file_detection() {
        assert!(is_torrent_file("/m/a.torrent") && is_torrent_file(r"C:\m\A.TORRENT"));
        for not in ["https://x.example/a.torrent", "magnet:?xt=urn:btih:a.torrent", "/m/a.mkv", "/m/torrent"] {
            assert!(!is_torrent_file(not), "{not}");
        }
    }

    #[test]
    fn screenshot_path_is_filename_safe() {
        let p = screenshot_path(std::path::Path::new("/d"), "A/B: c", 7);
        assert_eq!(p, std::path::Path::new("/d/screenshots/A_B__c-7.png"));
    }

    #[test]
    fn command_words_respect_quotes() {
        assert_eq!(command_words(r#""C:\Program Files\VLC\vlc.exe" --fs"#), [r"C:\Program Files\VLC\vlc.exe", "--fs"]);
        assert_eq!(command_words("  mpv   --fs "), ["mpv", "--fs"]);
        assert_eq!(command_words(r#"a "" b"#), ["a", "", "b"]);
        assert!(command_words("   ").is_empty());
    }

    #[test]
    fn seek_step_validates() {
        assert_eq!(seek_step(Some("15".into()), 10), 15);
        for bad in [None, Some("0".into()), Some("601".into()), Some("-5".into()), Some("x".into())] {
            assert_eq!(seek_step(bad, 10), 10);
        }
    }

    #[test]
    fn only_source_autoplays_only_without_a_choice() {
        let s = |u: &str| Stream { url: Some(u.into()), ..Default::default() };
        let bad = s("ftp://x.example/a"); // unplayable, doesn't count as a choice
        assert_eq!(only_source(&[]), None);
        assert_eq!(only_source(&[bad.clone(), s("https://x.example/a")]).as_deref(), Some("https://x.example/a"));
        assert_eq!(only_source(&[s("https://x.example/a"), s("https://x.example/b")]), None);
    }
}
