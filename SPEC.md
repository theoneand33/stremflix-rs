# Spec: Stremio-like desktop client (MVP)

## Goal
Build a desktop client that does what Stremio does at MVP level: install Stremio addons, browse and play what they serve (including torrent streams), and play a local media library, with playback progress persisting across restarts.

## Machine-verifiable acceptance (what `/goal` checks)
1. `cargo build` exits 0.
2. `cargo test` exits 0, with tests covering:
   - parsing valid and malformed addon manifests;
   - parsing catalog, meta, stream, and subtitle responses;
   - rejecting malformed addon responses without panicking;
   - SQLite persistence of addons, library entries, settings, and watch progress across reopen;
   - local library scanning of a fixture folder.
3. A bundled test addon in `test-addon/` serves valid manifest/catalog/meta/stream/subtitle responses; at least one integration test starts it and exercises the client's addon client against it.
4. README documents build, run, test, and end-to-end demo steps.
5. Architecture note states the chosen Rust UI library, the playback backend, and the torrent engine, with reasoning, including any awkward pairing.
6. No hardcoded piracy sources, no DRM circumvention, no bundled copyrighted content.
7. `git status` is clean after the final commit.

## GUI acceptance (run through the computer-use MCP on Xvfb; report status, do NOT claim met from `/goal`)
- Install a test addon by manifest URL, browse its catalog, pick a stream, play it.
- Play a legal magnet/torrent fixture.
- Add a local folder and play a file from it.
- Progress survives app restart.

For each GUI step the transcript must show:
(a) the exact command used to launch the app on the virtual display;
(b) accessibility-tree or OCR text confirming the expected screen appeared;
(c) the click/keystroke action taken;
(d) a player-state check — e.g. `mpv` IPC `get_property time-pos` > 0, or OCR showing a play state.

Raw screenshots alone are not sufficient evidence.

## Resources
- The Stremio addon protocol: manifest, catalog, meta, stream, subtitles.
- "Full plug-in support" means this protocol, not arbitrary native plugins.
- Legal fixtures only: public-domain HTTP streams, a legal torrent/magnet, sample local video files.

## Constraints
- Rust throughout. Choose the Rust UI library (egui, Iced, Slint, Dioxus, or similar) and state the choice and reasoning before building. The media stack (video surface embedding, torrent engine) constrains the viable UI library; say so if the pairing is awkward.
- Treat addon output as untrusted: validate URLs and sanitize addon-provided content.
- If a protocol detail, torrent engine, playback backend, or platform constraint is unclear, ask rather than guess.

## Deliverables
- Repo with README, short architecture note, and `test-addon/`.
- Tests as above.
- Demo steps reproducing the GUI acceptance list.

## Evidence rule
After each milestone, print the exact command you ran and its output/exit code in your response, so the evidence is in the transcript. Do not summarize test results without showing them.

## Non-goals
Cloning Stremio's backend, accounts, cloud sync, recommendations, or native plugin execution.