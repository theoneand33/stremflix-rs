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
| Calendar of upcoming episodes for library series | P2 | M | Depends on the saved library. |
| New-episode notifications | P2 | M | Depends on the saved library. |

### Library and organization

| Item | Priority | Size | Notes |
|---|---|---|---|
| Bulk selection in the library | P3 | M | |
| User profiles (household, child-safe) | P3 | L | Stremio supporter feature. |

### Player

| Item | Priority | Size | Notes |
|---|---|---|---|
| Chromecast / DLNA casting | P3 | XL | |
| Skip intro / recap / credits from addon markers | P2 | M | Stremio supporter feature; needs an addon source for the timestamps. |
| Sleep timer | P3 | S | |
| Screenshot capture | P3 | S | mpv `screenshot-to-file`. |
| A-B loop / repeat | P3 | S | |
| Frame-by-frame stepping | P3 | S | |
| Picture-in-picture / mini player | P3 | M | egui viewport or a second native window. |
| Audio output device selection | P3 | M | mpv `audio-device`. |
| Volume normalization / boost | P3 | M | mpv audio filter (`dynaudnorm`). |
| Preload the next episode for gapless playback | P3 | M | |

### Downloads and torrents

| Item | Priority | Size | Notes |
|---|---|---|---|
| Download queue with progress and pause/resume | P2 | M | Builds on the download manager. |
| Seeding limits (ratio/time) and a torrent manager UI | P3 | S | Torrents are stopped and deleted automatically now. |
| Custom listen port / UPnP / NAT-PMP | P3 | M | |

### Settings and streaming server

| Item | Priority | Size | Notes |
|---|---|---|---|
| Transcoding for formats the player can't handle | P3 | L | mpv already plays almost everything, so this is mostly irrelevant. |
| HTTP/SOCKS proxy for addon and stream requests | P2 | M | Also routes torrents; relevant for VPN users. |
| Configurable User-Agent | P3 | S | Some addons and streams reject the default. |
| Pause playback when the window is minimized | P3 | S | Stremio setting. |
| Launch playback in a separate window | P3 | M | Stremio setting; egui multi-viewport. |
| Log file and diagnostics export | P3 | S | |

### Addons

| Item | Priority | Size | Notes |
|---|---|---|---|
| Remote/LAN streaming server | P2 | L | Stremio serves the library to other devices; casting is a separate item. |
| Addon health indicator (unreachable / timeout) | P3 | S | |
| QR code to install an addon | P3 | S | |
| Per-type addon ordering | P3 | S | One global priority list today. |

### Accounts and sync

| Item | Priority | Size | Notes |
|---|---|---|---|
| Upload to the Stremio account (addons, library, progress) | P2 | M | Sign-in pulls addons, library and watched titles; nothing is pushed back. |
| Trakt integration (scrobbling, watched sync) | P3 | L | |
| Export / import all data (addons, library, progress) | P2 | S | Stremio offers a data export; good for backup and migration. |
| Simkl / MyAnimeList / Letterboxd sync | P3 | L | After Trakt. |

### Platform and app

| Item | Priority | Size | Notes |
|---|---|---|---|
| UI translations (i18n) | P3 | M | |
| Auto-install updates | P3 | M | Release builds already show a download link for a newer build; downloading and replacing the binary is still missing. |
| Android, iOS, TV clients | P3 | XL | Out of scope for an egui desktop app. |
| Remember last tab and window state on launch | P3 | S | |
| Command-line flags for play/open | P3 | S | Only `stremio://` links are parsed now. |
| Screen-reader labels and focus-order polish | P3 | M | AccessKit is enabled; coverage is partial. |
| Crash-safe recovery / resume after unexpected exit | P3 | S | WAL is on; no explicit recovery UI. |
