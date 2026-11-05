//! Tray icon with Show/Hide, Play/Pause and Quit. Linux speaks StatusNotifierItem over D-Bus
//! (ksni on zbus, no GTK); Windows and macOS use tray-icon. Clicks go to `on`, from any thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Toggle,
    PlayPause,
    Quit,
}

const ITEMS: [(Action, &str); 3] = [(Action::Toggle, "Show/Hide"), (Action::PlayPause, "Play/Pause"), (Action::Quit, "Quit")];
const SIZE: u32 = 64;

pub struct Tray {
    up: Arc<AtomicBool>,
    #[cfg(any(windows, target_os = "macos"))]
    _icon: tray_icon::TrayIcon,
}

impl Tray {
    /// A tray host shows the icon, so a hidden window can be brought back.
    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }
}

/// The app icon scaled to tray size, as RGBA.
fn pixels(png: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?;
    Some(img.resize_exact(SIZE, SIZE, image::imageops::FilterType::Triangle).into_rgba8().into_raw())
}

/// RGBA to the ARGB32 (big-endian) pixels StatusNotifierItem wants.
#[cfg(any(target_os = "linux", test))]
fn argb(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, a]| [a, r, g, b]).collect()
}

#[cfg(target_os = "linux")]
struct Sni {
    icon: Vec<u8>,
    on: Arc<dyn Fn(Action) + Send + Sync>,
    up: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl ksni::Tray for Sni {
    fn id(&self) -> String {
        "stremflix-rs".into()
    }
    fn title(&self) -> String {
        "Stremflix-rs".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![ksni::Icon { width: SIZE as i32, height: SIZE as i32, data: self.icon.clone() }]
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        (self.on)(Action::Toggle)
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        ITEMS.into_iter().map(|(a, label)| ksni::menu::StandardItem { label: label.into(), activate: Box::new(move |t: &mut Self| (t.on)(a)), ..Default::default() }.into()).collect()
    }
    fn watcher_online(&self) {
        self.up.store(true, Ordering::Relaxed);
    }
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.up.store(false, Ordering::Relaxed);
        true // keep running: the host may come back (e.g. a shell restart)
    }
}

/// Starts the tray in the background. Without D-Bus or a tray host it logs and stays down.
#[cfg(target_os = "linux")]
pub fn spawn(rt: &tokio::runtime::Runtime, png: &[u8], on: impl Fn(Action) + Send + Sync + 'static) -> Option<Tray> {
    use ksni::TrayMethods;
    let up = Arc::new(AtomicBool::new(false));
    let sni = Sni { icon: argb(&pixels(png).unwrap_or_default()), on: Arc::new(on), up: up.clone() };
    let flag = up.clone();
    rt.spawn(async move {
        match sni.spawn().await {
            Ok(_handle) => flag.store(true, Ordering::Relaxed), // the service runs on after the handle drops
            Err(e) => eprintln!("tray: {e}"),
        }
    });
    Some(Tray { up })
}

/// Builds the tray icon; call on the main thread once the event loop runs. A failure logs and stays down.
#[cfg(any(windows, target_os = "macos"))]
pub fn spawn(_rt: &tokio::runtime::Runtime, png: &[u8], on: impl Fn(Action) + Send + Sync + 'static) -> Option<Tray> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let menu = Menu::new();
    let mut ids = Vec::new();
    for (a, label) in ITEMS {
        let item = MenuItem::new(label, true, None);
        ids.push((item.id().clone(), a));
        if let Err(e) = menu.append(&item) {
            eprintln!("tray menu: {e}");
        }
    }
    let on = Arc::new(on);
    let on2 = on.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Some((_, a)) = ids.iter().find(|(id, _)| *id == e.id) {
            on(*a)
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |e| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            on2(Action::Toggle)
        }
    }));
    let mut b = TrayIconBuilder::new().with_menu(Box::new(menu)).with_menu_on_left_click(false).with_tooltip("Stremflix-rs");
    if let Some(icon) = pixels(png).and_then(|p| tray_icon::Icon::from_rgba(p, SIZE, SIZE).ok()) {
        b = b.with_icon(icon);
    }
    b.build().inspect_err(|e| eprintln!("tray: {e}")).ok().map(|icon| Tray { up: Arc::new(AtomicBool::new(true)), _icon: icon })
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub fn spawn(_rt: &tokio::runtime::Runtime, _png: &[u8], _on: impl Fn(Action) + Send + Sync + 'static) -> Option<Tray> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_tray_sized_argb() {
        assert_eq!(argb(&[1, 2, 3, 4, 5, 6, 7, 8]), [4, 1, 2, 3, 8, 5, 6, 7]);
        let px = pixels(include_bytes!("../assets/icon.png")).unwrap();
        assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
    }
}
