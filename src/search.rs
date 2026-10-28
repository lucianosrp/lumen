//! Smart search, like Apple Photos': a search field pinned at the top of the sidebar, a
//! suggestions popover while typing (people, places, albums, recent searches), and results
//! from Immich's CLIP search shown in the grid in relevance order, best match at the top.
//!
//! Requests run in the background and carry a sequence number: replies for anything but the
//! latest query (or filter change) are dropped, so an old, slow answer never replaces a newer
//! one. The first smart search after the server idled can take most of a minute while it loads
//! its model; the grid says so after a few seconds, and focusing the field warms it up early.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::Core;
use crate::icons::IconStyle;
use crate::immich::Immich;
use crate::library::Asset;
use crate::loader::{Collection, Source, group};
use crate::ui::{Align, Font, Layer, Quad, Text, contains, rgb, theme};
use crate::widgets::{self, Edit, ICON_WEIGHT, TextField};

/// Results per request.
const PAGE: usize = 250;
/// After this long without an answer, explain that the server may be loading its model.
const SLOW: Duration = Duration::from_secs(3);
/// A cold server took 47 s to answer the first query; leave room for that.
const TIMEOUT: Duration = Duration::from_secs(150);
/// Focusing the field wakes the server's model unless a search ran this recently.
const WARM_FOR: Duration = Duration::from_secs(10 * 60);
const RECENT_MAX: usize = 10;
/// Extra header height for the filter chips (logical px).
pub const CHIPS: f32 = 40.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    /// CLIP search for a description.
    Smart(String),
    City(String),
    Country(String),
}

impl Query {
    fn title(&self) -> String {
        match self {
            Query::Smart(t) => {
                let t: String = if t.chars().count() > 60 { t.chars().take(58).collect::<String>() + "…" } else { t.clone() };
                format!("“{}”", t.trim())
            }
            Query::City(n) | Query::Country(n) => n.clone(),
        }
    }

    fn text(&self) -> &str {
        match self {
            Query::Smart(t) | Query::City(t) | Query::Country(t) => t,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Kind {
    #[default]
    All,
    Photos,
    Videos,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum When {
    #[default]
    Any,
    Year,
    FiveYears,
}

/// The chip row under the results header.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Filters {
    pub kind: Kind,
    pub favorites: bool,
    pub when: When,
}

impl Filters {
    fn any(&self) -> bool {
        *self != Filters::default()
    }
}

enum State {
    Loading,
    Ready,
    Failed(String),
}

/// The query whose results are (or are about to be) in the grid.
struct Active {
    query: Query,
    filters: Filters,
    seq: u64,
    /// Collection key of its results, unique per request.
    key: String,
    assets: Vec<Asset>,
    /// Next page to ask for, if the server has more.
    next: Option<u32>,
    state: State,
    started: Instant,
    /// The grid has shown this collection; leaving it afterwards means the user moved on.
    shown: bool,
    /// A next-page request is in flight.
    more: bool,
    /// The grid generation when results were last handed to the loader: until it changes, the
    /// grid hasn't caught up, so don't ask for more yet.
    published: Option<u64>,
}

/// Where Esc goes back to.
enum Back {
    Collection(Collection, String),
    Map,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PlaceKind {
    City,
    Country,
}

enum Reply {
    Results { seq: u64, page: u32, result: Result<(Vec<Asset>, Option<u32>), String>, took: Duration },
    Places(Vec<(String, PlaceKind)>),
}

#[derive(Clone, Debug, PartialEq)]
enum Row {
    Search(String),
    Recent(String),
    Person { id: String, name: String },
    Place(String, PlaceKind),
    Album { id: String, name: String, count: usize },
}

enum Item {
    Header(&'static str),
    Row(Row),
    /// Help text when there's nothing to suggest yet.
    Hint,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Field,
    Clear,
    Row(usize),
    Forget(usize),
    Panel,
    Chip(Chip),
    Retry,
    ClearFilters,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chip {
    Kind(Kind),
    Favorites,
    When(When),
}

pub struct Search {
    pub field: TextField,
    pub focused: bool,
    /// Highlighted suggestion (index among rows, headers excluded).
    sel: Option<usize>,
    cursor: (f32, f32),
    /// Clickable rects from the last frame, topmost first.
    hits: Vec<(Hit, [f32; 4])>,
    armed: Option<Hit>,
    recent: Vec<String>,
    places: Vec<(String, PlaceKind)>,
    places_asked: bool,
    places_pending: bool,
    active: Option<Active>,
    back: Option<Back>,
    seq: u64,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    task: Option<tokio::task::AbortHandle>,
    /// Last time the server answered (or was asked to warm up) a smart search.
    warm: Option<Instant>,
    epoch: Instant,
    /// Headless testing: hold every request this long first, or fail it.
    pub delay: Option<Duration>,
    pub fail: bool,
    /// Headless `--search-wait`: render mid-request, so don't count as busy.
    hold: bool,
}

impl Search {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            field: TextField::default(),
            focused: false,
            sel: None,
            cursor: (-1.0, -1.0),
            hits: Vec::new(),
            armed: None,
            recent: load_recent(),
            places: Vec::new(),
            places_asked: false,
            places_pending: false,
            active: None,
            back: None,
            seq: 0,
            tx,
            rx,
            task: None,
            warm: None,
            epoch: Instant::now(),
            delay: None,
            fail: false,
            hold: false,
        }
    }

    /// Nothing is being fetched.
    pub fn is_idle(&self) -> bool {
        self.hold || !self.places_pending && self.active.as_ref().is_none_or(|a| !matches!(a.state, State::Loading) && !a.more)
    }

    /// Results (or their loading/empty/error state) own the grid area right now.
    fn showing(&self, core_key: &str) -> bool {
        self.active.as_ref().is_some_and(|a| !a.shown || a.key == core_key)
    }
}

fn wake_of(core: &Core) -> std::sync::Arc<dyn Fn() + Send + Sync> {
    core.loader.waker()
}

// ---------------------------------------------------------------------------------------------
// Commands

/// Puts the keyboard in the field (`/`, Ctrl+F, or a click), sliding the sidebar in if needed.
pub fn focus(core: &mut Core, select_all: bool) {
    core.sidebar.reveal();
    let se = &mut core.search;
    if !se.focused {
        se.focused = true;
        se.sel = (!se.field.is_empty()).then_some(0);
    }
    if select_all {
        se.field.select_all();
    }
    fetch_places(core);
    warm_up(core);
}

fn blur(core: &mut Core) {
    core.search.focused = false;
    core.search.sel = None;
}

/// Runs `query` with `filters` and shows its results in the grid.
fn run(core: &mut Core, query: Query, filters: Filters) {
    if query.text().trim().is_empty() {
        return;
    }
    // Remember where we came from, unless we're already looking at results.
    let on_results = core.search.active.as_ref().is_some_and(|a| a.shown && a.key == core.collection.key);
    if !on_results && core.search.back.is_none() {
        core.search.back = Some(if core.map.is_some() {
            Back::Map
        } else {
            Back::Collection((*core.collection).clone(), core.sidebar.selected().to_string())
        });
    }
    crate::settings_ui::leave(core);
    core.set_map(false);
    if core.viewer.is_open() {
        core.viewer.close_since(0);
    }
    let se = &mut core.search;
    if let Some(t) = se.task.take() {
        t.abort();
    }
    se.seq += 1;
    let seq = se.seq;
    let key = format!("search:{seq}");
    let title = query.title();
    se.active = Some(Active {
        query: query.clone(),
        filters,
        seq,
        key: key.clone(),
        assets: Vec::new(),
        next: None,
        state: State::Loading,
        started: Instant::now(),
        shown: false,
        more: false,
        published: None,
    });
    core.sidebar.set_selected("search");
    core.loader.open(Collection::results(&key, &title, Vec::new()));
    request(core, 1);
}

/// Smart search for what's typed (Enter, or "Search for …").
fn submit(core: &mut Core, text: &str) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    remember(core, &text);
    core.search.field.set(&text);
    blur(core);
    run(core, Query::Smart(text), Filters::default());
}

/// The chips changed: same query, new filters, from the top.
fn refilter(core: &mut Core, filters: Filters) {
    let Some(a) = &core.search.active else { return };
    if a.filters == filters {
        return;
    }
    let q = a.query.clone();
    run(core, q, filters);
}

/// Esc: clears the field and goes back to what was on screen before searching.
fn leave(core: &mut Core) {
    let se = &mut core.search;
    se.field.clear();
    blur(core);
    let se = &mut core.search;
    let was_showing = se.active.is_some();
    se.active = None;
    se.seq += 1;
    if let Some(t) = se.task.take() {
        t.abort();
    }
    let back = se.back.take();
    if !was_showing {
        return;
    }
    match back {
        Some(Back::Map) => {
            core.sidebar.set_selected("places");
            core.set_map(true);
        }
        Some(Back::Collection(c, key)) => {
            core.sidebar.set_selected(&key);
            core.loader.open(c);
        }
        None => {
            core.sidebar.set_selected("library");
            core.loader.open(Collection::library());
        }
    }
}

/// Esc with the keyboard elsewhere (from the main key handler): leave the results, if shown.
pub fn escape(core: &mut Core) -> bool {
    let key = core.collection.key.clone();
    if core.search.showing(&key) && core.map.is_none() && core.settings_page.is_none() {
        leave(core);
        return true;
    }
    false
}

fn activate_row(core: &mut Core, row: Row) {
    match row {
        Row::Search(t) | Row::Recent(t) => submit(core, &t),
        Row::Person { id, name } => navigate(core, Collection::person(&id, &name)),
        Row::Album { id, name, .. } => navigate(core, Collection::album(&id, &name)),
        Row::Place(name, kind) => {
            core.search.field.set(&name);
            blur(core);
            let q = if kind == PlaceKind::City { Query::City(name) } else { Query::Country(name) };
            run(core, q, Filters::default());
        }
    }
}

/// A person or album suggestion: open it like its sidebar row.
fn navigate(core: &mut Core, c: Collection) {
    let se = &mut core.search;
    se.field.clear();
    se.active = None;
    se.back = None;
    se.seq += 1;
    blur(core);
    core.sidebar.set_selected(&c.key);
    core.apply(crate::sidebar::Action::Open(c));
}

fn remember(core: &mut Core, text: &str) {
    let r = &mut core.search.recent;
    r.retain(|q| !q.eq_ignore_ascii_case(text));
    r.insert(0, text.to_string());
    r.truncate(RECENT_MAX);
    save_recent(r);
}

fn forget(core: &mut Core, text: &str) {
    let r = &mut core.search.recent;
    r.retain(|q| q != text);
    save_recent(r);
}

// ---------------------------------------------------------------------------------------------
// Network

fn immich(core: &Core) -> Option<Immich> {
    match core.loader.source() {
        Source::Immich(i) => Some(i.clone()),
        Source::Demo(_) => None,
    }
}

/// Asks for one page of the active query.
fn request(core: &mut Core, page: u32) {
    let Some(a) = &core.search.active else { return };
    let (seq, query, filters) = (a.seq, a.query.clone(), a.filters);
    let (tx, wake, delay, fail) = (core.search.tx.clone(), wake_of(core), core.search.delay, core.search.fail);
    let server = immich(core);
    let task = core.loader.handle().spawn(async move {
        let start = Instant::now();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        let result = match server {
            _ if fail => Err("Simulated failure (--search-fail): error sending request for url".to_string()),
            None => Err("Search needs an Immich server.".to_string()),
            Some(im) => fetch(&im, &query, filters, page).await.map_err(|e| explain(&e)),
        };
        let _ = tx.send(Reply::Results { seq, page, result, took: start.elapsed() });
        wake();
    });
    if page == 1 {
        core.search.task = Some(task.abort_handle());
    }
}

async fn fetch(im: &Immich, query: &Query, f: Filters, page: u32) -> Result<(Vec<Asset>, Option<u32>)> {
    let mut body = json!({ "page": page, "size": PAGE });
    let path = match query {
        Query::Smart(t) => {
            body["query"] = json!(t);
            "/search/smart"
        }
        Query::City(c) => {
            (body["city"], body["order"]) = (json!(c), json!("desc"));
            "/search/metadata"
        }
        Query::Country(c) => {
            (body["country"], body["order"]) = (json!(c), json!("desc"));
            "/search/metadata"
        }
    };
    match f.kind {
        Kind::All => {}
        Kind::Photos => body["type"] = json!("IMAGE"),
        Kind::Videos => body["type"] = json!("VIDEO"),
    }
    if f.favorites {
        body["isFavorite"] = json!(true);
    }
    let days = match f.when {
        When::Any => None,
        When::Year => Some(365),
        When::FiveYears => Some(5 * 365 + 1),
    };
    if let Some(d) = days {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_secs() as i64);
        body["takenAfter"] = json!(crate::library::iso_date(now - d * 86_400));
    }
    let res = im.post_json_within(path, body, TIMEOUT).await?;
    let assets = &res["assets"];
    let items = assets["items"].as_array().context("unexpected reply from the server")?;
    let out: Vec<Asset> = items.iter().filter_map(asset_of).collect();
    let next = match &assets["nextPage"] {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(|n| n as u32),
        _ => None,
    };
    Ok((out, next.filter(|_| !items.is_empty())))
}

/// An `AssetResponseDto` as the grid's `Asset`.
fn asset_of(v: &Value) -> Option<Asset> {
    let id = v["id"].as_str()?.to_string();
    let num = |v: &Value| v.as_f64().filter(|n| *n > 0.0);
    let exif = &v["exifInfo"];
    let ratio = match (num(&v["width"]), num(&v["height"])) {
        (Some(w), Some(h)) => w / h,
        _ => match (num(&exif["exifImageWidth"]), num(&exif["exifImageHeight"])) {
            // EXIF orientations 5–8 are rotated a quarter turn.
            (Some(w), Some(h)) if matches!(exif["orientation"].as_str(), Some("5" | "6" | "7" | "8")) => h / w,
            (Some(w), Some(h)) => w / h,
            _ => 1.0,
        },
    };
    Some(Asset {
        id,
        taken: v["fileCreatedAt"].as_str().or(v["localDateTime"].as_str()).unwrap_or_default().to_string(),
        ratio: ratio as f32,
        video: v["type"].as_str() == Some("VIDEO"),
        thumbhash: v["thumbhash"].as_str().map(String::from),
        live_video: v["livePhotoVideoId"].as_str().map(String::from),
        duration_ms: crate::immich::duration_ms(&v["duration"]),
        favorite: v["isFavorite"].as_bool().unwrap_or(false),
    })
}

/// A short, human message for a failed request.
fn explain(e: &anyhow::Error) -> String {
    if let Some(r) = e.chain().find_map(|c| c.downcast_ref::<reqwest::Error>()) {
        if r.is_timeout() {
            return "The server took too long to answer. It may still be loading its search model; try again in a moment.".into();
        }
        if r.is_connect() {
            return "Couldn't reach the server. Check your connection.".into();
        }
    }
    let msg = format!("{e:#}");
    if msg.contains("Smart search is not enabled") || msg.contains("machine learning") {
        return "Smart search is turned off on the server (Machine Learning settings).".into();
    }
    msg
}

/// Cities and countries for the Places suggestions, fetched the first time the field is used.
fn fetch_places(core: &mut Core) {
    if core.search.places_asked {
        return;
    }
    core.search.places_asked = true;
    let Some(im) = immich(core) else { return };
    core.search.places_pending = true;
    let (tx, wake) = (core.search.tx.clone(), wake_of(core));
    core.loader.handle().spawn(async move {
        let mut out = Vec::new();
        for (kind, k) in [("city", PlaceKind::City), ("country", PlaceKind::Country)] {
            match im.get_json::<Vec<String>>(&format!("/search/suggestions?type={kind}")).await {
                Ok(names) => out.extend(names.into_iter().filter(|n| !n.trim().is_empty()).map(|n| (n, k))),
                Err(e) => log::warn!("search suggestions ({kind}): {e:#}"),
            }
        }
        let _ = tx.send(Reply::Places(out));
        wake();
    });
}

/// Loads the server's CLIP model while the user is still typing, so the first real search
/// doesn't have to wait for it.
fn warm_up(core: &mut Core) {
    if core.search.warm.is_some_and(|t| t.elapsed() < WARM_FOR) || core.search.delay.is_some() {
        return;
    }
    core.search.warm = Some(Instant::now());
    let Some(im) = immich(core) else { return };
    core.loader.handle().spawn(async move {
        let start = Instant::now();
        let r = im.post_json_within("/search/smart", json!({ "query": "photo", "size": 1 }), TIMEOUT).await;
        log::info!("search warm-up: {} in {:.1} s", if r.is_ok() { "ok" } else { "failed" }, start.elapsed().as_secs_f32());
    });
}

/// Takes replies: results for the latest request only.
fn pump(core: &mut Core) {
    while let Ok(reply) = core.search.rx.try_recv() {
        match reply {
            Reply::Places(p) => {
                core.search.places = p;
                core.search.places_pending = false;
            }
            Reply::Results { seq, page, result, took } => {
                let Some(a) = core.search.active.as_mut().filter(|a| a.seq == seq) else { continue };
                if page == 1 {
                    core.search.task = None;
                }
                match result {
                    Ok((items, next)) => {
                        core.search.warm = Some(Instant::now());
                        log::info!("search {:?} page {page}: {} results in {:.2} s", a.query, items.len(), took.as_secs_f32());
                        // Page 2+ adds to what the grid has now (trashed tiles stay gone).
                        let base = if page > 1 && core.collection.key == a.key {
                            core.library.as_ref().map_or_else(|| a.assets.clone(), |l| l.assets.clone())
                        } else if page > 1 {
                            a.assets.clone()
                        } else {
                            Vec::new()
                        };
                        let mut seen: HashSet<String> = base.iter().map(|x| x.id.clone()).collect();
                        let mut all = base;
                        all.extend(items.into_iter().filter(|x| seen.insert(x.id.clone())));
                        (a.assets, a.next, a.state, a.more) = (all.clone(), next, State::Ready, false);
                        let title = a.query.title();
                        let key = a.key.clone();
                        a.published = Some(core.generation);
                        core.loader.open(Collection::results(&key, &title, all));
                    }
                    Err(e) if page == 1 => {
                        log::warn!("search {:?} failed: {e}", a.query);
                        a.state = State::Failed(e);
                    }
                    Err(e) => {
                        log::warn!("search {:?} page {page} failed: {e}", a.query);
                        (a.more, a.next) = (false, None);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Input

/// Routes input to the search field and its popover before anything else sees it: while the
/// field has focus, every key is the field's (so `.`, `a`, `m`, Delete… type instead of acting).
/// Returns true if the event was consumed.
pub fn input(core: &mut Core, event: &WindowEvent, mods: ModifiersState) -> bool {
    match event {
        WindowEvent::KeyboardInput { event: k, .. } => {
            if core.search.focused {
                if k.state == ElementState::Pressed {
                    // The app's own chords keep working: quit, sidebar, settings.
                    if mods.control_key() && matches!(&k.logical_key, Key::Character(c) if matches!(c.as_str(), "q" | "b" | ",")) {
                        return false;
                    }
                    key(core, k, mods);
                }
                return true;
            }
            let ch = match &k.logical_key {
                Key::Character(c) => c.to_lowercase(),
                _ => String::new(),
            };
            let slash = ch == "/" && !mods.control_key() && !mods.alt_key() && !mods.super_key();
            if k.state == ElementState::Pressed && (slash || (mods.control_key() && ch == "f")) && !core.actions.popup_open() {
                if core.viewer.is_open() {
                    core.viewer.close_since(0);
                }
                focus(core, true);
                return true;
            }
            false
        }
        WindowEvent::CursorMoved { position, .. } => {
            core.search.cursor = (position.x as f32, position.y as f32);
            if let Some(Hit::Row(r) | Hit::Forget(r)) = hit_at(core) {
                core.search.sel = Some(r);
            }
            false
        }
        WindowEvent::MouseWheel { .. } => matches!(hit_at(core), Some(Hit::Row(_) | Hit::Forget(_) | Hit::Panel)),
        WindowEvent::MouseInput { state: ElementState::Pressed, button, .. } => {
            if core.actions.popup_open() {
                return false;
            }
            let hit = hit_at(core);
            if let Some(h) = hit.filter(|_| *button == MouseButton::Left) {
                core.search.armed = Some(h);
                if h == Hit::Field && !core.search.focused {
                    focus(core, false);
                }
                return true;
            }
            if hit.is_some() {
                return true;
            }
            // A click anywhere else takes the keyboard back (and still does its own thing).
            if core.search.focused {
                blur(core);
            }
            false
        }
        WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
            let Some(h) = core.search.armed.take() else { return false };
            if hit_at(core) == Some(h) {
                click(core, h);
            }
            true
        }
        _ => false,
    }
}

fn hit_at(core: &Core) -> Option<Hit> {
    let (x, y) = core.search.cursor;
    core.search.hits.iter().find(|(_, r)| contains(*r, x, y)).map(|h| h.0)
}

fn click(core: &mut Core, h: Hit) {
    match h {
        Hit::Field | Hit::Panel => {}
        Hit::Clear => {
            core.search.field.clear();
            core.search.sel = None;
            if !core.search.focused {
                focus(core, false);
            }
        }
        Hit::Row(i) => {
            if let Some(row) = rows(core).into_iter().nth(i) {
                activate_row(core, row);
            }
        }
        Hit::Forget(i) => {
            if let Some(Row::Recent(t)) = rows(core).into_iter().nth(i) {
                forget(core, &t);
                core.search.sel = None;
            }
        }
        Hit::Chip(c) => {
            let Some(mut f) = core.search.active.as_ref().map(|a| a.filters) else { return };
            match c {
                Chip::Kind(k) => f.kind = k,
                Chip::Favorites => f.favorites = !f.favorites,
                Chip::When(w) => f.when = w,
            }
            refilter(core, f);
        }
        Hit::Retry => {
            if let Some(a) = &core.search.active {
                let (q, f) = (a.query.clone(), a.filters);
                run(core, q, f);
            }
        }
        Hit::ClearFilters => refilter(core, Filters::default()),
    }
}

fn key(core: &mut Core, k: &winit::event::KeyEvent, mods: ModifiersState) {
    let n = rows(core).len();
    let se = &mut core.search;
    match &k.logical_key {
        Key::Named(NamedKey::Escape) => leave(core),
        Key::Named(NamedKey::Tab) => blur(core),
        Key::Named(NamedKey::Enter) => {
            let row = se.sel.and_then(|i| rows(core).into_iter().nth(i));
            match row {
                Some(r) => activate_row(core, r),
                None => {
                    let t = core.search.field.text.clone();
                    if t.trim().is_empty() {
                        blur(core);
                    } else {
                        submit(core, &t);
                    }
                }
            }
        }
        Key::Named(NamedKey::ArrowDown) if n > 0 => se.sel = Some(se.sel.map_or(0, |i| (i + 1).min(n - 1))),
        Key::Named(NamedKey::ArrowUp) if n > 0 => se.sel = se.sel.and_then(|i| i.checked_sub(1)),
        Key::Named(NamedKey::ArrowDown | NamedKey::ArrowUp) => {}
        _ => {
            if se.field.key(k, mods) == Edit::Changed {
                se.sel = (!se.field.text.trim().is_empty()).then_some(0);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Suggestions

/// How well `name` matches the typed text `q` (lowercase): lower is better.
fn score(name: &str, q: &str) -> Option<u8> {
    let n = name.to_lowercase();
    if n == q {
        Some(0)
    } else if n.starts_with(q) {
        Some(1)
    } else if n.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(q)) {
        Some(2)
    } else if q.chars().count() >= 4 && n.contains(q) {
        Some(3)
    } else {
        None
    }
}

fn best<T>(items: impl Iterator<Item = (String, T)>, q: &str, max: usize) -> Vec<(String, T)> {
    let mut m: Vec<(u8, usize, String, T)> = items.filter_map(|(n, t)| Some((score(&n, q)?, n.chars().count(), n, t))).collect();
    m.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    m.into_iter().take(max).map(|(_, _, n, t)| (n, t)).collect()
}

/// The popover's contents for what's typed: sections with their rows.
fn items(core: &Core) -> Vec<Item> {
    let se = &core.search;
    let text = se.field.text.trim();
    let mut out = Vec::new();
    if text.is_empty() {
        if se.recent.is_empty() {
            out.push(Item::Hint);
        } else {
            out.push(Item::Header("Recent Searches"));
            out.extend(se.recent.iter().map(|r| Item::Row(Row::Recent(r.clone()))));
        }
        return out;
    }
    let q = text.to_lowercase();
    out.push(Item::Row(Row::Search(text.to_string())));
    let people = best(core.sidebar.people().into_iter().map(|(id, name)| (name, id)), &q, 4);
    if !people.is_empty() {
        out.push(Item::Header("People"));
        out.extend(people.into_iter().map(|(name, id)| Item::Row(Row::Person { id, name })));
    }
    let places = best(se.places.iter().cloned(), &q, 4);
    if !places.is_empty() {
        out.push(Item::Header("Places"));
        out.extend(places.into_iter().map(|(n, k)| Item::Row(Row::Place(n, k))));
    }
    let albums = best(core.sidebar.albums().into_iter().map(|(id, name, count)| (name, (id, count))), &q, 4);
    if !albums.is_empty() {
        out.push(Item::Header("Albums"));
        out.extend(albums.into_iter().map(|(name, (id, count))| Item::Row(Row::Album { id, name, count })));
    }
    out
}

fn rows(core: &Core) -> Vec<Row> {
    items(core)
        .into_iter()
        .filter_map(|it| match it {
            Item::Row(r) => Some(r),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Per frame

/// Takes replies, follows navigation, pages in more results, and draws the field, popover,
/// filter chips and the empty/loading/error states. Returns true while animating.
pub fn update(core: &mut Core, now: Instant) -> bool {
    pump(core);
    core.search.hits.clear();
    let key = core.collection.key.clone();
    let grid = core.map.is_none() && core.settings_page.is_none();

    // Follow the grid: the results appeared, or the user went somewhere else.
    if let Some(a) = &mut core.search.active {
        if a.key == key {
            a.shown = true;
        } else if a.shown || (!grid && !matches!(a.state, State::Loading)) {
            let focused = core.search.focused;
            let se = &mut core.search;
            (se.active, se.back) = (None, None);
            se.seq += 1;
            if let Some(t) = se.task.take() {
                t.abort();
            }
            if !focused {
                se.field.clear();
            }
        }
    }
    let showing = grid && core.search.showing(&key);
    core.set_header_extra(if showing { CHIPS } else { 0.0 });

    // Page in more as the end comes into view.
    if showing
        && let Some(a) = &core.search.active
        && matches!(a.state, State::Ready)
        && a.next.is_some()
        && !a.more
        && a.key == key
        && a.published != Some(core.generation)
        && let Some(lib) = &core.library
        && lib.len() > 0
    {
        let vp = core.layout.vp;
        let near = core.layout.index_at(vp.width - 1.0, vp.height * 2.5).is_some_and(|(i, _, _)| i + 1 >= lib.len());
        if near {
            let page = a.next.unwrap();
            core.search.active.as_mut().unwrap().more = true;
            request(core, page);
        }
    }

    let t = now.duration_since(core.search.epoch).as_secs_f32();
    let mut busy = draw_field(core, t);
    if showing {
        busy |= draw_results_chrome(core, now, t);
    }
    if core.search.focused {
        draw_popover(core);
    }
    busy && !core.search.hold
}

/// Header title and subtitle while results are on screen.
pub fn header(core: &Core) -> Option<(String, String)> {
    let grid = core.map.is_none() && core.settings_page.is_none();
    let a = core.search.active.as_ref().filter(|_| grid && core.search.showing(&core.collection.key))?;
    let n = core.library.as_ref().filter(|_| a.key == core.collection.key).map_or(0, |l| l.len());
    let sub = match &a.state {
        State::Loading => "Searching…".to_string(),
        State::Failed(_) => "Search failed".to_string(),
        State::Ready if n == 0 => "No results".to_string(),
        State::Ready if !matches!(a.query, Query::Smart(_)) => {
            // A place is a set of photos rather than ranked matches: count them like albums.
            let lib = core.library.as_ref().unwrap();
            let plural = |n: usize, what: &str| format!("{} {what}{}", group(n), if n == 1 { "" } else { "s" });
            let more = if a.next.is_some() { "+" } else { "" };
            let counts = match (lib.len() - lib.videos, lib.videos) {
                (p, 0) => plural(p, "Photo"),
                (0, v) => plural(v, "Video"),
                (p, v) => format!("{} · {}", plural(p, "Photo"), plural(v, "Video")),
            };
            format!("{counts}{more}{}", if a.more { "  ·  Loading more…" } else { "" })
        }
        State::Ready => {
            let more = if a.next.is_some() { "+" } else { "" };
            let what = format!("{}{more} result{}", group(n), if n == 1 && more.is_empty() { "" } else { "s" });
            if a.more { format!("{what}  ·  Loading more…") } else { what }
        }
    };
    Some((a.query.title(), sub))
}

fn loading(core: &Core) -> Option<Instant> {
    core.search.active.as_ref().filter(|a| matches!(a.state, State::Loading)).map(|a| a.started)
}

/// The pill in the sidebar's top strip. Returns true while its spinner turns.
fn draw_field(core: &mut Core, t: f32) -> bool {
    let s = core.scale;
    let r = core.sidebar.search_rect(s);
    if r[0] + r[2] <= 0.0 {
        return false;
    }
    let p = theme::palette();
    let focused = core.search.focused;
    let spinning = loading(core).is_some();
    let base = &mut core.ui.base;
    let radius = r[3] / 2.0;
    if focused {
        let g = (3.0 * s).round();
        base.quad(Quad::solid([r[0] - g, r[1] - g, r[2] + 2.0 * g, r[3] + 2.0 * g], rgb(p.accent, if p.dark { 0.55 } else { 0.5 }), radius + g));
    }
    let hovered = contains(r, core.search.cursor.0, core.search.cursor.1);
    let fill = p.shade(p.surface, if focused { 0.06 } else if hovered { 0.12 } else { 0.09 });
    base.quad(Quad::solid(r, rgb(if focused { theme::mix(p.surface, p.bg, 0.5) } else { fill }, 1.0), radius));
    let mid = r[1] + r[3] / 2.0;
    let icon_x = r[0] + 15.0 * s;
    if spinning {
        widgets::spinner(base, [icon_x, mid], 6.5 * s, t, p.muted);
    } else {
        let px = widgets::even_with(r[3], 14.0 * s);
        base.icon_styled("search", [icon_x.round(), mid.round()], px, rgb(p.muted, 1.0), IconStyle::weight(2.2));
    }
    let has_text = !core.search.field.is_empty();
    let right = if has_text { 30.0 } else { 12.0 } * s;
    let area = [r[0] + 28.0 * s, r[1], r[2] - 28.0 * s - right, r[3]];
    let Core { search, renderer, ui, .. } = &mut *core;
    search.field.draw(&mut ui.base, &mut renderer.ui, area, 13.0, s, "Search", focused);
    if has_text {
        let d = (16.0 * s).round();
        let c = [(r[0] + r[2] - 8.0 * s - d / 2.0).round(), mid.round()];
        let hit = [c[0] - d / 2.0 - 4.0 * s, r[1], d + 8.0 * s, r[3]];
        let hover = contains(hit, search.cursor.0, search.cursor.1);
        ui.base.quad(Quad::solid([c[0] - d / 2.0, c[1] - d / 2.0, d, d], rgb(p.muted, if hover { 0.95 } else { 0.6 }), d / 2.0));
        let px = widgets::even_with(d, 10.0 * s);
        ui.base.icon_styled("x", c, px, rgb(theme::mix(p.surface, p.bg, 0.5), 1.0), IconStyle::weight(3.0));
        search.hits.push((Hit::Clear, hit));
    }
    search.hits.push((Hit::Field, r));
    spinning
}

/// Filter chips under the header, and the centred loading / empty / error states.
fn draw_results_chrome(core: &mut Core, now: Instant, t: f32) -> bool {
    let s = core.scale;
    let Some(a) = &core.search.active else { return false };
    let (filters, started) = (a.filters, a.started);
    let state = match &a.state {
        State::Loading => 0,
        State::Ready => 1,
        State::Failed(_) => 2,
    };
    let error = if let State::Failed(e) = &a.state { e.clone() } else { String::new() };
    let query = a.query.title();
    let n = core.library.as_ref().filter(|_| a.key == core.collection.key).map_or(0, |l| l.len());
    let p = theme::palette();
    let viewing = core.viewer.is_open();

    // Chips.
    let mut x = core.left + 20.0 * s;
    let mid = (core.layout.vp.top - CHIPS * s / 2.0 - 4.0 * s).round();
    let groups: [&[(Chip, &str)]; 3] = [
        &[(Chip::Kind(Kind::All), "All"), (Chip::Kind(Kind::Photos), "Photos"), (Chip::Kind(Kind::Videos), "Videos")],
        &[(Chip::Favorites, "Favorites")],
        &[(Chip::When(When::Any), "Any Time"), (Chip::When(When::Year), "Past Year"), (Chip::When(When::FiveYears), "Past 5 Years")],
    ];
    let mut hits = Vec::new();
    for g in groups {
        for &(chip, label) in g {
            let on = match chip {
                Chip::Kind(k) => filters.kind == k,
                Chip::Favorites => filters.favorites,
                Chip::When(w) => filters.when == w,
            };
            let icon = matches!(chip, Chip::Favorites).then_some("heart");
            let fs = 12.5 * s;
            let tw = core.renderer.ui.measure(label, fs, if on { 600 } else { 500 }, Font::Sans);
            let w = (tw + 24.0 * s + if icon.is_some() { 18.0 * s } else { 0.0 }).round();
            let h = (26.0 * s).round();
            let r = [x.round(), mid - h / 2.0, w, h];
            let hover = !viewing && contains(r, core.search.cursor.0, core.search.cursor.1);
            let (bg, fg) = if on {
                (rgb(if hover { widgets::colors::accent_hover() } else { p.accent }, 1.0), theme::text_rgba(p.on_accent, 1.0))
            } else {
                (rgb(p.shade(p.bg, if hover { 0.17 } else { 0.11 }), 1.0), theme::text_rgba(p.fg, if p.dark { 0.9 } else { 1.0 }))
            };
            let base = &mut core.ui.base;
            base.quad(Quad::solid(r, bg, h / 2.0));
            let mut tx = r[0] + 12.0 * s;
            if let Some(i) = icon {
                let px = widgets::even_with(h, 13.0 * s);
                let style = if on { IconStyle::filled() } else { IconStyle::weight(ICON_WEIGHT) };
                let c = if on { p.on_accent } else { p.red };
                base.icon_styled(i, [(tx + 6.0 * s).round(), mid], px, rgb(c, 1.0), style);
                tx += 18.0 * s;
            }
            base.text(Text::new(label, tx, (mid - fs * 0.625).round(), fs).weight(if on { 600 } else { 500 }).color(fg));
            hits.push((Hit::Chip(chip), r));
            x += w + 6.0 * s;
        }
        x += 10.0 * s;
    }
    if !viewing {
        core.search.hits.extend(hits);
    }

    // Centred states over the empty grid.
    if n > 0 && state == 1 || viewing {
        return false;
    }
    let (w, h) = (core.size.0 as f32, core.size.1 as f32);
    let top = core.layout.vp.top;
    let cx = core.left + (w - core.left) / 2.0;
    let cy = top + (h - top) * 0.38;
    let max_w = ((w - core.left) - 80.0 * s).min(440.0 * s);
    let title = |core: &mut Core, t: &str, y: f32| centered(core, t, cx, y, 17.0, 700, theme::text(), max_w);
    let body = |core: &mut Core, t: &str, y: f32| centered(core, t, cx, y, 13.0, 400, theme::text_dim(), max_w);
    match state {
        0 => {
            widgets::spinner(&mut core.ui.base, [cx, cy], 13.0 * s, t, p.muted);
            let y = title(core, "Searching…", cy + 26.0 * s);
            if now.duration_since(started) >= SLOW && matches!(core.search.active.as_ref().map(|a| &a.query), Some(Query::Smart(_))) {
                body(core, "Waking up search on the server — the first search can take up to a minute.", y + 6.0 * s);
            }
            return true;
        }
        1 => {
            let px = widgets::icon_px(44.0 * s);
            core.ui.base.icon_styled("search-x", [cx.round(), (cy - 6.0 * s).round()], px, rgb(p.muted, 0.9), IconStyle::weight(1.6));
            let y = title(core, &format!("No results for {query}"), cy + 30.0 * s);
            let hint = if filters.any() {
                "Nothing matches with these filters."
            } else {
                "Check the spelling, or describe what's in the photo, like “dog on a beach” or “birthday cake”."
            };
            let y = body(core, hint, y + 6.0 * s);
            if filters.any() {
                let r = text_button(core, "Clear Filters", cx, y + 28.0 * s);
                core.search.hits.push((Hit::ClearFilters, r));
            }
        }
        _ => {
            let px = widgets::icon_px(40.0 * s);
            core.ui.base.icon_styled("triangle-alert", [cx.round(), (cy - 6.0 * s).round()], px, rgb(p.red, 1.0), IconStyle::weight(1.8));
            let y = title(core, "Couldn't Search", cy + 30.0 * s);
            let y = body(core, &error, y + 6.0 * s);
            let r = text_button(core, "Try Again", cx, y + 28.0 * s);
            core.search.hits.push((Hit::Retry, r));
        }
    }
    false
}

/// Centred, wrapped lines starting at `y` (sizes logical); returns the y below them.
#[allow(clippy::too_many_arguments)]
fn centered(core: &mut Core, text: &str, cx: f32, y: f32, size: f32, weight: u16, color: [u8; 4], max_w: f32) -> f32 {
    let fs = size * core.scale;
    let mut y = y;
    // Balanced: the narrowest width that still needs no more lines, so no word sits alone.
    let uir = &mut core.renderer.ui;
    let count = widgets::wrap(uir, text, fs, weight, max_w).len();
    let (mut lo, mut hi) = (max_w * 0.4, max_w);
    while count > 1 && hi - lo > 4.0 {
        let mid = (lo + hi) / 2.0;
        if widgets::wrap(uir, text, fs, weight, mid).len() > count { lo = mid } else { hi = mid }
    }
    for line in widgets::wrap(uir, text, fs, weight, hi) {
        core.ui.base.text(Text::new(line, cx, y.round(), fs).weight(weight).color(color).align(Align::Center));
        y += fs * 1.35;
    }
    y
}

/// A small filled button centred at (cx, mid); returns its rect.
fn text_button(core: &mut Core, label: &str, cx: f32, mid: f32) -> [f32; 4] {
    let s = core.scale;
    let fs = 13.0 * s;
    let w = (core.renderer.ui.measure(label, fs, 600, Font::Sans) + 32.0 * s).round();
    let h = (30.0 * s).round();
    let r = [(cx - w / 2.0).round(), (mid - h / 2.0).round(), w, h];
    let p = theme::palette();
    let hover = contains(r, core.search.cursor.0, core.search.cursor.1);
    let bg = if hover { widgets::colors::accent_hover() } else { p.accent };
    core.ui.base.quad(Quad::solid(r, rgb(bg, 1.0), 8.0 * s));
    core.ui.base.text(Text::new(label, cx, (mid - fs * 0.625).round(), fs).weight(600).color(theme::text_rgba(p.on_accent, 1.0)).align(Align::Center));
    r
}

/// Suggestions under the focused field.
fn draw_popover(core: &mut Core) {
    let s = core.scale;
    let items = items(core);
    if items.is_empty() {
        return;
    }
    let field = core.sidebar.search_rect(s);
    let (row_h, head_h, hint_h, pad) = (34.0 * s, 26.0 * s, 58.0 * s, 6.0 * s);
    let height: f32 = items
        .iter()
        .map(|it| match it {
            Item::Header(_) => head_h,
            Item::Row(_) => row_h,
            Item::Hint => hint_h,
        })
        .sum::<f32>()
        + 2.0 * pad;
    let w = (340.0 * s).min(core.size.0 as f32 - field[0] - 12.0 * s).max(field[2]);
    let r = [field[0], field[1] + field[3] + 8.0 * s, w.round(), height.min(core.size.1 as f32 - field[1] - field[3] - 20.0 * s).round()];
    let p = theme::palette();
    let Core { search: se, renderer, ui, sidebar, .. } = &mut *core;
    let out = &mut ui.popup;
    widgets::popover_bg(out, r, 10.0 * s, s);
    let clip = r;
    let mut y = r[1] + pad;
    let mut index = 0;
    let mut hits = Vec::new();
    for it in &items {
        match it {
            Item::Header(label) => {
                out.text(Text::new(*label, r[0] + 14.0 * s, y + 9.0 * s, 11.5 * s).weight(600).color(theme::text_faint()).clip(clip));
                y += head_h;
            }
            Item::Hint => {
                let px = widgets::even_with(28.0 * s, 16.0 * s);
                out.icon_styled("sparkles", [(r[0] + 24.0 * s).round(), (y + 20.0 * s).round()], px, rgb(p.accent, 1.0), IconStyle::weight(ICON_WEIGHT));
                let t = "Search for what's in your photos, like “sunset at the beach”, or for people, places and albums.";
                let fs = 12.5 * s;
                for (k, line) in widgets::wrap(&mut renderer.ui, t, fs, 400, r[2] - 58.0 * s).into_iter().enumerate() {
                    out.text(Text::new(line, r[0] + 44.0 * s, (y + 10.0 * s + k as f32 * fs * 1.3).round(), fs).color(theme::text_dim()).clip(clip));
                }
                y += hint_h;
            }
            Item::Row(row) => {
                let rr = [r[0] + pad, y, r[2] - 2.0 * pad, row_h];
                let sel = se.sel == Some(index);
                if sel {
                    out.quad(Quad::solid(rr, rgb(p.accent, 1.0), 7.0 * s));
                }
                let fg = if sel { theme::text_rgba(p.on_accent, 1.0) } else { theme::text() };
                let faint = if sel { theme::text_rgba(p.on_accent, 0.75) } else { theme::text_faint() };
                let tint = if sel { p.on_accent } else { p.accent };
                let mid = y + row_h / 2.0;
                let ix = (rr[0] + 18.0 * s).round();
                let tx = rr[0] + 38.0 * s;
                let fs = 13.0 * s;
                let ty = (mid - fs * 0.625).round();
                let icon = |out: &mut Layer, name: &'static str, color: u32| {
                    let px = widgets::even_with(row_h.round(), 15.0 * s);
                    out.icon_styled(name, [ix, mid.round()], px, rgb(color, 1.0), IconStyle::weight(ICON_WEIGHT));
                };
                let thumb = |out: &mut Layer, key: String, round: bool| {
                    let d = (22.0 * s).round();
                    let tr = [ix - d / 2.0, (mid - d / 2.0).round(), d, d];
                    let radius = if round { d / 2.0 } else { 5.0 * s };
                    match sidebar.image(&key) {
                        Some(img) => out.quad(Quad::image(tr, img, [0.0, 0.0, 1.0, 1.0], 1.0, radius)),
                        None => out.quad(Quad::solid(tr, rgb(p.shade(p.raised, 0.12), 1.0), radius)),
                    }
                };
                let mut right_w = 0.0;
                let mut detail = |out: &mut Layer, uir: &mut crate::ui_render::UiRenderer, t: String| {
                    let ds = 12.0 * s;
                    right_w = uir.measure(&t, ds, 500, Font::Sans) + 12.0 * s;
                    out.text(Text::new(t, rr[0] + rr[2] - 10.0 * s, (mid - ds * 0.625).round(), ds).weight(500).color(faint).align(Align::Right).clip(clip));
                };
                let label = match row {
                    Row::Search(t) => {
                        icon(out, "search", tint);
                        let lead = "Search for ";
                        let lw = renderer.ui.measure(lead, fs, 400, Font::Sans);
                        out.text(Text::new(lead, tx, ty, fs).color(faint).clip(clip));
                        let max = rr[0] + rr[2] - 10.0 * s - (tx + lw);
                        let q = widgets::ellipsize(&mut renderer.ui, &format!("“{t}”"), fs, 600, max);
                        out.text(Text::new(q, tx + lw, ty, fs).weight(600).color(fg).clip(clip));
                        None
                    }
                    Row::Recent(t) => {
                        icon(out, "history", if sel { p.on_accent } else { p.muted });
                        if sel {
                            let d = (18.0 * s).round();
                            let c = [(rr[0] + rr[2] - 8.0 * s - d / 2.0).round(), mid.round()];
                            let px = widgets::even_with(d, 12.0 * s);
                            out.icon_styled("x", c, px, rgb(p.on_accent, 0.9), IconStyle::weight(2.4));
                            hits.push((Hit::Forget(index), [c[0] - d / 2.0 - 4.0 * s, rr[1], d + 8.0 * s, rr[3]]));
                            right_w = d + 12.0 * s;
                        }
                        Some(t.clone())
                    }
                    Row::Person { id, name } => {
                        thumb(out, format!("person:{id}"), true);
                        detail(out, &mut renderer.ui, "Person".into());
                        Some(name.clone())
                    }
                    Row::Place(name, kind) => {
                        icon(out, "map-pin", tint);
                        detail(out, &mut renderer.ui, if *kind == PlaceKind::City { "City" } else { "Country" }.into());
                        Some(name.clone())
                    }
                    Row::Album { id, name, count } => {
                        thumb(out, format!("album:{id}"), false);
                        detail(out, &mut renderer.ui, group(*count));
                        Some(name.clone())
                    }
                };
                if let Some(l) = label {
                    let max = rr[0] + rr[2] - 10.0 * s - right_w - tx;
                    let l = widgets::ellipsize(&mut renderer.ui, &l, fs, 500, max.max(10.0));
                    out.text(Text::new(l, tx, ty, fs).weight(500).color(fg).clip(clip));
                }
                hits.push((Hit::Row(index), rr));
                index += 1;
                y += row_h;
            }
        }
    }
    // Rows sit over the panel; popover hits come before the field's and the chips'.
    hits.push((Hit::Panel, r));
    hits.append(&mut se.hits);
    se.hits = hits;
}

// ---------------------------------------------------------------------------------------------
// Recent searches: `~/.config/lumen/state.json`, written atomically.

fn state_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("lumen/state.json"))
}

fn load_recent() -> Vec<String> {
    let Some(v) = state_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
        return Vec::new();
    };
    v["recent_searches"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).take(RECENT_MAX).collect()).unwrap_or_default()
}

fn save_recent(recent: &[String]) {
    // Headless runs leave the user's files alone.
    if crate::settings::READ_ONLY.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Some(path) = state_path() else { return };
    let mut v = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(|v| v.is_object()).unwrap_or(json!({}));
    v["recent_searches"] = json!(recent);
    let write = || -> Result<()> {
        let dir = path.parent().context("no config dir")?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".state.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(&v)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    if let Err(e) = write() {
        log::warn!("saving recent searches: {e:#}");
    }
}

// ---------------------------------------------------------------------------------------------
// Headless

/// Screenshot flags: `--search <query>` runs a search and waits for its results and thumbnails;
/// `--search-type <text>` focuses the field and types, showing the suggestions without
/// submitting (`--search-down N` moves the highlight, `--search-enter` then picks it, as the
/// keys would); `--search-focus` shows the empty
/// popover (recent searches; seed them with `--search-recent "a|b"`); `--search-filter
/// videos|photos|favorites|year|5years` (comma-separated) sets the chips; `--search-scroll-end`
/// scrolls to the end to page in more; `--search-esc` then presses Esc; `--search-delay <ms>` holds every request that long
/// (a cold server) and `--search-wait <ms>` renders that long after submitting without
/// waiting; `--search-fail` makes requests fail.
#[derive(Default)]
pub struct Shot {
    query: Option<String>,
    typed: Option<String>,
    down: usize,
    enter: bool,
    esc: bool,
    focus: bool,
    recent: Option<Vec<String>>,
    filters: Filters,
    scroll_end: bool,
    delay: Option<u64>,
    wait: Option<u64>,
    fail: bool,
}

impl Shot {
    pub fn parse(args: &[String]) -> Option<Self> {
        let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        let has = |name: &str| args.iter().any(|a| a == name);
        let mut filters = Filters::default();
        for f in flag("--search-filter").iter().flat_map(|f| f.split(',')) {
            match f.trim() {
                "videos" => filters.kind = Kind::Videos,
                "photos" => filters.kind = Kind::Photos,
                "favorites" => filters.favorites = true,
                "year" => filters.when = When::Year,
                "5years" => filters.when = When::FiveYears,
                other => log::warn!("--search-filter: unknown filter {other}"),
            }
        }
        let shot = Self {
            query: flag("--search"),
            typed: flag("--search-type"),
            down: flag("--search-down").and_then(|s| s.parse().ok()).unwrap_or(0),
            enter: has("--search-enter"),
            esc: has("--search-esc"),
            focus: has("--search-focus"),
            recent: flag("--search-recent").map(|r| r.split('|').map(String::from).filter(|s| !s.is_empty()).collect()),
            filters,
            scroll_end: has("--search-scroll-end"),
            delay: flag("--search-delay").and_then(|s| s.parse().ok()),
            wait: flag("--search-wait").and_then(|s| s.parse().ok()),
            fail: has("--search-fail"),
        };
        (shot.query.is_some() || shot.typed.is_some() || shot.focus).then_some(shot)
    }
}

pub fn drive(core: &mut Core, shot: &Shot, settle: &dyn Fn(&mut Core) -> Result<()>) -> Result<()> {
    if let Some(r) = &shot.recent {
        core.search.recent = r.clone();
    }
    core.search.delay = shot.delay.map(Duration::from_millis);
    core.search.fail = shot.fail;
    if let Some(q) = &shot.query {
        let start = Instant::now();
        core.search.field.set(q);
        run(core, Query::Smart(q.clone()), shot.filters);
        if let Some(ms) = shot.wait {
            while start.elapsed() < Duration::from_millis(ms) {
                std::thread::sleep(Duration::from_millis(15));
                core.pump();
                core.update(Instant::now());
            }
            core.search.hold = true;
            println!("search: rendering while {}", header(core).map_or("-".into(), |h| h.1));
            return Ok(());
        }
        settle(core)?;
        println!("search {q:?}: {} after {:.2} s", header(core).map_or("-".into(), |h| h.1), start.elapsed().as_secs_f32());
        if shot.scroll_end {
            for _ in 0..3 {
                core.layout.scroll_to_fraction(1.0);
                settle(core)?;
            }
            println!("search {q:?} after scrolling to the end: {}", header(core).map_or("-".into(), |h| h.1));
        }
        if shot.esc {
            // As Esc would from the grid.
            escape(core);
            settle(core)?;
            println!("search: Esc went back to {} ({})", core.collection.title, core.sidebar.selected());
        }
    }
    if let Some(t) = &shot.typed {
        focus(core, false);
        core.search.field.set(t);
        core.search.sel = (!t.trim().is_empty()).then_some(0);
        settle(core)?;
        let n = rows(core).len();
        for _ in 0..shot.down.min(n.saturating_sub(1)) {
            let se = &mut core.search;
            se.sel = Some(se.sel.map_or(0, |i| (i + 1).min(n - 1)));
        }
        core.update(Instant::now());
        println!("search suggestions for {t:?}: {:?}", rows(core));
        if shot.enter {
            let row = core.search.sel.and_then(|i| rows(core).into_iter().nth(i));
            println!("search: choosing {row:?}");
            if let Some(r) = row {
                activate_row(core, r);
            }
            settle(core)?;
            println!("search: now showing {} ({})", core.collection.title, header(core).map_or("-".into(), |h| h.1));
        }
    } else if shot.focus {
        focus(core, false);
        settle(core)?;
    }
    Ok(())
}
