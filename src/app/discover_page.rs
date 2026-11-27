//! The Discover tab: a catalog grid picked by type, catalog and extras.

use crate::*;

impl App {
    /// Pick a type, catalog and its extras (genre, year, sort, ...); the catalog shows as a poster grid that loads further pages as it scrolls.
    pub(crate) fn ui_discover(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let cats: Vec<(&String, &String, &CatalogDef)> = self
            .addons
            .iter()
            .filter(|(_, m)| m.has_resource("catalog"))
            .flat_map(|(u, m)| m.catalogs.iter().filter(|c| c.discoverable()).map(move |c| (u, &m.name, c)))
            .collect();
        if cats.is_empty() {
            ui.add_space(40.0);
            muted(ui, "Install an addon with a catalog in Addons to start browsing.");
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
            (d.src, d.page) = (Some(src.clone()), Paged::new(cur.2.supports("skip")));
            let client = self.client.clone();
            self.spawn(&ctx, async move {
                let res = client.discover(&src.0, &src.1, &src.2, &src.3).await.map_err(|e| format!("{e:#}"));
                Ok(Msg::Discover(src, res))
            });
        }
        // Picker changes apply on the next frame, after the checks above.
        let d = &mut self.discover;
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            dropdown(ui, "discover_kind", kind_label(&d.kind).to_owned(), |ui| {
                for k in &kinds {
                    ui.selectable_value(&mut d.kind, k.to_string(), kind_label(k));
                }
            });
            let cat_label = |addon: &str, c: &CatalogDef| format!("{addon} - {}", c.name.as_deref().unwrap_or(&c.id));
            dropdown(ui, "discover_cat", cat_label(cur.1, cur.2), |ui| {
                for (u, addon, c) in &in_kind {
                    ui.selectable_value(&mut d.cat, ((*u).clone(), c.id.clone()), cat_label(addon, c));
                }
            });
            for e in cur.2.pickers() {
                let at = d.extras.iter().position(|(n, _)| *n == e.name);
                let text = at.map_or_else(|| format!("Any {}", e.name), |i| d.extras[i].1.clone());
                dropdown(ui, ("discover_extra", &e.name), text, |ui| {
                    let mut pick = None;
                    if !cur.2.is_required(&e.name) && ui.selectable_label(at.is_none(), format!("Any {}", e.name)).clicked() {
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
        let (mut want, mut open, mut quick, mut more) = (Vec::new(), None, None, false);
        match &self.discover.page.items {
            None => {
                ui.spinner();
            }
            Some(items) if items.is_empty() => {
                muted(ui, "Nothing in this catalog.");
            }
            Some(items) => {
                let mut near_end = false;
                ui.horizontal_wrapped(|ui| {
                    for (i, m) in items.iter().enumerate() {
                        let (r, rect) = card(ui, CARD, &m.name);
                        // About a row of cards before the end, so the next page is on its way when the grid runs out.
                        near_end |= i + 6 >= items.len() && ui.is_rect_visible(r.rect);
                        self.paint_poster(ui, &r, rect, &m.poster, &m.name, &mut want);
                        meta_caption(ui, &r, rect, m);
                        if card_play(ui, &r, rect, &m.name) {
                            quick = Some(m.clone());
                        }
                        if r.on_hover_text(card_tip(m)).clicked() {
                            open = Some(m.clone());
                        }
                    }
                });
                if self.discover.page.loading_more {
                    ui.spinner();
                }
                more = near_end;
            }
        }
        if let (true, Some(src)) = (more, self.discover.src.clone())
            && let Some(skip) = self.discover.page.next_page(true)
        {
            let client = self.client.clone();
            self.spawn(&ctx, async move {
                let extras: Vec<_> = src.3.iter().cloned().chain([("skip".to_string(), skip.to_string())]).collect();
                let res = client.discover(&src.0, &src.1, &src.2, &extras).await.map_err(|e| format!("{e:#}"));
                Ok(Msg::DiscoverMore(src, skip, res))
            });
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
}
