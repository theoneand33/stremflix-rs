//! Episodes of series: watch order, what is new or upcoming, and which new ones were already notified.

use std::time::Duration;
use stremflix_rs::addon::{MetaPreview, Video};

/// How far back an episode's release date still counts as new.
pub(crate) const NEW_EPISODE_DAYS: i64 = 14;

/// Most `<series id>:<episode id>` keys kept per series; the oldest of that series go first.
/// A series' own `<series id>:` marker is never evicted, or its next episode would be silently re-seeded.
pub(crate) const NOTIFIED_PER_SERIES: usize = 50;

/// How often saved series are re-checked for new episodes while the app runs.
pub(crate) const NOTIFY_EVERY: Duration = Duration::from_secs(6 * 3600);

/// Drops every notified key whose series is no longer in the saved library, so stale keys don't pile up.
pub(crate) fn prune_unsaved(seen: &mut Vec<String>, saved: &[&str]) {
    seen.retain(|k| saved.iter().any(|id| k.strip_prefix(*id).is_some_and(|r| r.starts_with(':'))));
}

/// Records `eps` of `series` in `seen` and returns those never seen before. A series
/// with no entry yet (first run, or newly saved) is only recorded, so its backlog stays silent.
pub(crate) fn fresh_episodes(seen: &mut Vec<String>, series: &str, eps: &[&str]) -> Vec<String> {
    let marker = format!("{series}:");
    let known = seen.contains(&marker);
    if !known {
        seen.push(marker.clone());
    }
    let mut fresh = Vec::new();
    for ep in eps {
        let k = format!("{series}:{ep}");
        if !seen.contains(&k) {
            seen.push(k);
            if known {
                fresh.push(ep.to_string());
            }
        }
    }
    // Cap only this series' episode keys (never its marker, never other series), but always
    // keep the whole current window so an episode still listed can't be dropped and re-notified.
    let mut excess = seen.iter().filter(|k| k.starts_with(&marker) && **k != marker).count().saturating_sub(NOTIFIED_PER_SERIES.max(eps.len()));
    if excess > 0 {
        seen.retain(|k| {
            let evict = excess > 0 && k.starts_with(&marker) && *k != marker;
            if evict {
                excess -= 1;
            }
            !evict
        });
    }
    fresh
}

/// Replaces any earlier row for the series, so refetches don't duplicate rows; an empty `videos` only removes it.
pub(crate) fn upsert_by_id(list: &mut Vec<(MetaPreview, Vec<Video>)>, meta: MetaPreview, videos: Vec<Video>) {
    list.retain(|(m, _)| m.id != meta.id);
    if !videos.is_empty() {
        list.push((meta, videos));
    }
}

/// Desktop notification; never blocks the UI and a missing notification service only logs.
pub(crate) fn os_notify(title: String, body: String) {
    std::thread::spawn(move || {
        if let Err(e) = notify_rust::Notification::new().appname("Stremflix-rs").summary(&title).body(&body).show() {
            stremflix_rs::elog!("notification: {e}");
        }
    });
}

/// Days since 1970-01-01 (UTC) today.
pub(crate) fn unix_days() -> i64 {
    jiff::Timestamp::now().as_second().div_euclid(86400)
}

/// `YYYY-MM-DD` of a day count since 1970-01-01; empty outside jiff's year range (-9999..=9999).
pub(crate) fn date_string(days: i64) -> String {
    jiff::Span::new().try_days(days).and_then(|n| jiff::civil::date(1970, 1, 1).checked_add(n)).map(|d| d.to_string()).unwrap_or_default()
}

/// Numbered episodes released on `since..=today` (`YYYY-MM-DD`), in list order.
pub(crate) fn recent_episodes(videos: Vec<Video>, since: &str, today: &str) -> Vec<Video> {
    let day = |v: &Video| v.released.as_deref().and_then(|d| d.get(..10)).map(str::to_owned);
    videos.into_iter().filter(|v| v.episode.is_some() && v.season != Some(0) && day(v).is_some_and(|d| d.as_str() >= since && d.as_str() <= today)).collect()
}

/// How far ahead the Library calendar looks.
pub(crate) const CALENDAR_DAYS: i64 = 30;

/// Calendar entries (series, episode) grouped by release day within `today..=today+CALENDAR_DAYS`, earliest
/// first, headed Today, Tomorrow, then the date. `released` is read as a UTC date (no timezone database);
/// anything that is not `YYYY-MM-DD` is skipped.
pub(crate) fn calendar_groups(entries: &[(MetaPreview, Vec<Video>)], today: i64) -> Vec<(String, Vec<(&MetaPreview, &Video)>)> {
    let well_formed = |d: &str| d.len() == 10 && d.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
    let (from, to) = (date_string(today), date_string(today + CALENDAR_DAYS));
    let mut days: Vec<(&str, &MetaPreview, &Video)> = entries
        .iter()
        .flat_map(|(m, vs)| vs.iter().map(move |v| (m, v)))
        .filter(|(_, v)| v.episode.is_some() && v.season != Some(0))
        .filter_map(|(m, v)| v.released.as_deref().and_then(|d| d.get(..10)).filter(|d| well_formed(d) && *d >= from.as_str() && *d <= to.as_str()).map(|d| (d, m, v)))
        .collect();
    days.sort_by(|a, b| (a.0, &a.1.name, a.2.season, a.2.episode).cmp(&(b.0, &b.1.name, b.2.season, b.2.episode)));
    let mut out: Vec<(String, Vec<(&MetaPreview, &Video)>)> = Vec::new();
    for (d, m, v) in days {
        let head = if d == from { "Today".to_string() } else if d == date_string(today + 1) { "Tomorrow".to_string() } else { d.to_string() };
        match out.last_mut() {
            Some((h, g)) if *h == head => g.push((m, v)),
            _ => out.push((head, vec![(m, v)])),
        }
    }
    out
}

/// An episode's release day, marked when it is after `today` (both `YYYY-MM-DD`): addons list episodes before they air, with no streams yet.
pub(crate) fn episode_date(released: Option<&str>, today: &str) -> String {
    let day: String = released.map(|d| d.chars().take(10).collect()).unwrap_or_default();
    if day.as_str() > today { format!("Airs {day}") } else { day }
}

pub(crate) fn season_label(season: Option<u32>) -> String {
    match season {
        Some(0) => "Specials".into(),
        Some(n) => format!("Season {n}"),
        None => "Other".into(),
    }
}

/// Episodes in watch order: by season then episode, with specials (season 0), then unnumbered videos, last.
pub(crate) fn sort_episodes(videos: &mut [Video]) {
    videos.sort_by_key(|v| (matches!(v.season, Some(0) | None), v.season.is_none(), v.season, v.episode));
}

/// The episode after `current` in `sort_episodes` order; a regular episode never rolls over into specials or unnumbered videos.
pub(crate) fn next_episode<'a>(episodes: &'a [Video], current: &str) -> Option<&'a Video> {
    let i = episodes.iter().position(|v| v.id == current)?;
    let regular = |v: &Video| v.season.is_some_and(|s| s > 0);
    episodes.get(i + 1).filter(|n| regular(n) || !regular(&episodes[i]))
}

/// Series id of a local episode progress key's video id, for history saved before the series id was stored:
/// `tt1:<season>:<episode>` → `tt1`, `kitsu:1:<episode>` → `kitsu:1` (no season). Other ids are returned unchanged.
pub(crate) fn series_id(video: &str) -> &str {
    let num = |x: &str| x.parse::<u32>().is_ok();
    match video.rsplitn(3, ':').collect::<Vec<_>>()[..] {
        [e, s, id] if num(e) && num(s) && id.contains(|c: char| c.is_ascii_digit()) => id,
        [e, ..] if num(e) => &video[..video.len() - e.len() - 1],
        _ => video,
    }
}

/// Episode to start a series at: the one after the last watched (in list order), else the first regular one.
pub(crate) fn start_episode(episodes: &[Video], watched: impl Fn(&Video) -> bool) -> Option<&Video> {
    let first = || episodes.iter().find(|v| v.season.is_some_and(|s| s > 0)).or(episodes.first());
    episodes.iter().rposition(watched).and_then(|i| next_episode(episodes, &episodes[i].id)).or_else(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_episodes_seeds_dedupes_and_roundtrips() {
        let mut seen = Vec::new();
        // First sight of a series records its backlog silently.
        assert!(fresh_episodes(&mut seen, "tt1", &["tt1:1:1", "tt1:1:2"]).is_empty());
        assert_eq!(fresh_episodes(&mut seen, "tt1", &["tt1:1:1", "tt1:1:2", "tt1:1:3"]), ["tt1:1:3"]);
        // Each episode notifies once.
        assert!(fresh_episodes(&mut seen, "tt1", &["tt1:1:3"]).is_empty());
        // Another series is seeded on its own.
        assert!(fresh_episodes(&mut seen, "tt2", &["tt2:1:1"]).is_empty());
    }

    #[test]
    fn markers_survive_the_cap_and_other_series_are_untouched() {
        let mut seen = Vec::new();
        fresh_episodes(&mut seen, "tt1", &["e1", "e2"]);
        fresh_episodes(&mut seen, "tt3", &[]);
        // Many batches for one series push it past its per-series cap.
        for batch in 0..5 {
            let eps: Vec<String> = (0..20).map(|i| format!("b{batch}e{i}")).collect();
            let refs: Vec<&str> = eps.iter().map(String::as_str).collect();
            fresh_episodes(&mut seen, "tt3", &refs);
        }
        let tt3: Vec<&String> = seen.iter().filter(|k| k.starts_with("tt3:") && k.as_str() != "tt3:").collect();
        assert_eq!(tt3.len(), NOTIFIED_PER_SERIES, "capped per series");
        assert_eq!(tt3.last().unwrap().as_str(), "tt3:b4e19", "newest kept, oldest dropped");
        // Markers survive the cap pass, and the other series keeps its keys.
        assert!(seen.contains(&"tt3:".to_string()) && seen.contains(&"tt1:".to_string()));
        assert!(seen.contains(&"tt1:e1".to_string()) && seen.contains(&"tt1:e2".to_string()));
        // A series whose marker survived still notifies its next episode instead of re-seeding.
        assert_eq!(fresh_episodes(&mut seen, "tt3", &["new"]), ["new"]);
        // A window larger than the cap is kept whole (no re-notify of episodes still listed).
        let big: Vec<String> = (0..NOTIFIED_PER_SERIES + 5).map(|i| format!("w{i}")).collect();
        let refs: Vec<&str> = big.iter().map(String::as_str).collect();
        fresh_episodes(&mut seen, "tt3", &refs);
        assert!(fresh_episodes(&mut seen, "tt3", &refs).is_empty());
    }

    #[test]
    fn prune_unsaved_drops_only_unsaved_series() {
        let mut seen = vec!["tt1:".into(), "tt1:e1".into(), "tt10:".into(), "tt10:e1".into(), "kitsu:7:".into(), "kitsu:7:e1".into(), "gone:".into(), "gone:e1".into()];
        prune_unsaved(&mut seen, &["tt1", "kitsu:7"]);
        // tt10 is a different series than tt1 even though its id starts with "tt1".
        assert_eq!(seen, ["tt1:", "tt1:e1", "kitsu:7:", "kitsu:7:e1"]);
    }

    #[test]
    fn empty_window_still_seeds_marker() {
        let mut seen = Vec::new();
        assert!(fresh_episodes(&mut seen, "tt1", &[]).is_empty());
        assert_eq!(fresh_episodes(&mut seen, "tt1", &["tt1:1:5"]), ["tt1:1:5"]);
    }

    #[test]
    fn refetch_does_not_duplicate_rows() {
        let meta = |id: &str| MetaPreview { id: id.into(), kind: "series".into(), name: id.into(), ..Default::default() };
        let video = |id: &str| Video { id: id.into(), title: None, released: None, season: Some(1), episode: Some(1), thumbnail: None };
        let mut list = Vec::new();
        upsert_by_id(&mut list, meta("a"), vec![video("a1")]);
        upsert_by_id(&mut list, meta("b"), vec![video("b1")]);
        upsert_by_id(&mut list, meta("a"), vec![video("a2")]);
        assert_eq!(list.iter().map(|(m, _)| m.id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(list[1].1[0].id, "a2");
        // An empty refetch (episodes aged out of the window) removes the old row.
        upsert_by_id(&mut list, meta("a"), vec![]);
        assert_eq!(list.iter().map(|(m, _)| m.id.as_str()).collect::<Vec<_>>(), ["b"]);

    }

    #[test]
    fn calendar_window() {
        let meta = MetaPreview { id: "tt1".into(), kind: "series".into(), name: "Show".into(), ..Default::default() };
        let v = |id: &str, d: Option<&str>, season| Video { id: id.into(), title: None, season: Some(season), episode: Some(1), released: d.map(Into::into), thumbnail: None };
        let today = 19_800; // any fixed day
        let (t, tm, far) = (date_string(today), date_string(today + 1), date_string(today + 5));
        let videos = vec![
            v("far", Some(&format!("{far}T00:00:00.000Z")), 1),
            v("tm", Some(&tm), 1),
            v("today", Some(&t), 1),
            v("past", Some(&date_string(today - 1)), 1),
            v("out", Some(&date_string(today + CALENDAR_DAYS + 1)), 1),
            v("bad", Some("2024-0x-10"), 1),
            v("short", Some("2024"), 1),
            v("none", None, 1),
            v("special", Some(&t), 0),
        ];
        let entries = [(meta, videos)];
        let g = calendar_groups(&entries, today);
        let shape: Vec<(&str, Vec<&str>)> = g.iter().map(|(h, e)| (h.as_str(), e.iter().map(|(_, v)| v.id.as_str()).collect())).collect();
        assert_eq!(shape, vec![("Today", vec!["today"]), ("Tomorrow", vec!["tm"]), (far.as_str(), vec!["far"])]);
        assert!(calendar_groups(&[], today).is_empty());
    }

    #[test]
    fn new_episodes_window() {
        assert_eq!(date_string(0), "1970-01-01");
        assert_eq!(date_string(19782), "2024-02-29");
        assert_eq!((date_string(3_000_000), date_string(i64::MAX)), (String::new(), String::new()), "out of range is empty");
        let v = |id: &str, season, rel: Option<&str>| Video { id: id.into(), title: None, season, episode: Some(1), released: rel.map(Into::into), thumbnail: None };
        let vs = vec![
            v("old", Some(1), Some("2024-01-01T00:00:00.000Z")),
            v("new", Some(1), Some("2024-03-05T00:00:00.000Z")),
            v("future", Some(1), Some("2024-04-01")),
            v("special", Some(0), Some("2024-03-05")),
            v("undated", Some(1), None),
        ];
        let ids: Vec<_> = recent_episodes(vs, "2024-02-20", "2024-03-06").into_iter().map(|v| v.id).collect();
        assert_eq!(ids, ["new"]);
        // Both ends of the window are inclusive; a day outside either is not.
        let edge = vec![v("before", Some(1), Some("2024-02-19")), v("since", Some(1), Some("2024-02-20")), v("today", Some(1), Some("2024-03-06")), v("after", Some(1), Some("2024-03-07"))];
        let ids: Vec<_> = recent_episodes(edge, "2024-02-20", "2024-03-06").into_iter().map(|v| v.id).collect();
        assert_eq!(ids, ["since", "today"]);
    }

    #[test]
    fn episodes_sort_with_specials_last() {
        let v = |season, episode| Video { id: format!("tt1:{season:?}:{episode}"), title: None, season, episode: Some(episode), released: None, thumbnail: None };
        let mut eps = vec![v(Some(0), 1), v(None, 1), v(Some(2), 1), v(Some(1), 2), v(Some(1), 1)];
        sort_episodes(&mut eps);
        let order: Vec<_> = eps.iter().map(|e| (e.season, e.episode)).collect();
        assert_eq!(order, [(Some(1), Some(1)), (Some(1), Some(2)), (Some(2), Some(1)), (Some(0), Some(1)), (None, Some(1))]);
        assert_eq!((season_label(Some(0)), season_label(Some(3))), ("Specials".to_string(), "Season 3".to_string()));
    }

    #[test]
    fn next_episode_in_watch_order() {
        let v = |season, episode| Video { id: format!("{season:?}:{episode}"), title: None, season, episode: Some(episode), released: None, thumbnail: None };
        let mut eps = vec![v(Some(1), 1), v(Some(1), 2), v(Some(2), 1), v(Some(0), 1), v(Some(0), 2), v(None, 1)];
        sort_episodes(&mut eps);
        let next = |id: &str| next_episode(&eps, id).map(|n| n.id.as_str());
        assert_eq!(next("Some(1):2"), Some("Some(2):1"), "crosses seasons");
        assert_eq!(next("Some(1):1"), Some("Some(1):2"));
        assert_eq!(next("Some(2):1"), None, "finale doesn't roll into specials");
        assert_eq!(next("Some(0):1"), Some("Some(0):2"));
        assert_eq!(next("None:1"), None, "last video");
        assert_eq!(next("missing"), None);
        let start = |seen: &[&str]| start_episode(&eps, |v| seen.contains(&v.id.as_str())).map(|n| n.id.as_str());
        assert_eq!(start(&[]), Some("Some(1):1"), "first regular episode");
        assert_eq!(start(&["Some(1):1", "Some(1):2"]), Some("Some(2):1"), "after the last watched");
        assert_eq!(start(&["Some(2):1"]), Some("Some(1):1"), "finale watched: start over");
    }

    #[test]
    fn series_id_strips_season_and_episode() {
        assert_eq!(series_id("tt1:1:2"), "tt1");
        assert_eq!(series_id("tt1"), "tt1");
        assert_eq!(series_id("kitsu:1234:5"), "kitsu:1234");
        assert_eq!(series_id("x:a:b"), "x:a:b");
    }
}
