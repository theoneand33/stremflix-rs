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
use stremflix_rs::addon::{AddonClient, CatalogDef, Manifest, Meta, MetaPreview, PlayTarget, Stream, Video, parse_magnet_hash, sanitize_text};
use stremflix_rs::db::{Db, Progress, SavedTitle, Watched};
use stremflix_rs::library::{LibraryItem, scan};
use eframe::glow::{self, HasContext};
use stremflix_rs::player::{GlProc, Player, SPEEDS, SUB_COLORS, SubStyle, hwdec_value, prefer_lang, valid_langs};
use stremflix_rs::stremio_import::{self, Import};
use stremflix_rs::torrent::{Profile, TorrentEngine};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Home,
    Discover,
    Addons,
    Library,
    Settings,
}

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

/// Order of the saved-titles list in the Library tab.
#[derive(PartialEq, Clone, Copy)]
enum LibSort {
    Added,
    Title,
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

/// Saved titles of `kind`, newest first unless sorted by title (case-insensitive).
fn saved_view(items: &[SavedTitle], kind: LibKind, sort: LibSort) -> Vec<&SavedTitle> {
    let mut v: Vec<&SavedTitle> = items.iter().filter(|t| kind.matches(&t.kind)).collect();
    if sort == LibSort::Title {
        v.sort_by_key(|t| t.title.to_lowercase());
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
    /// Search title and results.
    Catalog(String, Vec<MetaPreview>),
    /// Row source (addon URL, type, catalog id) and its items or error.
    Row((String, String, String), Result<Vec<MetaPreview>, String>),
    /// Row source, the `skip` it was requested with, and the next page or error.
    More((String, String, String), usize, Result<Vec<MetaPreview>, String>),
    /// Discover grid source and its items or error.
    Discover(DiscoverSrc, Result<Vec<MetaPreview>, String>),
    /// meta, episode (series only), streams, subtitle URLs, what to autoplay on arrival,
    /// the `bingeGroup` to prefer (next episode), and the `nav` it was requested at
    Streams(MetaPreview, Option<Video>, Vec<Stream>, Subs, Autoplay, Option<String>, u64),
    /// series, its full meta (with episodes), and the `nav` it was requested at
    Episodes(MetaPreview, Meta, u64),
    /// Full meta of a movie (genres, cast, ...) for the detail page.
    Details(Meta),
    /// poster URL, image bytes (empty on failure)
    Poster(String, Vec<u8>),
    /// key, title, resumable source, playable URL/path, subtitle URLs, torrent id, the `nav` it was requested at
    Play(String, String, String, String, Subs, Option<usize>, u64),
    /// progress key, poster URL found for a Continue Watching entry
    RecentPoster(String, String),
    /// Stremio storage that was read, and what it holds.
    Imported(Import),
    /// Library folder and its scanned media files.
    Scanned(PathBuf, Result<Vec<LibraryItem>, String>),
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

/// Addon URL, type, catalog id, genre.
type DiscoverSrc = (String, String, String, Option<String>);

/// Discover tab pickers and the grid they loaded.
#[derive(Default)]
struct Discover {
    kind: String,
    /// Addon URL and catalog id.
    cat: (String, String),
    genre: Option<String>,
    src: Option<DiscoverSrc>,
    /// `None` while loading.
    items: Option<Vec<MetaPreview>>,
}

const RED: Color32 = Color32::from_rgb(229, 9, 20);
/// Stremio's purple, used for the player's seek bar.
const ACCENT: Color32 = Color32::from_rgb(123, 91, 245);
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
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), egui::FontId::proportional(15.0), ui.visuals().text_color(), rect.width() - 16.0);
    job.halign = Align::Center; // each line centered around x = 0
    let g = p.layout_job(job);
    p.galley(pos2(rect.center().x, rect.center().y - g.size().y / 2.0), g, Color32::WHITE);
}

fn stream_row(ui: &mut egui::Ui, n: usize, s: &Stream) -> egui::Response {
    let mut parts = [&s.name, &s.title, &s.description].into_iter().flatten().map(|x| x.trim()).filter(|x| !x.is_empty());
    let head = parts.next().unwrap_or("Stream").replace('\n', " ");
    let parts: Vec<&str> = parts.collect();
    // Torrent addons already describe the file (name, size) in their text; don't repeat it.
    let stem = s.hints.filename.as_deref().map(|f| f.rsplit_once('.').map_or(f, |x| x.0)).filter(|f| !f.is_empty());
    let described = stem.is_some_and(|f| parts.iter().any(|p| p.contains(f)));
    let file = if described { String::new() } else { [s.hints.filename.clone(), s.hints.video_size.map(fmt_size)].into_iter().flatten().collect::<Vec<_>>().join("  ·  ") };
    let external = (s.target().is_none()).then_some("Opens in your browser");
    let details: Vec<&str> = parts.into_iter().chain(Some(file.as_str()).filter(|f| !f.is_empty())).chain(external).collect();
    list_row(ui, n, head, details.join("\n"), 0.0)
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
    let p = ui.painter();
    let hg = p.layout(head.clone(), egui::FontId::new(17.0, bold()), Color32::WHITE, wrap);
    let has_details = !details.is_empty();
    let mut job = egui::text::LayoutJob::simple(details, egui::FontId::proportional(14.0), Color32::from_gray(170), wrap);
    job.sections[0].format.line_height = Some(20.0);
    let dg = p.layout_job(job);
    let block = hg.size().y + if has_details { 8.0 + dg.size().y } else { 0.0 };
    let h = (pad * 2.0 + block).max(88.0);
    let (rect, r) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Play {head}")));
    let t = ui.ctx().animate_bool(r.id, r.hovered() || r.has_focus());
    let p = ui.painter();
    // Inset so neighbouring rows' highlights never touch.
    p.rect_filled(rect.shrink2(vec2(4.0, 4.0)), 8, Color32::from_gray(51).gamma_multiply(t));
    let num = p.layout_no_wrap(n.to_string(), egui::FontId::proportional(24.0), Color32::from_gray(210));
    p.galley(pos2(rect.left() + text_x / 2.0 - num.size().x / 2.0, rect.center().y - num.size().y / 2.0), num, Color32::WHITE);
    let top = rect.center().y - block / 2.0;
    p.galley(pos2(rect.left() + text_x, top), hg.clone(), Color32::WHITE);
    p.galley(pos2(rect.left() + text_x, top + hg.size().y + 8.0), dg, Color32::WHITE);
    // Play button: outlined circle that fills white on hover.
    let c = pos2(rect.right() - 48.0, rect.center().y);
    let fg = Color32::from_gray(160).lerp_to_gamma(Color32::WHITE, t);
    p.circle(c, 20.0, Color32::WHITE.gamma_multiply(t), (2.0, fg));
    let tri = Color32::WHITE.lerp_to_gamma(Color32::BLACK, t);
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

/// The addon's `proxyHeaders` for playing `url`, only when it is that addon's own http(s) stream:
/// never for torrents (`url` is then the local torrent server, not `source`) or local files.
fn request_headers(streams: &[Stream], source: &str, url: &str) -> Vec<(String, String)> {
    let own = |s: &&Stream| matches!(s.target(), Some(PlayTarget::Http(u)) if u.as_str() == url);
    (source == url).then(|| streams.iter().find(own)).flatten().map(|s| s.hints.request_headers.clone()).unwrap_or_default()
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

/// mpv's `keep-open` pauses on the last frame at end of file.
fn at_end(pos: f64, dur: f64, paused: bool) -> bool {
    paused && dur > 0.0 && pos >= dur - 1.0
}

/// The playable source when there is exactly one; `None` when there's a choice (or nothing).
fn only_source(streams: &[Stream]) -> Option<String> {
    let mut it = streams.iter().filter_map(Stream::source);
    let first = it.next();
    if it.next().is_some() { None } else { first }
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
    /// Audio & Subtitles panel open.
    menu: bool,
    /// Seek-bar position while dragging, in seconds.
    scrub: Option<f64>,
    /// Screen/idle inhibit, attempted once per unpaused stretch (inner `None`: unavailable); embedded mpv can't do it itself.
    awake: Option<Option<keepawake::KeepAwake>>,
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
    torrent: Arc<tokio::sync::OnceCell<TorrentEngine>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    tab: Tab,
    addons: Vec<(String, Manifest)>,
    addon_url: String,
    /// Addon being reconfigured (old transport URL) and the pasted result URL.
    configuring: Option<(String, String)>,
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
    /// Poster bytes by URL; `None` while loading, empty if the fetch failed.
    posters: HashMap<String, Option<Arc<[u8]>>>,
    folder: String,
    /// Preferred audio / subtitle languages (mpv `alang` / `slang`), as edited; saved to the settings table only when valid.
    langs: [String; 2],
    library: Vec<LibraryItem>,
    /// Titles saved with "Add to library", newest first.
    saved: Vec<SavedTitle>,
    lib_sort: LibSort,
    lib_kind: LibKind,
    recent: Vec<Progress>,
    watched: Vec<Watched>,
    /// Keys (`<type>:<id>`) of movies and episodes marked watched.
    watched_keys: std::collections::HashSet<String>,
    /// Stremio data path to import from; empty = auto-detect.
    import_path: String,
    /// Addon whose details are shown in the Addons tab.
    addon_details: Option<String>,
    /// Torrent download folder as edited (empty = in the data directory); saved when valid.
    torrent_dir: String,
    torrent_cache: u64,
    torrent_profile: Profile,
    /// External player command, e.g. `vlc` or `mpv --fs`.
    external_player: String,
    /// mpv hardware decoding.
    hwdec: bool,
    /// Last error, shown in a dismissible red line until cleared.
    error: Option<String>,
    /// Nav pill animation: tab it's heading to, start rect, target rect, start time.
    pill: Option<(Tab, egui::Rect, egui::Rect, f64)>,
    player: Option<Playing>,
    last_poll: Instant,
    /// GL function loader of eframe's context, for libmpv's OpenGL renderer.
    gl_proc: Option<GlProc>,
    /// Video framebuffer, its texture (owned by egui once registered) and pixel size; reused across playbacks.
    video: Option<(glow::Framebuffer, glow::Texture, egui::TextureId, [usize; 2])>,
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

/// Show `e` in the UI and on stderr (headless runs, CI).
fn report(slot: &mut Option<String>, e: impl std::fmt::Display) {
    eprintln!("error: {e:#}");
    *slot = Some(format!("{e:#}"));
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

impl App {
    fn new(data_dir: PathBuf) -> anyhow::Result<Self> {
        let db = Db::open(&data_dir.join("stremflix-rs.db"))?;
        let (tx, rx) = channel();
        let mut app = Self {
            sub_style: db.setting("sub_style")?.map(|s| SubStyle::decode(&s)).unwrap_or_default(),
            rt: tokio::runtime::Runtime::new()?,
            addons: db.addons()?,
            library: db.library_items()?,
            saved: db.saved_titles()?,
            lib_sort: LibSort::Added,
            lib_kind: LibKind::All,
            recent: db.recent_progress(20)?,
            watched: db.watched(50)?,
            watched_keys: db.watched_keys()?,
            import_path: String::new(),
            addon_details: None,
            torrent_dir: db.setting("torrent_dir")?.unwrap_or_default(),
            torrent_cache: db.setting("torrent_cache")?.and_then(|v| v.parse().ok()).unwrap_or(0),
            torrent_profile: Profile::from_label(&db.setting("torrent_profile")?.unwrap_or_default()),
            external_player: db.setting("external_player")?.unwrap_or_else(|| "vlc".into()),
            hwdec: db.setting("hwdec")?.as_deref() != Some("0"),
            error: None,
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
            configuring: None,
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
            posters: HashMap::new(),
            player: None,
            pill: None,
            last_poll: Instant::now(),
            gl_proc: None,
            video: None,
        };
        if app.addons.is_empty() {
            app.tab = Tab::Addons;
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

    /// Resolve `source` (http URL, magnet, or local path) and start playback.
    fn play(&mut self, ctx: &egui::Context, key: String, title: String, source: String, subs: Subs) {
        self.nav += 1;
        let nav = self.nav;
        if !source.starts_with("magnet:") && !is_torrent_file(&source) {
            let _ = self.tx.send(Msg::Play(key, title, source.clone(), source, subs, None, nav));
            return;
        }
        // A chosen folder gets its own subfolder: the engine clears its folder at start.
        let dir = match self.db.setting("torrent_dir").ok().flatten().filter(|d| !d.is_empty()) {
            Some(d) => PathBuf::from(d).join("stremflix-rs-torrents"),
            None => self.data_dir.join("torrents"),
        };
        let (cell, cache, profile) = (self.torrent.clone(), self.torrent_cache, self.torrent_profile);
        let dht = std::env::var_os("STREMFLIX_RS_NO_DHT").is_none();
        self.spawn(ctx, async move {
            let engine = cell.get_or_try_init(|| TorrentEngine::start(dir, dht, cache, profile)).await?;
            let (url, id) = engine.stream_url(&source, None).await?;
            Ok(Msg::Play(key, title, source, url, subs, Some(id), nav))
        });
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

    /// Stop every torrent but `keep`, so none keeps downloading and seeding. Runs on the UI thread,
    /// so it always finishes before a later `play` adds the next torrent.
    fn release_torrents(&self, keep: Option<usize>) {
        if let Some(engine) = self.torrent.get() {
            // shortcut: blocks the UI up to 2 s while librqbit deletes; fine for a few torrents at a time.
            let _ = self.rt.block_on(tokio::time::timeout(Duration::from_secs(2), engine.release_except(keep)));
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
                self.addons.insert(slot, (url, m));
                if saved.is_ok() {
                    let urls: Vec<String> = self.addons.iter().map(|(u, _)| u.clone()).collect();
                    if let Err(e) = self.db.reorder_addons(&urls) {
                        report(&mut self.error, format!("saving addon order: {e:#}"));
                    }
                }
                self.addon_url.clear();
                self.build_rows(ctx);
                self.fetch_recent_posters(ctx);
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
                // `Only` autoplays when there's no choice to make; otherwise show the stream picker.
                if autoplay != Autoplay::Off {
                    match (src, self.play_key()) {
                        (Some(src), Some((key, title))) => self.play(ctx, key, title, src, subs),
                        _ => self.show_streams = true,
                    }
                }
            }
            Msg::Episodes(meta, mut m, nav) => {
                if nav != self.nav {
                    return;
                }
                let mut videos = std::mem::take(&mut m.videos);
                // A Continue Watching entry only knows the episode title.
                let meta = MetaPreview { name: m.name.clone(), poster: meta.poster.or(m.poster.clone()), ..meta };
                self.details = Some(m);
                sort_episodes(&mut videos);
                // Episodes imported as watched from Stremio: saved once as local marks, so they can be unmarked.
                let bk = format!("bitfield:{}:{}", meta.kind, meta.id);
                if let Ok(Some(field)) = self.db.setting(&bk) {
                    let keys: Vec<String> = stremio_import::watched_videos(&field, &videos).into_iter().map(|id| format!("{}:{id}", meta.kind)).collect();
                    // An anchor missing from this addon's list marks nothing; keep the field for another source.
                    if !keys.is_empty() {
                        match self.db.apply_watched_bitfield(&bk, &keys) {
                            Ok(()) => self.watched_keys.extend(keys),
                            Err(e) => report(&mut self.error, format!("saving watched episodes: {e:#}")),
                        }
                    }
                }
                self.season = videos.first().and_then(|v| v.season);
                (self.episodes, self.episode) = (videos, None);
                self.selected = Some((meta.clone(), Vec::new(), Vec::new()));
                self.scroll_top = true;
                let Some((_, show)) = self.pending.take().filter(|(id, _)| *id == meta.id) else { return };
                let key = |v: &Video| format!("{}:{}", meta.kind, v.id);
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
            Msg::Details(m) => {
                if self.details_want == (m.kind.clone(), m.id.clone()) {
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
            Msg::Imported(imp) => {
                if let Err(e) = imp.save(&self.db) {
                    report(&mut self.error, format!("saving import: {e:#}"));
                    return;
                }
                self.addons = self.db.addons().unwrap_or_default();
                self.recent = self.db.recent_progress(20).unwrap_or_default();
                self.watched = self.db.watched(50).unwrap_or_default();
                self.watched_keys = self.db.watched_keys().unwrap_or_default();
                self.build_rows(ctx);
                self.fetch_recent_posters(ctx);
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
                let lang = |k| self.db.setting(k).ok().flatten().unwrap_or_default();
                let headers = self.selected.as_ref().map(|(_, streams, _)| request_headers(streams, &source, &url)).unwrap_or_default();
                // Remember the episode's series (its key holds only the video id) and the stream's binge group
                // (empty if none), so the series' play button and Continue Watching can find them later.
                if let (Some((m, streams, _)), Some(_)) = (&self.selected, &self.episode)
                    && self.play_key().is_some_and(|(k, _)| k == key)
                {
                    let group = streams.iter().find(|s| s.source().as_ref() == Some(&source)).and_then(|s| s.hints.binge_group.clone());
                    let _ = self.db.set_setting(&format!("series:{key}"), &m.id);
                    let _ = self.db.set_setting(&format!("binge:{}:{}", m.kind, m.id), &group.unwrap_or_default());
                }
                match Player::launch(&url, &title, start, &subs, [&lang("alang"), &lang("slang")], lang("sub_off") == "1", &headers, self.hwdec, gl_proc, Box::new(move || wake.request_repaint())) {
                    Ok(player) => {
                        let _ = player.apply_sub_style(&self.sub_style);
                        let p = Playing { player, key, title, source, url, torrent, pos: start, dur: 0.0, poster, osd_until: ctx.input(|i| i.time) + 3.0, menu: false, scrub: None, awake: None };
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
        if let Some(p) = &self.player {
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
        match self.db.set_watched(key, on) {
            Ok(()) => {
                self.watched_keys = self.db.watched_keys().unwrap_or_default();
                self.watched = self.db.watched(50).unwrap_or_default();
                self.recent = self.db.recent_progress(20).unwrap_or_default();
            }
            Err(e) => report(&mut self.error, format!("saving watched state: {e:#}")),
        }
    }

    fn stop_player(&mut self) {
        self.save_progress();
        self.player = None;
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
        if at_end(st.pos, st.dur, st.paused) && self.playing_episode() {
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
        self.rows = rows
            .into_iter()
            .map(|(title, i)| {
                let (u, _, c) = cats[i];
                Row { title, src: (u.clone(), c.kind.clone(), c.id.clone()), items: None, skippable: c.supports("skip"), fetched: 0, loading_more: false, exhausted: false }
            })
            .collect();
        for src in self.rows.iter().map(|r| r.src.clone()).collect::<Vec<_>>() {
            let client = self.client.clone();
            self.spawn(ctx, async move {
                let res = client.catalog(&src.0, &src.1, &src.2, 0).await.map_err(|e| format!("{e:#}"));
                Ok(Msg::Row(src, res))
            });
        }
    }

    fn search(&mut self, ctx: &egui::Context) {
        let query = self.search.trim().to_string();
        if query.is_empty() {
            return;
        }
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

    /// Add the selected title to the library, or remove it.
    fn toggle_saved(&mut self, meta: &MetaPreview) {
        let key = format!("{}:{}", meta.kind, meta.id);
        let res = if self.saved.iter().any(|t| t.key == key) {
            self.saved.retain(|t| t.key != key);
            self.db.remove_saved_title(&key)
        } else {
            let t = SavedTitle { key, title: meta.name.clone(), kind: meta.kind.clone(), poster: meta.poster.clone() };
            self.saved.insert(0, t.clone());
            self.db.add_saved_title(&t)
        };
        if let Err(e) = res {
            report(&mut self.error, format!("saving library: {e:#}"));
        }
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
                async move { c.subtitles(&u, &k, &i).await }
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
        if let Some(d) = self.selected_details() {
            Self::ui_details(ui, d);
        }
        let mut episode = None;
        let mut toggle: Option<(String, bool)> = None;
        // Movies show their streams; a series shows them once an episode is picked.
        if let Some((_, streams, subs)) = &self.selected
            && let Some(key) = self.play_key()
            && (self.episodes.is_empty() || self.episode.is_some())
        {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let r = ui.label(RichText::new("Streams").size(24.0).strong().family(bold()).color(Color32::WHITE));
                if std::mem::take(&mut self.show_streams) {
                    r.scroll_to_me(Some(Align::TOP));
                }
                ui.label(RichText::new(format!("{}  ·  {} subtitle track{}", key.1, subs.len(), if subs.len() == 1 { "" } else { "s" })).size(15.0).color(Color32::from_gray(170)));
                let seen = self.watched_keys.contains(&key.0);
                if small_button(ui, if seen { "Mark unwatched" } else { "Mark watched" }).clicked() {
                    toggle = Some((key.0.clone(), !seen));
                }
            });
            ui.add_space(8.0);
            for (i, s) in streams.iter().enumerate() {
                if (i == 0 || streams[i - 1].addon != s.addon) && !s.addon.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new(&s.addon).size(17.0).family(bold()).color(Color32::from_gray(200)));
                }
                if stream_row(ui, i + 1, s).clicked() {
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
                let r = ui.label(RichText::new("Episodes").size(24.0).strong().family(bold()).color(Color32::WHITE));
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
                        if card_play(ui, &r, rect, &p.title) {
                            play = Some((p.key.clone(), p.title.clone(), p.source.clone(), vec![]));
                        } else if r.clicked() {
                            resume = Some(p.clone());
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
                ui.label(RichText::new(title).size(20.0).strong().family(bold()).color(Color32::WHITE));
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
        if let Some((m, autoplay)) = open {
            self.open_meta(&ctx, m, autoplay);
        }
        if let Some((m, v)) = episode {
            self.open_streams(&ctx, m, Some(v), Autoplay::Only, None);
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

    /// Pick a type, catalog and genre; the catalog's first page shows as a poster grid.
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
        // Keep the pickers valid as addons change: fall back to the first type, catalog and (if required) genre.
        if !kinds.contains(&d.kind.as_str()) {
            d.kind = kinds[0].into();
        }
        let in_kind: Vec<_> = cats.iter().filter(|(_, _, c)| c.kind == d.kind).collect();
        let cur = *in_kind.iter().find(|(u, _, c)| **u == d.cat.0 && c.id == d.cat.1).unwrap_or(&in_kind[0]);
        d.cat = (cur.0.clone(), cur.2.id.clone());
        let genres = cur.2.genres();
        if !d.genre.as_ref().is_some_and(|g| genres.contains(g)) {
            d.genre = genres.first().filter(|_| cur.2.requires_genre()).cloned();
        }
        let src = (d.cat.0.clone(), d.kind.clone(), d.cat.1.clone(), d.genre.clone());
        if d.src.as_ref() != Some(&src) {
            (d.src, d.items) = (Some(src.clone()), None);
            let client = self.client.clone();
            self.spawn(&ctx, async move {
                let res = client.discover(&src.0, &src.1, &src.2, src.3.as_deref()).await.map_err(|e| format!("{e:#}"));
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
            if !genres.is_empty() {
                egui::ComboBox::from_id_salt("discover_genre").selected_text(d.genre.as_deref().unwrap_or("All genres")).show_ui(ui, |ui| {
                    if !cur.2.requires_genre() {
                        ui.selectable_value(&mut d.genre, None, "All genres");
                    }
                    for g in genres {
                        ui.selectable_value(&mut d.genre, Some(g.clone()), g);
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
        ui.label(RichText::new(title).size(20.0).strong().family(bold()).color(Color32::WHITE));
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
                // Cover-fit: crop the image to the banner's aspect ratio.
                let (ra, ia) = (rect.aspect_ratio(), texture.size.x / texture.size.y);
                let uv = if ia > ra {
                    let f = ra / ia;
                    egui::Rect::from_min_max(pos2((1.0 - f) / 2.0, 0.0), pos2((1.0 + f) / 2.0, 1.0))
                } else {
                    let f = ia / ra;
                    egui::Rect::from_min_max(pos2(0.0, (1.0 - f) / 2.0), pos2(1.0, (1.0 + f) / 2.0))
                };
                img.uv(uv).paint_at(ui, rect);
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

    /// Detail facts under the banner: year, runtime, rating, genres, cast, director, links.
    fn ui_details(ui: &mut egui::Ui, d: &Meta) {
        ui.add_space(14.0);
        let gray = Color32::from_gray(170);
        let facts: Vec<String> = [d.release_info.clone(), d.runtime.clone(), d.imdb_rating.as_ref().map(|r| format!("IMDb {r}"))]
            .into_iter()
            .flatten()
            .chain((!d.genres.is_empty()).then(|| d.genres.join(", ")))
            .collect();
        if !facts.is_empty() {
            ui.label(RichText::new(facts.join("  ·  ")).size(16.0).color(Color32::WHITE));
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
    }

    /// Full-window Netflix-style player: letterboxed video, controls that fade out after
    /// 3 s without input, red seek bar, ±10 s skips, volume, Audio & Subtitles panel.
    fn ui_player(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let has_next = self.playing_episode() && self.episode.as_ref().is_some_and(|c| next_episode(&self.episodes, &c.id).is_some());
        let mut next = false;
        let Some(p) = &mut self.player else { return };
        let now = ctx.input(|i| i.time);
        let fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let mut cmds: Vec<Vec<String>> = Vec::new();
        let mut cmd = |a: &[&str]| cmds.push(a.iter().map(|s| s.to_string()).collect());
        let (mut back, mut toggle_fs, mut external) = (false, false, false);
        let mut seek: Option<(f64, bool)> = None;
        let mut style = self.sub_style;
        ui.painter().rect_filled(rect, 0, Color32::BLACK);

        // Video: mpv renders on the GPU into our framebuffer at on-screen pixel size.
        let [vw, vh] = p.player.state.video;
        if let (true, Some(gl)) = (vw > 0 && vh > 0, frame.gl().cloned()) {
            let fit = (rect.width() / vw as f32).min(rect.height() / vh as f32);
            let vrect = egui::Rect::from_center_size(rect.center(), vec2(vw as f32, vh as f32) * fit);
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
        if bg.clicked() {
            if p.menu { p.menu = false } else { cmd(&["cycle", "pause"]) }
        }
        toggle_fs |= bg.double_clicked();
        ctx.input(|i| {
            use egui::Key::*;
            let k = |key| i.key_pressed(key);
            if k(Space) || k(K) { cmd(&["cycle", "pause"]) }
            if k(ArrowLeft) { seek = Some((-10.0, true)) }
            if k(ArrowRight) { seek = Some((10.0, true)) }
            if k(ArrowUp) { cmd(&["add", "volume", "5"]) }
            if k(ArrowDown) { cmd(&["add", "volume", "-5"]) }
            if k(M) { cmd(&["cycle", "mute"]) }
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
        let show = st.paused || p.menu || p.scrub.is_some() || now < p.osd_until;
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
            paint.rect_filled(egui::Rect::from_min_size(track.min, vec2(track.width() * frac, th)), 2, ACCENT.gamma_multiply(a));
            let knob = pos2(track.left() + track.width() * frac, track.center().y);
            paint.circle_filled(knob, if hot { 9.0 } else { 7.0 }, ACCENT.gamma_multiply(a));
            paint.circle_filled(knob, 3.0, white);
            if let Some(h) = r.hover_pos().filter(|_| st.dur > 0.0) {
                paint.text(pos2(h.x, bar.top() - 6.0), egui::Align2::CENTER_BOTTOM, fmt_time(at(h.x)), font.clone(), white);
            }
            paint.text(pos2(rect.left() + 24.0, by), egui::Align2::LEFT_CENTER, fmt_time(pos), font.clone(), white);
            paint.text(pos2(rect.right() - 24.0, by), egui::Align2::RIGHT_CENTER, fmt_time(st.dur), font, white);

            // Control row: play, next episode, volume on the left; audio & subtitles on the right.
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
            let cc = icon(ui, pos2(rect.right() - 52.0, y), icons::SUBTITLES, "Audio & Subtitles");
            if cc.clicked() {
                p.menu = !p.menu;
            }

            // Audio & Subtitles panel: two columns, selected track marked with a check.
            if p.menu {
                egui::Area::new(ui.scope_id().with("tracks")).order(egui::Order::Foreground).pivot(egui::Align2::RIGHT_BOTTOM).fixed_pos(pos2(rect.right() - 24.0, y - 76.0)).show(&ctx, |ui| {
                    egui::Frame::new().fill(Color32::from_rgba_unmultiplied(38, 38, 38, 240)).corner_radius(6).inner_margin(20).show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            for (kind, head, prop) in [("audio", "Audio", "aid"), ("sub", "Subtitles", "sid")] {
                                ui.vertical(|ui| {
                                    ui.set_min_width(200.0);
                                    ui.label(RichText::new(head).size(20.0).family(bold()).color(Color32::WHITE));
                                    ui.add_space(8.0);
                                    let tracks: Vec<_> = st.tracks.iter().filter(|t| t.kind == kind).collect();
                                    let off = !tracks.iter().any(|t| t.selected);
                                    let entries = (kind == "sub").then_some((None, "Off", None, off)).into_iter().chain(tracks.iter().map(|t| (Some(t.id), t.label.as_str(), t.lang.as_deref(), t.selected)));
                                    for (id, label, lang, sel) in entries {
                                        let text = RichText::new(format!("{}  {label}", if sel { icons::CHECK } else { "   " })).size(16.0);
                                        let text = if sel { text.color(Color32::WHITE).family(bold()) } else { text.color(Color32::from_gray(180)) };
                                        if ui.add(egui::Button::new(text).frame(false)).clicked() {
                                            let v = id.map_or("no".to_string(), |i| i.to_string());
                                            cmd(&["set", prop, &v]);
                                            // Remember the subtitle choice for the next video: Off, or the picked language first in `slang`.
                                            if kind == "sub" {
                                                let mut save = vec![("sub_off", if id.is_none() { "1" } else { "" }.to_string())];
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
                                    if kind == "sub" {
                                        ui.add_space(12.0);
                                        ui.label(RichText::new("Style").size(16.0).family(bold()).color(Color32::WHITE));
                                        ui.horizontal(|ui| {
                                            ui.add_sized([90.0, 20.0], egui::Label::new(format!("Size {:.1}x", style.scale)));
                                            if ui.button("−").clicked() { style.scale -= 0.1 }
                                            if ui.button("+").clicked() { style.scale += 0.1 }
                                        });
                                        ui.horizontal(|ui| {
                                            for (i, (name, _)) in SUB_COLORS.iter().enumerate() {
                                                if ui.selectable_label(style.color == i, *name).clicked() { style.color = i }
                                            }
                                        });
                                        ui.checkbox(&mut style.background, "Background box");
                                        ui.horizontal(|ui| {
                                            ui.add_sized([90.0, 20.0], egui::Label::new(format!("Delay {:+.1}s", style.delay)));
                                            if ui.button("−").clicked() { style.delay -= 0.1 }
                                            if ui.button("+").clicked() { style.delay += 0.1 }
                                            if ui.button("Reset").clicked() { style.delay = 0.0 }
                                        });
                                    }
                                    ui.add_space(8.0);
                                    let (i, key) = if kind == "audio" { (0, "alang") } else { (1, "slang") };
                                    let valid = valid_langs(&self.langs[i]);
                                    let l = ui.label(RichText::new(format!("Preferred {} languages", if kind == "audio" { "audio" } else { "subtitle" })).size(13.0).color(Color32::from_gray(180)));
                                    let r = ui.add(egui::TextEdit::singleline(&mut self.langs[i]).hint_text("Preferred: en,eng").desired_width(200.0)).labelled_by(l.id);
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
                                        && let Err(e) = self.db.set_setting("sub_off", "")
                                    {
                                        report(&mut self.error, format!("saving sub_off: {e:#}"));
                                    }
                                    if valid.is_none() {
                                        ui.label(RichText::new("Use codes like en,eng,pt-BR (max 8)").size(13.0).color(Color32::from_rgb(230, 90, 90)));
                                    }
                                });
                            }
                            ui.vertical(|ui| {
                                ui.set_min_width(200.0);
                                ui.label(RichText::new("Playback").size(20.0).family(bold()).color(Color32::WHITE));
                                ui.add_space(8.0);
                                ui.label(RichText::new("Speed").size(16.0).family(bold()).color(Color32::WHITE));
                                ui.horizontal_wrapped(|ui| {
                                    for sp in SPEEDS {
                                        if ui.selectable_label((st.speed - sp).abs() < 0.01, format!("{sp}x")).clicked() {
                                            cmd(&["set", "speed", &sp.to_string()]);
                                        }
                                    }
                                });
                                ui.add_space(8.0);
                                if ui.checkbox(&mut self.hwdec, "Hardware decoding").changed() {
                                    cmd(&["set", "hwdec", hwdec_value(self.hwdec)]);
                                    if let Err(e) = self.db.set_setting("hwdec", if self.hwdec { "1" } else { "0" }) {
                                        report(&mut self.error, format!("saving hwdec: {e:#}"));
                                    }
                                }
                                ui.add_space(8.0);
                                external = ui.button("Open in external player").on_hover_text(format!("Pause here and open in: {}", self.external_player)).clicked();
                            });
                        });
                    });
                });
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
            let (client, url) = (self.client.clone(), self.addon_url.trim().to_string());
            self.spawn(&ctx, async move {
                let m = client.fetch_manifest(&url).await?;
                Ok(Msg::Installed(url, m, None))
            });
        }
        // Blank path = auto-detect.
        if form_row(ui, "Stremio data:", &mut self.import_path, "Auto-detect local Stremio", "Import from Stremio", Some("Import addons, watched titles and progress"), true) {
            let path = Some(PathBuf::from(self.import_path.trim())).filter(|p| !p.as_os_str().is_empty());
            self.spawn(&ctx, async move {
                let (_, imp) = tokio::task::spawn_blocking(move || stremio_import::import(path.as_deref())).await??;
                Ok(Msg::Imported(imp))
            });
        }
        ui.separator();
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
            if m.behavior_hints.configuration_required {
                ui.label(RichText::new("Needs configuration before use. Press Configure.").weak());
            }
            if self.addon_details.as_ref() == Some(url) {
                Self::ui_addon_details(ui, m);
            }
            if let Some((old, pasted)) = self.configuring.as_mut().filter(|(o, _)| o == url)
                && form_row(ui, "Configured URL:", pasted, "https://.../manifest.json", "Replace", None, false)
            {
                replace = Some((old.clone(), pasted.trim().to_string()));
            }
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
    }

    /// Manifest facts under an addon row: id, types, resources, catalogs, description.
    fn ui_addon_details(ui: &mut egui::Ui, m: &Manifest) {
        // Types and resource names are not sanitized at parse time; strip control and bidi characters here.
        let list = |v: Vec<&str>| v.into_iter().map(|t| sanitize_text(t, 64)).collect::<Vec<_>>().join(", ");
        let catalogs: Vec<&str> = m.catalogs.iter().map(|c| c.name.as_deref().unwrap_or(&c.id)).collect();
        let gray = Color32::from_gray(170);
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
                ui.add(egui::Label::new(RichText::new(d).color(Color32::WHITE)).wrap());
            }
        });
    }

    /// Torrent engine and external player options; each is saved as soon as it changes.
    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let mut save: Vec<(&str, String)> = Vec::new();
        ui.add_space(8.0);
        ui.label(RichText::new("Torrents").size(20.0).strong().family(bold()).color(Color32::WHITE));
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
        let note = "Stopped torrents stay on disk up to the cache size, and are cleared when the app starts.";
        ui.label(RichText::new(note).size(13.0).color(Color32::from_gray(170)));
        if self.torrent.get().is_some() {
            ui.label(RichText::new("A torrent has already played: restart the app to apply changes.").size(13.0).color(Color32::from_gray(170)));
        }
        ui.add_space(16.0);
        ui.label(RichText::new("Player").size(20.0).strong().family(bold()).color(Color32::WHITE));
        if form_row(ui, "External player:", &mut self.external_player, "vlc", "Save", Some("Command run with the stream URL; no shell"), true) {
            save.push(("external_player", self.external_player.trim().to_string()));
        }
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
            ui.label(RichText::new("Saved titles").size(20.0).strong().family(bold()).color(Color32::WHITE));
            egui::ComboBox::from_id_salt("lib_sort").selected_text(self.lib_sort.label()).show_ui(ui, |ui| {
                for s in [LibSort::Added, LibSort::Title] {
                    ui.selectable_value(&mut self.lib_sort, s, s.label());
                }
            });
            egui::ComboBox::from_id_salt("lib_kind").selected_text(self.lib_kind.label()).show_ui(ui, |ui| {
                for k in [LibKind::All, LibKind::Movie, LibKind::Series] {
                    ui.selectable_value(&mut self.lib_kind, k, k.label());
                }
            });
        });
        ui.add_space(4.0);
        let items: Vec<MetaPreview> = saved_view(&self.saved, self.lib_kind, self.lib_sort).into_iter().filter_map(SavedTitle::preview).collect();
        if self.saved.is_empty() {
            ui.label(RichText::new("Nothing saved yet. Open a title and choose \"Add to library\".").color(Color32::from_gray(170)));
        } else if items.is_empty() {
            ui.label(RichText::new("No saved titles of this type.").color(Color32::from_gray(170)));
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
        for it in &self.library {
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
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(&ctx, msg);
        }
        self.poll_player(&ctx);
        ctx.request_repaint_after(Duration::from_millis(500));
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
                    let text = if self.tab == tab { text.strong().family(bold()).color(Color32::WHITE) } else { text.color(Color32::from_gray(180)) };
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
                    ui.painter().set(pill_slot, egui::epaint::RectShape::filled(rect, 17, Color32::from_gray(51)));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Search in the same dark gray pill; Enter or the icon runs it.
                    // Right-to-left: the field is added first so the icon sits on its left.
                    let bg = ui.painter().add(egui::Shape::Noop);
                    ui.add_space(12.0);
                    let r = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Titles, people, genres").frame(egui::Frame::NONE.inner_margin(egui::Margin { top: 2, ..Default::default() })).desired_width(170.0)); // top margin: optically center the text; width leaves room for five tabs at the 1000 px default size
                    let (icon, b) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                    b.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Search"));
                    ui.painter().text(icon.center(), egui::Align2::CENTER_CENTER, icons::MAGNIFYING_GLASS, egui::FontId::proportional(18.0), Color32::from_gray(180));
                    let b = b.on_hover_text("Search").on_hover_cursor(egui::CursorIcon::PointingHand);
                    let pill = r.rect.union(b.rect);
                    ui.painter().set(bg, egui::epaint::RectShape::filled(egui::Rect::from_center_size(pos2(pill.center().x, icon.center().y), vec2(pill.width() + 24.0, 34.0)), 17, Color32::from_gray(51)));
                    let r = r.labelled_by(b.id);
                    if b.clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                        self.tab = Tab::Home;
                        self.search(&ctx);
                    }
                });
            });
            if let Some(e) = self.error.clone() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Error: {e}")).color(Color32::from_rgb(230, 90, 90)));
                    if ui.small_button("Dismiss").clicked() {
                        self.error = None;
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
        self.stop_player();
    }
}

fn main() -> eframe::Result {
    let mut app = App::new(data_dir()).expect("failed to initialise app data");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_maximized(true)
            .with_title("Stremflix-rs")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).expect("bundled icon is a valid PNG")),
        ..Default::default()
    };
    eframe::run_native(
        "Stremflix-rs",
        opts,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            install_fonts(&cc.egui_ctx);
            let mut v = egui::Visuals::dark();
            v.panel_fill = Color32::from_rgb(20, 20, 20);
            v.window_fill = v.panel_fill;
            v.extreme_bg_color = Color32::from_gray(32);
            v.selection.bg_fill = RED;
            // Netflix-style flat buttons: no outline or grow on hover, gray that dims when hovered.
            let w = &mut v.widgets;
            for (st, gray) in [(&mut w.inactive, 82), (&mut w.hovered, 56), (&mut w.active, 45), (&mut w.open, 56)] {
                (st.weak_bg_fill, st.bg_stroke, st.corner_radius, st.expansion) = (Color32::from_gray(gray), egui::Stroke::NONE, 4.into(), 0.0);
                st.fg_stroke.color = Color32::WHITE;
            }
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            cc.egui_ctx.set_visuals_of(egui::Theme::Dark, v);
            cc.egui_ctx.style_mut_of(egui::Theme::Dark, |s| s.spacing.button_padding = vec2(14.0, 6.0));
            app.gl_proc = cc.get_proc_address.clone();
            app.build_rows(&cc.egui_ctx);
            app.fetch_recent_posters(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_view_filters_and_sorts() {
        let t = |key: &str, title: &str| SavedTitle {
            key: key.into(),
            title: title.into(),
            kind: key.split_once(':').unwrap().0.into(),
            poster: None,
        };
        // Newest first, as stored.
        let items = [t("series:tt2", "beta"), t("movie:tt3", "Alpha"), t("movie:tt1", "gamma")];
        let keys = |v: Vec<&SavedTitle>| v.into_iter().map(|t| t.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(saved_view(&items, LibKind::All, LibSort::Added)), ["series:tt2", "movie:tt3", "movie:tt1"]);
        assert_eq!(keys(saved_view(&items, LibKind::Movie, LibSort::Added)), ["movie:tt3", "movie:tt1"]);
        // Case-insensitive title order, and the filter holds while sorting.
        assert_eq!(keys(saved_view(&items, LibKind::All, LibSort::Title)), ["movie:tt3", "series:tt2", "movie:tt1"]);
        assert_eq!(keys(saved_view(&items, LibKind::Series, LibSort::Title)), ["series:tt2"]);
        assert!(saved_view(&items, LibKind::Series, LibSort::Added).len() == 1);
        assert_eq!(saved_view(&[], LibKind::All, LibSort::Title).len(), 0);
    }

    #[test]
    fn episodes_sort_with_specials_last() {
        let v = |season, episode| Video { id: format!("tt1:{season:?}:{episode}"), title: None, season, episode: Some(episode), released: None };
        let mut eps = vec![v(Some(0), 1), v(None, 1), v(Some(2), 1), v(Some(1), 2), v(Some(1), 1)];
        sort_episodes(&mut eps);
        let order: Vec<_> = eps.iter().map(|e| (e.season, e.episode)).collect();
        assert_eq!(order, [(Some(1), Some(1)), (Some(1), Some(2)), (Some(2), Some(1)), (Some(0), Some(1)), (None, Some(1))]);
        assert_eq!((season_label(Some(0)), season_label(Some(3))), ("Specials".to_string(), "Season 3".to_string()));
    }

    #[test]
    fn next_episode_in_watch_order() {
        let v = |season, episode| Video { id: format!("{season:?}:{episode}"), title: None, season, episode: Some(episode), released: None };
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
    fn rewatch_past_90_percent_leaves_continue_watching() {
        let (_dir, mut app) = test_app();
        app.set_watched("movie:tt1", true);
        let p = |position| Progress { key: "movie:tt1".into(), title: "T".into(), source: "https://x.example/v".into(), position, duration: 100.0, poster: None };
        app.record_progress(p(50.0));
        assert_eq!(app.recent.len(), 1, "a rewatch keeps a resume point");
        app.record_progress(p(95.0));
        assert!(app.recent.is_empty() && app.db.progress("movie:tt1").unwrap().is_none());
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
    fn torrent_file_detection() {
        assert!(is_torrent_file("/m/a.torrent") && is_torrent_file(r"C:\m\A.TORRENT"));
        for not in ["https://x.example/a.torrent", "magnet:?xt=urn:btih:a.torrent", "/m/a.mkv", "/m/torrent"] {
            assert!(!is_torrent_file(not), "{not}");
        }
    }

    #[test]
    fn command_words_respect_quotes() {
        assert_eq!(command_words(r#""C:\Program Files\VLC\vlc.exe" --fs"#), [r"C:\Program Files\VLC\vlc.exe", "--fs"]);
        assert_eq!(command_words("  mpv   --fs "), ["mpv", "--fs"]);
        assert_eq!(command_words(r#"a "" b"#), ["a", "", "b"]);
        assert!(command_words("   ").is_empty());
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
