//! Fullscreen photo viewer: grows out of a grid tile, loads progressively (thumbhash →
//! thumbnail → preview → original on deep zoom), and lets you zoom, pan and page through.

use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use image::{DynamicImage, ImageDecoder, ImageReader, imageops::FilterType};
use serde::Deserialize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::grid::{Layout, Spec};
use crate::library::{self, Library};
use crate::loader::{Loader, Source, thumbnail_bytes};
use crate::render::Renderer;
use crate::ui::{Align, Layer, Quad, Text, contains, rgb, theme};

const OPEN_TIME: f32 = 0.30;
const CLOSE_TIME: f32 = 0.28;
const SLIDE_TIME: f32 = 0.26;
const FADE_TIME: f32 = 0.22;
/// Longest wait for the thumbnail before the open animation starts anyway.
const OPEN_WAIT: Duration = Duration::from_millis(120);
const CHROME_IDLE: Duration = Duration::from_secs(2);
/// Neighbours kept decoded on each side of the current photo.
const KEEP: usize = 2;
/// Decoded originals are capped here to bound GPU memory (~130 MB with mips).
const FULL_MAX: u32 = 6144;

const HASH: usize = 0;
const THUMB: usize = 1;
const PREVIEW: usize = 2;
const FULL: usize = 3;
const INFO: usize = 4;

const WEEKDAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
    "November", "December",
];

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct Details {
    local_date_time: Option<String>,
    original_file_name: Option<String>,
    original_mime_type: Option<String>,
    /// "0:01:05.123000" on older servers, milliseconds on newer ones.
    duration: Option<serde_json::Value>,
    exif_info: Option<Exif>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct Exif {
    city: Option<String>,
    state: Option<String>,
    country: Option<String>,
    make: Option<String>,
    model: Option<String>,
    lens_model: Option<String>,
    f_number: Option<f64>,
    exposure_time: Option<String>,
    iso: Option<f64>,
    focal_length: Option<f64>,
    file_size_in_byte: Option<u64>,
    exif_image_width: Option<f64>,
    exif_image_height: Option<f64>,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

enum Loaded {
    Image { id: String, kind: usize, w: u32, h: u32, rgba: Vec<u8> },
    Info { id: String, details: Box<Details> },
    Failed { id: String, kind: usize },
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Load {
    #[default]
    Idle,
    Busy,
    Done,
    Failed,
}

struct Tex {
    id: crate::ui::ImageId,
    w: u32,
    h: u32,
    at: Instant,
}

#[derive(Default)]
struct Slot {
    tex: [Option<Tex>; 4],
    state: [Load; 5],
    info: Option<Details>,
}

impl Slot {
    fn settled(&self, kind: usize) -> bool {
        matches!(self.state[kind], Load::Done | Load::Failed)
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// Waiting briefly for the thumbnail so frame 0 already looks like the tile.
    Waiting(Instant),
    Opening(Instant),
    Shown,
    Closing { start: Instant, img: [f32; 4], clip: [f32; 4], to_tile: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Close,
    Info,
    Play,
    Panel,
}

struct Open {
    lib: Arc<Library>,
    index: usize,
    phase: Phase,
    /// Zoom relative to fit, and pan of the image centre from the screen centre; `_s` is displayed.
    zoom: f32,
    zoom_s: f32,
    pan: [f32; 2],
    pan_s: [f32; 2],
    slide: Option<(usize, f32, Instant)>,
    panel: bool,
    chrome: f32,
    active_at: Instant,
    cursor: (f32, f32),
    press: Option<((f32, f32), [f32; 2])>,
    dragged: bool,
    last_click: Option<(Instant, (f32, f32))>,
    swipe: f32,
    swipe_done: bool,
    reveal: bool,
    /// Last frame's on-screen image and its visible part, for closing and hit tests.
    img: [f32; 4],
    clip: [f32; 4],
    hits: Vec<(Hit, [f32; 4])>,
    screen: (f32, f32),
    /// Width of the original in pixels, for 1:1 zoom.
    native_width: f32,
}

pub struct Viewer {
    source: Source,
    rt: tokio::runtime::Handle,
    wake: Arc<dyn Fn() + Send + Sync>,
    tx: Sender<Loaded>,
    rx: Receiver<Loaded>,
    open: Option<Open>,
    slots: HashMap<String, Slot>,
    queue: VecDeque<Loaded>,
    last_tick: Instant,
    /// Keeps the chrome visible regardless of mouse activity (headless screenshots).
    pub hold_chrome: bool,
}

/// What headless screenshot mode should show.
pub struct Shot {
    pub at: String,
    pub open_at: Option<u64>,
    pub info: bool,
    pub next: i32,
    pub zoom: Option<f32>,
    pub close_at: Option<u64>,
}

impl Shot {
    /// "newest", "oldest", an index, or a negative index from the newest end.
    pub fn index(&self, n: usize) -> usize {
        let last = n.saturating_sub(1) as i64;
        let i = match self.at.as_str() {
            "newest" => last,
            "oldest" => 0,
            s => s.parse::<i64>().map_or(last, |i| if i < 0 { last + 1 + i } else { i }),
        };
        i.clamp(0, last) as usize
    }
}

impl Viewer {
    pub fn new(loader: &Loader) -> Self {
        let (tx, rx) = channel();
        Self {
            source: loader.source().clone(),
            rt: loader.handle(),
            wake: loader.waker(),
            tx,
            rx,
            open: None,
            slots: HashMap::new(),
            queue: VecDeque::new(),
            last_tick: Instant::now(),
            hold_chrome: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn open(&mut self, lib: Arc<Library>, index: usize, now: Instant) {
        if index >= lib.len() {
            return;
        }
        self.open = Some(Open {
            lib,
            index,
            phase: Phase::Waiting(now),
            zoom: 1.0,
            zoom_s: 1.0,
            pan: [0.0; 2],
            pan_s: [0.0; 2],
            slide: None,
            panel: false,
            chrome: 0.0,
            active_at: now,
            cursor: (0.0, 0.0),
            press: None,
            dragged: false,
            last_click: None,
            swipe: 0.0,
            swipe_done: false,
            reveal: false,
            img: [0.0; 4],
            clip: [0.0; 4],
            hits: Vec::new(),
            screen: (1.0, 1.0),
            native_width: 0.0,
        });
    }

    /// Opens the tile under a click at physical (x, y). Returns true if one was hit.
    pub fn click_grid(&mut self, lib: Option<&Arc<Library>>, layout: &Layout, left: f32, x: f32, y: f32, now: Instant) -> bool {
        let Some(lib) = lib else { return false };
        let Some((i, _, _)) = layout.index_at(x - left, y) else { return false };
        if x < left || !contains(Spec::Grid(layout.target).rect(i, &layout.vp), x - left, y) {
            return false;
        }
        self.open(lib.clone(), i, now);
        if let Some(o) = &mut self.open {
            o.cursor = (x, y);
        }
        true
    }

    /// Headless: restarts the open animation as if it began `ms` ago.
    pub fn rewind(&mut self, ms: u64) {
        if let Some(o) = &mut self.open {
            o.phase = Phase::Opening(Instant::now() - Duration::from_millis(ms));
        }
    }

    /// Touchpad pinch: zooms by `factor` around `at` (physical px).
    pub fn pinch(&mut self, factor: f32, at: (f32, f32)) {
        if let Some(o) = &mut self.open {
            o.zoom_by(factor, at);
        }
    }

    /// Headless: zooms by `factor` around the centre.
    pub fn zoom(&mut self, factor: f32) {
        if let Some(o) = &mut self.open {
            let c = (o.screen.0 / 2.0, o.screen.1 / 2.0);
            o.zoom_by(factor, c);
        }
    }

    /// Headless: starts closing as if `ms` ago.
    pub fn close_since(&mut self, ms: u64) {
        let now = Instant::now() - Duration::from_millis(ms);
        self.close(now);
    }

    pub fn toggle_info(&mut self) {
        if let Some(o) = &mut self.open {
            o.panel = !o.panel;
        }
    }

    /// True while the current photo's images or details are still on their way.
    pub fn loading(&self) -> bool {
        let Some(o) = &self.open else { return false };
        let busy = self
            .slots
            .get(&o.lib.assets[o.index].id)
            .is_none_or(|s| !(s.settled(THUMB) && s.settled(PREVIEW) && s.settled(INFO)) || s.state[FULL] == Load::Busy);
        busy || !self.queue.is_empty()
    }

    /// Moves `delta` photos through the library with a slide.
    pub fn go(&mut self, delta: i32, now: Instant) {
        let Some(o) = &mut self.open else { return };
        if !matches!(o.phase, Phase::Shown | Phase::Opening(_)) {
            return;
        }
        let to = (o.index as i64 + delta as i64).clamp(0, o.lib.len() as i64 - 1) as usize;
        if to == o.index {
            return;
        }
        o.slide = Some((o.index, delta.signum() as f32, now));
        o.index = to;
        (o.zoom, o.zoom_s, o.pan, o.pan_s) = (1.0, 1.0, [0.0; 2], [0.0; 2]);
        o.last_click = None;
        o.reveal = true;
    }

    fn close(&mut self, now: Instant) {
        let Some(o) = &mut self.open else { return };
        if let Phase::Closing { .. } = o.phase {
            return;
        }
        o.phase = Phase::Closing { start: now, img: o.img, clip: o.clip, to_tile: true };
        o.slide = None;
        o.panel = false;
    }

    /// Routes window input while open. Returns true if the event was consumed.
    pub fn input(&mut self, event: &WindowEvent, mods: ModifiersState, now: Instant) -> bool {
        let Some(o) = &mut self.open else { return false };
        let closing = matches!(o.phase, Phase::Closing { .. });
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let p = (position.x as f32, position.y as f32);
                if (p.0 - o.cursor.0).abs() + (p.1 - o.cursor.1).abs() > 0.5 {
                    o.active_at = now;
                }
                o.cursor = p;
                if let Some((start, pan)) = o.press {
                    o.dragged |= (p.0 - start.0).abs() + (p.1 - start.1).abs() > 5.0;
                    if o.dragged && o.zoom > 1.001 {
                        o.pan = [pan[0] + p.0 - start.0, pan[1] + p.1 - start.1];
                        o.clamp_pan();
                        o.pan_s = o.pan;
                    }
                }
                false
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } if !closing => {
                match state {
                    ElementState::Pressed => {
                        o.press = Some((o.cursor, o.pan));
                        o.dragged = false;
                    }
                    ElementState::Released => {
                        let clicked = o.press.take().is_some() && !o.dragged;
                        if clicked {
                            self.click(now);
                        }
                    }
                }
                true
            }
            WindowEvent::MouseWheel { delta, phase, .. } if !closing => {
                o.active_at = now;
                let c = o.cursor;
                match *delta {
                    MouseScrollDelta::LineDelta(_, y) => o.zoom_by(1.25f32.powf(y), c),
                    MouseScrollDelta::PixelDelta(p) => {
                        let (x, y) = (p.x as f32, p.y as f32);
                        if mods.control_key() || mods.super_key() {
                            o.zoom_by((y * 0.01).exp(), c);
                        } else if o.zoom > 1.001 {
                            o.pan = [o.pan[0] + x * 2.0, o.pan[1] + y * 2.0];
                            o.clamp_pan();
                            o.pan_s = o.pan;
                        } else if x.abs() > y.abs() {
                            o.swipe -= x;
                        } else {
                            o.zoom_by((y * 0.006).exp(), c);
                        }
                    }
                }
                if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                    o.swipe = 0.0;
                    o.swipe_done = false;
                }
                let threshold = 70.0 * o.screen.1 / 900.0;
                if !o.swipe_done && o.swipe.abs() > threshold {
                    let dir = o.swipe.signum() as i32;
                    o.swipe_done = true;
                    o.swipe = 0.0;
                    self.go(dir, now);
                }
                true
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed || closing {
                    return true;
                }
                o.active_at = now;
                let center = (o.screen.0 / 2.0, o.screen.1 / 2.0);
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Escape | NamedKey::Backspace) => self.close(now),
                    Key::Named(NamedKey::ArrowLeft) => self.go(-1, now),
                    Key::Named(NamedKey::ArrowRight) => self.go(1, now),
                    Key::Named(NamedKey::Space | NamedKey::Enter) => self.play(),
                    Key::Character("i") => o.panel = !o.panel,
                    Key::Character("=" | "+") => o.zoom_by(1.5, center),
                    Key::Character("-" | "_") => o.zoom_by(1.0 / 1.5, center),
                    Key::Character("0") => o.zoom_by(0.0, center),
                    Key::Character("f") | Key::Named(NamedKey::F11) => return false,
                    Key::Character("q") if mods.control_key() => return false,
                    _ => {}
                }
                true
            }
            WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } => true,
            _ => false,
        }
    }

    fn click(&mut self, now: Instant) {
        let Some(o) = &mut self.open else { return };
        let p = o.cursor;
        o.active_at = now;
        match o.hits.iter().find(|(_, r)| contains(*r, p.0, p.1)).map(|h| h.0) {
            Some(Hit::Close) => self.close(now),
            Some(Hit::Info) => o.panel = !o.panel,
            Some(Hit::Play) => self.play(),
            Some(Hit::Panel) => {}
            None if contains(o.clip, p.0, p.1) => {
                let double = o.last_click.is_some_and(|(t, q)| {
                    now.duration_since(t) < Duration::from_millis(350) && (q.0 - p.0).abs() + (q.1 - p.1).abs() < 10.0
                });
                if double {
                    o.last_click = None;
                    let one = o.one_to_one();
                    o.zoom_by(if o.zoom > 1.01 { 0.0 } else { one.max(2.0) / o.zoom }, p);
                } else {
                    o.last_click = Some((now, p));
                }
            }
            None => self.close(now),
        }
    }

    /// Streams a video in mpv; the viewer itself only shows the poster.
    fn play(&mut self) {
        let Some(o) = &self.open else { return };
        let asset = &o.lib.assets[o.index];
        let Source::Immich(immich) = &self.source else { return };
        if !asset.video {
            return;
        }
        let url = format!("{}/api/assets/{}/video/playback", immich.base(), asset.id);
        let spawned = std::process::Command::new("mpv")
            .arg(format!("--http-header-fields=x-api-key: {}", immich.api_key()))
            .arg("--force-window=immediate")
            .arg(format!("--title={}", asset.taken.get(..10).unwrap_or("Video")))
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            // Reap it in the background so it doesn't linger as a zombie.
            Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
            Err(e) => log::warn!("couldn't start mpv: {e}"),
        }
    }

    /// Advances animations, streams images in and draws into `out`. Returns true while moving.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        now: Instant,
        lib: Option<&Arc<Library>>,
        layout: &mut Layout,
        left: f32,
        scale: f32,
        size: (u32, u32),
        r: &mut Renderer,
        out: &mut Layer,
    ) -> bool {
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        let Some(mut o) = self.open.take() else {
            if !self.slots.is_empty() {
                self.free_all(r);
            }
            while self.rx.try_recv().is_ok() {}
            self.queue.clear();
            return false;
        };
        o.screen = (size.0 as f32, size.1 as f32);

        // A new library (sync refresh, another album): follow the photo if it's still there.
        if let Some(lib) = lib.filter(|l| !Arc::ptr_eq(l, &o.lib)) {
            let id = &o.lib.assets[o.index].id;
            match lib.assets.iter().position(|a| &a.id == id) {
                Some(i) => {
                    o.lib = lib.clone();
                    o.index = i;
                    o.slide = None;
                    o.reveal = true;
                }
                None => {
                    o.phase = match o.phase {
                        Phase::Closing { start, img, clip, .. } => Phase::Closing { start, img, clip, to_tile: false },
                        _ => Phase::Closing { start: now, img: o.img, clip: o.clip, to_tile: false },
                    };
                }
            }
        }
        let current = o.lib.assets[o.index].id.clone();

        self.receive(r, &current, now);

        // Phase transitions.
        let slot = self.slots.get(&current);
        match o.phase {
            Phase::Waiting(t) if slot.is_some_and(|s| s.settled(THUMB)) || now.duration_since(t) > OPEN_WAIT => {
                o.phase = Phase::Opening(now)
            }
            Phase::Opening(t) if now.duration_since(t).as_secs_f32() >= OPEN_TIME => o.phase = Phase::Shown,
            Phase::Closing { start, .. } if now.duration_since(start).as_secs_f32() >= CLOSE_TIME => {
                self.free_all(r);
                return true;
            }
            _ => {}
        }
        if o.slide.is_some_and(|(_, _, t)| now.duration_since(t).as_secs_f32() >= SLIDE_TIME) {
            o.slide = None;
        }

        // Scroll the grid (hidden behind us) so closing lands on the current photo's tile.
        if std::mem::take(&mut o.reveal) {
            reveal(layout, o.index);
        }

        self.keep(&o, now, r);
        self.request(&o);

        let busy = self.draw(&mut o, now, dt, layout, left, scale, r, out);
        self.open = Some(o);
        busy || !self.queue.is_empty()
    }

    /// Uploads finished decodes: every small one, but at most one big one per frame.
    fn receive(&mut self, r: &mut Renderer, current: &str, now: Instant) {
        self.queue.extend(self.rx.try_iter());
        let mut big = 0;
        while let Some(msg) = self.queue.pop_front() {
            match msg {
                Loaded::Image { id, kind, w, h, rgba } => {
                    let Some(slot) = self.slots.get_mut(&id) else { continue };
                    if kind == FULL && id != current {
                        slot.state[FULL] = Load::Idle;
                        continue;
                    }
                    if w * h > 300_000 {
                        if big > 0 {
                            self.queue.push_front(Loaded::Image { id, kind, w, h, rgba });
                            break;
                        }
                        big += 1;
                    }
                    let image = r.ui.create_image(&r.device, &r.queue, w, h, &rgba);
                    if let Some(old) = slot.tex[kind].replace(Tex { id: image, w, h, at: now }) {
                        r.ui.free_image(old.id);
                    }
                    slot.state[kind] = Load::Done;
                }
                Loaded::Info { id, details } => {
                    if let Some(slot) = self.slots.get_mut(&id) {
                        slot.info = Some(*details);
                        slot.state[INFO] = Load::Done;
                    }
                }
                Loaded::Failed { id, kind } => {
                    if let Some(slot) = self.slots.get_mut(&id) {
                        slot.state[kind] = Load::Failed;
                    }
                }
            }
        }
    }

    /// Keeps slots for the current photo ± KEEP (plus whatever is sliding out) and frees the rest.
    fn keep(&mut self, o: &Open, now: Instant, r: &mut Renderer) {
        let lib = &o.lib;
        let window = o.index.saturating_sub(KEEP)..(o.index + KEEP + 1).min(lib.len());
        let mut keep: Vec<usize> = window.collect();
        if let Some((from, _, _)) = o.slide {
            keep.push(from);
        }
        let ids: Vec<&str> = keep.iter().map(|&i| lib.assets[i].id.as_str()).collect();
        self.slots.retain(|id, slot| {
            let kept = ids.contains(&id.as_str());
            for (k, tex) in slot.tex.iter_mut().enumerate() {
                if (!kept || (k == FULL && *id != lib.assets[o.index].id))
                    && let Some(t) = tex.take()
                {
                    r.ui.free_image(t.id);
                    slot.state[k] = Load::Idle;
                }
            }
            kept
        });
        for &i in &keep {
            let asset = &lib.assets[i];
            if self.slots.contains_key(&asset.id) {
                continue;
            }
            let mut slot = Slot::default();
            if let Some((w, h, rgba)) = decode_thumbhash(asset.thumbhash.as_deref()) {
                // Already "faded in": it's the instant placeholder.
                let id = r.ui.create_image(&r.device, &r.queue, w, h, &rgba);
                slot.tex[HASH] = Some(Tex { id, w, h, at: now - Duration::from_secs(1) });
            }
            slot.state[HASH] = Load::Done;
            self.slots.insert(asset.id.clone(), slot);
        }
    }

    /// Starts fetches in priority order: the current photo first, then its neighbours'
    /// previews, two at a time so they don't starve it of bandwidth.
    fn request(&mut self, o: &Open) {
        let lib = &o.lib;
        let i = o.index;
        let mut order = vec![(i, THUMB), (i, INFO), (i, PREVIEW)];
        for d in 1..=KEEP {
            for j in [i.checked_add(d), i.checked_sub(d)].into_iter().flatten().filter(|&j| j < lib.len()) {
                order.extend([(j, THUMB), (j, PREVIEW), (j, INFO)]);
            }
        }
        let mut previews = self.slots.values().filter(|s| s.state[PREVIEW] == Load::Busy).count();
        for (j, kind) in order {
            let asset = &lib.assets[j];
            let Some(slot) = self.slots.get_mut(&asset.id) else { continue };
            if slot.state[kind] != Load::Idle {
                continue;
            }
            if kind == PREVIEW {
                if previews >= 2 {
                    continue;
                }
                previews += 1;
            }
            slot.state[kind] = Load::Busy;
            self.spawn(asset.id.clone(), j, kind, None);
        }

        // Past the preview's resolution, fetch the original (images only, on demand).
        let asset = &lib.assets[i];
        let Some(slot) = self.slots.get_mut(&asset.id) else { return };
        let Some(preview) = slot.tex[PREVIEW].as_ref() else { return };
        let fit = fit_rect(preview.w as f32 / preview.h as f32, o.screen);
        if !asset.video && slot.state[FULL] == Load::Idle && fit[2] * o.zoom > preview.w as f32 * 1.2 {
            slot.state[FULL] = Load::Busy;
            let mime = slot.info.as_ref().and_then(|d| d.original_mime_type.clone()).unwrap_or_default();
            let hint = FullHint { mime, aspect: preview.w as f32 / preview.h as f32, preview_w: preview.w };
            self.spawn(asset.id.clone(), i, FULL, Some(hint));
        }
    }

    fn spawn(&self, id: String, index: usize, kind: usize, hint: Option<FullHint>) {
        let (source, tx, wake) = (self.source.clone(), self.tx.clone(), self.wake.clone());
        self.rt.spawn(async move {
            let msg = match fetch(&source, &id, index, kind, hint).await {
                Ok(msg) => msg,
                Err(e) => {
                    if let Source::Immich(_) = source {
                        log::warn!("viewer {}: {e:#}", ["thumbhash", "thumbnail", "preview", "original", "details"][kind]);
                    }
                    Loaded::Failed { id, kind }
                }
            };
            let _ = tx.send(msg);
            wake();
        });
    }

    fn free_all(&mut self, r: &mut Renderer) {
        for (_, slot) in self.slots.drain() {
            for t in slot.tex.into_iter().flatten() {
                r.ui.free_image(t.id);
            }
        }
    }

    fn aspect(&self, lib: &Library, i: usize) -> f32 {
        let slot = self.slots.get(&lib.assets[i].id);
        match slot.and_then(|s| s.tex[PREVIEW].as_ref().or(s.tex[FULL].as_ref())) {
            Some(t) => t.w as f32 / t.h as f32,
            None => lib.assets[i].ratio,
        }
        .clamp(0.05, 20.0)
    }

    /// Image layers from the blurriest up; each fades in over the ones below it.
    fn draw_asset(&self, out: &mut Layer, id: &str, img: [f32; 4], clip: [f32; 4], alpha: f32, now: Instant) -> bool {
        if clip[2] <= 0.0 || clip[3] <= 0.0 || img[2] <= 0.0 || img[3] <= 0.0 {
            return false;
        }
        let uv = [
            (clip[0] - img[0]) / img[2],
            (clip[1] - img[1]) / img[3],
            (clip[0] + clip[2] - img[0]) / img[2],
            (clip[1] + clip[3] - img[1]) / img[3],
        ];
        let Some(slot) = self.slots.get(id) else { return false };
        let fade = |t: &Tex| (now.duration_since(t.at).as_secs_f32() / FADE_TIME).min(1.0);
        let base = (0..4).rev().find(|&k| slot.tex[k].as_ref().is_some_and(|t| fade(t) >= 1.0));
        if base.is_none() {
            out.quad(Quad::solid(clip, rgb(0x26262a, alpha), 0.0));
        }
        let mut fading = false;
        for k in base.unwrap_or(0)..4 {
            let Some(t) = &slot.tex[k] else { continue };
            let f = fade(t);
            fading |= f < 1.0;
            out.quad(Quad::image(clip, t.id, uv, alpha * ease_out(f), 0.0));
        }
        fading
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &self,
        o: &mut Open,
        now: Instant,
        dt: f32,
        layout: &Layout,
        left: f32,
        s: f32,
        r: &mut Renderer,
        out: &mut Layer,
    ) -> bool {
        let (w, h) = o.screen;
        let screen = [0.0, 0.0, w, h];
        let lib = o.lib.clone();
        let asset = &lib.assets[o.index];
        let a = self.aspect(&o.lib, o.index);
        let mut busy = false;
        let slot = self.slots.get(&asset.id);
        let exif = slot.and_then(|s| s.info.as_ref()?.exif_info.as_ref());
        o.native_width = match exif.and_then(|e| Some((e.exif_image_width?, e.exif_image_height?))) {
            Some((ew, eh)) => (if (ew >= eh) == (a >= 1.0) { ew } else { eh }) as f32,
            None => slot.and_then(|s| s.tex[FULL].as_ref().or(s.tex[PREVIEW].as_ref())).map_or(0.0, |t| t.w as f32),
        };

        // Smoothly approach zoom/pan targets.
        let k = 1.0 - (-dt * 18.0).exp();
        o.zoom_s += (o.zoom - o.zoom_s) * k;
        for (ps, p) in o.pan_s.iter_mut().zip(o.pan) {
            *ps += (p - *ps) * k;
        }
        if (o.zoom_s - o.zoom).abs() < 0.001 && (o.pan_s[0] - o.pan[0]).abs() + (o.pan_s[1] - o.pan[1]).abs() < 0.5 {
            (o.zoom_s, o.pan_s) = (o.zoom, o.pan);
        } else {
            busy = true;
        }
        let fit = fit_rect(a, o.screen);
        let (iw, ih) = (fit[2] * o.zoom_s, fit[3] * o.zoom_s);
        let shown_img = [w / 2.0 + o.pan_s[0] - iw / 2.0, h / 2.0 + o.pan_s[1] - ih / 2.0, iw, ih];
        let shown = (shown_img, intersect(shown_img, screen));

        // The tile this photo occupies in the grid right now, as (whole image, visible square).
        let tile = {
            let vp = layout.vp;
            let t = layout.current(now).rect(o.index, &vp);
            let gap = if t[2] < 40.0 * s { 0.0 } else { (t[2] * 0.012).clamp(1.0, 2.0 * s) };
            let t = [left + t[0] + gap / 2.0, t[1] + gap / 2.0, t[2] - gap, t[3] - gap];
            let visible = t[1] + t[3] > vp.top * 0.5 && t[1] < h && t[0] + t[2] > left && t[0] < w;
            let (tw, th) = if a >= 1.0 { (t[3] * a, t[3]) } else { (t[2], t[2] / a) };
            let img = [t[0] + t[2] / 2.0 - tw / 2.0, t[1] + t[3] / 2.0 - th / 2.0, tw, th];
            visible.then_some((img, t))
        };

        let (img, clip, alpha, backdrop) = match o.phase {
            Phase::Waiting(_) => {
                busy = true;
                match tile {
                    Some((img, t)) => (img, t, 1.0, 0.0),
                    None => (shown.0, shown.1, 0.0, 0.0),
                }
            }
            Phase::Opening(start) => {
                busy = true;
                let e = ease_out((now.duration_since(start).as_secs_f32() / OPEN_TIME).min(1.0));
                match tile {
                    Some((img, t)) => (lerp4(img, shown.0, e), lerp4(t, shown.1, e), 1.0, e),
                    None => (scale_about(shown.0, 0.9 + 0.1 * e), scale_about(shown.1, 0.9 + 0.1 * e), e, e),
                }
            }
            Phase::Shown => (shown.0, shown.1, 1.0, 1.0),
            Phase::Closing { start, img, clip, to_tile } => {
                busy = true;
                let e = ease_out((now.duration_since(start).as_secs_f32() / CLOSE_TIME).min(1.0));
                match tile.filter(|_| to_tile) {
                    Some((ti, t)) => (lerp4(img, ti, e), lerp4(clip, t, e), 1.0, 1.0 - e),
                    None => {
                        let k = 1.0 - 0.12 * e;
                        (scale_about(img, k), scale_about(clip, k), 1.0 - e, 1.0 - e)
                    }
                }
            }
        };
        o.img = shown.0;
        o.clip = shown.1;
        if !matches!(o.phase, Phase::Closing { .. }) {
            o.img = img;
            o.clip = clip;
        }

        out.quad(Quad::solid(screen, rgb(0x000000, backdrop), 0.0));

        // Paging slides the old photo out and the new one in.
        let mut dx = 0.0;
        if let Some((from, dir, start)) = o.slide {
            busy = true;
            let e = ease_out((now.duration_since(start).as_secs_f32() / SLIDE_TIME).min(1.0));
            let travel = w + 40.0 * s;
            dx = dir * travel * (1.0 - e);
            let fa = self.aspect(&o.lib, from);
            let f = fit_rect(fa, o.screen);
            let f = [f[0] - dir * travel * e, f[1], f[2], f[3]];
            busy |= self.draw_asset(out, &o.lib.assets[from].id, f, intersect(f, screen), 1.0, now);
        }
        let mv = |r: [f32; 4]| [r[0] + dx, r[1], r[2], r[3]];
        busy |= self.draw_asset(out, &asset.id, mv(img), mv(clip), alpha, now);

        // Chrome fades with the open/close progress and after a while without the mouse.
        let progress = match o.phase {
            Phase::Waiting(_) => 0.0,
            Phase::Closing { .. } => backdrop,
            // Chrome trails the photo in, so it doesn't cross the grid's header mid-flight.
            _ => backdrop * backdrop,
        };
        let idle = now.duration_since(o.active_at) > CHROME_IDLE;
        let target = if self.hold_chrome || o.panel || !idle { 1.0 } else { 0.0 };
        o.chrome += (target - o.chrome) * (1.0 - (-dt * 9.0).exp());
        if (target - o.chrome).abs() < 0.01 {
            o.chrome = target;
        } else {
            busy = true;
        }
        // Wake up in time to fade the chrome out.
        busy |= target > 0.0 && !self.hold_chrome && !o.panel;
        o.hits.clear();
        let ca = o.chrome * progress;
        if ca > 0.003 {
            let details = self.slots.get(&asset.id).and_then(|s| s.info.as_ref());
            self.draw_chrome(o, details, ca, s, out);
            if o.panel {
                self.draw_panel(o, details, ca, s, r, out);
            }
        }
        if asset.video && progress > 0.0 {
            let c = (w / 2.0 + dx, h / 2.0);
            let rad = 38.0 * s;
            let rect = [c.0 - rad, c.1 - rad, rad * 2.0, rad * 2.0];
            out.quad(Quad::solid(rect, rgb(0x000000, 0.45 * progress), rad));
            out.quad(Quad::solid(rect, rgb(0xffffff, 0.12 * progress), rad));
            out.text(
                Text::new("\u{f04b}", c.0 + 3.0 * s, c.1 - 19.0 * s, 30.0 * s)
                    .icon()
                    .align(Align::Center)
                    .color(fade_color(theme::TEXT, progress)),
            );
            if let Some(d) = self.slots.get(&asset.id).and_then(|s| s.info.as_ref()?.duration.as_ref()) {
                let label = format_duration(d);
                out.text(
                    Text::new(label, c.0, c.1 + rad + 10.0 * s, 13.0 * s)
                        .weight(600)
                        .align(Align::Center)
                        .color(fade_color(theme::TEXT, progress)),
                );
            }
            o.hits.push((Hit::Play, rect));
        }
        busy
    }

    fn draw_chrome(&self, o: &mut Open, details: Option<&Details>, ca: f32, s: f32, out: &mut Layer) {
        let w = o.screen.0;
        // A soft top scrim, stacked from translucent strips.
        let (bands, height) = (14, 110.0 * s);
        for b in 0..bands {
            let t = b as f32 / bands as f32;
            let y = t * height;
            out.quad(Quad::solid([0.0, y, w, height / bands as f32 + 0.5], rgb(0x000000, 0.6 * (1.0 - t).powi(2) * ca), 0.0));
        }

        let button = |out: &mut Layer, x: f32, icon: &str, on: bool| {
            let d = 34.0 * s;
            let rect = [x, 16.0 * s, d, d];
            let bg = if on { rgb(theme::ACCENT, 0.9 * ca) } else { rgb(0xffffff, 0.14 * ca) };
            out.quad(Quad::solid(rect, bg, d / 2.0));
            out.text(
                Text::new(icon, x + d / 2.0, rect[1] + d / 2.0 - 10.0 * s, 16.0 * s)
                    .icon()
                    .align(Align::Center)
                    .color(fade_color(theme::TEXT, ca)),
            );
            rect
        };
        let close = button(out, 16.0 * s, "\u{f053}", false);
        let info = button(out, w - 50.0 * s, "\u{f129}", o.panel);
        o.hits.push((Hit::Close, close));
        o.hits.push((Hit::Info, info));

        let Some(d) = details else { return };
        let when = d.local_date_time.as_deref().and_then(DateTime::parse);
        let place = d.exif_info.as_ref().and_then(place);
        let (title, subtitle) = match (place, when) {
            (Some(p), Some(t)) => (p, format!("{} · {}", t.date(), t.time())),
            (None, Some(t)) => (t.date(), t.time()),
            (Some(p), None) => (p, String::new()),
            (None, None) => (d.original_file_name.clone().unwrap_or_default(), String::new()),
        };
        let x = 64.0 * s;
        let max = (w - x - 80.0 * s).max(50.0);
        out.text(Text::new(title, x, 15.0 * s, 16.0 * s).weight(600).max_width(max).color(fade_color(theme::TEXT, ca)));
        out.text(Text::new(subtitle, x, 36.0 * s, 13.0 * s).weight(500).max_width(max).color(fade_color(theme::TEXT_DIM, ca)));
    }

    fn draw_panel(&self, o: &mut Open, details: Option<&Details>, ca: f32, s: f32, r: &mut Renderer, out: &mut Layer) {
        let (pw, pad) = (320.0 * s, 18.0 * s);
        let x = o.screen.0 - pw - 16.0 * s;
        let mut y = 66.0 * s;
        let top = y;
        let inner = pw - pad * 2.0;
        let mut texts: Vec<Text> = Vec::new();
        let mut rules: Vec<f32> = Vec::new();
        let line = |texts: &mut Vec<Text>, y: &mut f32, text: String, size: f32, weight: u16, color: [u8; 4]| {
            texts.push(Text::new(text, x + pad, *y, size * s).weight(weight).max_width(inner).color(fade_color(color, ca)));
            *y += size * s * 1.45;
        };
        y += pad;
        let Some(d) = details else {
            let failed = self.slots.get(&o.lib.assets[o.index].id).is_some_and(|s| s.state[INFO] == Load::Failed);
            let text = if failed { "No details available" } else { "Loading…" };
            line(&mut texts, &mut y, text.into(), 14.0, 500, theme::TEXT_DIM);
            return finish_panel(out, x, top, pw, y + pad, ca, s, texts, rules, &mut o.hits);
        };
        let exif = d.exif_info.clone().unwrap_or_default();
        if let Some(t) = d.local_date_time.as_deref().and_then(DateTime::parse) {
            line(&mut texts, &mut y, t.date(), 15.0, 600, theme::TEXT);
            line(&mut texts, &mut y, t.time(), 13.0, 500, theme::TEXT_DIM);
        }
        if let Some(name) = &d.original_file_name {
            line(&mut texts, &mut y, name.clone(), 13.0, 500, theme::TEXT_DIM);
        }

        let camera = match (exif.make.as_deref(), exif.model.as_deref()) {
            (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => Some(model.to_string()),
            (Some(make), Some(model)) => Some(format!("{make} {model}")),
            (make, model) => make.or(model).map(String::from),
        };
        let settings: Vec<String> = [
            exif.iso.map(|v| format!("ISO {v:.0}")),
            exif.focal_length.map(|v| format!("{} mm", trim_num(v))),
            exif.f_number.map(|v| format!("ƒ/{}", trim_num(v))),
            exif.exposure_time.as_deref().map(|v| format!("{v} s")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if camera.is_some() || !settings.is_empty() || exif.exif_image_width.is_some() {
            y += 8.0 * s;
            rules.push(y);
            y += 14.0 * s;
            if let Some(c) = camera {
                line(&mut texts, &mut y, c, 15.0, 600, theme::TEXT);
            }
            if let Some(lens) = exif.lens_model.clone() {
                line(&mut texts, &mut y, lens, 13.0, 500, theme::TEXT_DIM);
            }
            if !settings.is_empty() {
                // Evenly spaced exposure "chips", like Photos' camera row.
                y += 4.0 * s;
                let gap = 6.0 * s;
                let cw = (inner - gap * (settings.len() as f32 - 1.0)) / settings.len() as f32;
                for (k, v) in settings.into_iter().enumerate() {
                    let cx = x + pad + k as f32 * (cw + gap);
                    out.quad(Quad::solid([cx, y, cw, 26.0 * s], rgb(0xffffff, 0.08 * ca), 7.0 * s));
                    let size = if r.ui.measure(&v, 12.0 * s, 600, crate::ui::Font::Sans) > cw - 6.0 * s { 10.5 } else { 12.0 };
                    texts.push(
                        Text::new(v, cx + cw / 2.0, y + (26.0 - size * 1.25) / 2.0 * s, size * s)
                            .weight(600)
                            .align(Align::Center)
                            .color(fade_color(theme::TEXT, ca)),
                    );
                }
                y += 26.0 * s + 10.0 * s;
            }
            let dims = match (exif.exif_image_width, exif.exif_image_height) {
                (Some(w), Some(h)) => Some(format!("{w:.0} × {h:.0} · {:.1} MP", w * h / 1e6)),
                _ => None,
            };
            let size = exif.file_size_in_byte.map(format_bytes);
            let row: Vec<String> = [dims, size].into_iter().flatten().collect();
            if !row.is_empty() {
                line(&mut texts, &mut y, row.join(" · "), 13.0, 500, theme::TEXT_DIM);
            }
        }

        let region: Vec<&str> = [exif.city.as_deref(), exif.state.as_deref(), exif.country.as_deref()]
            .into_iter()
            .flatten()
            .filter(|p| !p.is_empty())
            .collect();
        if !region.is_empty() || exif.latitude.is_some() {
            y += 8.0 * s;
            rules.push(y);
            y += 14.0 * s;
            if let Some((first, rest)) = region.split_first() {
                line(&mut texts, &mut y, first.to_string(), 15.0, 600, theme::TEXT);
                if !rest.is_empty() {
                    line(&mut texts, &mut y, rest.join(", "), 13.0, 500, theme::TEXT_DIM);
                }
            }
            if let (Some(lat), Some(lon)) = (exif.latitude, exif.longitude) {
                let ns = if lat >= 0.0 { "N" } else { "S" };
                let ew = if lon >= 0.0 { "E" } else { "W" };
                let text = format!("{:.4}° {ns}, {:.4}° {ew}", lat.abs(), lon.abs());
                line(&mut texts, &mut y, text, 12.0, 500, theme::TEXT_FAINT);
            }
        }
        finish_panel(out, x, top, pw, y + pad - 4.0 * s, ca, s, texts, rules, &mut o.hits)
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_panel(
    out: &mut Layer,
    x: f32,
    top: f32,
    w: f32,
    bottom: f32,
    ca: f32,
    s: f32,
    texts: Vec<Text>,
    rules: Vec<f32>,
    hits: &mut Vec<(Hit, [f32; 4])>,
) {
    let rect = [x, top, w, bottom - top];
    out.quad(Quad::solid(rect, rgb(0x1c1c1e, 0.94 * ca), 14.0 * s));
    for y in rules {
        out.quad(Quad::solid([x + 18.0 * s, y, w - 36.0 * s, 1.0], rgb(theme::SEPARATOR, ca), 0.0));
    }
    for t in texts {
        out.text(t);
    }
    hits.push((Hit::Panel, rect));
}

impl Open {
    fn fit(&self) -> [f32; 4] {
        // Aspect from the last drawn frame keeps this in sync with what's on screen.
        let a = if self.img[3] > 0.0 { self.img[2] / self.img[3] } else { 1.0 };
        fit_rect(a, self.screen)
    }

    fn clamp_pan(&mut self) {
        let f = self.fit();
        for (k, (size, room)) in [(f[2], self.screen.0), (f[3], self.screen.1)].into_iter().enumerate() {
            let max = ((size * self.zoom - room) / 2.0).max(0.0);
            self.pan[k] = self.pan[k].clamp(-max, max);
        }
    }

    /// Zoom (relative to fit) at which one image pixel covers one screen pixel.
    fn one_to_one(&self) -> f32 {
        let f = self.fit();
        let native = self.native_width.max(1.0);
        native / f[2].max(1.0)
    }

    /// Multiplies the zoom, keeping the point under `at` fixed; 0 resets to fit.
    fn zoom_by(&mut self, factor: f32, at: (f32, f32)) {
        if !matches!(self.phase, Phase::Shown | Phase::Opening(_)) {
            return;
        }
        let max = (self.one_to_one() * 3.0).max(4.0);
        let z = if factor <= 0.0 { 1.0 } else { (self.zoom * factor).clamp(1.0, max) };
        let c = (at.0 - self.screen.0 / 2.0, at.1 - self.screen.1 / 2.0);
        let q = ((c.0 - self.pan[0]) / self.zoom, (c.1 - self.pan[1]) / self.zoom);
        self.pan = [c.0 - q.0 * z, c.1 - q.1 * z];
        self.zoom = z;
        self.clamp_pan();
    }
}

struct FullHint {
    mime: String,
    aspect: f32,
    preview_w: u32,
}

async fn fetch(source: &Source, id: &str, index: usize, kind: usize, hint: Option<FullHint>) -> Result<Loaded> {
    let image = |w, h, rgba| Loaded::Image { id: id.to_string(), kind, w, h, rgba };
    let immich = match source {
        Source::Immich(immich) => immich,
        Source::Demo(_) => {
            return match kind {
                THUMB | PREVIEW => {
                    let img = library::demo_image(index as u64, if kind == THUMB { 128 } else { 1024 });
                    Ok(image(img.width(), img.height(), img.into_raw()))
                }
                _ => bail!("not available in the demo"),
            };
        }
    };
    let decoded = |bytes: Vec<u8>, max: u32| tokio::task::spawn_blocking(move || decode(&bytes, max));
    match kind {
        THUMB => {
            let (w, h, rgba) = decoded(thumbnail_bytes(immich, id).await?, 512).await??;
            Ok(image(w, h, rgba))
        }
        PREVIEW => {
            let bytes = immich.get_bytes(&format!("/assets/{id}/thumbnail?size=preview")).await?;
            let (w, h, rgba) = decoded(bytes, 4096).await??;
            Ok(image(w, h, rgba))
        }
        FULL => {
            let hint = hint.unwrap_or(FullHint { mime: String::new(), aspect: 1.0, preview_w: 0 });
            let usable = |(w, h, _): &(u32, u32, Vec<u8>)| {
                let a = *w as f32 / *h as f32;
                (a / hint.aspect - 1.0).abs() < 0.03 && *w > hint.preview_w + hint.preview_w / 10
            };
            // The image crate can't read HEIC/RAW; the server's full-size JPEG covers those.
            if matches!(hint.mime.as_str(), "image/jpeg" | "image/png" | "image/webp") {
                match immich.get_bytes(&format!("/assets/{id}/original")).await {
                    Ok(bytes) => match decoded(bytes, FULL_MAX).await? {
                        Ok(d) if usable(&d) => return Ok(image(d.0, d.1, d.2)),
                        Ok(_) => log::info!("original of {id} doesn't match its preview; trying fullsize"),
                        Err(e) => log::info!("original of {id}: {e:#}; trying fullsize"),
                    },
                    Err(e) => log::info!("original of {id}: {e:#}; trying fullsize"),
                }
            }
            let bytes = immich.get_bytes(&format!("/assets/{id}/thumbnail?size=fullsize")).await?;
            let d = decoded(bytes, FULL_MAX).await??;
            if !usable(&d) {
                bail!("fullsize of {id} is no sharper than the preview");
            }
            Ok(image(d.0, d.1, d.2))
        }
        _ => {
            let details: Details = immich.get_json(&format!("/assets/{id}")).await?;
            Ok(Loaded::Info { id: id.to_string(), details: Box::new(details) })
        }
    }
}

/// Decodes to RGBA8, applying EXIF orientation and capping the longest side at `max`.
fn decode(bytes: &[u8], max: u32) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    if img.width().max(img.height()) > max {
        img = img.resize(max, max, FilterType::CatmullRom);
    }
    let rgba = img.to_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
}

fn decode_thumbhash(hash: Option<&str>) -> Option<(u32, u32, Vec<u8>)> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(hash?).ok()?;
    let (w, h, rgba) = thumbhash::thumb_hash_to_rgba(&bytes).ok()?;
    Some((w as u32, h as u32, rgba))
}

/// Scrolls the grid so tile `i` is comfortably on screen.
pub fn reveal(layout: &mut Layout, i: usize) {
    let vp = layout.vp;
    let t = Spec::Grid(layout.target).rect(i, &vp);
    if t[1] < vp.top || t[1] + t[3] > vp.height {
        layout.scroll_by(t[1] + t[3] / 2.0 - (vp.top + vp.height) / 2.0);
    }
}

fn fit_rect(a: f32, (w, h): (f32, f32)) -> [f32; 4] {
    let (fw, fh) = if w / h > a { (h * a, h) } else { (w, w / a) };
    [(w - fw) / 2.0, (h - fh) / 2.0, fw, fh]
}

fn intersect(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let (x0, y0) = (a[0].max(b[0]), a[1].max(b[1]));
    let (x1, y1) = ((a[0] + a[2]).min(b[0] + b[2]), (a[1] + a[3]).min(b[1] + b[3]));
    [x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)]
}

fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|k| a[k] + (b[k] - a[k]) * t)
}

fn scale_about(r: [f32; 4], k: f32) -> [f32; 4] {
    let (cx, cy) = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
    [cx - r[2] * k / 2.0, cy - r[3] * k / 2.0, r[2] * k, r[3] * k]
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn fade_color(c: [u8; 4], a: f32) -> [u8; 4] {
    [c[0], c[1], c[2], (c[3] as f32 * a.clamp(0.0, 1.0)) as u8]
}

fn place(e: &Exif) -> Option<String> {
    let parts: Vec<&str> =
        [e.city.as_deref(), e.country.as_deref()].into_iter().flatten().filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn trim_num(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn format_bytes(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{} KB", b.div_ceil(1024)),
    }
}

/// "0:01:05.123000" or 65123 (ms) → "1:05".
fn format_duration(d: &serde_json::Value) -> String {
    let secs = match d {
        serde_json::Value::Number(ms) => ms.as_f64().unwrap_or(0.0) as u64 / 1000,
        v => v.as_str().unwrap_or("").split(':').fold(0, |acc, p| {
            acc * 60 + p.split('.').next().unwrap_or("0").parse::<u64>().unwrap_or(0)
        }),
    };
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Wall-clock capture time; Immich's `localDateTime` is local time written with a "Z".
struct DateTime {
    y: i64,
    m: u32,
    d: u32,
    hh: u32,
    mm: u32,
}

impl DateTime {
    fn parse(s: &str) -> Option<Self> {
        let n = |r: std::ops::Range<usize>| s.get(r)?.parse().ok();
        Some(Self { y: n(0..4)?, m: n(5..7)? as u32, d: n(8..10)? as u32, hh: n(11..13)? as u32, mm: n(14..16)? as u32 })
    }

    fn date(&self) -> String {
        // Days since 1970-01-01 (a Thursday), Howard Hinnant's days-from-civil.
        let (y, m) = if self.m <= 2 { (self.y - 1, self.m + 9) } else { (self.y, self.m - 3) };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * m as i64 + 2) / 5 + self.d as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
        let month = MONTHS[(self.m as usize).clamp(1, 12) - 1];
        format!("{weekday}, {} {month} {}", self.d, self.y)
    }

    fn time(&self) -> String {
        format!("{:02}:{:02}", self.hh, self.mm)
    }
}
