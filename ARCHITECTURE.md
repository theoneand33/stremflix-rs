# Architecture

```
src/addon.rs    Stremio addon protocol: types, sanitizing parsers, async HTTP client
src/db.rs       SQLite (rusqlite, bundled): addons, library folders/items, settings, progress
src/library.rs  Recursive media-file scan (walkdir)
src/player.rs   mpv process + JSON IPC (time-pos / duration polling)
src/torrent.rs  librqbit session + its HTTP streaming API on 127.0.0.1
src/stremio_import.rs  Reads a local Stremio's localStorage (WebKit SQLite or Chromium LevelDB via rusty-leveldb)
src/main.rs     egui/eframe GUI; tokio runtime for network/torrent work, results via mpsc
test-addon/     Std-only HTTP addon server (lib + bin). The bin can also seed via librqbit.
```

## Choices

**UI: egui (eframe 0.37).** Immediate-mode UI keeps the app to one small file with no widget-state plumbing. It builds with only system GL/X11/Wayland libraries, with no GTK or webview dependency. It runs on Xvfb. eframe also exposes an AccessKit tree, which is useful for automated GUI checks. Iced is similar. Slint would need a separate markup language. Dioxus desktop would pull in a webview and make native video harder.

**Playback: external `mpv` driven over JSON IPC.** mpv plays nearly every container and codec, as well as HTTP URLs with Range seeking and external subtitle files (`--sub-file`). It needs no Rust FFI. The app reads `time-pos` and `duration` once per second over the IPC socket and saves them to SQLite. Resuming passes `--start=<pos>`. The same socket lets tests and CI check player state from outside the app.

**Torrents: librqbit 9 (embedded).** It is a pure-Rust BitTorrent engine with magnet/DHT support and sequential, streamable file reads. Its built-in HTTP API (`/torrents/{id}/stream/{file}`, Range-aware) is bound to 127.0.0.1 on a random port. This turns any torrent into an HTTP URL that mpv already knows how to play, so the player sees only one kind of input.

## Awkward pairings (stated honestly)

- **egui + video surface.** egui renders through its own GL/wgpu painter and has no native video widget. The options are (a) libmpv's render API into an egui texture or GL callback, which needs unsafe FFI, a libmpv dev dependency, and GL context sharing with eframe's glow backend, or (b) mpv's `--wid` embedding, which only works on X11 and breaks on Wayland/macOS. For the MVP, mpv opens **its own window** and the egui window acts as the browser and remote. This is the main UX compromise. libmpv rendering into an eframe `PaintCallback` is the upgrade path.
- **librqbit + tokio vs. egui's synchronous loop.** The app owns a tokio runtime. UI events spawn async tasks, and results return over `std::sync::mpsc` with `ctx.request_repaint()`. The UI thread never blocks.
- **librqbit's HTTP API** needs the `http-api` feature and also `tracing-subscriber-utils` (librqbit 9.1 fails to compile without it). It also needs `librqbit-dualstack-sockets` for its listener type.
- **mpv IPC on Windows** uses named pipes. Only Unix sockets are implemented, so Windows playback-progress tracking is unsupported for now.

## Protocol notes

- An addon is identified by its transport URL ending in `/manifest.json`. Resources are fetched from `<base>/<resource>/<type>/<id>.json`, with type and id percent-encoded.
- Streams: `url` (http/https or `magnet:`) or `infoHash` + `fileIdx` + `sources`. `tracker:<url>` sources become `&tr=` in the magnet. `peer:<host:port>` is a **local extension** (becomes `x.pe=`) that the test addon uses to point at its seeder without a tracker or DHT. `externalUrl`/`ytId` streams are ignored.
- `fileIdx` is carried through saved progress as BEP 53 `&so=<idx>`.

## Untrusted addon output

- Response bodies are capped at 4 MiB. Requests time out after 15 s and follow at most 3 redirects.
- Parsers never panic. Structurally invalid responses are errors. Individually invalid catalog, stream, or subtitle entries are dropped.
- Text has control and bidi-override characters stripped, and lengths are capped. egui renders plain text only, so markup cannot inject anything.
- URLs must be `http`/`https` with a host. `file:`, `javascript:`, `data:`, and similar schemes are rejected. Info hashes must be 40 hex characters. Ids may not contain `/` or control characters.
- The media source is passed to mpv after `--`, so it can never be read as an option. `--no-ytdl` stops mpv from running youtube-dl on addon URLs.

## Persistence

SQLite in WAL mode at `$STREMIO_LITE_DATA/stremio-lite.db`. Progress keys are `<type>:<id>` for addon items, `file:<path>` for local files, and `btih:<hash>` for raw magnets. Each row stores a resumable source: URL, magnet, or path.
