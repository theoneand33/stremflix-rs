//! Streams and subtitles of a title: which one to autoplay, the headers it needs, and subtitles matched to the exact file.

use std::time::{Duration, Instant};
use stremflix_rs::addon::{AddonClient, PlayTarget, Stream};

/// Addon subtitles as (url, lang).
pub(crate) type Subs = Vec<(String, String)>;

/// Media file `(index, name, size)` of a torrent.
pub(crate) type MediaFile = (usize, String, u64);

/// A subtitle an addon offers for the playing item, listed in the player's search panel.
#[derive(Clone)]
pub(crate) struct SubFound {
    pub(crate) addon: String,
    pub(crate) lang: String,
    pub(crate) url: String,
}

/// State of the player's subtitle search panel.
#[derive(Default)]
pub(crate) struct SubSearch {
    pub(crate) open: bool,
    pub(crate) busy: bool,
    pub(crate) filter: String,
    pub(crate) results: Vec<SubFound>,
    pub(crate) status: String,
}

/// Results whose language or addon name contains `q` (case-insensitive, blank matches all).
pub(crate) fn filter_subs<'a>(found: &'a [SubFound], q: &str) -> Vec<&'a SubFound> {
    let q = q.trim().to_lowercase();
    found.iter().filter(|f| f.lang.to_lowercase().contains(&q) || f.addon.to_lowercase().contains(&q)).collect()
}

/// Sleep timer: pause at this time; the minutes chosen.
pub(crate) type Sleep = (Instant, u64);

/// What to play when streams arrive: nothing, the only stream, or the top one; with a binge group, its stream first.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Autoplay {
    Off,
    Only,
    Top(Option<String>),
    /// The next episode once one ends: like `Only` with the group, and the sleep timer keeps running into it.
    Next(Option<String>, Option<Sleep>),
}

/// Playable source of the first stream in binge group `group` (same addon/quality as the previous episode).
pub(crate) fn binge_source(streams: &[Stream], group: &str) -> Option<String> {
    streams.iter().filter(|s| s.hints.binge_group.as_deref() == Some(group)).find_map(Stream::source)
}

/// Progress-key prefix of trailers, which are never saved to history; `/` keeps it off any `<kind>:<id>` key.
pub(crate) const TRAILER: &str = "trailer/";

/// The addon's `proxyHeaders` for playing `url`, only when it is that addon's own http(s) stream:
/// never for torrents (`url` is then the local torrent server, not `source`) or local files.
pub(crate) fn request_headers(streams: &[Stream], source: &str, url: &str) -> Vec<(String, String)> {
    own_stream(streams, source, url).map(|s| s.hints.request_headers.clone()).unwrap_or_default()
}

/// The stream whose own http(s) URL is `url`, when `url` is also the `source` being played.
pub(crate) fn own_stream<'a>(streams: &'a [Stream], source: &str, url: &str) -> Option<&'a Stream> {
    let own = |s: &&Stream| matches!(s.target(), Some(PlayTarget::Http(u)) if u.as_str() == url);
    (source == url).then(|| streams.iter().find(own)).flatten()
}

/// The playable source when there is exactly one; `None` when there's a choice (or nothing).
pub(crate) fn only_source(streams: &[Stream]) -> Option<String> {
    let mut it = streams.iter().filter_map(Stream::source);
    let first = it.next();
    if it.next().is_some() { None } else { first }
}

/// Inputs of [`hashed_subs`].
pub(crate) struct SubLookup {
    pub(crate) addons: Vec<String>,
    pub(crate) kind: String,
    pub(crate) id: String,
    pub(crate) hints: stremflix_rs::addon::BehaviorHints,
    pub(crate) headers: Vec<(String, String)>,
}

/// HLS/DASH manifest URL: a list of segments, not the video file.
pub(crate) fn is_playlist(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| [".m3u8", ".mpd"].iter().any(|x| u.path().to_ascii_lowercase().ends_with(x)))
}

/// Re-ask subtitle addons with the stream's `videoHash`/`videoSize`/`filename` (the hash from the stream's hints,
/// else read from `url`), so OpenSubtitles can match the exact file. Matches come first, then the earlier `subs`;
/// on timeout or without a hash, `subs` unchanged.
pub(crate) async fn hashed_subs(client: AddonClient, l: SubLookup, url: &str, subs: Subs) -> Subs {
    // Playback waits on this; one shared deadline, so a slow hash or addon cannot hold it up for long
    // and a slow addon does not discard the fast ones' matches.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let ask = async {
        // Without a hash the addons would only repeat the earlier lookup.
        let (hash, size) = match l.hints.video_hash.clone() {
            Some(h) => (h, l.hints.video_size),
            // Playlists and YouTube pages are not the video file, so they have no hash.
            None if stremflix_rs::addon::is_youtube(url) || is_playlist(url) => return Vec::new(),
            None => match tokio::time::timeout_at(deadline, client.video_hash(url, &l.headers)).await {
                Ok(Ok((h, n))) => (h, Some(n)),
                _ => return Vec::new(),
            },
        };
        let extra: Vec<(&str, String)> = [("videoHash", Some(hash)), ("videoSize", size.map(|n| n.to_string())), ("filename", l.hints.filename.clone())]
            .into_iter()
            .filter_map(|(k, v)| Some((k, v?)))
            .collect();
        let ask = |u: String| {
            let (c, k, i, e) = (client.clone(), l.kind.clone(), l.id.clone(), extra.clone());
            async move { tokio::time::timeout_at(deadline, c.subtitles(&u, &k, &i, &e)).await? }
        };
        concurrently(l.addons.clone(), ask).await.into_iter().flatten().flatten().map(|s| (s.url, s.lang)).collect::<Subs>()
    };
    let mut out = ask.await;
    for s in subs {
        if !out.iter().any(|(u, _)| *u == s.0) {
            out.push(s);
        }
    }
    out
}

/// Run `f` for every item at once; results come back in item order (e.g. addon priority).
pub(crate) async fn concurrently<I, T, F>(items: Vec<I>, f: impl Fn(I) -> F) -> Vec<anyhow::Result<T>>
where
    T: Send + 'static,
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
{
    let tasks: Vec<_> = items.into_iter().map(|i| tokio::spawn(f(i))).collect();
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        out.push(t.await.unwrap_or_else(|e| Err(e.into())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binge_source_matches_group() {
        let s = |u: &str, g: Option<&str>| Stream { url: Some(u.into()), hints: stremflix_rs::addon::BehaviorHints { binge_group: g.map(String::from), ..Default::default() }, ..Default::default() };
        let streams = [s("https://a/1", None), s("javascript:x", Some("hd")), s("https://a/2", Some("sd")), s("https://a/3", Some("hd"))];
        assert_eq!(binge_source(&streams, "hd").as_deref(), Some("https://a/3"), "skips unplayable");
        assert_eq!(binge_source(&streams, "4k"), None);
    }

    #[test]
    fn request_headers_only_for_own_http_stream() {
        let h = vec![("Referer".to_string(), "https://a.example/".to_string())];
        let s = |u: &str| Stream { url: Some(u.into()), hints: stremflix_rs::addon::BehaviorHints { request_headers: h.clone(), ..Default::default() }, ..Default::default() };
        let magnet = format!("magnet:?xt=urn:btih:{}", "a".repeat(40));
        let streams = [s("https://a.example/v.mp4"), s(&magnet)];
        assert_eq!(request_headers(&streams, "https://a.example/v.mp4", "https://a.example/v.mp4"), h);
        assert!(request_headers(&streams, "https://b.example/v.mp4", "https://b.example/v.mp4").is_empty());
        assert!(request_headers(&streams, &magnet, "http://127.0.0.1:1234/stream").is_empty(), "torrent");
        assert!(request_headers(&streams, "/media/v.mkv", "/media/v.mkv").is_empty(), "local file");
    }

    #[test]
    fn filter_subs_matches_lang_or_addon() {
        let f = |addon: &str, lang: &str| SubFound { addon: addon.into(), lang: lang.into(), url: String::new() };
        let all = [f("OpenSubtitles", "eng"), f("Other", "por-BR"), f("Other", "fre")];
        assert_eq!(filter_subs(&all, " ENG ").len(), 1);
        assert_eq!(filter_subs(&all, "other").len(), 2);
        assert_eq!(filter_subs(&all, "").len(), 3);
        assert!(filter_subs(&all, "xx").is_empty());
    }

    #[tokio::test]
    async fn hashed_subs_put_matches_first() {
        let base = test_addon::spawn(test_addon::Config { media_dir: None, torrent: None }).unwrap();
        let hints = stremflix_rs::addon::BehaviorHints { video_hash: Some("8e245d9679d31e12".into()), ..Default::default() };
        let l = |hints| SubLookup { addons: vec![format!("{base}/manifest.json")], kind: "movie".into(), id: test_addon::META_ID.into(), hints, headers: vec![] };
        let test_en = (format!("{base}/subs/test-en.vtt"), "eng".to_string());
        let old = vec![("https://x.example/a.srt".to_string(), "fre".to_string()), test_en.clone()];
        let out = hashed_subs(AddonClient::new(), l(hints.clone()), "/local/file.mkv", old.clone()).await;
        assert_eq!(out, [test_en, old[0].clone()], "match first, deduped by URL");
        let out = hashed_subs(AddonClient::new(), l(Default::default()), "/local/file.mkv", old.clone()).await;
        assert_eq!(out, old, "no hash: unchanged");
        // An addon that never answers times out alone; the fast one's match is kept.
        let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut slow = l(hints);
        slow.addons.insert(0, format!("http://{}/manifest.json", hang.local_addr().unwrap()));
        let out = hashed_subs(AddonClient::new(), slow, "/local/file.mkv", vec![]).await;
        assert_eq!(out, [(format!("{base}/subs/test-en.vtt"), "eng".to_string())]);
    }

    #[tokio::test]
    async fn concurrently_keeps_item_order() {
        let out = concurrently(vec![30u64, 0, 10], |ms| async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            anyhow::Ok(ms)
        })
        .await;
        assert_eq!(out.into_iter().map(Result::unwrap).collect::<Vec<_>>(), [30, 0, 10]);
    }

    #[test]
    fn only_source_autoplays_only_without_a_choice() {
        let s = |u: &str| Stream { url: Some(u.into()), ..Default::default() };
        let bad = s("ftp://x.example/a"); // unplayable, doesn't count as a choice
        assert_eq!(only_source(&[]), None);
        assert_eq!(only_source(&[bad.clone(), s("https://x.example/a")]).as_deref(), Some("https://x.example/a"));
        assert_eq!(only_source(&[s("https://x.example/a"), s("https://x.example/b")]), None);
    }
}
