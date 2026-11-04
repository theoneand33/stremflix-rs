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
| Trailers (`trailers` / `ytId`) | P2 | M | Needs YouTube resolution; mpv with yt-dlp can play them. |
| Calendar of upcoming episodes for library series | P2 | M | Depends on the saved library. |
| New-episode notifications | P2 | M | Depends on the saved library. |

### Player

| Item | Priority | Size | Notes |
|---|---|---|---|
| Send `videoHash` / `videoSize` to subtitle addons (OpenSubtitles matching) | P2 | M | Needs the OpenSubtitles hash of the file. |
| Chromecast / DLNA casting | P3 | XL | |

### Settings and streaming server

| Item | Priority | Size | Notes |
|---|---|---|---|
| Transcoding for formats the player can't handle | P3 | L | mpv already plays almost everything, so this is mostly irrelevant. |

### Accounts and sync

| Item | Priority | Size | Notes |
|---|---|---|---|
| Stremio account login, with addon and library sync (api.strem.io) | P2 | L | Today there's only a one-way import from a local install. |
| Trakt integration (scrobbling, watched sync) | P3 | L | |

### Platform and app

| Item | Priority | Size | Notes |
|---|---|---|---|
| UI translations (i18n) | P3 | M | |
| Auto-install updates | P3 | M | Release builds already show a download link for a newer build; downloading and replacing the binary is still missing. |
| Android, iOS, TV clients | P3 | XL | Out of scope for an egui desktop app. |
