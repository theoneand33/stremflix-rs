# AGENTS.md

Stremflix-rs: a Rust desktop Stremio-addon client (egui, libmpv, librqbit). Project overview: [README.md](README.md). Module map and design choices: [ARCHITECTURE.md](ARCHITECTURE.md). Planned work: [TODO.md](TODO.md).

## Setup

- Rust 1.95+, edition 2024.
- libmpv is required at runtime and for `cargo test` (`apt install libmpv-dev` on Debian/Ubuntu, `pacman -S mpv` on Arch).
- Optional: `yt-dlp` (YouTube streams), `ffmpeg` (demo media via `scripts/make-fixtures.sh`).

## Commands

CI (`.github/workflows/check.yml`) runs these on Linux, macOS, and Windows. Run them before you push:

```sh
cargo build --workspace --all-targets --locked
cargo test --workspace --locked
```

Set `STREMFLIX_RS_NO_DHT=1` for tests and any headless or offline run. Set `STREMFLIX_RS_MPV_ARGS=--ao=null` when there is no audio device. Use `STREMFLIX_RS_DATA=<tmp dir>` so a run never touches the real database.

## Layout

- `src/main.rs`: egui GUI, tokio runtime, results back over mpsc. It is large; search before you read.
- `src/addon.rs`, `db.rs`, `player.rs`, `torrent.rs`, `download.rs`, `stremio_import.rs`, `tray.rs`, `widgets.rs`: see ARCHITECTURE.md.
- `src/widgets.rs` holds reusable widgets with no app state and no addon types. Keep it that way.
- `test-addon/`: std-only addon server used for end-to-end runs and tests.
- `tests/`: integration tests (addon parsing, persistence, Stremio import, instance, library scan).

## Rules

- Keep the app headless-friendly: no feature may require a display server beyond what Xvfb provides, a real audio device, or network access in tests. Use the test addon and fixtures.
- Never hardcode addons, catalogs, trackers, or other content sources. No DRM circumvention.
- Keep the UI thread free of blocking work: spawn on tokio and return results over mpsc with `ctx.request_repaint()`.
- Route every reqwest client through the proxy setup in `addon.rs`; loopback stays unproxied.
- New behavior gets a test in `tests/` or a unit test next to the code. Parsers of addon or Stremio data must sanitize input.
- Update README.md, ARCHITECTURE.md, or TODO.md when a change alters what they describe. Remove a TODO.md row when you finish the item.
- Do not commit `*.db`, `demo-media/`, `opencode.json`, or `.claude/worktrees/`.

## Pull requests

- One concern per PR. Note unrelated findings as follow-ups instead of fixing them.
- Conventional Commits for messages (`feat:`, `fix:`, `refactor:`, `docs:`).
