//! Photo actions, Apple-Photos style: favorite, archive, add to album, move to Recently Deleted
//! and restore, from the viewer's toolbar, the grid's multi-selection bar, right-click menus and
//! the keyboard.
//!
//! Changes are optimistic: the grid updates at once (leaving tiles dim away, then the gap closes
//! via `Library::edit`, keeping thumbnails and scroll position), a toast offers Undo, and the
//! server call runs in order on the loader's runtime. A failure puts everything back. Once the
//! server confirms, the cached timelines are patched so revisits and live refreshes agree.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::Core;
use crate::grid::Spec;
use crate::library::{Asset, Library};
use crate::loader::{Collection, Patch, Source, group};
use crate::render::{LEAVING_SHIFT, SELECTED_BIT};
use crate::icons::IconStyle;
use crate::ui::{Align, Font, Layer, Quad, Text, contains, rgb, theme};
use crate::widgets::{self, ICON_WEIGHT, ROUND, ROUND_PITCH, TOOLBAR_Y, round_button};

/// How long removed tiles take to dim away before the grid closes the gap.
const LEAVE_TIME: f32 = 0.18;
const TOAST_TIME: Duration = Duration::from_secs(5);
const TOAST_FADE: f32 = 0.2;
const SELECTED: u8 = 1;
const LEAVING: u8 = 2;
const ICON: f32 = 16.0;

/// What an asset looks like to the collections that matter here.
#[derive(Clone, Copy, PartialEq, Debug)]
struct State {
    favorite: bool,
    archived: bool,
    trashed: bool,
}

/// A change sent to the server.
#[derive(Clone, PartialEq, Debug)]
pub enum Kind {
    Favorite(bool),
    Archive(bool),
    Trash,
    Restore,
    Album { id: String, name: String },
    NewAlbum(String),
    /// Only as the undo of `Album`.
    RemoveFromAlbum { id: String, name: String },
    /// Only as the undo of `NewAlbum`.
    DeleteAlbum { id: String, name: String },
}

impl Kind {
    fn apply(&self, s: State) -> State {
        match self {
            Kind::Favorite(f) => State { favorite: *f, ..s },
            Kind::Archive(a) => State { archived: *a, ..s },
            Kind::Trash => State { trashed: true, ..s },
            Kind::Restore => State { trashed: false, ..s },
            _ => s,
        }
    }

    /// Album edits get their toast once the server says what actually changed.
    fn toast_on_reply(&self) -> bool {
        matches!(self, Kind::Album { .. } | Kind::NewAlbum(_) | Kind::RemoveFromAlbum { .. } | Kind::DeleteAlbum { .. })
    }

    fn failure(&self) -> &'static str {
        match self {
            Kind::Favorite(_) => "Couldn't update Favorites",
            Kind::Archive(true) => "Couldn't archive",
            Kind::Archive(false) => "Couldn't unarchive",
            Kind::Trash => "Couldn't move to Recently Deleted",
            Kind::Restore => "Couldn't restore",
            Kind::Album { .. } => "Couldn't add to the album",
            Kind::NewAlbum(_) => "Couldn't create the album",
            Kind::RemoveFromAlbum { .. } => "Couldn't remove from the album",
            Kind::DeleteAlbum { .. } => "Couldn't delete the album",
        }
    }
}

/// Whether an asset in state `s` belongs in collection `key`; None when that takes more than
/// these flags to know (albums, people).
fn member(key: &str, s: State) -> Option<bool> {
    let timeline = !s.trashed && !s.archived;
    match key {
        "trash" => Some(s.trashed),
        "archive" => Some(!s.trashed && s.archived),
        "favorites" => Some(timeline && s.favorite),
        "library" | "videos" => Some(timeline),
        k if k.starts_with("place:") => Some(timeline),
        _ if s.trashed => Some(false),
        _ => None,
    }
}

/// A user-facing command; resolved against its targets when run.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Cmd {
    Open,
    Favorite(bool),
    Archive(bool),
    Album,
    Trash,
    Restore,
    Clear,
}

impl Cmd {
    fn icon(self) -> &'static str {
        match self {
            Cmd::Open => "maximize-2",
            Cmd::Favorite(_) => "heart",
            Cmd::Archive(true) => "archive",
            Cmd::Archive(false) => "archive-restore",
            Cmd::Album => "image-plus",
            Cmd::Trash => "trash-2",
            Cmd::Restore => "rotate-ccw",
            Cmd::Clear => "x",
        }
    }

    /// Icon, style and tint; `favorite` says whether the targets are already favorites
    /// (a filled red heart, like Photos).
    fn look(self, favorite: bool) -> (&'static str, IconStyle, Option<u32>) {
        match self {
            Cmd::Favorite(_) if favorite => ("heart", IconStyle::filled(), Some(theme::palette().red)),
            c => (c.icon(), IconStyle::weight(ICON_WEIGHT), None),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Cmd::Open => "Open",
            Cmd::Favorite(true) => "Favorite",
            Cmd::Favorite(false) => "Unfavorite",
            Cmd::Archive(true) => "Archive",
            Cmd::Archive(false) => "Unarchive",
            Cmd::Album => "Add to Album…",
            Cmd::Trash => "Move to Trash",
            Cmd::Restore => "Restore",
            Cmd::Clear => "",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    /// Viewer top bar, acting on the photo on screen.
    Toolbar(Cmd),
    /// Selection bar, acting on the selection.
    Bar(Cmd),
    Undo,
    Item(usize),
    Row(usize),
    Cancel,
    /// Swallows clicks on a popover's background.
    Panel,
}

struct Op {
    kind: Kind,
    /// Copies as they'll be afterwards (favorite flag updated), and each one's state before/after.
    assets: Vec<Asset>,
    before: Vec<State>,
    after: Vec<State>,
    undoable: bool,
}

#[derive(Default)]
struct Reply {
    /// Assets the server actually changed (adding to an album skips ones already in it).
    added: Vec<String>,
    album: Option<String>,
}

struct Done {
    op: u64,
    result: Result<Reply, String>,
}

struct Undo {
    kind: Kind,
    assets: Vec<Asset>,
    states: Vec<State>,
}

struct Toast {
    text: String,
    error: bool,
    undo: Option<Undo>,
    at: Instant,
}

struct Menu {
    at: (f32, f32),
    /// None draws a separator.
    items: Vec<Option<Cmd>>,
    targets: Vec<Asset>,
}

struct Picker {
    targets: Vec<Asset>,
    text: String,
    /// Logical pixels.
    scroll: f32,
    /// Highlighted row: 0 is "New Album…", then the albums matching `text`.
    sel: Option<usize>,
}

type Job = (u64, Kind, Vec<String>);

pub struct Actions {
    cursor: (f32, f32),
    /// Per-tile SELECTED/LEAVING marks, indexed like `lib`, the library they were made for.
    lib: Option<Arc<Library>>,
    key: String,
    marks: Vec<u8>,
    selected: usize,
    anchor: Option<usize>,
    leaving: Option<Instant>,
    /// Favorite flags changed this session, ahead of what the timeline data says.
    favorites: HashMap<String, bool>,
    menu: Option<Menu>,
    picker: Option<Picker>,
    toast: Option<Toast>,
    ops: HashMap<u64, Op>,
    next_op: u64,
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
    rx: Receiver<Done>,
    /// Clickable rects from the last frame, topmost first.
    hits: Vec<(Hit, [f32; 4])>,
    armed: Option<Hit>,
    /// The press that started this click was ours; so is its release.
    swallow: bool,
    /// Headless: toasts stay up and nothing fades.
    pub hold: bool,
}

impl Actions {
    pub fn new(loader: &crate::loader::Loader) -> Self {
        let (tx, rx) = channel();
        let (jobs, queue) = tokio::sync::mpsc::unbounded_channel();
        loader.handle().spawn(worker(loader.source().clone(), queue, tx, loader.waker()));
        Self {
            cursor: (-1.0, -1.0),
            lib: None,
            key: String::new(),
            marks: Vec::new(),
            selected: 0,
            anchor: None,
            leaving: None,
            favorites: HashMap::new(),
            menu: None,
            picker: None,
            toast: None,
            ops: HashMap::new(),
            next_op: 1,
            jobs,
            rx,
            hits: Vec::new(),
            armed: None,
            swallow: false,
            hold: false,
        }
    }

    /// Grid instance flags for tile `i`.
    pub fn flags(&self, i: usize, now: Instant) -> u32 {
        let m = self.marks.get(i).copied().unwrap_or(0);
        let mut f = if m & SELECTED != 0 { SELECTED_BIT } else { 0 };
        if m & LEAVING != 0 {
            let t = self.leaving.map_or(1.0, |t| (now.duration_since(t).as_secs_f32() / LEAVE_TIME).min(1.0));
            f |= ((t * 255.0) as u32) << LEAVING_SHIFT;
        }
        f
    }

    /// No request is waiting for the server.
    pub fn is_idle(&self) -> bool {
        self.ops.is_empty() && self.leaving.is_none()
    }

    pub fn toast_text(&self) -> Option<&str> {
        self.toast.as_ref().map(|t| t.text.as_str())
    }

    fn favorite(&self, a: &Asset) -> bool {
        self.favorites.get(&a.id).copied().unwrap_or(a.favorite)
    }

    fn clear_selection(&mut self) {
        for m in &mut self.marks {
            *m &= !SELECTED;
        }
        self.selected = 0;
        self.anchor = None;
    }

    fn set_selected(&mut self, i: usize, on: bool) {
        let Some(m) = self.marks.get_mut(i) else { return };
        if (*m & SELECTED != 0) != on {
            *m ^= SELECTED;
            if on { self.selected += 1 } else { self.selected -= 1 }
        }
    }

    fn is_selected(&self, i: usize) -> bool {
        self.marks.get(i).is_some_and(|m| m & SELECTED != 0)
    }

    fn selection(&self) -> Vec<Asset> {
        let Some(lib) = &self.lib else { return Vec::new() };
        self.marks.iter().enumerate().filter(|(_, m)| **m & SELECTED != 0).map(|(i, _)| lib.assets[i].clone()).collect()
    }

    fn toast(&mut self, text: String, error: bool, undo: Option<Undo>) {
        self.toast = Some(Toast { text, error, undo, at: Instant::now() });
    }
}

// ---------------------------------------------------------------------------------------------
// Library edits

/// Keeps the marks in step with the grid's library: a refresh of the same collection keeps
/// them by asset id (see `refresh`), anything else starts over.
fn sync_marks(core: &mut Core) {
    let a = &mut core.actions;
    let same = match (&a.lib, &core.library) {
        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
        (None, None) => true,
        _ => false,
    };
    if same && a.key == core.collection.key {
        return;
    }
    a.lib = core.library.clone();
    a.key = core.collection.key.clone();
    a.marks = vec![0; a.lib.as_ref().map_or(0, |l| l.len())];
    (a.selected, a.anchor, a.leaving, a.menu) = (0, None, None, None);
}

/// A sync brought a newer copy of the collection on screen: swap it in without dropping
/// thumbnails, the selection or the scroll position, so a refresh that matches what we already
/// show (e.g. after our own change) changes nothing visible.
pub fn refresh(core: &mut Core, library: Arc<Library>, generation: u64) {
    let Some(old) = core.library.clone() else {
        core.renderer.set_library(&library);
        core.layout.reset(library.len(), false);
        (core.library, core.generation) = (Some(library), generation);
        return;
    };
    let at: HashMap<&str, usize> = library.assets.iter().enumerate().map(|(i, a)| (a.id.as_str(), i)).collect();
    let map: Vec<Option<usize>> = old.assets.iter().map(|a| at.get(a.id.as_str()).copied()).collect();
    drop(at);
    swap_library(core, library, map, Some(generation));
}

/// Shows `new` in place of the current library (same collection); `map` says where each old
/// index went. Keeps the first visible tile where it is on screen.
fn swap_library(core: &mut Core, new: Arc<Library>, map: Vec<Option<usize>>, generation: Option<u64>) {
    sync_marks(core);
    let vp = core.layout.vp;
    let (t, cols) = (core.layout.target.tile(&vp), core.layout.target.cols as usize);
    let anchor = core.layout.index_at(vp.width / 2.0, vp.top).map(|(i, _, _)| {
        let y = (i / cols) as f32 * t - core.layout.target.scroll;
        ((i..map.len()).chain((0..i).rev()).find_map(|k| map[k]), y)
    });
    core.renderer.remap_library(&new, &map);
    core.generation = generation.unwrap_or_else(|| core.loader.adopt(new.clone()));
    let g = core.generation;
    core.pending.retain_mut(|th| match map.get(th.index).copied().flatten() {
        Some(j) => {
            (th.index, th.generation) = (j, g);
            true
        }
        None => false,
    });
    core.layout.reset(new.len(), true);
    if let Some((Some(j), y)) = anchor {
        let (t, cols) = (core.layout.target.tile(&vp), core.layout.target.cols as usize);
        core.layout.target.scroll = (j / cols) as f32 * t - y;
        core.layout.target.clamp(new.len(), &vp);
    }

    let a = &mut core.actions;
    let mut marks = vec![0u8; new.len()];
    for (i, &m) in a.marks.iter().enumerate() {
        if m != 0
            && let Some(j) = map.get(i).copied().flatten()
        {
            marks[j] = m;
        }
    }
    a.selected = marks.iter().filter(|m| **m & SELECTED != 0).count();
    a.anchor = a.anchor.and_then(|i| map.get(i).copied().flatten());
    if !marks.iter().any(|m| m & LEAVING != 0) {
        a.leaving = None;
    }
    a.marks = marks;
    a.lib = Some(new.clone());
    core.library = Some(new);
}

/// Removes and adds assets in the grid's current library.
fn edit_grid(core: &mut Core, remove: &HashSet<String>, add: &[Asset]) {
    let Some(lib) = core.library.clone() else { return };
    let have: HashSet<&str> = lib.assets.iter().map(|a| a.id.as_str()).collect();
    let add: Vec<Asset> = add.iter().filter(|a| !have.contains(a.id.as_str()) && core.collection.admits(a)).cloned().collect();
    if add.is_empty() && !remove.iter().any(|id| have.contains(id.as_str())) {
        return;
    }
    drop(have);
    let (new, map) = lib.edit(remove, &add);
    swap_library(core, Arc::new(new), map, None);
}

/// Tiles marked as leaving have finished dimming (or nobody is watching): close the gap.
fn commit_leaving(core: &mut Core) {
    let Some(lib) = core.actions.lib.clone() else { return };
    let remove: HashSet<String> =
        core.actions.marks.iter().enumerate().filter(|(_, m)| **m & LEAVING != 0).map(|(i, _)| lib.assets[i].id.clone()).collect();
    core.actions.leaving = None;
    edit_grid(core, &remove, &[]);
}

/// Moves `assets` from states `from` to `to` on screen: favorite flags, and membership of the
/// collection being shown.
fn apply_local(core: &mut Core, assets: &[Asset], to: &[State]) {
    sync_marks(core);
    let key = core.collection.key.clone();
    let mut remove = HashSet::new();
    let mut add = Vec::new();
    for (a, s) in assets.iter().zip(to) {
        core.actions.favorites.insert(a.id.clone(), s.favorite);
        match member(&key, *s) {
            Some(false) => _ = remove.insert(a.id.clone()),
            Some(true) => add.push(Asset { favorite: s.favorite, ..a.clone() }),
            None => {}
        }
    }
    if !add.is_empty() {
        edit_grid(core, &HashSet::new(), &add);
    }
    if remove.is_empty() {
        return;
    }
    let a = &mut core.actions;
    let Some(lib) = a.lib.clone() else { return };
    let mut any = false;
    for (i, asset) in lib.assets.iter().enumerate() {
        if remove.contains(&asset.id) {
            if a.marks[i] & SELECTED != 0 {
                a.selected -= 1;
            }
            a.marks[i] = LEAVING;
            any = true;
        }
    }
    if any {
        a.leaving = Some(Instant::now());
        // In the viewer the grid is hidden: no point dimming it first.
        if core.viewer.is_open() {
            commit_leaving(core);
        }
    }
}

fn state_of(core: &Core, a: &Asset) -> State {
    let key = core.collection.key.as_str();
    State { favorite: core.actions.favorite(a), archived: key == "archive", trashed: key == "trash" }
}

/// Changes `assets` locally and asks the server to do the same. `before` defaults to what the
/// current collection implies (undo passes the states it recorded).
fn perform(core: &mut Core, kind: Kind, assets: Vec<Asset>, before: Option<Vec<State>>, undoable: bool) {
    if assets.is_empty() {
        return;
    }
    let before = before.unwrap_or_else(|| assets.iter().map(|a| state_of(core, a)).collect());
    let after: Vec<State> = before.iter().map(|s| kind.apply(*s)).collect();
    let assets: Vec<Asset> = assets.into_iter().zip(&after).map(|(a, s)| Asset { favorite: s.favorite, ..a }).collect();
    apply_local(core, &assets, &after);
    let id = core.actions.next_op;
    core.actions.next_op += 1;
    let op = Op { kind: kind.clone(), assets, before, after, undoable };
    if !kind.toast_on_reply() {
        let undo = if undoable { undo_of(&op, &Reply::default()) } else { None };
        core.actions.toast(message(&op, None), false, undo);
    }
    let ids = op.assets.iter().map(|a| a.id.clone()).collect();
    core.actions.ops.insert(id, op);
    let _ = core.actions.jobs.send((id, kind, ids));
}

/// The server answered: patch caches and toast, or put things back.
fn finish(core: &mut Core, done: Done) {
    let Some(op) = core.actions.ops.remove(&done.op) else { return };
    match done.result {
        Ok(reply) => {
            if op.kind.toast_on_reply() {
                let undo = if op.undoable { undo_of(&op, &reply) } else { None };
                core.actions.toast(message(&op, Some(&reply)), false, undo);
            }
            patch_caches(core, &op, &reply);
            // Album counts and covers change with membership and trash.
            if !matches!(op.kind, Kind::Favorite(_) | Kind::Archive(_)) {
                core.sidebar.refresh_albums(&core.loader);
            }
        }
        Err(e) => {
            log::error!("{}: {e}", op.kind.failure());
            let assets: Vec<Asset> = op.assets.iter().zip(&op.before).map(|(a, s)| Asset { favorite: s.favorite, ..a.clone() }).collect();
            apply_local(core, &assets, &op.before);
            let short: String = e.chars().take(90).collect();
            core.actions.toast(format!("{} · {short}", op.kind.failure()), true, None);
        }
    }
}

/// Brings every cached timeline in line with a confirmed change.
fn patch_caches(core: &Core, op: &Op, reply: &Reply) {
    let known: Vec<(String, &'static str)> = [
        (Collection::library(), "library"),
        (Collection::favorites(), "favorites"),
        (Collection::archive(), "archive"),
        (Collection::trash(), "trash"),
    ]
    .into_iter()
    .map(|(c, k)| (c.query, k))
    .collect();
    let changes: HashMap<String, (Asset, State)> =
        op.assets.iter().zip(&op.after).map(|(a, s)| (a.id.clone(), (a.clone(), *s))).collect();
    let moves = matches!(op.kind, Kind::Archive(_) | Kind::Trash | Kind::Restore);
    let album = match &op.kind {
        Kind::Album { id, .. } | Kind::RemoveFromAlbum { id, .. } => Some(format!("albumId={id}")),
        Kind::NewAlbum(_) => reply.album.as_ref().map(|id| format!("albumId={id}")),
        _ => None,
    };
    core.loader.patch_caches(move |query, assets| {
        let Some(&(_, key)) = known.iter().find(|(q, _)| q == query) else {
            return if moves || album.as_deref() == Some(query) { Patch::Forget } else { Patch::Keep };
        };
        let mut changed = false;
        let mut out: Vec<Asset> = Vec::with_capacity(assets.len());
        let mut present = HashSet::new();
        for a in assets {
            match changes.get(&a.id) {
                Some((_, s)) if member(key, *s) == Some(false) => changed = true,
                Some((_, s)) => {
                    present.insert(a.id.as_str());
                    changed |= a.favorite != s.favorite;
                    out.push(Asset { favorite: s.favorite, ..a.clone() });
                }
                None => out.push(a.clone()),
            }
        }
        for (id, (a, s)) in &changes {
            if member(key, *s) == Some(true) && !present.contains(id.as_str()) {
                out.push(a.clone());
                changed = true;
            }
        }
        if changed { Patch::Replace(out) } else { Patch::Keep }
    });
}

fn undo_of(op: &Op, reply: &Reply) -> Option<Undo> {
    let pick = |keep: &dyn Fn(usize) -> bool| -> (Vec<Asset>, Vec<State>) {
        (0..op.assets.len()).filter(|&i| keep(i)).map(|i| (op.assets[i].clone(), op.after[i])).unzip()
    };
    let (kind, (assets, states)) = match &op.kind {
        Kind::Favorite(v) => (Kind::Favorite(!v), pick(&|i| op.before[i].favorite != *v)),
        Kind::Archive(v) => (Kind::Archive(!v), pick(&|i| op.before[i].archived != *v)),
        Kind::Trash => (Kind::Restore, pick(&|i| !op.before[i].trashed)),
        Kind::Restore => (Kind::Trash, pick(&|i| op.before[i].trashed)),
        Kind::Album { id, name } => {
            let added: HashSet<&str> = reply.added.iter().map(|s| s.as_str()).collect();
            (Kind::RemoveFromAlbum { id: id.clone(), name: name.clone() }, pick(&|i| added.contains(op.assets[i].id.as_str())))
        }
        Kind::NewAlbum(name) => (Kind::DeleteAlbum { id: reply.album.clone()?, name: name.clone() }, pick(&|_| true)),
        _ => return None,
    };
    (!assets.is_empty()).then_some(Undo { kind, assets, states })
}

fn undo(core: &mut Core) {
    let Some(u) = core.actions.toast.as_mut().and_then(|t| t.undo.take()) else { return };
    perform(core, u.kind, u.assets, Some(u.states), false);
}

/// "1 photo", "3 videos", "12 items".
fn noun<'a>(assets: impl Iterator<Item = &'a Asset>) -> String {
    let (n, videos) = assets.fold((0, 0), |(n, v), a| (n + 1, v + a.video as usize));
    let what = if videos == n { "video" } else if videos == 0 { "photo" } else { "item" };
    format!("{} {what}{}", group(n), if n == 1 { "" } else { "s" })
}

fn message(op: &Op, reply: Option<&Reply>) -> String {
    let many = op.assets.len() > 1;
    let all = noun(op.assets.iter());
    let one_or = |one: &str, many_fmt: String| if many { many_fmt } else { one.to_string() };
    match &op.kind {
        Kind::Favorite(true) => one_or("Added to Favorites", format!("Added {all} to Favorites")),
        Kind::Favorite(false) => one_or("Removed from Favorites", format!("Removed {all} from Favorites")),
        Kind::Archive(true) => one_or("Archived", format!("Archived {all}")),
        Kind::Archive(false) => one_or("Unarchived", format!("Unarchived {all}")),
        Kind::Trash => one_or("Moved to Recently Deleted", format!("Moved {all} to Recently Deleted")),
        Kind::Restore => one_or("Restored", format!("Restored {all}")),
        Kind::Album { name, .. } => {
            let added: HashSet<&str> = reply.map(|r| r.added.iter().map(|s| s.as_str()).collect()).unwrap_or_default();
            match op.assets.iter().filter(|a| added.contains(a.id.as_str())).count() {
                0 => format!("Already in “{name}”"),
                _ => format!("Added {} to “{name}”", noun(op.assets.iter().filter(|a| added.contains(a.id.as_str())))),
            }
        }
        Kind::NewAlbum(name) => format!("Created “{name}” with {all}"),
        Kind::RemoveFromAlbum { name, .. } => format!("Removed {all} from “{name}”"),
        Kind::DeleteAlbum { name, .. } => format!("Deleted album “{name}”"),
    }
}

/// Runs requests one at a time, in order, so an undo can never overtake what it undoes.
async fn worker(
    source: Source,
    mut jobs: tokio::sync::mpsc::UnboundedReceiver<Job>,
    tx: Sender<Done>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    while let Some((op, kind, ids)) = jobs.recv().await {
        let result = request(&source, &kind, &ids).await.map_err(|e| format!("{e:#}"));
        if let Err(e) = &result {
            log::warn!("{kind:?}: {e}");
        }
        let _ = tx.send(Done { op, result });
        wake();
    }
}

async fn request(source: &Source, kind: &Kind, ids: &[String]) -> Result<Reply> {
    let Source::Immich(immich) = source else {
        // The demo has no server: pretend it took a moment and worked.
        tokio::time::sleep(Duration::from_millis(120)).await;
        return Ok(Reply { added: ids.to_vec(), album: Some("demo".into()) });
    };
    let reply = |album: Option<String>| Ok(Reply { added: ids.to_vec(), album });
    match kind {
        Kind::Favorite(f) => immich.put_json("/assets", json!({ "ids": ids, "isFavorite": f })).await?,
        Kind::Archive(a) => {
            let visibility = if *a { "archive" } else { "timeline" };
            immich.put_json("/assets", json!({ "ids": ids, "visibility": visibility })).await?
        }
        // `force: false` moves to Recently Deleted; nothing here ever deletes permanently.
        Kind::Trash => immich.delete_json("/assets", json!({ "ids": ids, "force": false })).await?,
        Kind::Restore => immich.post_json("/trash/restore/assets", json!({ "ids": ids })).await?,
        Kind::Album { id, .. } => {
            let res = immich.put_json(&format!("/albums/{id}/assets"), json!({ "ids": ids })).await?;
            let added = res
                .as_array()
                .context("unexpected reply")?
                .iter()
                .filter(|r| r["success"].as_bool() == Some(true))
                .filter_map(|r| r["id"].as_str().map(String::from))
                .collect();
            return Ok(Reply { added, album: Some(id.clone()) });
        }
        Kind::NewAlbum(name) => {
            let res = immich.post_json("/albums", json!({ "albumName": name, "assetIds": ids })).await?;
            let id = res["id"].as_str().context("no album id in reply")?.to_string();
            return reply(Some(id));
        }
        Kind::RemoveFromAlbum { id, .. } => immich.delete_json(&format!("/albums/{id}/assets"), json!({ "ids": ids })).await?,
        // Deletes the album only, never its photos.
        Kind::DeleteAlbum { id, .. } => immich.delete_json(&format!("/albums/{id}"), serde_json::Value::Null).await?,
    };
    reply(None)
}

// ---------------------------------------------------------------------------------------------
// Commands

fn run(core: &mut Core, cmd: Cmd, targets: Vec<Asset>) {
    match cmd {
        Cmd::Open => {
            if let (Some(lib), Some(a)) = (core.library.clone(), targets.first())
                && let Some(i) = lib.assets.iter().position(|b| b.id == a.id)
            {
                core.viewer.open(lib, i, Instant::now());
            }
        }
        Cmd::Favorite(v) => perform(core, Kind::Favorite(v), targets, None, true),
        Cmd::Archive(v) => perform(core, Kind::Archive(v), targets, None, true),
        Cmd::Trash => perform(core, Kind::Trash, targets, None, true),
        Cmd::Restore => perform(core, Kind::Restore, targets, None, true),
        Cmd::Album if !targets.is_empty() => open_picker(core, targets),
        Cmd::Album => {}
        Cmd::Clear => core.actions.clear_selection(),
    }
}

fn in_trash(core: &Core) -> bool {
    core.collection.key == "trash"
}

fn favorite_cmd(core: &Core, targets: &[Asset]) -> Cmd {
    Cmd::Favorite(!targets.iter().all(|a| core.actions.favorite(a)))
}

/// What the toolbar, bar and menus offer for `targets` in the current collection.
fn commands(core: &Core, targets: &[Asset]) -> Vec<Cmd> {
    if in_trash(core) {
        return vec![Cmd::Restore];
    }
    vec![favorite_cmd(core, targets), Cmd::Archive(core.collection.key != "archive"), Cmd::Album, Cmd::Trash]
}

fn viewer_target(core: &Core) -> Vec<Asset> {
    core.viewer.current().map(|(lib, i)| vec![lib.assets[i].clone()]).unwrap_or_default()
}

fn open_menu(core: &mut Core, at: (f32, f32), targets: Vec<Asset>, open: bool) {
    let mut items: Vec<Option<Cmd>> = Vec::new();
    if open {
        items.extend([Some(Cmd::Open), None]);
    }
    let cmds = commands(core, &targets);
    for c in cmds {
        if c == Cmd::Trash {
            items.push(None);
        }
        items.push(Some(c));
    }
    core.actions.menu = Some(Menu { at, items, targets });
}

/// Right-click on grid tile `i`: acts on the selection if the tile is part of it.
fn open_tile_menu(core: &mut Core, i: usize, at: (f32, f32)) {
    sync_marks(core);
    let Some(lib) = core.library.clone() else { return };
    let targets = if core.actions.is_selected(i) { core.actions.selection() } else { vec![lib.assets[i].clone()] };
    let single = targets.len() == 1;
    open_menu(core, at, targets, single);
}

fn open_picker(core: &mut Core, targets: Vec<Asset>) {
    core.actions.menu = None;
    core.actions.picker = Some(Picker { targets, text: String::new(), scroll: 0.0, sel: None });
}

/// Albums matching the picker's text: (id, name, count).
fn picker_albums(core: &Core, text: &str) -> Vec<(String, String, usize)> {
    let t = text.trim().to_lowercase();
    core.sidebar.albums().into_iter().filter(|(_, name, _)| t.is_empty() || name.to_lowercase().contains(&t)).collect()
}

fn choose_row(core: &mut Core, row: usize) {
    let Some(p) = core.actions.picker.take() else { return };
    if row == 0 {
        let name = p.text.trim();
        let name = if name.is_empty() { "Untitled Album".to_string() } else { name.to_string() };
        perform(core, Kind::NewAlbum(name), p.targets, None, true);
    } else if let Some((id, name, _)) = picker_albums(core, &p.text).into_iter().nth(row - 1) {
        perform(core, Kind::Album { id, name }, p.targets, None, true);
    }
}

/// Grid tile under a physical point, if any.
fn tile_at(core: &Core, (x, y): (f32, f32)) -> Option<usize> {
    if core.library.is_none() || core.map.is_some() || core.viewer.is_open() || x < core.left {
        return None;
    }
    let (i, _, _) = core.layout.index_at(x - core.left, y)?;
    contains(Spec::Grid(core.layout.target).rect(i, &core.layout.vp), x - core.left, y).then_some(i)
}

/// Ctrl+click toggles, Shift+click extends from the last clicked tile.
fn select_click(core: &mut Core, i: usize, mods: ModifiersState) {
    sync_marks(core);
    let a = &mut core.actions;
    match a.anchor.filter(|_| mods.shift_key()) {
        Some(from) => {
            for k in from.min(i)..=from.max(i) {
                a.set_selected(k, true);
            }
        }
        None => {
            let on = !a.is_selected(i);
            a.set_selected(i, on);
        }
    }
    a.anchor = Some(i);
}

// ---------------------------------------------------------------------------------------------
// Input

/// Routes window input to popovers, the selection and shortcuts before anything else sees it.
/// Returns true if the event was consumed.
pub fn input(core: &mut Core, event: &WindowEvent, mods: ModifiersState) -> bool {
    let scale = core.scale;
    match event {
        WindowEvent::CursorMoved { position, .. } => {
            core.actions.cursor = (position.x as f32, position.y as f32);
            let c = core.actions.cursor;
            if let Some(Hit::Row(r)) = hit_at(core, c)
                && let Some(p) = &mut core.actions.picker
            {
                p.sel = Some(r);
            }
            false
        }
        WindowEvent::MouseWheel { delta, .. } if core.actions.picker.is_some() => {
            let p = core.actions.picker.as_mut().unwrap();
            p.scroll -= match delta {
                MouseScrollDelta::LineDelta(_, y) => y * 50.0,
                MouseScrollDelta::PixelDelta(d) => d.y as f32 / scale,
            };
            true
        }
        WindowEvent::MouseWheel { .. } if core.actions.menu.is_some() => {
            core.actions.menu = None;
            true
        }
        WindowEvent::MouseInput { state: ElementState::Pressed, button, .. } => {
            let c = core.actions.cursor;
            let hit = hit_at(core, c);
            core.actions.swallow = false;
            if let Some(h) = hit.filter(|_| *button == MouseButton::Left) {
                core.actions.armed = Some(h);
                return true;
            }
            // A click outside an open popover only dismisses it.
            if core.actions.picker.is_some() {
                if hit.is_none() {
                    core.actions.picker = None;
                }
                core.actions.swallow = true;
                return true;
            }
            if core.actions.menu.take().is_some() {
                core.actions.swallow = true;
                return true;
            }
            if core.sidebar.contains(c, scale) {
                return false;
            }
            match button {
                MouseButton::Left if mods.control_key() || mods.shift_key() => {
                    let Some(i) = tile_at(core, c) else { return false };
                    select_click(core, i, mods);
                    core.actions.swallow = true;
                    true
                }
                MouseButton::Right => {
                    if let Some(i) = tile_at(core, c) {
                        open_tile_menu(core, i, c);
                    } else if core.viewer.photo().is_some_and(|r| contains(r, c.0, c.1)) {
                        let t = viewer_target(core);
                        open_menu(core, c, t, false);
                    } else {
                        return false;
                    }
                    core.actions.swallow = true;
                    true
                }
                _ => false,
            }
        }
        WindowEvent::MouseInput { state: ElementState::Released, .. } => {
            if let Some(h) = core.actions.armed.take() {
                if hit_at(core, core.actions.cursor) == Some(h) {
                    activate(core, h);
                }
                return true;
            }
            std::mem::take(&mut core.actions.swallow)
        }
        WindowEvent::KeyboardInput { event, .. } => {
            if core.actions.picker.is_some() {
                if event.state == ElementState::Pressed {
                    picker_key(core, event, mods);
                }
                return true;
            }
            if event.state != ElementState::Pressed {
                return false;
            }
            if core.actions.menu.take().is_some() && event.logical_key == Key::Named(NamedKey::Escape) {
                return true;
            }
            shortcut(core, &event.logical_key, mods)
        }
        _ => false,
    }
}

fn hit_at(core: &Core, (x, y): (f32, f32)) -> Option<Hit> {
    core.actions.hits.iter().find(|(_, r)| contains(*r, x, y)).map(|h| h.0)
}

fn activate(core: &mut Core, hit: Hit) {
    match hit {
        Hit::Toolbar(cmd) => {
            let t = viewer_target(core);
            run(core, cmd, t);
        }
        Hit::Bar(cmd) => {
            let t = core.actions.selection();
            run(core, cmd, t);
        }
        Hit::Undo => undo(core),
        Hit::Item(i) => {
            if let Some(menu) = core.actions.menu.take()
                && let Some(Some(cmd)) = menu.items.get(i)
            {
                run(core, *cmd, menu.targets);
            }
        }
        Hit::Row(r) => choose_row(core, r),
        Hit::Cancel => core.actions.picker = None,
        Hit::Panel => {}
    }
}

/// Keyboard shortcuts, for the photo in the viewer or the grid's selection:
/// `.` favorite/unfavorite, Delete move to Recently Deleted, `a` or Ctrl+Shift+A add to album,
/// Ctrl+L archive/unarchive (Photos' "hide" key), `r` restore (in Recently Deleted), Ctrl+Z undo.
/// Grid only: Ctrl+A select all, Esc clear the selection.
fn shortcut(core: &mut Core, key: &Key, mods: ModifiersState) -> bool {
    let ch = match key {
        Key::Character(c) => c.to_lowercase(),
        _ => String::new(),
    };
    let ctrl = mods.control_key();
    if ctrl && ch == "z" {
        undo(core);
        return true;
    }
    let grid = !core.viewer.is_open() && core.map.is_none();
    if grid && core.library.is_some() {
        sync_marks(core);
        if ctrl && !mods.shift_key() && ch == "a" {
            let a = &mut core.actions;
            for k in 0..a.marks.len() {
                a.set_selected(k, true);
            }
            return true;
        }
        if *key == Key::Named(NamedKey::Escape) && core.actions.selected > 0 {
            core.actions.clear_selection();
            return true;
        }
    }
    let targets = if core.viewer.is_open() {
        viewer_target(core)
    } else if grid {
        core.actions.selection()
    } else {
        Vec::new()
    };
    if targets.is_empty() {
        return false;
    }
    let trash = in_trash(core);
    let cmd = match key {
        Key::Named(NamedKey::Delete) if !trash => Cmd::Trash,
        _ if trash && ch == "r" && !ctrl => Cmd::Restore,
        _ if trash => return false,
        _ if ch == "." && !ctrl => favorite_cmd(core, &targets),
        _ if ch == "a" && (!ctrl || mods.shift_key()) => Cmd::Album,
        _ if ch == "l" && ctrl => Cmd::Archive(core.collection.key != "archive"),
        _ => return false,
    };
    run(core, cmd, targets);
    true
}

fn picker_key(core: &mut Core, event: &winit::event::KeyEvent, mods: ModifiersState) {
    let rows = core.actions.picker.as_ref().map_or(0, |p| picker_albums(core, &p.text).len() + 1);
    let Some(p) = &mut core.actions.picker else { return };
    match &event.logical_key {
        Key::Named(NamedKey::Escape) => core.actions.picker = None,
        Key::Named(NamedKey::Enter) => {
            if let Some(r) = p.sel {
                choose_row(core, r);
            }
        }
        Key::Named(NamedKey::ArrowDown) => p.sel = Some(p.sel.map_or(0, |r| (r + 1).min(rows - 1))),
        Key::Named(NamedKey::ArrowUp) => p.sel = Some(p.sel.map_or(0, |r| r.saturating_sub(1))),
        Key::Named(NamedKey::Backspace) => {
            p.text.pop();
            retarget(core);
        }
        _ if !mods.control_key() => {
            if let Some(t) = &event.text {
                let t: String = t.chars().filter(|c| !c.is_control()).collect();
                if !t.is_empty() {
                    p.text.push_str(&t);
                    retarget(core);
                }
            }
        }
        _ => {}
    }
}

/// After typing: highlight the album named exactly that, else the first match, else "New Album".
fn retarget(core: &mut Core) {
    let Some(text) = core.actions.picker.as_ref().map(|p| p.text.clone()) else { return };
    let albums = picker_albums(core, &text);
    let t = text.trim().to_lowercase();
    let sel = if t.is_empty() {
        None
    } else {
        Some(albums.iter().position(|a| a.1.to_lowercase() == t).or((!albums.is_empty()).then_some(0)).map_or(0, |i| i + 1))
    };
    let p = core.actions.picker.as_mut().unwrap();
    (p.sel, p.scroll) = (sel, 0.0);
}

// ---------------------------------------------------------------------------------------------
// Per frame

/// Before the frame is built: follow library changes, take server replies, close gaps.
pub fn pump(core: &mut Core, now: Instant) {
    sync_marks(core);
    while let Ok(done) = core.actions.rx.try_recv() {
        finish(core, done);
    }
    if core.actions.leaving.is_some_and(|t| now.duration_since(t).as_secs_f32() >= LEAVE_TIME) {
        commit_leaving(core);
    }
}

/// Draws the viewer toolbar, selection bar, toast, menu and album picker. True while animating.
pub fn draw(core: &mut Core, now: Instant) -> bool {
    let a = &mut core.actions;
    a.hits.clear();
    if a.toast.as_ref().is_some_and(|t| !a.hold && now.duration_since(t.at) > TOAST_TIME + Duration::from_secs_f32(TOAST_FADE)) {
        a.toast = None;
    }
    let mut busy = a.leaving.is_some();
    let s = core.scale;
    let (w, h) = (core.size.0 as f32, core.size.1 as f32);
    let mut overlay = std::mem::take(&mut core.ui.overlay);
    let mut popup = std::mem::take(&mut core.ui.popup);

    // Popovers first: hits are searched in order, and they sit on top.
    if core.actions.picker.is_some() {
        draw_picker(core, &mut popup);
    }
    if core.actions.menu.is_some() {
        draw_menu(core, &mut popup);
    }

    let viewing = core.viewer.is_open();
    let mut bottom = h - 22.0 * s;
    let (x0, gw) = if viewing { (0.0, w) } else { (core.left, w - core.left) };
    if viewing {
        draw_toolbar(core, &mut overlay);
        // Above the video controls.
        bottom = h - 96.0 * s;
    } else if core.map.is_none() && core.actions.selected > 0 {
        let top = draw_bar(core, &mut overlay, x0, gw, bottom);
        bottom = top - 10.0 * s;
    }
    if core.map.is_none() {
        busy |= draw_toast(core, &mut overlay, x0 + gw / 2.0, bottom, now);
    }
    core.ui.overlay = overlay;
    core.ui.popup = popup;
    busy
}

fn alpha(c: [u8; 4], a: f32) -> [u8; 4] {
    [c[0], c[1], c[2], (c[3] as f32 * a.clamp(0.0, 1.0)) as u8]
}

/// Round buttons left of the viewer's info button, in its style.
fn draw_toolbar(core: &mut Core, out: &mut Layer) {
    let s = core.scale;
    let ca = core.viewer.chrome();
    let targets = viewer_target(core);
    let cmds = if targets.is_empty() { Vec::new() } else { commands(core, &targets) };
    let pitch = (ROUND_PITCH * s).round();
    core.viewer.toolbar_width = cmds.len() as f32 * pitch;
    if ca < 0.003 || targets.is_empty() {
        return;
    }
    let favorite = core.actions.favorite(&targets[0]);
    let (cursor, armed) = (core.actions.cursor, core.actions.armed);
    let d = (ROUND * s).round();
    // The info button's slot (drawn by the viewer), then leftwards at an even pitch.
    let mut x = (core.size.0 as f32 - 16.0 * s - d).round();
    // Right to left: trash nearest the info button, the heart furthest, like Photos' bar.
    for cmd in cmds.into_iter().rev() {
        x -= pitch;
        let rect = [x, (TOOLBAR_Y * s).round(), d, d];
        let hovered = contains(rect, cursor.0, cursor.1);
        let pressed = hovered && armed == Some(Hit::Toolbar(cmd));
        // Inside Archive, the archive button shows as switched on (it unarchives).
        let active = matches!(cmd, Cmd::Archive(false));
        let (icon, style, tint) = if active { ("archive", IconStyle::weight(ICON_WEIGHT), None) } else { cmd.look(favorite) };
        // Over the viewer's dark surround.
        let tint = tint.map(|_| theme::hud().red);
        let st = widgets::State { hovered, pressed, active };
        round_button(out, rect[0], rect[1], s, icon, style, tint, st, ca);
        core.actions.hits.push((Hit::Toolbar(cmd), rect));
    }
}

/// The floating "N selected" pill; returns its top edge.
fn draw_bar(core: &mut Core, out: &mut Layer, x0: f32, gw: f32, bottom: f32) -> f32 {
    let s = core.scale;
    let targets = core.actions.selection();
    let mut cmds = commands(core, &targets);
    cmds.push(Cmd::Clear);
    let (bh, size, isize) = (44.0 * s, 13.0 * s, (ICON * s).round());
    let label = format!("{} selected", group(core.actions.selected));
    let ui = &mut core.renderer.ui;
    let lw = ui.measure(&label, size, 600, Font::Sans);
    let widths: Vec<f32> = cmds
        .iter()
        .map(|c| match c {
            Cmd::Clear => 32.0 * s,
            c => 24.0 * s + isize + 7.0 * s + ui.measure(bar_label(*c), size, 500, Font::Sans),
        })
        .collect();
    let gap = 2.0 * s;
    let total = 18.0 * s + lw + 14.0 * s + widths.iter().sum::<f32>() + gap * widths.len() as f32 + 6.0 * s;
    let (bx, by) = (x0 + (gw - total) / 2.0, bottom - bh);
    let bar = [bx, by, total, bh];
    let p = theme::palette();
    popover_bg(out, bar, bh / 2.0, 1.0, s);
    let cy = by + bh / 2.0;
    out.text(Text::new(label, bx + 18.0 * s, cy - size * 0.625, size).weight(600).color(theme::text()));
    let mut x = bx + 18.0 * s + lw + 14.0 * s;
    let cursor = core.actions.cursor;
    for (cmd, bw) in cmds.into_iter().zip(widths) {
        let rect = [x, by + 5.0 * s, bw, bh - 10.0 * s];
        let hovered = contains(rect, cursor.0, cursor.1);
        if hovered {
            let down = core.actions.armed == Some(Hit::Bar(cmd));
            out.quad(Quad::solid(rect, rgb(p.shade(p.raised, if down { 0.16 } else { 0.1 }), 1.0), rect[3] / 2.0));
        }
        // "Unfavorite" shows the filled heart it removes.
        let (icon, style, tint) = cmd.look(matches!(cmd, Cmd::Favorite(false)));
        match cmd {
            Cmd::Clear => {
                out.icon_styled(icon, [(x + bw / 2.0).round(), cy.round()], isize, rgb(p.fg_dim, 1.0), style);
            }
            _ => {
                let c = [(x + 12.0 * s + isize / 2.0).round(), cy.round()];
                out.icon_styled(icon, c, isize, rgb(tint.unwrap_or(p.fg), 1.0), style);
                out.text(Text::new(bar_label(cmd), x + 12.0 * s + isize + 7.0 * s, cy - size * 0.625, size).weight(500).color(theme::text()));
            }
        }
        core.actions.hits.push((Hit::Bar(cmd), rect));
        x += bw + gap;
    }
    core.actions.hits.push((Hit::Panel, bar));
    by
}

fn bar_label(c: Cmd) -> &'static str {
    match c {
        Cmd::Album => "Add to Album",
        Cmd::Trash => "Delete",
        c => c.label(),
    }
}

fn draw_toast(core: &mut Core, out: &mut Layer, cx: f32, bottom: f32, now: Instant) -> bool {
    let Some(t) = &core.actions.toast else { return false };
    let s = core.scale;
    let age = now.duration_since(t.at).as_secs_f32();
    let a = if core.actions.hold {
        1.0
    } else {
        (age / TOAST_FADE).min(1.0).min(((TOAST_TIME.as_secs_f32() + TOAST_FADE - age) / TOAST_FADE).clamp(0.0, 1.0))
    };
    let (th, size) = (40.0 * s, 13.5 * s);
    let ui = &mut core.renderer.ui;
    let tw = ui.measure(&t.text, size, 500, Font::Sans).min(core.size.0 as f32 * 0.7);
    let uw = if t.undo.is_some() { ui.measure("Undo", size, 600, Font::Sans) + 26.0 * s } else { 0.0 };
    let p = theme::palette();
    let icon = if t.error { "circle-alert" } else { "circle-check" };
    let total = 18.0 * s + 22.0 * s + tw + uw + 18.0 * s;
    let (x, y) = ((cx - total / 2.0).round(), (bottom - th + (1.0 - a) * 8.0 * s).round());
    popover_bg(out, [x, y, total, th], th / 2.0, a, s);
    let cy = y + th / 2.0;
    let ic = if t.error { p.red } else { p.green };
    let px = crate::widgets::even_with(th, ICON * s);
    out.icon_styled(icon, [x + 18.0 * s + px / 2.0, cy], px, rgb(ic, a), IconStyle::weight(ICON_WEIGHT));
    let color = if t.error { theme::text_rgba(p.red, 1.0) } else { theme::text() };
    out.text(Text::new(t.text.clone(), x + 40.0 * s, cy - size * 0.625, size).weight(500).max_width(tw + 1.0).color(alpha(color, a)));
    if t.undo.is_some() {
        let ux = x + 40.0 * s + tw + 10.0 * s;
        out.quad(Quad::solid([ux, cy - 8.0 * s, s.round().max(1.0), 16.0 * s], rgb(p.shade(p.raised, 0.2), a), 0.0));
        out.text(Text::new("Undo", ux + 14.0 * s, cy - size * 0.625, size).weight(600).color(alpha(theme::text_rgba(p.accent, 1.0), a)));
        core.actions.hits.push((Hit::Undo, [ux, y, uw + 18.0 * s, th]));
    }
    core.actions.hits.push((Hit::Panel, [x, y, total, th]));
    a < 1.0 || !core.actions.hold
}

fn draw_menu(core: &mut Core, out: &mut Layer) {
    let Some(menu) = &core.actions.menu else { return };
    let s = core.scale;
    let (row, sep, pad, size) = (28.0 * s, 9.0 * s, 5.0 * s, 13.0 * s);
    let favorite = menu.targets.iter().all(|a| core.actions.favorite(a));
    let label = |c: Cmd| match c {
        Cmd::Favorite(_) if favorite => "Unfavorite",
        c => c.label(),
    };
    let ui = &mut core.renderer.ui;
    let width = menu.items.iter().flatten().map(|c| ui.measure(label(*c), size, 500, Font::Sans)).fold(0.0, f32::max) + 60.0 * s;
    let width = width.max(200.0 * s);
    let height = pad * 2.0 + menu.items.iter().map(|i| if i.is_some() { row } else { sep }).sum::<f32>();
    let (w, h) = (core.size.0 as f32, core.size.1 as f32);
    // Opens down-right of the cursor, flipping when it would leave the window.
    let x = if menu.at.0 + width > w - 8.0 * s { menu.at.0 - width } else { menu.at.0 };
    let y = if menu.at.1 + height > h - 8.0 * s { (menu.at.1 - height).max(8.0 * s) } else { menu.at.1 };
    let rect = [x, y, width, height];
    let p = theme::palette();
    out.quad(Quad::solid([x - 6.0 * s, y - 2.0 * s, width + 12.0 * s, height + 14.0 * s], rgb(0x000000, if p.dark { 0.25 } else { 0.1 }), 16.0 * s));
    popover_bg(out, rect, 10.0 * s, 1.0, s);
    let cursor = core.actions.cursor;
    let mut cy = y + pad;
    let mut hits = Vec::new();
    for (i, item) in menu.items.iter().enumerate() {
        let Some(cmd) = item else {
            out.quad(Quad::solid([x + 12.0 * s, (cy + sep / 2.0).round(), width - 24.0 * s, s.round().max(1.0)], rgb(p.shade(p.raised, 0.12), 1.0), 0.0));
            cy += sep;
            continue;
        };
        let r = [x + pad, cy, width - pad * 2.0, row];
        let hovered = contains(r, cursor.0, cursor.1);
        if hovered {
            out.quad(Quad::solid(r, rgb(theme::accent(), 1.0), 6.0 * s));
        }
        let (icon, style, tint) = cmd.look(favorite);
        let mid = cy + row / 2.0;
        let fg = if hovered { p.on_accent } else { p.fg };
        let ic = if hovered { fg } else { tint.unwrap_or(fg) };
        let px = crate::widgets::even_with(row, ICON * s);
        out.icon_styled(icon, [(x + 22.0 * s).round(), (cy + row / 2.0).round()], px, rgb(ic, 1.0), style);
        out.text(Text::new(label(*cmd), x + 40.0 * s, mid - size * 0.625, size).weight(500).color(theme::text_rgba(fg, 0.96)));
        hits.push((Hit::Item(i), r));
        cy += row;
    }
    hits.push((Hit::Panel, rect));
    core.actions.hits.extend(hits);
}

/// A popover's body: `raised` with a hairline edge a shade lighter (dark) or darker (light).
fn popover_bg(out: &mut Layer, r: [f32; 4], radius: f32, a: f32, s: f32) {
    let p = theme::palette();
    let line = s.round().max(1.0);
    let edge = if p.dark { p.shade(p.raised, 0.14) } else { theme::mix(p.raised, 0x000000, 0.14) };
    out.quad(Quad::solid([r[0] - line, r[1] - line, r[2] + 2.0 * line, r[3] + 2.0 * line], rgb(edge, 0.9 * a), radius + line));
    out.quad(Quad::solid(r, rgb(p.raised, 0.98 * a), radius));
}

/// Clips `r` to `clip`, also returning the matching texture coordinates.
fn clipped(r: [f32; 4], clip: [f32; 4]) -> Option<([f32; 4], [f32; 4])> {
    let (x0, y0) = (r[0].max(clip[0]), r[1].max(clip[1]));
    let (x1, y1) = ((r[0] + r[2]).min(clip[0] + clip[2]), (r[1] + r[3]).min(clip[1] + clip[3]));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let uv = [(x0 - r[0]) / r[2], (y0 - r[1]) / r[3], (x1 - r[0]) / r[2], (y1 - r[1]) / r[3]];
    Some(([x0, y0, x1 - x0, y1 - y0], uv))
}

fn draw_picker(core: &mut Core, out: &mut Layer) {
    let s = core.scale;
    let (w, h) = (core.size.0 as f32, core.size.1 as f32);
    let Some(p) = &core.actions.picker else { return };
    let albums = picker_albums(core, &p.text);
    let (text, sel, targets) = (p.text.clone(), p.sel, noun(p.targets.iter()));
    let (sw, sh) = (440.0 * s, (560.0 * s).min(h - 80.0 * s));
    let (x, y) = ((w - sw) / 2.0, (h - sh) / 2.0);
    let sheet = [x, y, sw, sh];
    let pal = theme::palette();
    let accent_text = theme::text_rgba(pal.accent, 1.0);
    out.quad(Quad::solid([0.0, 0.0, w, h], rgb(0x000000, if pal.dark { 0.45 } else { 0.25 }), 0.0));
    popover_bg(out, sheet, 14.0 * s, 1.0, s);
    out.text(Text::new("Add to Album", x + 20.0 * s, y + 16.0 * s, 17.0 * s).weight(700).color(theme::text()));
    out.text(Text::new(targets, x + 20.0 * s, y + 40.0 * s, 12.5 * s).weight(500).color(theme::text_dim()));
    let cancel = [x + sw - 84.0 * s, y + 12.0 * s, 70.0 * s, 30.0 * s];
    out.text(Text::new("Cancel", cancel[0] + cancel[2] - 6.0 * s, y + 18.0 * s, 13.5 * s).weight(500).align(Align::Right).color(accent_text));

    let field = [x + 16.0 * s, y + 66.0 * s, sw - 32.0 * s, 34.0 * s];
    out.quad(Quad::solid(field, rgb(pal.shade(pal.raised, 0.08), 1.0), 8.0 * s));
    let fs = 14.0 * s;
    let ty = field[1] + field[3] / 2.0 - fs * 0.625;
    let px = crate::widgets::even_with(field[3], 15.0 * s);
    let c = [(field[0] + 12.0 * s + px / 2.0).round(), (field[1] + field[3] / 2.0).round()];
    out.icon_styled("search", c, px, rgb(pal.muted, 0.9), IconStyle::weight(ICON_WEIGHT));
    let tx = field[0] + 34.0 * s;
    let tw = core.renderer.ui.measure(&text, fs, 400, Font::Sans);
    if text.is_empty() {
        out.text(Text::new("Search, or name a new album", tx, ty, fs).color(theme::text_faint()));
    } else {
        out.text(Text::new(text.clone(), tx, ty, fs).color(theme::text()).clip(field));
    }
    out.quad(Quad::solid([tx + tw + 1.0 * s, field[1] + 8.0 * s, 1.5 * s, field[3] - 16.0 * s], rgb(theme::accent(), 1.0), 0.0));

    let list = [x + 8.0 * s, y + 110.0 * s, sw - 16.0 * s, sh - 118.0 * s];
    let rh = 50.0 * s;
    let rows = albums.len() + 1;
    let content = rows as f32 * rh;
    let max = (content - list[3]).max(0.0) / s;
    let p = core.actions.picker.as_mut().unwrap();
    if let Some(r) = p.sel {
        // Keep the keyboard highlight in view.
        let (top, bottom) = (r as f32 * 50.0, r as f32 * 50.0 + 50.0);
        let view = list[3] / s;
        p.scroll = p.scroll.min(top).max(bottom - view);
    }
    p.scroll = p.scroll.clamp(0.0, max);
    let scroll = p.scroll * s;
    let mut hits = Vec::new();
    let first = (scroll / rh) as usize;
    for r in first..rows.min(first + (list[3] / rh) as usize + 2) {
        let ry = list[1] + r as f32 * rh - scroll;
        let row = [list[0], ry, list[2], rh];
        let Some((vis, _)) = clipped(row, list) else { continue };
        if sel == Some(r) {
            out.quad(Quad::solid(vis, rgb(pal.shade(pal.raised, 0.07), 1.0), 8.0 * s));
        }
        let thumb = [row[0] + 10.0 * s, ry + 6.0 * s, 38.0 * s, 38.0 * s];
        let mid = ry + rh / 2.0;
        let clip_text = |t: Text| t.clip(list);
        if r == 0 {
            if let Some((q, _)) = clipped(thumb, list) {
                out.quad(Quad::solid(q, rgb(pal.shade(pal.raised, 0.1), 1.0), 7.0 * s));
            }
            let px = crate::widgets::even_with(thumb[2].round(), 18.0 * s);
            let c = [(thumb[0] + thumb[2] / 2.0).round(), (thumb[1] + thumb[3] / 2.0).round()];
            out.icon_styled("plus", c, px, rgb(pal.accent, 1.0), IconStyle::weight(ICON_WEIGHT));
            out.clip_last(list);
            let label = if text.trim().is_empty() { "New Album…".to_string() } else { format!("New Album “{}”", text.trim()) };
            out.text(clip_text(Text::new(label, thumb[0] + 50.0 * s, mid - 14.0 * s * 0.625, 14.0 * s).weight(500).max_width(list[2] - 80.0 * s).color(accent_text)));
        } else {
            let (id, name, count) = &albums[r - 1];
            match (core.sidebar.image(&format!("album:{id}")), clipped(thumb, list)) {
                (Some(img), Some((q, uv))) => out.quad(Quad::image(q, img, uv, 1.0, 7.0 * s)),
                (None, Some((q, _))) => out.quad(Quad::solid(q, rgb(pal.shade(pal.raised, 0.1), 1.0), 7.0 * s)),
                _ => {}
            }
            let tx = thumb[0] + 50.0 * s;
            out.text(clip_text(Text::new(name.clone(), tx, mid - 17.0 * s, 14.0 * s).weight(500).max_width(list[2] - 80.0 * s).color(theme::text())));
            let c = format!("{} item{}", group(*count), if *count == 1 { "" } else { "s" });
            out.text(clip_text(Text::new(c, tx, mid + 2.0 * s, 12.0 * s).weight(500).color(theme::text_faint())));
        }
        hits.push((Hit::Row(r), vis));
    }
    if rows == 1 && !text.trim().is_empty() {
        out.text(Text::new("No albums match", list[0] + list[2] / 2.0, list[1] + rh + 20.0 * s, 13.0 * s).align(Align::Center).color(theme::text_faint()));
    }
    core.actions.hits.push((Hit::Cancel, cancel));
    core.actions.hits.extend(hits);
    core.actions.hits.push((Hit::Panel, sheet));
}

// ---------------------------------------------------------------------------------------------
// Headless

/// Screenshot flags that drive the same code paths as clicks and keys.
pub struct Shot {
    /// `--select-tiles 0,1,-1`: grid indices (negative from the newest end).
    select: Vec<i64>,
    /// `--context-menu <index>|viewer`, and `--menu-hover <item>` to show a highlighted row.
    menu: Option<String>,
    menu_hover: Option<usize>,
    /// `--album-picker`, optionally `--type <text>`.
    picker: bool,
    typed: Option<String>,
    /// `--toast <text>`: a sample toast with Undo.
    toast: Option<String>,
    /// `--do favorite|unfavorite|archive|unarchive|trash|restore|album:<name>|new-album:<name> --on <asset id>`
    act: Option<(String, String)>,
    /// `--undo`: after the screenshot, press the toast's Undo and wait for the server.
    undo: bool,
}

impl Shot {
    pub fn parse(args: &[String]) -> Option<Self> {
        let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
        let has = |name: &str| args.iter().any(|a| a == name);
        let shot = Shot {
            select: flag("--select-tiles").map(|s| s.split(',').filter_map(|v| v.trim().parse().ok()).collect()).unwrap_or_default(),
            menu: flag("--context-menu"),
            menu_hover: flag("--menu-hover").and_then(|s| s.parse().ok()),
            picker: has("--album-picker"),
            typed: flag("--type"),
            toast: flag("--toast"),
            act: flag("--do").map(|d| (d, flag("--on").unwrap_or_default())),
            undo: has("--undo"),
        };
        let any = !shot.select.is_empty() || shot.menu.is_some() || shot.picker || shot.toast.is_some() || shot.act.is_some();
        any.then_some(shot)
    }
}

fn resolve(lib: &Library, i: i64) -> usize {
    let n = lib.len() as i64;
    (if i < 0 { n + i } else { i }).clamp(0, (n - 1).max(0)) as usize
}

/// Sets up what `shot` asks for, settling in between.
pub fn drive(core: &mut Core, shot: &Shot, settle: &dyn Fn(&mut Core) -> Result<()>) -> Result<()> {
    core.actions.hold = true;
    let lib = core.library.clone().context("no library")?;
    sync_marks(core);
    if !shot.select.is_empty() {
        let idx: Vec<usize> = shot.select.iter().map(|&i| resolve(&lib, i)).collect();
        crate::viewer::reveal(&mut core.layout, idx[0]);
        for &i in &idx {
            select_click(core, i, ModifiersState::CONTROL);
        }
        settle(core)?;
    }
    if let Some(spec) = &shot.menu {
        if spec == "viewer" {
            let r = core.viewer.photo().context("--context-menu viewer needs --open")?;
            let t = viewer_target(core);
            open_menu(core, (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0), t, false);
        } else {
            let i = resolve(&lib, spec.parse().context("--context-menu <tile index>|viewer")?);
            crate::viewer::reveal(&mut core.layout, i);
            settle(core)?;
            let r = Spec::Grid(core.layout.target).rect(i, &core.layout.vp);
            open_tile_menu(core, i, (core.left + r[0] + r[2] / 2.0, r[1] + r[3] / 2.0));
        }
        core.update(Instant::now());
        if let Some(n) = shot.menu_hover
            && let Some(&(_, r)) = core.actions.hits.iter().find(|h| h.0 == Hit::Item(n))
        {
            core.actions.cursor = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
        }
    }
    if shot.picker {
        let mut targets = core.actions.selection();
        if targets.is_empty() {
            targets = viewer_target(core);
        }
        if targets.is_empty() {
            targets = lib.assets.last().cloned().into_iter().collect();
        }
        open_picker(core, targets);
        if let Some(t) = &shot.typed {
            core.actions.picker.as_mut().unwrap().text = t.clone();
            retarget(core);
        }
    }
    if let Some(text) = &shot.toast {
        let undo = Undo { kind: Kind::Favorite(false), assets: Vec::new(), states: Vec::new() };
        core.actions.toast(text.clone(), false, Some(undo));
    }
    if let Some((what, id)) = &shot.act {
        let source = core.viewer.current().map(|(l, _)| l.clone()).unwrap_or(lib);
        let asset = source.assets.iter().find(|a| &a.id == id).cloned();
        let Some(asset) = asset else { bail!("--on {id}: not in the collection on screen (use --select to pick one)") };
        let (verb, arg) = what.split_once(':').unwrap_or((what.as_str(), ""));
        let kind = match verb {
            "favorite" => Kind::Favorite(true),
            "unfavorite" => Kind::Favorite(false),
            "archive" => Kind::Archive(true),
            "unarchive" => Kind::Archive(false),
            "trash" => Kind::Trash,
            "restore" => Kind::Restore,
            "new-album" => Kind::NewAlbum(arg.to_string()),
            "album" => {
                let found = core.sidebar.albums().into_iter().find(|a| a.1 == arg).context("--do album:<name>: no such album")?;
                Kind::Album { id: found.0, name: found.1 }
            }
            _ => bail!("--do {what}: unknown action"),
        };
        println!("doing {kind:?} on {id}");
        perform(core, kind, vec![asset], None, true);
        settle(core)?;
        println!("toast: {}", core.actions.toast_text().unwrap_or("-"));
    }
    Ok(())
}

/// After the screenshot: `--undo` presses Undo and waits for the server.
pub fn after(core: &mut Core, shot: &Shot, settle: &dyn Fn(&mut Core) -> Result<()>) -> Result<()> {
    if !shot.undo {
        return Ok(());
    }
    if core.actions.toast.as_ref().is_none_or(|t| t.undo.is_none()) {
        bail!("--undo: nothing to undo");
    }
    let count = |core: &Core| core.library.as_ref().map_or(0, |l| l.len());
    let before = count(core);
    undo(core);
    settle(core)?;
    println!("undo toast: {} (grid {before} → {})", core.actions.toast_text().unwrap_or("-"), count(core));
    Ok(())
}
