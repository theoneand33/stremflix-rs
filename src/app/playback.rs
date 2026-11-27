//! Playback rules that do not touch the GUI: pausing on minimize, the sleep timer, autoplay, loops, media keys, the screen-sleep inhibit.

use std::path::PathBuf;
use stremflix_rs::player::VOLUME_MAX;

/// Our own pause from minimizing: `Sent` until mpv reports it, `Held` while it stays paused,
/// `Yielded` once the user or the sleep timer changes the pause while minimized (left alone until restore).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum AutoPause {
    Off,
    Sent,
    Held,
    Yielded,
}

/// Whether to pause (`Some(true)`) or resume (`Some(false)`) for a minimize change, and the new bookkeeping; only resumes what it paused.
pub(crate) fn pause_step(enabled: bool, away: bool, paused: bool, auto: AutoPause) -> (Option<bool>, AutoPause) {
    // A resume only counts as the user's once mpv has confirmed our pause.
    let auto = match auto {
        AutoPause::Sent if paused => AutoPause::Held,
        AutoPause::Held if !paused => AutoPause::Yielded,
        other => other,
    };
    match (enabled && away, auto) {
        (true, AutoPause::Off) if !paused => (Some(true), AutoPause::Sent),
        (true, _) => (None, auto),
        (false, AutoPause::Sent | AutoPause::Held) => (Some(false), AutoPause::Off),
        (false, _) => (None, AutoPause::Off),
    }
}

/// Preload the next episode in the last 2 min; the 60 s floor keeps a still-loading stream from triggering it.
pub(crate) fn preload_due(pos: f64, dur: f64) -> bool {
    dur > 60.0 && dur - pos <= 120.0
}

/// mpv's `keep-open` pauses on the last frame at end of file. That frame can sit well before the declared
/// duration, so trust `eof`; the 90% floor keeps a stream that died mid-episode from skipping to the next one.
pub(crate) fn at_end(pos: f64, dur: f64, paused: bool, eof: bool) -> bool {
    paused && eof && dur > 0.0 && pos >= dur * 0.9
}

/// Sets loop point `i` (0 = A, 1 = B) to `t`, keeping A before B; true if the points were swapped.
pub(crate) fn set_ab(ab: &mut [Option<f64>; 2], i: usize, t: f64) -> bool {
    ab[i] = Some(t);
    let swap = matches!(*ab, [Some(a), Some(b)] if a > b);
    if swap {
        ab.swap(0, 1);
    }
    swap
}

/// mpv loops only with Repeat on or both A-B points set.
pub(crate) fn looping(repeat: bool, ab: [Option<f64>; 2]) -> bool {
    repeat || (ab[0].is_some() && ab[1].is_some())
}

/// `timer_paused` bookkeeping: confirmed once mpv reports paused, dropped when the user resumes.
pub(crate) fn timer_pause_step(state: Option<bool>, paused: bool) -> Option<bool> {
    match (state, paused) {
        (Some(false), true) => Some(true),
        (Some(true), false) => None,
        (s, _) => s,
    }
}

/// End-of-episode autoplay; never after the sleep timer's own pause or while our minimize pause holds.
pub(crate) fn autoplay_due(autoplay_next: bool, timer_paused: Option<bool>, auto: AutoPause, at_end: bool) -> bool {
    autoplay_next && timer_paused.is_none() && matches!(auto, AutoPause::Off | AutoPause::Yielded) && at_end
}

/// Fraction of the seek bar the cache covers, never behind the playhead.
pub(crate) fn buffered_frac(pos: f64, cached_to: f64, dur: f64) -> f32 {
    if dur > 0.0 { (cached_to.max(pos) / dur).clamp(0.0, 1.0) as f32 } else { 0.0 }
}

/// What an OS media control event does to the player.
#[derive(Debug, PartialEq)]
pub(crate) enum MediaAction {
    Cmd(&'static [&'static str]),
    /// Seconds, and whether relative.
    Seek(f64, bool),
    Next,
    Stop,
}

pub(crate) fn media_action(e: souvlaki::MediaControlEvent) -> Option<MediaAction> {
    use souvlaki::{MediaControlEvent as E, SeekDirection};
    let sign = |d| if d == SeekDirection::Forward { 1.0 } else { -1.0 };
    Some(match e {
        E::Play => MediaAction::Cmd(&["set", "pause", "no"]),
        E::Pause => MediaAction::Cmd(&["set", "pause", "yes"]),
        E::Toggle => MediaAction::Cmd(&["cycle", "pause"]),
        E::Next => MediaAction::Next,
        E::Stop => MediaAction::Stop,
        E::Seek(d) => MediaAction::Seek(10.0 * sign(d), true),
        E::SeekBy(d, by) => MediaAction::Seek(by.as_secs_f64() * sign(d), true),
        E::SetPosition(p) => MediaAction::Seek(p.0.as_secs_f64(), false),
        _ => return None,
    })
}

/// Inhibits screen dimming, blanking and idle sleep until dropped; `None` when no inhibit service is reachable.
pub(crate) fn keep_awake() -> Option<keepawake::KeepAwake> {
    // Linux: one missing service (ScreenSaver or logind) fails the combined request outright, so fall back to each alone.
    // catch_unwind: a failed attempt drops its half-built guard, whose unwrap can panic (see release_awake).
    [(true, true), (true, false), (false, true)].into_iter().find_map(|(display, idle)| {
        std::panic::catch_unwind(|| keepawake::Builder::default().display(display).idle(idle).reason("Video playback").app_name("Stremflix").app_reverse_domain("io.github.stremflix").create().ok()).ok().flatten()
    })
}

/// keepawake's Linux drop unwraps the D-Bus UnInhibit call, which fails if the screensaver service went away mid-playback.
pub(crate) fn release_awake(awake: Option<Option<keepawake::KeepAwake>>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(awake)));
}

/// `<data>/screenshots/<title>-<unix secs>.png`, with the title reduced to filename-safe characters.
pub(crate) fn screenshot_path(dir: &std::path::Path, title: &str, secs: u64) -> PathBuf {
    let t: String = title.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).take(60).collect();
    dir.join("screenshots").join(format!("{t}-{secs}.png"))
}

pub(crate) fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Turn wheel scroll (points) into whole volume steps, keeping the remainder; one wheel line is one step.
pub(crate) fn wheel_steps(acc: &mut f32, dy: f32, line: f32) -> i32 {
    if dy * *acc < 0.0 { *acc = 0.0; } // a direction change starts fresh
    *acc += dy;
    let n = (*acc / line).trunc();
    *acc -= n * line;
    n as i32
}

/// Whole-number volume at a fraction of the slider's width.
pub(crate) fn volume_at(frac: f32) -> f64 {
    (f64::from(frac.clamp(0.0, 1.0)) * VOLUME_MAX).round()
}

/// Hover time snapped to steps of 1/200 of the duration (at least 2 s), so small mouse moves reuse the last preview.
pub(crate) fn preview_time(t: f64, dur: f64) -> f64 {
    let step = (dur / 200.0).max(2.0);
    ((t / step).round() * step).clamp(0.0, dur.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn end_of_file_and_loops() {
        assert!(at_end(99.5, 100.0, true, true) && at_end(95.0, 100.0, true, true), "last frame before the declared duration");
        assert!(!at_end(99.5, 100.0, false, true) && !at_end(99.5, 100.0, true, false), "user pause near the end");
        assert!(!at_end(50.0, 100.0, true, true) && !at_end(0.0, 0.0, true, true), "stream died early, no duration");
        let mut ab = [None; 2];
        assert!(!set_ab(&mut ab, 1, 10.0) && ab == [None, Some(10.0)]);
        assert!(set_ab(&mut ab, 0, 20.0) && ab == [Some(10.0), Some(20.0)], "A after B swaps");
        assert!(!looping(false, [None, Some(1.0)]) && looping(false, ab) && looping(true, [None; 2]));
    }

    #[test]
    fn pause_on_minimize_only_resumes_own_pause() {
        use AutoPause::*;
        assert_eq!(pause_step(true, true, false, Off), (Some(true), Sent));
        assert_eq!(pause_step(true, true, true, Off), (None, Off)); // user paused: leave alone
        assert_eq!(pause_step(true, false, true, Held), (Some(false), Off));
        assert_eq!(pause_step(false, true, false, Off), (None, Off));
        assert_eq!(pause_step(false, true, true, Held), (Some(false), Off)); // setting turned off
        assert_eq!(pause_step(true, true, true, Held), (None, Held)); // our pause, still minimized: hold
    }

    #[test]
    fn sleep_timer_pause_survives_minimize() {
        use AutoPause::*;
        // The sleep timer paused while minimized: no re-pause, and restore does not resume it.
        assert_eq!(pause_step(true, true, true, Yielded), (None, Yielded));
        assert_eq!(pause_step(true, false, true, Yielded), (None, Off));
        assert_eq!(pause_step(true, false, true, Sent), (Some(false), Off), "our own pause still resumes");
    }

    #[test]
    fn user_pause_while_minimized_is_kept() {
        // Auto-pause, media-key Play, then Pause again, all while minimized: the user's state wins until restore.
        let mut auto = AutoPause::Off;
        let mut step = |paused, away| {
            let (pause, next) = pause_step(true, away, paused, auto);
            auto = next;
            pause
        };
        assert_eq!(step(false, true), Some(true)); // auto-pause sent
        assert_eq!(step(false, true), None); // mpv not paused yet: still ours
        assert_eq!(step(true, true), None); // mpv confirms the pause
        assert_eq!(step(false, true), None); // user Play: not re-paused
        assert_eq!(step(true, true), None); // user Pause: kept
        assert_eq!(step(true, false), None); // restore: user's pause stays
        assert_eq!(auto, AutoPause::Off);
    }

    #[test]
    fn volume_at_spans_the_whole_range() {
        assert_eq!(volume_at(0.0), 0.0);
        assert_eq!(volume_at(0.5), 65.0);
        assert_eq!(volume_at(1.0), VOLUME_MAX);
        // Dragging past either end of the slider.
        assert_eq!(volume_at(-0.2), 0.0);
        assert_eq!(volume_at(1.2), VOLUME_MAX);
    }

    #[test]
    fn wheel_accumulates_to_steps() {
        let mut acc = 0.0;
        // egui's native line_scroll_speed: one real notch is 40 pt.
        assert_eq!(wheel_steps(&mut acc, 40.0, 40.0), 1);
        assert_eq!((0..5).map(|_| wheel_steps(&mut acc, 40.0, 40.0)).sum::<i32>(), 5);
        assert_eq!(wheel_steps(&mut acc, -80.0, 40.0), -2);
        assert_eq!((0..4).map(|_| wheel_steps(&mut acc, 10.0, 40.0)).sum::<i32>(), 1);
        assert_eq!(wheel_steps(&mut acc, 0.0, 40.0), 0);
        // A +30 leftover must not cancel a fresh -40 notch.
        acc = 30.0;
        assert_eq!(wheel_steps(&mut acc, -40.0, 40.0), -1);
    }

    #[test]
    fn buffered_frac_clamps() {
        assert_eq!(buffered_frac(10.0, 50.0, 100.0), 0.5);
        assert_eq!(buffered_frac(10.0, 0.0, 100.0), 0.1);
        assert_eq!(buffered_frac(10.0, 500.0, 100.0), 1.0);
        assert_eq!(buffered_frac(10.0, 50.0, 0.0), 0.0);
    }

    #[test]
    fn media_events_map_to_player_actions() {
        use souvlaki::{MediaControlEvent as E, MediaPosition, SeekDirection::*};
        let s = Duration::from_secs;
        assert_eq!(media_action(E::Toggle), Some(MediaAction::Cmd(&["cycle", "pause"])));
        assert_eq!(media_action(E::Play), Some(MediaAction::Cmd(&["set", "pause", "no"])));
        assert_eq!(media_action(E::Pause), Some(MediaAction::Cmd(&["set", "pause", "yes"])));
        assert_eq!(media_action(E::Next), Some(MediaAction::Next));
        assert_eq!(media_action(E::Stop), Some(MediaAction::Stop));
        assert_eq!(media_action(E::Seek(Backward)), Some(MediaAction::Seek(-10.0, true)));
        assert_eq!(media_action(E::SeekBy(Forward, s(30))), Some(MediaAction::Seek(30.0, true)));
        assert_eq!(media_action(E::SetPosition(MediaPosition(s(95)))), Some(MediaAction::Seek(95.0, false)));
        assert_eq!(media_action(E::Previous), None);
        assert_eq!(media_action(E::Quit), None);
    }

    #[test]
    fn screenshot_path_is_filename_safe() {
        let p = screenshot_path(std::path::Path::new("/d"), "A/B: c", 7);
        assert_eq!(p, std::path::Path::new("/d/screenshots/A_B__c-7.png"));
    }

    #[test]
    fn preview_time_snaps_to_steps() {
        assert_eq!(preview_time(61.0, 100.0), 62.0);
        assert_eq!(preview_time(1001.0, 2000.0), 1000.0);
        assert_eq!(preview_time(1999.0, 2000.0), 2000.0);
        assert_eq!(preview_time(-5.0, 100.0), 0.0);
    }
}
