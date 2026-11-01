//! Stremio Lite desktop GUI (egui/eframe). Network and torrent work runs on a tokio
//! runtime; results come back to the UI thread over a channel.

use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use egui::{Align, Color32, Layout, RichText, Sense, UiBuilder, pos2, vec2};
use stremio_lite::addon::{AddonClient, CatalogDef, Manifest, MetaPreview, Stream, parse_magnet_hash};
use stremio_lite::db::{Db, Progress, Watched};
use stremio_lite::library::{LibraryItem, scan};
use eframe::glow::{self, HasContext};
use stremio_lite::player::{GlProc, Player};
use stremio_lite::stremio_import::{self, Import};
use stremio_lite::torrent::TorrentEngine;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Home,
    Addons,
    Library,
}

enum Msg {
    Status(String),
    Installed(String, Manifest),
    /// Search title and results.
    Catalog(String, Vec<MetaPreview>),
    /// Row source (addon URL, type, catalog id) and its items or error.
    Row((String, String, String), Result<Vec<MetaPreview>, String>),
    /// meta, streams, subtitle URLs, play the first stream on arrival
    Streams(MetaPreview, Vec<Stream>, Vec<String>, bool),
    /// poster URL, image bytes (empty on failure)
    Poster(String, Vec<u8>),
    /// key, title, resumable source, playable URL/path, subtitle URLs
    Play(String, String, String, String, Vec<String>),
    /// progress key, poster URL found for a Continue Watching entry
    RecentPoster(String, String),
    /// Stremio storage that was read, and what it holds.
    Imported(PathBuf, Import),
}

/// One horizontal catalog row on the home screen.
struct Row {
    title: String,
    src: (String, String, String),
    /// `None` while loading; empty rows are hidden.
    items: Option<Vec<MetaPreview>>,
}

const RED: Color32 = Color32::from_rgb(229, 9, 20);
const CARD: egui::Vec2 = egui::Vec2::new(150.0, 225.0);

/// Netflix-style card: no border, grows on hover/focus, shrinks while pressed.
/// Returns the click response and the animated rect to paint into.
fn card(ui: &mut egui::Ui, size: egui::Vec2, label: &str) -> (egui::Response, egui::Rect) {
    const GROW: f32 = 1.06;
    // Reserve room for the grown card so the scroll area doesn't clip it.
    let (slot, r) = ui.allocate_exact_size(size * GROW, Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    let target = if r.is_pointer_button_down_on() { 0.96 } else if r.hovered() || r.has_focus() { GROW } else { 1.0 };
    let s = ui.ctx().animate_value_with_time(r.id, target, 0.15);
    (r, egui::Rect::from_center_size(slot.center(), size * s))
}

/// Bold weight of the UI font (egui's `strong()` only brightens the color).
fn bold() -> egui::FontFamily {
    egui::FontFamily::Name("bold".into())
}

/// Inter (OFL) as the UI font: the closest free match to Netflix Sans, which is proprietary.
/// egui's default fonts stay as fallbacks for symbols and emoji.
fn install_fonts(ctx: &egui::Context) {
    let mut f = egui::FontDefinitions::default();
    for (name, bytes) in [("Inter", &include_bytes!("../assets/fonts/Inter-Regular.ttf")[..]), ("Inter-Bold", &include_bytes!("../assets/fonts/Inter-Bold.ttf")[..])] {
        f.font_data.insert(name.into(), Arc::new(egui::FontData::from_static(bytes)));
    }
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
        ui.add(egui::Button::new(RichText::new(text).size(size).strong().family(bold())).min_size(min).corner_radius((min.y / 2.0) as u8))
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

/// Gray card with centered, wrapped text (used when there's no poster).
fn card_text(ui: &egui::Ui, rect: egui::Rect, text: &str) {
    let p = ui.painter();
    p.rect_filled(rect, 6, Color32::from_gray(40));
    let g = p.layout(text.to_owned(), egui::FontId::proportional(15.0), ui.visuals().text_color(), rect.width() - 16.0);
    p.galley(rect.center() - g.size() / 2.0, g, Color32::WHITE);
}

/// Netflix episode-list row: index, bold name, gray details, play circle; highlights on hover.
fn stream_row(ui: &mut egui::Ui, n: usize, s: &Stream) -> egui::Response {
    let mut parts = [&s.name, &s.title, &s.description].into_iter().flatten().map(|x| x.trim()).filter(|x| !x.is_empty());
    let head = parts.next().unwrap_or("Stream").replace('\n', " ");
    let details = parts.collect::<Vec<_>>().join("\n");
    let (w, text_x, pad) = (ui.available_width().min(960.0), 84.0, 18.0);
    let p = ui.painter();
    let hg = p.layout(head.clone(), egui::FontId::new(17.0, bold()), Color32::WHITE, w - text_x - 96.0);
    let dg = p.layout(details, egui::FontId::proportional(14.0), Color32::from_gray(170), w - text_x - 96.0);
    let block = hg.size().y + if dg.size().y > 0.0 { 6.0 + dg.size().y } else { 0.0 };
    let h = (pad * 2.0 + block).max(88.0);
    let (rect, r) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Play {head}")));
    let t = ui.ctx().animate_bool(r.id, r.hovered() || r.has_focus());
    let p = ui.painter();
    p.rect_filled(rect, 4, Color32::from_gray(51).gamma_multiply(t));
    p.hline(rect.x_range(), rect.bottom(), (1.0, Color32::from_gray(64)));
    let num = p.layout_no_wrap(n.to_string(), egui::FontId::proportional(24.0), Color32::from_gray(210));
    p.galley(pos2(rect.left() + text_x / 2.0 - num.size().x / 2.0, rect.center().y - num.size().y / 2.0), num, Color32::WHITE);
    let top = rect.center().y - block / 2.0;
    p.galley(pos2(rect.left() + text_x, top), hg.clone(), Color32::WHITE);
    p.galley(pos2(rect.left() + text_x, top + hg.size().y + 6.0), dg, Color32::WHITE);
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

/// The playable source when there is exactly one; `None` when there's a choice (or nothing).
fn only_source(streams: &[Stream]) -> Option<String> {
    let mut it = streams.iter().filter_map(Stream::source);
    let first = it.next();
    if it.next().is_some() { None } else { first }
}

struct Playing {
    player: Player,
    key: String,
    title: String,
    source: String,
    pos: f64,
    dur: f64,
    poster: Option<String>,
    /// Controls stay visible until this time (seconds, egui clock).
    osd_until: f64,
    /// Audio & Subtitles panel open.
    menu: bool,
    /// Seek-bar position while dragging, in seconds.
    scrub: Option<f64>,
}

struct App {
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
    rows: Vec<Row>,
    search_results: Option<(String, Vec<MetaPreview>)>,
    selected: Option<(MetaPreview, Vec<Stream>, Vec<String>)>,
    scroll_top: bool,
    /// Scroll the stream picker into view on the next frame.
    show_streams: bool,
    magnet: String,
    search: String,
    /// Poster bytes by URL; `None` while loading, empty if the fetch failed.
    posters: HashMap<String, Option<Arc<[u8]>>>,
    folder: String,
    library: Vec<LibraryItem>,
    recent: Vec<Progress>,
    watched: Vec<Watched>,
    /// Stremio data path to import from; empty = auto-detect.
    import_path: String,
    /// Nav pill animation: tab it's heading to, start rect, target rect, start time.
    pill: Option<(Tab, egui::Rect, egui::Rect, f64)>,
    player: Option<Playing>,
    status: String,
    last_poll: Instant,
    /// GL function loader of eframe's context, for libmpv's OpenGL renderer.
    gl_proc: Option<GlProc>,
    /// Video framebuffer, its texture (owned by egui once registered) and pixel size; reused across playbacks.
    video: Option<(glow::Framebuffer, glow::Texture, egui::TextureId, [usize; 2])>,
}

fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("STREMIO_LITE_DATA") {
        return d.into();
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("stremio-lite")
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

impl App {
    fn new() -> anyhow::Result<Self> {
        let data_dir = data_dir();
        let db = Db::open(&data_dir.join("stremio-lite.db"))?;
        let (tx, rx) = channel();
        let mut app = Self {
            rt: tokio::runtime::Runtime::new()?,
            addons: db.addons()?,
            library: db.library_items()?,
            recent: db.recent_progress(20)?,
            watched: db.watched(50)?,
            import_path: String::new(),
            folder: db.setting("last_folder")?.unwrap_or_default(),
            db,
            data_dir,
            client: AddonClient::new(),
            torrent: Default::default(),
            tx,
            rx,
            tab: Tab::Home,
            addon_url: String::new(),
            rows: Vec::new(),
            search_results: None,
            selected: None,
            scroll_top: false,
            show_streams: false,
            magnet: String::new(),
            search: String::new(),
            posters: HashMap::new(),
            player: None,
            pill: None,
            status: "Ready".into(),
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
            let msg = f.await.unwrap_or_else(|e| Msg::Status(format!("Error: {e:#}")));
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Resolve `source` (http URL, magnet, or local path) and start playback.
    fn play(&mut self, ctx: &egui::Context, key: String, title: String, source: String, subs: Vec<String>) {
        self.status = format!("Opening {title}...");
        if !source.starts_with("magnet:") {
            let _ = self.tx.send(Msg::Play(key, title, source.clone(), source, subs));
            return;
        }
        let (cell, dir) = (self.torrent.clone(), self.data_dir.join("torrents"));
        let dht = std::env::var_os("STREMIO_LITE_NO_DHT").is_none();
        self.spawn(ctx, async move {
            let engine = cell.get_or_try_init(|| TorrentEngine::start(dir, dht)).await?;
            let url = engine.stream_url(&source, None).await?;
            Ok(Msg::Play(key, title, source, url, subs))
        });
    }

    fn handle(&mut self, ctx: &egui::Context, msg: Msg) {
        match msg {
            Msg::Status(s) => self.status = s,
            Msg::Installed(url, m) => {
                self.status = format!("Installed {} {}", m.name, m.version);
                if let Err(e) = self.db.add_addon(&url, &m) {
                    self.status = format!("Error saving addon: {e:#}");
                }
                self.addons.retain(|(u, _)| *u != url);
                self.addons.push((url, m));
                self.addon_url.clear();
                self.build_rows(ctx);
                self.fetch_recent_posters(ctx);
            }
            Msg::Catalog(name, items) => {
                self.status = format!("{name}: {} items", items.len());
                self.search_results = Some((name, items));
            }
            Msg::Row(src, res) => {
                let Some(row) = self.rows.iter_mut().find(|r| r.src == src) else { return };
                row.items = Some(res.unwrap_or_else(|e| {
                    self.status = format!("Error loading {}: {e}", row.title);
                    Vec::new()
                }));
            }
            Msg::Streams(meta, streams, subs, autoplay) => {
                self.status = format!("{}: {} streams", meta.name, streams.len());
                // Autoplay only when there's no choice to make; otherwise show the stream picker.
                if autoplay {
                    match only_source(&streams) {
                        Some(src) => self.play(ctx, format!("{}:{}", meta.kind, meta.id), meta.name.clone(), src, subs.clone()),
                        None => self.show_streams = true,
                    }
                }
                self.selected = Some((meta, streams, subs));
                self.scroll_top = true;
            }
            Msg::RecentPoster(key, url) => {
                if let Err(e) = self.db.set_progress_poster(&key, &url) {
                    self.status = format!("Error saving poster: {e:#}");
                }
                for p in self.recent.iter_mut().filter(|p| p.key == key) {
                    p.poster = Some(url.clone());
                }
            }
            Msg::Imported(path, imp) => {
                if let Err(e) = imp.save(&self.db) {
                    self.status = format!("Error saving import: {e:#}");
                    return;
                }
                self.status = format!(
                    "Imported {} addons, {} in progress, {} watched from {}",
                    imp.addons.len(),
                    imp.progress.len(),
                    imp.watched.len(),
                    path.display()
                );
                self.addons = self.db.addons().unwrap_or_default();
                self.recent = self.db.recent_progress(20).unwrap_or_default();
                self.watched = self.db.watched(50).unwrap_or_default();
                self.build_rows(ctx);
                self.fetch_recent_posters(ctx);
            }
            Msg::Poster(url, bytes) => {
                self.posters.insert(url, Some(bytes.into()));
            }
            Msg::Play(key, title, source, url, subs) => {
                self.stop_player();
                let prev = self.db.progress(&key).ok().flatten();
                let start = match &prev {
                    Some(p) if p.position > 3.0 && (p.duration <= 0.0 || p.position < p.duration - 5.0) => p.position,
                    _ => 0.0,
                };
                // Poster of the opened title, else the one saved earlier for this key.
                let poster = (self.selected.as_ref())
                    .filter(|(m, ..)| format!("{}:{}", m.kind, m.id) == key)
                    .and_then(|(m, ..)| m.poster.clone())
                    .or(prev.and_then(|p| p.poster));
                let wake = ctx.clone();
                let Some(gl_proc) = self.gl_proc.clone() else {
                    self.status = "Error: no OpenGL context for the player".into();
                    return;
                };
                match Player::launch(&url, &title, start, &subs, gl_proc, Box::new(move || wake.request_repaint())) {
                    Ok(player) => {
                        self.status = if start > 0.0 { format!("Resuming at {}", fmt_time(start)) } else { "Playing".into() };
                        let p = Playing { player, key, title, source, pos: start, dur: 0.0, poster, osd_until: 0.0, menu: false, scrub: None };
                        self.player = Some(p);
                        // Force a fresh render so the previous video's last frame never flashes.
                        if let Some(v) = &mut self.video {
                            v.3 = [0, 0];
                        }
                    }
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
                ctx.request_repaint();
            }
        }
    }

    fn save_progress(&mut self) {
        if let Some(p) = &self.player {
            if p.pos > 0.0 {
                let prog = Progress { key: p.key.clone(), title: p.title.clone(), source: p.source.clone(), position: p.pos, duration: p.dur, poster: p.poster.clone() };
                if let Err(e) = self.db.save_progress(&prog) {
                    self.status = format!("Error saving progress: {e:#}");
                }
                self.recent = self.db.recent_progress(20).unwrap_or_default();
            }
        }
    }

    fn stop_player(&mut self) {
        self.save_progress();
        self.player = None;
    }

    fn poll_player(&mut self) {
        let Some(p) = &mut self.player else { return };
        if !p.player.poll() || p.player.state.error.is_some() {
            self.status = p.player.state.error.clone().unwrap_or_else(|| "Player closed".into());
            self.stop_player();
            return;
        }
        if self.last_poll.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_poll = Instant::now();
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
                Row { title, src: (u.clone(), c.kind.clone(), c.id.clone()), items: None }
            })
            .collect();
        for src in self.rows.iter().map(|r| r.src.clone()).collect::<Vec<_>>() {
            let client = self.client.clone();
            self.spawn(ctx, async move {
                let res = client.catalog(&src.0, &src.1, &src.2).await.map_err(|e| format!("{e:#}"));
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
            self.status = "Error: no installed addon supports search".into();
            return;
        }
        let client = self.client.clone();
        self.status = format!("Searching for \"{query}\"...");
        self.spawn(ctx, async move {
            let (mut items, mut errors) = (Vec::new(), Vec::new());
            for (u, kind, id) in &targets {
                match client.search(u, kind, id, &query).await {
                    Ok(ms) => items.extend(ms),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            if errors.len() == targets.len() {
                anyhow::bail!("search failed: {}", errors.join("; "));
            }
            Ok(Msg::Catalog(format!("Results for \"{query}\""), items))
        });
    }

    fn fetch_poster(&mut self, ctx: &egui::Context, url: String) {
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
            // Not `spawn`: a missing poster is not worth an error in the status bar.
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

    fn open_meta(&mut self, ctx: &egui::Context, meta: MetaPreview, autoplay: bool) {
        let client = self.client.clone();
        let stream_addons = self.addons_for("stream", &meta.kind, &meta.id);
        let sub_addons = self.addons_for("subtitles", &meta.kind, &meta.id);
        self.status = format!("Loading streams for {}...", meta.name);
        self.spawn(ctx, async move {
            let (mut streams, mut subs, mut errors) = (Vec::new(), Vec::new(), Vec::new());
            for u in &stream_addons {
                match client.streams(u, &meta.kind, &meta.id).await {
                    Ok(s) => streams.extend(s),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            for u in &sub_addons {
                // Subtitles are optional; a failing subtitle addon must not block playback.
                if let Ok(s) = client.subtitles(u, &meta.kind, &meta.id).await {
                    subs.extend(s.into_iter().map(|s| s.url));
                }
            }
            if streams.is_empty() && !errors.is_empty() {
                anyhow::bail!("no streams: {}", errors.join("; "));
            }
            Ok(Msg::Streams(meta, streams, subs, autoplay))
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
        let (mut want, mut open, mut play) = (Vec::new(), None, None);
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
        if let Some((meta, streams, subs)) = &self.selected {
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let r = ui.label(RichText::new("Streams").size(24.0).strong().family(bold()).color(Color32::WHITE));
                if std::mem::take(&mut self.show_streams) {
                    r.scroll_to_me(Some(Align::TOP));
                }
                ui.label(RichText::new(format!("{}  ·  {} subtitle track(s)", meta.name, subs.len())).size(15.0).color(Color32::from_gray(170)));
            });
            ui.add_space(8.0);
            let playable = streams.iter().filter_map(|s| Some((s, s.source()?)));
            for (i, (s, src)) in playable.enumerate() {
                if stream_row(ui, i + 1, s).clicked() {
                    play = Some((format!("{}:{}", meta.kind, meta.id), meta.name.clone(), src, subs.clone()));
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
                        let r = r.on_hover_text(format!("Resume {} ({} / {})", p.title, fmt_time(p.position), fmt_time(p.duration)));
                        let frac = if p.duration > 0.0 { (p.position / p.duration).clamp(0.0, 1.0) as f32 } else { 0.0 };
                        let bar = egui::Rect::from_min_max(pos2(rect.left() + 10.0, rect.bottom() - 10.0), pos2(rect.right() - 10.0, rect.bottom() - 6.0));
                        ui.painter().rect_filled(bar, 2, Color32::from_gray(90));
                        ui.painter().rect_filled(egui::Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())), 2, RED);
                        if r.clicked() {
                            play = Some((p.key.clone(), p.title.clone(), p.source.clone(), vec![]));
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
                        let hint = if w.episodes > 0 { format!("{} · {} episodes watched", w.title, w.episodes) } else { format!("{} · Watched", w.title) };
                        if r.on_hover_text(hint).clicked()
                            && let Some((kind, id)) = w.key.split_once(':')
                        {
                            let meta = MetaPreview { id: id.into(), kind: kind.into(), name: w.title.clone(), poster: w.poster.clone(), background: None, description: None };
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
            self.ui_cards(ui, "search", items, &mut want, &mut open);
        }
        if clear {
            self.search_results = None;
        }
        for row in &self.rows {
            match &row.items {
                Some(items) if items.is_empty() => continue,
                Some(items) => {
                    Self::row_title(ui, &row.title);
                    self.ui_cards(ui, &row.title, items, &mut want, &mut open);
                }
                None => {
                    Self::row_title(ui, &row.title);
                    ui.spinner();
                }
            }
        }
        want.sort();
        want.dedup();
        for u in want {
            self.fetch_poster(&ctx, u);
        }
        if let Some((m, autoplay)) = open {
            self.open_meta(&ctx, m, autoplay);
        }
        if let Some((k, t, s, subs)) = play {
            self.play(&ctx, k, t, s, subs);
        }
    }

    fn row_title(ui: &mut egui::Ui, title: &str) {
        ui.add_space(22.0);
        ui.label(RichText::new(title).size(20.0).strong().family(bold()).color(Color32::WHITE));
        ui.add_space(4.0);
    }

    /// Horizontally scrolling poster cards; a click selects the title.
    fn ui_cards(&self, ui: &mut egui::Ui, salt: &str, items: &[MetaPreview], want: &mut Vec<String>, open: &mut Option<(MetaPreview, bool)>) {
        egui::ScrollArea::horizontal().id_salt(salt).show(ui, |ui| {
            ui.horizontal(|ui| {
                for m in items {
                    let (r, rect) = card(ui, CARD, &m.name);
                    self.paint_poster(ui, &r, rect, &m.poster, &m.name, want);
                    if r.on_hover_text(m.description.as_deref().unwrap_or(&m.name)).clicked() {
                        *open = Some((m.clone(), false));
                    }
                }
            });
        });
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
        play: &mut Option<(String, String, String, Vec<String>)>,
    ) {
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, (w * 0.42).clamp(320.0, 560.0)), Sense::hover());
        if std::mem::take(&mut self.scroll_top) {
            ui.scroll_to_rect(rect, Some(Align::TOP));
        }
        ui.painter().rect_filled(rect, 12, Color32::from_gray(28));
        // Background first; fall back to the poster when it is missing or rejected.
        let bg = self.image(&meta.background, want);
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
                if hero_button(ui, "▶  Play", [Color32::WHITE, Color32::from_white_alpha(191)], Color32::BLACK).clicked() {
                    match &self.selected {
                        Some((m, streams, subs)) if m == meta => match only_source(streams) {
                            Some(src) => *play = Some((format!("{}:{}", m.kind, m.id), m.name.clone(), src, subs.clone())),
                            None => show_streams = true,
                        },
                        _ => *open = Some((meta.clone(), true)),
                    }
                }
                self.show_streams |= show_streams;
                let gray = |a| Color32::from_rgba_unmultiplied(109, 109, 110, a);
                if hero_button(ui, "More info", [gray(179), gray(102)], Color32::WHITE).clicked() {
                    *open = Some((meta.clone(), false));
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
            ui.add(egui::Label::new(RichText::new(&meta.name).size(46.0).strong().family(bold()).color(Color32::WHITE)).wrap());
        });
    }

    /// Full-window Netflix-style player: letterboxed video, controls that fade out after
    /// 3 s without input, red seek bar, ±10 s skips, volume, Audio & Subtitles panel.
    fn ui_player(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let Some(p) = &mut self.player else { return };
        let now = ctx.input(|i| i.time);
        let fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let mut cmds: Vec<Vec<String>> = Vec::new();
        let mut cmd = |a: &[&str]| cmds.push(a.iter().map(|s| s.to_string()).collect());
        let (mut back, mut toggle_fs) = (false, false);
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
                            self.status = format!("Error: GL setup failed: {:?}", a.err().or(b.err()));
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
            if k(ArrowLeft) { cmd(&["seek", "-10"]) }
            if k(ArrowRight) { cmd(&["seek", "10"]) }
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
            // Icon button: glyph that grows on hover, like Netflix's control bar.
            let icon = |ui: &mut egui::Ui, c: egui::Pos2, glyph: &str, label: &str| {
                let r = ui.interact(egui::Rect::from_center_size(c, vec2(48.0, 48.0)), ui.scope_id().with(label), Sense::CLICK);
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
                let s = ui.ctx().animate_value_with_time(r.id, if r.hovered() { 1.2 } else { 1.0 }, 0.12);
                paint.text(c, egui::Align2::CENTER_CENTER, glyph, egui::FontId::proportional(30.0 * s), white);
                r.on_hover_cursor(egui::CursorIcon::PointingHand)
            };
            // Scrims behind the top and bottom controls.
            paint.rect_filled(egui::Rect::from_min_size(rect.min, vec2(rect.width(), 80.0)), 0, Color32::from_black_alpha((110.0 * a) as u8));
            paint.rect_filled(egui::Rect::from_min_max(pos2(rect.left(), rect.bottom() - 130.0), rect.max), 0, Color32::from_black_alpha((150.0 * a) as u8));
            back |= icon(ui, rect.min + vec2(44.0, 40.0), "←", "Back to browse").clicked();

            // Seek bar: gray track, red progress and knob; drag or click to seek.
            let bar = egui::Rect::from_min_max(pos2(rect.left() + 28.0, rect.bottom() - 104.0), pos2(rect.right() - 130.0, rect.bottom() - 80.0));
            let r = ui.interact(bar, ui.scope_id().with("seek"), Sense::CLICK | Sense::DRAG);
            r.widget_info(|| egui::WidgetInfo::slider(true, st.pos, "Seek"));
            let at = |x: f32| (((x - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64) * st.dur;
            if (r.dragged() || r.drag_started()) && st.dur > 0.0 {
                p.scrub = r.interact_pointer_pos().map(|q| at(q.x));
            }
            if let Some(t) = (r.clicked() || r.drag_stopped()).then(|| r.interact_pointer_pos().map(|q| at(q.x)).or(p.scrub)).flatten() {
                cmd(&["seek", &format!("{t:.1}"), "absolute"]);
            }
            if !r.dragged() {
                p.scrub = None;
            }
            let pos = p.scrub.unwrap_or(st.pos);
            let frac = if st.dur > 0.0 { (pos / st.dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
            let hot = r.hovered() || r.dragged();
            let th = ctx.animate_value_with_time(r.id.with("h"), if hot { 7.0 } else { 4.0 }, 0.1);
            let track = egui::Rect::from_center_size(bar.center(), vec2(bar.width(), th));
            paint.rect_filled(track, 2, Color32::from_gray(128).gamma_multiply(a * 0.7));
            paint.rect_filled(egui::Rect::from_min_size(track.min, vec2(track.width() * frac, th)), 2, RED.gamma_multiply(a));
            paint.circle_filled(pos2(track.left() + track.width() * frac, track.center().y), if hot { 10.0 } else { 7.0 }, RED.gamma_multiply(a));
            if let Some(h) = r.hover_pos().filter(|_| st.dur > 0.0) {
                paint.text(pos2(h.x, bar.top() - 8.0), egui::Align2::CENTER_BOTTOM, fmt_time(at(h.x)), egui::FontId::proportional(14.0), white);
            }
            paint.text(pos2(rect.right() - 28.0, bar.center().y), egui::Align2::RIGHT_CENTER, fmt_time(st.dur - pos), egui::FontId::proportional(16.0), white);

            // Control row.
            let y = rect.bottom() - 44.0;
            let x = |i: f32| rect.left() + 52.0 + i * 68.0;
            if icon(ui, pos2(x(0.0), y), if st.paused { "▶" } else { "⏸" }, if st.paused { "Play" } else { "Pause" }).clicked() {
                cmd(&["cycle", "pause"]);
            }
            if icon(ui, pos2(x(1.0), y), "↺", "Back 10 seconds").clicked() {
                cmd(&["seek", "-10"]);
            }
            if icon(ui, pos2(x(2.0), y), "↻", "Forward 10 seconds").clicked() {
                cmd(&["seek", "10"]);
            }
            let vol = icon(ui, pos2(x(3.0), y), if st.muted || st.volume <= 0.0 { "🔇" } else { "🔊" }, "Mute");
            if vol.clicked() {
                cmd(&["cycle", "mute"]);
            }
            // Volume slider next to the speaker.
            let vbar = egui::Rect::from_min_size(pos2(x(3.0) + 30.0, y - 12.0), vec2(110.0, 24.0));
            let vr = ui.interact(vbar, ui.scope_id().with("volume"), Sense::CLICK | Sense::DRAG);
            vr.widget_info(|| egui::WidgetInfo::slider(true, st.volume, "Volume"));
            if let Some(q) = vr.interact_pointer_pos().filter(|_| vr.dragged() || vr.clicked()) {
                let v = ((q.x - vbar.left()) / vbar.width()).clamp(0.0, 1.0) * 100.0;
                cmd(&["set", "volume", &format!("{v:.0}")]);
            }
            let vt = egui::Rect::from_center_size(vbar.center(), vec2(vbar.width(), 4.0));
            let vf = if st.muted { 0.0 } else { (st.volume / 100.0).clamp(0.0, 1.0) as f32 };
            paint.rect_filled(vt, 2, Color32::from_gray(128).gamma_multiply(a * 0.7));
            paint.rect_filled(egui::Rect::from_min_size(vt.min, vec2(vt.width() * vf, 4.0)), 2, white);
            paint.circle_filled(pos2(vt.left() + vt.width() * vf, vt.center().y), 6.0, white);
            paint.text(pos2(rect.center().x, y), egui::Align2::CENTER_CENTER, &p.title, egui::FontId::new(18.0, bold()), white);
            let cc = icon(ui, pos2(rect.right() - 120.0, y), "💬", "Audio & Subtitles");
            if cc.clicked() {
                p.menu = !p.menu;
            }
            toggle_fs |= icon(ui, pos2(rect.right() - 52.0, y), "⛶", if fs { "Exit full screen" } else { "Full screen" }).clicked();

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
                                    let entries = (kind == "sub").then_some((None, "Off", off)).into_iter().chain(tracks.iter().map(|t| (Some(t.id), t.label.as_str(), t.selected)));
                                    for (id, label, sel) in entries {
                                        let text = RichText::new(format!("{}  {label}", if sel { "✓" } else { "   " })).size(16.0);
                                        let text = if sel { text.color(Color32::WHITE).family(bold()) } else { text.color(Color32::from_gray(180)) };
                                        if ui.add(egui::Button::new(text).frame(false)).clicked() {
                                            let v = id.map_or("no".to_string(), |i| i.to_string());
                                            cmd(&["set", prop, &v]);
                                        }
                                    }
                                });
                            }
                        });
                    });
                });
            }
        }
        for c in &cmds {
            let args: Vec<&str> = c.iter().map(String::as_str).collect();
            if let Err(e) = p.player.command(&args) {
                self.status = format!("Error: {e:#}");
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
            self.status = "Stopped".into();
        }
    }

    fn ui_addons(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let l = ui.label("Manifest URL:");
            ui.add(egui::TextEdit::singleline(&mut self.addon_url).hint_text("http://127.0.0.1:7070/manifest.json").desired_width(400.0))
                .labelled_by(l.id);
            if primary_button(ui, "Install").clicked() {
                let (client, url) = (self.client.clone(), self.addon_url.trim().to_string());
                self.status = format!("Installing {url}...");
                self.spawn(&ctx, async move {
                    let m = client.fetch_manifest(&url).await?;
                    Ok(Msg::Installed(url, m))
                });
            }
        });
        ui.horizontal(|ui| {
            let l = ui.label("Stremio data:");
            ui.add(egui::TextEdit::singleline(&mut self.import_path).hint_text("Auto-detect local Stremio").desired_width(400.0))
                .labelled_by(l.id);
            if primary_button(ui, "Import from Stremio").on_hover_text("Import addons, watched titles and progress").clicked() {
                let path = Some(PathBuf::from(self.import_path.trim())).filter(|p| !p.as_os_str().is_empty());
                self.status = "Importing from Stremio...".into();
                self.spawn(&ctx, async move {
                    let (path, imp) = tokio::task::spawn_blocking(move || stremio_import::import(path.as_deref())).await??;
                    Ok(Msg::Imported(path, imp))
                });
            }
        });
        ui.separator();
        let mut remove = None;
        for (url, m) in &self.addons {
            ui.horizontal(|ui| {
                ui.label(format!("{} {} ({})", m.name, m.version, url));
                if small_button(ui, "Uninstall").clicked() {
                    remove = Some(url.clone());
                }
            });
        }
        if let Some(u) = remove {
            let _ = self.db.remove_addon(&u);
            self.addons.retain(|(x, _)| *x != u);
            self.build_rows(&ctx);
        }
    }

    fn ui_library(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let l = ui.label("Magnet link:");
            ui.add(egui::TextEdit::singleline(&mut self.magnet).hint_text("magnet:?xt=urn:btih:...").desired_width(500.0))
                .labelled_by(l.id);
            if primary_button(ui, "Play magnet").clicked() {
                match parse_magnet_hash(self.magnet.trim()) {
                    Some(h) => {
                        let m = self.magnet.trim().to_string();
                        self.play(&ctx, format!("btih:{h}"), format!("Torrent {}", &h[..8]), m, vec![]);
                    }
                    None => self.status = "Error: invalid magnet link".into(),
                }
            }
        });
        ui.horizontal(|ui| {
            let l = ui.label("Folder path:");
            ui.add(egui::TextEdit::singleline(&mut self.folder).hint_text("/path/to/videos").desired_width(400.0)).labelled_by(l.id);
            if primary_button(ui, "Add folder").clicked() {
                let dir = PathBuf::from(self.folder.trim());
                match scan(&dir).map_err(anyhow::Error::from).and_then(|items| {
                    self.db.add_library_folder(&dir, &items)?;
                    self.db.set_setting("last_folder", &dir.to_string_lossy())?;
                    Ok(items.len())
                }) {
                    Ok(n) => {
                        self.status = format!("Added {n} files from {}", dir.display());
                        self.library = self.db.library_items().unwrap_or_default();
                    }
                    Err(e) => self.status = format!("Error: {e:#}"),
                }
            }
        });
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
        self.poll_player();
        ctx.request_repaint_after(Duration::from_millis(500));
        if self.player.is_some() {
            self.ui_player(ui, frame);
            return;
        }

        egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(40, 14)).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            // Fixed-height row so the nav and search pills share one center line.
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 44.0), Layout::left_to_right(Align::Center), |ui| {
                ui.label(RichText::new("STREMIO LITE").size(26.0).strong().family(bold()).color(RED));
                ui.add_space(20.0);
                // Netflix nav: current tab is bold white on a dark gray pill that slides between tabs.
                let pill_slot = ui.painter().add(egui::Shape::Noop);
                let mut sel_rect = None;
                for (tab, name) in [(Tab::Home, "Home"), (Tab::Addons, "Addons"), (Tab::Library, "Library")] {
                    let text = RichText::new(name).size(15.0);
                    let text = if self.tab == tab { text.strong().family(bold()).color(Color32::WHITE) } else { text.color(Color32::from_gray(180)) };
                    let r = ui.add(egui::Button::new(text).fill(Color32::TRANSPARENT).min_size(vec2(0.0, 34.0))).on_hover_cursor(egui::CursorIcon::PointingHand);
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
                    let r = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Titles, people, genres").frame(egui::Frame::NONE.inner_margin(egui::Margin { top: 2, ..Default::default() })).desired_width(220.0)); // top margin: optically center the text
                    // Painted magnifier: the emoji glyph sits off-center in its line box.
                    let (icon, b) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                    b.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Search"));
                    let (c, stroke) = (icon.center() - vec2(1.5, 1.5), egui::Stroke::new(1.8, Color32::from_gray(180)));
                    ui.painter().circle_stroke(c, 5.5, stroke);
                    ui.painter().line_segment([c + vec2(4.0, 4.0), icon.center() + vec2(7.0, 7.0)], stroke);
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
            // Player / status line: OCR- and screen-reader-friendly state.
            let state = match &self.player {
                Some(p) => format!("Player: playing \"{}\" at {} / {}", p.title, fmt_time(p.pos), fmt_time(p.dur)),
                None => "Player: stopped".into(),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(state).color(Color32::from_gray(160)));
                if self.player.is_some() && small_button(ui, "Stop").clicked() {
                    self.stop_player();
                }
                ui.label(RichText::new(format!("Status: {}", self.status)).color(Color32::from_gray(160)));
            });
            ui.add_space(8.0);
            // Back lives outside the scroll area, so it stays put while the detail page scrolls.
            if self.selected.is_some() {
                if pill_button(ui, "←  Back", 16.0, vec2(0.0, 40.0), white_fill(), Color32::BLACK).on_hover_text("Back to browse").clicked() {
                    self.selected = None;
                    self.scroll_top = true;
                }
                ui.add_space(6.0);
            }
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Home => self.ui_home(ui),
                Tab::Addons => self.ui_addons(ui),
                Tab::Library => self.ui_library(ui),
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&glow::Context>) {
        self.stop_player();
    }
}

fn main() -> eframe::Result {
    let mut app = App::new().expect("failed to initialise app data");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]).with_title("Stremio Lite"),
        ..Default::default()
    };
    eframe::run_native(
        "Stremio Lite",
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
    fn only_source_autoplays_only_without_a_choice() {
        let s = |u: &str| Stream { url: Some(u.into()), ..Default::default() };
        let bad = s("ftp://x.example/a"); // unplayable, doesn't count as a choice
        assert_eq!(only_source(&[]), None);
        assert_eq!(only_source(&[bad.clone(), s("https://x.example/a")]).as_deref(), Some("https://x.example/a"));
        assert_eq!(only_source(&[s("https://x.example/a"), s("https://x.example/b")]), None);
    }
}
