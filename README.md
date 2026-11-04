<img src="assets/icon.png" width="64" height="64" alt="">

# Stremflix-rs

A Rust desktop client for [Stremio addons](https://github.com/Stremio/stremio-addon-sdk/blob/master/docs/protocol.md). Install addons by manifest URL, browse and search their catalogs, and play their HTTP and torrent streams or local video files. Playback progress is saved in SQLite.

The client ships with no content sources; you install every addon yourself. Design: [ARCHITECTURE.md](ARCHITECTURE.md) (egui, libmpv, librqbit). Planned work: [TODO.md](TODO.md).

## Install

Download a build from GitHub Releases (`stremflix-rs-<tag>-<target>`):

| Target | Archive |
|---|---|
| `linux-x86_64`, `linux-arm64` | `.AppImage` (bundles libmpv), `.tar.gz` (needs system libmpv) |
| `macos-arm64`, `macos-x86_64` | `.tar.gz` with `Stremflix-rs.app` |
| `windows-x86_64` | `.zip` (includes `libmpv-2.dll`) |

A nightly build runs every 3 hours when `main` has new commits; tagged releases are cut manually. Release builds check their channel on startup and link to newer builds.

## Build from source

Requirements:

- Rust 1.85+ (edition 2024)
- libmpv at runtime and for `cargo test` (`pacman -S mpv`, `apt install libmpv2`). On Windows, put `libmpv-2.dll` next to the exe or on `PATH`.
- Optional: `yt-dlp` on `PATH` for YouTube (`ytId`) streams and trailers, `ffmpeg` for demo media

```sh
cargo build                     # app (stremflix-rs) and test-addon
cargo run --bin stremflix-rs
cargo test
```

| Variable | Purpose |
|---|---|
| `STREMFLIX_RS_DATA` | Data directory for the DB and torrents. Default: `$XDG_DATA_HOME/stremflix-rs` (`~/.local/share/stremflix-rs`), or `%APPDATA%\stremflix-rs` on Windows |
| `STREMFLIX_RS_MPV_ARGS` | Extra mpv options, e.g. `--ao=null` for headless runs |
| `STREMFLIX_RS_IPC` | mpv IPC endpoint. Default: `$TMPDIR/stremflix-rs-mpv.sock`, or `\\.\pipe\stremflix-rs-mpv` on Windows |
| `STREMFLIX_RS_NO_DHT` | Disable the BitTorrent DHT (offline/CI) |

## Usage

### Addons and search

Addons lists installed addons in priority order; Up/Down reorder them, and the stream list and Home rows follow. Details shows an addon's id, types, resources, and catalogs. Streams are grouped by addon. `externalUrl` streams open in the browser; `ytId` streams play through yt-dlp, as do the trailer buttons on a title's detail page (from `trailerStreams` or legacy `trailers`).

The header search queries every catalog that supports the `search` extra. Results appear as a row on Home.

### Playing

Hover a poster and press its play button. Continue Watching titles resume with the same stream; a series resumes the episode in progress or starts the next one. Clicking elsewhere on a poster opens the stream picker. The × in a Continue Watching card's corner removes it from the row.

For series, pick a season and episode. ⏭ in the player opens the next episode. If the current stream has a `bingeGroup`, the next episode autoplays a stream from the same group (toggle in Settings); otherwise the app plays the only stream or shows the picker.

### Player controls

| Key | Action |
|---|---|
| Space, K, click | Pause |
| ←/→ | Skip 10 s |
| ↑/↓, M | Volume, mute |
| F, double-click | Full screen |
| Esc | Leave full screen / back |

The bottom-right buttons open the Audio, Subtitles, and Playback panels. Playback sets speed, hardware decoding, and can hand the stream to an external player (command set in Settings; quote paths with spaces). For torrents, stay on the player screen while the external player streams: going back stops the torrent.

### Torrents

Play a magnet or `.torrent` from Library → Torrent, drop a `.torrent` on the window, or paste a magnet with no text field focused (dragging magnet links doesn't work). Settings sets the torrent folder, a cache size, and a connection profile (Soft 40 / Default 128 / Fast 300 peers). With no cache (the default), a torrent's files are deleted when playback stops.

### `stremio://` links

The app takes a `stremio://` link as its first argument; a running instance receives it and comes to the front.

- `stremio://<host>/…/manifest.json` opens Addons with the URL filled in (https, or http for localhost). Confirm with Install.
- `stremio:///detail/<type>/<id>[/<episode id>]` opens the title, or the episode's stream picker.

```sh
stremflix-rs 'stremio:///detail/movie/tt0111161'
```

Scheme registration: the Linux AppImage declares it (installed by AppImageLauncher or appimaged); macOS picks it up once `Stremflix-rs.app` is in Applications or opened; Windows registers it per user at start unless another app (e.g. Stremio) owns it.

### Import from Stremio

Addons → Import from Stremio copies addons, watched titles (Watch It Again), and progress (Continue Watching) from a local Stremio install. It reads a temp copy and never changes Stremio's files. Re-importing keeps newer local progress. Leave the path empty to auto-detect:

| OS | Location (format) |
|---|---|
| Linux | `~/.var/app/com.stremio.Stremio`, `~/.local/share/*stremio*` (WebKit SQLite) |
| macOS | `~/Library/WebKit/*stremio*`, `~/Library/Application Support/*stremio*` (WebKit SQLite / Chromium LevelDB) |
| Windows | `*stremio*` folders in `%APPDATA%`, `%LOCALAPPDATA%`, `%LOCALAPPDATA%\Programs` (Chromium LevelDB) |

Only the Linux Flatpak (Stremio 5) is tested against a real install; fixtures cover the rest. `cargo test --test stremio_import -- --ignored --nocapture` reads this machine's Stremio into a temp DB.

### Stremio account

Settings → Stremio account signs in to api.strem.io and copies the account's addons, library (as saved titles), and watched titles, on sign-in, at startup, and with Sync. Sync is one-way: nothing is uploaded, and already installed addons are kept. Titles you remove from the library or mark unwatched here stay that way after a sync. The auth key is stored in the local DB (owner-only on Linux and macOS); Sign out forgets it. Account progress needs the stream that was playing, which the account doesn't hold, so it isn't copied.

## End-to-end demo

```sh
./scripts/make-fixtures.sh   # writes demo-media/ (ffmpeg test pattern + 440 Hz tone)
cargo run -p test-addon -- --port 7070 --media-dir demo-media/addon --seed   # serves HTTP and seeds a torrent; prints the magnet
STREMFLIX_RS_DATA=/tmp/stremflix-rs-demo STREMFLIX_RS_NO_DHT=1 cargo run --bin stremflix-rs
```

1. Addons: install `http://127.0.0.1:7070/manifest.json`.
2. Home: open Test Pattern in the Test Addon - Test Movies row and play the HTTP stream (subtitle loads), then the torrent stream. Or paste the printed magnet into Library → Torrent.
3. Library: add the absolute path of `demo-media/library` and play test-pattern-local.
4. Close and restart with the same command: Continue Watching resumes at the saved position.

Query player state while it runs: `printf '{"command":["get_property","time-pos"]}\n' | socat - /tmp/stremflix-rs-mpv.sock`.

## Legal

No hardcoded addons, catalogs, trackers, or other content sources, and no DRM circumvention. Demo media is generated by `ffmpeg testsrc2`. Install only addons and play only content you have the rights to.
