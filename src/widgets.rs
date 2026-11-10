//! Reusable egui widgets and the app's theme. Nothing here knows about the app's state or
//! addon types: screens in `main.rs` compose these and own what they mean.

use eframe::egui;
use egui::{Align, Color32, Layout, RichText, Sense, pos2, vec2};
use std::sync::Arc;

pub const RED: Color32 = Color32::from_rgb(229, 9, 20);
/// Validation and failure text.
pub const ERROR: Color32 = Color32::from_rgb(230, 90, 90);
/// Selectable accent colors; the first is Stremio's purple.
pub const ACCENTS: [(&str, Color32); 4] = [
    ("Purple", Color32::from_rgb(123, 91, 245)),
    ("Blue", Color32::from_rgb(37, 120, 235)),
    ("Green", Color32::from_rgb(30, 160, 90)),
    ("Orange", Color32::from_rgb(230, 120, 20)),
];
pub static ACCENT_IDX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub fn accent() -> Color32 {
    ACCENTS[ACCENT_IDX.load(std::sync::atomic::Ordering::Relaxed)].1
}
/// Index of the saved accent name; unknown or missing falls back to the default.
pub fn accent_index(name: Option<&str>) -> usize {
    ACCENTS.iter().position(|a| Some(a.0) == name).unwrap_or(0)
}
/// Strong text color at `alpha`: a hover/fill wash that reads on both themes.
pub fn tint(ui: &egui::Ui, alpha: u8) -> Color32 {
    let c = ui.visuals().strong_text_color();
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}
/// Dark background of the player's floating panels and buttons.
pub fn surface(alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(24, 24, 28, alpha)
}
/// Applies the theme and accent to egui; the player menu keeps the dark visuals.
pub fn apply_theme(ctx: &egui::Context, light: bool) {
    for (theme, is_light) in [(egui::Theme::Dark, false), (egui::Theme::Light, true)] {
        let mut v = if is_light { egui::Visuals::light() } else { egui::Visuals::dark() };
        v.panel_fill = if is_light { Color32::from_gray(245) } else { Color32::from_rgb(20, 20, 20) };
        v.window_fill = v.panel_fill;
        v.extreme_bg_color = if is_light { Color32::WHITE } else { Color32::from_gray(32) };
        v.selection.bg_fill = accent();
        if !is_light {
            // Keep the old dark palette: egui's defaults are dimmer than the grays used before theming.
            v.widgets.noninteractive.fg_stroke.color = Color32::from_gray(200);
            v.weak_text_color = Some(Color32::from_gray(170));
        } else {
            v.weak_text_color = Some(Color32::from_gray(95));
        }
        // Netflix-style flat buttons: no outline or grow on hover, gray that dims when hovered.
        let w = &mut v.widgets;
        let grays = if is_light { [215, 200, 185, 200] } else { [82, 56, 45, 56] };
        for (st, gray) in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open].into_iter().zip(grays) {
            (st.weak_bg_fill, st.bg_stroke, st.corner_radius, st.expansion) = (Color32::from_gray(gray), egui::Stroke::NONE, 4.into(), 0.0);
            st.fg_stroke.color = if is_light { Color32::BLACK } else { Color32::WHITE };
        }
        ctx.set_visuals_of(theme, v);
        ctx.style_mut_of(theme, |s| s.spacing.button_padding = vec2(14.0, 6.0));
    }
    ctx.set_theme(if light { egui::Theme::Light } else { egui::Theme::Dark });
}

/// Phosphor Fill weight (same codepoints as Regular); used for play and pause.
pub fn fill() -> egui::FontFamily {
    egui::FontFamily::Name("Phosphor-Fill".into())
}

/// Phosphor Regular alone. Inter has its own glyphs at some Phosphor codepoints (e.g. ARROW_LEFT), so icons must not go through Inter.
pub fn phosphor() -> egui::FontFamily {
    egui::FontFamily::Name("Phosphor".into())
}

/// Bold weight of the UI font (egui's `strong()` only brightens the color).
pub fn bold() -> egui::FontFamily {
    egui::FontFamily::Name("bold".into())
}

/// Phosphor Regular codepoints used by the UI.
pub mod icons {
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
    pub const WAVEFORM: &str = "\u{E802}";
    pub const GEAR: &str = "\u{E270}";
    pub const X: &str = "\u{E4F6}";
}

/// Inter (OFL) as the UI font: the closest free match to Netflix Sans, which is proprietary.
/// egui's default fonts stay as fallbacks for symbols and emoji.
pub fn install_fonts(ctx: &egui::Context) {
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

// Text

/// Bold heading text in the strong color.
pub fn heading_text(ui: &egui::Ui, text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).size(size).strong().family(bold()).color(ui.visuals().strong_text_color())
}

/// Heading of a page or of a title's Streams/Episodes block.
pub fn page_heading(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    let t = heading_text(ui, text, 24.0);
    ui.label(t)
}

/// Heading of a section; see `row_title` for one that also sets the space around it.
pub fn section_heading(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    let t = heading_text(ui, text, 20.0);
    ui.label(t)
}

/// Section heading spaced apart from what comes before it.
pub fn row_title(ui: &mut egui::Ui, title: &str) {
    ui.add_space(22.0);
    section_heading(ui, title);
    ui.add_space(4.0);
}

/// Secondary line of explanation or status.
pub fn caption(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    let c = ui.visuals().weak_text_color();
    ui.label(RichText::new(text).size(13.0).color(c))
}

/// Validation or failure message at caption size.
pub fn error_caption(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    ui.label(RichText::new(text).size(13.0).color(ERROR))
}

/// Body-sized text in the weak color, for empty states and statuses.
pub fn muted(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    let c = ui.visuals().weak_text_color();
    ui.label(RichText::new(text).color(c))
}

/// Label of a control row.
pub fn field_label(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let c = ui.visuals().text_color();
    ui.label(RichText::new(text).size(15.0).color(c))
}

// Buttons

/// Keyboard-focus outline for the custom-painted controls.
fn focus_ring(ui: &egui::Ui, r: &egui::Response, radius: u8) {
    if r.has_focus() {
        ui.painter().rect_stroke(r.rect, radius, egui::Stroke::new(2.0, accent()), egui::StrokeKind::Inside);
    }
}

/// Size of the hero banner's buttons.
const HERO_BUTTON: egui::Vec2 = egui::Vec2::new(130.0, 46.0);

/// Sets the fill of the button drawn next in `ui`: `fill` = [normal, hovered], no outline.
pub fn pill_style(ui: &mut egui::Ui, fill: [Color32; 2], fg: Color32) {
    let w = &mut ui.visuals_mut().widgets;
    for (st, f) in [(&mut w.inactive, fill[0]), (&mut w.hovered, fill[1]), (&mut w.active, fill[1])] {
        (st.weak_bg_fill, st.bg_stroke, st.fg_stroke.color) = (f, egui::Stroke::NONE, fg);
    }
}

/// Fully rounded button; takes its colors from `pill_style`.
fn pill_widget<'a>(text: &str, size: f32, min: egui::Vec2) -> egui::Button<'a> {
    let rt = |s: &str, family| RichText::new(s).size(size).strong().family(family);
    // A leading icon ("<icon>  Label") is drawn in Phosphor, not Inter; see `phosphor`.
    let b = match text.split_once("  ") {
        Some((i, t)) if !i.is_empty() && i.chars().all(|c| ('\u{E000}'..='\u{F8FF}').contains(&c)) => egui::Button::new((rt(i, phosphor()), rt(t, bold()))),
        _ => egui::Button::new(rt(text, bold())),
    };
    b.min_size(min).corner_radius((min.y / 2.0) as u8)
}

/// A hero-sized pill to build other controls from, such as a menu button.
pub fn hero_pill<'a>(text: &str) -> egui::Button<'a> {
    pill_widget(text, 18.0, HERO_BUTTON)
}

fn pill_button(ui: &mut egui::Ui, text: &str, size: f32, min: egui::Vec2, fill: [Color32; 2], fg: Color32) -> egui::Response {
    ui.scope(|ui| {
        pill_style(ui, fill, fg);
        ui.add(pill_widget(text, size, min))
    })
    .inner
}

/// White primary pill (Netflix "Play") and translucent gray secondary pill.
pub fn white_fill() -> [Color32; 2] {
    [Color32::WHITE, Color32::from_white_alpha(191)]
}
pub fn gray_fill() -> [Color32; 2] {
    [Color32::from_rgba_unmultiplied(109, 109, 110, 179), Color32::from_rgba_unmultiplied(109, 109, 110, 102)]
}

pub fn hero_button(ui: &mut egui::Ui, text: &str, fill: [Color32; 2], text_color: Color32) -> egui::Response {
    pill_button(ui, text, 18.0, HERO_BUTTON, fill, text_color)
}

/// Small secondary pill for toolbar actions (Uninstall, Clear, Stop …).
pub fn small_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let (fill, fg) = if ui.visuals().dark_mode { (gray_fill(), Color32::WHITE) } else { ([tint(ui, 34), tint(ui, 20)], ui.visuals().strong_text_color()) };
    pill_button(ui, text, 14.0, vec2(0.0, 32.0), fill, fg)
}

/// Small white pill for the main action in a row (Install, Play …).
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    // A white pill vanishes on the light panel.
    let (fill, fg) = if ui.visuals().dark_mode { (white_fill(), Color32::BLACK) } else { ([Color32::BLACK, Color32::from_gray(60)], Color32::WHITE) };
    pill_button(ui, text, 14.0, vec2(0.0, 32.0), fill, fg)
}

/// Frameless accent-colored text button (Reset delay …).
pub fn link_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(13.0).color(accent())).frame(false))
}

/// Full-width button inside a player popover.
pub fn block_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let min = vec2(ui.available_width(), 34.0);
    ui.add(egui::Button::new(RichText::new(text).size(14.0).color(Color32::WHITE)).fill(Color32::from_white_alpha(16)).corner_radius(8).min_size(min))
}

/// Rounded chip, accent-filled when selected.
pub fn pill(ui: &mut egui::Ui, label: &str, selected: bool, role: egui::Role) -> egui::Response {
    let fg = if selected { Color32::WHITE } else { ui.visuals().strong_text_color() };
    let g = ui.painter().layout_no_wrap(label.to_string(), egui::FontId::new(14.0, if selected { bold() } else { egui::FontFamily::Proportional }), fg);
    let (rect, r) = ui.allocate_exact_size(vec2(g.size().x + 24.0, 30.0), Sense::click());
    r.widget_info(|| egui::WidgetInfo::selected(role, true, selected, label));
    let fill = if selected { accent() } else { tint(ui, if r.hovered() { 30 } else { 16 }) };
    ui.painter().rect_filled(rect, 15, fill);
    focus_ring(ui, &r, 15);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Wrapping line of `pill`s.
pub fn chip_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        add(ui);
    });
}

/// Color dot with an accent ring when selected.
pub fn swatch(ui: &mut egui::Ui, name: &str, color: Color32, selected: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, name));
    focus_ring(ui, &r, 15);
    if selected || r.hovered() {
        ui.painter().circle_stroke(rect.center(), 13.0, egui::Stroke::new(2.0, if selected { accent() } else { Color32::from_gray(120) }));
    }
    ui.painter().circle_filled(rect.center(), 9.0, color);
    r.on_hover_text(name).on_hover_cursor(egui::CursorIcon::PointingHand)
}

// Rows

/// Label on the left, a color dot per `(name, hex)` choice on the right.
pub fn swatch_row(ui: &mut egui::Ui, label: &str, choices: &[(&str, &str)], selected: &mut usize) {
    ui.horizontal(|ui| {
        ui.set_min_height(30.0); // swatch height: egui centers the label on the row height so far
        field_label(ui, label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for (i, (name, hex)) in choices.iter().enumerate().rev() {
                if swatch(ui, name, Color32::from_hex(hex).unwrap_or(Color32::WHITE), *selected == i).clicked() {
                    *selected = i;
                }
            }
        });
    });
}

/// Label on the left, `− value +` on the right; returns -1, 0 or 1.
pub fn stepper(ui: &mut egui::Ui, label: &str, value: &str) -> i32 {
    let mut step = 0;
    ui.horizontal(|ui| {
        ui.set_min_height(36.0);
        field_label(ui, label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if pill(ui, "+", false, egui::Role::Button).on_hover_text(format!("Increase {label}")).clicked() {
                step = 1;
            }
            ui.add_sized([56.0, 30.0], egui::Label::new(RichText::new(value).size(14.0).color(ui.visuals().strong_text_color())));
            if pill(ui, "−", false, egui::Role::Button).on_hover_text(format!("Decrease {label}")).clicked() {
                step = -1;
            }
        });
    });
    step
}

/// Full-width row with a label and an on/off switch.
pub fn switch_row(ui: &mut egui::Ui, label: &str, on: &mut bool) -> egui::Response {
    let (rect, mut r) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    if r.clicked() {
        *on = !*on;
        r.mark_changed();
    }
    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, *on, label));
    focus_ring(ui, &r, 6);
    let p = ui.painter();
    if r.hovered() {
        p.rect_filled(rect, 6, tint(ui, 14));
    }
    p.text(pos2(rect.left() + 8.0, rect.center().y), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(15.0), ui.visuals().text_color());
    let t = ui.ctx().animate_bool_with_time(r.id, *on, 0.12);
    let track = egui::Rect::from_center_size(pos2(rect.right() - 26.0, rect.center().y), vec2(36.0, 20.0));
    p.rect_filled(track, 10, Color32::from_gray(80).lerp_to_gamma(accent(), t));
    p.circle_filled(pos2(egui::lerp(track.left() + 10.0..=track.right() - 10.0, t), track.center().y), 7.0, Color32::WHITE);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Settings column width: fields of consecutive rows start at the same x.
const LABEL_WIDTH: f32 = 150.0;

/// A label and whatever `add` puts after it; `add` gets the label's id for `labelled_by`.
pub fn field_row<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui, egui::Id) -> R) -> R {
    ui.horizontal(|ui| {
        ui.set_min_height(32.0);
        let l = ui.label(label);
        ui.add_space((LABEL_WIDTH - l.rect.width()).max(0.0));
        add(ui, l.id)
    })
    .inner
}

/// Labeled dropdown over `(name, value)` choices; a value not listed shows as "Custom". True when one is picked.
pub fn combo_row<T: Clone + PartialEq>(ui: &mut egui::Ui, label: &str, value: &mut T, choices: &[(&str, T)]) -> bool {
    field_row(ui, label, |ui, _| {
        let shown = choices.iter().find(|c| c.1 == *value).map_or("Custom", |c| c.0);
        let mut changed = false;
        dropdown(ui, label, shown, |ui| {
            for (name, v) in choices {
                changed |= ui.selectable_value(value, v.clone(), *name).changed();
            }
        });
        changed
    })
}

/// The app's dropdown: a field-styled button with a chevron over a rounded, shadowed menu
/// whose picked row is an accent wash instead of a solid fill.
pub fn dropdown<'a, R>(
    ui: &mut egui::Ui,
    id_salt: impl egui::AsIdSalt,
    text: impl egui::IntoAtoms<'a>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        let (field, edge, edge_hover) = if v.dark_mode { (v.extreme_bg_color, Color32::from_gray(60), Color32::from_gray(110)) } else { (v.extreme_bg_color, Color32::from_gray(205), Color32::from_gray(150)) };
        let w = &mut v.widgets;
        for (st, edge) in [(&mut w.inactive, edge), (&mut w.hovered, edge_hover), (&mut w.active, edge_hover), (&mut w.open, accent())] {
            (st.weak_bg_fill, st.bg_stroke, st.corner_radius) = (field, egui::Stroke::new(1.0, edge), 6.into());
        }
        egui::ComboBox::from_id_salt(id_salt)
            .selected_text(text)
            .icon(|ui, rect, vis, open| {
                // Thin chevron that flips up while the menu is open.
                let (c, s) = (rect.center(), rect.height().min(rect.width()) * 0.3);
                let dy = if open { -s / 2.0 } else { s / 2.0 };
                let tip = pos2(c.x, c.y + dy);
                let stroke = egui::Stroke::new(1.5, vis.fg_stroke.color);
                ui.painter().line(vec![pos2(c.x - s, c.y - dy), tip, pos2(c.x + s, c.y - dy)], stroke);
            })
            .popup_style(egui::style::StyleModifier::from(|s: &mut egui::Style| {
                let dark = s.visuals.dark_mode;
                let v = &mut s.visuals;
                v.window_fill = if dark { Color32::from_gray(30) } else { Color32::WHITE };
                v.window_stroke = egui::Stroke::new(1.0, Color32::from_gray(if dark { 52 } else { 215 }));
                v.menu_corner_radius = 10.into();
                v.popup_shadow = egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(if dark { 140 } else { 45 }) };
                let a = accent();
                v.selection.bg_fill = Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), if dark { 100 } else { 55 });
                v.selection.stroke.color = v.strong_text_color();
                let hover = if dark { Color32::from_gray(48) } else { Color32::from_gray(236) };
                for st in [&mut v.widgets.hovered, &mut v.widgets.active] {
                    st.weak_bg_fill = hover;
                }
                for st in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
                    st.corner_radius = 6.into();
                }
                s.spacing.menu_margin = egui::Margin::same(6);
                s.spacing.button_padding = vec2(12.0, 7.0);
                s.spacing.item_spacing.y = 2.0;
            }))
            .show_ui(ui, add)
    })
    .inner
}

/// Labeled text field and its action button on one centered line.
/// True when the button is pressed, or Enter in the field, with non-blank text (any text if `allow_blank`).
/// `tip`: hover text of the button.
pub fn form_row(ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str, action: &str, tip: Option<&str>, allow_blank: bool) -> bool {
    field_row(ui, label, |ui, l| {
        let r = ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(400.0)).labelled_by(l);
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let b = primary_button(ui, action);
        let b = match tip {
            Some(t) => b.on_hover_text(t),
            None => b,
        };
        (b.clicked() || enter) && (allow_blank || !text.trim().is_empty())
    })
}

/// Message with a Dismiss button after it; true when dismissed.
pub fn dismissible(ui: &mut egui::Ui, message: impl Into<egui::WidgetText>) -> bool {
    ui.horizontal(|ui| {
        ui.label(message);
        small_button(ui, "Dismiss").clicked()
    })
    .inner
}

/// Small caps section heading inside a player popover.
pub fn menu_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(text.to_uppercase()).size(11.5).family(bold()).color(ui.visuals().weak_text_color()));
    ui.add_space(4.0);
}

/// Full-width popover row: accent check when selected, highlight on hover, long labels truncated.
pub fn menu_row(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, label));
    focus_ring(ui, &r, 6);
    let p = ui.painter();
    if r.hovered() {
        p.rect_filled(rect, 6, tint(ui, 14));
    }
    if selected {
        p.text(pos2(rect.left() + 14.0, rect.center().y), egui::Align2::LEFT_CENTER, icons::CHECK, egui::FontId::new(16.0, phosphor()), accent());
    }
    let font = egui::FontId::new(15.0, if selected { bold() } else { egui::FontFamily::Proportional });
    let color = if selected { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
    let mut job = egui::text::LayoutJob::simple_singleline(label.to_string(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 50.0);
    let g = p.layout_job(job);
    p.galley(pos2(rect.left() + 40.0, rect.center().y - g.size().y / 2.0), g, color);
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Netflix episode-list row: index, bold name, gray details, play circle; highlights on hover.
/// `reserve`: extra width kept free of text on the right (for an overlaid button).
pub fn list_row(ui: &mut egui::Ui, n: usize, head: String, details: String, reserve: f32) -> egui::Response {
    let (w, text_x, pad) = (ui.available_width().min(960.0), 84.0, 18.0);
    let wrap = (w - text_x - 96.0 - reserve).max(40.0);
    let (strong, weak, panel) = (ui.visuals().strong_text_color(), ui.visuals().weak_text_color(), ui.visuals().panel_fill);
    let p = ui.painter();
    let hg = p.layout(head.clone(), egui::FontId::new(17.0, bold()), strong, wrap);
    let has_details = !details.is_empty();
    let mut job = egui::text::LayoutJob::simple(details, egui::FontId::proportional(14.0), weak, wrap);
    job.sections[0].format.line_height = Some(20.0);
    let dg = p.layout_job(job);
    let block = hg.size().y + if has_details { 8.0 + dg.size().y } else { 0.0 };
    let h = (pad * 2.0 + block).max(88.0);
    let (rect, r) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Play {head}")));
    let t = ui.ctx().animate_bool(r.id, r.hovered() || r.has_focus());
    // Hover wash: strong text at alpha 34 is the old gray 51 on the dark panel.
    let hover = tint(ui, 34);
    let p = ui.painter();
    // Inset so neighbouring rows' highlights never touch.
    p.rect_filled(rect.shrink2(vec2(4.0, 4.0)), 8, hover.gamma_multiply(t));
    let num = p.layout_no_wrap(n.to_string(), egui::FontId::proportional(24.0), strong);
    p.galley(pos2(rect.left() + text_x / 2.0 - num.size().x / 2.0, rect.center().y - num.size().y / 2.0), num, strong);
    let top = rect.center().y - block / 2.0;
    p.galley(pos2(rect.left() + text_x, top), hg.clone(), strong);
    p.galley(pos2(rect.left() + text_x, top + hg.size().y + 8.0), dg, weak);
    // Play button: outlined circle that fills with strong text on hover.
    let c = pos2(rect.right() - 48.0, rect.center().y);
    p.circle(c, 20.0, strong.gamma_multiply(t), (2.0, strong));
    let tri = strong.lerp_to_gamma(panel, t);
    p.add(egui::Shape::convex_polygon(vec![c + vec2(-6.0, -9.0), c + vec2(10.0, 0.0), c + vec2(-6.0, 9.0)], tri, egui::Stroke::NONE));
    r
}

// Cards

pub const CARD: egui::Vec2 = egui::Vec2::new(150.0, 225.0);
/// A hovered or focused card's scale.
pub const GROW: f32 = 1.06;

/// Netflix-style card: no border, grows on hover/focus, shrinks while pressed.
/// Returns the click response and the animated rect to paint into.
pub fn card(ui: &mut egui::Ui, size: egui::Vec2, label: &str) -> (egui::Response, egui::Rect) {
    // Reserve room for the grown card so the scroll area doesn't clip it.
    let (slot, r) = ui.allocate_exact_size(size * GROW, Sense::click());
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
    let target = if r.is_pointer_button_down_on() { 0.96 } else if r.contains_pointer() || r.has_focus() { GROW } else { 1.0 };
    let s = ui.ctx().animate_value_with_time(r.id, target, 0.15);
    (r, egui::Rect::from_center_size(slot.center(), size * s))
}

/// Round play button in the middle of a card, shown while the card is hovered or the button focused.
/// Registered after the card, so it takes the click.
pub fn card_play(ui: &mut egui::Ui, card: &egui::Response, rect: egui::Rect, label: &str) -> bool {
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
    r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label).clicked()
}

/// Gray card with centered, wrapped text (used when there's no poster).
pub fn card_text(ui: &egui::Ui, rect: egui::Rect, text: &str) {
    // Clipped: a very long title would otherwise spill past the card.
    let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    p.rect_filled(rect, 6, Color32::from_gray(40));
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), egui::FontId::proportional(15.0), Color32::WHITE, rect.width() - 16.0);
    job.halign = Align::Center; // each line centered around x = 0
    let g = p.layout_job(job);
    p.galley(pos2(rect.center().x, rect.center().y - g.size().y / 2.0), g, Color32::WHITE);
}

/// Draw `text` as a QR code of egui rects (no image files); dark on white so phones scan it in any theme.
pub fn qr_ui(ui: &mut egui::Ui, text: &str) {
    let Ok(code) = qrcode::QrCode::new(text) else {
        muted(ui, "Link too long for a QR code");
        return;
    };
    let (w, quiet, px) = (code.width(), 2, 4.0);
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat((w + 2 * quiet) as f32 * px), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, Color32::WHITE);
    for (i, c) in code.to_colors().into_iter().enumerate() {
        if c == qrcode::Color::Dark {
            let p = rect.min + vec2(((i % w) + quiet) as f32, ((i / w) + quiet) as f32) * px;
            ui.painter().rect_filled(egui::Rect::from_min_size(p, egui::Vec2::splat(px)), 0.0, Color32::BLACK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_rows_line_up_and_unlisted_combo_value_survives() {
        let (ctx, mut xs, mut value) = (egui::Context::default(), Vec::new(), 7u32);
        ctx.run_ui(Default::default(), |ui| {
            xs.clear();
            for label in ["A:", "A longer label:"] {
                field_row(ui, label, |ui, _| xs.push(ui.cursor().left()));
            }
            assert!(!combo_row(ui, "Limit:", &mut value, &[("Off", 0)]));
        })
        .drop_without_applying_deltas();
        assert_eq!(xs[0], xs[1]);
        assert_eq!(value, 7);
    }

    #[test]
    fn open_dropdown_menu_gets_the_accent_wash() {
        let ctx = egui::Context::default();
        let (mut id, mut fill) = (None, None);
        for _ in 0..2 {
            ctx.run_ui(Default::default(), |ui| {
                let r = dropdown(ui, "d", "Pick", |ui| ui.visuals().selection.bg_fill);
                id = Some(r.response.id);
                fill = r.inner;
            })
            .drop_without_applying_deltas();
            egui::Popup::open_id(&ctx, id.unwrap().with("popup"));
        }
        let fill = fill.expect("menu should render once opened");
        assert!(fill.a() < 255 && fill != accent());
    }
}
