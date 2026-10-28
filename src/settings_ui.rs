//! The Settings page: replaces the grid (like the Places map) while the sidebar stays.
//! Opened from the sidebar's gear row or Ctrl+, and left with Esc or another sidebar row.
//!
//! Controls edit `Core.settings` in place; changes are applied live and saved once the mouse
//! button is up. Cache sizes and server details load on background threads.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde::Deserialize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::Core;
use crate::loader::{Loader, Source, Tier, cache_dir, group};
use crate::render::{atlas_bytes, atlas_dims};
use crate::settings::{self, GpuMemory, Quality, ThumbSize};
use crate::storage::{self, Sizes, format_bytes};
use crate::ui::{Align, Text, rgb, theme};
use crate::icons::IconStyle;
use crate::widgets::{ButtonKind, Card, Ctx, Input, colors};

/// Logical pixels.
const HEADER: f32 = 76.0;
const MAX_WIDTH: f32 = 640.0;
const ROW: f32 = 44.0;
const ROW_SUB: f32 = 56.0;
const WHEEL_STEP: f32 = 80.0;
/// How long the "Clear all caches" button waits for its confirming second click.
const CONFIRM_FOR: Duration = Duration::from_secs(3);
const FLASH_FOR: Duration = Duration::from_secs(4);
/// Cache sizes are re-measured this often while the page is open.
const REMEASURE: Duration = Duration::from_secs(10);
/// Thumbnail cache limit steps in MB; 0 = unlimited.
const LIMITS: [u32; 10] = [250, 500, 750, 1000, 1500, 2000, 3000, 4000, 5000, 0];

#[derive(Deserialize)]
struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

#[derive(Deserialize)]
struct Stats {
    images: u64,
    videos: u64,
}

#[derive(Deserialize)]
struct Me {
    name: String,
}

/// `GET /api/server/storage`: the disk holding the library.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Disk {
    disk_size_raw: u64,
    disk_use_raw: u64,
    disk_available_raw: u64,
    disk_usage_percentage: f64,
}

/// `GET /api/users/me`: what this user's assets take.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Quota {
    quota_usage_in_bytes: Option<u64>,
    quota_size_in_bytes: Option<u64>,
}

/// `GET /api/server/statistics` (admins only).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerStats {
    photos: u64,
    videos: u64,
    usage: u64,
    usage_photos: Option<u64>,
    usage_videos: Option<u64>,
    #[serde(default)]
    usage_by_user: Vec<UserUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserUsage {
    user_name: String,
    photos: u64,
    videos: u64,
    usage: u64,
}

/// Whatever of the server's storage this key may read; each part is None when it may not
/// (401/403/404) or the request failed.
#[derive(Default)]
struct ServerStorage {
    disk: Option<Disk>,
    quota: Option<Quota>,
    stats: Option<ServerStats>,
}

/// Above this share of the server disk in use, the bar turns red with a note.
const DISK_WARN: f64 = 90.0;

struct ServerInfo {
    version: String,
    user: Option<String>,
    stats: Option<Stats>,
    latency: Duration,
}

enum Server {
    Demo,
    Loading,
    Ok(ServerInfo),
    Failed(String),
}

enum Bg {
    Sizes(Sizes),
    Server(Result<ServerInfo, String>),
    Storage(ServerStorage),
    Cleared(&'static str, u64),
}

/// What the page asks for after a frame; done once the controls have been drawn.
enum Act {
    Clear(&'static str),
    Reload,
    Test,
    OpenFolder,
}

pub enum Response {
    Ignored,
    Handled,
    Close,
}

pub struct Page {
    /// Logical pixels.
    scroll: f32,
    scroll_to: f32,
    content: f32,
    input: Input,
    anims: HashMap<String, f32>,
    last: Instant,
    tx: Sender<Bg>,
    rx: Receiver<Bg>,
    wake: Arc<dyn Fn() + Send + Sync>,
    sizes: Option<Sizes>,
    measuring: bool,
    measured_at: Instant,
    server: Server,
    server_storage: Option<ServerStorage>,
    storage_loading: bool,
    /// "Clear all caches" was clicked once at this time.
    confirm_all: Option<Instant>,
    /// Which clear is running in the background.
    clearing: Option<&'static str>,
    /// A short result message under a row: (row key, message, when).
    flash: Option<(&'static str, String, Instant)>,
    /// Settings changed since the last save.
    dirty: bool,
}

impl Page {
    pub fn new(loader: &Loader) -> Self {
        let (tx, rx) = channel();
        let mut page = Self {
            scroll: 0.0,
            scroll_to: 0.0,
            content: 0.0,
            input: Input::default(),
            anims: HashMap::new(),
            last: Instant::now(),
            tx,
            rx,
            wake: loader.waker(),
            sizes: None,
            measuring: false,
            measured_at: Instant::now(),
            server: Server::Loading,
            server_storage: None,
            storage_loading: false,
            confirm_all: None,
            clearing: None,
            flash: None,
            dirty: false,
        };
        page.measure();
        page.test(loader);
        page
    }

    /// True once cache sizes and server details have arrived (for headless screenshots).
    pub fn is_idle(&self) -> bool {
        !self.measuring && self.clearing.is_none() && !matches!(self.server, Server::Loading) && !self.storage_loading
    }

    fn measure(&mut self) {
        if self.measuring {
            return;
        }
        self.measuring = true;
        let (tx, wake) = (self.tx.clone(), self.wake.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Bg::Sizes(storage::measure()));
            wake();
        });
    }

    fn test(&mut self, loader: &Loader) {
        let Source::Immich(immich) = loader.source() else {
            self.server = Server::Demo;
            return;
        };
        self.server = Server::Loading;
        self.fetch_storage(loader);
        let (immich, tx, wake) = (immich.clone(), self.tx.clone(), self.wake.clone());
        loader.handle().spawn(async move {
            let start = Instant::now();
            let info = match immich.get_json::<Version>("/server/version").await {
                Ok(v) => {
                    let latency = start.elapsed();
                    let (me, stats) = tokio::join!(
                        immich.get_json::<Me>("/users/me"),
                        immich.get_json::<Stats>("/assets/statistics")
                    );
                    Ok(ServerInfo {
                        version: format!("v{}.{}.{}", v.major, v.minor, v.patch),
                        user: me.ok().map(|m| m.name).filter(|n| !n.is_empty()),
                        stats: stats.ok(),
                        latency,
                    })
                }
                Err(e) => Err(format!("{e:#}")),
            };
            let _ = tx.send(Bg::Server(info));
            wake();
        });
    }

    /// Server disk, own usage and (for admins) library statistics, in the background.
    fn fetch_storage(&mut self, loader: &Loader) {
        let Source::Immich(immich) = loader.source() else { return };
        self.storage_loading = true;
        let (immich, tx, wake) = (immich.clone(), self.tx.clone(), self.wake.clone());
        loader.handle().spawn(async move {
            let (disk, quota, stats) = tokio::join!(
                immich.get_json::<Disk>("/server/storage"),
                immich.get_json::<Quota>("/users/me"),
                immich.get_json::<ServerStats>("/server/statistics"),
            );
            // Not every key may read every part (statistics are admin-only): just leave it out.
            let quiet = |what: &str, e: &anyhow::Error| log::debug!("server storage: {what}: {e:#}");
            let storage = ServerStorage {
                disk: disk.inspect_err(|e| quiet("disk", e)).ok(),
                quota: quota.inspect_err(|e| quiet("quota", e)).ok(),
                stats: stats.inspect_err(|e| quiet("statistics", e)).ok(),
            };
            let _ = tx.send(Bg::Storage(storage));
            wake();
        });
    }

    /// Routes window input while the page is open.
    pub fn event(&mut self, event: &WindowEvent, cursor: (f32, f32), over_sidebar: bool, mods: ModifiersState, scale: f32) -> Response {
        match event {
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                if over_sidebar {
                    return Response::Ignored;
                }
                if cursor.1 >= HEADER * scale {
                    self.input.press(cursor);
                }
                Response::Handled
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                if self.input.origin.is_none() {
                    return Response::Ignored;
                }
                self.input.release(cursor);
                Response::Handled
            }
            WindowEvent::MouseWheel { delta, .. } if !over_sidebar => {
                match *delta {
                    MouseScrollDelta::LineDelta(_, y) => self.scroll_to -= y * WHEEL_STEP,
                    MouseScrollDelta::PixelDelta(p) => {
                        self.scroll_to -= p.y as f32 / scale;
                        self.scroll = self.scroll_to;
                    }
                }
                Response::Handled
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if mods.control_key() || mods.super_key() || mods.alt_key() {
                    return Response::Ignored;
                }
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Escape) => return Response::Close,
                    Key::Named(NamedKey::PageDown | NamedKey::Space) => self.scroll_to += 400.0,
                    Key::Named(NamedKey::PageUp) => self.scroll_to -= 400.0,
                    Key::Named(NamedKey::ArrowDown) => self.scroll_to += 60.0,
                    Key::Named(NamedKey::ArrowUp) => self.scroll_to -= 60.0,
                    Key::Named(NamedKey::Home) => self.scroll_to = 0.0,
                    Key::Named(NamedKey::End) => self.scroll_to = f32::MAX,
                    // Grid and map shortcuts don't apply here.
                    Key::Character("=" | "+" | "-" | "_" | "0" | "m") => {}
                    _ => return Response::Ignored,
                }
                Response::Handled
            }
            _ => Response::Ignored,
        }
    }

    /// Headless: scrolls to a logical offset or to a section by name ("storage", "server"…).
    pub fn scroll_to(&mut self, at: &str) {
        let y = at.parse::<f32>().ok().or_else(|| {
            let key = format!("section:{}", at.to_lowercase());
            self.anims.get(&key).copied()
        });
        self.scroll_to = y.unwrap_or(0.0);
        self.scroll = self.scroll_to;
    }

    /// Headless: a click at a physical position, handled on the next frame.
    pub fn click(&mut self, at: (f32, f32)) {
        self.input.cursor = at;
        self.input.press(at);
        self.input.release(at);
    }

    /// Redraws once `d` from now, e.g. to expire a confirm state without polling every frame.
    fn wake_after(&self, d: Duration) {
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            std::thread::sleep(d + Duration::from_millis(20));
            wake();
        });
    }

    fn save(&mut self, s: &settings::Settings) {
        self.dirty = false;
        if let Err(e) = s.save() {
            log::warn!("{e:#}");
        }
    }
}

/// Headless options: `--settings`, `--settings-scroll <px|section>`, `--settings-click x,y`
/// (repeatable, physical px) and `--settings-hover x,y`.
pub struct Shot {
    pub scroll: Option<String>,
    pub clicks: Vec<(f32, f32)>,
    pub hover: Option<(f32, f32)>,
}

impl Shot {
    pub fn parse(args: &[String]) -> Option<Self> {
        let point = |s: &String| {
            let (x, y) = s.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        };
        let values = |name: &'static str| args.windows(2).filter(move |w| w[0] == name).map(|w| &w[1]);
        let shot = Shot {
            scroll: values("--settings-scroll").next().cloned(),
            clicks: values("--settings-click").filter_map(point).collect(),
            hover: values("--settings-hover").next().and_then(point),
        };
        let any = args.iter().any(|a| a == "--settings") || shot.scroll.is_some() || !shot.clicks.is_empty() || shot.hover.is_some();
        any.then_some(shot)
    }
}

pub fn open(core: &mut Core) {
    if core.settings_page.is_none() {
        core.set_map(false);
        core.settings_page = Some(Page::new(&core.loader));
    }
    core.sidebar.set_selected("settings");
}

/// Closes the page, saving anything pending; the caller shows something else.
pub fn leave(core: &mut Core) {
    if let Some(mut page) = core.settings_page.take()
        && page.dirty
    {
        page.save(&core.settings);
    }
}

/// Back to the grid, showing whatever collection was open.
pub fn close(core: &mut Core) {
    leave(core);
    if core.map.is_none() {
        core.sidebar.set_selected(&core.collection.key);
    }
}

pub fn toggle(core: &mut Core) {
    if core.settings_page.is_some() { close(core) } else { open(core) }
}

fn idx<T: PartialEq + Copy>(all: &[T], v: T) -> usize {
    all.iter().position(|&x| x == v).unwrap_or(0)
}

/// Builds the page in place of the grid. Returns true while anything is moving.
pub fn update(core: &mut Core, now: Instant) -> bool {
    core.instances.clear();
    core.loader.set_wants(core.generation, []);
    let Some(page) = core.settings_page.as_mut() else { return false };
    let dt = now.duration_since(page.last).as_secs_f32().min(0.05);
    page.last = now;

    while let Ok(msg) = page.rx.try_recv() {
        match msg {
            Bg::Sizes(s) => {
                page.sizes = Some(s);
                page.measuring = false;
                page.measured_at = Instant::now();
            }
            Bg::Server(r) => page.server = r.map_or_else(Server::Failed, Server::Ok),
            Bg::Storage(st) => {
                page.server_storage = Some(st);
                page.storage_loading = false;
            }
            Bg::Cleared(what, freed) => {
                page.clearing = None;
                page.flash = Some((what, format!("Freed {}", format_bytes(freed)), now));
                page.wake_after(FLASH_FOR);
                page.measuring = false;
                page.measure();
            }
        }
    }
    if page.clearing.is_none() && now.duration_since(page.measured_at) > REMEASURE {
        page.measured_at = now;
        page.measure();
    }
    if page.confirm_all.is_some_and(|t| now.duration_since(t) > CONFIRM_FOR) {
        page.confirm_all = None;
    }
    if page.flash.as_ref().is_some_and(|f| now.duration_since(f.2) > FLASH_FOR) {
        page.flash = None;
    }

    let s = core.scale;
    let (w, h) = (core.size.0 as f32, core.size.1 as f32);
    let left = core.left;
    let top = HEADER * s;
    let view = (h - top) / s;
    let max = (page.content - view).max(0.0);
    page.scroll_to = page.scroll_to.clamp(0.0, max);
    let gliding = (page.scroll_to - page.scroll).abs() > 0.3;
    page.scroll = if gliding { page.scroll + (page.scroll_to - page.scroll) * (1.0 - (-dt * 20.0).exp()) } else { page.scroll_to };
    page.input.cursor = core.sidebar.cursor;

    // Facts the page shows, gathered before the UI borrows the renderer.
    let max_tex = core.renderer.device.limits().max_texture_dimension_2d;
    let atlas_now = core.renderer.atlas_bytes();
    let slots = [core.renderer.small.capacity(), core.renderer.large.capacity(), core.renderer.xl.capacity()];
    let adapter = core.renderer.adapter.clone();
    let demo = matches!(core.loader.source(), Source::Demo(_));
    let server_url = match core.loader.source() {
        Source::Immich(i) => i.base().to_string(),
        Source::Demo(_) => "Demo library (no server)".into(),
    };
    let collection_title = core.collection.title.clone();
    let lib_counts = core.library.as_ref().map(|l| (l.len() - l.videos, l.videos));
    let syncing = core.status.clone();

    let before = core.settings.clone();
    let quads_before = core.ui.base.quads.len();
    let mut acts = Vec::new();
    let area = [left, 0.0, w - left, h];
    let mut ctx = Ctx {
        layer: &mut core.ui.base,
        uir: &mut core.renderer.ui,
        input: &page.input,
        anims: &mut page.anims,
        s,
        dt,
        clip: [left, top, w - left, h - top],
        animating: false,
    };
    ctx.quad(area, rgb(theme::bg(), 1.0), 0.0);

    let cw = (area[2] - 48.0 * s).min(MAX_WIDTH * s);
    let x = left + (area[2] - cw) / 2.0;
    let right = x + cw - 16.0 * s;
    let mut y = top + 8.0 * s - page.scroll * s;
    let st = &mut core.settings;
    let scroll = page.scroll;
    let flash = |key: &str| page.flash.as_ref().filter(|f| f.0 == key).map(|f| f.1.clone());

    let pal = theme::palette();
    let section = |ctx: &mut Ctx, y: &mut f32, key: &str, icon: &'static str, color: u32, title: &str| {
        // Remember where each section starts, for `--settings-scroll <section>`.
        let offset = (*y - (top + 8.0 * s)) / s + scroll;
        ctx.anims.insert(format!("section:{key}"), offset);
        let mid = *y + 14.0 * s;
        ctx.icon_tile(x + 2.0 * s, mid, icon, color);
        ctx.text(title, x + 36.0 * s, mid, 15.0, 700, theme::text(), Align::Left);
        *y += 38.0 * s;
    };
    let gap = 28.0 * s;

    // --- Appearance ----------------------------------------------------------------------
    section(&mut ctx, &mut y, "appearance", "palette", pal.accent, "Appearance");
    let mut card = Card::begin(&ctx, x, y, cw);
    let r = card.row(&mut ctx, ROW_SUB);
    let name = theme::omarchy_name();
    let sub = match (&name, st.follow_omarchy && theme::from_omarchy()) {
        (Some(n), true) => format!("Using the “{n}” theme · changes apply live"),
        (Some(n), false) if !st.follow_omarchy => format!("Classic palette · Omarchy theme “{n}” available"),
        (Some(_), false) => "The Omarchy theme couldn't be read · using the classic palette".to_string(),
        (None, _) => "No Omarchy theme found · using the classic palette".to_string(),
    };
    ctx.row_label_until(r, "Follow Omarchy theme", Some(&sub), true, right - 40.0 * s);
    ctx.toggle("follow_omarchy", right, r[1] + r[3] / 2.0, &mut st.follow_omarchy, name.is_some());
    let r = card.row(&mut ctx, ROW);
    let mid = r[1] + r[3] / 2.0;
    let mode = if pal.dark { "Dark" } else { "Light" };
    ctx.row_label(r, "Palette", None, true);
    let swatches = [pal.bg, pal.surface, pal.raised, pal.fg, pal.muted, pal.accent, pal.selection, pal.red, pal.green];
    let (d, step) = ((18.0 * s).round(), (24.0 * s).round());
    let mut sx = (right - d).round();
    for &c in swatches.iter().rev() {
        let ring = s.round().max(1.0);
        let edge = pal.shade(colors::card(), 0.28);
        ctx.quad([sx - ring, (mid - d / 2.0).round() - ring, d + 2.0 * ring, d + 2.0 * ring], rgb(edge, 1.0), d / 2.0 + ring);
        ctx.quad([sx, (mid - d / 2.0).round(), d, d], rgb(c, 1.0), d / 2.0);
        sx -= step;
    }
    ctx.text(mode, sx + step - 10.0 * s, mid, 12.5, 500, colors::text_secondary(), Align::Right);
    y = card.end(&mut ctx) + gap;

    // --- Library & Grid ------------------------------------------------------------------
    section(&mut ctx, &mut y, "grid", "layout-grid", pal.tints[3], "Library & Grid");
    let mut card = Card::begin(&ctx, x, y, cw);
    let r = card.row(&mut ctx, ROW_SUB);
    let until = right - ctx.segmented_width(&["Small", "Medium", "Large"]);
    ctx.row_label_until(r, "Thumbnail size", Some("How large photos are when a collection opens"), true, until);
    let sizes_all = [ThumbSize::Small, ThumbSize::Medium, ThumbSize::Large];
    let mut i = idx(&sizes_all, st.thumb_size);
    if ctx.segmented("thumb_size", right, r[1] + r[3] / 2.0, &["Small", "Medium", "Large"], &mut i) {
        st.thumb_size = sizes_all[i];
    }
    let r = card.row(&mut ctx, ROW_SUB);
    ctx.row_label_until(r, "Gaps between photos", Some("Thin dark lines once tiles are big enough to read as photos"), true, right - 40.0 * s);
    ctx.toggle("gaps", right, r[1] + r[3] / 2.0, &mut st.gaps, true);
    let r = card.row(&mut ctx, ROW_SUB);
    let q_all = [Quality::Fast, Quality::Balanced, Quality::Sharp];
    let sub = match st.quality {
        Quality::Fast => "Smallest downloads; never loads full-size previews",
        Quality::Balanced => "Sharp thumbnails, previews for the largest tiles",
        Quality::Sharp => "Higher-resolution thumbnails and previews sooner",
    };
    let until = right - ctx.segmented_width(&["Fast", "Balanced", "Sharp"]);
    ctx.row_label_until(r, "Image quality", Some(sub), true, until);
    let mut i = idx(&q_all, st.quality);
    if ctx.segmented("quality", right, r[1] + r[3] / 2.0, &["Fast", "Balanced", "Sharp"], &mut i) {
        st.quality = q_all[i];
    }
    y = card.end(&mut ctx) + gap;

    // --- Scrolling & Gestures ------------------------------------------------------------
    section(&mut ctx, &mut y, "scrolling", "hand", pal.tints[4], "Scrolling & Gestures");
    // Sliders give way on narrow windows so labels keep some room.
    let slider_w = (cw * 0.28).clamp(110.0 * s, 180.0 * s);
    let value_w = 64.0 * s;
    let mut card = Card::begin(&ctx, x, y, cw);
    let r = card.row(&mut ctx, ROW_SUB);
    ctx.row_label_until(r, "Scroll speed", Some("Touchpad and mouse wheel"), true, right - value_w - slider_w - 10.0 * s);
    let mid = r[1] + r[3] / 2.0;
    let mut v = (st.scroll_speed - 0.5) / 1.5;
    if ctx.slider("scroll_speed", right - value_w - slider_w, mid, slider_w, &mut v, 16) {
        st.scroll_speed = 0.5 + v * 1.5;
    }
    ctx.text(format!("{:.1}×", st.scroll_speed), right, mid, 13.0, 500, colors::text_secondary(), Align::Right);
    let r = card.row(&mut ctx, ROW_SUB);
    ctx.row_label_until(r, "Pinch zoom speed", Some("How quickly the grid reflows as you pinch"), true, right - value_w - slider_w - 10.0 * s);
    let mid = r[1] + r[3] / 2.0;
    let mut v = (st.pinch_speed - 0.5) / 1.5;
    if ctx.slider("pinch_speed", right - value_w - slider_w, mid, slider_w, &mut v, 16) {
        st.pinch_speed = 0.5 + v * 1.5;
    }
    ctx.text(format!("{:.1}×", st.pinch_speed), right, mid, 13.0, 500, colors::text_secondary(), Align::Right);
    let r = card.row(&mut ctx, ROW_SUB);
    ctx.row_label_until(r, "Momentum scrolling", Some("Keep gliding after a touchpad swipe"), true, right - 40.0 * s);
    ctx.toggle("momentum", right, r[1] + r[3] / 2.0, &mut st.momentum, true);
    y = card.end(&mut ctx) + gap;

    // --- Playback ------------------------------------------------------------------------
    section(&mut ctx, &mut y, "playback", "film", pal.tints[0], "Playback");
    let mut card = Card::begin(&ctx, x, y, cw);
    let r = card.row(&mut ctx, ROW);
    ctx.row_label_until(r, "Autoplay videos in the viewer", None, true, right - 40.0 * s);
    ctx.toggle("autoplay", right, r[1] + r[3] / 2.0, &mut st.autoplay, true);
    let r = card.row(&mut ctx, ROW_SUB);
    ctx.row_label_until(r, "Loop short clips", Some("Videos under 15 seconds repeat, like Live Photos"), true, right - 40.0 * s);
    ctx.toggle("loop_short", right, r[1] + r[3] / 2.0, &mut st.loop_short, true);
    let r = card.row(&mut ctx, ROW);
    ctx.row_label_until(r, "Default volume", None, true, right - value_w - slider_w - 30.0 * s);
    let mid = r[1] + r[3] / 2.0;
    ctx.icon("volume-1", right - value_w - slider_w - 20.0 * s, mid, 16.0, colors::text_secondary());
    if ctx.slider("volume", right - value_w - slider_w, mid, slider_w, &mut st.volume, 21) {
        st.volume = (st.volume * 20.0).round() / 20.0;
    }
    ctx.text(format!("{:.0}%", st.volume * 100.0), right, mid, 13.0, 500, colors::text_secondary(), Align::Right);
    let r = card.row(&mut ctx, ROW_SUB);
    ctx.row_label_until(r, "Play videos and Live Photos on hover", Some("In the grid, while the pointer rests on them"), true, right - 40.0 * s);
    ctx.toggle("hover_play", right, r[1] + r[3] / 2.0, &mut st.hover_play, true);
    let r = card.row(&mut ctx, ROW);
    let on = st.hover_play;
    ctx.row_label_until(r, "Sound when playing on hover", None, on, right - 40.0 * s);
    ctx.toggle("hover_sound", right, r[1] + r[3] / 2.0, &mut st.hover_sound, on);
    y = card.end(&mut ctx) + gap;

    // --- Storage -------------------------------------------------------------------------
    section(&mut ctx, &mut y, "storage", "hard-drive", pal.tints[1], "Storage");
    let mut card = Card::begin(&ctx, x, y, cw);
    let r = card.row(&mut ctx, 92.0);
    storage_overview(&mut ctx, r, page.sizes.as_ref(), st.thumb_cache_bytes());
    let busy = page.clearing.is_some();

    let r = card.row(&mut ctx, ROW_SUB);
    let mid = r[1] + r[3] / 2.0;
    let sub = match (&page.sizes, flash("thumbs")) {
        (_, Some(f)) => f,
        (_, None) if page.clearing == Some("thumbs") => "Clearing…".into(),
        (_, None) if storage::is_trimming() => "Removing least recently used thumbnails…".into(),
        (Some(z), None) => format!("{} · {} files", format_bytes(z.thumbs.bytes), group(z.thumbs.files as usize)),
        (None, None) => "Calculating…".into(),
    };
    let until = right - ctx.button_width("Clear");
    ctx.row_label_until(r, "Thumbnails", Some(&sub), true, until);
    if ctx.button_right(right, mid, "Clear", ButtonKind::Secondary, !busy).0 {
        acts.push(Act::Clear("thumbs"));
    }

    let r = card.row(&mut ctx, ROW_SUB);
    let mid = r[1] + r[3] / 2.0;
    let sub = match (&page.sizes, flash("tiles")) {
        (_, Some(f)) => f,
        (_, None) if page.clearing == Some("tiles") => "Clearing…".into(),
        (Some(z), None) => format!("{} · limited to 150 MB", format_bytes(z.tiles.bytes)),
        (None, None) => "Calculating…".into(),
    };
    let until = right - ctx.button_width("Clear");
    ctx.row_label_until(r, "Map tiles", Some(&sub), true, until);
    if ctx.button_right(right, mid, "Clear", ButtonKind::Secondary, !busy).0 {
        acts.push(Act::Clear("tiles"));
    }

    let r = card.row(&mut ctx, ROW_SUB);
    let mid = r[1] + r[3] / 2.0;
    let sub = if !syncing.is_empty() {
        syncing.clone()
    } else {
        match &page.sizes {
            Some(z) => format!("{} · timelines and map places for offline start", format_bytes(z.metadata.bytes)),
            None => "Calculating…".into(),
        }
    };
    ctx.row_label_until(r, "Library metadata", Some(&sub), true, right - 150.0 * s);
    let label = format!("Reload {collection_title}");
    let label = if label.chars().count() > 26 { "Reload collection".to_string() } else { label };
    if ctx.button_right(right, mid, &label, ButtonKind::Secondary, !demo && syncing.is_empty()).0 {
        acts.push(Act::Reload);
    }

    let r = card.row(&mut ctx, ROW_SUB);
    let mid = r[1] + r[3] / 2.0;
    ctx.row_label_until(r, "Thumbnail cache limit", Some("Least recently viewed thumbnails go first"), true, right - value_w - slider_w - 10.0 * s);
    let steps = LIMITS.len();
    let cur = LIMITS.iter().position(|&l| l == st.thumb_cache_mb).unwrap_or_else(|| {
        // A hand-edited value: the nearest step.
        let mb = if st.thumb_cache_mb == 0 { u32::MAX } else { st.thumb_cache_mb };
        (0..steps).min_by_key(|&i| (if LIMITS[i] == 0 { u32::MAX } else { LIMITS[i] }).abs_diff(mb)).unwrap()
    });
    let mut v = cur as f32 / (steps - 1) as f32;
    if ctx.slider("thumb_cache", right - value_w - slider_w, mid, slider_w, &mut v, steps) {
        st.thumb_cache_mb = LIMITS[(v * (steps - 1) as f32).round() as usize];
    }
    let label = match st.thumb_cache_mb {
        0 => "No limit".to_string(),
        mb => format_bytes(mb as u64 * (1 << 20)),
    };
    ctx.text(label, right, mid, 13.0, 500, colors::text_secondary(), Align::Right);

    let r = card.row(&mut ctx, ROW_SUB);
    let mid = r[1] + r[3] / 2.0;
    let confirming = page.confirm_all.is_some();
    let sub = match flash("all") {
        Some(f) => f,
        None if page.clearing == Some("all") => "Clearing…".into(),
        None if confirming => "Click again to delete every cached file".into(),
        None => "Thumbnails, map tiles and metadata download again as needed".into(),
    };
    let until = right - ctx.button_width("Clear All…");
    ctx.row_label_until(r, "Clear all caches", Some(&sub), true, until);
    let (label, kind) =
        if confirming { ("Confirm", ButtonKind::DestructiveFilled) } else { ("Clear All…", ButtonKind::Destructive) };
    if ctx.button_right(right, mid, label, kind, !busy).0 {
        if confirming {
            page.confirm_all = None;
            acts.push(Act::Clear("all"));
        } else {
            page.confirm_all = Some(now);
            let wake = page.wake.clone();
            std::thread::spawn(move || {
                std::thread::sleep(CONFIRM_FOR + Duration::from_millis(20));
                wake();
            });
        }
    }
    y = card.end(&mut ctx);
    if let Some(st) = page.server_storage.as_ref().filter(|st| st.disk.is_some() || st.quota.is_some() || st.stats.is_some()) {
        y = server_storage(&mut ctx, x, y + 18.0 * s, cw, st);
    }
    y += gap;

    // --- GPU memory ----------------------------------------------------------------------
    section(&mut ctx, &mut y, "gpu", "cpu", pal.tints[2], "Graphics Memory");
    let mut card = Card::begin(&ctx, x, y, cw);
    let r = card.row(&mut ctx, ROW_SUB);
    let mid = r[1] + r[3] / 2.0;
    let g_all = [GpuMemory::Low, GpuMemory::Normal, GpuMemory::High];
    let est = |m| [Tier::Small, Tier::Large, Tier::Xl].iter().map(|&t| atlas_bytes(atlas_dims(m, t, max_tex))).sum::<u64>();
    let sub = match st.gpu_memory {
        GpuMemory::Low => format!("About {} of video memory · suits integrated graphics", format_bytes(est(GpuMemory::Low))),
        GpuMemory::Normal => format!("About {} of video memory", format_bytes(est(GpuMemory::Normal))),
        GpuMemory::High => format!("About {} · fewer reloads when scrolling far", format_bytes(est(GpuMemory::High))),
    };
    let until = right - ctx.segmented_width(&["Low", "Normal", "High"]);
    ctx.row_label_until(r, "Thumbnail memory", Some(&sub), true, until);
    let mut i = idx(&g_all, st.gpu_memory);
    if ctx.segmented("gpu_memory", right, mid, &["Low", "Normal", "High"], &mut i) {
        st.gpu_memory = g_all[i];
    }
    let r = card.row(&mut ctx, ROW);
    let mid = r[1] + r[3] / 2.0;
    ctx.row_label(r, "In use", None, true);
    let detail = format!(
        "{}  ·  {} small, {} large, {} preview tiles",
        format_bytes(atlas_now),
        group(slots[0]),
        group(slots[1]),
        group(slots[2])
    );
    ctx.text(detail, right, mid, 12.5, 400, colors::text_secondary(), Align::Right);
    y = card.end(&mut ctx) + gap;

    // --- Server --------------------------------------------------------------------------
    section(&mut ctx, &mut y, "server", "server", pal.tints[5], "Server");
    let mut card = Card::begin(&ctx, x, y, cw);
    let r = card.row(&mut ctx, ROW);
    let mid = r[1] + r[3] / 2.0;
    ctx.row_label(r, "Immich server", None, true);
    ctx.text(server_url, right, mid, 13.0, 400, colors::text_secondary(), Align::Right);
    let r = card.row(&mut ctx, ROW_SUB);
    let mid = r[1] + r[3] / 2.0;
    let (dot, status, sub) = match &page.server {
        Server::Demo => (pal.muted, "Offline demo".to_string(), "Run with an Immich server configured to connect".to_string()),
        Server::Loading => (colors::pending(), "Connecting…".to_string(), "Asking the server for its version".to_string()),
        Server::Ok(i) => (
            colors::green(),
            format!("Connected · {} ms", i.latency.as_millis().max(1)),
            match &i.user {
                Some(u) => format!("Immich {} · signed in as {u}", i.version),
                None => format!("Immich {}", i.version),
            },
        ),
        Server::Failed(e) => (colors::red(), "Can't reach the server".to_string(), e.clone()),
    };
    let dx = r[0] + 16.0 * s;
    ctx.quad([dx, mid - 12.5 * s, 8.0 * s, 8.0 * s], rgb(dot, 1.0), 4.0 * s);
    ctx.text(status, dx + 15.0 * s, mid - 8.5 * s, 13.5, 500, theme::text(), Align::Left);
    let sub_max = cw - 200.0 * s;
    let sub = ctx.ellipsize(&sub, 11.5, 400, sub_max);
    ctx.text(sub, dx, mid + 9.5 * s, 11.5, 400, colors::text_secondary(), Align::Left);
    let testing = matches!(page.server, Server::Loading);
    let (label, kind) = match page.server {
        Server::Loading => ("Testing…", ButtonKind::Secondary),
        Server::Failed(_) => ("Try Again", ButtonKind::Primary),
        _ => ("Test Connection", ButtonKind::Secondary),
    };
    if ctx.button_right(right, mid, label, kind, !testing && !demo).0
    {
        acts.push(Act::Test);
    }
    let r = card.row(&mut ctx, ROW);
    let mid = r[1] + r[3] / 2.0;
    ctx.row_label(r, "Library", None, true);
    let counts = match (&page.server, lib_counts) {
        (Server::Ok(ServerInfo { stats: Some(st), .. }), _) => {
            format!("{} photos · {} videos", group(st.images as usize), group(st.videos as usize))
        }
        (_, Some((p, v))) => format!("{} photos · {} videos", group(p), group(v)),
        _ => "—".into(),
    };
    ctx.text(counts, right, mid, 13.0, 400, colors::text_secondary(), Align::Right);
    y = card.end(&mut ctx) + gap;

    // --- About ---------------------------------------------------------------------------
    section(&mut ctx, &mut y, "about", "info", pal.muted, "About");
    let mut card = Card::begin(&ctx, x, y, cw);
    let r = card.row(&mut ctx, ROW);
    ctx.row_label(r, "Lumen", None, true);
    ctx.text(format!("Version {}", env!("CARGO_PKG_VERSION")), right, r[1] + r[3] / 2.0, 13.0, 400, colors::text_secondary(), Align::Right);
    let r = card.row(&mut ctx, ROW);
    ctx.row_label(r, "Graphics", None, true);
    let driver = if adapter.driver.is_empty() { String::new() } else { format!(" · {}", adapter.driver) };
    let gpu = format!("{} · {:?}{driver}", adapter.name, adapter.backend);
    let gpu = ctx.ellipsize(&gpu, 13.0, 400, cw - 140.0 * s);
    ctx.text(gpu, right, r[1] + r[3] / 2.0, 13.0, 400, colors::text_secondary(), Align::Right);
    let r = card.row(&mut ctx, ROW_SUB);
    let mid = r[1] + r[3] / 2.0;
    let dir = cache_dir().display().to_string();
    let until = right - ctx.button_width("Open Folder");
    ctx.row_label_until(r, "Cache folder", Some(&dir), true, until);
    if ctx.button_right(right, mid, "Open Folder", ButtonKind::Secondary, true).0 {
        acts.push(Act::OpenFolder);
    }
    let r = card.row(&mut ctx, ROW_SUB);
    ctx.row_label(r, "Settings file", Some(&settings::path().display().to_string()), true);
    y = card.end(&mut ctx) + 36.0 * s;

    page.content = (y - (top - page.scroll * s)) / s;

    // Header over the scrolled content: opaque, with a hairline once something is beneath it.
    ctx.quad([left, 0.0, w - left, top], rgb(theme::bg(), 1.0), 0.0);
    if page.scroll > 1.0 {
        let line = s.round().max(1.0);
        ctx.quad([left, top - line, w - left, line], rgb(theme::separator(), 1.0), 0.0);
    }
    let animating = ctx.animating;
    let base = &mut core.ui.base;
    // Title lines up with the column when it's centred in a wide window.
    let tx = (x + 2.0 * s).max(left + 20.0 * s);
    base.text(Text::new("Settings", tx, 14.0 * s, 26.0 * s).weight(700).color(theme::text()));
    base.text(Text::new("Changes apply right away", tx, 48.0 * s, 13.0 * s).weight(500).color(theme::text_dim()));
    if !syncing.is_empty() {
        let t = Text::new(syncing, w - 20.0 * s, 22.0 * s, 14.0 * s).weight(500).color(theme::text_dim()).align(Align::Right);
        base.text(t);
    }
    // Drawn after the sidebar, so move the page's quads underneath it.
    let added = base.quads.len() - quads_before;
    base.quads.rotate_right(added);
    page.input.end_frame();

    // Apply and persist what changed; sliders save once they're let go.
    if core.settings != before {
        settings::apply(core);
        page_mut(core).dirty = true;
    }
    let page = page_mut(core);
    if page.dirty && !page.input.busy() {
        let s = core.settings.clone();
        page_mut(core).save(&s);
    }
    for act in acts {
        run(core, act);
    }
    animating || gliding
}

fn page_mut(core: &mut Core) -> &mut Page {
    core.settings_page.as_mut().unwrap()
}

fn run(core: &mut Core, act: Act) {
    match act {
        Act::Clear(what) => {
            let page = page_mut(core);
            page.clearing = Some(what);
            let (tx, wake) = (page.tx.clone(), page.wake.clone());
            std::thread::spawn(move || {
                let freed = match what {
                    "thumbs" => storage::clear_thumbs(),
                    "tiles" => storage::clear_tiles(),
                    _ => storage::clear_all(),
                };
                let _ = tx.send(Bg::Cleared(what, freed));
                wake();
            });
        }
        Act::Reload => {
            let c = (*core.collection).clone();
            log::info!("reloading {} from the server", c.key);
            core.loader.reload(c);
        }
        Act::Test => {
            let page = core.settings_page.as_mut().unwrap();
            page.test(&core.loader);
        }
        Act::OpenFolder => {
            let dir = cache_dir();
            let _ = std::fs::create_dir_all(&dir);
            if settings::READ_ONLY.load(std::sync::atomic::Ordering::Relaxed) {
                println!("would open {}", dir.display());
            } else if let Err(e) = std::process::Command::new("xdg-open").arg(&dir).spawn() {
                log::warn!("xdg-open {}: {e}", dir.display());
            }
        }
    }
}

/// Total, a stacked bar per cache, and a legend.
fn storage_overview(ctx: &mut Ctx, r: [f32; 4], sizes: Option<&Sizes>, limit: u64) {
    let s = ctx.s;
    let (x, w) = (r[0] + 16.0 * s, r[2] - 32.0 * s);
    let top = r[1] + 14.0 * s;
    let Some(z) = sizes else {
        ctx.text("Calculating…", x, top + 12.0 * s, 13.5, 500, colors::text_secondary(), Align::Left);
        return;
    };
    let total = z.total();
    ctx.text(format_bytes(total), x, top + 10.0 * s, 20.0, 700, theme::text(), Align::Left);
    let tw = ctx.measure(&format_bytes(total), 20.0, 700);
    ctx.text("used by Lumen on this computer", x + tw + 8.0 * s, top + 12.0 * s, 12.5, 400, colors::text_secondary(), Align::Left);
    let lim = if limit == 0 { "Thumbnails: no limit".to_string() } else { format!("Thumbnails limited to {}", format_bytes(limit)) };
    ctx.text(lim, x + w, top + 12.0 * s, 12.0, 400, colors::text_secondary(), Align::Right);

    let bar = [x, top + 30.0 * s, w, 10.0 * s];
    ctx.quad(bar, rgb(colors::track(), 1.0), 5.0 * s);
    let p = theme::palette();
    let parts = [
        (z.thumbs.bytes, p.accent, "Thumbnails"),
        (z.tiles.bytes, p.tints[3], "Map tiles"),
        (z.metadata.bytes, p.tints[2], "Metadata"),
        (z.other.bytes, p.muted, "Other"),
    ];
    // Scale against the thumbnail limit when there is one, so the bar shows headroom.
    let scale_to = if limit > 0 { (limit + z.tiles.bytes + z.metadata.bytes + z.other.bytes).max(total) } else { total };
    if scale_to > 0 {
        // Segments overlap left to right so the joins stay rounded only at the ends.
        let mut ends = Vec::new();
        let mut acc = 0.0;
        for (b, c, _) in parts {
            acc += b as f32 / scale_to as f32 * w;
            ends.push((acc, c, b));
        }
        for (end, c, b) in ends.into_iter().rev() {
            if b > 0 && end > 0.5 {
                ctx.quad([x, bar[1], end.max(bar[3]), bar[3]], rgb(c, 1.0), 5.0 * s);
            }
        }
    }
    let mut lx = x;
    let ly = bar[1] + bar[3] + 16.0 * s;
    for (b, c, name) in parts {
        if b == 0 && name == "Other" {
            continue;
        }
        ctx.quad([lx, ly - 4.0 * s, 8.0 * s, 8.0 * s], rgb(c, 1.0), 4.0 * s);
        let label = format!("{name}  {}", format_bytes(b));
        ctx.text(label.clone(), lx + 13.0 * s, ly, 12.0, 400, colors::text_secondary(), Align::Left);
        lx += 13.0 * s + ctx.measure(&label, 12.0, 400) + 22.0 * s;
    }
}

/// Storage › Server: the disk under the library, this user's usage and, for admins, the
/// library's split and per-user usage. Starts at `y` (a caption above the card); returns the
/// card's bottom.
fn server_storage(ctx: &mut Ctx, x: f32, y: f32, cw: f32, st: &ServerStorage) -> f32 {
    let s = ctx.s;
    let p = theme::palette();
    let right = x + cw - 16.0 * s;
    ctx.text("Server", x + 16.0 * s, y + 8.0 * s, 12.0, 600, theme::text_faint(), Align::Left);
    let mut card = Card::begin(ctx, x, y + 22.0 * s, cw);

    if let Some(d) = &st.disk {
        let pct = d.disk_usage_percentage.clamp(0.0, 100.0);
        let warn = pct >= DISK_WARN;
        let r = card.row(ctx, if warn { 104.0 } else { 86.0 });
        let (lx, w) = (r[0] + 16.0 * s, r[2] - 32.0 * s);
        let top = r[1] + 14.0 * s;
        let used = format_bytes(d.disk_use_raw);
        ctx.text(used.clone(), lx, top + 10.0 * s, 20.0, 700, theme::text(), Align::Left);
        let tw = ctx.measure(&used, 20.0, 700);
        let of = format!("of {} used on the server disk", format_bytes(d.disk_size_raw));
        ctx.text(of, lx + tw + 8.0 * s, top + 12.0 * s, 12.5, 400, colors::text_secondary(), Align::Left);
        let free = format!("{} free", format_bytes(d.disk_available_raw));
        let free_color = if warn { theme::text_rgba(p.red, 1.0) } else { colors::text_secondary() };
        ctx.text(free, lx + w, top + 12.0 * s, 12.5, if warn { 600 } else { 400 }, free_color, Align::Right);
        let bar = [lx, top + 30.0 * s, w, 10.0 * s];
        ctx.quad(bar, rgb(colors::track(), 1.0), 5.0 * s);
        let fill = if warn { p.red } else if pct >= 75.0 { p.tints[2] } else { p.accent };
        let fw = (w * (pct / 100.0) as f32).max(bar[3]);
        ctx.quad([bar[0], bar[1], fw, bar[3]], rgb(fill, 1.0), 5.0 * s);
        let ly = bar[1] + bar[3] + 16.0 * s;
        ctx.text(format!("{pct:.1}% full"), lx, ly, 12.0, 500, colors::text_secondary(), Align::Left);
        if warn {
            let ny = ly + 20.0 * s;
            let px = crate::widgets::even_with(16.0 * s, 15.0 * s);
            ctx.layer.icon_styled("triangle-alert", [(lx + px / 2.0).round(), ny.round()], px, rgb(p.red, 1.0), IconStyle::weight(2.0));
            let note = "Server disk almost full — uploads may fail";
            ctx.text(note, lx + px + 7.0 * s, ny, 12.5, 600, theme::text_rgba(p.red, 1.0), Align::Left);
        }
    }

    if let Some(q) = &st.quota
        && let Some(used) = q.quota_usage_in_bytes
    {
        let r = card.row(ctx, ROW_SUB);
        let mid = r[1] + r[3] / 2.0;
        let (sub, value) = match q.quota_size_in_bytes.filter(|&q| q > 0) {
            Some(quota) => {
                let pct = used as f64 / quota as f64 * 100.0;
                (format!("{pct:.0}% of your {} quota", format_bytes(quota)), format!("{} of {}", format_bytes(used), format_bytes(quota)))
            }
            None => ("Photos and videos you own · no quota".to_string(), format_bytes(used)),
        };
        let vw = ctx.measure(&value, 13.0, 500);
        ctx.row_label_until(r, "Your library", Some(&sub), true, right - vw);
        ctx.text(value, right, mid, 13.0, 500, colors::text_secondary(), Align::Right);
    }

    if let Some(t) = &st.stats {
        let r = card.row(ctx, ROW_SUB);
        let mid = r[1] + r[3] / 2.0;
        let mut sub = format!("{} photos · {} videos", group(t.photos as usize), group(t.videos as usize));
        if let (Some(ph), Some(vi)) = (t.usage_photos, t.usage_videos) {
            sub += &format!("  ·  photos {}, videos {}", format_bytes(ph), format_bytes(vi));
        }
        let value = format_bytes(t.usage);
        let vw = ctx.measure(&value, 13.0, 500);
        ctx.row_label_until(r, "Everyone's libraries", Some(&sub), true, right - vw);
        ctx.text(value, right, mid, 13.0, 500, colors::text_secondary(), Align::Right);

        if t.usage_by_user.len() > 1 {
            let mut users: Vec<&UserUsage> = t.usage_by_user.iter().collect();
            users.sort_by_key(|u| std::cmp::Reverse(u.usage));
            let max = users.first().map_or(1, |u| u.usage.max(1));
            for u in users.into_iter().take(8) {
                let r = card.row(ctx, ROW_SUB);
                let mid = r[1] + r[3] / 2.0;
                let sub = format!("{} photos · {} videos", group(u.photos as usize), group(u.videos as usize));
                let value = format_bytes(u.usage);
                // A slim bar of each user's share, right-aligned before the figure.
                let bw = (cw * 0.22).clamp(70.0 * s, 140.0 * s);
                let vx = right - 64.0 * s;
                let bx = vx - 12.0 * s - bw;
                let th = 4.0 * s;
                ctx.quad([bx, mid - th / 2.0, bw, th], rgb(colors::track(), 1.0), th / 2.0);
                let f = (u.usage as f32 / max as f32).clamp(0.0, 1.0);
                ctx.quad([bx, mid - th / 2.0, (bw * f).max(th), th], rgb(p.accent, 1.0), th / 2.0);
                ctx.row_label_until(r, &u.user_name, Some(&sub), true, bx);
                ctx.text(value, right, mid, 13.0, 500, colors::text_secondary(), Align::Right);
            }
        }
    }
    card.end(ctx)
}
