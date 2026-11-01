//! Stremio Lite desktop GUI (egui/eframe). Network and torrent work runs on a tokio
//! runtime; results come back to the UI thread over a channel.

use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use stremio_lite::addon::{AddonClient, Manifest, MetaPreview, PlayTarget, Stream, parse_magnet_hash};
use stremio_lite::db::{Db, Progress};
use stremio_lite::library::{LibraryItem, scan};
use stremio_lite::player::Player;
use stremio_lite::torrent::TorrentEngine;

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Discover,
    Addons,
    Library,
}

enum Msg {
    Status(String),
    Installed(String, Manifest),
    Catalog(String, Vec<(String, MetaPreview)>),
    Streams(MetaPreview, Vec<Stream>, Vec<String>),
    /// poster URL, image bytes (empty on failure)
    Poster(String, Vec<u8>),
    /// key, title, resumable source, playable URL/path, subtitle URLs
    Play(String, String, String, String, Vec<String>),
}

struct Playing {
    player: Player,
    key: String,
    title: String,
    source: String,
    pos: f64,
    dur: f64,
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
    catalog: Option<(String, Vec<(String, MetaPreview)>)>,
    selected: Option<(MetaPreview, Vec<Stream>, Vec<String>)>,
    magnet: String,
    search: String,
    /// Poster bytes by URL; `None` while loading, empty if the fetch failed.
    posters: HashMap<String, Option<Arc<[u8]>>>,
    folder: String,
    library: Vec<LibraryItem>,
    recent: Vec<Progress>,
    player: Option<Playing>,
    status: String,
    last_poll: Instant,
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
            folder: db.setting("last_folder")?.unwrap_or_default(),
            db,
            data_dir,
            client: AddonClient::new(),
            torrent: Default::default(),
            tx,
            rx,
            tab: Tab::Discover,
            addon_url: String::new(),
            catalog: None,
            selected: None,
            magnet: String::new(),
            search: String::new(),
            posters: HashMap::new(),
            player: None,
            status: "Ready".into(),
            last_poll: Instant::now(),
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
            }
            Msg::Catalog(name, items) => {
                self.status = format!("{name}: {} items", items.len());
                self.catalog = Some((name, items));
                self.selected = None;
            }
            Msg::Streams(meta, streams, subs) => {
                self.status = format!("{}: {} streams", meta.name, streams.len());
                self.selected = Some((meta, streams, subs));
            }
            Msg::Poster(url, bytes) => {
                self.posters.insert(url, Some(bytes.into()));
            }
            Msg::Play(key, title, source, url, subs) => {
                self.stop_player();
                let start = match self.db.progress(&key) {
                    Ok(Some(p)) if p.position > 3.0 && (p.duration <= 0.0 || p.position < p.duration - 5.0) => p.position,
                    _ => 0.0,
                };
                match Player::launch(&url, &title, start, &subs) {
                    Ok(player) => {
                        self.status = if start > 0.0 { format!("Resuming at {}", fmt_time(start)) } else { "Playing".into() };
                        self.player = Some(Playing { player, key, title, source, pos: start, dur: 0.0 });
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
                let prog = Progress { key: p.key.clone(), title: p.title.clone(), source: p.source.clone(), position: p.pos, duration: p.dur };
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
        if self.last_poll.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_poll = Instant::now();
        let Some(p) = &mut self.player else { return };
        if !p.player.is_running() {
            self.status = "Player closed".into();
            self.stop_player();
            return;
        }
        if let Some((pos, dur)) = p.player.position() {
            (p.pos, p.dur) = (pos, dur);
            self.save_progress();
        }
    }

    fn open_catalog(&mut self, ctx: &egui::Context, url: String, name: String, kind: String, id: String) {
        let client = self.client.clone();
        self.status = format!("Loading {name}...");
        self.spawn(ctx, async move {
            let items = client.catalog(&url, &kind, &id).await?;
            Ok(Msg::Catalog(name, items.into_iter().map(|m| (url.clone(), m)).collect()))
        });
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
            .flat_map(|(u, m)| m.catalogs.iter().filter(|c| c.supports("search")).map(move |c| (u.clone(), c.kind.clone(), c.id.clone())))
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
                    Ok(ms) => items.extend(ms.into_iter().map(|m| (u.clone(), m))),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            if errors.len() == targets.len() {
                anyhow::bail!("search failed: {}", errors.join("; "));
            }
            Ok(Msg::Catalog(format!("Search \"{query}\""), items))
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

    fn open_meta(&mut self, ctx: &egui::Context, meta: MetaPreview) {
        let client = self.client.clone();
        let matches = |m: &Manifest, res: &str| {
            m.has_resource(res)
                && m.types.contains(&meta.kind)
                && m.id_prefixes.as_ref().is_none_or(|ps| ps.iter().any(|p| meta.id.starts_with(p.as_str())))
        };
        let stream_addons: Vec<String> = self.addons.iter().filter(|(_, m)| matches(m, "stream")).map(|(u, _)| u.clone()).collect();
        let sub_addons: Vec<String> = self.addons.iter().filter(|(_, m)| matches(m, "subtitles")).map(|(u, _)| u.clone()).collect();
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
            Ok(Msg::Streams(meta, streams, subs))
        });
    }

    fn ui_discover(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let l = ui.label("Magnet link:");
            ui.add(egui::TextEdit::singleline(&mut self.magnet).hint_text("magnet:?xt=urn:btih:...").desired_width(500.0))
                .labelled_by(l.id);
            if ui.button("Play magnet").clicked() {
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
            let l = ui.label("Search:");
            let r = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Title...").desired_width(400.0)).labelled_by(l.id);
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Search").clicked() || enter {
                self.search(&ctx);
            }
        });
        ui.separator();
        ui.heading("Catalogs");
        let mut open = None;
        ui.horizontal_wrapped(|ui| {
            for (url, m) in &self.addons {
                if !m.has_resource("catalog") {
                    continue;
                }
                for c in m.catalogs.iter().filter(|c| !c.requires_extra()) {
                    let name = format!("{} - {}", m.name, c.name.as_deref().unwrap_or(&c.id));
                    if ui.button(&name).clicked() {
                        open = Some((url.clone(), name, c.kind.clone(), c.id.clone()));
                    }
                }
            }
        });
        if let Some((u, n, k, i)) = open {
            self.open_catalog(&ctx, u, n, k, i);
        }
        if let Some((name, items)) = &self.catalog {
            ui.label(format!("Catalog: {name}"));
            let (mut pick, mut want) = (None, Vec::new());
            let size = egui::vec2(120.0, 180.0);
            ui.horizontal_wrapped(|ui| {
                for (_, m) in items {
                    let poster = m.poster.as_ref().map(|u| (u, self.posters.get(u)));
                    let r = ui
                        .vertical(|ui| {
                            ui.set_width(size.x);
                            let r = match poster {
                                Some((u, Some(Some(b)))) if !b.is_empty() => {
                                    let img = egui::Image::from_bytes(format!("bytes://{u}"), b.clone()).alt_text(&m.name).fit_to_exact_size(size);
                                    ui.add(egui::Button::image(img))
                                }
                                _ => ui.add_sized(size, egui::Button::new(&m.name).wrap()),
                            };
                            ui.add(egui::Label::new(&m.name).truncate());
                            r
                        })
                        .inner;
                    if let Some((u, None)) = poster
                        && ui.is_rect_visible(r.rect)
                    {
                        want.push(u.clone());
                    }
                    if r.on_hover_text(m.description.as_deref().unwrap_or(&m.name)).clicked() {
                        pick = Some(m.clone());
                    }
                }
            });
            for u in want {
                self.fetch_poster(&ctx, u);
            }
            if let Some(m) = pick {
                self.open_meta(&ctx, m);
            }
        }
        let mut play = None;
        if let Some((meta, streams, subs)) = &self.selected {
            ui.separator();
            ui.heading(&meta.name);
            if let Some(d) = &meta.description {
                ui.label(d);
            }
            ui.label(format!("{} subtitle track(s)", subs.len()));
            for s in streams {
                if ui.button(format!("Play: {}", s.label())).clicked() {
                    let source = match s.target() {
                        Some(PlayTarget::Http(u)) => u.to_string(),
                        Some(PlayTarget::Magnet(m, Some(idx))) => format!("{m}&so={idx}"),
                        Some(PlayTarget::Magnet(m, None)) => m,
                        None => continue,
                    };
                    play = Some((format!("{}:{}", meta.kind, meta.id), meta.name.clone(), source, subs.clone()));
                }
            }
        }
        if let Some((k, t, s, subs)) = play {
            self.play(&ctx, k, t, s, subs);
        }
    }

    fn ui_addons(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let l = ui.label("Manifest URL:");
            ui.add(egui::TextEdit::singleline(&mut self.addon_url).hint_text("http://127.0.0.1:7070/manifest.json").desired_width(400.0))
                .labelled_by(l.id);
            if ui.button("Install").clicked() {
                let (client, url) = (self.client.clone(), self.addon_url.trim().to_string());
                self.status = format!("Installing {url}...");
                self.spawn(&ctx, async move {
                    let m = client.fetch_manifest(&url).await?;
                    Ok(Msg::Installed(url, m))
                });
            }
        });
        ui.separator();
        let mut remove = None;
        for (url, m) in &self.addons {
            ui.horizontal(|ui| {
                ui.label(format!("{} {} ({})", m.name, m.version, url));
                if ui.button("Uninstall").clicked() {
                    remove = Some(url.clone());
                }
            });
        }
        if let Some(u) = remove {
            let _ = self.db.remove_addon(&u);
            self.addons.retain(|(x, _)| *x != u);
        }
    }

    fn ui_library(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let l = ui.label("Folder path:");
            ui.add(egui::TextEdit::singleline(&mut self.folder).hint_text("/path/to/videos").desired_width(400.0)).labelled_by(l.id);
            if ui.button("Add folder").clicked() {
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
                if ui.button(format!("Play {}", it.title)).clicked() {
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(&ctx, msg);
        }
        self.poll_player();
        ctx.request_repaint_after(Duration::from_millis(500));

        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Discover, "Discover");
                ui.selectable_value(&mut self.tab, Tab::Addons, "Addons");
                ui.selectable_value(&mut self.tab, Tab::Library, "Library");
            });
            ui.separator();
            // Player / status line: OCR- and screen-reader-friendly state.
            let state = match &self.player {
                Some(p) => format!("Player: playing \"{}\" at {} / {}", p.title, fmt_time(p.pos), fmt_time(p.dur)),
                None => "Player: stopped".into(),
            };
            ui.horizontal(|ui| {
                ui.label(state);
                if self.player.is_some() && ui.button("Stop").clicked() {
                    self.stop_player();
                }
            });
            ui.label(format!("Status: {}", self.status));
            ui.separator();
            ui.heading("Continue watching");
            let mut resume = None;
            for p in self.recent.iter().take(5) {
                ui.horizontal(|ui| {
                    if ui.button(format!("Resume {}", p.title)).clicked() {
                        resume = Some(p.clone());
                    }
                    ui.label(format!("{} / {}", fmt_time(p.position), fmt_time(p.duration)));
                });
            }
            if let Some(p) = resume {
                self.play(&ctx, p.key, p.title, p.source, vec![]);
            }
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Discover => self.ui_discover(ui),
                Tab::Addons => self.ui_addons(ui),
                Tab::Library => self.ui_library(ui),
            });
        });
    }

    fn on_exit(&mut self) {
        self.stop_player();
    }
}

fn main() -> eframe::Result {
    let app = App::new().expect("failed to initialise app data");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]).with_title("Stremio Lite"),
        ..Default::default()
    };
    eframe::run_native(
        "Stremio Lite",
        opts,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}
