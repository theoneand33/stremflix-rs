<img src="assets/icon.png" width="64" height="64" alt="">

# Scarlet

Scarlet is a desktop client for [Stremio addons](https://github.com/Stremio/stremio-addon-sdk/blob/master/docs/protocol.md), written in Rust. You install addons by manifest URL, browse and search their catalogs, and play their HTTP and torrent streams. It also plays video files from local folders. Playback progress is saved in SQLite and survives restarts.

The client ships with no content sources. You install every addon yourself. All media in this repo is generated locally.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design: egui UI, libmpv playback, and librqbit torrents. Planned work is in [TODO.md](TODO.md).

## Requirements

- Rust 1.85 or newer (edition 2024)
- libmpv (`libmpv.so.2` and headers, for example `pacman -S mpv` or `apt install libmpv-dev`), linked for the built-in player
- `ffmpeg`, only needed to generate demo media
- Linux, macOS, or Windows. Release archives cover `linux-x86_64`, `macos-arm64`, and `windows-x86_64`.

The Windows release zip includes `libmpv-2.dll`. To build from source on Windows, you need an MSVC build of libmpv. The Windows step in `.github/workflows/release.yml` shows the pinned build that releases use.

## Player controls

Playback runs inside the app window. The controls fade after 3 s without input.

Click the video or press Space or K to pause. Press ←/→ to skip 10 s, ↑/↓ for volume, and M to mute. Press F or double-click for full screen. Press Esc to leave full screen or go back. 💬 opens the Audio & Subtitles panel.

## Series

Open a series to pick a season and an episode. The ⏭ control in the player opens the next episode in the list.

If the stream you were watching has a `bingeGroup` (set by the addon in `behaviorHints`), the next episode autoplays a stream from the same group. Without a match, the app plays the only stream or shows the stream picker.

## Play button on posters

Hover a poster and press the play button in its middle to start playback. If the title is in Continue Watching, it resumes with the same stream. For a series, it resumes the episode in progress, or it starts the episode after the last one you watched. That episode uses a stream from the series' last `bingeGroup`, or the top stream if none matches. A movie with no progress plays its top stream. A click elsewhere on a Continue Watching poster opens the title's stream picker. For a series, that is the episode's stream picker.

## Search

The search pill in the header queries every installed addon catalog that supports the `search` extra. Results appear as a row on Home.

## Build

```sh
cargo build            # builds the app (stremio-lite) and test-addon
```

## Run

```sh
cargo run --bin stremio-lite
```

Environment variables:

| Variable | Purpose |
|---|---|
| `STREMIO_LITE_DATA` | Data directory for the DB and torrent downloads. Default: `$XDG_DATA_HOME/stremio-lite`, or `%APPDATA%\stremio-lite` on Windows. Without `XDG_DATA_HOME`, Unix uses `~/.local/share/stremio-lite`. |
| `STREMIO_LITE_MPV_ARGS` | Extra mpv options, e.g. `--ao=null` for headless runs |
| `STREMIO_LITE_IPC` | mpv IPC endpoint. Default: `$TMPDIR/stremio-lite-mpv.sock` on Unix, `\\.\pipe\stremio-lite-mpv` on Windows |
| `STREMIO_LITE_NO_DHT` | Set to disable the BitTorrent DHT (offline/CI use) |

### Import from Stremio

**Addons → Import from Stremio** copies your installed addons, watched movies and episodes, and in-progress titles from a local Stremio install. Watched titles appear in **Watch It Again**, and progress appears in **Continue Watching**. Leave the path empty to auto-detect, or point it at a Stremio data folder. The app reads Stremio's files from a temp copy and never changes them. You can re-import safely: newer local progress is kept.

Auto-detect searches these locations:

| OS | Location (format) |
|---|---|
| Linux | `~/.var/app/com.stremio.Stremio`, `~/.local/share/*stremio*` (WebKit SQLite) |
| macOS | `~/Library/WebKit/*stremio*`, `~/Library/Application Support/*stremio*` (WebKit SQLite / Chromium LevelDB) |
| Windows | `%APPDATA%`, `%LOCALAPPDATA%`, `%LOCALAPPDATA%\Programs` `*stremio*` folders (Chromium LevelDB) |

Only the Linux Flatpak build (Stremio 5) has been tested against a real install. Fixture tests cover the other formats.

## Test

```sh
cargo test
```

The tests cover:

- manifest, catalog, meta, stream, and subtitle parsing, including series `videos` and `bingeGroup`
- rejection of malformed and oversized responses. Every truncation of every sample goes to every parser, and none may panic.
- URL safety: only `http`/`https` URLs and valid magnets are accepted
- SQLite persistence across reopen
- library scanning of `tests/fixtures/library`
- Stremio import from WebKit SQLite and Chromium LevelDB fixtures. `cargo test --test stremio_import -- --ignored --nocapture` also reads this machine's Stremio into a temp DB.
- an integration test that starts `test-addon` in-process and runs the real addon client against it

## End-to-end demo

1. Generate demo media: an ffmpeg test pattern with a 440 Hz tone.
   ```sh
   ./scripts/make-fixtures.sh            # writes demo-media/
   ```
2. Start the test addon. It serves the HTTP stream and seeds the same file over BitTorrent:
   ```sh
   cargo run -p test-addon -- --port 7070 --media-dir demo-media/addon --seed
   # prints: test addon: http://127.0.0.1:7070/manifest.json
   #         seeding ... as magnet:?xt=urn:btih:<hash>&dn=test-pattern.mp4&x.pe=127.0.0.1:<port>
   ```
3. Start the app with a fresh data directory:
   ```sh
   STREMIO_LITE_DATA=/tmp/stremio-lite-demo STREMIO_LITE_NO_DHT=1 cargo run --bin stremio-lite
   ```
4. **Install an addon and play from it.** Open **Addons**, enter `http://127.0.0.1:7070/manifest.json`, and click **Install**. Open **Home**, click the **Test Pattern** card in the **Test Addon - Test Movies** row, then click **Play: Test Addon - Test pattern (HTTP)**. The built-in player opens with the test subtitle loaded.
5. **Play a torrent.** On **Home**, click **Play: Test Addon - Test pattern (torrent)**. You can also open **Library**, paste the printed magnet into **Magnet link**, and click **Play magnet**. librqbit downloads the file from the local seeder and streams it to the player over `http://127.0.0.1:<port>/torrents/<id>/stream/<file>`.
6. **Play a local file.** Open **Library**, enter the absolute path of `demo-media/library`, click **Add folder**, then click **Play test-pattern-local**.
7. **Check that progress persists.** Let a video play for a while, then close the app. Restart it with the same command. The **Continue Watching** row on **Home** shows the item at its saved position. Click the card to resume there.

You can query player state from outside the app while it runs. On Linux or macOS:

```sh
printf '{"command":["get_property","time-pos"]}\n' | socat - /tmp/stremio-lite-mpv.sock
# or: python3 -c 'import socket,json;s=socket.socket(socket.AF_UNIX);s.connect("/tmp/stremio-lite-mpv.sock");s.send(b"{\"command\":[\"get_property\",\"time-pos\"]}\n");print(s.recv(4096))'
```

On Windows, open the pipe `\\.\pipe\stremio-lite-mpv` instead.

## Releases

Release archives are named `stremio-lite-<tag>-<target>` and published on GitHub Releases:

| Target | Archive |
|---|---|
| `linux-x86_64` | `.tar.gz` (needs system libmpv), `.AppImage` (bundles libmpv) |
| `macos-arm64` | `.tar.gz` |
| `windows-x86_64` | `.zip` (includes `libmpv-2.dll`) |

A nightly build runs every 3 hours and is skipped when `main` has no new commits. Tagged releases come from a manual workflow run.

## Legal

The client contains no hardcoded addons, catalogs, trackers, or other content sources, and it has no DRM circumvention. The demo video is generated by `ffmpeg testsrc2`. Install only addons and play only content you have the rights to.
