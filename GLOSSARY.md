# Glossary

Terms the code and its modules are named after.

- **Addon**: a Stremio addon, identified by its transport URL ending in `/manifest.json`. Installed addons have a global priority, plus an optional order per content type.
- **Item key**: the key of progress, saved titles, watched marks and downloads (`key.rs`). An addon item is `<type>:<id>`, and an episode's id is its video id (`series:tt1:1:2`). A local file is `file:<path>`, a raw magnet `btih:<info hash>`.
- **Setting**: one entry in the settings table, with its key, default and stored form (`settings::Setting<T>`). A *family* of settings shares a key prefix: `<key>:<suffix>`, such as `addon_order:movie` or `binge:<series key>`.
- **Navigation**: opening a title, an episode or a link, playing, or going Back. Each one bumps `App::nav` (`begin_nav`), and replies requested by an earlier navigation are dropped.
- **Autoplay**: what to play when a title's streams arrive: nothing, the only stream, the top one, or (for the next episode) the stream in the same binge group, carrying the sleep timer.
- **Binge group**: `behaviorHints.bingeGroup`, which marks streams from the same source; the next episode prefers the group the last one played from.
- **Catalog row**: one horizontal row of a catalog on Home or the search page.
- **Paging**: loading a catalog's items a page at a time through the `skip` extra (`catalog::Paged`).
- **Download queue**: unfinished downloads, oldest first, at most two transferring (`download::Queue`). A *transfer* is one run of a job; a stopped run's late events are dropped.
- **Release (torrents)**: stopping every torrent but the playing one, so none keeps downloading or seeding. It runs in the background, and the engine runs it before any stream asked after it.
- **Child-safe profile**: a profile locked with a PIN. It hides the Addons page, most settings and addons flagged adult.
- **Video surface**: the framebuffer libmpv renders into, shown as an egui texture.
