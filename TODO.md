# TODO

Priority: **P0** blocks common use · **P1** expected by Stremio users · **P2** nice to have · **P3** low value or large scope.
Difficulty: **S** under a day · **M** a few days · **L** a week or more · **XL** a major project.

## Windows builds

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Windows installer or signed exe | P3 | M | Without signing, SmartScreen shows a warning. |

## Stremio features this client lacks

### Content and browsing

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Trailers (`trailers` / `ytId`) | P2 | M | Needs YouTube resolution; mpv with yt-dlp can play them. |
| Calendar of upcoming episodes for library series | P2 | M | Depends on the saved library. |
| New-episode notifications | P2 | M | Depends on the saved library. |

### Addons

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Browse community addons (`addon_catalog` resource) | P2 | M | |
| Reorder addons (sets stream and catalog priority) | P2 | S | |
| Addon detail view (description, types, resources, version) | P3 | S | |

### Streams

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| `externalUrl` streams (open in browser) | P2 | S | |
| `ytId` streams | P2 | S | Same yt-dlp path as trailers. |
| Group and sort the stream list by addon | P2 | S | |
| `.torrent` file and drag-and-drop magnet support | P3 | S | |

### Player

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Send `videoHash` / `videoSize` to subtitle addons (OpenSubtitles matching) | P2 | M | Needs the OpenSubtitles hash of the file. |
| Playback speed | P2 | S | |
| Open in an external player (VLC, mpv) | P2 | S | |
| Chromecast / DLNA casting | P3 | XL | |

### Settings and streaming server

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Settings page (languages, autoplay, player defaults) | P1 | M | A settings table exists in the DB, but there's no UI for it. |
| Torrent settings: cache size and location, connection profile | P2 | S | |
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
