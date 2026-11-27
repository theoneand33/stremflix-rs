//! Home: catalog rows, search results, the banner and an open title's page.

use crate::*;

impl App {
    /// Nav pill position: eases (cubic out) from where it is to the selected tab.
    /// The title the Home banner shows: the open one, else the first item of the first loaded row.
    pub(crate) fn featured(&self) -> Option<MetaPreview> {
        match &self.selected {
            Some((m, ..)) => Some(m.clone()),
            None => self.rows.iter().find_map(|r| r.page.items.as_ref()?.first().cloned()),
        }
    }

    /// Rebuild home rows from installed catalogs: generic Trending/Popular rows first, then the rest.
    pub(crate) fn build_rows(&mut self, ctx: &egui::Context) {
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
            rows.push((catalog_title(addon, c), i));
        }
        let rows: Vec<Row> = rows
            .into_iter()
            .map(|(title, i)| {
                let (u, _, c) = cats[i];
                Row { title, src: (u.clone(), c.kind.clone(), c.id.clone()), page: Paged::new(c.supports("skip")), error: None }
            })
            .collect();
        let keys: Vec<String> = rows.iter().map(|r| row_key(&r.src)).collect();
        let mut rows: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
        let ordered: Vec<Row> = arrange_rows(&keys, &self.home_order).into_iter().filter_map(|i| rows[i].take()).collect();
        self.home_all = ordered.iter().map(|r| (row_key(&r.src), r.title.clone())).collect();
        self.rows = ordered.into_iter().filter(|r| !self.home_hidden.contains(&row_key(&r.src))).collect();
        for src in self.rows.iter().map(|r| r.src.clone()).collect::<Vec<_>>() {
            self.fetch_row(ctx, src);
        }
        self.fetch_new_episodes(ctx);
    }

    /// Ask for the first page of a Home row.
    pub(crate) fn fetch_row(&self, ctx: &egui::Context, src: (String, String, String)) {
        let client = self.client.clone();
        self.spawn(ctx, async move {
            let res = client.catalog(&src.0, &src.1, &src.2, 0).await.map_err(|e| format!("{e:#}"));
            Ok(Msg::Row(src, res))
        });
    }

    /// Ask a meta addon for the episodes of each saved series and keep those released recently.
    pub(crate) fn fetch_new_episodes(&self, ctx: &egui::Context) {
        // At most 4 meta requests in flight, so a long library doesn't flood the addons.
        let slots = Arc::new(tokio::sync::Semaphore::new(4));
        for meta in self.saved.iter().filter(|t| t.kind == "series").filter_map(SavedTitle::preview) {
            let addons = self.addons_for("meta", &meta.kind, &meta.id);
            let (client, tx, ctx, epoch, slots) = (self.client.clone(), self.tx.clone(), ctx.clone(), self.new_eps_gen, slots.clone());
            // Not `spawn`: a series whose meta fails to load just has no row entry.
            self.rt.spawn(async move {
                let Ok(_permit) = slots.acquire_owned().await else { return };
                if let Ok(m) = client.first_meta(&addons, &meta.kind, &meta.id, |m| !m.videos.is_empty()).await {
                    let _ = tx.send(Msg::NewEpisodes(epoch, meta, m.videos));
                    ctx.request_repaint();
                }
            });
        }
    }

    pub(crate) fn remember_search(&mut self, query: &str) {
        settings::push_history(&mut self.search_history, query);
        self.save_search_history();
    }

    pub(crate) fn save_search_history(&mut self) {
        let r = settings::SEARCH_HISTORY.set(&self.db, &self.search_history);
        if let Err(e) = r {
            report(&mut self.error, e);
        }
    }

    pub(crate) fn search(&mut self, ctx: &egui::Context) {
        let query = self.search.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.remember_search(&query);
        let rows: Vec<Row> = self
            .search_catalogs()
            .into_iter()
            .map(|(title, src)| Row { title, src, page: Paged::default(), error: None })
            .collect();
        if rows.is_empty() {
            report(&mut self.error, "no installed addon supports search");
            return;
        }
        for src in rows.iter().map(|r| r.src.clone()) {
            let (client, query) = (self.client.clone(), query.clone());
            self.spawn(ctx, async move {
                let res = client.search(&src.0, &src.1, &src.2, &query).await.map_err(|e| format!("{e:#}"));
                Ok(Msg::Search(query, src, res))
            });
        }
        self.close_details();
        self.tab = Tab::Home;
        self.search_page = Some(SearchPage { query, rows, failed: Vec::new() });
    }

    /// Searchable catalogs of installed addons: row title and source (addon URL, type, catalog id).
    pub(crate) fn search_catalogs(&self) -> Vec<(String, (String, String, String))> {
        self.addons
            .iter()
            .filter(|(_, m)| m.has_resource("catalog"))
            .flat_map(|(u, m)| m.catalogs.iter().filter(|c| c.searchable()).map(move |c| (catalog_title(&m.name, c), (u.clone(), c.kind.clone(), c.id.clone()))))
            .collect()
    }

    /// Search results: one row per searched catalog, filled in as replies arrive.
    pub(crate) fn ui_search(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (mut want, mut open, mut quick) = (Vec::new(), None, None);
        let scroll = std::mem::take(&mut self.scroll_top);
        let Some(page) = &self.search_page else { return };
        let heading = page_heading(ui, format!("Results for \"{}\"", page.query));
        if scroll {
            ui.scroll_to_rect(heading.rect, Some(Align::TOP));
        }
        for row in &page.rows {
            match &row.page.items {
                Some(items) if items.is_empty() => continue,
                Some(items) => {
                    row_title(ui, &row.title);
                    self.ui_cards(ui, &row.title, items, &mut want, &mut open, &mut quick);
                }
                None => {
                    row_title(ui, &row.title);
                    ui.spinner();
                }
            }
        }
        if page.rows.iter().all(|r| r.page.items.as_ref().is_some_and(Vec::is_empty)) && page.failed.len() < page.rows.len() {
            ui.add_space(22.0);
            muted(ui, "No results.");
        }
        want.sort();
        want.dedup();
        for u in want {
            self.fetch_poster(&ctx, u);
        }
        if let Some((m, autoplay)) = open {
            self.open_meta(&ctx, m, autoplay);
        }
        if let Some(m) = quick {
            self.quick_play(&ctx, m);
        }
    }

    pub(crate) fn ui_home(&mut self, ui: &mut egui::Ui) {
        if self.selected.is_none() && self.search_page.is_some() {
            return self.ui_search(ui);
        }
        let ctx = ui.ctx().clone();
        let (mut want, mut open, mut play, mut quick, mut resume) = (Vec::new(), None, None, None, None);
        let mut remove: Option<String> = None;
        let mut trailer = None;
        match self.featured() {
            Some(meta) => self.ui_hero(ui, &meta, &mut want, &mut open, &mut play, &mut trailer),
            None if self.rows.is_empty() => {
                ui.add_space(40.0);
                muted(ui, "Install an addon with a catalog in Addons to start browsing.");
            }
            None => {
                ui.add_space(40.0);
                ui.spinner();
            }
        }
        let mut person = None;
        if let Some(d) = self.selected_details()
            && let Some(t) = trailer
            && let Some(src) = t.source()
        {
            // Not `play`: its `nav` bump would drop the title's still-loading streams and episodes.
            let key = format!("{TRAILER}{}", t.yt_id.clone().unwrap_or_default());
            let _ = self.tx.send(Msg::Play(key, format!("{} - {}", d.name, t.title.as_deref().unwrap_or("Trailer")), src.clone(), src, vec![], None, None, self.nav));
        }
        // The open title's page reads in a centered column, like its banner.
        let col_w = ui.available_width().min(1024.0);
        let mut col = ui.new_child(UiBuilder::new().max_rect(egui::Rect::from_min_size(pos2(ui.cursor().left() + (ui.available_width() - col_w) / 2.0, ui.cursor().top()), vec2(col_w, f32::INFINITY))));
        let ui_outer = &mut *ui;
        let ui = &mut col;
        if let Some(d) = self.selected_details() {
            Self::ui_details(ui, d, &mut person);
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
            ui.add_space(36.0);
            ui.horizontal(|ui| {
                let r = page_heading(ui, "Streams");
                if std::mem::take(&mut self.show_streams) {
                    r.scroll_to_me(Some(Align::TOP));
                }
                let seen = self.watched_keys.contains(&key.0);
                if small_button(ui, if seen { "Mark unwatched" } else { "Mark watched" }).clicked() {
                    toggle = Some((key.0.clone(), !seen));
                }
                // Last and truncated: a long title would push the button out of the window.
                ui.add(egui::Label::new(RichText::new(format!("{}  ·  {} subtitle track{}", key.1, subs.len(), if subs.len() == 1 { "" } else { "s" })).size(15.0).color(ui.visuals().weak_text_color())).truncate());
            });
            ui.add_space(8.0);
            if streams.is_empty() {
                muted(ui, "No streams found.");
            }
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
            ui.add_space(36.0);
            ui.horizontal(|ui| {
                let r = page_heading(ui, "Episodes");
                if self.episode.is_none() && std::mem::take(&mut self.show_streams) {
                    r.scroll_to_me(Some(Align::TOP));
                }
                let mut seasons: Vec<Option<u32>> = self.episodes.iter().map(|v| v.season).collect();
                seasons.dedup();
                dropdown(ui, "season", season_label(self.season), |ui| {
                    for s in seasons {
                        ui.selectable_value(&mut self.season, s, season_label(s));
                    }
                });
                for (scope, season) in [("season", Some(self.season)), ("series", None)] {
                    let keys: Vec<String> = self.episodes.iter().filter(|v| season.is_none_or(|s| v.season == s)).map(|v| key::item(&meta.kind, &v.id)).collect();
                    let all = keys.iter().all(|k| self.watched_keys.contains(k));
                    if small_button(ui, &format!("Mark {scope} {}", if all { "unwatched" } else { "watched" })).clicked() {
                        bulk = Some((keys, !all));
                    }
                }
            });
            ui.add_space(8.0);
            let today = date_string(unix_days());
            for v in self.episodes.iter().filter(|v| v.season == self.season) {
                let k = key::item(&meta.kind, &v.id);
                let seen = self.watched_keys.contains(&k);
                let r = episode_row(ui, v, seen, &today);
                if self.episode.as_ref() == Some(v) {
                    // Same inset and radius as the row's hover fill.
                    ui.painter().rect_stroke(r.rect.shrink(4.0), 8, (2.0, accent(ui.ctx())), egui::StrokeKind::Inside);
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
        let used = col.min_rect();
        let ui = ui_outer;
        if self.selected.is_some() {
            ui.advance_cursor_after_rect(used);
        }
        if !self.recent.is_empty() {
            row_title(ui, "Continue Watching");
            egui::ScrollArea::horizontal().id_salt("recent").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for p in &self.recent {
                        let (r, rect) = card(ui, CARD, &p.title);
                        self.paint_poster(ui, &r, rect, &p.poster, &p.title, &mut want);
                        // Unknown duration: no total and no progress bar.
                        let at = if p.duration > 0.0 { format!("{} / {}", fmt_time(p.position), fmt_time(p.duration)) } else { fmt_time(p.position) };
                        card_caption(ui, &r, &p.title, &at);
                        let r = r.on_hover_text(format!("{} ({at})", p.title));
                        if p.duration > 0.0 {
                            let frac = (p.position / p.duration).clamp(0.0, 1.0) as f32;
                            let bar = egui::Rect::from_min_max(pos2(rect.left() + 10.0, rect.bottom() - 10.0), pos2(rect.right() - 10.0, rect.bottom() - 6.0));
                            ui.painter().rect_filled(bar, 2, Color32::from_gray(90));
                            ui.painter().rect_filled(egui::Rect::from_min_size(bar.min, vec2(bar.width() * frac, bar.height())), 2, accent(ui.ctx()));
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
            .filter(|(m, _)| self.saved.iter().any(|t| t.key == key::item("series", &m.id)))
            .filter_map(|(m, vs)| vs.iter().filter(|v| !self.watched_keys.contains(&key::item(&m.kind, &v.id))).max_by_key(|v| (v.released.as_deref().and_then(|d| d.get(..10)), v.season, v.episode)).map(|v| (m, v)))
            .collect();
        fresh.sort_by(|a, b| b.1.released.cmp(&a.1.released));
        if !fresh.is_empty() {
            row_title(ui, "New Episodes");
            egui::ScrollArea::horizontal().id_salt("new_episodes").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (m, v) in fresh {
                        let label = format!("{} S{}E{}", m.name, v.season.unwrap_or(0), v.episode.unwrap_or(0));
                        let (r, rect) = card(ui, CARD, &label);
                        self.paint_poster(ui, &r, rect, &m.poster, &m.name, &mut want);
                        card_caption(ui, &r, &m.name, &format!("S{}E{}{}", v.season.unwrap_or(0), v.episode.unwrap_or(0), v.title.as_deref().map(|t| format!(" · {t}")).unwrap_or_default()));
                        if r.on_hover_text(label).clicked() {
                            fresh_click = Some((m.clone(), v.id.clone()));
                        }
                    }
                });
            });
        }
        if !self.watched.is_empty() {
            row_title(ui, "Watch It Again");
            egui::ScrollArea::horizontal().id_salt("watched").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for w in &self.watched {
                        let (r, rect) = card(ui, CARD, &w.title);
                        self.paint_poster(ui, &r, rect, &w.poster, &w.title, &mut want);
                        card_caption(ui, &r, &w.title, &if w.episodes > 0 { format!("{} episodes watched", w.episodes) } else { "Watched".to_string() });
                        let meta = key::split(&w.key).map(|(kind, id)| MetaPreview { id: id.into(), kind: kind.into(), name: w.title.clone(), poster: w.poster.clone(), ..Default::default() });
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
        let (mut more, mut retry) = (Vec::new(), Vec::new());
        for row in &self.rows {
            match &row.page.items {
                Some(items) if items.is_empty() => {
                    if let Some(e) = &row.error {
                        row_title(ui, &row.title);
                        ui.horizontal(|ui| {
                            ui.set_min_height(32.0);
                            if small_button(ui, "Retry").clicked() {
                                retry.push(row.src.clone());
                            }
                            // Last and truncated: a long error would push the button out of the window.
                            ui.add(egui::Label::new(RichText::new(format!("Couldn't load: {e}")).color(ui.visuals().weak_text_color())).truncate()).on_hover_text(e);
                        });
                    }
                }
                Some(items) => {
                    row_title(ui, &row.title);
                    let near_end = self.ui_cards(ui, &row.title, items, &mut want, &mut open, &mut quick);
                    if near_end {
                        more.push(row.src.clone());
                    }
                }
                None => {
                    row_title(ui, &row.title);
                    ui.spinner();
                }
            }
        }
        for src in retry {
            if let Some(row) = self.rows.iter_mut().find(|r| r.src == src) {
                (row.page, row.error) = (Paged::new(row.page.skippable), None);
            }
            self.fetch_row(&ctx, src);
        }
        for src in more {
            let Some(skip) = self.rows.iter_mut().find(|r| r.src == src).and_then(|r| r.page.next_page(true)) else { continue };
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
                Ok(()) => self.reload_history(),
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
            self.open_streams(&ctx, m, Some(v), Autoplay::Only);
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
        if let Some(name) = person {
            self.search = name;
            self.search(&ctx);
        }
    }

    /// Horizontally scrolling poster cards; a click selects the title.
    /// Returns true when one of the last 3 cards is on screen.
    pub(crate) fn ui_cards(&self, ui: &mut egui::Ui, salt: &str, items: &[MetaPreview], want: &mut Vec<String>, open: &mut Option<(MetaPreview, bool)>, quick: &mut Option<MetaPreview>) -> bool {
        if items.is_empty() {
            return false;
        }
        // Cards past the viewport are not built at all; the spare ones keep Tab able to walk into the next card.
        const SPARE: usize = 2;
        let mut near_end = false;
        let slot = CARD * GROW + vec2(0.0, CAPTION_H);
        let step = slot.x + ui.spacing().item_spacing.x;
        egui::ScrollArea::horizontal().id_salt(salt).show_viewport(ui, |ui, view| {
            ui.set_width((step * items.len() as f32 - ui.spacing().item_spacing.x).max(0.0));
            ui.set_height(slot.y);
            let origin = ui.max_rect().min;
            let first = ((view.min.x / step).floor() as usize).saturating_sub(SPARE).min(items.len());
            let last = ((view.max.x / step).ceil() as usize + SPARE).min(items.len());
            // A child's widget ids depend on how many siblings came before it: count the skipped cards so focus and hover animations survive scrolling.
            ui.skip_ahead_auto_ids(first);
            for (i, m) in items[first..last].iter().enumerate().map(|(i, m)| (i + first, m)) {
                let at = egui::Rect::from_min_size(origin + vec2(i as f32 * step, 0.0), slot);
                ui.scope_builder(UiBuilder::new().max_rect(at), |ui| {
                    let (r, rect) = card(ui, CARD, &m.name);
                    if r.gained_focus() {
                        r.scroll_to_me(None);
                    }
                    // The scroll viewport clips, so a visible third-to-last card means near the end.
                    near_end |= i + 3 >= items.len() && ui.is_rect_visible(r.rect);
                    self.paint_poster(ui, &r, rect, &m.poster, &m.name, want);
                    meta_caption(ui, &r, rect, m);
                    if card_play(ui, &r, rect, &m.name) {
                        *quick = Some(m.clone());
                    }
                    if r.on_hover_text(card_tip(m)).clicked() {
                        *open = Some((m.clone(), false));
                    }
                });
            }
        });
        near_end
    }

    /// Poster image in `rect`, or the title on a gray card until it loads.
    pub(crate) fn paint_poster(&self, ui: &egui::Ui, r: &egui::Response, rect: egui::Rect, poster: &Option<String>, name: &str, want: &mut Vec<String>) {
        // Only fetch posters of cards that are on screen.
        let mut lazy = Vec::new();
        let img = if ui.is_rect_visible(r.rect) { self.card_image(poster, &mut lazy) } else { None };
        want.extend(lazy);
        match img {
            Some(t) => egui::Image::from_texture(t).alt_text(name).corner_radius(12).paint_at(ui, rect),
            None => card_text(ui, rect, name),
        }
        card_edge(ui, rect);
    }

    /// Edge-to-edge banner under the header: backdrop fading into the page, then the title.
    /// On Home the title, Play and description are centered; on the open title's page they sit left,
    /// with its facts and the Library and Trailer buttons. A clicked trailer goes to `trailer`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ui_hero(
        &mut self,
        ui: &mut egui::Ui,
        meta: &MetaPreview,
        want: &mut Vec<String>,
        open: &mut Option<(MetaPreview, bool)>,
        play: &mut Option<(String, String, String, Subs)>,
        trailer: &mut Option<Stream>,
    ) {
        let detail = self.selected.as_ref().is_some_and(|(m, ..)| m == meta);
        let screen = ui.ctx().content_rect();
        let h = if detail { (screen.height() * 0.7).clamp(440.0, 640.0) } else { (screen.height() * 0.84).clamp(480.0, 800.0) };
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
        if std::mem::take(&mut self.scroll_top) {
            ui.scroll_to_rect(rect, Some(Align::TOP));
        }
        let (background, logo, genres, runtime, year, rating, trailers) = {
            let d = self.selected_details();
            (
                meta.background.clone().or_else(|| d?.background.clone()),
                d.and_then(|d| d.logo.clone()).or_else(|| meta.logo.clone()),
                d.map(|d| d.genres.clone()).filter(|g| !g.is_empty()).unwrap_or_else(|| meta.genres.clone()),
                d.and_then(|d| d.runtime.clone()),
                d.and_then(|d| d.release_info.clone()).or_else(|| meta.release_info.clone()),
                d.and_then(|d| d.imdb_rating.clone()).or_else(|| meta.imdb_rating.clone()),
                d.map(|d| d.trailers.clone()).unwrap_or_default(),
            )
        };
        // Out through the page margins to the window edges, and up under the header.
        let bleed = rect.expand2(vec2(f32::from(PAGE_X), 0.0));
        let painter = ui.painter().with_clip_rect(bleed.intersect(screen));
        let (dark, bg) = (ui.visuals().dark_mode, ui.visuals().panel_fill);
        painter.rect_filled(bleed, 0, if dark { RAISED } else { Color32::from_gray(225) });
        // Backdrop first; fall back to the poster when it is missing or rejected.
        if let Some(t) = self.image(&background, want).or_else(|| self.image(&meta.poster, want)) {
            painter.image(t.id, bleed, cover_uv(bleed, t.size), Color32::WHITE);
        }
        let wash = |a: u8| if dark { Color32::from_black_alpha(a) } else { Color32::from_white_alpha(a) };
        painter.rect_filled(bleed, 0, wash(if detail { 150 } else { 70 }));
        fade(&painter, egui::Rect::from_min_max(pos2(bleed.left(), bleed.bottom() - bleed.height() * 0.5), bleed.max), Color32::TRANSPARENT, bg);
        if detail {
            fade_x(&painter, bleed.with_max_x(bleed.left() + bleed.width() * 0.7), bg.gamma_multiply(0.85), Color32::TRANSPARENT);
        }
        let (strong, soft, weak) = (ui.visuals().strong_text_color(), ui.visuals().text_color(), ui.visuals().weak_text_color());
        let kind = match meta.kind.as_str() {
            "movie" => "Movie",
            "series" => "Series",
            k => k,
        };
        let in_lib = self.saved.iter().any(|t| t.key == key::item(&meta.kind, &meta.id));
        // Play: the only stream right away, else the title's streams; Choose source: always the streams.
        let (mut do_play, mut choose) = (false, false);
        let capped = |text: &str, font: egui::FontId, color, width, rows| {
            let mut job = egui::text::LayoutJob::simple(text.to_owned(), font, color, width);
            job.wrap.max_rows = rows;
            egui::Label::new(job).wrap()
        };
        if detail {
            let col_w = rect.width().min(1024.0);
            let col = egui::Rect::from_min_max(pos2(rect.center().x - col_w / 2.0, rect.top() + HEADER_H + 16.0), pos2(rect.center().x + col_w / 2.0, rect.bottom() - 36.0));
            ui.scope_builder(UiBuilder::new().max_rect(col).layout(Layout::bottom_up(Align::LEFT)), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    let (main, arrow) = split_button(ui, "Play", "Choose source");
                    (do_play, choose) = (main.clicked(), arrow.clicked());
                    let label = if in_lib { format!("{}  In library", icons::CHECK) } else { format!("{}  Library", icons::PLUS) };
                    if dark_pill(ui, &label).clicked() {
                        self.toggle_saved(meta);
                    }
                    // One pill: addons often send several trailers under the title's own name.
                    match trailers.as_slice() {
                        [] => {}
                        [t] => {
                            if dark_pill(ui, &format!("{}  Trailer", icons::PLAY)).clicked() {
                                *trailer = Some(t.clone());
                            }
                        }
                        // Scoped: the pill's style must not carry over to the buttons after it.
                        many => drop(ui.scope(|ui| {
                            let button = dark_pill_widget(ui, &format!("{}  Trailers", icons::PLAY));
                            egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
                                ui.set_width(200.0);
                                for (i, t) in many.iter().enumerate() {
                                    let name = match t.title.as_deref() {
                                        Some(title) if title != meta.name => title.to_string(),
                                        _ => format!("Trailer {}", i + 1),
                                    };
                                    if menu_item(ui, icons::PLAY, &name).clicked() {
                                        *trailer = Some(t.clone());
                                    }
                                }
                            });
                        })),
                    }
                    self.list_menu(ui, meta);
                });
                ui.add_space(22.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    if let Some(r) = &rating {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.label(RichText::new(icons::STAR).family(fill()).size(14.0).color(Color32::from_rgb(250, 204, 21)));
                        ui.label(RichText::new(r).size(15.0).family(bold()).color(Color32::from_rgb(74, 222, 128)));
                        ui.spacing_mut().item_spacing.x = 12.0;
                    }
                    for fact in [&year, &runtime].into_iter().flatten() {
                        ui.label(RichText::new(fact).size(15.0).color(soft));
                    }
                    tag(ui, kind);
                    if !genres.is_empty() {
                        ui.add(egui::Label::new(RichText::new(genres.join(", ")).size(15.0).color(weak)).truncate());
                    }
                });
                ui.add_space(26.0);
                match self.image(&logo, want) {
                    Some(t) => ui.add(egui::Image::from_texture(t).alt_text(&meta.name).max_height(110.0).max_width(col_w.min(520.0))),
                    None => ui.add(capped(&meta.name, egui::FontId::new(48.0, bold()), strong, col_w * 0.8, 2)),
                };
            });
        } else {
            let col_w = (rect.width() - 32.0).min(640.0);
            let col = egui::Rect::from_min_max(pos2(rect.center().x - col_w / 2.0, rect.top() + HEADER_H + 16.0), pos2(rect.center().x + col_w / 2.0, rect.bottom() - 48.0));
            ui.scope_builder(UiBuilder::new().max_rect(col).layout(Layout::bottom_up(Align::Center)), |ui| {
                let facts: Vec<&String> = [&year, &runtime].into_iter().flatten().collect();
                if !facts.is_empty() {
                    ui.label(RichText::new(facts.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" · ")).size(13.0).color(weak));
                }
                ui.add_space(14.0);
                if let Some(d) = &meta.description {
                    // Laid out here: a Label given text would wrap it to the whole column.
                    let mut job = egui::text::LayoutJob::simple(d.clone(), egui::FontId::proportional(15.0), soft, col_w.min(460.0));
                    (job.wrap.max_rows, job.halign) = (2, Align::Center);
                    ui.add(egui::Label::new(ui.painter().layout_job(job)));
                }
                ui.add_space(18.0);
                let row_w = split_button_width(ui, "Play") + 2.0 * (10.0 + 47.0);
                ui.allocate_ui_with_layout(vec2(row_w, 47.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let (main, arrow) = split_button(ui, "Play", "Choose source");
                    (do_play, choose) = (main.clicked(), arrow.clicked());
                    let (icon, label) = if in_lib { (icons::CHECK, "Remove from library") } else { (icons::PLUS, "Add to library") };
                    if round_button(ui, icon, label).clicked() {
                        self.toggle_saved(meta);
                    }
                    self.list_menu(ui, meta);
                });
                ui.add_space(18.0);
                let line: Vec<&str> = std::iter::once(kind).chain(genres.iter().take(2).map(String::as_str)).collect();
                ui.label(RichText::new(line.join(" · ")).size(15.0).color(soft));
                ui.add_space(10.0);
                let r = match self.image(&logo, want) {
                    Some(t) => ui.add(egui::Image::from_texture(t).alt_text(&meta.name).max_height(150.0).max_width(col_w.min(460.0)).sense(Sense::click())),
                    None => ui.add(capped(&meta.name, egui::FontId::new(52.0, bold()), strong, col_w, 2).sense(Sense::click())),
                };
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("More info").clicked() {
                    *open = Some((meta.clone(), false));
                }
            });
        }
        if do_play {
            match &self.selected {
                Some((m, streams, subs)) if m == meta => match (only_source(streams), self.play_key()) {
                    (Some(src), Some((k, t))) => *play = Some((k, t, src, subs.clone())),
                    _ => self.show_streams = true,
                },
                _ => *open = Some((meta.clone(), true)),
            }
        }
        if choose {
            self.show_streams = true;
            if !detail {
                *open = Some((meta.clone(), false));
            }
        }
    }

    /// The open title's page under its banner: description beside its facts (director, genres, links), then the cast.
    /// A clicked cast or director name goes to `search`.
    pub(crate) fn ui_details(ui: &mut egui::Ui, d: &Meta, search: &mut Option<String>) {
        ui.add_space(32.0);
        let (soft, weak) = (ui.visuals().text_color(), ui.visuals().weak_text_color());
        let facts_w = if ui.available_width() > 720.0 { 300.0 } else { ui.available_width() };
        let mut facts = |ui: &mut egui::Ui| {
            let heading = |ui: &mut egui::Ui, t: &str| {
                ui.label(RichText::new(t).size(12.5).color(weak));
            };
            if !d.director.is_empty() {
                heading(ui, "Director");
                ui.horizontal_wrapped(|ui| {
                    // Flush: the separators carry their own spaces.
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for (i, person) in d.director.iter().enumerate() {
                        if i > 0 {
                            ui.label(RichText::new(", ").size(15.0).color(soft));
                        }
                        if ui.link(RichText::new(person).size(15.0).color(soft)).on_hover_text(format!("Search for {person}")).clicked() {
                            *search = Some(person.clone());
                        }
                    }
                });
                ui.add_space(14.0);
            }
            if !d.genres.is_empty() {
                heading(ui, "Genre");
                chip_row(ui, |ui| {
                    for g in &d.genres {
                        tag(ui, g);
                    }
                });
                ui.add_space(14.0);
            }
            if !d.links.is_empty() {
                heading(ui, "Links");
                ui.horizontal_wrapped(|ui| {
                    for l in &d.links {
                        // `links` holds only http(s) URLs (checked in `parse_meta`).
                        ui.hyperlink_to(&l.name, &l.url).on_hover_text(format!("{}: {}", l.category, l.url));
                    }
                });
            }
        };
        let description = |ui: &mut egui::Ui| {
            if let Some(text) = &d.description {
                ui.label(RichText::new(text).size(15.5).color(soft).line_height(Some(24.0)));
            }
        };
        if facts_w < ui.available_width() {
            ui.horizontal_top(|ui| {
                let w = ui.available_width() - facts_w - 36.0 - 2.0 * ui.spacing().item_spacing.x;
                ui.allocate_ui_with_layout(vec2(w, 0.0), Layout::top_down(Align::LEFT), |ui| {
                    ui.set_width(w);
                    description(ui);
                });
                ui.add_space(36.0);
                ui.allocate_ui_with_layout(vec2(facts_w, 0.0), Layout::top_down(Align::LEFT), |ui| {
                    ui.set_width(facts_w);
                    facts(ui);
                });
            });
        } else {
            description(ui);
            ui.add_space(20.0);
            facts(ui);
        }
        if !d.cast.is_empty() {
            ui.add_space(32.0);
            ui.label(heading_text(ui, "Cast", 18.0));
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
                for name in &d.cast {
                    if person(ui, name).on_hover_text(format!("Search for {name}")).clicked() {
                        *search = Some(name.clone());
                    }
                }
            });
        }
    }
}
