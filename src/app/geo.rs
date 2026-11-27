//! Window size and position, restored on launch.

use eframe::egui;
use stremflix_rs::settings::Setting;

/// Window size and position at the last exit.
pub(crate) const WINDOW: Setting<Geo> = Setting::new("window", Geo::default, |s| s.parse().ok(), Geo::to_string);

/// Default is a maximized 1000x700 window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geo {
    pub(crate) size: [f32; 2],
    pub(crate) pos: Option<[f32; 2]>,
    pub(crate) maximized: bool,
}

impl Default for Geo {
    fn default() -> Self {
        Self { size: [1000.0, 700.0], pos: None, maximized: true }
    }
}

impl std::fmt::Display for Geo {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{},{},{}", self.size[0], self.size[1], u8::from(self.maximized))?;
        self.pos.map_or(Ok(()), |p| write!(f, ",{},{}", p[0], p[1]))
    }
}

impl std::str::FromStr for Geo {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        let n: Vec<f32> = s.split(',').map(|v| v.parse().map_err(|_| ())).collect::<Result<_, _>>()?;
        if !n.iter().all(|v| v.is_finite()) || !matches!(n.len(), 3 | 5) || !(200.0..=16384.0).contains(&n[0]) || !(150.0..=16384.0).contains(&n[1]) {
            return Err(());
        }
        // Off-screen positions (e.g. Windows' -32000 minimized spot) drop the position, keep the size.
        let pos = (n.len() == 5 && n[3].abs() <= 10000.0 && n[4].abs() <= 10000.0).then(|| [n[3], n[4]]);
        Ok(Self { size: [n[0], n[1]], pos, maximized: n[2] != 0.0 })
    }
}

impl Geo {
    /// Fullscreen is transient and a maximized rect is not the restore size, so those keep the last normal rect.
    pub(crate) fn track(&mut self, vp: &egui::ViewportInfo) {
        if vp.minimized == Some(true) || vp.fullscreen == Some(true) {
            return;
        }
        // Unknown (None) keeps the last known state.
        if let Some(m) = vp.maximized {
            self.maximized = m;
        }
        if self.maximized {
            return;
        }
        // Clamp to the ranges from_str accepts, or one bad field would discard the whole record on next launch.
        if let Some(r) = vp.inner_rect {
            self.size = [r.width().clamp(200.0, 16384.0), r.height().clamp(150.0, 16384.0)];
        }
        // shortcut: Wayland reports no outer position, so only size is restored there.
        let pos = vp.outer_rect.map(|r| [r.min.x, r.min.y]).filter(|p| p.iter().all(|v| v.abs() <= 10000.0));
        self.pos = pos.or(self.pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geo_round_trips_and_rejects_junk() {
        let g = Geo { size: [800.0, 600.0], pos: Some([-10.0, 20.0]), maximized: false };
        assert_eq!(g.to_string().parse(), Ok(g));
        assert_eq!(Geo::default().to_string().parse(), Ok(Geo::default()));
        assert!("1,2,3".parse::<Geo>().is_err() && "x".parse::<Geo>().is_err() && "800,600,0,1".parse::<Geo>().is_err());
        assert!("20000,600,0".parse::<Geo>().is_err() && "800,20000,0".parse::<Geo>().is_err());
        assert_eq!("800,600,0,-32000,-32000".parse(), Ok(Geo { size: [800.0, 600.0], pos: None, maximized: false }));
    }

    #[test]
    fn geo_track_keeps_state_and_stays_parseable() {
        let r = |w, h| egui::Rect::from_min_size(egui::pos2(50_000.0, 5.0), egui::vec2(w, h));
        let vp = |maximized, fullscreen, w, h| egui::ViewportInfo { maximized, fullscreen, inner_rect: Some(r(w, h)), outer_rect: Some(r(w, h)), ..Default::default() };
        let mut g = Geo { maximized: true, ..Default::default() };
        g.track(&vp(None, Some(true), 1920.0, 1080.0));
        g.track(&vp(None, None, 1920.0, 1080.0));
        assert!(g.maximized && g.size == [1000.0, 700.0]);
        g.track(&vp(Some(false), None, 10.0, 99999.0));
        assert!(!g.maximized && g.size == [200.0, 16384.0] && g.pos.is_none());
        assert_eq!(g.to_string().parse(), Ok(g));
    }
}
