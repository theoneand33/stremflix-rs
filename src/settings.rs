//! Every app setting: its key in the settings table, its default, and how it is stored.
//! A missing or invalid stored value reads as the default.

use crate::db::Db;
use crate::player::{SPEEDS, SubStyle, VOLUME_MAX};
use crate::torrent::Profile;
use anyhow::Result;

/// One setting, or with `get_at`/`set_at` a family of them keyed `<key>:<suffix>`.
pub struct Setting<T> {
    pub key: &'static str,
    default: fn() -> T,
    decode: fn(&str) -> Option<T>,
    encode: fn(&T) -> String,
}

impl<T> Setting<T> {
    pub const fn new(key: &'static str, default: fn() -> T, decode: fn(&str) -> Option<T>, encode: fn(&T) -> String) -> Self {
        Self { key, default, decode, encode }
    }

    pub fn get(&self, db: &Db) -> Result<T> {
        self.read(db, self.key)
    }

    pub fn set(&self, db: &Db, v: &T) -> Result<()> {
        db.set_setting(self.key, &(self.encode)(v))
    }

    pub fn clear(&self, db: &Db) -> Result<()> {
        db.remove_setting(self.key)
    }

    pub fn get_at(&self, db: &Db, suffix: &str) -> Result<T> {
        self.read(db, &self.key_at(suffix))
    }

    pub fn set_at(&self, db: &Db, suffix: &str, v: &T) -> Result<()> {
        db.set_setting(&self.key_at(suffix), &(self.encode)(v))
    }

    /// The settings-table key of family member `suffix`.
    pub fn key_at(&self, suffix: &str) -> String {
        format!("{}:{suffix}", self.key)
    }

    /// Key and stored text of `v`, for writing several settings after a UI pass.
    pub fn entry(&self, v: &T) -> (&'static str, String) {
        (self.key, (self.encode)(v))
    }

    fn read(&self, db: &Db, key: &str) -> Result<T> {
        Ok(db.setting(key)?.as_deref().and_then(self.decode).unwrap_or_else(self.default))
    }
}

const fn flag(key: &'static str, default: fn() -> bool) -> Setting<bool> {
    let decode: fn(&str) -> Option<bool> = |s| match s {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    };
    Setting::new(key, default, decode, |b| if *b { "1" } else { "0" }.into())
}

const fn text(key: &'static str) -> Setting<String> {
    Setting::new(key, String::new, |s| Some(s.into()), String::clone)
}

/// Keys hold `|` and URLs, so a list is a JSON array.
const fn keys(key: &'static str) -> Setting<Vec<String>> {
    Setting::new(key, Vec::new, |s| serde_json::from_str(s).ok(), |v| serde_json::to_string(v).unwrap_or_default())
}

/// A number, or the default when it does not parse.
macro_rules! number {
    ($key:expr, $t:ty, $default:expr) => {
        Setting::<$t>::new($key, || $default, |s| s.parse().ok(), |v| v.to_string())
    };
}

pub const THEME_LIGHT: Setting<bool> = Setting::new("theme", || false, |s| Some(s == "light"), |l| if *l { "light" } else { "dark" }.into());
/// Accent color by name; the widgets decide what an unknown one shows.
pub const ACCENT: Setting<String> = text("accent");
pub const UI_SCALE: Setting<f32> = Setting::new("ui_scale", || 1.0, |s| s.parse().ok().map(clamp_scale), |v| v.to_string());
pub const SUB_STYLE: Setting<SubStyle> = Setting::new("sub_style", SubStyle::default, |s| Some(SubStyle::decode(s)), SubStyle::encode);
/// Videos start with subtitles off: `None` until chosen once (see [`subs_off`]).
pub const SUB_OFF: Setting<Option<bool>> = Setting::new("sub_off", || None, |s| Some(Some(!matches!(s, "0" | ""))), |v| if *v == Some(true) { "1" } else { "0" }.into());
/// Preferred audio / subtitle languages (mpv `alang` / `slang`), already validated.
pub const ALANG: Setting<String> = text("alang");
pub const SLANG: Setting<String> = text("slang");
pub const DEFAULT_SPEED: Setting<f64> = Setting::new("default_speed", || 1.0, |s| s.parse().ok().filter(|v| SPEEDS.contains(v)), |v| v.to_string());
pub const VOLUME: Setting<f64> = Setting::new("volume", || 100.0, |s| s.parse().ok().filter(|v| (0.0..=VOLUME_MAX).contains(v)), |v| v.to_string());
pub const SEEK_STEP_RANGE: std::ops::RangeInclusive<u32> = 1..=600;
/// Arrow-key seek in seconds, plain and with Shift.
pub const SEEK_STEP: Setting<u32> = Setting::new("seek_step", || 10, |s| s.parse().ok().filter(|v| SEEK_STEP_RANGE.contains(v)), |v| v.to_string());
pub const SEEK_STEP_SHIFT: Setting<u32> = Setting::new("seek_step_shift", || 30, |s| s.parse().ok().filter(|v| SEEK_STEP_RANGE.contains(v)), |v| v.to_string());
pub const HWDEC: Setting<bool> = flag("hwdec", || true);
pub const AUTOPLAY_NEXT: Setting<bool> = flag("autoplay_next", || true);
pub const INTRODB: Setting<bool> = flag("introdb", || true);
pub const NOTIFY_EPS: Setting<bool> = flag("notify_eps", || true);
pub const TRAY_ON_CLOSE: Setting<bool> = flag("tray_on_close", || false);
pub const PAUSE_ON_MINIMIZE: Setting<bool> = flag("pause_on_minimize", || false);
/// External player command, e.g. `vlc` or `mpv --fs`.
pub const EXTERNAL_PLAYER: Setting<String> = Setting::new("external_player", || "vlc".into(), |s| Some(s.into()), String::clone);
pub const USER_AGENT: Setting<String> = text("user_agent");
/// Torrent download folder; empty is inside the data folder.
pub const TORRENT_DIR: Setting<String> = text("torrent_dir");
/// Bytes of stopped torrents kept (0 = delete a torrent when its playback stops).
pub const TORRENT_CACHE: Setting<u64> = number!("torrent_cache", u64, 0);
pub const TORRENT_PROFILE: Setting<Profile> = Setting::new("torrent_profile", || Profile::Default, |s| Some(Profile::from_label(s)), |p| p.label().into());
/// Bytes/s; 0 is unlimited.
pub const TORRENT_DOWN_LIMIT: Setting<u32> = number!("torrent_down_limit", u32, 0);
pub const TORRENT_UP_LIMIT: Setting<u32> = number!("torrent_up_limit", u32, 0);
/// 0 is no limit of that kind.
pub const SEED_RATIO: Setting<f64> = number!("seed_ratio", f64, 0.0);
pub const SEED_SECS: Setting<u64> = number!("seed_secs", u64, 0);
/// Proxy type ("", "http", "socks5"), host:port, username, password; applied at startup.
pub const PROXY: [Setting<String>; 4] = [text("proxy_kind"), text("proxy_addr"), text("proxy_user"), text("proxy_pass")];
/// Custom Home row order and hidden rows (row keys).
pub const HOME_ORDER: Setting<Vec<String>> = keys("home_order");
pub const HOME_HIDDEN: Setting<Vec<String>> = keys("home_hidden");
/// Addon priority per content type, at `addon_order:<type>`: addon URLs, first = highest.
pub const ADDON_ORDER: Setting<Vec<String>> = keys("addon_order");
/// `<series id>:<episode id>` keys already notified, plus a `<series id>:` marker per series.
pub const NOTIFIED_EPS: Setting<Vec<String>> = keys("notified_eps");
pub const SEARCH_HISTORY: Setting<Vec<String>> = Setting::new("search_history", Vec::new, |s| Some(decode_history(s)), |h| h.join("\n"));
pub const LAST_FOLDER: Setting<String> = text("last_folder");
/// PIN of a child-safe profile; empty when it is not locked.
// shortcut: kept as typed in the profile's database, which anyone who can read it can also edit; hash it if the lock must resist that.
pub const CHILD_PIN: Setting<String> = text("child_pin");
/// Stremio account auth key; `None` when signed out.
pub const STREMIO_AUTH: Setting<Option<String>> = Setting::new("stremio_auth", || None, |s| Some(Some(s.to_owned()).filter(|k| !k.is_empty())), |k| k.clone().unwrap_or_default());
pub const STREMIO_EMAIL: Setting<String> = text("stremio_email");
/// At `series:<episode progress key>`: the series id, for history whose key holds only the video id.
pub const EPISODE_SERIES: Setting<Option<String>> = Setting::new("series", || None, |s| Some(Some(s.into())), |v| v.clone().unwrap_or_default());
/// At `binge:<series key>`: the binge group the series last played from; `None` if none.
pub const BINGE_GROUP: Setting<Option<String>> = Setting::new("binge", || None, |s| Some(Some(s.to_owned()).filter(|g| !g.is_empty())), |v| v.clone().unwrap_or_default());
/// At `bitfield:<series key>`: episodes imported as watched from Stremio, until applied as local marks.
pub const WATCHED_BITFIELD: Setting<Option<String>> = Setting::new("bitfield", || None, |s| Some(Some(s.into())), |v| v.clone().unwrap_or_default());

/// Settings an export carries. Not secrets (`stremio_auth`), local paths, or `external_player` (a command to run).
pub const EXPORTED: [&str; 9] = [AUTOPLAY_NEXT.key, HWDEC.key, SUB_STYLE.key, SUB_OFF.key, TORRENT_CACHE.key, TORRENT_PROFILE.key, DEFAULT_SPEED.key, ALANG.key, SLANG.key];

/// Whether videos start with subtitles off: as last chosen, else off on a fresh install and on with a legacy `slang`.
pub fn subs_off(db: &Db) -> Result<bool> {
    Ok(match SUB_OFF.get(db)? {
        Some(off) => off,
        None => SLANG.get(db)?.is_empty(),
    })
}

/// Bounds a UI zoom factor to 0.75-2.0 in 0.05 steps; non-finite falls back to 1.0.
pub fn clamp_scale(z: f32) -> f32 {
    if z.is_finite() { ((z * 20.0).round() / 20.0).clamp(0.75, 2.0) } else { 1.0 }
}

const HISTORY_MAX: usize = 10;
const HISTORY_QUERY_MAX: usize = 100;

/// Puts `q` first in `h`: trimmed, capped in length, deduped case-insensitively, list capped.
pub fn push_history(h: &mut Vec<String>, q: &str) {
    let q: String = q.trim().chars().take(HISTORY_QUERY_MAX).collect();
    if q.is_empty() {
        return;
    }
    h.retain(|x| x.to_lowercase() != q.to_lowercase());
    h.insert(0, q);
    h.truncate(HISTORY_MAX);
}

fn decode_history(s: &str) -> Vec<String> {
    let mut h = Vec::new();
    for l in s.lines().rev() {
        push_history(&mut h, l);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn missing_or_invalid_values_read_as_the_default() {
        let (_d, db) = db();
        assert!(HWDEC.get(&db).unwrap() && !TRAY_ON_CLOSE.get(&db).unwrap());
        assert_eq!(SEEK_STEP.get(&db).unwrap(), 10);
        for bad in ["0", "601", "x", ""] {
            db.set_setting(SEEK_STEP.key, bad).unwrap();
            assert_eq!(SEEK_STEP.get(&db).unwrap(), 10, "{bad}");
        }
        db.set_setting(VOLUME.key, "500").unwrap();
        assert_eq!(VOLUME.get(&db).unwrap(), 100.0);
        db.set_setting(UI_SCALE.key, "9").unwrap();
        assert_eq!(UI_SCALE.get(&db).unwrap(), 2.0);
        db.set_setting(STREMIO_AUTH.key, "").unwrap();
        assert_eq!(STREMIO_AUTH.get(&db).unwrap(), None);
    }

    #[test]
    fn values_round_trip_and_families_are_keyed_by_suffix() {
        let (_d, db) = db();
        SEEK_STEP.set(&db, &15).unwrap();
        assert_eq!(SEEK_STEP.get(&db).unwrap(), 15);
        THEME_LIGHT.set(&db, &true).unwrap();
        assert_eq!(db.setting("theme").unwrap().as_deref(), Some("light"));
        let order = vec!["https://a.example/m.json|movie|top \"x\"".to_string(), "a\"b:c|d".into()];
        ADDON_ORDER.set_at(&db, "movie", &order).unwrap();
        assert_eq!(ADDON_ORDER.get_at(&db, "movie").unwrap(), order);
        assert!(ADDON_ORDER.get_at(&db, "series").unwrap().is_empty());
        db.set_setting(HOME_ORDER.key, "garbage").unwrap();
        assert!(HOME_ORDER.get(&db).unwrap().is_empty());
        BINGE_GROUP.set_at(&db, "series:tt1", &Some("hd".into())).unwrap();
        assert_eq!(BINGE_GROUP.get_at(&db, "series:tt1").unwrap().as_deref(), Some("hd"));
        BINGE_GROUP.set_at(&db, "series:tt1", &None).unwrap();
        assert_eq!(BINGE_GROUP.get_at(&db, "series:tt1").unwrap(), None, "no group clears it");
        SEARCH_HISTORY.set(&db, &vec!["b".into(), "a".into()]).unwrap();
        assert_eq!(SEARCH_HISTORY.get(&db).unwrap(), ["b", "a"]);
    }

    #[test]
    fn subtitles_start_off_on_a_fresh_install_and_follow_the_last_choice() {
        let (_d, db) = db();
        assert!(subs_off(&db).unwrap());
        SLANG.set(&db, &"en".into()).unwrap();
        assert!(!subs_off(&db).unwrap(), "legacy slang without a choice");
        SUB_OFF.set(&db, &Some(true)).unwrap();
        assert!(subs_off(&db).unwrap());
        db.set_setting(SUB_OFF.key, "").unwrap();
        assert!(!subs_off(&db).unwrap(), "legacy empty value is on");
    }

    #[test]
    fn history_push_dedupe_cap_roundtrip() {
        let mut h = Vec::new();
        push_history(&mut h, "  Matrix ");
        push_history(&mut h, "   ");
        push_history(&mut h, "dune");
        push_history(&mut h, "MATRIX");
        assert_eq!(h, ["MATRIX", "dune"]);
        for i in 0..20 {
            push_history(&mut h, &format!("q{i}"));
        }
        assert_eq!(h.len(), HISTORY_MAX);
        assert_eq!(h[0], "q19");
        push_history(&mut h, &"x".repeat(500));
        assert_eq!(h[0].len(), HISTORY_QUERY_MAX);
        assert!(decode_history("").is_empty());
    }

    #[test]
    fn clamp_scale_bounds_and_rounds() {
        assert_eq!(clamp_scale(0.1), 0.75);
        assert_eq!(clamp_scale(9.0), 2.0);
        assert_eq!(clamp_scale(1.1000001), 1.1);
        assert_eq!(clamp_scale(f32::NAN), 1.0);
    }
}
