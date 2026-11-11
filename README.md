<div align="center">
<img src="assets/icon.png" width="64" height="64" alt="">

# Stremflix-rs
![Home page with a hero title and a row of film posters](assets/screenshot.png)

<small> Screenshot titles are the Blender Foundation's open movies (CC BY 3.0 and 4.0), resized: Big Buck Bunny, Sintel, Tears of Steel, Elephants Dream, Cosmos Laundromat. </small>

</div>

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

- Rust 1.95+ (edition 2024)
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

Addons lists installed addons in priority order; Up/Down reorder them, and the stream list and Home rows follow. The All/Movies/Shows switch orders metadata, streams, and subtitles per type; catalog rows and search keep the global order. Each addon shows whether it is reachable, timed out, or unreachable. Details shows an addon's id, types, resources, and catalogs. Streams are grouped by addon. `externalUrl` streams open in the browser; `ytId` streams play through yt-dlp, as do the trailer buttons on a title's detail page (from `trailerStreams` or legacy `trailers`).

Press Enter in the header search to open a results page with one row for each catalog that supports the `search` extra. Rows fill in as addons reply.

### Playing

Hover a poster and press its play button. Continue Watching titles resume with the same stream; a series resumes the episode in progress or starts the next one. Clicking elsewhere on a poster opens the stream picker. The × in a Continue Watching card's corner removes it from the row.

For series, pick a season and episode. ⏭ in the player opens the next episode. If the current stream has a `bingeGroup`, the next episode autoplays a stream from the same group (toggle in Settings); otherwise the app plays the only stream or shows the picker.

### Player controls

| Key | Action |
|---|---|
| Space, K, click | Pause |
| ←/→, Shift+←/→ | Skip 10 s / 30 s (Settings) |
| ↑/↓, wheel, M | Volume, mute |
| I | Playback stats (codec, dropped frames, buffer; peers and speed for torrents) |
| ,/. | Step one frame back / forward (pauses) |
| F, double-click | Full screen |
| Esc | Leave full screen / back |

A Skip button appears over intros, recaps, and credits. Spans come from the addon, chapter names, or TheIntroDB (looked up by IMDb id; a Settings switch turns it off, since it sends what you watch). The player menu has a sleep timer (15, 30, 60, or 90 min) that pauses playback, and Settings can pause playback while the window is minimized. The bottom-right buttons open the Audio, Subtitles, and Playback panels. Playback sets speed, repeat file, an A-B loop (set A and B at the current position), hardware decoding, and can hand the stream to an external player (command set in Settings; quote paths with spaces). For torrents, stay on the player screen while the external player streams: going back stops the torrent.

### Torrents

Play a magnet or `.torrent` from Library → Torrent, drop a `.torrent` on the window, or paste a magnet with no text field focused (dragging magnet links doesn't work). Settings sets the torrent folder, a cache size, a connection profile (Soft 40 / Default 128 / Fast 300 peers), and download/upload speed limits. With no cache (the default), a torrent's files are deleted when playback stops. Finished torrents keep seeding after playback until a ratio or time limit is reached (both Off by default: stop at once). Settings lists torrents with a Delete button.

### `stremio://` links

The app takes a `stremio://` link as its first argument; a running instance receives it and comes to the front.

- `stremio://<host>/…/manifest.json` opens Addons with the URL filled in (https, or http for localhost). Confirm with Install.
- `stremio:///detail/<type>/<id>[/<episode id>]` opens the title, or the episode's stream picker.

```sh
stremflix-rs 'stremio:///detail/movie/tt0111161'
```

The same argument can be a magnet link, a `.torrent` path, or a local media file path: it plays in the running window (or a new one). `--help` prints usage; unknown flags exit with status 2. Use `--` before a file name that starts with `-`. Local media files open from the command line only (Finder file-open and non-UTF8 paths are not supported yet).

Scheme registration: the Linux AppImage declares it (installed by AppImageLauncher or appimaged); macOS picks it up once `Stremflix-rs.app` is in Applications or opened; Windows registers it per user at start unless another app (e.g. Stremio) owns it.

### Settings and recovery

The app reopens on the tab you last used. Settings sets the HTTP User-Agent. If the last run did not exit cleanly, the next launch shows a notice. Startup also checks the database and reports corruption; neither blocks startup.

### Import from Stremio

Addons → Import from Stremio copies addons, library titles (saved titles), watched titles (Watch It Again), and progress (Continue Watching) from a local Stremio install. It reads a temp copy and never changes Stremio's files. Re-importing keeps newer local progress. Leave the path empty to auto-detect:

| OS | Location (format) |
|---|---|
| Linux | `~/.var/app/com.stremio.Stremio`, `~/.local/share/*stremio*` (WebKit SQLite) |
| macOS | `~/Library/WebKit/*stremio*`, `~/Library/Application Support/*stremio*` (WebKit SQLite / Chromium LevelDB) |
| Windows | `*stremio*` folders in `%APPDATA%`, `%LOCALAPPDATA%`, `%LOCALAPPDATA%\Programs` (Chromium LevelDB) |

Only the Linux Flatpak (Stremio 5) is tested against a real install; fixtures cover the rest. `cargo test --test stremio_import -- --ignored --nocapture` reads this machine's Stremio into a temp DB.

### Stremio account

Settings → Stremio account signs in to api.strem.io and copies the account's addons, library (as saved titles), and watched titles, on sign-in, at startup, and with Sync. Sync is one-way: nothing is uploaded, already installed addons are kept, and deletions on the account aren't removed here. Addons and library titles you remove here, and titles or episodes you mark unwatched, stay that way after a sync or import. The auth key is stored in the local DB (owner-only on Linux and macOS); Sign out clears it there and tries to sign the key out on the server. Account progress needs the stream that was playing, which the account doesn't hold, so it isn't copied.

## Comparison with Stremio

Stremflix-rs against the official Stremio desktop app (Flathub `com.stremio.Stremio` 1.2.2), each launched 10 times on Linux with an i7-10700KF, 32 GB RAM, a GeForce GTX 1070 Ti (driver 580.196.07), and a 1920x1080 Wayland desktop (both apps ran through XWayland). Stremflix-rs was a release build of commit `9308832` with a fresh data directory. Stremio was signed out. Values are medians.

| | Stremflix-rs | Stremio | Difference |
|---|---|---|---|
| Time to first visible window | 0.08 s | 0.58 s | 7x faster |
| Memory 15 s after launch, RSS | 136 MB | 1229 MB | 9x less |
| Memory 15 s after launch, PSS | 86 MB | 1018 MB | 12x less |
| Processes | 1 | 8 | 8x fewer |
| CPU time in the first 15 s | 0.2 s | 4.5 s | 26x less |
| On disk | 42 MB binary (plus libmpv) | 134 MB Flatpak app (plus runtime) | 3x smaller |

Memory sums every process the app started. PSS splits shared pages between the processes that map them, so it is the fairer figure for Stremio's multi-process layout. "First visible window" is when a new window larger than 300x300 pixels is mapped, not when the first frame is painted, so it understates the time to a usable UI for both apps. Stremio loads its interface from the web, so its numbers also depend on the network. Stremio does more than this client: it bundles a streaming server and a full web UI. Nothing here measures playback, scrolling, search latency, or GPU load.

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

Stremflix-rs is an independent, community-developed project. It is not affiliated with, authorized, maintained, sponsored, or endorsed by SmartCode Ltd (the creators of the official Stremio application).
