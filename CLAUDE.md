# Lumen

Apple-Photos-style photo viewer for Linux, backed by an Immich server. Rust, wgpu 30, winit 0.30,
glyphon (text), GStreamer (video). Config: `~/.config/lumen/config.json` (`url`, `fallback_url`, `use_fallback`, `api_key`; mode 600, see `account.rs`).

## Layout
- `main.rs`: `Core` (app state, per-frame `update`, header), `App` (winit window and input), headless `screenshot()` and CLI flags
- `grid.rs` (+ `grid.wgsl`, `render.rs`): timeline grid, a single instanced draw. Zoom levels, blend animations, continuous pinch (`Spec::Scaled`)
- `render.rs`: thumbnail atlases by tier (small 64 / large 256 / xl 1024 px) plus a mipmapped thumbhash atlas
- `loader.rs`: library sync by collection (`Collection`, `Loader::open`), thumbnail workers fed by a priority want-list, disk caches in `~/.cache/lumen/`
- `ui.rs` / `ui_render.rs` / `ui.wgsl`: immediate-mode draw list (`Frame { base, overlay }` of quads, images and text)
- `sidebar.rs`, `viewer.rs`, `video.rs` (GStreamer), `map.rs` (Places), `gestures.rs` (Wayland pinch; remove once on winit 0.31)
- `actions.rs`: selection, favorite, archive, trash/restore, add-to-album, context menus, toasts with undo. Optimistic edits go through `Library::edit` and `Renderer::remap_library`
- `search.rs`: smart search (Immich CLIP): sidebar search field (`widgets::TextField`), suggestions popover, results as a `Collection::results` kept in relevance order (`Library::ordered`, grid from the top), filter chips, paging. Headless: `--search "<q>"`, `--search-type "<text>"`, see `search::Shot`
- `hover.rs`: hover-to-play previews and badges. `live.rs`: Immich socket.io push events lead to `Loader::refresh`
- `settings.rs` / `settings_ui.rs` / `widgets.rs` / `storage.rs`: preferences (`~/.config/lumen/settings.json`), the Settings page, reusable controls, cache sizes and limits
- `account.rs` / `login.rs`: saved account, key check (`account::verify`), sign out; the sign-in screen shown while no key is saved (`Core.login`). Signing in/out sets `Core.switch` and the window rebuilds `Core` (`Core::rebuild`) on the same renderer. Headless: `--login [--login-paste config|<key>] [--login-server] [--login-local/--login-remote <addr>]`
- `immich.rs` routing: a client has preferred-first addresses (LAN, then remote); `connect()` probes `/server/ping`, failed requests reroute once, `watch_routes` returns to the LAN address. Cache keys use `identity()` (the first address), not `base()`. Away from home the LAN IP doesn't answer, so the remote address is used
- UI layers draw `base`, then `overlay` (viewer), then `popup` (menus, sheets, toasts)

## Working on it
- Build: `cargo build --release`. Don't build debug or add more target dirs; the user cares about disk usage.
- Verify visually with headless renders, not GUI windows: `./target/release/lumen --screenshot /tmp/x.png [--select favorites|album:<name>] [--fit|--zoom N] [--open newest] [--map] [--pinch 1.5,end]`, then look at the PNG.
- `--demo N` runs without a server.
- The Immich server holds the user's real library. Read-only unless the feature requires writes, and never permanently delete. Never print the API key.
- For parallel feature work, use git worktrees under `~/repos/lumen-wt/<name>`, seeded with `cp -r --reflink=always target` (btrfs), and remove them after merging.
- Commit trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
