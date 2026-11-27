//! The Settings tab, the profile lock, and the Stremio account.

use crate::*;

impl App {
    /// Pull the signed-in Stremio account's addons and library into the local DB.
    pub(crate) fn sync_account(&mut self, ctx: &egui::Context) {
        // A locked profile keeps the addons it was locked with.
        if self.account_busy || self.locked() {
            return;
        }
        let Some(key) = self.account_key() else { return };
        self.account_busy = true;
        let client = self.client.clone();
        self.spawn(ctx, async move {
            let imp = stremio_import::pull(&client, &key).await.map_err(|e| format!("Stremio account sync: {e:#}"));
            Ok(Msg::Synced(key, imp))
        });
    }

    /// Push local addons and library to the signed-in Stremio account.
    pub(crate) fn upload_account(&mut self, ctx: &egui::Context) {
        if self.account_busy {
            return;
        }
        let Some(key) = self.account_key() else { return };
        let up = match stremio_import::Upload::read(&self.db) {
            Ok(up) => up,
            Err(e) => return report(&mut self.error, format!("reading library: {e:#}")),
        };
        (self.account_busy, self.account_note) = (true, None);
        let client = self.client.clone();
        self.spawn(ctx, async move {
            let r = stremio_import::push(&client, &key, &up).await;
            let r = r.map(|(a, l)| format!("Uploaded {a} addons and {l} changed library items.")).map_err(|e| format!("Stremio account upload: {e:#}"));
            Ok(Msg::Uploaded(key, r))
        });
    }

    /// Profile switcher and the child-safe lock. True while the profile is locked: the rest of Settings stays hidden.
    pub(crate) fn ui_profile(&mut self, ui: &mut egui::Ui) -> bool {
        let shown = |p: &str| if p.is_empty() { "Default".to_string() } else { p.to_string() };
        if self.locked() {
            caption(ui, format!("{} is child-safe: addons and settings are locked.", shown(&self.profile)));
            if pin_row(ui, &mut self.pin_entry, "Unlock") {
                if self.pin_entry == self.child_pin {
                    self.set_child_pin(ui.ctx(), String::new());
                } else {
                    report(&mut self.error, "wrong PIN");
                }
                self.pin_entry.clear();
            }
            return self.locked();
        }
        let names = profile::list(&self.root);
        let choices: Vec<(String, String)> = std::iter::once(String::new()).chain(names).map(|n| (shown(&n), n)).collect();
        let choices: Vec<(&str, String)> = choices.iter().map(|(l, n)| (l.as_str(), n.clone())).collect();
        let mut switch = combo_row(ui, "Profile", &mut self.profile_next, &choices);
        if form_row(ui, "New profile", &mut self.profile_new, "Name", "Create", Some("A profile has its own addons, library, progress and settings"), false) {
            self.profile_next = self.profile_new.trim().to_string();
            switch = true;
        }
        if switch {
            match profile::switch(&self.root, &self.profile_next) {
                Ok(()) => self.profile_new.clear(),
                Err(e) => {
                    report(&mut self.error, format!("switching profile: {e:#}"));
                    self.profile_next = self.profile.clone();
                }
            }
        }
        if self.profile_next != self.profile {
            caption(ui, format!("Restart the app to open {}.", shown(&self.profile_next)));
        }
        if pin_row(ui, &mut self.pin_entry, "Lock") {
            let pin = std::mem::take(&mut self.pin_entry);
            self.set_child_pin(ui.ctx(), pin);
        }
        caption(ui, "Child-safe: a PIN locks this profile. It hides the Addons page, these settings and addons flagged adult until the PIN is entered here.");
        self.locked()
    }

    /// Lock the profile with `pin`, or unlock it with an empty one, and reload the addons either way.
    pub(crate) fn set_child_pin(&mut self, ctx: &egui::Context, pin: String) {
        if let Err(e) = settings::CHILD_PIN.set(&self.db, &pin) {
            return report(&mut self.error, format!("saving the PIN: {e:#}"));
        }
        // Also undo a switch that was picked but not started: a locked profile can only be left by its PIN.
        if !pin.is_empty() && let Err(e) = profile::switch(&self.root, &self.profile) {
            report(&mut self.error, format!("switching profile: {e:#}"));
        }
        self.profile_next = self.profile.clone();
        self.child_pin = pin;
        if let (true, Ok(urls)) = (self.locked(), self.db.unrated_addons()) && !urls.is_empty() {
            let client = self.client.clone();
            self.spawn(ctx, async move {
                let mut found = Vec::new();
                for u in urls {
                    // A failed fetch leaves the addon hidden.
                    if let Ok(m) = client.fetch_manifest(&u).await {
                        found.push((u, m));
                    }
                }
                Ok(Msg::Rated(found))
            });
        }
        self.reload_addons();
        // Nothing loaded from a now hidden addon stays open, and replies still on their way are dropped (`nav`).
        self.close_details();
        (self.pending, self.opening, self.browse, self.search_page, self.discover) = (None, None, None, None, Discover::default());
        self.build_rows(ctx);
    }

    /// Torrent engine and external player options; each is saved as soon as it changes.
    pub(crate) fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let mut save: Vec<(&str, String)> = Vec::new();
        ui.add_space(8.0);
        section_heading(ui, "Profile");
        if self.ui_profile(ui) {
            return;
        }
        row_title(ui, "Appearance");
        field_row(ui, "Theme", |ui, _| {
            for (name, light) in [("Dark", false), ("Light", true)] {
                if pill(ui, name, self.light == light, egui::Role::RadioButton).clicked() && self.light != light {
                    self.light = light;
                    save.push(settings::THEME_LIGHT.entry(&light));
                }
            }
            ui.add_space(12.0);
            field_label(ui, "Accent");
            for (i, (name, color)) in ACCENTS.iter().enumerate() {
                if swatch(ui, name, *color, self.accent == i).clicked() && self.accent != i {
                    self.accent = i;
                    save.push(settings::ACCENT.entry(&name.to_string()));
                }
            }
        });
        ui.scope(|ui| {
            ui.set_max_width(ROW_WIDTH);
            let step = stepper(ui, "UI scale", &format!("{:.0}%", self.ui_scale * 100.0));
            if step != 0 {
                ui.ctx().set_zoom_factor(clamp_scale(self.ui_scale + step as f32 * 0.25));
            }
        });
        caption(ui, "Also Ctrl and +, Ctrl and −, Ctrl and 0.");
        if !save.is_empty() {
            apply_theme(ui.ctx(), self.light, ACCENTS[self.accent].1);
        }
        row_title(ui, "Stremio account");
        let ctx = ui.ctx().clone();
        if let Some(email) = self.account.clone() {
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
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
                    let key = self.account_key().unwrap_or_default();
                    let cleared = self.db.transaction(|| settings::STREMIO_AUTH.clear(&self.db).and_then(|()| settings::STREMIO_EMAIL.clear(&self.db)));
                    match cleared {
                        Ok(()) => {
                            // A running sync for this key is dropped when it ends.
                            (self.account, self.account_busy, self.confirm_upload, self.account_note) = (None, false, false, None);
                            let client = self.client.clone();
                            // Best effort: the key is already forgotten locally.
                            self.rt.spawn(async move { stremio_import::logout(&client, &key).await.inspect_err(|e| stremflix_rs::elog!("Stremio sign-out: {e:#}")) });
                        }
                        Err(e) => report(&mut self.error, format!("signing out: {e:#}")),
                    }
                }
            });
            if self.confirm_upload {
                caption(ui, "Overwrites the account's addon list with this order (addons only on the account are kept, last). Saved titles missing on the account are added (ones it has, even removed, stay as they are), and progress is updated only where it is newer there. Nothing is deleted.");
                ui.horizontal(|ui| {
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
                caption(ui, note);
            }
        } else {
            let mut go = false;
            field_row(ui, "Email", |ui, l| {
                ui.add(egui::TextEdit::singleline(&mut self.login.0).desired_width(260.0)).labelled_by(l);
                let l = field_label(ui, "Password");
                let r = ui.add(egui::TextEdit::singleline(&mut self.login.1).password(true).desired_width(200.0)).labelled_by(l.id);
                go = ui.add_enabled_ui(!self.account_busy, |ui| primary_button(ui, "Sign in")).inner.clicked()
                    || !self.account_busy && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if self.account_busy {
                    ui.spinner();
                }
            });
            caption(ui, "Copies your addons, library and watched titles from api.strem.io. Settings can upload them back after signing in.");
            let email = self.login.0.trim().to_string();
            if go && !email.is_empty() && !self.login.1.is_empty() {
                // Not kept after use, even if signing in fails.
                let (client, password) = (self.client.clone(), std::mem::take(&mut self.login.1));
                self.account_busy = true;
                self.spawn(&ctx, async move {
                    let key = stremio_import::login(&client, &email, &password).await.map_err(|e| format!("Stremio sign-in: {e:#}"));
                    Ok(Msg::SignedIn(email, key))
                });
            }
        }
        row_title(ui, "Data");
        if small_button(ui, "Export data").on_hover_text("Write addons, saved titles, progress and settings to a JSON file in the data folder").clicked() {
            match self.db.export_file(&self.data_dir.join("exports")) {
                Ok(p) => self.data_msg = Some(format!("Exported to {}", p.display())),
                Err(e) => report(&mut self.error, format!("exporting: {e:#}")),
            }
        }
        caption(ui, "The export includes addon URLs, which can hold account tokens. Keep it private.");
        if form_row(ui, "Import file", &mut self.data_import, "/path/to/stremflix-rs-export.json", "Import", Some("Merges the file into your data; nothing is deleted"), false) {
            match self.db.import_file(std::path::Path::new(self.data_import.trim())) {
                Ok(m) => self.data_msg = Some(format!("{m}. Restart the app to apply.")),
                Err(e) => report(&mut self.error, format!("importing: {e:#}")),
            }
        }
        if let Some(m) = &self.data_msg {
            caption(ui, m);
        }
        row_title(ui, "Diagnostics");
        caption(ui, format!("Version {}", stremflix_rs::diag::version()));
        let out = self.data_dir.join("diagnostics.txt");
        if small_button(ui, "Export diagnostics").on_hover_text(format!("Version, OS and recent log without secrets, saved to {}", out.display())).clicked() {
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
                Ok(()) => self.diag_msg = Some(format!("Diagnostics saved to {}", out.display())),
                Err(e) => report(&mut self.error, format!("diagnostics export: {e}")),
            }
        }
        if let Some(m) = &self.diag_msg {
            caption(ui, m);
        }
        row_title(ui, "Torrents");
        let hint = self.data_dir.join("torrents").display().to_string();
        if form_row(ui, "Cache location", &mut self.torrent_dir, &hint, "Save", Some("Empty: the app's data folder"), true) {
            let d = self.torrent_dir.trim().to_string();
            // Relative paths would depend on the working directory.
            if d.is_empty() || std::path::Path::new(&d).is_absolute() {
                save.push(settings::TORRENT_DIR.entry(&d));
            } else {
                report(&mut self.error, "cache location must be an absolute path");
            }
        }
        if combo_row(ui, "Cache size", &mut self.torrent_cache, &CACHE_SIZES) {
            save.push(settings::TORRENT_CACHE.entry(&self.torrent_cache));
        }
        if combo_row(ui, "Connections", &mut self.torrent_profile, &Profile::ALL.map(|p| (p.label(), p))) {
            save.push(settings::TORRENT_PROFILE.entry(&self.torrent_profile));
        }
        for (title, setting, val) in [("Download limit", &settings::TORRENT_DOWN_LIMIT, &mut self.torrent_limits.0), ("Upload limit", &settings::TORRENT_UP_LIMIT, &mut self.torrent_limits.1)] {
            if combo_row(ui, title, val, &SPEED_LIMITS) {
                save.push(setting.entry(val));
            }
        }
        if combo_row(ui, "Seed to ratio", &mut self.torrent_seed.ratio, &SEED_RATIOS) {
            save.push(settings::SEED_RATIO.entry(&self.torrent_seed.ratio));
        }
        if combo_row(ui, "Seed for", &mut self.torrent_seed.secs, &SEED_TIMES) {
            save.push(settings::SEED_SECS.entry(&self.torrent_seed.secs));
        }
        let note = "Finished torrents keep seeding after playback ends (the \"Seed for\" timer starts then, not when the download completes) until a seeding limit is reached (both Off: stop at once). Seeding torrents are exempt from the cache size: they stay on disk until their limit is reached and the next release. Other stopped torrents stay on disk up to the cache size, and are cleared when the app starts.";
        caption(ui, note);
        if self.torrent.get().is_some() {
            caption(ui, "A torrent has already played: restart the app to apply changes.");
        }
        if let Some(engine) = self.torrent.get() {
            ui.ctx().request_repaint_after(Duration::from_secs(2));
            let playing = self.player.as_ref().and_then(|p| p.torrent);
            for t in engine.list() {
                ui.horizontal(|ui| {
                    if self.confirm_delete == Some(t.id) {
                        field_label(ui, "Delete files?");
                        if small_button(ui, "Yes").clicked() {
                            self.confirm_delete = None;
                            // The engine also refuses the torrent being streamed; this covers a play that started after the prompt.
                            if playing == Some(t.id) {
                                report(&mut self.error, "torrent is playing");
                            } else {
                                // Off the UI thread: deleting a big torrent's files can take a while.
                                let (engine, id, tx, ctx) = (engine.clone(), t.id, self.tx.clone(), ui.ctx().clone());
                                self.rt.spawn(async move {
                                    if let Err(e) = engine.remove(id).await {
                                        let _ = tx.send(Msg::Error(format!("{e:#}")));
                                    }
                                    ctx.request_repaint();
                                });
                            }
                        }
                        if small_button(ui, "No").clicked() {
                            self.confirm_delete = None;
                        }
                    } else if playing != Some(t.id) && small_button(ui, "Delete").on_hover_text("Stop and delete its files").clicked() {
                        self.confirm_delete = Some(t.id);
                    }
                    let state = if t.paused { "stopped" } else { "running" };
                    let (done, total) = t.progress;
                    ui.add(egui::Label::new(format!("{} - {state}, {}% of {}, uploaded {}", t.name, done * 100 / total.max(1), fmt_size(total), fmt_size(t.uploaded))).truncate());
                });
            }
        }
        row_title(ui, "Proxy");
        combo_row(ui, "Type", &mut self.proxy[0], &[("Off", String::new()), ("HTTP", "http".to_string()), ("SOCKS5", "socks5".to_string())]);
        let mut apply = false;
        if !self.proxy[0].is_empty() {
            field_row(ui, "Address", |ui, l| {
                ui.add(egui::TextEdit::singleline(&mut self.proxy[1]).hint_text("host:port").desired_width(260.0)).labelled_by(l);
            });
            field_row(ui, "Username", |ui, l| {
                ui.add(egui::TextEdit::singleline(&mut self.proxy[2]).hint_text("optional").desired_width(160.0)).labelled_by(l);
                let l = field_label(ui, "Password");
                ui.add(egui::TextEdit::singleline(&mut self.proxy[3]).password(true).desired_width(160.0)).labelled_by(l.id);
            });
        }
        let [kind, addr, user, pass] = &self.proxy;
        match stremflix_rs::addon::proxy_url(kind, addr, user, pass) {
            Ok(_) => apply = primary_button(ui, "Save proxy").clicked(),
            Err(e) => {
                error_caption(ui, e);
            }
        }
        if apply {
            for (setting, v) in settings::PROXY.iter().zip(&self.proxy) {
                save.push(setting.entry(v));
            }
        }
        let note = "Applies after a restart. HTTP covers addons, downloads, the update check and mpv streams, but not torrents. \
            SOCKS5 covers addons, downloads, the update check and torrent peers and HTTP trackers, but not mpv's direct HTTP streams \
            (mpv has no SOCKS support) or DHT. The password is stored unencrypted in the app's database.";
        caption(ui, note);
        row_title(ui, "Player");
        if form_row(ui, "External player", &mut self.external_player, "vlc", "Save", Some("Command run with the stream URL; no shell"), true) {
            save.push(settings::EXTERNAL_PLAYER.entry(&self.external_player.trim().to_string()));
        }
        let mut style = self.sub_style;
        ui.scope(|ui| {
            ui.set_max_width(ROW_WIDTH);
            if switch_row(ui, "Play next episode automatically", &mut self.autoplay_next).changed() {
                save.push(settings::AUTOPLAY_NEXT.entry(&self.autoplay_next));
            }
            if switch_row(ui, "Get skip times from TheIntroDB (sends what you watch)", &mut self.introdb).changed() {
                save.push(settings::INTRODB.entry(&self.introdb));
            }
            if switch_row(ui, "Hardware decoding", &mut self.hwdec).changed() {
                save.push(settings::HWDEC.entry(&self.hwdec));
            }
            if switch_row(ui, "Notify about new episodes", &mut self.notify_eps).changed() {
                save.push(settings::NOTIFY_EPS.entry(&self.notify_eps));
            }
            if switch_row(ui, "Minimize to tray on close", &mut self.tray_on_close).changed() {
                save.push(settings::TRAY_ON_CLOSE.entry(&self.tray_on_close));
            }
            if switch_row(ui, "Pause when minimized", &mut self.pause_on_minimize).changed() {
                save.push(settings::PAUSE_ON_MINIMIZE.entry(&self.pause_on_minimize));
            }
            let step = stepper(ui, "Subtitle size", &format!("{:.1}x", style.scale));
            if step != 0 {
                style.scale = (style.scale * 10.0).round() / 10.0 + step as f64 * 0.1;
            }
            swatch_row(ui, "Subtitle color", &SUB_COLORS, &mut style.color);
            swatch_row(ui, "Subtitle outline", &SUB_OUTLINES, &mut style.outline);
            style.pos += stepper(ui, "Subtitle position", &format!("{}%", style.pos)) as f64 * 5.0;
            switch_row(ui, "Subtitle background box", &mut style.background);
            style.delay += stepper(ui, "Subtitle delay", &format!("{:+.1}s", style.delay)) as f64 * 0.1;
            if style.delay != 0.0 && link_button(ui, "Reset delay").clicked() {
                style.delay = 0.0;
            }
            let mut off = settings::subs_off(&self.db).unwrap_or(true);
            if switch_row(ui, "Subtitles off by default", &mut off).changed() {
                save.push(settings::SUB_OFF.entry(&Some(off)));
            }
            field_row(ui, "Default speed", |ui, _| {
                ui.spacing_mut().item_spacing.x = 6.0; // seven chips end inside the row
                for sp in SPEEDS {
                    if pill(ui, &format!("{sp}x"), self.default_speed == sp, egui::Role::RadioButton).clicked() {
                        self.default_speed = sp;
                        save.push(settings::DEFAULT_SPEED.entry(&sp));
                    }
                }
            });
        });
        let style = style.clamped();
        if style != self.sub_style {
            self.sub_style = style;
            save.push(settings::SUB_STYLE.entry(&style));
        }
        for (i, setting, label) in [(0, &settings::SEEK_STEP, "Arrow-key seek"), (1, &settings::SEEK_STEP_SHIFT, "Shift + arrow seek")] {
            field_row(ui, label, |ui, l| {
                field_style(ui);
                if ui.add(egui::DragValue::new(&mut self.seek_steps[i]).range(SEEK_STEP_RANGE).suffix(" s")).labelled_by(l).changed() {
                    save.push(setting.entry(&self.seek_steps[i]));
                }
            });
        }
        row_title(ui, "Languages");
        for (i, setting, label) in [(0, &settings::ALANG, "Audio"), (1, &settings::SLANG, "Subtitles")] {
            field_row(ui, label, |ui, l| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.langs[i]).hint_text("en,eng").desired_width(400.0)).labelled_by(l);
                match valid_langs(&self.langs[i]) {
                    Some(v) if r.changed() => {
                        // As in the player: a subtitle language preference turns subtitles back on after a remembered "Off".
                        if i == 1 && !v.is_empty() {
                            save.push(settings::SUB_OFF.entry(&Some(false)));
                        }
                        save.push(setting.entry(&v));
                    }
                    None => {
                        error_caption(ui, "Use codes like en,eng,pt-BR (max 8)");
                    }
                    _ => {}
                }
            });
        }
        caption(ui, "Comma-separated, most preferred first. Also editable in the player.");
        row_title(ui, "Network");
        field_row(ui, "User-Agent", |ui, l| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.user_agent).hint_text("stremflix-rs").desired_width(400.0)).labelled_by(l);
            match stremflix_rs::addon::valid_user_agent(&self.user_agent) {
                Some(v) if r.changed() => {
                    stremflix_rs::addon::set_user_agent(&v);
                    save.push(settings::USER_AGENT.entry(&v));
                }
                None => {
                    error_caption(ui, format!("Printable ASCII only, max {} characters", stremflix_rs::addon::MAX_UA_LEN));
                }
                _ => {}
            }
        });
        caption(ui, "Sent to addons and for downloads. Empty uses the default. Torrent (librqbit) traffic is not covered.");
        row_title(ui, "Customize Home");
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
                ui.add(egui::Label::new(if hidden { RichText::new(title).weak() } else { RichText::new(title) }).truncate());
            });
        }
        if self.home_all.is_empty() {
            muted(ui, "No catalog rows yet.");
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
            save.push(settings::HOME_ORDER.entry(&self.home_order));
            save.push(settings::HOME_HIDDEN.entry(&self.home_hidden));
            if rebuild {
                self.build_rows(&ctx);
            } else {
                let order = &self.home_all;
                self.rows.sort_by_key(|r| order.iter().position(|(k, _)| *k == row_key(&r.src)));
                self.rows.retain(|r| !self.home_hidden.contains(&row_key(&r.src)));
            }
        }
        for (k, v) in save {
            if let Err(e) = self.db.set_setting(k, &v) {
                report(&mut self.error, format!("saving {k}: {e:#}"));
            }
        }
    }
}
