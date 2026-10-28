mod gestures;
mod grid;
mod immich;
mod library;
mod loader;
mod render;
mod sidebar;
mod ui;
mod ui_render;
mod viewer;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Fullscreen, Theme, Window, WindowId};

use grid::{Layout, Viewport};
use library::Library;
use loader::{Collection, Loader, Msg, Source, Thumb, Tier, group};
use render::{Instance, LARGE_BIT, NO_SLOT, Renderer, XL_BIT};
use ui::{Text, theme};

/// Tiles below this many physical pixels show only their thumbhash.
const SMALL_MIN: f32 = 18.0;
/// Tiles at or above this size get the 256px thumbnail instead of the 64px one.
const LARGE_MIN: f32 = 88.0;
/// Tiles at or above this size get the server's ~1440px preview.
const XL_MIN: f32 = 400.0;
const FADE: f32 = 0.22;
/// Touchpad pixels → screen pixels.
const TOUCHPAD_SPEED: f32 = 2.5;
/// Logical pixels per mouse-wheel notch.
const WHEEL_STEP: f32 = 300.0;
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
    "November", "December",
];

/// Everything except the window: shared by the app and the headless screenshot mode.
struct Core {
    loader: Loader,
    renderer: Renderer,
    library: Option<Arc<Library>>,
    generation: u64,
    layout: Layout,
    status: String,
    frame: u64,
    pending: VecDeque<Thumb>,
    instances: Vec<Instance>,
    size: (u32, u32),
    scale: f32,
    last_tick: Instant,
    smooth_scroll: f32,
    velocity: f32,
    /// Physical pixels taken by the sidebar; the grid lives to the right of it.
    left: f32,
    /// UI drawn over the grid, rebuilt every frame.
    ui: ui::Frame,
    sidebar: sidebar::Sidebar,
    /// What the grid is showing (the library, an album…).
    collection: Arc<Collection>,
    viewer: viewer::Viewer,
}

fn viewport(size: (u32, u32), scale: f32, left: f32) -> Viewport {
    Viewport { width: (size.0 as f32 - left).max(1.0), height: size.1 as f32, top: 76.0 * scale }
}

impl Core {
    fn new(loader: Loader, renderer: Renderer, size: (u32, u32), scale: f32) -> Self {
        let sidebar = sidebar::Sidebar::new(&loader);
        let viewer = viewer::Viewer::new(&loader);
        Self {
            loader,
            renderer,
            library: None,
            generation: 0,
            layout: Layout::new(viewport(size, scale, 0.0)),
            status: String::new(),
            frame: 1,
            pending: VecDeque::new(),
            instances: Vec::new(),
            size,
            scale,
            last_tick: Instant::now(),
            smooth_scroll: 0.0,
            velocity: 0.0,
            left: 0.0,
            ui: ui::Frame::default(),
            sidebar,
            collection: Arc::new(Collection::library()),
            viewer,
        }
    }

    fn resize(&mut self, size: (u32, u32), scale: f32) {
        self.size = size;
        self.scale = scale;
        self.layout.resize(viewport(size, scale, self.left));
    }

    /// Reserves `left` physical pixels on the left (e.g. for the sidebar).
    fn set_left(&mut self, left: f32) {
        if (left - self.left).abs() > 0.5 {
            self.left = left;
            self.layout.resize(viewport(self.size, self.scale, left));
        }
    }

    fn pump(&mut self) {
        while let Ok(msg) = self.loader.rx.try_recv() {
            match msg {
                Msg::Library { library, generation, collection } => {
                    // A different collection starts at its newest photos; a refresh keeps its place.
                    let refresh = self.library.is_some() && collection.key == self.collection.key;
                    self.collection = collection;
                    self.renderer.set_library(&library);
                    self.layout.reset(library.len(), refresh);
                    self.library = Some(library);
                    self.generation = generation;
                    self.pending.clear();
                }
                Msg::Status(s) => self.status = s,
                Msg::Thumb(t) if t.generation == self.generation => self.pending.push_back(t),
                Msg::Thumb(_) => {}
            }
        }
    }

    /// Advances animations and rebuilds the frame. Returns true while anything is still moving.
    fn update(&mut self, now: Instant) -> bool {
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        self.frame += 1;
        let frame = self.frame;
        self.ui.clear();
        let sidebar_moving = sidebar::update(self, now);

        // Mouse wheels glide to their target; touchpads coast after the fingers lift.
        if self.smooth_scroll.abs() > 0.5 {
            let step = self.smooth_scroll * (1.0 - (-dt * 22.0).exp());
            self.layout.scroll_by(step);
            self.smooth_scroll -= step;
        } else {
            self.smooth_scroll = 0.0;
        }
        if self.velocity.abs() > 20.0 {
            self.layout.scroll_by(self.velocity * dt);
            self.velocity *= (-dt * 2.2).exp();
        } else {
            self.velocity = 0.0;
        }

        if self.library.is_none() {
            self.instances.clear();
            self.build_header();
            return sidebar_moving;
        }
        let vp = self.layout.vp;

        // Wants come from where the grid is heading, nearest the centre first, plus a
        // screen of prefetch above and below once nothing is moving.
        let target = grid::Spec::Grid(self.layout.target);
        let settled = !self.layout.animating(now) && self.velocity == 0.0 && self.smooth_scroll == 0.0;
        let margin = if settled { vp.height } else { 0.0 };
        let mut wants: Vec<(f32, (usize, Tier))> = Vec::new();
        for r in self.layout.visible(&target, margin) {
            for i in r {
                let rect = target.rect(i, &vp);
                let Some(tier) = tier_for(rect[2]) else { continue };
                let outside = rect[1] + rect[3] < 0.0 || rect[1] > vp.height;
                // Previews are big downloads: only for what's actually on screen.
                if tier == Tier::Xl && outside {
                    continue;
                }
                // While a preview downloads, the cached thumbnail stands in for it.
                if tier == Tier::Xl && self.renderer.large.get(i).is_none() {
                    wants.push((-1.0, (i, Tier::Large)));
                }
                let atlas = match tier {
                    Tier::Small => &mut self.renderer.small,
                    Tier::Large => &mut self.renderer.large,
                    Tier::Xl => &mut self.renderer.xl,
                };
                match atlas.get(i) {
                    Some(slot) => atlas.touch(slot, frame),
                    None => {
                        let (dx, dy) = (rect[0] + rect[2] / 2.0 - vp.width / 2.0, rect[1] + rect[3] / 2.0 - vp.height / 2.0);
                        wants.push((dx * dx + dy * dy + if outside { 1e12 } else { 0.0 }, (i, tier)));
                    }
                }
            }
        }
        wants.sort_by(|a, b| a.0.total_cmp(&b.0));
        let caps = [&self.renderer.small, &self.renderer.large, &self.renderer.xl].map(|a| a.capacity() * 3 / 4);
        let mut counts = [0; 3];
        self.loader.set_wants(
            self.generation,
            wants.into_iter().map(|w| w.1).filter(|&(_, t)| {
                let k = t as usize;
                counts[k] += 1;
                counts[k] <= caps[k]
            }),
        );

        // Build this frame's tiles from the (possibly mid-zoom) on-screen layout.
        let spec = self.layout.current(now);
        self.instances.clear();
        let mut fading = false;
        for r in self.layout.visible(&spec, 0.0) {
            for i in r {
                let mut rect = spec.rect(i, &vp);
                if rect[1] > vp.height || rect[1] + rect[3] < 0.0 || rect[0] > vp.width || rect[0] + rect[2] < 0.0 {
                    continue;
                }
                let size = rect[2];
                let (mut slot, mut loaded) = (NO_SLOT, None);
                let r = &mut self.renderer;
                let xl = (size >= XL_MIN * 0.75).then(|| r.xl.get(i)).flatten();
                let large = (size >= LARGE_MIN * 0.75).then(|| r.large.get(i)).flatten();
                let small = r.small.get(i);
                if let Some(s) = xl {
                    r.xl.touch(s, frame);
                    (slot, loaded) = (s | XL_BIT, Some(r.xl.loaded_at(s)));
                } else if let Some(s) = large {
                    r.large.touch(s, frame);
                    (slot, loaded) = (s | LARGE_BIT, Some(r.large.loaded_at(s)));
                } else if let Some(s) = small {
                    r.small.touch(s, frame);
                    (slot, loaded) = (s, Some(r.small.loaded_at(s)));
                }
                // Only fade in from the blurry placeholder; a sharper tier just replaces a lower one.
                let upgrade = (xl.is_some() && (large.is_some() || small.is_some())) || (xl.is_none() && large.is_some() && small.is_some());
                let fade = if upgrade { 1.0 } else { loaded.map_or(0.0, |t| (now.duration_since(t).as_secs_f32() / FADE).min(1.0)) };
                fading |= fade < 1.0 && slot != NO_SLOT;
                // Hairline gaps when tiles are big enough to read as photos; a seamless mosaic below.
                let gap = if size < 40.0 * self.scale { 0.0 } else { (size * 0.012).clamp(1.0, 2.0 * self.scale) };
                rect = [self.left + rect[0] + gap / 2.0, rect[1] + gap / 2.0, rect[2] - gap, rect[3] - gap];
                self.instances.push(Instance { rect, th: i as u32, slot, fade: ease_out(fade), _pad: 0.0 });
            }
        }

        // Uploads last, so eviction never takes a slot touched this frame.
        let start = Instant::now();
        while let Some(t) = self.pending.front() {
            if start.elapsed() > Duration::from_millis(5) {
                break;
            }
            if !self.renderer.upload_thumb(t.index, t.tier, &t.mips, frame, now) {
                break;
            }
            self.pending.pop_front();
        }

        let (lib, overlay) = (self.library.as_ref(), &mut self.ui.overlay);
        let viewing = self.viewer.update(now, lib, &mut self.layout, self.left, self.scale, self.size, &mut self.renderer, overlay);

        self.build_header();
        sidebar_moving || viewing || fading || self.layout.animating(now) || !self.pending.is_empty() || self.velocity != 0.0 || self.smooth_scroll != 0.0
    }

    /// Handles a sidebar click (or `--select`).
    fn apply(&mut self, action: sidebar::Action) {
        match action {
            sidebar::Action::Open(c) => self.loader.open(c),
            // Placeholder until the map view lands.
            sidebar::Action::Places => log::info!("Places selected; no map view yet"),
        }
    }

    /// Title, counts and sync status over the grid's top scrim.
    fn build_header(&mut self) {
        let library = self.collection.key == "library";
        let (title, subtitle) = match &self.library {
            None => ("Lumen".to_string(), String::new()),
            Some(lib) if lib.len() == 0 && library => ("Library".to_string(), "No photos yet".to_string()),
            Some(lib) if lib.len() == 0 => (self.collection.title.clone(), "No photos".to_string()),
            Some(lib) => {
                let photos = lib.len() - lib.videos;
                let plural = |n: usize, what: &str| format!("{} {what}{}", group(n), if n == 1 { "" } else { "s" });
                let counts = match (photos, lib.videos) {
                    (p, 0) if !library => plural(p, "Photo"),
                    (0, v) if !library => plural(v, "Video"),
                    (p, v) => format!("{} · {}", plural(p, "Photo"), plural(v, "Video")),
                };
                if self.layout.is_fit_all() {
                    let years = |a: &library::Asset| a.year_month().map_or(0, |(y, _)| y);
                    let (first, last) = (years(&lib.assets[0]), years(lib.assets.last().unwrap()));
                    let span = if first == last { first.to_string() } else { format!("{first} – {last}") };
                    if library {
                        ("All Photos".to_string(), format!("{span}  ·  {counts}"))
                    } else {
                        (self.collection.title.clone(), format!("{counts}  ·  {span}"))
                    }
                } else {
                    let vp = self.layout.vp;
                    let i = self.layout.index_at(vp.width / 2.0, vp.top + 2.0).map_or(0, |(i, _, _)| i);
                    let label = lib.assets[i]
                        .year_month()
                        .map(|(y, m)| format!("{} {y}", MONTHS[(m as usize).clamp(1, 12) - 1]))
                        .unwrap_or_default();
                    if library { (label, counts) } else { (self.collection.title.clone(), format!("{counts}  ·  {label}")) }
                }
            }
        };
        let (s, x) = (self.scale, self.left + 20.0 * self.scale);
        let base = &mut self.ui.base;
        base.text(Text::new(title, x, 14.0 * s, 26.0 * s).weight(700).color(theme::TEXT));
        base.text(Text::new(subtitle, x, 48.0 * s, 13.0 * s).weight(500).color(theme::TEXT_DIM));
        if !self.status.is_empty() {
            let right = self.size.0 as f32 - 20.0 * s;
            base.text(
                Text::new(self.status.clone(), right, 22.0 * s, 14.0 * s)
                    .weight(500)
                    .color(theme::TEXT_DIM)
                    .align(ui::Align::Right),
            );
        }
    }

    fn draw(&mut self, view: &wgpu::TextureView) {
        let header = self.layout.vp.top;
        self.renderer.render(view, self.size, header, self.scale, &self.instances, &self.ui);
    }
}

fn tier_for(size: f32) -> Option<Tier> {
    if size >= XL_MIN {
        Some(Tier::Xl)
    } else if size >= LARGE_MIN {
        Some(Tier::Large)
    } else if size >= SMALL_MIN {
        Some(Tier::Small)
    } else {
        None
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

// ---------------------------------------------------------------------------------------------
// Window

struct Gpu {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    instance: wgpu::Instance,
}

struct App {
    source: Source,
    proxy: EventLoopProxy<()>,
    core: Option<Core>,
    gpu: Option<Gpu>,
    window: Option<Arc<Window>>,
    modifiers: ModifiersState,
    cursor: (f32, f32),
    zoom_accum: f32,
    last_pixel_scroll: Option<(Instant, f32)>,
    pinch: Option<Receiver<gestures::Pinch>>,
    /// ln(scale) already turned into zoom steps during the current pinch.
    pinch_applied: f64,
    /// Where the left button went down, to tell clicks from drags.
    press: Option<(f32, f32)>,
}

/// Finger-distance change (as a ratio) per grid zoom level.
const PINCH_STEP: f64 = 1.3;

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Lumen")
            .with_inner_size(winit::dpi::LogicalSize::new(1440.0, 920.0))
            .with_theme(Some(Theme::Dark));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(
            event_loop.owned_display_handle(),
        )));
        let surface = instance.create_surface(window.clone()).expect("surface");
        let (adapter, device, queue) = pollster::block_on(gpu_device(&instance, Some(&surface))).expect("gpu");
        let size = window.inner_size();
        let mut config = surface.get_default_config(&adapter, size.width.max(1), size.height.max(1)).expect("config");
        let caps = surface.get_capabilities(&adapter);
        config.format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(config.format);
        config.present_mode = wgpu::PresentMode::AutoVsync;
        surface.configure(&device, &config);

        let proxy = self.proxy.clone();
        let loader = Loader::start(self.source.clone(), move || {
            let _ = proxy.send_event(());
        });
        let renderer = Renderer::new(device, queue, config.format);

        use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        if let Ok(RawDisplayHandle::Wayland(h)) = window.display_handle().map(|h| h.as_raw()) {
            let (tx, rx) = std::sync::mpsc::channel();
            let proxy = self.proxy.clone();
            let wake = Arc::new(move || {
                let _ = proxy.send_event(());
            });
            if gestures::start(h.display.as_ptr(), tx, wake) {
                self.pinch = Some(rx);
            }
        }
        self.core = Some(Core::new(loader, renderer, (config.width, config.height), window.scale_factor() as f32));
        self.gpu = Some(Gpu { surface, config, instance });
        window.request_redraw();
        self.window = Some(window);
    }

    fn user_event(&mut self, _: &ActiveEventLoop, _: ()) {
        if let (Some(rx), Some(core)) = (&self.pinch, &mut self.core) {
            let now = Instant::now();
            let step = PINCH_STEP.ln();
            while let Ok(p) = rx.try_recv() {
                match p {
                    gestures::Pinch::Begin => self.pinch_applied = 0.0,
                    gestures::Pinch::Update { scale } if scale > 0.0 => {
                        let l = scale.ln();
                        let (ax, ay) = (self.cursor.0 - core.left, self.cursor.1);
                        while l - self.pinch_applied > step {
                            core.velocity = 0.0;
                            core.layout.zoom(-1, ax, ay, now);
                            self.pinch_applied += step;
                        }
                        while l - self.pinch_applied < -step {
                            core.velocity = 0.0;
                            core.layout.zoom(1, ax, ay, now);
                            self.pinch_applied -= step;
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let (Some(core), Some(gpu), Some(window)) = (&mut self.core, &mut self.gpu, &self.window) else { return };
        let now = Instant::now();
        let scale = window.scale_factor() as f32;
        // The viewer captures input while it's open.
        if core.viewer.is_open() && core.viewer.input(&event, self.modifiers, now) {
            window.request_redraw();
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                gpu.config.width = size.width.max(1);
                gpu.config.height = size.height.max(1);
                gpu.surface.configure(&core.renderer.device, &gpu.config);
                core.resize((gpu.config.width, gpu.config.height), scale);
                window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                core.resize(core.size, scale);
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                core.sidebar.cursor = self.cursor;
                window.request_redraw();
            }
            // Sidebar: hover, clicks, its own scrolling.
            WindowEvent::CursorLeft { .. } => {
                core.sidebar.cursor = (-1.0, -1.0);
                window.request_redraw();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. }
                if core.sidebar.contains(self.cursor, scale) =>
            {
                if let Some(action) = core.sidebar.click(scale, core.size.1 as f32) {
                    core.apply(action);
                }
                window.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } if core.sidebar.wheel(delta, scale) => window.request_redraw(),
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed => self.press = Some(self.cursor),
                ElementState::Released => {
                    let (x, y) = self.cursor;
                    let still = self.press.take().is_some_and(|p| (p.0 - x).abs() + (p.1 - y).abs() < 6.0);
                    if still && core.viewer.click_grid(core.library.as_ref(), &core.layout, core.left, x, y, now) {
                        (core.velocity, core.smooth_scroll) = (0.0, 0.0);
                        window.request_redraw();
                    }
                }
            },
            WindowEvent::MouseWheel { delta, phase, .. } => {
                let zooming = self.modifiers.control_key() || self.modifiers.super_key();
                match (delta, zooming) {
                    (MouseScrollDelta::LineDelta(_, y), true) => self.zoom_accum -= y,
                    (MouseScrollDelta::PixelDelta(p), true) => self.zoom_accum -= p.y as f32 / 40.0,
                    (MouseScrollDelta::LineDelta(_, y), false) => {
                        core.velocity = 0.0;
                        core.smooth_scroll -= y * WHEEL_STEP * scale;
                    }
                    (MouseScrollDelta::PixelDelta(p), false) => {
                        let dy = -p.y as f32 * TOUCHPAD_SPEED;
                        core.smooth_scroll = 0.0;
                        core.velocity = 0.0;
                        core.layout.scroll_by(dy);
                        // Track finger speed so we can coast when the touchpad reports lift-off.
                        if let Some((t, _)) = self.last_pixel_scroll {
                            let dt = now.duration_since(t).as_secs_f32().max(0.004);
                            let v = dy / dt;
                            let prev = self.last_pixel_scroll.map_or(v, |(_, pv)| pv);
                            self.last_pixel_scroll = Some((now, prev * 0.6 + v * 0.4));
                        } else {
                            self.last_pixel_scroll = Some((now, 0.0));
                        }
                    }
                }
                if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) && !zooming {
                    if let Some((t, v)) = self.last_pixel_scroll.take() {
                        if now.duration_since(t) < Duration::from_millis(80) {
                            core.velocity = v.clamp(-40_000.0, 40_000.0);
                        }
                    }
                }
                if self.zoom_accum.abs() >= 1.0 {
                    let steps = self.zoom_accum.trunc() as i32;
                    self.zoom_accum -= steps as f32;
                    core.velocity = 0.0;
                    core.layout.zoom(steps.signum(), self.cursor.0 - core.left, self.cursor.1, now);
                }
                window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let (cx, cy) = (core.layout.vp.width / 2.0, core.layout.vp.height / 2.0);
                let page = core.layout.vp.height * 0.85;
                match event.logical_key.as_ref() {
                    Key::Character("=" | "+") => core.layout.zoom(-1, cx, cy, now),
                    Key::Character("-" | "_") => core.layout.zoom(1, cx, cy, now),
                    Key::Character("0") => {
                        let last = core.layout.levels() - 1;
                        core.layout.zoom_to(last, cx, cy, now)
                    }
                    Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => core.smooth_scroll += page,
                    Key::Named(NamedKey::PageUp) => core.smooth_scroll -= page,
                    Key::Named(NamedKey::ArrowDown) => core.smooth_scroll += 240.0 * scale,
                    Key::Named(NamedKey::ArrowUp) => core.smooth_scroll -= 240.0 * scale,
                    Key::Named(NamedKey::Home) => core.layout.scroll_to_fraction(0.0),
                    Key::Named(NamedKey::End) => core.layout.scroll_to_fraction(1.0),
                    Key::Named(NamedKey::F11) | Key::Character("f") => {
                        let fs = window.fullscreen().is_some();
                        window.set_fullscreen((!fs).then_some(Fullscreen::Borderless(None)));
                    }
                    Key::Character("q") if self.modifiers.control_key() => event_loop.exit(),
                    Key::Character("b") if self.modifiers.control_key() => core.sidebar.toggle(),
                    _ => {}
                }
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                core.pump();
                let animating = core.update(now);
                let frame = match gpu.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                    wgpu::CurrentSurfaceTexture::Outdated => {
                        gpu.surface.configure(&core.renderer.device, &gpu.config);
                        window.request_redraw();
                        return;
                    }
                    wgpu::CurrentSurfaceTexture::Lost => {
                        gpu.surface = gpu.instance.create_surface(window.clone()).expect("surface");
                        gpu.surface.configure(&core.renderer.device, &gpu.config);
                        window.request_redraw();
                        return;
                    }
                    _ => {
                        window.request_redraw();
                        return;
                    }
                };
                let view = frame.texture.create_view(&Default::default());
                core.draw(&view);
                window.pre_present_notify();
                core.renderer.queue.present(frame);
                if animating {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

async fn gpu_device(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance),
            force_fallback_adapter: false,
            compatible_surface: surface,
            ..Default::default()
        })
        .await?;
    log::info!("GPU: {:?}", adapter.get_info());
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_limits: wgpu::Limits { max_texture_dimension_2d: 8192, ..wgpu::Limits::downlevel_defaults() }
                .using_resolution(adapter.limits()),
            ..Default::default()
        })
        .await?;
    Ok((adapter, device, queue))
}

// ---------------------------------------------------------------------------------------------
// Headless: render one frame to a PNG once everything visible has loaded.

struct Shot {
    zoom: i32,
    fit: bool,
    /// Sidebar row to click first, see `Sidebar::select`.
    select: Option<String>,
    sidebar: bool,
    viewer: Option<viewer::Shot>,
}

fn screenshot(source: Source, path: &str, size: (u32, u32), scale: f32, shot: Shot) -> Result<()> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let (_, device, queue) = pollster::block_on(gpu_device(&instance, None))?;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let renderer = Renderer::new(device, queue, format);
    let mut core = Core::new(Loader::start(source, || {}), renderer, size, scale);
    core.sidebar.set_shown(shot.sidebar);

    let settle = |core: &mut Core| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut quiet = 0;
        while quiet < 5 {
            if Instant::now() > deadline {
                bail!("timed out waiting for thumbnails");
            }
            std::thread::sleep(Duration::from_millis(30));
            core.pump();
            let busy = core.update(Instant::now());
            let done = core.library.is_some()
                && !busy
                && core.loader.is_idle()
                && core.sidebar.is_idle()
                && core.status.is_empty()
                && !core.viewer.loading();
            quiet = if done { quiet + 1 } else { 0 };
        }
        Ok(())
    };
    settle(&mut core)?;
    if let Some(spec) = &shot.select {
        if let Some(action) = core.sidebar.select(spec, size.1 as f32, scale)? {
            core.apply(action);
        }
        settle(&mut core)?;
    }
    let (cx, cy) = (core.layout.vp.width / 2.0, size.1 as f32 / 2.0);
    if shot.fit {
        core.layout.zoom_to(core.layout.levels() - 1, cx, cy, Instant::now());
    } else if shot.zoom != 0 {
        core.layout.zoom(shot.zoom, cx, cy, Instant::now());
    }
    settle(&mut core)?;
    if let Some(shot) = shot.viewer {
        let lib = core.library.clone().context("no library")?;
        let i = shot.index(lib.len());
        viewer::reveal(&mut core.layout, i);
        settle(&mut core)?;
        core.viewer.hold_chrome = true;
        core.viewer.open(lib, i, Instant::now());
        if shot.info {
            core.viewer.toggle_info();
        }
        settle(&mut core)?;
        for _ in 0..shot.next.unsigned_abs() {
            core.viewer.go(shot.next.signum(), Instant::now());
            settle(&mut core)?;
        }
        if let Some(z) = shot.zoom {
            core.viewer.zoom(z);
            settle(&mut core)?;
        }
        if let Some(ms) = shot.open_at {
            core.viewer.rewind(ms);
        }
        if let Some(ms) = shot.close_at {
            core.viewer.close_since(ms);
        }
        core.update(Instant::now());
    }

    let device = &core.renderer.device;
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    core.draw(&tex.create_view(&Default::default()));

    let device = &core.renderer.device;
    let row = (size.0 * 4).next_multiple_of(256);
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (row * size.1) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        tex.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None },
        },
        wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
    );
    core.renderer.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely())?;
    let data = buf.slice(..).get_mapped_range().expect("mapped");
    let mut img = image::RgbaImage::new(size.0, size.1);
    for y in 0..size.1 {
        let src = &data[(y * row) as usize..(y * row + size.0 * 4) as usize];
        img.as_mut()[(y * size.0 * 4) as usize..((y + 1) * size.0 * 4) as usize].copy_from_slice(src);
    }
    img.save(path)?;
    println!("wrote {path}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------

fn source_from(args: &[String]) -> Result<Source> {
    if let Some(i) = args.iter().position(|a| a == "--demo") {
        let n = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(40_000);
        return Ok(Source::Demo(n));
    }
    let file = dirs::config_dir().map(|d| d.join("lumen/config.json"));
    let cfg: Option<serde_json::Value> =
        file.as_ref().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok());
    let get = |env: &str, key: &str| {
        std::env::var(env).ok().or_else(|| cfg.as_ref()?.get(key)?.as_str().map(String::from))
    };
    let (Some(url), Some(key)) = (get("IMMICH_URL", "url"), get("IMMICH_API_KEY", "api_key")) else {
        bail!(
            "no Immich server configured.\n\nSet IMMICH_URL and IMMICH_API_KEY, or write {}\n  {{ \"url\": \"https://photos.example.com\", \"api_key\": \"…\" }}\n\nOr try it without a server:  lumen --demo 40000",
            file.map_or("~/.config/lumen/config.json".into(), |f| f.display().to_string())
        );
    };
    Ok(Source::Immich(immich::Immich::new(&url, &key)?))
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("lumen=info,wgpu=error")).init();
    let args: Vec<String> = std::env::args().collect();
    let source = source_from(&args)?;
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();

    if let Some(path) = flag("--screenshot") {
        let size = flag("--size")
            .and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
            .unwrap_or((1600, 1000));
        let scale = flag("--scale").and_then(|s| s.parse().ok()).unwrap_or(1.0);
        let shot = Shot {
            zoom: flag("--zoom").and_then(|s| s.parse().ok()).unwrap_or(0),
            fit: args.iter().any(|a| a == "--fit"),
            select: flag("--select"),
            sidebar: !args.iter().any(|a| a == "--no-sidebar"),
            viewer: flag("--open").map(|at| viewer::Shot {
                at,
                open_at: flag("--open-at").and_then(|s| s.parse().ok()),
                info: args.iter().any(|a| a == "--info"),
                next: flag("--next").and_then(|s| s.parse().ok()).unwrap_or(0),
                zoom: flag("--viewer-zoom").and_then(|s| s.parse().ok()),
                close_at: flag("--close-at").and_then(|s| s.parse().ok()),
            }),
        };
        return screenshot(source, &path, size, scale, shot);
    }

    let event_loop = EventLoop::<()>::with_user_event().build().context("event loop")?;
    let mut app = App {
        source,
        proxy: event_loop.create_proxy(),
        core: None,
        gpu: None,
        window: None,
        modifiers: ModifiersState::empty(),
        cursor: (0.0, 0.0),
        zoom_accum: 0.0,
        last_pixel_scroll: None,
        pinch: None,
        pinch_applied: 0.0,
        press: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
