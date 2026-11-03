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

### Player

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
| Send `videoHash` / `videoSize` to subtitle addons (OpenSubtitles matching) | P2 | M | Needs the OpenSubtitles hash of the file. |
| Chromecast / DLNA casting | P3 | XL | |

### Settings and streaming server

| Item | Priority | Difficulty | Notes |
|---|---|---|---|
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
| Android, iOS, TV clients | P3 | XL | Out of scope for an egui desktop app. |
