//! The floating header, the search palette and the toasts over every page; tab and Back navigation.

use crate::*;

impl App {
    /// A locked profile never shows the Addons page, even for one frame; a saved tab or an install link may point there.
    pub(crate) fn leave_locked_tab(&mut self) {
        if self.locked() && self.tab == Tab::Addons {
            self.tab = Tab::Home;
        }
    }

    /// Floating header over the page: logo, centered nav and search button, profile menu.
    /// `scrolled`: the page's scroll offset; the header turns solid as content moves under it.
    pub(crate) fn ui_header(&mut self, ui: &mut egui::Ui, full: egui::Rect, scrolled: f32) {
        let ctx = ui.ctx().clone();
        let bar = egui::Rect::from_min_size(full.min, vec2(full.width(), HEADER_H));
        let bg = ui.visuals().panel_fill;
        // Registered before the header's buttons and after the page: clicks between the buttons stop here, not on what scrolled under it.
        ui.interact(bar, ui.make_persistent_id("header"), Sense::click());
        // Scrim: a soft fade over a banner, solid once the page scrolls under the header.
        ui.painter().rect_filled(bar, 0, bg.gamma_multiply((scrolled / 60.0).clamp(0.0, 1.0)));
        fade(ui.painter(), bar.with_max_y(bar.bottom() + 24.0), bg.gamma_multiply(0.92), Color32::TRANSPARENT);
        let inner = bar.shrink2(vec2(f32::from(PAGE_X), 0.0));
        ui.scope_builder(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)), |ui| {
            ui.add(egui::Image::new((logo(ui.ctx()).id(), vec2(28.0, 28.0))).alt_text("Stremflix-rs"));
            ui.add_space(2.0);
            ui.label(RichText::new("Stremflix").size(17.0).family(bold()).color(ui.visuals().strong_text_color()));
            if self.selected.is_some() && self.tab == Tab::Home {
                ui.add_space(14.0);
                if island_button(ui, icons::ARROW_LEFT, "Back to browse").clicked() {
                    self.close_details();
                }
            }
            // A clicked title shows no page until its addons reply.
            if let Some(title) = self.loading() {
                ui.add_space(12.0);
                ui.spinner().on_hover_text(format!("Loading {title}"));
            }
        });
        let locked = self.locked();
        // Nav island and search button, centered on the window as one group.
        let tabs = [(Tab::Home, "Home"), (Tab::Discover, "Discover"), (Tab::Library, "Library")];
        let tab_w: f32 = tabs.iter().map(|(_, n)| ui.painter().layout_no_wrap((*n).into(), egui::FontId::proportional(14.0), Color32::WHITE).size().x + 30.0).sum();
        let nav_w = tab_w + 2.0 * (tabs.len() - 1) as f32 + 10.0;
        let left = bar.center().x - (nav_w + 10.0 + 42.0) / 2.0;
        let nav = egui::Rect::from_min_size(pos2(left, bar.center().y - 22.0), vec2(nav_w, 44.0));
        island(ui.painter(), nav);
        let pill_slot = ui.painter().add(egui::Shape::Noop);
        let mut sel_rect = None;
        ui.scope_builder(UiBuilder::new().max_rect(nav.shrink(5.0)).layout(Layout::left_to_right(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (tab, name) in tabs {
                // The search page is not a tab, so nothing is current while it is up.
                let current = self.tab == tab && (self.search_page.is_none() || self.selected.is_some());
                let r = nav_tab(ui, name, current);
                if current {
                    sel_rect = Some(r.rect);
                }
                if r.clicked() {
                    self.go_tab(tab);
                }
            }
        });
        if let Some(target) = sel_rect {
            let rect = self.pill_rect(&ctx, target);
            ui.painter().set(pill_slot, nav_pill(rect));
        }
        let search = egui::Rect::from_min_size(pos2(nav.right() + 10.0, bar.center().y - 21.0), vec2(42.0, 42.0));
        let r = ui.put(search, |ui: &mut egui::Ui| island_button(ui, icons::MAGNIFYING_GLASS, "Search"));
        // "/" (when not typing) or Ctrl+F opens search too.
        if r.clicked() || ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F) || (!self.had_focus && i.consume_key(egui::Modifiers::NONE, egui::Key::Slash))) {
            // Opens empty, so recent searches show.
            (self.search_open, self.search) = (true, String::new());
        }
        let shown = if self.profile.is_empty() { "Default".to_string() } else { self.profile.clone() };
        let r = ui.put(egui::Rect::from_center_size(pos2(inner.right() - 16.0, bar.center().y), vec2(32.0, 32.0)), |ui: &mut egui::Ui| avatar(ui, &shown));
        egui::Popup::menu(&r).align(egui::RectAlign::BOTTOM_END).show(|ui| {
            ui.set_width(200.0);
            menu_heading(ui, &shown);
            if !locked && menu_item(ui, icons::PUZZLE_PIECE, "Addons").clicked() {
                self.go_tab(Tab::Addons);
            }
            if menu_item(ui, icons::GEAR, "Settings").clicked() {
                self.go_tab(Tab::Settings);
            }
        });
    }

    /// Search as a centered command palette over the dimmed page: query field, then recent searches.
    pub(crate) fn ui_search_palette(&mut self, ctx: &egui::Context) {
        if !self.search_open {
            return;
        }
        let id = egui::Id::unique("search_palette");
        let area = egui::Modal::default_area(id).anchor(egui::Align2::CENTER_TOP, vec2(0.0, 120.0));
        let frame = egui::Frame::popup(&ctx.global_style()).corner_radius(14).inner_margin(0);
        let (mut run, mut del, mut clear) = (None, None, false);
        let resp = egui::Modal::new(id).area(area).frame(frame).backdrop_color(Color32::from_black_alpha(170)).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 32.0).min(576.0));
            let weak = ui.visuals().weak_text_color();
            egui::Frame::NONE.inner_margin(egui::Margin::symmetric(18, 12)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::MAGNIFYING_GLASS).family(phosphor()).size(18.0).color(weak));
                    let edit = egui::TextEdit::singleline(&mut self.search).hint_text("Search titles, people, genres…").frame(egui::Frame::NONE).font(egui::FontId::proportional(16.0)).desired_width(ui.available_width() - 52.0);
                    let r = ui.add(edit).accessible_name("Search");
                    // Checked before focus is taken back below, which would clear `lost_focus`.
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.search.trim().is_empty() {
                        run = Some(self.search.clone());
                    } else if ui.memory(|m| m.focused().is_none()) {
                        r.request_focus();
                    }
                    tag(ui, "ESC");
                });
            });
            let line = ui.cursor().top();
            ui.painter().hline(ui.max_rect().x_range(), line, egui::Stroke::new(1.0, HAIRLINE));
            egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 10)).show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                if !self.search.trim().is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(18.0);
                        muted(ui, "Press Enter to search");
                        ui.add_space(18.0);
                    });
                } else if self.search_history.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(28.0);
                        muted(ui, "Start typing to search...");
                        ui.add_space(28.0);
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        caption(ui, "Recent searches");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| clear = link_button(ui, "Clear").clicked());
                    });
                    for (i, q) in self.search_history.iter().enumerate() {
                        let r = menu_item(ui, icons::MAGNIFYING_GLASS, q);
                        let x = egui::Rect::from_center_size(pos2(r.rect.right() - 20.0, r.rect.center().y), vec2(28.0, 28.0));
                        let xr = ui.interact(x, r.id.with("remove"), Sense::click()).on_hover_text("Remove");
                        xr.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Remove {q} from recent searches")));
                        if r.hovered() || xr.hovered() {
                            ui.painter().text(x.center(), egui::Align2::CENTER_CENTER, icons::X, egui::FontId::new(14.0, phosphor()), weak);
                        }
                        if xr.clicked() {
                            del = Some(i);
                        } else if r.clicked() {
                            run = Some(q.clone());
                        }
                    }
                }
            });
        });
        if clear || del.is_some() {
            match del {
                Some(i) => drop(self.search_history.remove(i)),
                None => self.search_history.clear(),
            }
            self.save_search_history();
        }
        if let Some(q) = run {
            self.search = q;
            self.search(ctx);
            self.search_open = false;
        } else if resp.should_close() {
            self.search_open = false;
        }
    }

    /// Errors, notices and the update offer as cards in the bottom-right corner.
    pub(crate) fn ui_toasts(&mut self, ctx: &egui::Context) {
        if self.error.is_none() && self.notice.is_none() && self.update.is_none() {
            return;
        }
        egui::Area::new(egui::Id::unique("toasts")).order(egui::Order::Foreground).anchor(egui::Align2::RIGHT_BOTTOM, vec2(-24.0, -24.0)).show(ctx, |ui| {
            ui.set_max_width(440.0);
            let toast = |ui: &mut egui::Ui| egui::Frame::popup(ui.style()).corner_radius(12).inner_margin(egui::Margin::symmetric(14, 8));
            if let Some(e) = self.error.clone()
                && toast(ui).show(ui, |ui| dismissible(ui, RichText::new(format!("Error: {e}")).color(ERROR))).inner
            {
                self.error = None;
            }
            if let Some(n) = self.notice.clone()
                && toast(ui).show(ui, |ui| dismissible(ui, n)).inner
            {
                self.notice = None;
            }
            if let Some((url, asset)) = self.update.clone() {
                toast(ui).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(32.0);
                        ui.label("A new version is available.");
                        if self.installing {
                            ui.spinner();
                            ui.label("Installing…");
                        } else if let Some(asset) = asset
                            && small_button(ui, "Install").on_hover_text("Download it and replace this build; it runs after a restart").clicked()
                        {
                            self.installing = true;
                            let (tx, ctx) = (self.tx.clone(), ui.ctx().clone());
                            self.rt.spawn(async move {
                                let _ = tx.send(Msg::UpdateInstalled(stremflix_rs::update::install(&asset).await.map_err(|e| format!("{e:#}"))));
                                ctx.request_repaint();
                            });
                        }
                        ui.hyperlink_to("Release page", url);
                        if !self.installing && small_button(ui, "Dismiss").clicked() {
                            self.update = None;
                        }
                    });
                });
            }
        });
    }

    pub(crate) fn pill_rect(&mut self, ctx: &egui::Context, target: egui::Rect) -> egui::Rect {
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

    /// A nav click. Home while already on Home also leaves the open title, like Back; from another tab it returns to that title.
    pub(crate) fn go_tab(&mut self, tab: Tab) {
        if tab == Tab::Home && self.tab == Tab::Home && self.selected.is_some() {
            self.close_details();
        }
        self.tab = tab;
        self.search_page = None;
    }

    /// Escape or the mouse's back button: close the file picker, else the title page, else the search page.
    pub(crate) fn go_back(&mut self) {
        if self.file_pick.is_some() {
            self.clear_file_pick();
        } else if self.tab == Tab::Home && self.selected.is_some() {
            self.close_details();
        } else if self.tab == Tab::Home {
            self.search_page = None;
        }
    }
}
