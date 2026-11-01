<img src="assets/icon.png" width="64" height="64" alt="">

# Scarlet

Scarlet (formerly Stremio Lite) is a minimal Stremio-style desktop client written in Rust. It installs [Stremio addons](https://github.com/Stremio/stremio-addon-sdk/blob/master/docs/protocol.md) by manifest URL, browses their catalogs, and plays their HTTP and torrent streams. It also plays files from local folders. Playback progress is saved in SQLite, so it survives restarts.

The client ships with no content sources. You install every addon yourself, and all media in this repo is generated locally.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design choices: egui UI, mpv playback, and librqbit torrents.

## Requirements

- Rust 1.85 or newer (edition 2024)
- libmpv (`libmpv.so.2` and headers, e.g. `pacman -S mpv` / `apt install libmpv-dev`), linked for the built-in player
- `ffmpeg`, only needed to generate demo media
- Linux or macOS (player IPC uses Unix sockets)

## Player controls

Playback runs inside the app window with Netflix-style controls that fade after 3 s without input.
Click the video or press Space/K to pause, ←/→ to skip 10 s, ↑/↓ for volume, M to mute, F or double-click for full screen, Esc to leave full screen or go back. 💬 opens the Audio & Subtitles panel.

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
| `STREMIO_LITE_DATA` | Data directory for the DB and torrent downloads. Default: `$XDG_DATA_HOME/stremio-lite` |
| `STREMIO_LITE_MPV_ARGS` | Extra mpv options, e.g. `--ao=null` for headless runs |
| `STREMIO_LITE_IPC` | mpv IPC socket path. Default: `$TMPDIR/stremio-lite-mpv.sock` |
| `STREMIO_LITE_NO_DHT` | Set to disable the BitTorrent DHT (offline/CI use) |

### Import from Stremio

**Addons → Import from Stremio** copies your installed addons, watched movies and episodes, and in-progress titles from a local Stremio install. Watched titles show in **Watch It Again** and progress shows in **Continue Watching**. Leave the path empty to auto-detect, or point it at a Stremio data folder. Stremio's files are read from a temp copy and never changed. Re-importing is safe: newer local progress is kept.

Auto-detect searches these locations:

| OS | Location (format) |
|---|---|
| Linux | `~/.var/app/com.stremio.Stremio`, `~/.local/share/*stremio*` (WebKit SQLite) |
| macOS | `~/Library/WebKit/*stremio*`, `~/Library/Application Support/*stremio*` (WebKit SQLite / Chromium LevelDB) |
| Windows | `%APPDATA%`, `%LOCALAPPDATA%`, `%LOCALAPPDATA%\Programs` `*stremio*` folders (Chromium LevelDB) |

Only the Linux Flatpak build (Stremio 5) has been tested against a real install; the other formats are covered by fixture tests.

## Test

```sh
cargo test
```

The tests cover:

- manifest, catalog, meta, stream, and subtitle parsing
- rejection of malformed and oversized responses. Every truncation of every sample is fed to every parser, and none may panic.
- URL safety: only `http`/`https` URLs and valid magnets are accepted
- SQLite persistence across reopen
- library scanning of `tests/fixtures/library`
- Stremio import from WebKit SQLite and Chromium LevelDB fixtures. `cargo test --test stremio_import -- --ignored --nocapture` also reads this machine's Stremio, into a temp DB.
- an integration test that starts `test-addon` in-process and runs the real addon client against it

## End-to-end demo

1. Generate legal demo media (an ffmpeg test pattern with a 440 Hz tone):
   ```sh
   ./scripts/make-fixtures.sh            # writes demo-media/
   ```
2. Start the test addon. It serves the HTTP stream and also seeds the same file over BitTorrent:
   ```sh
   cargo run -p test-addon -- --port 7070 --media-dir demo-media/addon --seed
   # prints: test addon: http://127.0.0.1:7070/manifest.json
   #         seeding ... as magnet:?xt=urn:btih:<hash>&dn=test-pattern.mp4&x.pe=127.0.0.1:<port>
   ```
3. Start the app with a fresh data directory:
   ```sh
   STREMIO_LITE_DATA=/tmp/stremio-lite-demo STREMIO_LITE_NO_DHT=1 cargo run --bin stremio-lite
   ```
4. **Install an addon and play from it.** Open **Addons**, enter `http://127.0.0.1:7070/manifest.json`, and click **Install**. Open **Home**, click the **Test Pattern** card in the **Test Addon - Test Movies** row, then **Play: Test Addon - Test pattern (HTTP)**. The built-in player opens with the test subtitle loaded.
5. **Play a torrent.** On **Home**, click **Play: Test Addon - Test pattern (torrent)**. Alternatively, open **Library**, paste the printed magnet into **Magnet link**, and click **Play magnet**. librqbit downloads the file from the local seeder and streams it to the player over `http://127.0.0.1:<port>/torrents/<id>/stream/<file>`.
6. **Play a local file.** Open **Library**, enter the absolute path of `demo-media/library`, click **Add folder**, then **Play test-pattern-local**.
7. **Check that progress persists.** Let a video play for a while, then close the app. Restart it with the same command. The **Continue Watching** row on **Home** shows the item with its saved position. Click its card to resume at that position.

You can query player state from outside the app at any time:

```sh
printf '{"command":["get_property","time-pos"]}\n' | socat - /tmp/stremio-lite-mpv.sock
# or: python3 -c 'import socket,json;s=socket.socket(socket.AF_UNIX);s.connect("/tmp/stremio-lite-mpv.sock");s.send(b"{\"command\":[\"get_property\",\"time-pos\"]}\n");print(s.recv(4096))'
```

## Legal

The client contains no hardcoded addons, catalogs, trackers, or other content sources, and it has no DRM circumvention. The demo video is generated by `ffmpeg testsrc2`. Only install addons and play content you have the rights to.
