# Local API

Stremflix-rs serves a small HTTP API so other apps on this machine can show Continue Watching, search the installed addons, and start playback in the open window.

- Address: `http://127.0.0.1:11471/api/`. Set `STREMFLIX_RS_API` to another `host:port` (`127.0.0.1:0` picks a free port) or to `off`. The log names the bound address (`local API on http://…`).
- No authentication. Any program that can reach the address can read the watch history and start playback. A web page in a browser can also start playback, because it can send a POST without reading the reply. Binding a non-loopback address exposes the API to the network.
- The `Host` header, when sent, must be an IP address or `localhost`. Other names get 400, so a web page can't read replies through DNS rebinding.
- Requests go to the running window and use its active profile: the same addons, Continue Watching, and child-safe limits as the window.
- Responses are JSON with `Content-Type: application/json`. Errors return `{"error": "<message>"}` with status 400 (bad or missing parameter), 404 (unknown path or title), 405 (wrong method), or 503 (the app did not answer within 20 s).
- Thumbnails are the addon's image URLs (`poster`, `background`). Fetch them directly. They can be `null`. Local files and raw torrents have no poster.

## `GET /api/continue`

Continue Watching, most recent first (up to 20 entries).

```sh
curl http://127.0.0.1:11471/api/continue
```

```json
[
  {"type": "series", "id": "tt0903747:1:2", "title": "Cat's in the Bag...", "poster": "https://…/poster.jpg", "position": 812.4, "duration": 2880.0},
  {"type": "file", "id": "/home/me/Videos/clip.mkv", "title": "clip", "poster": null, "position": 30.0, "duration": 95.5}
]
```

- `type` and `id` identify the entry for `/api/play`. For a series, `id` is the episode id (`<series id>:<season>:<episode>`).
- `type` is the addon type (`movie`, `series`, …), `file` for a local file (`id` is its path), or `btih` for a torrent (`id` is its info hash).
- `position` and `duration` are in seconds.

## `GET /api/search?q=<query>`

Searches every installed catalog that supports search and returns the combined results in addon order, without duplicates. The query is percent-encoded and capped at 100 characters. Catalogs that fail or time out are left out.

```sh
curl 'http://127.0.0.1:11471/api/search?q=big%20buck%20bunny'
```

```json
[
  {"id": "tt1254207", "type": "movie", "name": "Big Buck Bunny", "poster": "https://…/poster.jpg", "background": "https://…/bg.jpg", "description": "…"}
]
```

Returns 400 when no installed addon supports search.

## `POST /api/play?type=<type>&id=<id>[&name=<title>]`

Shows the window and starts playback, like a poster's play button. Any video that is playing stops first.

- Entry in Continue Watching: resumes it with the same stream.
- Movie: plays the top stream from the addons.
- Series: resumes the episode in progress, or plays the next unwatched one.
- `name` (optional, from the search result) titles a new Continue Watching entry until the addon's metadata loads. Without it, the entry starts out titled with its id.
- `file` and `btih` work only for entries in Continue Watching. Otherwise the API returns 404, so it never opens a path or magnet it was given.

```sh
curl -X POST 'http://127.0.0.1:11471/api/play?type=movie&id=tt1254207&name=Big%20Buck%20Bunny'
```

```json
{"ok": true}
```

`ok` means the request was accepted. Streams load in the background. If none can play, the window shows the error.
