//! Opening titles, episodes and links, fetching their streams, and starting playback.

use crate::*;

impl App {
    /// Resolve `source` (http URL, magnet, or local path) and start playback.
    pub(crate) fn play(&mut self, ctx: &egui::Context, key: String, title: String, source: String, subs: Subs) {
        self.start_play(ctx, key, title, source, subs, None);
    }

    /// Like `play`, with the sleep timer of the player it follows.
    pub(crate) fn start_play(&mut self, ctx: &egui::Context, key: String, title: String, source: String, subs: Subs, sleep: Option<Sleep>) {
        let nav = self.begin_nav();
        let lookup = self.sub_lookup(&key, &source);
        let torrent = is_torrent_source(&source);
        let mut subs = subs;
        if !torrent {
            subs.extend(player::sidecar_subs(std::path::Path::new(&source))); // empty for URLs and missing paths
        }
        if !torrent && lookup.is_none() {
            let _ = self.tx.send(Msg::Play(key, title, source.clone(), source, subs, None, sleep, nav));
            return;
        }
        let (start, client) = (self.engine(), self.client.clone());
        self.spawn(ctx, async move {
            let (url, id) = match torrent {
                true => {
                    let engine = start.await?;
                    if parse_magnet_hash(&source).is_some() {
                        let files = engine.media_choices(&source).await?;
                        if !files.is_empty() {
                            return Ok(Msg::PickFile(key, title, source, subs, files, nav));
                        }
                    }
                    let (url, id) = engine.stream_url(&source, None, false).await?;
                    (url, Some(id))
                }
                false => (source.clone(), None),
            };
            let subs = match lookup {
                Some(l) => hashed_subs(client, l, &url, subs).await,
                None => subs,
            };
            Ok(Msg::Play(key, title, source, url, subs, id, sleep, nav))
        });
    }

    /// The torrent engine, started on first use.
    pub(crate) fn engine(&self) -> impl Future<Output = anyhow::Result<Arc<TorrentEngine>>> + Send + 'static {
        // A chosen folder gets its own subfolder: the engine clears its folder at start.
        let dir = match settings::TORRENT_DIR.get(&self.db).ok().filter(|d| !d.is_empty()) {
            Some(d) => PathBuf::from(d).join("stremflix-rs-torrents"),
            None => self.data_dir.join("torrents"),
        };
        let (cell, cache, profile, limits, seed) = (self.torrent.clone(), self.torrent_cache, self.torrent_profile, self.torrent_limits, self.torrent_seed);
        let dht = std::env::var_os("STREMFLIX_RS_NO_DHT").is_none();
        async move { cell.get_or_try_init(|| async move { TorrentEngine::start(dir, dht, cache, profile, limits, seed).await }).await.cloned() }
    }

    /// What a hash-matched subtitle query needs for the selected title's stream `source`;
    /// `None` without subtitle addons or outside the selected title's streams.
    pub(crate) fn sub_lookup(&self, key: &str, source: &str) -> Option<SubLookup> {
        let (m, streams, _) = self.selected.as_ref().filter(|_| self.play_key().is_some_and(|(k, _)| k == key))?;
        let s = streams.iter().find(|s| s.source().as_deref() == Some(source))?;
        let id = self.episode.as_ref().map_or(m.id.clone(), |v| v.id.clone());
        let addons = self.addons_for("subtitles", &m.kind, &id);
        (!addons.is_empty()).then(|| SubLookup { addons, kind: m.kind.clone(), id, hints: s.hints.clone(), headers: request_headers(streams, source, source) })
    }

    /// Play a magnet link or a local `.torrent` file (typed, pasted or dropped).
    pub(crate) fn play_torrent(&mut self, ctx: &egui::Context, input: &str) {
        let input = input.trim();
        if is_torrent_file(input) {
            let name = std::path::Path::new(input).file_stem().map_or("Torrent".into(), |n| n.to_string_lossy().into_owned());
            return self.play(ctx, key::file(input), name, input.into(), vec![]);
        }
        match parse_magnet_hash(input) {
            Some(h) => self.play(ctx, key::magnet(&h), format!("Torrent {}", &h[..8]), input.into(), vec![]),
            None => report(&mut self.error, "not a magnet link or .torrent file"),
        }
    }

    /// Stop every torrent but `keep`, so none keeps downloading and seeding; the engine runs it before any later stream.
    pub(crate) fn release_torrents(&self, keep: Option<usize>) {
        if let Some(engine) = self.torrent.get() {
            self.rt.spawn(engine.release_except(keep));
        }
    }

    /// Drop a pending file picker and stop its torrent, which nothing else will play.
    pub(crate) fn clear_file_pick(&mut self) {
        if self.file_pick.take().is_some() {
            self.release_torrents(self.player.as_ref().and_then(|p| p.torrent));
        }
    }

    /// Title being opened, while its request is the current navigation and unanswered.
    pub(crate) fn loading(&self) -> Option<&str> {
        self.opening.as_ref().filter(|(nav, _)| *nav == self.nav).map(|(_, t)| t.as_str())
    }

    /// Leave the open title's page, dropping replies still on their way to it.
    pub(crate) fn close_details(&mut self) {
        (self.selected, self.details) = (None, None);
        (self.episodes, self.episode) = (Vec::new(), None);
        self.scroll_top = true;
        self.begin_nav();
    }

    /// Start a navigation (open a title or episode, play, Back): replies for earlier ones are dropped, a file pick is closed.
    pub(crate) fn begin_nav(&mut self) -> u64 {
        self.clear_file_pick();
        self.nav += 1;
        self.nav
    }

    /// URLs of installed addons serving `res` for this type and id.
    pub(crate) fn addons_for(&self, res: &str, kind: &str, id: &str) -> Vec<String> {
        let urls: Vec<String> = self.addons.iter().filter(|(_, m)| m.serves(res, kind, id)).map(|(u, _)| u.clone()).collect();
        order_urls(urls, self.addon_orders.get(kind).map_or(&[][..], |o| o))
    }

    /// Fetch a movie's full meta for the detail page from the first addon that serves it.
    pub(crate) fn fetch_details(&mut self, ctx: &egui::Context, meta: &MetaPreview) {
        let addons = self.addons_for("meta", &meta.kind, &meta.id);
        let (client, tx, ctx) = (self.client.clone(), self.tx.clone(), ctx.clone());
        let (kind, id) = (meta.kind.clone(), meta.id.clone());
        self.details_want = (kind.clone(), id.clone());
        // Not `spawn`: the page works without details, so a failure is not worth an error.
        self.rt.spawn(async move {
            if let Ok(m) = client.first_meta(&addons, &kind, &id, |_| true).await {
                let _ = tx.send(Msg::Details(m));
                ctx.request_repaint();
            }
        });
    }

    /// Full meta of the selected title, once loaded.
    pub(crate) fn selected_details(&self) -> Option<&Meta> {
        let (m, ..) = self.selected.as_ref()?;
        self.details.as_ref().filter(|d| d.id == m.id && d.kind == m.kind)
    }

    /// Progress key and player title of the selected movie or episode.
    /// Episodes are keyed `<type>:<video id>` (e.g. `series:tt1:1:2`), so each episode resumes on its own.
    pub(crate) fn play_key(&self) -> Option<(String, String)> {
        let (m, ..) = self.selected.as_ref()?;
        Some(match &self.episode {
            Some(v) => {
                let se = format!("{} S{}E{}", m.name, v.season.unwrap_or(0), v.episode.unwrap_or(0));
                (key::item(&m.kind, &v.id), v.title.as_ref().map_or(se.clone(), |t| format!("{se} - {t}")))
            }
            None => (key::item(&m.kind, &m.id), m.name.clone()),
        })
    }

    /// Whether the player shows the selected series episode (not e.g. a Continue Watching entry).
    pub(crate) fn playing_episode(&self) -> bool {
        self.episode.is_some() && self.player.as_ref().zip(self.play_key()).is_some_and(|(p, (k, _))| p.key == k)
    }

    /// Remember the episode's series (its key holds only the video id) and the stream's binge group
    /// (empty if none), so the series' play button and Continue Watching can find them later.
    pub(crate) fn remember_episode_stream(&self, key: &str, source: &str) {
        let (Some((m, streams, _)), Some(_)) = (&self.selected, &self.episode) else { return };
        if !self.play_key().is_some_and(|(k, _)| k == key) {
            return;
        }
        let _ = settings::EPISODE_SERIES.set_at(&self.db, key, &Some(m.id.clone()));
        // A resume has no stream list; it must not erase the group it is still playing from.
        if let Some(s) = streams.iter().find(|s| s.source().as_deref() == Some(source)) {
            let _ = settings::BINGE_GROUP.set_at(&self.db, &key::item(&m.kind, &m.id), &s.hints.binge_group);
        }
    }

    /// The binge group this series last played from.
    pub(crate) fn saved_binge(&self, m: &MetaPreview) -> Option<String> {
        settings::BINGE_GROUP.get_at(&self.db, &key::item(&m.kind, &m.id)).ok().flatten()
    }

    /// Series load their episode list; anything else loads streams for its own id.
    pub(crate) fn open_meta(&mut self, ctx: &egui::Context, meta: MetaPreview, autoplay: bool) {
        // Opening any title cancels a play/show request still waiting for episodes.
        self.pending = None;
        if meta.kind != "series" {
            self.fetch_details(ctx, &meta);
            return self.open_streams(ctx, meta, None, if autoplay { Autoplay::Only } else { Autoplay::Off });
        }
        let client = self.client.clone();
        let addons = self.addons_for("meta", &meta.kind, &meta.id);
        let nav = self.begin_nav();
        self.opening = Some((nav, meta.name.clone()));
        self.spawn(ctx, async move {
            match client.first_meta(&addons, &meta.kind, &meta.id, |m| !m.videos.is_empty()).await {
                Ok(m) => Ok(Msg::Episodes(meta, m, nav)),
                Err(errors) => {
                    let why = if errors.is_empty() { "no addon lists them".to_string() } else { errors.join("; ") };
                    Ok(Msg::NoEpisodes(meta.id, format!("no episodes for {}: {why}", meta.name), nav))
                }
            }
        });
    }

    /// Card play button: resume the title (or a series' episode in progress) with its last stream,
    /// else play the top stream of the movie or of the series' next episode (its last binge group first).
    pub(crate) fn quick_play(&mut self, ctx: &egui::Context, meta: MetaPreview) {
        let key = key::item(&meta.kind, &meta.id);
        if let Some(p) = self.recent.iter().find(|p| p.key == key).cloned() {
            return self.resume(ctx, p);
        }
        self.tab = Tab::Home;
        if meta.kind == "series" {
            let id = meta.id.clone();
            self.open_meta(ctx, meta, false);
            self.pending = Some((id, OnEpisodes::Next));
            return;
        }
        self.fetch_details(ctx, &meta);
        self.open_streams(ctx, meta, None, Autoplay::Top(None));
    }

    /// `stremio://` link: prefill an addon for the user to confirm with Install, or open a title's page.
    pub(crate) fn open_link(&mut self, ctx: &egui::Context, link: DeepLink) {
        match link {
            DeepLink::Addon(url) => {
                // Invalidate in-flight play/streams/episodes replies so they can't act over the Addons tab.
                self.begin_nav();
                self.pending = None;
                (self.tab, self.addon_url) = (Tab::Addons, url.into());
            }
            DeepLink::Detail { kind, id, video } => {
                self.tab = Tab::Home;
                let meta = MetaPreview { name: id.clone(), id, kind, ..Default::default() };
                // Only an episode link opens a stream picker; `(id, None)` would autoplay the next episode.
                let pending = video.filter(|_| meta.kind == "series").map(|v| (meta.id.clone(), OnEpisodes::Pick(v)));
                self.open_meta(ctx, meta, false);
                self.pending = pending;
            }
        }
    }

    /// Continue Watching card: the title's page with its stream picker (for a series, the episode's).
    /// Torrents and local files have no page, so they resume.
    pub(crate) fn open_progress(&mut self, ctx: &egui::Context, p: Progress) {
        let Some(meta) = self.progress_meta(&p) else {
            return self.play(ctx, p.key, p.title, p.source, vec![]);
        };
        let video = key::addon_item(&p.key).map(|(_, id)| id.to_owned());
        let pending = video.filter(|_| meta.kind == "series").map(|v| (meta.id.clone(), OnEpisodes::Pick(v)));
        self.open_meta(ctx, meta, false);
        self.pending = pending;
    }

    /// Resume a Continue Watching entry with its saved stream. A series episode loads its series first,
    /// so the player knows the next episode for autoplay and the up-next card.
    pub(crate) fn resume(&mut self, ctx: &egui::Context, p: Progress) {
        match self.progress_meta(&p).filter(|m| m.kind == "series") {
            Some(meta) => {
                self.tab = Tab::Home;
                let id = meta.id.clone();
                self.open_meta(ctx, meta, false);
                self.pending = Some((id, OnEpisodes::Resume(p)));
            }
            None => self.play(ctx, p.key, p.title, p.source, vec![]),
        }
    }

    /// The title a Continue Watching entry belongs to (the series, for an episode); `None` for torrents and local files.
    fn progress_meta(&self, p: &Progress) -> Option<MetaPreview> {
        let (kind, id) = key::addon_item(&p.key)?;
        let mut meta = MetaPreview { id: id.into(), kind: kind.into(), name: p.title.clone(), poster: p.poster.clone(), ..Default::default() };
        if kind == "series" {
            let saved = settings::EPISODE_SERIES.get_at(&self.db, &p.key).ok().flatten();
            meta.id = saved.unwrap_or_else(|| series_id(id).into());
        }
        Some(meta)
    }

    /// Fetch streams and subtitles for a movie, or for `episode` of a series.
    pub(crate) fn open_streams(&mut self, ctx: &egui::Context, meta: MetaPreview, episode: Option<Video>, autoplay: Autoplay) {
        let id = episode.as_ref().map_or(meta.id.clone(), |v| v.id.clone());
        let fetch = self.fetch_streams(&meta.kind, &id);
        let nav = self.begin_nav();
        self.opening = Some((nav, episode.as_ref().and_then(|v| v.title.clone()).unwrap_or_else(|| meta.name.clone())));
        self.spawn(ctx, async move {
            let (streams, subs) = fetch.await?;
            Ok(Msg::Streams(meta, episode, streams, subs, autoplay, nav))
        });
    }

    /// Streams (in addon priority order, tagged with the addon's name) and subtitles of `kind`/`id` from every addon serving them.
    pub(crate) fn fetch_streams(&self, kind: &str, id: &str) -> impl Future<Output = anyhow::Result<(Vec<Stream>, Subs)>> + Send + 'static {
        let client = self.client.clone();
        let stream_addons = self.addons_for("stream", kind, id);
        let names: Vec<String> = stream_addons.iter().map(|u| self.addons.iter().find(|(x, _)| x == u).map(|(_, m)| m.name.clone()).unwrap_or_default()).collect();
        let sub_addons = self.addons_for("subtitles", kind, id);
        let (kind, vid) = (kind.to_owned(), id.to_owned());
        async move {
            let (mut streams, mut subs, mut errors) = (Vec::new(), Vec::new(), Vec::new());
            let ask_streams = |u: String| {
                let (c, k, i) = (client.clone(), kind.clone(), vid.clone());
                async move { c.streams(&u, &k, &i).await }
            };
            let ask_subs = |u: String| {
                let (c, k, i) = (client.clone(), kind.clone(), vid.clone());
                async move { c.subtitles(&u, &k, &i, &[]).await }
            };
            let (stream_res, sub_res) = tokio::join!(concurrently(stream_addons, ask_streams), concurrently(sub_addons, ask_subs));
            // Addon priority order, so the picker groups streams by addon.
            for (res, name) in stream_res.into_iter().zip(names) {
                match res {
                    Ok(s) => streams.extend(s.into_iter().map(|s| Stream { addon: name.clone(), ..s })),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            // Subtitles are optional; a failing subtitle addon must not block playback.
            for s in sub_res.into_iter().flatten() {
                subs.extend(s.into_iter().map(|s| (s.url, s.lang)));
            }
            if streams.is_empty() && !errors.is_empty() {
                anyhow::bail!("no streams: {}", errors.join("; "));
            }
            Ok((streams, subs))
        }
    }
}
