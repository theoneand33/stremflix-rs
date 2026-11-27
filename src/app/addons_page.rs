//! The Addons tab: install, order, configure, update and browse addons.

use crate::*;

impl App {
    pub(crate) fn ui_addons(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(8.0);
        if form_row(ui, "Manifest URL", &mut self.addon_url, "http://127.0.0.1:7070/manifest.json", "Install", None, false) {
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
        if form_row(ui, "Stremio data", &mut self.import_path, "Auto-detect local Stremio", "Import from Stremio", Some("Import addons, library, watched titles and progress"), true) {
            let path = Some(PathBuf::from(self.import_path.trim())).filter(|p| !p.as_os_str().is_empty());
            self.spawn(&ctx, async move {
                let (_, imp) = tokio::task::spawn_blocking(move || stremio_import::import(path.as_deref())).await??;
                Ok(Msg::Imported(imp))
            });
        }
        if ui.add_enabled_ui(!self.checking_updates && !self.addons.is_empty(), |ui| small_button(ui, if self.checking_updates { "Checking…" } else { "Check for updates" })).inner.clicked() {
            self.checking_updates = true;
            let (client, urls) = (self.client.clone(), self.addons.iter().map(|(u, _)| u.clone()).collect::<Vec<_>>());
            self.spawn(&ctx, async move {
                let mut out = Vec::new();
                // Four at a time keeps a long addon list from opening dozens of connections.
                for chunk in urls.chunks(4) {
                    let res = concurrently(chunk.to_vec(), |u| {
                        let c = client.clone();
                        async move { Ok((u.clone(), c.fetch_manifest(&u).await.map_err(|e| health_error(&e)))) }
                    })
                    .await;
                    out.extend(res.into_iter().flatten());
                }
                Ok(Msg::UpdateCheck(out))
            });
        }
        row_title(ui, "Installed addons");
        let mut update = None;
        let mut remove = None;
        let mut configure = None;
        let mut replace = None;
        let mut swap = None;
        let mut details = None;
        let mut reset_order = false;
        if self.addons.is_empty() {
            muted(ui, "No addons installed. Paste a manifest URL above or import from Stremio.");
        } else {
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                field_label(ui, "Order for");
                for (k, label) in [("", "All"), ("movie", "Movies"), ("series", "Shows")] {
                    if pill(ui, label, self.addon_order_kind == k, egui::Role::RadioButton).clicked() {
                        self.addon_order_kind = k.to_string();
                    }
                }
                if !self.addon_order_kind.is_empty() && self.addon_orders.get(&self.addon_order_kind).is_some_and(|o| !o.is_empty()) && small_button(ui, "Reset to global").clicked() {
                    reset_order = true;
                }
            });
            caption(ui, "Per-type order applies to metadata, streams and subtitles; catalog rows and search keep the global order.");
        }
        let n = self.addons.len();
        let urls: Vec<String> = self.addons.iter().map(|(u, _)| u.clone()).collect();
        let view = arrange_rows(&urls, self.addon_orders.get(&self.addon_order_kind).map_or(&[][..], |o| o));
        for (pos, &i) in view.iter().enumerate() {
            let (url, m) = &self.addons[i];
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), Layout::left_to_right(Align::Center), |ui| {
                // Order is priority: streams and catalogs of higher addons come first.
                if ui.add_enabled_ui(pos > 0, |ui| small_button(ui, "Up")).inner.on_hover_text("Higher priority").clicked() {
                    swap = Some(pos - 1);
                }
                if ui.add_enabled_ui(pos + 1 < n, |ui| small_button(ui, "Down")).inner.on_hover_text("Lower priority").clicked() {
                    swap = Some(pos);
                }
                // Fixed-width name cell, so the actions line up whatever the names' lengths.
                ui.allocate_ui_with_layout(vec2(280.0, 32.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_width(280.0);
                    let mut job = egui::text::LayoutJob::default();
                    job.append(&m.name, 0.0, egui::TextFormat::simple(egui::FontId::proportional(15.0), ui.visuals().strong_text_color()));
                    job.append(&m.version, 8.0, egui::TextFormat::simple(egui::FontId::proportional(13.0), ui.visuals().weak_text_color()));
                    ui.add(egui::Label::new(job).truncate()).on_hover_text(url);
                });
                let open = self.addon_details.as_ref() == Some(url);
                if small_button(ui, if open { "Hide details" } else { "Details" }).clicked() {
                    details = Some((!open).then(|| url.clone()));
                }
                if m.behavior_hints.configurable && small_button(ui, "Configure").clicked() {
                    configure = Some(url.clone());
                }
                // Asked first, like deleting a download: a configured URL can't be recovered once removed.
                if self.confirm_uninstall.as_ref() == Some(url) {
                    field_label(ui, "Uninstall?");
                    if small_button(ui, "Yes").clicked() {
                        remove = Some(url.clone());
                    }
                    if small_button(ui, "No").clicked() {
                        self.confirm_uninstall = None;
                    }
                } else if small_button(ui, "Uninstall").clicked() {
                    self.confirm_uninstall = Some(url.clone());
                }
            });
            match self.addon_updates.iter().find(|(u, _)| u == url).map(|(_, r)| r) {
                Some(Ok(new)) if !same_addon(m, new) => {
                    muted(ui, "Addon id changed; not offered");
                }
                Some(Ok(new)) if stremflix_rs::addon::is_newer_addon(&new.version, &m.version) => {
                    ui.horizontal(|ui| {
                        ui.set_min_height(32.0);
                        ui.label(format!("Update available: {} \u{2192} {}", m.version, new.version));
                        if small_button(ui, "Update").clicked() {
                            update = Some((url.clone(), new.clone()));
                        }
                    });
                }
                Some(Err(e)) => {
                    muted(ui, e);
                }
                Some(Ok(_)) => {
                    muted(ui, "Reachable");
                }
                None => {}
            }
            if m.behavior_hints.configuration_required {
                muted(ui, "Needs configuration before use. Press Configure.");
            }
            if self.addon_details.as_ref() == Some(url) {
                Self::ui_addon_details(ui, m);
            }
            if let Some(link) = stremflix_rs::addon::install_link(url) {
                ui.collapsing("Install on phone (QR)", |ui| qr_ui(ui, &link));
            }
            if let Some((old, pasted)) = self.configuring.as_mut().filter(|(o, _)| o == url)
                && form_row(ui, "Configured URL", pasted, "https://.../manifest.json", "Replace", None, false)
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
        if reset_order {
            match settings::ADDON_ORDER.set_at(&self.db, &self.addon_order_kind, &Vec::new()) {
                Ok(()) => {
                    self.addon_orders.remove(&self.addon_order_kind);
                    self.build_rows(&ctx);
                }
                Err(e) => report(&mut self.error, format!("saving addon order: {e:#}")),
            }
        }
        if let Some(p) = swap {
            let mut ordered: Vec<String> = view.iter().map(|&i| urls[i].clone()).collect();
            ordered.swap(p, p + 1);
            let kind = self.addon_order_kind.clone();
            let saved = if kind.is_empty() {
                self.addons.sort_by_key(|(u, _)| ordered.iter().position(|o| o == u));
                self.db.reorder_addons(&ordered)
            } else {
                let r = settings::ADDON_ORDER.set_at(&self.db, &kind, &ordered);
                if r.is_ok() {
                    self.addon_orders.insert(kind, ordered);
                }
                r
            };
            if let Err(e) = saved {
                report(&mut self.error, format!("saving addon order: {e:#}"));
            }
            self.build_rows(&ctx);
        }
        if let Some(u) = remove {
            self.confirm_uninstall = None;
            let _ = self.db.remove_addon(&u);
            self.addons.retain(|(x, _)| *x != u);
            self.build_rows(&ctx);
        }
        self.ui_browse_addons(ui);
    }

    /// Addons listed by the `addon_catalog` resource of installed addons (e.g. Cinemeta's Community catalog), each installable.
    pub(crate) fn ui_browse_addons(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        row_title(ui, "Browse addons");
        let label = |addon: &str, c: &CatalogDef| sanitize_text(&format!("{addon} - {} ({})", c.name.as_deref().unwrap_or(&c.id), kind_label(&c.kind)), 400);
        let sources: Vec<(AddonCatalogSrc, String)> = self
            .addons
            .iter()
            .flat_map(|(u, m)| m.addon_catalogs.iter().map(move |c| ((u.clone(), c.kind.clone(), c.id.clone()), label(&m.name, c))))
            .collect();
        if sources.is_empty() {
            muted(ui, "Install an addon with addon catalogs to browse more addons, e.g. Cinemeta: https://v3-cinemeta.strem.io/manifest.json");
            return;
        }
        let mut pick = self.browse.as_ref().map(|b| b.0.clone()).filter(|s| sources.iter().any(|(x, _)| x == s));
        let shown = pick.as_ref().and_then(|p| sources.iter().find(|(x, _)| x == p)).map_or("Pick a catalog", |(_, l)| l.as_str());
        dropdown(ui, "addon_catalog", shown, |ui| {
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
            muted(ui, "No addons in this catalog.");
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
                    muted(ui, u.host_str().unwrap_or_default());
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
    pub(crate) fn ui_addon_details(ui: &mut egui::Ui, m: &Manifest) {
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
}
