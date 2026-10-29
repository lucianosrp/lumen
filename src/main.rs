mod account;
mod actions;
mod gestures;
mod grid;
mod hover;
mod icons;
mod immich;
mod library;
mod live;
mod loader;
mod login;
mod map;
mod render;
mod search;
mod settings;
mod settings_ui;
mod sidebar;
mod theme;
mod storage;
mod ui;
mod ui_render;
mod video;
mod viewer;
mod widgets;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Fullscreen, Theme, Window, WindowId};

use grid::{Layout, Viewport};
use library::Library;
use loader::{Collection, Loader, Msg, Source, Thumb, Tier, group};
use render::{Instance, LARGE_BIT, NO_SLOT, Renderer, XL_BIT};
use ui::Text;

// Tile-size thresholds for each thumbnail tier: see `settings::Quality::tiers`.
const FADE: f32 = 0.22;
/// Touchpad pixels → screen pixels (times Settings → Scroll speed).
const TOUCHPAD_SPEED: f32 = 2.5;
/// Logical pixels per mouse-wheel notch (times Settings → Scroll speed).
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
    /// The Places map, shown instead of the grid while open.
    map: Option<map::MapView>,
    /// Server push notifications ("assets changed"), when connected to Immich.
    live: Option<Receiver<()>>,
    // feat/settings: preferences, and the Settings page shown instead of the grid while open.
    settings: settings::Settings,
    settings_page: Option<settings_ui::Page>,
    // Hover previews (hover.rs): grid tiles play videos / Live Photos under a resting mouse.
    hover: hover::Hover,
    /// Settings › play on hover. Off disables all grid hover previews.
    hover_play: bool,
    /// Selection, photo actions and their popovers.
    actions: actions::Actions,
    // feat/search: the sidebar's search field, suggestions and results; and the extra header
    // height (logical px) its filter chips take.
    search: search::Search,
    header_extra: f32,
    /// The sign-in screen, shown instead of everything while signed out.
    login: Option<login::Login>,
    /// Asked for by the sign-in screen or Settings; the window rebuilds the app on it.
    switch: Option<Switch>,
}

/// Swapping the whole app for another account (or none).
pub enum Switch {
    SignIn(immich::Immich),
    SignOut,
}

fn viewport(size: (u32, u32), scale: f32, left: f32, extra: f32) -> Viewport {
    Viewport { width: (size.0 as f32 - left).max(1.0), height: size.1 as f32, top: (76.0 + extra) * scale }
}

impl Core {
    fn new(loader: Loader, renderer: Renderer, size: (u32, u32), scale: f32) -> Self {
        let sidebar = sidebar::Sidebar::new(&loader);
        let viewer = viewer::Viewer::new(&loader);
        let live = match loader.source() {
            Source::Immich(immich) => {
                // Pick the best address now, and come back to the local one when it returns.
                let i = immich.clone();
                loader.handle().spawn(async move {
                    let _ = i.connect().await;
                    i.watch_routes().await;
                });
                Some(live::start(immich.clone(), &loader.handle(), loader.waker()))
            }
            Source::Demo(_) => None,
        };
        // The renderer was built for a memory budget already; don't rebuild its atlases.
        let settings = settings::Settings { gpu_memory: renderer.gpu_memory, ..settings::Settings::load() };
        let actions = actions::Actions::new(&loader);
        theme::watch(loader.waker());
        let mut core = Self {
            loader,
            renderer,
            library: None,
            generation: 0,
            layout: Layout::new(viewport(size, scale, 0.0, 0.0)),
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
            map: None,
            live,
            settings,
            settings_page: None,
            hover: hover::Hover::new(),
            hover_play: true,
            actions,
            search: search::Search::new(),
            header_extra: 0.0,
            login: None,
            switch: None,
        };
        settings::apply(&mut core);
        core
    }

    /// Shows the sign-in screen (the core runs on an empty source meanwhile).
    fn sign_in_screen(&mut self) {
        self.login = Some(login::Login::new(self.loader.handle(), self.loader.waker()));
    }

    /// Rebuilds the app for `switch`, keeping the GPU renderer and window size.
    fn rebuild(self, switch: Switch, wake: impl Fn() + Send + Sync + 'static) -> Core {
        let (size, scale) = (self.size, self.scale);
        let Core { renderer, .. } = self;
        let source = match switch {
            Switch::SignIn(immich) => Source::Immich(immich.fresh().unwrap_or(immich)),
            Switch::SignOut => {
                if let Err(e) = account::sign_out() {
                    log::warn!("signing out: {e:#}");
                }
                Source::Demo(0)
            }
        };
        let signed_out = matches!(source, Source::Demo(0));
        let mut core = Core::new(Loader::start(source, wake), renderer, size, scale);
        if signed_out {
            core.sign_in_screen();
        }
        core
    }

    fn toggle_map(&mut self) {
        let open = self.map.is_none();
        self.set_map(open);
    }

    fn set_map(&mut self, open: bool) {
        match (self.map.take(), open) {
            (Some(m), false) => m.close(&mut self.renderer),
            (None, true) => self.map = Some(map::MapView::new(&self.loader, self.scale)),
            (m, _) => self.map = m,
        }
    }

    fn resize(&mut self, size: (u32, u32), scale: f32) {
        self.size = size;
        self.scale = scale;
        self.layout.resize(viewport(size, scale, self.left, self.header_extra));
    }

    /// Reserves `left` physical pixels on the left (e.g. for the sidebar).
    fn set_left(&mut self, left: f32) {
        if (left - self.left).abs() > 0.5 {
            self.left = left;
            self.layout.resize(viewport(self.size, self.scale, left, self.header_extra));
        }
    }

    /// Makes room under the header (search filter chips), in logical px.
    fn set_header_extra(&mut self, extra: f32) {
        if extra != self.header_extra {
            self.header_extra = extra;
            self.layout.resize(viewport(self.size, self.scale, self.left, extra));
        }
    }

    fn pump(&mut self) {
        // The server says assets changed: re-sync what's on screen (scroll position is kept).
        if self.live.as_ref().is_some_and(|rx| rx.try_iter().count() > 0) {
            self.loader.refresh((*self.collection).clone());
        }
        while let Ok(msg) = self.loader.rx.try_recv() {
            match msg {
                Msg::Library { library, generation, collection } => {
                    // A different collection starts at its newest photos; a refresh keeps its place.
                    let refresh = self.library.is_some() && collection.key == self.collection.key;
                    // Search results keep their order and start at the top.
                    self.layout.set_from_top(collection.ordered);
                    self.collection = collection;
                    if refresh {
                        // Keeps thumbnails, selection and scroll: a refresh that matches the screen is invisible.
                        actions::refresh(self, library, generation);
                        continue;
                    }
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
        actions::pump(self, Instant::now());
    }

    /// Advances animations and rebuilds the frame. Returns true while anything is still moving.
    fn update(&mut self, now: Instant) -> bool {
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        self.frame += 1;
        let frame = self.frame;
        self.ui.clear();
        if self.login.is_some() {
            return login::update(self, now);
        }
        let sidebar_moving = sidebar::update(self, now);
        let sidebar_moving = search::update(self, now) || sidebar_moving;
        // Settings › Playback: hover previews and their sound.
        self.hover_play = self.settings.hover_play;
        self.hover.sound = self.settings.hover_sound;

        // The Settings page replaces the grid, like the map below (feat/settings).
        if self.settings_page.is_some() {
            return settings_ui::update(self, now) || sidebar_moving;
        }

        // The map replaces the grid: no grid tiles drawn and no thumbnails requested.
        if let Some(map) = &mut self.map {
            self.instances.clear();
            self.loader.set_wants(self.generation, []);
            let area = [self.left, 0.0, self.size.0 as f32 - self.left, self.size.1 as f32];
            let busy = map.update(now, &mut self.renderer, self.library.as_ref(), area, self.layout.vp.top, self.scale);
            // Drawn after the sidebar, so move the map's quads underneath it.
            let before = self.ui.base.quads.len();
            map.draw(&mut self.ui);
            let added = self.ui.base.quads.len() - before;
            self.ui.base.quads.rotate_right(added);
            hover::update(self, now); // stops any grid preview

            self.build_header();
            return busy || sidebar_moving;
        }

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
        let tiers = self.settings.tiers();
        let mut wants: Vec<(f32, (usize, Tier))> = Vec::new();
        for r in self.layout.visible(&target, margin) {
            for i in r {
                let rect = target.rect(i, &vp);
                let Some(mut tier) = tier_for(rect[2], tiers) else { continue };
                // Mid-pinch sizes are transient; fetch big previews once the fingers lift.
                if tier == Tier::Xl && self.layout.pinching() {
                    tier = Tier::Large;
                }
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
                let xl = (size >= tiers.xl * 0.75).then(|| r.xl.get(i)).flatten();
                let large = (size >= tiers.large * 0.75).then(|| r.large.get(i)).flatten();
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
                let gap = if size < 40.0 * self.scale || !self.settings.gaps { 0.0 } else { (size * 0.012).clamp(1.0, 2.0 * self.scale) };
                rect = [self.left + rect[0] + gap / 2.0, rect[1] + gap / 2.0, rect[2] - gap, rect[3] - gap];
                self.instances.push(Instance { rect, th: i as u32, slot, fade: ease_out(fade), flags: self.actions.flags(i, now) });
            }
        }

        // Uploads last, so eviction never takes a slot touched this frame.
        let start = Instant::now();
        while let Some(t) = self.pending.front() {
            let budget = if self.layout.pinching() { 2 } else { 5 };
            if start.elapsed() > Duration::from_millis(budget) {
                break;
            }
            if !self.renderer.upload_thumb(t.index, t.tier, &t.mips, frame, now) {
                break;
            }
            self.pending.pop_front();
        }

        // Hover previews and tile badges, over the tiles but under the sidebar.
        let hovering = hover::update(self, now);

        let (lib, overlay) = (self.library.as_ref(), &mut self.ui.overlay);
        let viewing = self.viewer.update(now, lib, &mut self.layout, self.left, self.scale, self.size, &mut self.renderer, overlay);
        let acting = actions::draw(self, now);

        self.build_header();
        sidebar_moving || viewing || acting || fading || hovering || self.layout.animating(now) || !self.pending.is_empty() || self.velocity != 0.0 || self.smooth_scroll != 0.0
    }

    /// Handles a sidebar click (or `--select`).
    fn apply(&mut self, action: sidebar::Action) {
        match action {
            sidebar::Action::Open(c) => {
                settings_ui::leave(self);
                self.set_map(false);
                self.loader.open(c);
            }
            sidebar::Action::Places => {
                settings_ui::leave(self);
                self.set_map(true);
            }
            sidebar::Action::Settings => settings_ui::open(self),
        }
    }

    /// A map pin was clicked: show its photos in the grid.
    fn map_action(&mut self, action: map::Action) {
        let map::Action::ShowAssets { title, ids } = action;
        let key = format!("place:{title}:{}", ids.len());
        self.sidebar.set_selected(&key);
        self.set_map(false);
        self.loader.open(Collection::ids(&key, &title, ids.into_iter().collect()));
    }

    /// Title, counts and sync status over the grid's top scrim.
    fn build_header(&mut self) {
        let library = self.collection.key == "library";
        let search = search::header(self);
        let (title, subtitle) = match &self.library {
            _ if self.map.is_some() => self.map.as_ref().unwrap().header(),
            _ if search.is_some() => search.unwrap(),
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
        base.text(Text::new(title, x, 14.0 * s, 26.0 * s).weight(700).color(theme::text()));
        base.text(Text::new(subtitle, x, 48.0 * s, 13.0 * s).weight(500).color(theme::text_dim()));
        if !self.status.is_empty() {
            let right = self.size.0 as f32 - 20.0 * s;
            base.text(
                Text::new(self.status.clone(), right, 22.0 * s, 14.0 * s)
                    .weight(500)
                    .color(theme::text_dim())
                    .align(ui::Align::Right),
            );
        }
    }

    fn draw(&mut self, view: &wgpu::TextureView) {
        let header = self.layout.vp.top;
        self.renderer.render(view, self.size, header, self.scale, &self.instances, &self.ui);
    }
}

fn tier_for(size: f32, tiers: settings::Tiers) -> Option<Tier> {
    if size >= tiers.xl {
        Some(Tier::Xl)
    } else if size >= tiers.large {
        Some(Tier::Large)
    } else if size >= tiers.small {
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
    /// No key saved: start on the sign-in screen.
    signed_out: bool,
    proxy: EventLoopProxy<()>,
    core: Option<Core>,
    gpu: Option<Gpu>,
    window: Option<Arc<Window>>,
    modifiers: ModifiersState,
    cursor: (f32, f32),
    zoom_accum: f32,
    last_pixel_scroll: Option<(Instant, f32)>,
    pinch: Option<Receiver<gestures::Pinch>>,
    /// Cumulative scale at the previous pinch update, for continuous zooms.
    pinch_last: f64,
    /// Where the left button went down, to tell clicks from drags.
    press: Option<(f32, f32)>,
}



impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Lumen")
            // Wayland app id: matches lumen.desktop, so launchers and docks show its icon.
            .with_name("lumen", "lumen")
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
        let renderer = Renderer::new(device, queue, config.format, adapter.get_info(), settings::Settings::load().gpu_memory);

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
        let mut core = Core::new(loader, renderer, (config.width, config.height), window.scale_factor() as f32);
        if self.signed_out {
            core.sign_in_screen();
        }
        self.core = Some(core);
        self.gpu = Some(Gpu { surface, config, instance });
        window.request_redraw();
        self.window = Some(window);
    }

    fn user_event(&mut self, _: &ActiveEventLoop, _: ()) {
        if let (Some(rx), Some(core)) = (&self.pinch, &mut self.core) {
            let now = Instant::now();
            while let Ok(p) = rx.try_recv() {
                match p {
                    gestures::Pinch::Begin => {
                        self.pinch_last = 1.0;
                        if !core.viewer.is_open() && core.map.is_none() {
                            core.velocity = 0.0;
                            core.layout.pinch_begin(self.cursor.0 - core.left, self.cursor.1);
                        }
                    }
                    // The viewer and the map zoom continuously, following the fingers.
                    gestures::Pinch::Update { scale } if scale > 0.0 && (core.viewer.is_open() || core.map.is_some()) => {
                        let ratio = scale / self.pinch_last;
                        self.pinch_last = scale;
                        if core.viewer.is_open() {
                            core.viewer.pinch(ratio as f32, self.cursor);
                        } else if let Some(map) = &mut core.map {
                            let device_id = winit::event::DeviceId::dummy();
                            let ev = WindowEvent::PinchGesture { device_id, delta: ratio - 1.0, phase: TouchPhase::Moved };
                            map.event(&ev, self.modifiers, now);
                        }
                    }
                    gestures::Pinch::Update { scale } if scale > 0.0 => {
                        let ratio = scale / self.pinch_last;
                        self.pinch_last = scale;
                        core.layout.pinch_update(ratio as f32, now);
                    }
                    gestures::Pinch::End => core.layout.pinch_end(now),
                    _ => {}
                }
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        self.handle(event_loop, id, event);
        // Signing in or out: a fresh app for the new account, on the same window and GPU.
        if let Some(core) = self.core.take_if(|c| c.switch.is_some()) {
            let mut core = core;
            let switch = core.switch.take().unwrap();
            let proxy = self.proxy.clone();
            let core = core.rebuild(switch, move || {
                let _ = proxy.send_event(());
            });
            self.source = core.loader.source().clone();
            self.core = Some(core);
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }
}

impl App {
    fn handle(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let (Some(core), Some(gpu), Some(window)) = (&mut self.core, &mut self.gpu, &self.window) else { return };
        let now = Instant::now();
        let scale = window.scale_factor() as f32;
        if let WindowEvent::CursorMoved { position, .. } = &event {
            self.cursor = (position.x as f32, position.y as f32);
        }
        // Signed out: the sign-in screen takes all input.
        if let Some(login) = &mut core.login
            && login.event(&event, self.cursor, self.modifiers)
        {
            window.request_redraw();
            return;
        }
        if core.login.is_some() {
            match &event {
                WindowEvent::KeyboardInput { event: k, .. }
                    if k.state == ElementState::Pressed && self.modifiers.control_key() && k.logical_key == Key::Character("q".into()) =>
                {
                    return event_loop.exit();
                }
                WindowEvent::RedrawRequested
                | WindowEvent::Resized(_)
                | WindowEvent::ScaleFactorChanged { .. }
                | WindowEvent::ModifiersChanged(_)
                | WindowEvent::CloseRequested
                | WindowEvent::CursorMoved { .. } => {}
                _ => return,
            }
        }
        // The search field takes the keyboard while focused, then popovers, selection clicks and
        // action shortcuts come before everything else.
        // An address field in Settings takes typing before any shortcut.
        if let Some(page) = core.settings_page.as_mut().filter(|p| p.editing())
            && matches!(event, WindowEvent::KeyboardInput { .. })
            && matches!(page.event(&event, self.cursor, false, self.modifiers, scale), settings_ui::Response::Handled)
        {
            window.request_redraw();
            return;
        }
        if search::input(core, &event, self.modifiers) || actions::input(core, &event, self.modifiers) {
            window.request_redraw();
            return;
        }
        // The viewer captures input while it's open.
        if core.viewer.is_open() && core.viewer.input(&event, self.modifiers, now) {
            window.request_redraw();
            return;
        }
        // The map owns its area while open; the sidebar keeps its own clicks and scrolling.
        let over_sidebar = core.sidebar.contains(self.cursor, scale);
        // The Settings page owns its area while open (feat/settings).
        if let Some(page) = &mut core.settings_page {
            match page.event(&event, self.cursor, over_sidebar, self.modifiers, scale) {
                settings_ui::Response::Ignored => {}
                settings_ui::Response::Handled => return window.request_redraw(),
                settings_ui::Response::Close => {
                    settings_ui::close(core);
                    return window.request_redraw();
                }
            }
        }
        if let (Some(map), false) = (&mut core.map, over_sidebar) {
            match map.event(&event, self.modifiers, now) {
                map::Response::Ignored => {}
                map::Response::Handled => return window.request_redraw(),
                map::Response::Action(a) => {
                    core.map_action(a);
                    return window.request_redraw();
                }
            }
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
                core.hover.cursor = Some(self.cursor);
                window.request_redraw();
            }
            // Sidebar: hover, clicks, its own scrolling.
            WindowEvent::CursorLeft { .. } => {
                core.sidebar.cursor = (-1.0, -1.0);
                core.hover.cursor = None;
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
                        core.smooth_scroll -= y * WHEEL_STEP * core.settings.scroll_speed * scale;
                    }
                    (MouseScrollDelta::PixelDelta(p), false) => {
                        let dy = -p.y as f32 * TOUCHPAD_SPEED * core.settings.scroll_speed;
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
                        if now.duration_since(t) < Duration::from_millis(80) && core.settings.momentum {
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
                    Key::Named(NamedKey::Escape) => _ = search::escape(core),
                    Key::Character("b") if self.modifiers.control_key() => core.sidebar.toggle(),
                    Key::Character(",") if self.modifiers.control_key() => settings_ui::toggle(core),
                    Key::Character("m") => {
                        core.toggle_map();
                        core.sidebar.set_selected(if core.map.is_some() { "places" } else { "library" });
                        if core.map.is_none() {
                            core.loader.open(Collection::library());
                        }
                    }
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
    map: Option<map::Shot>,
    /// Simulated touchpad pinch: final scale, and whether the fingers lift.
    pinch: Option<(f32, bool)>,
    // feat/settings: `--set key=value` overrides, and `--settings` page options.
    settings: settings::Settings,
    page: Option<settings_ui::Shot>,
    /// Mouse resting on this tile (asset id, index, or screen point "x,y"; "a+b" moves on to
    /// b): render once the (last) preview plays, plus `hover_wait` ms.
    hover: Option<String>,
    hover_wait: u64,
    actions: Option<actions::Shot>,
    /// `--theme-wait <secs>`: before rendering, wait (up to that long) for the theme file to
    /// change, to check that a live palette switch recolours everything.
    theme_wait: Option<u64>,
    search: Option<search::Shot>,
}

fn screenshot(source: Source, login: Option<login::Shot>, path: &str, size: (u32, u32), scale: f32, shot: Shot) -> Result<()> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let (adapter, device, queue) = pollster::block_on(gpu_device(&instance, None))?;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let renderer = Renderer::new(device, queue, format, adapter.get_info(), shot.settings.gpu_memory);
    let prepare = |core: &mut Core| {
        core.settings = shot.settings.clone();
        settings::apply(core);
        core.sidebar.set_shown(shot.sidebar);
        // Videos only play when asked (--play-at), and never out loud.
        (core.viewer.autoplay, core.viewer.silent) = (false, true);
        (core.hover.silent, core.hover.instant) = (true, true);
    };
    let mut core = Core::new(Loader::start(source, || {}), renderer, size, scale);
    prepare(&mut core);
    // Signed out: the sign-in screen, optionally with a key pasted. Signing in carries on
    // with the rest of the shot on the real library.
    if let Some(l) = login {
        core.sign_in_screen();
        login::drive(&mut core, &l)?;
        match core.switch.take() {
            Some(switch) => {
                core = core.rebuild(switch, || {});
                prepare(&mut core);
            }
            None => return write_png(&mut core, size, format, path),
        }
    }

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
                && !core.viewer.loading()
                && core.actions.is_idle()
                && core.search.is_idle()
                && core.map.as_ref().is_none_or(|m| m.is_idle())
                && core.settings_page.as_ref().is_none_or(|p| p.is_idle());
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
    if let Some(s) = &shot.search {
        search::drive(&mut core, s, &settle)?;
    }
    let (cx, cy) = (core.layout.vp.width / 2.0, size.1 as f32 / 2.0);
    if let Some(shot) = &shot.map {
        core.toggle_map();
        let map = core.map.as_mut().unwrap();
        if let Some((lat, lon, z)) = shot.at {
            map.set_view(lat, lon, z);
        }
        settle(&mut core)?;
        let map = core.map.as_mut().unwrap();
        if let Some([x, y]) = shot.hover {
            map.hover(x, y);
        }
        if let Some([x, y]) = shot.click {
            let action = map.click(x, y);
            println!("map click at {x},{y}: {action:?}");
            if let Some(a) = action {
                core.map_action(a);
                settle(&mut core)?;
            }
        }
    } else if shot.fit {

        core.layout.zoom_to(core.layout.levels() - 1, cx, cy, Instant::now());
    } else if shot.zoom != 0 {
        core.layout.zoom(shot.zoom, cx, cy, Instant::now());
    }
    settle(&mut core)?;
    if let Some(shot) = shot.viewer {
        let lib = core.library.clone().context("no library")?;
        let i = shot.index(&lib);
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
        if let Some(t) = shot.play_at {
            core.viewer.play_at(t);
            let start = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(10));
                core.pump();
                core.update(Instant::now());
                let waited = start.elapsed();
                if shot.play_wait.map_or(core.viewer.video_ready(t), |ms| waited >= Duration::from_millis(ms)) {
                    break;
                }
                if waited > Duration::from_secs(60) {
                    bail!("timed out waiting for a video frame ({})", core.viewer.video_status());
                }
            }
            println!("{} after {:.0} ms", core.viewer.video_status(), start.elapsed().as_secs_f64() * 1000.0);
        }
        if let Some(ms) = shot.live_at {
            core.viewer.play_live();
            let start = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(10));
                core.pump();
                core.update(Instant::now());
                let (status, shown) = core.viewer.live_status();
                if shown.is_some_and(|d| d >= Duration::from_millis(ms)) {
                    println!("{status}");
                    break;
                }
                if start.elapsed() > Duration::from_secs(60) {
                    bail!("timed out waiting for the Live Photo's motion ({status})");
                }
            }
        }
        if let Some(ms) = shot.open_at {
            core.viewer.rewind(ms);
        }
        if let Some(ms) = shot.close_at {
            core.viewer.close_since(ms);
        }
        core.update(Instant::now());
    }
    if let Some(a) = &shot.actions {
        actions::drive(&mut core, a, &settle)?;
        core.pump();
        core.update(Instant::now());
    }

    if let Some(page) = &shot.page {
        settings_ui::open(&mut core);
        settle(&mut core)?;
        if let Some(at) = &page.scroll {
            core.settings_page.as_mut().unwrap().scroll_to(at);
            settle(&mut core)?;
        }
        for &at in &page.clicks {
            core.settings_page.as_mut().unwrap().click(at);
            settle(&mut core)?;
        }
        if let Some(at) = page.hover {
            // The page reads the pointer from the sidebar, which the window keeps up to date.
            core.sidebar.cursor = at;
            settle(&mut core)?;
        }
    }
    // Hover preview: rest the mouse on a tile and wait for its video to play.
    for at in shot.hover.iter().flat_map(|h| h.split('+')) {
        let point = at.split_once(',').and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?)));
        core.hover.cursor = match point {
            Some(p) => Some(p),
            None => {
                let lib = core.library.clone().context("no library")?;
                let i = viewer::Shot { at: at.to_string(), ..Default::default() }.index(&lib);
                viewer::reveal(&mut core.layout, i);
                settle(&mut core)?;
                let r = core.instances.iter().find(|t| t.th as usize == i).map(|t| t.rect).context("tile not on screen")?;
                Some((r[0] + r[2] / 2.0, r[1] + r[3] / 2.0))
            }
        };
        let start = Instant::now();
        let mut ready = None;
        loop {
            std::thread::sleep(Duration::from_millis(10));
            core.pump();
            core.update(Instant::now());
            let clip = core.hover.clip();
            if ready.is_none() && clip.is_some_and(|c| c.ready(Instant::now())) {
                ready = Some(Instant::now());
            }
            if ready.is_some_and(|t| t.elapsed() >= Duration::from_millis(shot.hover_wait)) {
                break;
            }
            if start.elapsed() > Duration::from_secs(60) {
                bail!("timed out waiting for a hover preview ({})", clip.map_or("none".into(), |c| c.status()));
            }
        }
        println!("hover: {}", core.hover.clip().map_or("none".into(), |c| c.status()));
    }

    if let Some((scale, end)) = shot.pinch {
        // Thirty finger updates over ~half a second, like a real gesture.
        core.layout.pinch_begin(cx, cy);
        for _ in 0..30 {
            core.layout.pinch_update(scale.powf(1.0 / 30.0), Instant::now());
            std::thread::sleep(Duration::from_millis(16));
            core.pump();
            core.update(Instant::now());
        }
        if end {
            core.layout.pinch_end(Instant::now());
            std::thread::sleep(Duration::from_millis(400));
        }
        core.pump();
        core.update(Instant::now());
    }
    if let Some(secs) = shot.theme_wait {
        let generation = theme::generation();
        println!("waiting up to {secs} s for the theme to change");
        let start = Instant::now();
        while theme::generation() == generation && start.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_millis(50));
            core.pump();
            core.update(Instant::now());
        }
        println!("theme generation {generation} -> {}", theme::generation());
        settle(&mut core)?;
    }
    write_png(&mut core, size, format, path)?;
    if let Some(a) = &shot.actions {
        actions::after(&mut core, a, &settle)?;
    }
    Ok(())
}

fn write_png(core: &mut Core, size: (u32, u32), format: wgpu::TextureFormat, path: &str) -> Result<()> {
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

/// The library to show, or `Demo(0)` and true when nobody is signed in (or `--login` asks
/// for the sign-in screen).
fn source_from(args: &[String]) -> Result<(Source, bool)> {
    if let Some(i) = args.iter().position(|a| a == "--demo") {
        let n = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(40_000);
        return Ok((Source::Demo(n), false));
    }
    let config = account::Config::load();
    if !config.signed_in() || args.iter().any(|a| a == "--login") {
        return Ok((Source::Demo(0), true));
    }
    Ok((Source::Immich(config.client()?), false))
}

/// Saved settings plus any `--set key=value` overrides (headless only; not saved unless
/// `--save-settings` is given).
fn settings_from(args: &[String]) -> Result<settings::Settings> {
    let save = args.iter().any(|a| a == "--save-settings");
    settings::READ_ONLY.store(!save, std::sync::atomic::Ordering::Relaxed);
    let mut s = settings::Settings::load();
    for kv in args.windows(2).filter(|w| w[0] == "--set").map(|w| &w[1]) {
        s.set(kv)?;
    }
    if save {
        s.save()?;
    }
    Ok(s)
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("lumen=info,wgpu=error")).init();
    let args: Vec<String> = std::env::args().collect();
    let (source, signed_out) = source_from(&args)?;
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
            viewer: flag("--open").or(flag("--live")).map(|at| viewer::Shot {
                at,
                open_at: flag("--open-at").and_then(|s| s.parse().ok()),
                info: args.iter().any(|a| a == "--info"),
                next: flag("--next").and_then(|s| s.parse().ok()).unwrap_or(0),
                zoom: flag("--viewer-zoom").and_then(|s| s.parse().ok()),
                close_at: flag("--close-at").and_then(|s| s.parse().ok()),
                play_at: flag("--play-at").and_then(|s| s.parse().ok()),
                play_wait: flag("--play-wait").and_then(|s| s.parse().ok()),
                live_at: flag("--live").map(|_| flag("--live-at").and_then(|s| s.parse().ok()).unwrap_or(700)),
            }),
            map: map::Shot::parse(&args),
            actions: actions::Shot::parse(&args),
            pinch: flag("--pinch").and_then(|s| {
                let (v, end) = s.strip_suffix(",end").map_or((s.as_str(), false), |v| (v, true));
                Some((v.parse().ok()?, end))
            }),
            settings: settings_from(&args)?,
            page: settings_ui::Shot::parse(&args),
            hover: flag("--hover"),
            hover_wait: flag("--hover-wait").and_then(|s| s.parse().ok()).unwrap_or(0),
            theme_wait: flag("--theme-wait").and_then(|s| s.parse().ok()),
            search: search::Shot::parse(&args),
        };
        let login = signed_out.then(|| login::Shot::parse(&args));
        return screenshot(source, login, &path, size, scale, shot);
    }

    let event_loop = EventLoop::<()>::with_user_event().build().context("event loop")?;
    let mut app = App {
        source,
        signed_out,
        proxy: event_loop.create_proxy(),
        core: None,
        gpu: None,
        window: None,
        modifiers: ModifiersState::empty(),
        cursor: (0.0, 0.0),
        zoom_accum: 0.0,
        last_pixel_scroll: None,
        pinch: None,
        pinch_last: 1.0,
        press: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
