//! The player page: video, controls and menus, and the playback lifecycle around it.

use crate::*;

impl App {
    /// Leave the player like its Back button does, so what opens next isn't behind it.
    pub(crate) fn leave_player(&mut self, ctx: &egui::Context) {
        if self.player.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            self.stop_player();
        }
    }

    /// Leave the player and stop every torrent, so none keeps downloading and seeding.
    pub(crate) fn stop_player(&mut self) {
        self.end_playback();
        self.release_torrents(None);
    }

    /// Leave the player, saving its progress; its torrent keeps running for whatever plays next.
    pub(crate) fn end_playback(&mut self) {
        self.sleep = None;
        self.timer_paused = None;
        self.preload = None;
        self.save_progress();
        self.player = None;
    }

    /// A media key or OS overlay button; ignored when nothing plays.
    pub(crate) fn media_event(&mut self, ctx: &egui::Context, e: souvlaki::MediaControlEvent) {
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
                return self.stop_player();
            }
            None => return,
        };
        if let Err(e) = res {
            report(&mut self.error, e);
        }
    }

    /// Publish the title, duration and play state to the OS media controls when they change.
    pub(crate) fn publish_media(&mut self) {
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

    pub(crate) fn poll_player(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.player else { return };
        if !p.player.poll() || p.player.state.error.is_some() {
            if let Some(e) = p.player.state.error.clone() {
                report(&mut self.error, e);
            }
            self.stop_player();
            return;
        }
        if self.sleep.is_some_and(|(t, _)| Instant::now() >= t) {
            self.sleep = None;
            self.timer_paused = Some(false);
            // The sleep timer's pause is not ours: minimize neither resumes nor re-pauses it.
            p.auto_pause = AutoPause::Yielded;
            if let Err(e) = p.player.command(&["set", "pause", "yes"]) {
                report(&mut self.error, e);
            }
            return;
        }
        let st = &p.player.state;
        self.timer_paused = timer_pause_step(self.timer_paused, st.paused);
        if autoplay_due(self.autoplay_next, self.timer_paused, p.auto_pause, at_end(st.pos, st.dur, st.paused, st.eof)) && !looping(p.repeat, p.ab) && self.playing_episode() {
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
        self.preload_next(ctx);
        // Asked once the duration is known: it selects the release and closes open-ended credits.
        if self.introdb
            && let Some(p) = self.player.as_mut().filter(|p| p.db_skips.is_none() && p.dur > 0.0)
        {
            p.db_skips = Some(vec![]);
            let (key, dur, client) = (p.key.clone(), p.dur, self.client.clone());
            self.spawn(ctx, async move {
                // No entry (a 4xx) or no network just means no button, so it is not shown as an error.
                let skips = client.introdb_skips(&key, dur).await.unwrap_or_default();
                Ok(Msg::IntroDb(key, skips))
            });
        }
    }

    /// Stop the playing episode and open the next one, autoplaying a stream from the same `bingeGroup`
    /// (else the only stream, else the picker). Without a next episode, just stop.
    pub(crate) fn play_next(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.player else { return };
        if let Some((pos, dur)) = p.player.position() {
            (p.pos, p.dur) = (pos, dur);
        }
        let binge = self.selected.as_ref().and_then(|(m, ..)| self.saved_binge(m));
        let next = (self.selected.as_ref().zip(self.episode.as_ref()))
            .and_then(|((m, ..), cur)| Some((m.clone(), next_episode(&self.episodes, &cur.id)?.clone())));
        let autoplay = Autoplay::Next(binge, self.sleep);
        let preload = self.preload.take();
        // The next episode may come from the same torrent (a season pack), so it keeps running until the next one plays.
        self.end_playback();
        match next {
            Some((m, v)) => match preload.filter(|(id, _)| *id == v.id).and_then(|(_, found)| found) {
                Some((streams, subs)) => {
                    let nav = self.begin_nav();
                    self.handle(ctx, Msg::Streams(m, Some(v), streams, subs, autoplay, nav));
                }
                None => self.open_streams(ctx, m, Some(v), autoplay),
            },
            None => self.release_torrents(None),
        }
    }

    /// Near the end of a series episode, fetch the next episode's streams so `play_next` needn't wait on addons.
    pub(crate) fn preload_next(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.player else { return };
        if self.preload.is_some() || !self.autoplay_next || looping(p.repeat, p.ab) || !preload_due(p.pos, p.dur) || !self.playing_episode() {
            return;
        }
        let Some((m, next)) = (self.selected.as_ref().zip(self.episode.as_ref())).and_then(|((m, ..), cur)| Some((m, next_episode(&self.episodes, &cur.id)?))) else { return };
        let (fetch, id) = (self.fetch_streams(&m.kind, &next.id), next.id.clone());
        self.preload = Some((id.clone(), None));
        self.spawn(ctx, async move { Ok(Msg::Preloaded(id, fetch.await.ok())) });
    }

    /// Full-window Netflix-style player: letterboxed video, controls that fade out after
    /// 3 s without input, red seek bar, ±10 s skips, volume, Audio / Subtitles / Playback panels.
    pub(crate) fn ui_player(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
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
                // First source whose fetch hasn't failed.
                let failed = |u: &String| self.posters.get(u).is_some_and(|p| p.failed);
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
            if self.video.is_none() {
                match VideoSurface::new(&gl, frame) {
                    Ok(v) => self.video = Some(v),
                    Err(e) => report(&mut self.error, e),
                }
            }
            if let Some(v) = &mut self.video {
                let id = v.render(&gl, &mut p.player, size);
                ui.painter().image(id, vrect, egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
        }
        let st = p.player.state.clone();
        if st.buffering || vw == 0 {
            egui::Spinner::new().size(56.0).color(accent(ui.ctx())).paint_at(ui, egui::Rect::from_center_size(rect.center(), vec2(56.0, 56.0)));
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
            // shortcut: multiply assumes nothing else changes speed mid-hold; a speed picked during a hold is halved on release.
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
        let typing = ctx.text_edit_focused(); // read outside ctx.input, like the ones below
        let line = ctx.options(|o| o.input_options.line_scroll_speed); // read outside ctx.input to avoid nested locking
        let vol_id = ui.scope_id().with("volume");
        let over_vol = ctx.read_response(vol_id).is_some_and(|r| r.hovered()); // also outside ctx.input
        let (old_vol, mut vol_held) = (p.volume, false);
        ctx.input(|i| {
            use egui::Key::*;
            // Not while typing: "k" or "," in a popover's text field must not pause or step.
            let k = |key| !typing && i.key_pressed(key);
            // Space activates a focused popover control, so it must not also toggle pause.
            // A short Space press pauses on release; a hold is 2x speed, like the mouse.
            if (i.key_released(Space) && focused.is_none() && !long) || k(K) { cmd(&["cycle", "pause"]) }
            let step = self.seek_steps[i.modifiers.shift as usize] as f64;
            if k(ArrowLeft) { seek = Some((-step, true)) }
            if k(ArrowRight) { seek = Some((step, true)) }
            if k(ArrowUp) { p.volume += 5.0 }
            if k(ArrowDown) { p.volume -= 5.0 }
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
            // Wheel over the bare video or the volume slider: `hovered` is false under a popover or another control.
            if (bg.hovered() || over_vol) && p.menu.is_none() {
                p.volume += f64::from(wheel_steps(&mut p.wheel, i.smooth_scroll_delta.y, line) * 5);
            } else {
                p.wheel = 0.0;
            }
            toggle_fs |= k(F);
            // egui drops the focus on Escape before this runs, so an open popover (the only place to type) closes first.
            if i.key_pressed(Escape) {
                if p.menu.is_some() { p.menu = None } else if fs { toggle_fs = true } else { back = true }
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
            paint.rect_filled(egui::Rect::from_min_size(track.min, vec2(track.width() * frac, th)), 2, accent(ui.ctx()).gamma_multiply(a));
            let knob = pos2(track.left() + track.width() * frac, track.center().y);
            paint.circle_filled(knob, if hot { 9.0 } else { 7.0 }, accent(ui.ctx()).gamma_multiply(a));
            paint.circle_filled(knob, 3.0, white);
            if let Some(t) = p.thumbs.as_ref().and_then(|t| t.take()) {
                let img = egui::ColorImage::from_rgba_unmultiplied(t.size, &t.rgba);
                match &mut p.thumb {
                    Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
                    None => p.thumb = Some(ctx.load_texture("seek-preview", img, egui::TextureOptions::LINEAR)),
                }
            }
            if let Some(h) = r.hover_pos().or(p.scrub.and(r.interact_pointer_pos())).filter(|_| st.dur > 0.0) {
                let mut label_y = bar.top() - 6.0;
                if let (Some(th), [vw, vh]) = (&mut p.thumbs, st.video) && vw > 0 && vh > 0 {
                    // Fits a 192 px box, keeping the video's aspect (portrait too).
                    let side = |a: usize, b: usize| (192 * a / a.max(b)).max(2) & !1;
                    let size = [side(vw, vh), side(vh, vw)];
                    th.request(preview_time(at(h.x), st.dur), size);
                    if let Some(tex) = &p.thumb {
                        // The previous preview stays up while the next one renders.
                        let s = vec2(size[0] as f32, size[1] as f32);
                        // min/max, not clamp: clamp panics once the window is narrower than the preview.
                        let x = h.x.min(rect.right() - 8.0 - s.x / 2.0).max(rect.left() + 8.0 + s.x / 2.0);
                        let img = egui::Rect::from_center_size(pos2(x, label_y - 28.0 - s.y / 2.0), s);
                        paint.rect_filled(img.expand(2.0), 6, Color32::from_black_alpha((220.0 * a) as u8));
                        paint.image(tex.id(), img, egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE.gamma_multiply(a));
                        label_y = img.bottom() + 24.0;
                    }
                }
                paint.text(pos2(h.x, label_y), egui::Align2::CENTER_BOTTOM, fmt_time(at(h.x)), font.clone(), white);
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
            let vol = icon(ui, pos2(vx, y), if st.muted || p.volume <= 0.0 { icons::SPEAKER_X } else { icons::SPEAKER_HIGH }, "Mute");
            if vol.clicked() {
                cmd(&["cycle", "mute"]);
            }
            // Volume slider next to the speaker.
            let vbar = egui::Rect::from_min_size(pos2(vx + 36.0, y - 12.0), vec2(150.0, 24.0));
            let vr = ui.interact(vbar, vol_id, Sense::CLICK | Sense::DRAG);
            vr.widget_info(|| egui::WidgetInfo::slider(true, p.volume, "Volume"));
            // Not `dragged`: that waits for egui's click-or-drag decision, so a press did nothing until release or 6 px.
            vol_held = vr.is_pointer_button_down_on();
            // `clicked` too: a tap can press and release within one frame.
            if vol_held || vr.clicked() {
                p.osd_until = now + 3.0;
                if let Some(q) = vr.interact_pointer_pos() {
                    p.volume = volume_at((q.x - vbar.left()) / vbar.width());
                }
                // The slider shows 0 while muted, so setting a volume also unmutes, even to the value it already had.
                if st.muted {
                    cmd(&["set", "mute", "no"]);
                }
            }
            let vt = egui::Rect::from_center_size(vbar.center(), vec2(vbar.width(), 4.0));
            let vf = if st.muted { 0.0 } else { (p.volume / VOLUME_MAX).clamp(0.0, 1.0) as f32 };
            paint.rect_filled(vt, 2, Color32::from_gray(128).gamma_multiply(a * 0.5));
            paint.rect_filled(egui::Rect::from_min_size(vt.min, vec2(vt.width() * vf, 4.0)), 2, white);
            paint.circle_filled(pos2(vt.left() + vt.width() * vf, vt.center().y), 7.0, white);
            // Audio, subtitles and playback each open their own popover above the button.
            for (i, (m, glyph, label)) in [(Menu::Audio, icons::WAVEFORM, "Audio"), (Menu::Subtitles, icons::SUBTITLES, "Subtitles"), (Menu::Playback, icons::GEAR, "Playback")].into_iter().enumerate() {
                let bx = rect.right() - 52.0 - (2 - i) as f32 * 64.0;
                let b = icon(ui, pos2(bx, y), glyph, label);
                if p.menu == Some(m) {
                    paint.rect_filled(egui::Rect::from_center_size(pos2(bx, y + 22.0), vec2(18.0, 3.0)), 2, accent(ui.ctx()).gamma_multiply(a));
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
                    egui::Frame::new().fill(surface(245)).stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(18))).corner_radius(12).inner_margin(egui::Margin::symmetric(12, 14)).show(ui, |ui| {
                        ui.style_mut().visuals = ui.ctx().style_of(egui::Theme::Dark).visuals.clone();
                        ui.set_width(300.0);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let title = match m { Menu::Audio => "Audio", Menu::Subtitles => "Subtitles", Menu::Playback => "Playback" };
                        ui.label(RichText::new(title).size(18.0).family(bold()).color(Color32::WHITE));
                        // Short windows: the body scrolls under the title instead of the popover running over the controls.
                        // Both bounds: the Area's first-frame size guess would otherwise cap it lower.
                        // 200: the popover's top stays under the top bar, clear of the full screen button.
                        let body = (y - rect.top() - 200.0).max(120.0);
                        egui::ScrollArea::vertical().id_salt("menu_body").max_height(body).min_scrolled_height(body).show(ui, |ui| {
                            if m != Menu::Playback {
                                let (kind, prop) = if m == Menu::Audio { ("audio", "aid") } else { ("sub", "sid") };
                                menu_heading(ui, "Track");
                                let tracks: Vec<_> = st.tracks.iter().filter(|t| t.kind == kind).collect();
                                let off = !tracks.iter().any(|t| t.selected);
                                let entries: Vec<_> = (kind == "sub").then_some((None, "Off", None, off)).into_iter().chain(tracks.iter().map(|t| (Some(t.id), t.label.as_str(), t.lang.as_deref(), t.selected))).collect();
                                if entries.is_empty() {
                                    caption(ui, "No tracks");
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
                                                    if let Err(e) = settings::ALANG.set(&self.db, &v) {
                                                        report(&mut self.error, format!("saving alang: {e:#}"));
                                                    }
                                                }
                                            }
                                            if kind == "sub" {
                                                let mut save = vec![settings::SUB_OFF.entry(&Some(id.is_none()))];
                                                // shortcut: name-only addon languages ("English") fail `valid_langs` and aren't remembered; map names to codes if users hit it.
                                                if let Some(v) = lang.and_then(|l| prefer_lang(&self.langs[1], l)) {
                                                    self.langs[1] = v.clone();
                                                    save.push(settings::SLANG.entry(&v));
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
                                    if style.audio_delay != 0.0 && link_button(ui, "Reset delay").clicked() {
                                        style.audio_delay = 0.0;
                                    }
                                }
                                if kind == "sub" {
                                    menu_heading(ui, "Search");
                                    let s = &mut p.search;
                                    if block_button(ui, if s.open { "Hide search" } else { "Search subtitles" }).clicked() {
                                        s.open = !s.open;
                                        search_req = s.open && s.results.is_empty() && !s.busy;
                                    }
                                    if s.open {
                                        ui.add(egui::TextEdit::singleline(&mut s.filter).hint_text("Filter by language or addon").desired_width(f32::INFINITY).margin(vec2(8.0, 6.0))).accessible_name("Filter subtitles");
                                        let hits = filter_subs(&s.results, &s.filter);
                                        let note = if s.busy { "Searching..." } else if hits.is_empty() { "No subtitles found" } else { "" };
                                        if !note.is_empty() {
                                            caption(ui, note);
                                        }
                                        egui::ScrollArea::vertical().id_salt("sub_search").max_height(200.0).show(ui, |ui| {
                                            // shortcut: first 100 matches only; type a filter to narrow.
                                            for f in hits.into_iter().take(100) {
                                                ui.horizontal(|ui| {
                                                    if small_button(ui, "Save").on_hover_text("Save next to the video or in downloads").clicked() {
                                                        sub_act = Some((true, f.clone()));
                                                    }
                                                    if menu_row(ui, &format!("{} · {}", f.lang, f.addon), false).clicked() {
                                                        sub_act = Some((false, f.clone()));
                                                    }
                                                });
                                            }
                                        });
                                        if !s.status.is_empty() {
                                            caption(ui, &s.status);
                                        }
                                    }
                                    menu_heading(ui, "Style");
                                    style.scale = (style.scale * 10.0).round() / 10.0 + stepper(ui, "Size", &format!("{:.1}x", style.scale)) as f64 * 0.1;
                                    swatch_row(ui, "Color", &SUB_COLORS, &mut style.color);
                                    swatch_row(ui, "Outline", &SUB_OUTLINES, &mut style.outline);
                                    style.pos += stepper(ui, "Position", &format!("{}%", style.pos)) as f64 * 5.0;
                                    switch_row(ui, "Background box", &mut style.background);
                                    style.delay += stepper(ui, "Delay", &format!("{:+.1}s", style.delay)) as f64 * 0.1;
                                    if style.delay != 0.0 && link_button(ui, "Reset delay").clicked() {
                                        style.delay = 0.0;
                                    }
                                }
                                let (i, lang) = if kind == "audio" { (0, &settings::ALANG) } else { (1, &settings::SLANG) };
                                menu_heading(ui, "Preferred languages");
                                let valid = valid_langs(&self.langs[i]);
                                let r = ui.add(egui::TextEdit::singleline(&mut self.langs[i]).hint_text("en,eng").desired_width(f32::INFINITY).margin(vec2(8.0, 6.0)));
                                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, format!("Preferred {kind} languages")));
                                if let Some(v) = &valid
                                    && r.changed()
                                    && let Err(e) = lang.set(&self.db, v)
                                {
                                    report(&mut self.error, format!("saving {}: {e:#}", lang.key));
                                }
                                // Typing a subtitle language preference turns subtitles back on after a remembered "Off".
                                if kind == "sub"
                                    && r.changed()
                                    && valid_langs(&self.langs[1]).is_some_and(|v| !v.is_empty())
                                    && let Err(e) = settings::SUB_OFF.set(&self.db, &Some(false))
                                {
                                    report(&mut self.error, format!("saving sub_off: {e:#}"));
                                }
                                if valid.is_none() {
                                    error_caption(ui, "Use codes like en,eng,pt-BR (max 8)");
                                }
                            } else {
                                menu_heading(ui, "Speed");
                                chip_row(ui, |ui| {
                                    for sp in SPEEDS {
                                        if pill(ui, &format!("{sp}x"), (st.speed - sp).abs() < 0.01, egui::Role::RadioButton).clicked() {
                                            cmd(&["set", "speed", &sp.to_string()]);
                                            // Also the starting speed of the next video, like the Default speed setting.
                                            self.default_speed = sp;
                                            if let Err(e) = settings::DEFAULT_SPEED.set(&self.db, &sp) {
                                                report(&mut self.error, format!("saving default_speed: {e:#}"));
                                            }
                                        }
                                    }
                                });
                                menu_heading(ui, &match self.sleep {
                                    Some((t, _)) => format!("Sleep timer ({} min left)", t.saturating_duration_since(Instant::now()).as_secs().div_ceil(60)),
                                    None => "Sleep timer".into(),
                                });
                                chip_row(ui, |ui| {
                                    if pill(ui, "Off", self.sleep.is_none(), egui::Role::RadioButton).clicked() {
                                        self.sleep = None;
                                    }
                                    for m in [15u64, 30, 60, 90] {
                                        if pill(ui, &format!("{m} min"), self.sleep.is_some_and(|(_, c)| c == m), egui::Role::RadioButton).clicked() {
                                            self.sleep = Some((Instant::now() + Duration::from_secs(m * 60), m));
                                        }
                                    }
                                });
                                menu_heading(ui, "Video");
                                chip_row(ui, |ui| {
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
                                if p.zoom != 0 && link_button(ui, "Reset zoom").clicked() {
                                    p.zoom = 0;
                                    cmd(&["set", "video-zoom", "0"]);
                                }
                                menu_heading(ui, "Loop");
                                chip_row(ui, |ui| {
                                    // A pill, not a switch row: the panel must stay short enough to clear the seek bar at 700 px.
                                    if pill(ui, "Repeat", p.repeat, egui::Role::CheckBox).clicked() {
                                        p.repeat = !p.repeat;
                                        cmd(&["set", "loop-file", if p.repeat { "inf" } else { "no" }]);
                                    }
                                    for (i, (name, prop)) in [("A", "ab-loop-a"), ("B", "ab-loop-b")].into_iter().enumerate() {
                                        let label = p.ab[i].map_or(format!("Set {name}"), |t| format!("{name} {t:.0}s"));
                                        if pill(ui, &label, p.ab[i].is_some(), egui::Role::Button).clicked() {
                                            cmd(&["set", prop, &st.pos.to_string()]);
                                            // Keep A before B so the labels match the loop mpv plays.
                                            if set_ab(&mut p.ab, i, st.pos) {
                                                cmd(&["set", "ab-loop-a", &p.ab[0].unwrap_or_default().to_string()]);
                                                cmd(&["set", "ab-loop-b", &p.ab[1].unwrap_or_default().to_string()]);
                                            }
                                        }
                                    }
                                    if pill(ui, "Clear", false, egui::Role::Button).clicked() {
                                        p.ab = [None; 2];
                                        cmd(&["set", "ab-loop-a", "no"]);
                                        cmd(&["set", "ab-loop-b", "no"]);
                                    }
                                });
                                menu_heading(ui, "Advanced");
                                if switch_row(ui, "Hardware decoding", &mut self.hwdec).changed() {
                                    cmd(&["set", "hwdec", hwdec_value(self.hwdec)]);
                                    if let Err(e) = settings::HWDEC.set(&self.db, &self.hwdec) {
                                        report(&mut self.error, format!("saving hwdec: {e:#}"));
                                    }
                                }
                                switch_row(ui, "Playback stats (I)", &mut p.stats);
                                ui.add_space(8.0);
                                external = block_button(ui, "Open in external player").on_hover_text(format!("Pause here and open in: {}", self.external_player)).clicked();
                            }
                        });
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
                egui::Frame::new().fill(surface(230)).corner_radius(8).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
                    for (k, v) in rows {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{k:<8}")).size(13.0).monospace().color(Color32::from_gray(150)));
                            ui.label(RichText::new(v).size(13.0).monospace().color(Color32::WHITE));
                        });
                    }
                });
            });
        }
        let mut card_shown = false;
        // Up next: in an episode's last 30 s, a card slides in from the right to play the next one.
        if let Some((series, label, img)) = up_next {
            // Needs a duration over 60 s so a short or still-loading stream doesn't show the card at once.
            let open = !p.next_dismissed && !looping(p.repeat, p.ab) && rect.width() >= 362.0 && st.dur > 60.0 && st.dur - st.pos <= 30.0;
            let t = ctx.animate_bool_with_time(ui.scope_id().with("up_next"), open, 0.4);
            card_shown = t > 0.0;
            if t > 0.0 {
                let ease = 1.0 - (1.0 - t).powi(3);
                let size = vec2(480.0f32.min(rect.width() - 64.0).max(330.0), 160.0);
                let card = egui::Rect::from_min_size(pos2(rect.right() - (size.x + 32.0) * ease, rect.bottom() - 130.0 - size.y), size);
                let paint = ui.painter().clone();
                // Swallows clicks on the card so they don't pause the video; the buttons, registered later, win.
                let _ = ui.interact(card, ui.scope_id().with("up_next_card"), Sense::CLICK);
                paint.rect_filled(card, 12, surface(255));
                // The buttons need 316 px; the still takes the rest (14 px at the narrowest).
                let thumb = egui::Rect::from_min_size(card.min, vec2((size.x - 316.0).clamp(0.0, size.y * 1.05), size.y));
                let left = egui::CornerRadius { nw: 12, sw: 12, ne: 0, se: 0 };
                paint.rect_filled(thumb, left, Color32::from_gray(40));
                if let (true, Some(t)) = (thumb.width() > 0.0, img) {
                    egui::Image::from_texture(t).corner_radius(left).uv(cover_uv(thumb, t.size)).paint_at(ui, thumb);
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
        // Skip button while the playhead is in an addon-marked, chapter-named or TheIntroDB intro/recap/credits span.
        // Credits of a series episode with a next one get a Next episode button instead, hidden while the up-next card shows, after it's dismissed, or while looping.
        if p.skip_done.is_some_and(|e| st.pos >= e) {
            p.skip_done = None;
        }
        let spans = p.skips.iter().copied().chain(chapter_skips(&st.chapters, st.dur)).chain(p.db_skips.iter().flatten().copied());
        if let Some(sk) = spans.filter(|s| !(has_next && s.kind == SkipKind::Credits && (card_shown || p.next_dismissed || looping(p.repeat, p.ab))) && p.skip_done != Some(s.end)).find(|s| s.start <= st.pos && st.pos < s.end)
            && rect.width() >= 250.0
        {
            let to_next = has_next && sk.kind == SkipKind::Credits;
            let label = if to_next { "Next episode" } else { sk.kind.label() };
            let r = egui::Rect::from_min_size(pos2(rect.right() - 32.0 - 200.0, rect.bottom() - 130.0 - 50.0), vec2(200.0, 50.0));
            let paint = ui.painter().clone();
            let resp = ui.interact(r, ui.scope_id().with("skip"), Sense::CLICK).on_hover_cursor(egui::CursorIcon::PointingHand);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
            paint.rect_filled(r, 25, surface(255));
            paint.rect_stroke(r, 25, egui::Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Inside);
            if resp.hovered() {
                paint.rect_filled(r, 25, Color32::from_white_alpha(20));
            }
            paint.text(r.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(17.0), Color32::WHITE);
            if resp.clicked() && to_next {
                next = true;
            } else if resp.clicked() {
                seek = Some((sk.end, false));
                p.skip_done = Some(sk.end);
            }
        }
        let style = style.clamped();
        if style != self.sub_style {
            self.sub_style = style;
            if let Err(e) = settings::SUB_STYLE.set(&self.db, &style) {
                report(&mut self.error, e);
            }
            let _ = p.player.apply_sub_style(&style);
        }
        p.volume = p.volume.clamp(0.0, VOLUME_MAX);
        if p.volume != old_vol {
            cmd(&["set", "volume", &p.volume.to_string()]);
            if st.muted {
                cmd(&["set", "mute", "no"]);
            }
        }
        // Saved on release: a write per step of a drag would stall the UI thread.
        if !vol_held && p.volume != self.volume {
            self.volume = p.volume;
            if let Err(e) = settings::VOLUME.set(&self.db, &p.volume) {
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
            if let Err(e) = p.player.command(&args) {
                report(&mut self.error, e);
            }
        }
        let (key, source, title) = (p.key.clone(), p.source.clone(), p.title.clone());
        if let Some((save, f)) = sub_act {
            if save {
                // Beside a local or downloaded video, else in the downloads folder.
                let src = std::path::Path::new(&source);
                let dir = src.is_file().then(|| src.parent().map(PathBuf::from)).flatten().unwrap_or_else(|| self.queue.dir().to_path_buf());
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
        } else if next {
            self.play_next(&ctx);
        }
    }
}
