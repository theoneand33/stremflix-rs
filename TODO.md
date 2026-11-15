# TODO

Priority: **P0** blocks common use · **P1** expected by Stremio users · **P2** nice to have · **P3** low value or large scope.
Size: **S** under a day · **M** a few days · **L** a week or more · **XL** a major project.

## Windows builds

| Item | Priority | Size | Notes |
|---|---|---|---|
| Windows installer or signed exe | P3 | M | Without signing, SmartScreen shows a warning. |

## Stremio features this client lacks

### Content and browsing

| Item | Priority | Size | Notes |
|---|---|---|---|
| Show the addon's `imdbRating` and a language flag on stream rows | P2 | S | Protocol already carries the rating; flags come from the stream name or title. Stremio Android shows flags (stremio-features #817). |
| Keep addon, catalog and filters when Discover switches Movies and Series | P2 | S | stremio-features #1957. |
| Hide the Calendar tab and other nav sections | P3 | S | stremio-features #1956. |

### Library and organization

| Item | Priority | Size | Notes |
|---|---|---|---|
| Bulk selection in the library | P3 | M | |
| Rename, hide, and reorder catalog rows per profile | P3 | M | Part of Stremio's profiles feature; do with profiles. |
| User profiles (household, child-safe) | P3 | L | Stremio now ships up to 6 profiles with optional PIN, kid profiles and per-profile addons (not on Linux). Most-requested missing Stremio feature (stremio-features #622). |

### Player

| Item | Priority | Size | Notes |
|---|---|---|---|
| Chromecast / DLNA casting | P3 | XL | |
| Picture-in-picture / mini player | P3 | M | egui viewport or a second native window. |
| Audio output device selection | P3 | M | mpv `audio-device`. |
| Volume normalization / boost | P3 | M | mpv audio filter (`dynaudnorm`). |
| Preload the next episode for gapless playback | P3 | M | |
| Smart subtitles: hide them when the audio track matches the subtitle language | P3 | S | stremio-features #1781. |
| Video shaders and picture filters (brightness, contrast, CRT) | P3 | S | mpv `glsl-shaders` and `brightness`/`contrast`; user-chosen shader paths only. #1789, #1961. |
| Audio passthrough (AC3/DTS/E-AC3) | P3 | S | mpv `audio-spdif`; for soundbars. |
| Buffer size and read-ahead setting | P2 | S | mpv `demuxer-max-bytes` and `cache-secs`; Stremio users ask for it (#1610, #1783). |

### Downloads and torrents

| Item | Priority | Size | Notes |
|---|---|---|---|
| Custom listen port / UPnP / NAT-PMP | P3 | M | |
| Per-file selection for multi-file torrents (season packs) | P3 | M | Pick which files to download; check what the picker already does first. |

### Settings and streaming server

| Item | Priority | Size | Notes |
|---|---|---|---|
| Transcoding for formats the player can't handle | P3 | L | mpv already plays almost everything, so this is mostly irrelevant. |
| Launch playback in a separate window | P3 | M | Stremio setting; egui multi-viewport. |

### Addons

| Item | Priority | Size | Notes |
|---|---|---|---|
| Remote/LAN streaming server | P2 | L | Stremio serves the library to other devices; casting is a separate item. |

### Accounts and sync

| Item | Priority | Size | Notes |
|---|---|---|---|
| Trakt integration (scrobbling, watched sync) | P3 | L | |
| Simkl / MyAnimeList / Letterboxd sync | P3 | L | After Trakt. |

### Platform and app

| Item | Priority | Size | Notes |
|---|---|---|---|
| UI translations (i18n) | P3 | M | |
| Auto-install updates | P3 | M | Release builds already show a download link for a newer build; downloading and replacing the binary is still missing. |
| Android, iOS, TV clients | P3 | XL | Out of scope for an egui desktop app. |
| Screen-reader labels and focus-order polish | P3 | M | AccessKit is enabled; coverage is partial. |
