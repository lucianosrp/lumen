# Lumen

Apple-Photos-style photo viewer for Linux, backed by an Immich server. Rust, wgpu 30, winit 0.30,
glyphon (text), GStreamer (video). Config: `~/.config/lumen/config.json` (`url`, `api_key`).

## Layout
- `main.rs`: `Core` (app state, per-frame `update`, header), `App` (winit window and input), headless `screenshot()` and CLI flags
- `grid.rs` (+ `grid.wgsl`, `render.rs`): timeline grid, a single instanced draw. Zoom levels, blend animations, continuous pinch (`Spec::Scaled`)
- `render.rs`: thumbnail atlases by tier (small 64 / large 256 / xl 1024 px) plus a mipmapped thumbhash atlas
- `loader.rs`: library sync by collection (`Collection`, `Loader::open`), thumbnail workers fed by a priority want-list, disk caches in `~/.cache/lumen/`
- `ui.rs` / `ui_render.rs` / `ui.wgsl`: immediate-mode draw list (`Frame { base, overlay }` of quads, images and text)
- `sidebar.rs`, `viewer.rs`, `video.rs`, `map.rs` (Places), `gestures.rs` (Wayland pinch; remove once on winit 0.31)

## Working on it
- Build: `cargo build --release`. Don't build debug or add more target dirs; the user cares about disk usage.
- Verify visually with headless renders, not GUI windows: `./target/release/lumen --screenshot /tmp/x.png [--select favorites|album:<name>] [--fit|--zoom N] [--open newest] [--map] [--pinch 1.5,end]`, then look at the PNG.
- `--demo N` runs without a server.
- The Immich server holds the user's real library. Read-only unless the feature requires writes, and never permanently delete. Never print the API key.
- For parallel feature work, use git worktrees under `~/repos/lumen-wt/<name>`, seeded with `cp -r --reflink=always target` (btrfs), and remove them after merging.
- Commit trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
