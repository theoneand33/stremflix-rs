# Architecture

```
src/addon.rs    Stremio addon protocol: types, sanitizing parsers, async HTTP client
src/db.rs       SQLite (rusqlite, bundled): addons, library folders/items, settings, progress
src/library.rs  Recursive media-file scan (walkdir)
src/download.rs Offline downloads: safe file names, streaming a URL (or the local torrent stream) to a file
src/player.rs   libmpv (FFI) in-process: OpenGL-rendered frames, observed properties, IPC server
src/torrent.rs  librqbit session + its HTTP streaming API on 127.0.0.1
src/tray.rs     Tray icon (Show/Hide, Play/Pause, Quit): ksni StatusNotifierItem over D-Bus on Linux (no GTK), tray-icon on Windows/macOS
src/stremio_import.rs  Reads a local Stremio's localStorage (WebKit SQLite or Chromium LevelDB via rusty-leveldb)
src/main.rs     egui/eframe GUI; tokio runtime for network/torrent work, results via mpsc
test-addon/     Std-only HTTP addon server (lib + bin). The bin can also seed via librqbit.
```

## Choices

**UI: egui (eframe 0.37, glow/OpenGL backend).** Immediate-mode UI keeps the app to one small file with no widget-state plumbing. It builds with only system GL/X11/Wayland libraries, with no GTK or webview dependency. It runs on Xvfb. eframe also exposes an AccessKit tree, which is useful for automated GUI checks. Iced is similar. Slint would need a separate markup language. Dioxus desktop would pull in a webview and make native video harder.

**Playback: libmpv embedded in-process.** mpv plays nearly every container and codec, as well as HTTP URLs with Range seeking and external subtitle files (`sub-add`). The app loads libmpv at runtime with `libloading`, on a background thread after the first frames (linking it put ~150 ms of library loading before `main`), and uses its OpenGL render API to draw each frame into a framebuffer the app owns. The framebuffer's texture is registered with egui and drawn under the app's own Netflix-style controls, so scaling and color conversion run on the GPU. Hardware decoding uses only zero-copy GL-interop decoders (`nvdec,vaapi,videotoolbox,d3d11va`) and falls back to software decoding. Copy-back decoders such as `vulkan-copy` produced corrupted frames when the Vulkan device differed from the GL one. Player state (`time-pos`, `duration`, `pause`, volume, tracks) arrives as observed-property events, so the UI thread never blocks on mpv. Position is saved to SQLite once per second; resuming sets `start`. mpv's JSON IPC server stays enabled over a Unix socket, or the `\\.\pipe\stremflix-rs-mpv` named pipe on Windows, so tests and CI can still check player state from outside the app.

**Torrents: librqbit 9 (embedded).** It is a pure-Rust BitTorrent engine with magnet/DHT support and sequential, streamable file reads. Its built-in HTTP API (`/torrents/{id}/stream/{file}`, Range-aware) is bound to 127.0.0.1 on a random port. This turns any torrent into an HTTP URL that mpv already knows how to play, so the player sees only one kind of input.

## Awkward pairings (stated honestly)

- **egui + video surface.** egui renders through its own GL/wgpu painter and has no native video widget. The options are (a) libmpv's render API into an egui texture or GL callback, which needs unsafe FFI, libmpv at runtime, and GL context sharing with eframe's glow backend, or (b) mpv's `--wid` embedding, which only works on X11 and breaks on Wayland/macOS. eframe runs on its glow (OpenGL) backend instead of the default wgpu, so libmpv can render into eframe's own GL context. eframe exposes that context's `get_proc_address`, and libmpv uses it to load GL functions. mpv renders during `App::ui` into an app-owned framebuffer at on-screen pixel size. The app then resets the GL state that mpv changes and egui's painter relies on: framebuffer, active texture, program, VAO, unpack alignment, scissor and blend. This works on X11, Wayland, macOS and Xvfb (Mesa llvmpipe).
- **librqbit + tokio vs. egui's synchronous loop.** The app owns a tokio runtime. UI events spawn async tasks, and results return over `std::sync::mpsc` with `ctx.request_repaint()`. The UI thread never blocks.
- **Proxy (Settings).** Read once at startup into `addon::PROXY` (changes apply on restart); SOCKS5 is sent as `socks5h` so DNS goes through it. Every reqwest client (addons, update check, downloads) uses it except for loopback. An HTTP proxy also becomes mpv's `http-proxy` for remote sources, but librqbit only supports SOCKS5. A SOCKS5 proxy covers librqbit peers and HTTP trackers, but not DHT or UDP trackers, and not mpv, because ffmpeg has no SOCKS support. librqbit passes SOCKS credentials to the proxy still percent-encoded, so torrent proxy auth fails if the username or password needs escaping. The local torrent stream is never proxied.
- **New-episode notifications.** The Home New Episodes fetch also runs at startup and every 6 hours from `logic` (so while hidden to the tray). `fresh_episodes` keeps a capped set of `<series id>:<episode id>` keys plus a `<series id>:` marker in the `notified_eps` setting; a series with no marker is only recorded, so first runs and newly saved series do not notify for their backlog. Delivery is an in-app banner plus notify-rust (zbus on Linux, WinRT toasts on Windows, mac-notification-sys on macOS); a missing notification service only logs. Setting `notify_eps`.
- **Minimize to tray vs. egui's loop.** A hidden window gets no `App::ui`, only `App::logic`, so messages, tray clicks and player polling run in `logic`. mpv's frames aren't rendered meanwhile, which would stall its core, so the video track is off (`vid=no`) while hidden and restored when shown. Without a tray host (no D-Bus, no StatusNotifierWatcher) closing quits as before. Wayland can't hide windows, so the window minimizes there.
- **OS media controls: souvlaki** (MPRIS on Linux, SMTC on Windows via the window's HWND, MPNowPlaying on macOS). Its `use_zbus` feature talks D-Bus in pure Rust, so there is no libdbus build dependency. Media-key events come back over the same `mpsc` channel with `ctx.request_repaint()`. Without a D-Bus session (CI, Xvfb) its MPRIS thread fails, logs, and the app runs on without controls.
- **librqbit's HTTP API** needs the `http-api` feature and also `tracing-subscriber-utils` (librqbit 9.1 fails to compile without it). It also needs `librqbit-dualstack-sockets` for its listener type.

## Protocol notes

- An addon is identified by its transport URL ending in `/manifest.json`. Resources are fetched from `<base>/<resource>/<type>/<id>.json`, with type and id percent-encoded.
- Streams: `url` (http/https or `magnet:`) or `infoHash` + `fileIdx` + `sources`. `tracker:<url>` sources become `&tr=` in the magnet. `peer:<host:port>` is a **local extension** (becomes `x.pe=`) that the test addon uses to point at its seeder without a tracker or DHT. An http(s) `externalUrl` opens in the browser. A `ytId` (11 characters of `[A-Za-z0-9_-]`) becomes `https://www.youtube.com/watch?v=<id>`. `behaviorHints.skip` is a **local extension**: `[{"type":"intro"|"recap"|"credits","start":<s>,"end":<s>}]` in seconds, shown as a "Skip …" button while playing. Entries need finite `0 <= start < end`, at most 15 min long; invalid ones are dropped, at most 10 kept. Chapters titled intro/opening/OP, recap/previously, or credits/ending/ED in the media itself do the same. For IMDb-id titles the player also asks [TheIntroDB](https://theintrodb.org/) `GET /v4/media` once the duration is known (no key; Settings can turn it off); its spans come last, after addon and chapter ones.
- `behaviorHints.notWebReady` only means Stremio-web needs the streaming server's proxy. mpv plays the stream directly, so the hint just adds a "Not web-ready" note to the stream row. `proxyHeaders.response` is ignored except a `Content-Type` of HLS (`application/vnd.apple.mpegurl`, `application/x-mpegurl`, `audio/(x-)mpegurl`) or DASH (`application/dash+xml`), which sets mpv's `demuxer-lavf-format` (`hls`/`dash`) for that stream. mpv sniffs content and ignores other response headers.
- `fileIdx` is carried through saved progress as BEP 53 `&so=<idx>`.
- Series: `meta.videos` gives the episode list and streams are requested with the episode id (`tt…:S:E`), so each episode resumes on its own. `behaviorHints.bingeGroup` marks streams from the same source, and the player autoplays the next episode from the same group.
- Search uses the catalog `search` extra (`<base>/catalog/<type>/<id>/search=<query>.json`) and only targets catalogs whose required extras are satisfied.
- Stremio account (`api.strem.io`): sync pulls `addonCollectionGet` and `datastoreGet` of `libraryItem`. Upload (Settings) sends `addonCollectionSet` with the local addon order as `{transportUrl, transportName, manifest, flags}` (account-only addons kept last, account flags kept) and `datastorePut` of only the changed `libraryItem`s with `_mtime` set to now. Account-only items are never deleted, and saved titles already on the account (even removed) are not touched; progress replaces the account's only when newer than its `lastWatched`.

## Untrusted addon output

- Response bodies are capped at 4 MiB. Requests time out after 15 s and follow at most 3 redirects.
- Parsers never panic. Structurally invalid responses are errors. Individually invalid catalog, stream, or subtitle entries are dropped.
- Text has control and bidi-override characters stripped, and lengths are capped. egui renders plain text only, so markup cannot inject anything.
- URLs must be `http`/`https` with a host. `file:`, `javascript:`, `data:`, and similar schemes are rejected. Info hashes must be 40 hex characters. Ids may not contain `/` or control characters.
- The media source is passed to mpv as a `loadfile` argument, so it can never be read as an option. `ytdl` is off except when the source is exactly a valid YouTube watch URL (`https://www.youtube.com/watch?v=<11-char id>`, such as the one built from a `ytId`), so mpv never runs yt-dlp on other URLs.

## Persistence

SQLite in WAL mode at `$STREMFLIX_RS_DATA/stremflix-rs.db`. Progress keys are `<type>:<id>` for addon items, `file:<path>` for local files, and `btih:<hash>` for raw magnets. Each row stores a resumable source: URL, magnet, or path.

Settings > Data exports addons, saved titles, progress, watched marks and safe settings to `exports/stremflix-rs-<time>.json` (`{"format":"stremflix-rs-export","version":1}`; no auth key, no local paths). Import treats the file as untrusted (16 MiB cap, validated entries) and merges it in one transaction, keeping newer progress.

Saved titles (`saved_titles`) are the default library. User lists live in `lists` (name unique ignoring case) and `list_items` (`list_id`, saved-title key; cascades when a list is deleted); adding to a list saves the title, and unsaving removes it from every list.

Downloads go to `$STREMFLIX_RS_DATA/downloads/`, named from the sanitized title (never from addon or torrent file paths), written to `<name>.part` and renamed when complete. The `downloads` table keeps path, type, meta id, video id and title; playing one uses the streaming progress key `<type>:<video id>`. A downloading torrent is pinned so other playback doesn't stop it. Downloads form a queue (the same table, `status` queued/active/paused/failed, plus `source` and `headers`): at most `MAX_ACTIVE` (2) transfer, the rest wait. Pause stops the transfer, flushes and keeps the `.part`, and the job cannot restart until its task has reported. The row stores the ETag (else Last-Modified) and total size seen at start; Resume sends `Range: bytes=<part length>-` with `If-Range` and continues only on a 206 whose `Content-Range` starts there with the same total, otherwise (or with no validator) restarts. Torrent downloads pause by dropping the stream and unpinning, and resume by re-adding the magnet. Progress reaches the UI about 4 times a second; active rows reload as paused after a restart.
