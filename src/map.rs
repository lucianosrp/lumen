//! Places: a world map of the library's geotagged photos, drawn as Apple-style photo pins
//! over Esri's Dark Gray Canvas raster basemap (CARTO's dark tiles now need an API key).
//!
//! Positions are Web-Mercator "world" coordinates in 0..1 (x east, y south); the camera is a
//! world-space centre plus a fractional zoom, where the world is `256 · 2^zoom` logical px wide.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, bail};
use image::imageops::FilterType;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::immich::Immich;
use crate::library::Library;
use crate::loader::{self, Loader, Source, group};
use crate::render::Renderer;
use crate::ui::{self, Font, ImageId, Quad, Text, theme};

/// Base layer (JPEG) and labels (transparent PNG), composited per tile.
const TILE_LAYERS: [(&str, &str); 2] = [
    ("https://server.arcgisonline.com/ArcGIS/rest/services/Canvas/World_Dark_Gray_Base/MapServer/tile", "base"),
    ("https://server.arcgisonline.com/ArcGIS/rest/services/Canvas/World_Dark_Gray_Reference/MapServer/tile", "ref"),
];
const USER_AGENT: &str = "Lumen/0.1 (personal photo viewer)";
const ATTRIBUTION: &str = "Esri, HERE, Garmin, © OpenStreetMap contributors, and the GIS user community";
const MAX_ZOOM: f64 = 18.0;
/// Deepest zoom the tile service has real imagery for; beyond it tiles are upscaled.
const TILE_MAX: u8 = 16;
const TILE_WORKERS: usize = 6;
const THUMB_WORKERS: usize = 6;
/// Tile disk cache budget; trimmed back to 80% of this, oldest-used first.
const DISK_CAP: u64 = 150 << 20;
/// GPU tile textures kept (256px each, ~350 KB with mips: ~70 MB).
const GPU_TILES: usize = 200;
const GPU_THUMBS: usize = 400;
/// Pin edge and clustering cell, in logical px.
const PIN: f32 = 56.0;
const CELL: f64 = 70.0;
const FADE: f32 = 0.2;

/// What the app should do in response to map input.
pub enum Action {
    ShowAssets { title: String, ids: Vec<String> },
}

impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::ShowAssets { title, ids } => {
                write!(f, "ShowAssets {{ title: {title:?}, {} ids: {:?}", ids.len(), &ids[..ids.len().min(3)])?;
                if ids.len() > 3 { write!(f, " … }}") } else { write!(f, " }}") }
            }
        }
    }
}

pub enum Response {
    /// Not for the map; let the app handle it.
    Ignored,
    Handled,
    Action(Action),
}

/// Headless screenshot options: `--map`, `--map-at lat,lon,zoom`, `--map-hover x,y`, `--map-click x,y`.
pub struct Shot {
    pub at: Option<(f64, f64, f64)>,
    pub hover: Option<[f32; 2]>,
    pub click: Option<[f32; 2]>,
}

impl Shot {
    pub fn parse(args: &[String]) -> Option<Self> {
        let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
        let nums = |name: &str| -> Vec<f64> {
            flag(name).map_or(Vec::new(), |s| s.split(',').filter_map(|v| v.trim().parse().ok()).collect())
        };
        let point = |name: &str| match nums(name)[..] {
            [x, y] => Some([x as f32, y as f32]),
            _ => None,
        };
        let at = match nums("--map-at")[..] {
            [lat, lon, z] => Some((lat, lon, z)),
            [lat, lon] => Some((lat, lon, 12.0)),
            _ => None,
        };
        let shot = Shot { at, hover: point("--map-hover"), click: point("--map-click") };
        let any = args.iter().any(|a| a == "--map") || shot.at.is_some() || shot.hover.is_some() || shot.click.is_some();
        any.then_some(shot)
    }
}

// ---------------------------------------------------------------------------------------------
// Background loading

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct RawMarker {
    id: String,
    lat: f64,
    lon: f64,
    city: Option<String>,
    state: Option<String>,
    country: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct TileKey {
    z: u8,
    x: u32,
    y: u32,
}

enum Msg {
    Markers(Vec<RawMarker>),
    Error(String),
    Tile(TileKey, image::RgbaImage),
    Thumb(String, image::RgbaImage),
}

struct Queue<J> {
    want: VecDeque<J>,
    inflight: HashSet<J>,
    failed: HashSet<J>,
}

impl<J: Clone + Eq + Hash> Queue<J> {
    fn new() -> Self {
        Self { want: VecDeque::new(), inflight: HashSet::new(), failed: HashSet::new() }
    }
    fn set(&mut self, wants: impl IntoIterator<Item = J>) {
        let Queue { want, inflight, failed } = self;
        want.clear();
        want.extend(wants.into_iter().filter(|j| !inflight.contains(j) && !failed.contains(j)));
    }
    fn idle(&self) -> bool {
        self.want.is_empty() && self.inflight.is_empty()
    }
}

struct Shared {
    tiles: Mutex<Queue<TileKey>>,
    thumbs: Mutex<Queue<String>>,
    notify: Notify,
    closed: AtomicBool,
    disk: AtomicU64,
    trimming: AtomicBool,
    tx: Mutex<Sender<Msg>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn send(&self, msg: Msg) {
        let _ = self.tx.lock().unwrap().send(msg);
        (self.wake)();
    }
}

/// Pops jobs off one queue, nearest-first as the UI ordered them, until the map closes.
async fn worker<J, F, Fut>(shared: Arc<Shared>, queue: fn(&Shared) -> &Mutex<Queue<J>>, load: F)
where
    J: Clone + Eq + Hash + fmt::Debug,
    F: Fn(J) -> Fut,
    Fut: Future<Output = Result<Msg>>,
{
    while !shared.closed.load(Ordering::Relaxed) {
        let notified = shared.notify.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        let job = {
            let mut q = queue(&shared).lock().unwrap();
            let job = q.want.pop_front();
            if let Some(j) = &job {
                q.inflight.insert(j.clone());
            }
            job
        };
        let Some(job) = job else {
            notified.await;
            continue;
        };
        let result = load(job.clone()).await;
        // Send before leaving `inflight`, so the queue never looks idle with a result in transit.
        let failed = match result {
            Ok(msg) => {
                shared.send(msg);
                false
            }
            Err(e) => {
                log::warn!("map {job:?}: {e:#}");
                true
            }
        };
        let mut q = queue(&shared).lock().unwrap();
        q.inflight.remove(&job);
        if failed {
            q.failed.insert(job);
        }
    }
}

fn tile_dir() -> PathBuf {
    loader::cache_dir().join("tiles")
}

async fn load_tile(shared: &Shared, http: &reqwest::Client, k: TileKey) -> Result<Msg> {
    let mut layers = Vec::new();
    for (i, (url, _)) in TILE_LAYERS.into_iter().enumerate() {
        let path = layers_path(k, i);
        layers.push(cached_get(shared, http, &format!("{url}/{}/{}/{}", k.z, k.y, k.x), &path).await?);
    }
    let decoded = tokio::task::spawn_blocking(move || {
        let mut base = image::load_from_memory(&layers[0]).map_err(|e| (e, 0))?.to_rgba8();
        let labels = image::load_from_memory(&layers[1]).map_err(|e| (e, 1))?;
        image::imageops::overlay(&mut base, &labels, 0, 0);
        Ok::<_, (image::ImageError, usize)>(base)
    })
    .await?;
    decoded.map(|img| Msg::Tile(k, img)).map_err(|(e, i)| {
        let _ = std::fs::remove_file(layers_path(k, i));
        e.into()
    })
}

fn layers_path(k: TileKey, layer: usize) -> PathBuf {
    tile_dir().join(format!("{}/{}/{}/{}", TILE_LAYERS[layer].1, k.z, k.x, k.y))
}

/// GET through the size-capped disk cache.
async fn cached_get(shared: &Shared, http: &reqwest::Client, url: &str, path: &Path) -> Result<Vec<u8>> {
    if let Ok(b) = tokio::fs::read(path).await {
        // mtime doubles as last-used time for eviction.
        let _ = std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(SystemTime::now()));
        return Ok(b);
    }
    let res = http.get(url).send().await?;
    if !res.status().is_success() {
        bail!("GET {url}: {}", res.status());
    }
    let b = res.bytes().await?.to_vec();
    let tmp = path.with_extension("tmp");
    let _ = tokio::fs::create_dir_all(path.parent().unwrap()).await;
    if tokio::fs::write(&tmp, &b).await.is_ok() && tokio::fs::rename(&tmp, path).await.is_ok() {
        let len = on_disk(b.len() as u64);
        let used = shared.disk.fetch_add(len, Ordering::Relaxed) + len;
        if used > DISK_CAP {
            trim_disk(shared);
        }
    }
    Ok(b)
}

/// Deletes least-recently-used tiles until the cache is back under 80% of its cap.
fn trim_disk(shared: &Shared) {
    if shared.trimming.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut files = Vec::new();
    walk(&tile_dir(), &mut files);
    files.sort_by_key(|f| f.1);
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    let before = total;
    for (path, _, len) in &files {
        if total <= DISK_CAP * 4 / 5 {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total -= len;
        }
    }
    if total != before {
        log::info!("map tile cache trimmed {} → {} MB", before >> 20, total >> 20);
    }
    shared.disk.store(total, Ordering::Relaxed);
    shared.trimming.store(false, Ordering::Release);
}

/// Small tiles cost whole filesystem blocks, so count those against the cap.
fn on_disk(len: u64) -> u64 {
    len.next_multiple_of(4096)
}

fn walk(dir: &Path, out: &mut Vec<(PathBuf, SystemTime, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            walk(&e.path(), out);
        } else {
            out.push((e.path(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), on_disk(meta.len())));
        }
    }
}

async fn load_thumb(immich: &Immich, id: String, px: u32) -> Result<Msg> {
    let bytes = loader::thumbnail_bytes(immich, &id).await?;
    let img = tokio::task::spawn_blocking(move || {
        image::load_from_memory(&bytes).map(|i| i.resize_to_fill(px, px, FilterType::CatmullRom).to_rgba8())
    })
    .await??;
    Ok(Msg::Thumb(id, img))
}

/// Shows the cached marker list instantly, then refreshes it from the server.
async fn load_markers(immich: Immich, shared: Arc<Shared>) {
    let key = immich.identity().bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    let path = loader::cache_dir().join(format!("map-markers-{key:016x}.json"));
    let cached: Option<Vec<RawMarker>> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    if let Some(m) = &cached {
        shared.send(Msg::Markers(m.clone()));
    }
    match immich.get_json::<Vec<RawMarker>>("/map/markers").await {
        Ok(markers) if cached.as_ref() != Some(&markers) => {
            let _ = std::fs::create_dir_all(loader::cache_dir());
            let _ = std::fs::write(&path, serde_json::to_vec(&markers).unwrap());
            shared.send(Msg::Markers(markers));
        }
        Ok(_) => {}
        Err(e) => {
            log::error!("map markers: {e:#}");
            if cached.is_none() {
                shared.send(Msg::Error(format!("Couldn't load places: {e:#}")));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The view

struct Marker {
    id: String,
    pos: [f64; 2],
    city: u32,
    country: u32,
    /// Position in the date-sorted library; higher is newer.
    rank: i64,
}

const NONE: u32 = u32::MAX;

struct Pin {
    /// Marker whose photo is shown: the cluster's most recent.
    rep: u32,
    members: Vec<u32>,
    title: String,
    pos: [f64; 2],
    target: [f64; 2],
    alpha: f32,
    dying: bool,
    hover: f32,
}

struct Tex {
    image: ImageId,
    used: u64,
    born: Instant,
}

struct Press {
    at: [f32; 2],
    last: [f32; 2],
    last_t: Instant,
    moved: bool,
}

pub struct MapView {
    shared: Arc<Shared>,
    rx: Receiver<Msg>,
    scale: f32,
    area: [f32; 4],
    header: f32,

    center: [f64; 2],
    zoom: f64,
    target_zoom: f64,
    anchor: [f32; 2],
    velocity: [f32; 2],
    pending_view: Option<(f64, f64, f64)>,
    need_fit: bool,

    cursor: Option<[f32; 2]>,
    press: Option<Press>,
    last_click: Option<(Instant, [f32; 2])>,

    markers: Vec<Marker>,
    names: Vec<String>,
    countries: usize,
    loaded: bool,
    ranked: usize,
    level: f64,
    pins: Vec<Pin>,
    /// Marker index → index of its live pin.
    owner: Vec<u32>,
    hovered: Option<usize>,

    tiles: HashMap<TileKey, Tex>,
    thumbs: HashMap<String, Tex>,
    uploads: VecDeque<Msg>,
    frame: u64,
    last_tick: Instant,
    busy: bool,
    status: String,
    layer: ui::Layer,
    top: ui::Layer,
}

fn mercator(lat: f64, lon: f64) -> [f64; 2] {
    let lat = lat.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    [(lon + 180.0) / 360.0, (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0]
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

impl MapView {
    pub fn new(loader: &Loader, scale: f32) -> Self {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            tiles: Mutex::new(Queue::new()),
            thumbs: Mutex::new(Queue::new()),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
            disk: AtomicU64::new(0),
            trimming: AtomicBool::new(false),
            tx: Mutex::new(tx),
            wake: loader.waker(),
        });
        let rt = loader.handle();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .build()
            .expect("http client");
        for _ in 0..TILE_WORKERS {
            let (s, http) = (shared.clone(), http.clone());
            rt.spawn(worker(shared.clone(), |s| &s.tiles, move |k| {
                let (s, http) = (s.clone(), http.clone());
                async move { load_tile(&s, &http, k).await }
            }));
        }
        let s = shared.clone();
        rt.spawn_blocking(move || {
            let mut files = Vec::new();
            walk(&tile_dir(), &mut files);
            let used = s.disk.fetch_add(files.iter().map(|f| f.2).sum(), Ordering::Relaxed);
            if used > DISK_CAP {
                trim_disk(&s);
            }
        });
        let (loaded, status) = match loader.source() {
            Source::Immich(immich) => {
                rt.spawn(load_markers(immich.clone(), shared.clone()));
                let px = (PIN * 1.15 * scale).ceil() as u32;
                for _ in 0..THUMB_WORKERS {
                    let immich = immich.clone();
                    rt.spawn(worker(shared.clone(), |s| &s.thumbs, move |id| {
                        let immich = immich.clone();
                        async move { load_thumb(&immich, id, px).await }
                    }));
                }
                (false, String::new())
            }
            Source::Demo(_) => (true, "Places needs an Immich server".to_string()),
        };
        Self {
            shared,
            rx,
            scale,
            area: [0.0; 4],
            header: 0.0,
            center: [0.5, 0.5],
            zoom: 2.0,
            target_zoom: 2.0,
            anchor: [0.0; 2],
            velocity: [0.0; 2],
            pending_view: None,
            need_fit: true,
            cursor: None,
            press: None,
            last_click: None,
            markers: Vec::new(),
            names: Vec::new(),
            countries: 0,
            loaded,
            ranked: 0,
            level: f64::NAN,
            pins: Vec::new(),
            owner: Vec::new(),
            hovered: None,
            tiles: HashMap::new(),
            thumbs: HashMap::new(),
            uploads: VecDeque::new(),
            frame: 0,
            last_tick: Instant::now(),
            busy: true,
            status,
            layer: ui::Layer::default(),
            top: ui::Layer::default(),
        }
    }

    /// Stops background work and frees GPU textures.
    pub fn close(mut self, renderer: &mut Renderer) {
        for t in self.tiles.drain().map(|t| t.1).chain(self.thumbs.drain().map(|t| t.1)) {
            renderer.ui.free_image(t.image);
        }
    }

    /// Frames a place instead of fitting all markers.
    pub fn set_view(&mut self, lat: f64, lon: f64, zoom: f64) {
        self.pending_view = Some((lat, lon, zoom));
        self.need_fit = false;
    }

    /// True once markers, visible tiles and pin photos have all arrived and nothing moves.
    pub fn is_idle(&self) -> bool {
        self.loaded
            && !self.busy
            && self.shared.tiles.lock().unwrap().idle()
            && self.shared.thumbs.lock().unwrap().idle()
    }

    /// Title and subtitle for the app header.
    pub fn header(&self) -> (String, String) {
        let sub = if !self.status.is_empty() {
            self.status.clone()
        } else if !self.loaded {
            "Loading places…".to_string()
        } else {
            let c = if self.countries == 1 { "Country" } else { "Countries" };
            format!("{} Photos · {} {c}", group(self.markers.len()), self.countries)
        };
        ("Places".to_string(), sub)
    }

    // --- camera ---

    fn world_px(&self) -> f64 {
        256.0 * self.scale as f64 * self.zoom.exp2()
    }

    fn mid(&self) -> [f64; 2] {
        let a = self.area;
        [(a[0] + a[2] / 2.0) as f64, (a[1] + a[3] / 2.0) as f64]
    }

    fn to_screen(&self, w: [f64; 2]) -> [f64; 2] {
        let (m, s) = (self.mid(), self.world_px());
        [m[0] + (w[0] - self.center[0]) * s, m[1] + (w[1] - self.center[1]) * s]
    }

    fn to_world(&self, p: [f32; 2]) -> [f64; 2] {
        let (m, s) = (self.mid(), self.world_px());
        [self.center[0] + (p[0] as f64 - m[0]) / s, self.center[1] + (p[1] as f64 - m[1]) / s]
    }

    /// The world never gets smaller than the view, so there's nothing to wrap or show past its edges.
    fn min_zoom(&self) -> f64 {
        (self.area[2].max(self.area[3]).max(1.0) as f64 / (256.0 * self.scale as f64)).log2()
    }

    fn clamp(&mut self) {
        let lo = self.min_zoom();
        self.zoom = self.zoom.clamp(lo, MAX_ZOOM);
        self.target_zoom = self.target_zoom.clamp(lo, MAX_ZOOM);
        let s = self.world_px();
        for (i, half) in [self.area[2] as f64 / 2.0 / s, self.area[3] as f64 / 2.0 / s].into_iter().enumerate() {
            self.center[i] = if half >= 0.5 { 0.5 } else { self.center[i].clamp(half, 1.0 - half) };
        }
    }

    fn pan(&mut self, d: [f32; 2]) {
        let s = self.world_px();
        self.center[0] -= d[0] as f64 / s;
        self.center[1] -= d[1] as f64 / s;
        self.clamp();
    }

    fn zoom_by(&mut self, delta: f64, at: [f32; 2]) {
        self.target_zoom = (self.target_zoom + delta).clamp(self.min_zoom(), MAX_ZOOM);
        self.anchor = at;
    }

    /// Keeps world point `w` under screen point `at`.
    fn pin_point(&mut self, w: [f64; 2], at: [f32; 2]) {
        let (m, s) = (self.mid(), self.world_px());
        self.center = [w[0] - (at[0] as f64 - m[0]) / s, w[1] - (at[1] as f64 - m[1]) / s];
    }

    /// Centre of the part of the map not under the header.
    fn focus(&self) -> [f32; 2] {
        let a = self.area;
        [a[0] + a[2] / 2.0, (a[1] + self.header + a[1] + a[3]) / 2.0]
    }

    fn look_at(&mut self, w: [f64; 2], zoom: f64) {
        self.zoom = zoom.clamp(self.min_zoom(), MAX_ZOOM);
        self.target_zoom = self.zoom;
        self.pin_point(w, self.focus());
        self.clamp();
    }

    fn fit(&mut self) {
        if self.markers.is_empty() {
            return self.look_at([0.5, 0.42], 0.0);
        }
        let (mut lo, mut hi) = ([1.0f64; 2], [0.0f64; 2]);
        for m in &self.markers {
            for i in 0..2 {
                lo[i] = lo[i].min(m.pos[i]);
                hi[i] = hi[i].max(m.pos[i]);
            }
        }
        let pad = 1.5 * (PIN * self.scale) as f64;
        let (w, h) = (self.area[2] as f64 - 2.0 * pad, (self.area[3] - self.header) as f64 - 2.0 * pad);
        let unit = 256.0 * self.scale as f64;
        let zx = (w.max(1.0) / ((hi[0] - lo[0]).max(1e-9) * unit)).log2();
        let zy = (h.max(1.0) / ((hi[1] - lo[1]).max(1e-9) * unit)).log2();
        self.look_at([(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0], zx.min(zy).min(15.0));
    }

    // --- input ---

    pub fn event(&mut self, event: &WindowEvent, mods: ModifiersState, now: Instant) -> Response {
        let inside = |p: [f32; 2], a: [f32; 4]| ui::contains(a, p[0], p[1]);
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let p = [position.x as f32, position.y as f32];
                if let Some(press) = &mut self.press {
                    let d = [p[0] - press.last[0], p[1] - press.last[1]];
                    if !press.moved && (p[0] - press.at[0]).hypot(p[1] - press.at[1]) > 4.0 * self.scale {
                        press.moved = true;
                    }
                    if press.moved {
                        let dt = now.duration_since(press.last_t).as_secs_f32().max(0.004);
                        let v = [d[0] / dt, d[1] / dt];
                        self.velocity = [self.velocity[0] * 0.6 + v[0] * 0.4, self.velocity[1] * 0.6 + v[1] * 0.4];
                        press.last = p;
                        press.last_t = now;
                        self.pan(d);
                    }
                }
                self.cursor = Some(p);
                // The app still tracks the cursor for itself.
                Response::Ignored
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                Response::Ignored
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                let Some(p) = self.cursor else { return Response::Ignored };
                match state {
                    ElementState::Pressed if inside(p, self.area) => {
                        self.velocity = [0.0; 2];
                        self.target_zoom = self.zoom;
                        self.press = Some(Press { at: p, last: p, last_t: now, moved: false });
                        Response::Handled
                    }
                    ElementState::Released => match self.press.take() {
                        Some(press) if press.moved => {
                            // Coast only if the pointer was still moving when released.
                            if now.duration_since(press.last_t) > Duration::from_millis(60) {
                                self.velocity = [0.0; 2];
                            }
                            Response::Handled
                        }
                        Some(_) => {
                            self.velocity = [0.0; 2];
                            let double = self.last_click.is_some_and(|(t, q)| {
                                now.duration_since(t) < Duration::from_millis(350)
                                    && (p[0] - q[0]).hypot(p[1] - q[1]) < 8.0 * self.scale
                            });
                            self.last_click = (!double).then_some((now, p));
                            match self.click(p[0], p[1]) {
                                Some(a) if !double => Response::Action(a),
                                None if double => {
                                    self.zoom_by(1.0, p);
                                    Response::Handled
                                }
                                _ => Response::Handled,
                            }
                        }
                        None => Response::Ignored,
                    },
                    _ => Response::Ignored,
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(p) = self.cursor.filter(|&p| inside(p, self.area)) else { return Response::Ignored };
                let zooming = mods.control_key() || mods.super_key();
                match *delta {
                    MouseScrollDelta::LineDelta(_, y) => self.zoom_by(y as f64 * 0.5, p),
                    MouseScrollDelta::PixelDelta(d) if zooming => self.zoom_by(d.y / 120.0, p),
                    // Two-finger touchpad scroll pans, like a map should.
                    MouseScrollDelta::PixelDelta(d) => {
                        self.velocity = [0.0; 2];
                        self.pan([d.x as f32 * 1.5, d.y as f32 * 1.5]);
                    }
                }
                Response::Handled
            }
            WindowEvent::PinchGesture { delta, .. } if delta.is_finite() => {
                let p = self.cursor.unwrap_or(self.focus());
                let w = self.to_world(p);
                self.zoom = (self.zoom + (1.0 + delta).max(0.1).log2()).clamp(self.min_zoom(), MAX_ZOOM);
                self.target_zoom = self.zoom;
                self.pin_point(w, p);
                self.clamp();
                Response::Handled
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let step = 160.0 * self.scale;
                match event.logical_key.as_ref() {
                    Key::Character("=" | "+") => self.zoom_by(1.0, self.focus()),
                    Key::Character("-" | "_") => self.zoom_by(-1.0, self.focus()),
                    Key::Character("0") => self.fit(),
                    Key::Named(NamedKey::ArrowLeft) => self.pan([step, 0.0]),
                    Key::Named(NamedKey::ArrowRight) => self.pan([-step, 0.0]),
                    Key::Named(NamedKey::ArrowUp) => self.pan([0.0, step]),
                    Key::Named(NamedKey::ArrowDown) => self.pan([0.0, -step]),
                    _ => return Response::Ignored,
                }
                Response::Handled
            }
            _ => Response::Ignored,
        }
    }

    /// The action for a click at physical pixel (x, y), if it lands on a pin.
    pub fn click(&mut self, x: f32, y: f32) -> Option<Action> {
        let i = self.pin_at([x, y])?;
        let pin = &self.pins[i];
        let mut members = pin.members.clone();
        members.sort_by_key(|&m| std::cmp::Reverse(self.markers[m as usize].rank));
        let ids = members.into_iter().map(|m| self.markers[m as usize].id.clone()).collect();
        Some(Action::ShowAssets { title: pin.title.clone(), ids })
    }

    /// Moves the (virtual) pointer, for headless hover screenshots.
    pub fn hover(&mut self, x: f32, y: f32) {
        self.cursor = Some([x, y]);
    }

    fn pin_rect(&self, pin: &Pin) -> [f32; 4] {
        let p = self.to_screen(pin.pos);
        let grow = 0.7 + 0.3 * ease_out(pin.alpha);
        let size = PIN * self.scale * grow * (1.0 + 0.1 * pin.hover);
        [p[0] as f32 - size / 2.0, p[1] as f32 - size / 2.0, size, size]
    }

    fn pin_at(&self, p: [f32; 2]) -> Option<usize> {
        if !ui::contains(self.area, p[0], p[1]) {
            return None;
        }
        // Topmost first: pins lower on screen draw over higher ones.
        self.pins
            .iter()
            .enumerate()
            .filter(|(_, pin)| !pin.dying && pin.alpha > 0.3 && ui::contains(self.pin_rect(pin), p[0], p[1]))
            .max_by(|a, b| a.1.pos[1].total_cmp(&b.1.pos[1]))
            .map(|(i, _)| i)
    }

    // --- data ---

    fn set_markers(&mut self, raw: Vec<RawMarker>) {
        let mut names: Vec<String> = Vec::new();
        let mut index: HashMap<String, u32> = HashMap::new();
        let mut intern = |s: Option<String>| match s.filter(|s| !s.is_empty()) {
            None => NONE,
            Some(s) => *index.entry(s.clone()).or_insert_with(|| {
                names.push(s);
                names.len() as u32 - 1
            }),
        };
        self.markers = raw
            .into_iter()
            .map(|m| Marker {
                pos: mercator(m.lat, m.lon),
                city: intern(m.city),
                country: intern(m.country),
                id: m.id,
                rank: -1,
            })
            .collect();
        self.names = names;
        self.countries = self.markers.iter().map(|m| m.country).filter(|&c| c != NONE).collect::<HashSet<_>>().len();
        self.loaded = true;
        self.ranked = 0;
        self.level = f64::NAN;
        self.pins.clear();
        self.owner.clear();
    }

    fn rank(&mut self, library: &Library) {
        let order: HashMap<&str, i64> = library.assets.iter().enumerate().map(|(i, a)| (a.id.as_str(), i as i64)).collect();
        for m in &mut self.markers {
            m.rank = order.get(m.id.as_str()).copied().unwrap_or(-1);
        }
        log::debug!("map: {} of {} markers in the library", self.markers.iter().filter(|m| m.rank >= 0).count(), self.markers.len());
        self.level = f64::NAN;
    }

    fn title(&self, members: &[u32]) -> String {
        let top = |field: fn(&Marker) -> u32| {
            let mut counts: HashMap<u32, usize> = HashMap::new();
            for &m in members {
                let v = field(&self.markers[m as usize]);
                if v != NONE {
                    *counts.entry(v).or_default() += 1;
                }
            }
            counts.into_iter().max_by_key(|&(v, n)| (n, std::cmp::Reverse(v)))
        };
        let n = members.len();
        match (top(|m| m.city), top(|m| m.country)) {
            (Some((c, k)), _) if k * 10 >= n * 6 => self.names[c as usize].clone(),
            (_, Some((c, _))) => self.names[c as usize].clone(),
            (Some((c, _)), None) => self.names[c as usize].clone(),
            (None, None) => "Unknown Location".to_string(),
        }
    }

    /// Groups markers on a world-space grid (so panning never reshuffles pins), then merges
    /// neighbours whose pins would overlap. Pins that survive keep their animation state.
    fn recluster(&mut self, level: f64) {
        self.level = level;
        let cell = CELL / (256.0 * level.exp2());
        let key = |p: [f64; 2]| ((p[0] / cell).floor() as i64, (p[1] / cell).floor() as i64);
        let mut cells: HashMap<(i64, i64), usize> = HashMap::new();
        let mut groups: Vec<(Vec<u32>, [f64; 2])> = Vec::new();
        for (i, m) in self.markers.iter().enumerate() {
            let g = *cells.entry(key(m.pos)).or_insert_with(|| {
                groups.push((Vec::new(), [0.0; 2]));
                groups.len() - 1
            });
            groups[g].0.push(i as u32);
            groups[g].1[0] += m.pos[0];
            groups[g].1[1] += m.pos[1];
        }
        let centroid: Vec<[f64; 2]> = groups.iter().map(|(m, s)| [s[0] / m.len() as f64, s[1] / m.len() as f64]).collect();
        let mut by_cell: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (g, c) in centroid.iter().enumerate() {
            by_cell.entry(key(*c)).or_default().push(g);
        }
        let mut order: Vec<usize> = (0..groups.len()).collect();
        order.sort_by_key(|&g| std::cmp::Reverse(groups[g].0.len()));
        let mut alive = vec![true; groups.len()];
        let mut merges = Vec::new();
        for &a in &order {
            if !alive[a] {
                continue;
            }
            let (ca, (kx, ky)) = (centroid[a], key(centroid[a]));
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for &b in by_cell.get(&(kx + dx, ky + dy)).into_iter().flatten() {
                        let cb = centroid[b];
                        if b != a && alive[b] && (ca[0] - cb[0]).hypot(ca[1] - cb[1]) < cell * 0.85 {
                            alive[b] = false;
                            merges.push((a, b));
                        }
                    }
                }
            }
        }
        for (a, b) in merges {
            let (members, sum) = std::mem::take(&mut groups[b]);
            groups[a].0.extend(members);
            groups[a].1[0] += sum[0];
            groups[a].1[1] += sum[1];
        }

        let old = std::mem::take(&mut self.pins);
        let old_owner = std::mem::take(&mut self.owner);
        let mut by_rep: HashMap<u32, usize> =
            old.iter().enumerate().filter(|(_, p)| !p.dying).map(|(i, p)| (p.rep, i)).collect();
        let mut reused = vec![false; old.len()];
        self.owner = vec![NONE; self.markers.len()];
        for (g, (members, sum)) in groups.into_iter().enumerate() {
            if !alive[g] {
                continue;
            }
            let target = [sum[0] / members.len() as f64, sum[1] / members.len() as f64];
            let rep = *members.iter().max_by_key(|&&m| (self.markers[m as usize].rank, std::cmp::Reverse(m))).unwrap();
            let idx = self.pins.len() as u32;
            for &m in &members {
                self.owner[m as usize] = idx;
            }
            let title = self.title(&members);
            let pin = match by_rep.remove(&rep) {
                Some(j) => {
                    reused[j] = true;
                    let o = &old[j];
                    Pin { rep, members, title, pos: o.pos, target, alpha: o.alpha, dying: false, hover: o.hover }
                }
                None => {
                    // New pins grow out of whichever pin used to hold their photo.
                    let from = old_owner.get(rep as usize).and_then(|&o| old.get(o as usize)).map_or(target, |p| p.pos);
                    Pin { rep, members, title, pos: from, target, alpha: 0.0, dying: false, hover: 0.0 }
                }
            };
            self.pins.push(pin);
        }
        // Vanishing pins slide into the pin that now holds their photo while fading out.
        for (j, mut p) in old.into_iter().enumerate() {
            if reused[j] {
                continue;
            }
            if !p.dying {
                p.dying = true;
                if let Some(&o) = self.owner.get(p.rep as usize).filter(|&&o| o != NONE) {
                    p.target = self.pins[o as usize].target;
                }
            }
            self.pins.push(p);
        }
        self.hovered = None;
    }

    // --- frame ---

    /// Advances animations, uploads arrived textures, queues downloads and rebuilds the map's
    /// draw list. `area` is the map's physical rect; `header` is the header height on top of it.
    /// Returns true while anything is still changing.
    pub fn update(
        &mut self,
        now: Instant,
        renderer: &mut Renderer,
        library: Option<&Arc<Library>>,
        area: [f32; 4],
        header: f32,
        scale: f32,
    ) -> bool {
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        self.frame += 1;
        let resized = area != self.area || scale != self.scale;
        (self.area, self.header, self.scale) = (area, header, scale);

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Markers(m) => {
                    self.status.clear();
                    self.set_markers(m);
                }
                Msg::Error(e) => {
                    self.status = e;
                    self.loaded = true;
                }
                m => self.uploads.push_back(m),
            }
        }
        if let Some(lib) = library {
            let ptr = Arc::as_ptr(lib) as usize;
            if self.loaded && self.ranked != ptr {
                self.ranked = ptr;
                self.rank(lib);
            }
        }
        if area[2] < 1.0 || area[3] < 1.0 {
            return true;
        }
        if let Some((lat, lon, z)) = self.pending_view.take() {
            self.look_at(mercator(lat, lon), z);
        } else if self.need_fit && self.loaded {
            self.need_fit = false;
            self.fit();
        } else if resized {
            self.clamp();
        }

        // Camera: zoom glides toward its target around the anchor; drags coast.
        let mut moving = false;
        if (self.target_zoom - self.zoom).abs() > 1e-3 {
            let w = self.to_world(self.anchor);
            self.zoom += (self.target_zoom - self.zoom) * (1.0 - (-dt as f64 * 14.0).exp());
            self.pin_point(w, self.anchor);
            moving = true;
        } else if self.zoom != self.target_zoom {
            let w = self.to_world(self.anchor);
            self.zoom = self.target_zoom;
            self.pin_point(w, self.anchor);
        }
        if self.press.is_none() && self.velocity[0].hypot(self.velocity[1]) > 15.0 {
            self.pan([self.velocity[0] * dt, self.velocity[1] * dt]);
            let k = (-dt * 4.0).exp();
            self.velocity = [self.velocity[0] * k, self.velocity[1] * k];
            moving = true;
        } else if self.press.is_none() {
            self.velocity = [0.0; 2];
        }
        self.clamp();

        // Clusters change in half-zoom steps.
        let level = ((self.zoom * 2.0).floor() / 2.0).min(MAX_ZOOM);
        if self.loaded && level != self.level {
            self.recluster(level);
        }
        let world_px = self.world_px();
        let k = 1.0 - (-dt as f64 * 12.0).exp();
        let hovered = if self.press.is_some() { None } else { self.cursor.and_then(|c| self.pin_at(c)) };
        self.hovered = hovered;
        for (i, p) in self.pins.iter_mut().enumerate() {
            let d = [p.target[0] - p.pos[0], p.target[1] - p.pos[1]];
            if d[0].hypot(d[1]) * world_px > 0.3 {
                p.pos = [p.pos[0] + d[0] * k, p.pos[1] + d[1] * k];
                moving = true;
            } else {
                p.pos = p.target;
            }
            let a = if p.dying { (p.alpha - dt / FADE).max(0.0) } else { (p.alpha + dt / 0.3).min(1.0) };
            let h = if Some(i) == hovered { 1.0 } else { 0.0 };
            let hv = p.hover + (h - p.hover) * (1.0 - (-dt * 16.0).exp());
            p.hover = if (hv - h).abs() < 0.01 { h } else { hv };
            moving |= a != p.alpha || p.hover != h;
            p.alpha = a;
        }
        if self.pins.iter().any(|p| p.dying && p.alpha <= 0.0) {
            self.pins.retain(|p| !(p.dying && p.alpha <= 0.0));
            self.hovered = self.cursor.and_then(|c| self.pin_at(c));
        }

        self.upload(renderer, now);
        let fading = self.build(renderer, now);
        self.busy = moving || fading || !self.uploads.is_empty();
        self.busy
    }

    /// Appends the map built by `update` to the UI: the map itself to the base layer, the hovered
    /// pin to the overlay.
    pub fn draw(&mut self, out: &mut ui::Frame) {
        out.base.quads.append(&mut self.layer.quads);
        out.base.texts.append(&mut self.layer.texts);
        out.overlay.quads.append(&mut self.top.quads);
        out.overlay.texts.append(&mut self.top.texts);
    }

    fn upload(&mut self, renderer: &mut Renderer, now: Instant) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(6) {
            let Some(msg) = self.uploads.pop_front() else { break };
            let (img, slot) = match msg {
                Msg::Tile(k, img) => (img, Slot::Tile(k)),
                Msg::Thumb(id, img) => (img, Slot::Thumb(id)),
                _ => continue,
            };
            let image = renderer.ui.create_image(&renderer.device, &renderer.queue, img.width(), img.height(), img.as_raw());
            let tex = Tex { image, used: self.frame, born: now };
            let old = match slot {
                Slot::Tile(k) => self.tiles.insert(k, tex),
                Slot::Thumb(id) => self.thumbs.insert(id, tex),
            };
            if let Some(old) = old {
                renderer.ui.free_image(old.image);
            }
        }
    }

    /// Lays out tiles, pins and chrome into `self.layer`, touching textures and queueing
    /// downloads for what's missing. Returns true while textures are fading in.
    fn build(&mut self, renderer: &mut Renderer, now: Instant) -> bool {
        self.layer.clear();
        self.top.clear();
        let (area, s, frame) = (self.area, self.scale, self.frame);
        let mut fading = false;
        let fade_of = |t: &Tex| (now.duration_since(t.born).as_secs_f32() / FADE).min(1.0);

        // Basemap at the integer zoom nearest the camera's.
        let tz = (self.zoom.round().clamp(0.0, MAX_ZOOM) as u8).min(TILE_MAX);
        let n = 1u32 << tz;
        let tl = self.to_world([area[0], area[1]]);
        let br = self.to_world([area[0] + area[2], area[1] + area[3]]);
        let range = |a: f64, b: f64| ((a * n as f64).floor().max(0.0) as u32, ((b * n as f64).ceil() as u32).min(n));
        let ((x0, x1), (y0, y1)) = (range(tl[0], br[0]), range(tl[1], br[1]));
        let focus = self.to_world(self.focus());
        let mut wants: Vec<(f64, TileKey)> = Vec::new();
        let mut quads: Vec<(TileKey, [f32; 4], [f32; 4], f32)> = Vec::new();
        for y in y0..y1 {
            for x in x0..x1 {
                let key = TileKey { z: tz, x, y };
                // Edges snapped to whole pixels and shared with neighbours: no seams.
                let a = self.to_screen([x as f64 / n as f64, y as f64 / n as f64]);
                let b = self.to_screen([(x + 1) as f64 / n as f64, (y + 1) as f64 / n as f64]);
                let (l, t, r, bt) = (a[0].round() as f32, a[1].round() as f32, b[0].round() as f32, b[1].round() as f32);
                let rect = [l, t, r - l, bt - t];
                let own = self.tiles.get(&key).map(fade_of);
                if own.is_none_or(|f| f < 1.0) {
                    // Stand in with the nearest loaded ancestor, cropped, or else loaded children.
                    let mut found = false;
                    for d in 1..=tz.min(8) {
                        let pk = TileKey { z: tz - d, x: x >> d, y: y >> d };
                        if self.tiles.contains_key(&pk) {
                            let f = (1u32 << d) as f32;
                            let (u, v) = ((x - (pk.x << d)) as f32 / f, (y - (pk.y << d)) as f32 / f);
                            quads.push((pk, rect, [u, v, u + 1.0 / f, v + 1.0 / f], 1.0));
                            found = true;
                            break;
                        }
                    }
                    if !found && tz < TILE_MAX {
                        for (cx, cy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                            let ck = TileKey { z: tz + 1, x: x * 2 + cx, y: y * 2 + cy };
                            if self.tiles.contains_key(&ck) {
                                let (hw, hh) = (rect[2] / 2.0, rect[3] / 2.0);
                                let r = [l + cx as f32 * hw, t + cy as f32 * hh, hw, hh];
                                quads.push((ck, r, [0.0, 0.0, 1.0, 1.0], 1.0));
                            }
                        }
                    }
                }
                match own {
                    Some(f) => {
                        fading |= f < 1.0;
                        quads.push((key, rect, [0.0, 0.0, 1.0, 1.0], ease_out(f)));
                    }
                    None => {
                        let c = [(x as f64 + 0.5) / n as f64 - focus[0], (y as f64 + 0.5) / n as f64 - focus[1]];
                        wants.push((c[0] * c[0] + c[1] * c[1], key));
                    }
                }
            }
        }
        for (key, rect, uv, alpha) in quads {
            let tex = self.tiles.get_mut(&key).unwrap();
            tex.used = frame;
            push(&mut self.layer, Quad::image(rect, tex.image, uv, alpha, 0.0), area);
        }
        wants.sort_by(|a, b| a.0.total_cmp(&b.0));
        let visible_tiles = ((x1 - x0) * (y1 - y0)) as usize;
        self.shared.tiles.lock().unwrap().set(wants.into_iter().map(|w| w.1));
        let cap = GPU_TILES.max(visible_tiles * 2);
        evict(&mut self.tiles, cap, frame, renderer);

        // Pins: dying ones underneath, then top to bottom, the hovered one last.
        let pad = PIN * s;
        let mut visible: Vec<usize> = (0..self.pins.len())
            .filter(|&i| {
                let r = self.pin_rect(&self.pins[i]);
                self.pins[i].alpha > 0.0
                    && r[0] + r[2] + pad > area[0]
                    && r[0] - pad < area[0] + area[2]
                    && r[1] + r[3] + pad > area[1]
                    && r[1] - pad < area[1] + area[3]
            })
            .collect();
        visible.sort_by(|&a, &b| {
            let (pa, pb) = (&self.pins[a], &self.pins[b]);
            (!pa.dying, Some(a) == self.hovered).cmp(&(!pb.dying, Some(b) == self.hovered)).then(pa.pos[1].total_cmp(&pb.pos[1]))
        });
        let mut thumb_wants: Vec<(f32, String)> = Vec::new();
        let fc = self.focus();
        for &i in &visible {
            let pin = &self.pins[i];
            let r = self.pin_rect(pin);
            // The hovered pin and its label go on the overlay, above every badge's text.
            let layer = if Some(i) == self.hovered { &mut self.top } else { &mut self.layer };
            let a = ease_out(pin.alpha);
            let id = &self.markers[pin.rep as usize].id;
            let radius = r[2] * 0.21;
            for k in 1..=4 {
                let e = k as f32 * 1.6 * s;
                let rect = [r[0] - e, r[1] - e + 2.5 * s, r[2] + 2.0 * e, r[3] + 2.0 * e];
                push(layer, Quad::solid(rect, [0.0, 0.0, 0.0, 0.075 * a], radius + e), area);
            }
            push(layer, Quad::solid(r, [1.0, 1.0, 1.0, a], radius), area);
            let b = 2.0 * s * r[2] / (PIN * s);
            let inner = [r[0] + b, r[1] + b, r[2] - 2.0 * b, r[3] - 2.0 * b];
            push(layer, Quad::solid(inner, ui::rgb(theme::separator(), a), radius - b), area);
            match self.thumbs.get_mut(id) {
                Some(t) => {
                    t.used = frame;
                    let f = fade_of(t);
                    fading |= f < 1.0;
                    push(layer, Quad::image(inner, t.image, [0.0, 0.0, 1.0, 1.0], a * ease_out(f), radius - b), area);
                }
                None if !pin.dying => {
                    let d = (r[0] + r[2] / 2.0 - fc[0]).hypot(r[1] + r[3] / 2.0 - fc[1]);
                    thumb_wants.push((d, id.clone()));
                }
                None => {}
            }
            let count = pin.members.len();
            if count > 1 {
                let label = group(count);
                let size = 12.0 * s;
                let tw = renderer.ui.measure(&label, size, 700, Font::Sans);
                let h = 20.0 * s;
                let w = (tw + 12.0 * s).max(h);
                let (cx, cy) = (r[0] + r[2] - 3.0 * s, r[1] + 3.0 * s);
                let ring = 1.5 * s;
                let outer = [cx - w / 2.0 - ring, cy - h / 2.0 - ring, w + 2.0 * ring, h + 2.0 * ring];
                push(layer, Quad::solid(outer, [1.0, 1.0, 1.0, a], outer[3] / 2.0), area);
                let p = theme::palette();
                push(layer, Quad::solid([cx - w / 2.0, cy - h / 2.0, w, h], ui::rgb(p.accent, a), h / 2.0), area);
                let text = Text::new(label, cx, cy - size * 0.625, size)
                    .weight(700)
                    .color(theme::text_rgba(p.on_accent, a))
                    .align(ui::Align::Center)
                    .clip(area);
                layer.text(text);
            }
        }
        thumb_wants.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.shared.thumbs.lock().unwrap().set(thumb_wants.into_iter().map(|w| w.1));
        evict(&mut self.thumbs, GPU_THUMBS.max(visible.len() * 2), frame, renderer);
        self.shared.notify.notify_waiters();

        // Header scrim, as over the grid.
        let h = self.header * 1.6;
        let bands = 24;
        for i in 0..bands {
            let t = (i as f32 + 0.5) / bands as f32;
            let alpha = 0.86 * (1.0 - t * t * (3.0 - 2.0 * t));
            let y0 = area[1] + (h * i as f32 / bands as f32).round();
            let y1 = area[1] + (h * (i + 1) as f32 / bands as f32).round();
            self.layer.quad(Quad::solid([area[0], y0, area[2], y1 - y0], ui::rgb(theme::scrim(), alpha), 0.0));
        }

        // Place name under the hovered pin.
        if let Some(pin) = self.hovered.map(|i| &self.pins[i]).filter(|p| p.hover > 0.05) {
            let r = self.pin_rect(pin);
            let size = 13.0 * s;
            let tw = renderer.ui.measure(&pin.title, size, 600, Font::Sans);
            let (w, h) = (tw + 20.0 * s, 26.0 * s);
            let (cx, y) = (r[0] + r[2] / 2.0, r[1] + r[3] + 8.0 * s);
            let a = pin.hover;
            push(&mut self.top, Quad::solid([cx - w / 2.0, y + 2.0 * s, w, h], [0.0, 0.0, 0.0, 0.3 * a], h / 2.0), area);
            let p = theme::palette();
            push(&mut self.top, Quad::solid([cx - w / 2.0, y, w, h], ui::rgb(p.raised, 0.96 * a), h / 2.0), area);
            let text = Text::new(pin.title.clone(), cx, y + (h - size * 1.25) / 2.0, size)
                .weight(600)
                .color(theme::text_rgba(p.fg, 0.95 * a))
                .align(ui::Align::Center)
                .clip(area);
            self.top.text(text);
        }

        if self.loaded && self.markers.is_empty() && self.status.is_empty() {
            let c = self.focus();
            // Over the (always dark) map tiles.
            let text = Text::new("No photos with a location yet", c[0], c[1] - 10.0 * s, 17.0 * s)
                .weight(600)
                .color(theme::text_rgba(theme::hud().fg_dim, 0.9))
                .align(ui::Align::Center);
            self.layer.text(text);
        }

        let size = 10.0 * s;
        let tw = renderer.ui.measure(ATTRIBUTION, size, 400, Font::Sans);
        let (w, h) = (tw + 12.0 * s, size * 1.25 + 6.0 * s);
        let (x, y) = (area[0] + area[2] - w - 6.0 * s, area[1] + area[3] - h - 6.0 * s);
        let hud = theme::hud();
        self.layer.quad(Quad::solid([x, y, w, h], ui::rgb(hud.glass, 0.7), 5.0 * s));
        self.layer.text(Text::new(ATTRIBUTION, x + 6.0 * s, y + 3.0 * s, size).color(theme::text_rgba(hud.fg_dim, 0.85)));
        fading
    }
}

impl Drop for MapView {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.notify.notify_waiters();
    }
}

enum Slot {
    Tile(TileKey),
    Thumb(String),
}

/// Frees least-recently-drawn textures beyond `cap`, never ones drawn this frame.
fn evict<K: Clone + Eq + Hash>(map: &mut HashMap<K, Tex>, cap: usize, frame: u64, renderer: &mut Renderer) {
    if map.len() <= cap {
        return;
    }
    let mut old: Vec<(u64, K)> = map.iter().filter(|(_, t)| t.used < frame).map(|(k, t)| (t.used, k.clone())).collect();
    old.sort_by_key(|o| o.0);
    for (_, k) in old.into_iter().take(map.len() - cap) {
        if let Some(t) = map.remove(&k) {
            renderer.ui.free_image(t.image);
        }
    }
}

/// Pushes a quad cropped to `clip`, adjusting its texture coordinates to match.
fn push(layer: &mut ui::Layer, mut q: Quad, clip: [f32; 4]) {
    let [x, y, w, h] = q.rect;
    let (x0, y0) = (x.max(clip[0]), y.max(clip[1]));
    let (x1, y1) = ((x + w).min(clip[0] + clip[2]), (y + h).min(clip[1] + clip[3]));
    if x1 <= x0 || y1 <= y0 || q.color[3] <= 0.0 {
        return;
    }
    if (x0, y0, x1, y1) != (x, y, x + w, y + h) {
        let [u0, v0, u1, v1] = q.uv;
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        q.uv = [lerp(u0, u1, (x0 - x) / w), lerp(v0, v1, (y0 - y) / h), lerp(u0, u1, (x1 - x) / w), lerp(v0, v1, (y1 - y) / h)];
        q.rect = [x0, y0, x1 - x0, y1 - y0];
    }
    layer.quad(q);
}
