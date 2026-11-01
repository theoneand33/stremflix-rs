# TODO

Priority: **P0** blocks common use · **P1** expected by Stremio users · **P2** nice to have · **P3** low value or large scope.
Difficulty: **S** under a day · **M** a few days · **L** a week or more · **XL** a major project.

## Windows builds

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Add a Windows target to the release matrix (nightly and stable) | P1 | M | `release.yml` builds only `linux-x86_64` and `macos-arm64`. Fetch a prebuilt mpv-dev archive in CI, link against it, and ship `mpv-2.dll` next to the exe. |
| Package Windows builds as `.zip` | P1 | S | The release step uploads only `*.tar.gz`, so it must also pick up `*.zip`. |
| Add `windows-latest` to `check.yml` | P1 | S | CI checks currently run only on Linux and macOS. |
| Use a named pipe for mpv IPC on Windows | P1 | S | `player.rs` defaults to a Unix socket in `$TMPDIR`. On Windows, use `\\.\pipe\stremio-lite-mpv`. |
| Use `%APPDATA%` as the default data dir on Windows | P1 | S | `data_dir()` falls back from `XDG_DATA_HOME` to `HOME`, and Windows has neither. |
| Windows installer or signed exe | P3 | M | Without signing, SmartScreen shows a warning. |

## Stremio features this client lacks

### Content and browsing

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Series: season and episode picker | P0 | M | `Meta` doesn't parse `videos`, and streams are requested with the series id instead of `tt…:S:E`, so most series streams return nothing. |
| Mark episodes or movies as watched/unwatched | P1 | S | Today the watched state comes only from the Stremio import. |
| Next episode / binge watching (autoplay next, `bingeGroup`) | P1 | M | Depends on the episode picker. |
| Discover page: browse a catalog by type and catalog, with genre filter | P1 | M | Needs the `genre` extra; catalogs that require a genre are skipped today. |
| Catalog paging (`skip` extra, infinite scroll) | P1 | S | Only the first page of each catalog loads. |
| Full detail page: genres, cast, director, runtime, IMDb rating, links, background | P1 | M | `Meta` parses only name, poster, description and release info. |
| Trailers (`trailers` / `ytId`) | P2 | M | Needs YouTube resolution; mpv with yt-dlp can play them. |
| Library of saved titles ("Add to library"), with sort and type filter | P1 | M | The current Library tab only holds local folders. |
| Calendar of upcoming episodes for library series | P2 | M | Depends on the saved library. |
| New-episode notifications | P2 | M | Depends on the saved library. |

### Addons

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Configure addons (`behaviorHints.configurable`, open `/configure`) | P1 | S | Open it in the system browser, then paste the result URL. |
| Browse community addons (`addon_catalog` resource) | P2 | M | |
| Reorder addons (sets stream and catalog priority) | P2 | S | |
| Addon detail view (description, types, resources, version) | P3 | S | |

### Streams

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Stream `behaviorHints`: `proxyHeaders`, `filename`, `videoSize`, `notWebReady` | P1 | M | Some debrid and HTTP addons fail without the request headers. |
| `externalUrl` streams (open in browser) | P2 | S | |
| `ytId` streams | P2 | S | Same yt-dlp path as trailers. |
| Group and sort the stream list by addon | P2 | S | |
| `.torrent` file and drag-and-drop magnet support | P3 | S | |

### Player

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Subtitle style: size, color, background, delay | P1 | S | mpv already has `sub-scale`, `sub-color` and `sub-delay`. |
| Default audio and subtitle language preference | P1 | S | Maps to mpv's `alang` and `slang`. |
| Send `videoHash` / `videoSize` to subtitle addons (OpenSubtitles matching) | P2 | M | Needs the OpenSubtitles hash of the file. |
| Playback speed | P2 | S | |
| Open in an external player (VLC, mpv) | P2 | S | |
| Chromecast / DLNA casting | P3 | XL | |

### Settings and streaming server

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Settings page (languages, autoplay, player defaults) | P1 | M | A settings table exists in the DB, but there's no UI for it. |
| Torrent settings: cache size and location, connection profile | P2 | S | |
| Clear the torrent cache | P2 | S | |
| Hardware decoding toggle | P2 | S | mpv `hwdec`. |
| Transcoding for formats the player can't handle | P3 | L | mpv already plays almost everything, so this is mostly irrelevant. |

### Accounts and sync

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Stremio account login, with addon and library sync (api.strem.io) | P2 | L | Today there's only a one-way import from a local install. |
| Trakt integration (scrobbling, watched sync) | P3 | L | |

### Platform and app

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| `stremio://` deep links (install an addon, open a title) | P2 | M | Needs OS protocol registration on each platform. |
| UI translations (i18n) | P3 | M | |
| Auto-update | P3 | M | Can check the nightly and stable GitHub releases. |
| Linux arm64 and macOS x86_64 builds | P3 | S | Add them to the release matrix. |
| Android, iOS, TV clients | P3 | XL | Out of scope for an egui desktop app. |
