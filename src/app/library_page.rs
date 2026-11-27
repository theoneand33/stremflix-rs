//! The Library tab: saved titles and lists, downloads, local folders, calendar.

use crate::*;

impl App {
    /// Queue stream `source` (http URL or magnet) for the downloads folder as `d`; it starts when a slot is free.
    /// `hint` (a file name or URL path) gives the file extension.
    pub(crate) fn download(&mut self, ctx: &egui::Context, d: Download, source: String, headers: Vec<(String, String)>, hint: Option<String>) {
        let step = self.queue.add(&self.db, d, source, headers, hint.as_deref());
        self.apply_downloads(ctx, step);
    }

    /// Run the transfers a queue change started, show its errors, list what finished.
    pub(crate) fn apply_downloads(&mut self, ctx: &egui::Context, step: download::Step) {
        for e in step.errors {
            report(&mut self.error, e);
        }
        if let Some(d) = step.finished {
            self.downloads.insert(0, d);
        }
        for t in step.start {
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            let engine = self.engine();
            self.rt.spawn(download::run(t, engine, move |e| {
                let _ = tx.send(Msg::Download(e));
                ctx.request_repaint();
            }));
        }
    }

    pub(crate) fn reload_library(&mut self) {
        self.library = self.db.library_items().unwrap_or_default();
        self.library_folders = self.db.library_folders().unwrap_or_default();
    }

    /// Scan `dir` off the UI thread (a big folder or network share can take minutes); the reply replaces its library items.
    pub(crate) fn scan_folder(&self, ctx: &egui::Context, dir: PathBuf, rescan: bool) {
        self.spawn(ctx, async move {
            let res = tokio::task::spawn_blocking({
                let dir = dir.clone();
                move || scan(&dir).map_err(|e| e.to_string())
            })
            .await?;
            Ok(Msg::Scanned(dir, rescan, res))
        });
    }

    /// Store year and rating on a saved title once its details are known (imports and older rows lack them).
    pub(crate) fn backfill_saved_facts(&mut self, d: &Meta) {
        let key = key::item(&d.kind, &d.id);
        let (year, rating) = saved_facts(d);
        if let Some(t) = self.saved.iter_mut().find(|t| t.key == key && (year.is_some_and(|y| t.year != Some(y)) || rating.is_some_and(|r| t.rating != Some(r)))) {
            (t.year, t.rating) = (year.or(t.year), rating.or(t.rating));
            if let Err(e) = self.db.set_saved_facts(&key, year, rating) {
                report(&mut self.error, format!("saving library: {e:#}"));
            }
        }
    }

    /// Add the selected title to the library, or remove it.
    pub(crate) fn toggle_saved(&mut self, meta: &MetaPreview) {
        let key = key::item(&meta.kind, &meta.id);
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
    pub(crate) fn lists_changed(&mut self, res: anyhow::Result<()>) {
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
    pub(crate) fn list_menu(&mut self, ui: &mut egui::Ui, meta: &MetaPreview) {
        let t = SavedTitle { key: key::item(&meta.kind, &meta.id), title: meta.name.clone(), kind: meta.kind.clone(), poster: meta.poster.clone(), year: None, rating: None, last_watched: None };
        let r = egui::containers::menu::MenuButton::from_button(round_pill(icons::LIST_PLUS)).ui(ui, |ui| {
            let mut change = None;
            for l in &self.lists {
                let mut on = l.keys.contains(&t.key);
                if ui.checkbox(&mut on, &l.name).changed() {
                    change = Some((l.id, on));
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.list_name).hint_text("New list…").char_limit(60).desired_width(140.0)).accessible_name("New list name");
                if small_button(ui, "Create").clicked() {
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
        r.0.accessible_name("Add to list").on_hover_text("Add to list");
    }

    pub(crate) fn ui_library(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (mut want, mut open, mut quick) = (Vec::new(), None, None);
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.set_min_height(32.0);
            section_heading(ui, "Saved titles");
            let before = self.lib_sort;
            dropdown(ui, "lib_sort", self.lib_sort.label(), |ui| {
                for s in [LibSort::Added, LibSort::Title, LibSort::Watched, LibSort::Rating, LibSort::Year] {
                    ui.selectable_value(&mut self.lib_sort, s, s.label());
                }
            });
            // Watch times are read when loaded, so refresh them when the order is picked.
            if self.lib_sort != before {
                self.saved = self.db.saved_titles().unwrap_or_default();
            }
            dropdown(ui, "lib_kind", self.lib_kind.label(), |ui| {
                for k in [LibKind::All, LibKind::Movie, LibKind::Series] {
                    ui.selectable_value(&mut self.lib_kind, k, k.label());
                }
            });
            let list = self.lists.iter().find(|l| Some(l.id) == self.lib_list);
            dropdown(ui, "lib_list", list.map_or("All saved", |l| l.name.as_str()), |ui| {
                ui.selectable_value(&mut self.lib_list, None, "All saved");
                for l in &self.lists {
                    ui.selectable_value(&mut self.lib_list, Some(l.id), &l.name);
                }
            });
            ui.add(egui::TextEdit::singleline(&mut self.list_name).hint_text("List name").char_limit(60).desired_width(140.0)).accessible_name("List name");
            let mut res = None;
            if small_button(ui, "New list").clicked() {
                res = Some(self.db.create_list(&self.list_name).map(|id| self.lib_list = Some(id)));
            }
            if let Some(id) = self.lib_list {
                if small_button(ui, "Rename").clicked() {
                    res = Some(self.db.rename_list(id, &self.list_name));
                }
                if small_button(ui, "Delete list").on_hover_text("Its titles stay saved").clicked() {
                    res = Some(self.db.delete_list(id));
                }
            }
            if let Some(res) = res {
                if res.is_ok() {
                    self.list_name.clear();
                }
                self.lists_changed(res);
            }
            let edit = ui.add(egui::TextEdit::singleline(&mut self.lib_query).hint_text("Filter by title").desired_width(180.0)).accessible_name("Filter library");
            if (edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape))) || (!self.lib_query.is_empty() && small_button(ui, icons::X).accessible_name("Clear filter").on_hover_text("Clear filter").clicked()) {
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
            muted(ui, "Nothing saved yet. Open a title and choose \"Add to library\".");
        } else if items.is_empty() {
            muted(ui, if list.is_some() { "No titles match in this list." } else { "No saved titles match." });
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
        ui.add_space(18.0);
        let mut cal_open = None;
        egui::CollapsingHeader::new(heading_text(ui, "Calendar", 20.0)).id_salt("calendar").show(ui, |ui| {
            let live: Vec<_> = self.upcoming.iter().filter(|(m, _)| self.saved.iter().any(|t| t.key == key::item("series", &m.id))).cloned().collect();
            let groups = calendar_groups(&live, unix_days());
            if groups.is_empty() {
                muted(ui, "No upcoming episodes for saved series in the next 30 days (or still loading).");
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
        row_title(ui, "Downloads");
        if self.downloads.is_empty() && self.queue.jobs().is_empty() {
            muted(ui, "Nothing downloaded yet. Open a title and choose \"Download\" on a stream.");
        }
        // (index, 0 pause, 1 resume, 2 remove)
        let mut act = None;
        for (i, j) in self.queue.jobs().iter().enumerate() {
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
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
                // Last and truncated: a long title would push the buttons out of the window.
                ui.add(egui::Label::new(&j.d.title).truncate());
            });
        }
        let step = match act {
            Some((i, 0)) => self.queue.pause(&self.db, i),
            Some((i, 1)) => self.queue.resume(&self.db, i),
            Some((i, _)) => self.queue.remove(&self.db, i),
            None => download::Step::default(),
        };
        self.apply_downloads(&ctx, step);
        let (mut play, mut delete) = (None, None);
        for d in &self.downloads {
            ui.horizontal(|ui| {
                // Stale if the file was removed outside the app; Delete still clears the row.
                let on_disk = d.path.exists();
                let btn = ui.add_enabled_ui(on_disk, |ui| primary_button(ui, &format!("Play {}", d.title))).inner;
                if btn.on_disabled_hover_text("File is missing").clicked() {
                    play = Some(d.clone());
                }
                if self.confirm_delete_dl.as_ref() == Some(&d.path) {
                    field_label(ui, "Delete file?");
                    if small_button(ui, "Yes").clicked() {
                        delete = Some(d.clone());
                    }
                    if small_button(ui, "No").clicked() {
                        self.confirm_delete_dl = None;
                    }
                } else if small_button(ui, "Delete").on_hover_text("Delete the downloaded file").clicked() {
                    self.confirm_delete_dl = Some(d.path.clone());
                }
                ui.add(egui::Label::new(d.path.display().to_string()).truncate());
            });
        }
        if let Some(d) = play {
            self.play(&ctx, d.key(), d.title, d.path.to_string_lossy().into_owned(), vec![]);
        }
        if let Some(d) = delete {
            self.confirm_delete_dl = None;
            let gone = std::fs::remove_file(&d.path).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) });
            match gone.map_err(anyhow::Error::from).and_then(|()| self.db.remove_download(&d)) {
                Ok(()) => {
                    self.downloads.retain(|x| x.path != d.path);
                    self.reload_history();
                }
                Err(e) => report(&mut self.error, format!("delete {}: {e:#}", d.path.display())),
            }
        }
        row_title(ui, "Local files");
        let tip = "Or drop a .torrent file on the window, or paste a magnet anywhere";
        if form_row(ui, "Torrent", &mut self.magnet, "magnet:?xt=urn:btih:... or /path/to/file.torrent", "Play torrent", Some(tip), false) {
            let m = self.magnet.clone();
            self.play_torrent(&ctx, &m);
        }
        if form_row(ui, "Folder path", &mut self.folder, "/path/to/videos", "Add folder", None, false) {
            self.scan_folder(&ctx, PathBuf::from(self.folder.trim()), false);
        }
        let (mut rescan, mut forget) = (None, None);
        for f in &self.library_folders {
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                if small_button(ui, "Rescan").on_hover_text("Look for added and removed files").clicked() {
                    rescan = Some(f.clone());
                }
                if small_button(ui, "Remove").on_hover_text("Take this folder out of the library; its files stay on disk").clicked() {
                    forget = Some(f.clone());
                }
                // Last and truncated: a long path would push the buttons out of the window.
                ui.add(egui::Label::new(f).truncate());
            });
        }
        if let Some(f) = rescan {
            self.scan_folder(&ctx, PathBuf::from(f), true);
        }
        if let Some(f) = forget {
            match self.db.remove_library_folder(&f) {
                Ok(()) => self.reload_library(),
                Err(e) => report(&mut self.error, format!("removing {f}: {e:#}")),
            }
        }
        let mut play = None;
        for it in self.library.iter().filter(|it| title_matches(&it.title, &self.lib_query)) {
            ui.horizontal(|ui| {
                if primary_button(ui, &format!("Play {}", it.title)).clicked() {
                    play = Some(it.clone());
                }
                ui.add(egui::Label::new(it.path.display().to_string()).truncate());
            });
        }
        if let Some(it) = play {
            let p = it.path.to_string_lossy().into_owned();
            self.play(&ctx, key::file(&p), it.title, p, vec![]);
        }
    }
}
