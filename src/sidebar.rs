//! The left sidebar, like Apple Photos': library sections, utilities, albums and people.
//!
//! Albums and people are fetched once in the background; their thumbnails arrive one by one
//! over our own channel and become UI images on the next frame.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

use anyhow::{Result, bail};
use futures::{StreamExt, stream};
use image::imageops::FilterType;
use serde::Deserialize;
use winit::event::MouseScrollDelta;

use crate::Core;
use crate::immich::Immich;
use crate::loader::{Collection, Loader, Source, group, thumbnail_bytes};
use crate::ui::{Align, Font, ImageId, Quad, Text, rgb, theme};

/// Logical pixels.
pub const WIDTH: f32 = 240.0;
const ROW: f32 = 30.0;
const HEADER: f32 = 34.0;
const THUMB: f32 = 22.0;
const THUMB_PX: u32 = 64;
const SLIDE_TIME: f32 = 0.22;
const WHEEL_STEP: f32 = 60.0;
/// Height of the pinned Settings row at the bottom (feat/settings).
const FOOTER: f32 = 48.0;
/// Height of the pinned strip at the top holding the search field (feat/search).
pub const SEARCH: f32 = 46.0;
/// The search pill's height inside it.
const SEARCH_PILL: f32 = 30.0;

/// What a click asks the app to do.
pub enum Action {
    Open(Collection),
    Places,
    /// The gear row pinned at the bottom (feat/settings).
    Settings,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Album {
    id: String,
    album_name: String,
    asset_count: usize,
    album_thumbnail_asset_id: Option<String>,
    end_date: Option<String>,
    updated_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Person {
    id: String,
    name: String,
    #[serde(default)]
    is_hidden: bool,
}

#[derive(Deserialize)]
struct People {
    people: Vec<Person>,
}

enum Fetched {
    Albums(Vec<Album>),
    People(Vec<Person>),
    /// Row key and a square RGBA8 thumbnail.
    Thumb(String, Vec<u8>),
    Done,
}

#[derive(Clone, Copy)]
enum Look {
    Icon(&'static str),
    Square,
    Circle,
}

#[derive(Clone)]
enum Target {
    Open(Collection),
    Places,
    /// Scrolls the sidebar down to its People section.
    People,
}

struct Row {
    key: String,
    label: String,
    look: Look,
    count: Option<String>,
    target: Target,
}

enum Item {
    Header(&'static str),
    Row(Row),
}

pub struct Sidebar {
    /// Physical cursor position, kept up to date by the window.
    pub cursor: (f32, f32),
    shown: bool,
    /// 0 = hidden, 1 = fully shown.
    anim: f32,
    scroll: f32,
    scroll_to: f32,
    selected: String,
    albums: Vec<Album>,
    people: Vec<Person>,
    /// Items with their top edge in logical content coordinates.
    items: Vec<(f32, Item)>,
    content: f32,
    images: HashMap<String, ImageId>,
    /// Row key → (scale bits, label truncated to fit).
    labels: HashMap<String, (u32, String)>,
    rx: Receiver<Fetched>,
    tx: Sender<Fetched>,
    pending: usize,
    last: Instant,
}

impl Sidebar {
    pub fn new(loader: &Loader) -> Self {
        let (tx, rx) = channel();
        let mut pending = 0;
        if let Source::Immich(immich) = loader.source() {
            let wake = loader.waker();
            loader.handle().spawn(fetch_albums(immich.clone(), tx.clone(), wake.clone()));
            loader.handle().spawn(fetch_people(immich.clone(), tx.clone(), wake));
            pending = 2;
        }
        let mut s = Self {
            cursor: (-1.0, -1.0),
            shown: true,
            anim: 1.0,
            scroll: 0.0,
            scroll_to: 0.0,
            selected: "library".into(),
            albums: Vec::new(),
            people: Vec::new(),
            items: Vec::new(),
            content: 0.0,
            images: HashMap::new(),
            labels: HashMap::new(),
            rx,
            tx,
            pending,
            last: Instant::now(),
        };
        s.rebuild();
        s
    }

    /// Albums as fetched for the sidebar, most recent first: (id, name, asset count).
    pub fn albums(&self) -> Vec<(String, String, usize)> {
        self.albums.iter().map(|a| (a.id.clone(), a.album_name.clone(), a.asset_count)).collect()
    }

    /// Named people as fetched for the sidebar: (id, name).
    pub fn people(&self) -> Vec<(String, String)> {
        self.people.iter().map(|p| (p.id.clone(), p.name.clone())).collect()
    }

    /// The search field's pill, physical px (it slides with the sidebar).
    pub fn search_rect(&self, scale: f32) -> [f32; 4] {
        let x0 = self.visible_width(scale) - WIDTH * scale;
        let top = ((SEARCH - SEARCH_PILL) / 2.0 + 3.0) * scale;
        [(x0 + 10.0 * scale).round(), top.round(), ((WIDTH - 20.0) * scale).round(), (SEARCH_PILL * scale).round()]
    }

    /// Slides the sidebar in if it's hidden (e.g. to focus the search field).
    pub fn reveal(&mut self) {
        self.shown = true;
    }

    /// A row's thumbnail, e.g. "album:<id>", once loaded.
    pub fn image(&self, key: &str) -> Option<ImageId> {
        self.images.get(key).copied()
    }

    /// Fetches albums again (names, counts, covers), e.g. after photos were added to one.
    pub fn refresh_albums(&mut self, loader: &Loader) {
        if let Source::Immich(immich) = loader.source() {
            self.pending += 1;
            loader.handle().spawn(fetch_albums(immich.clone(), self.tx.clone(), loader.waker()));
        }
    }

    /// True once albums, people and their thumbnails have all arrived.
    pub fn is_idle(&self) -> bool {
        self.pending == 0
    }

    pub fn toggle(&mut self) {
        self.shown = !self.shown;
    }

    /// Shows or hides without the slide animation.
    pub fn set_shown(&mut self, shown: bool) {
        self.shown = shown;
        self.anim = if shown { 1.0 } else { 0.0 };
    }

    fn visible_width(&self, scale: f32) -> f32 {
        WIDTH * scale * ease(self.anim)
    }

    pub fn contains(&self, (x, y): (f32, f32), scale: f32) -> bool {
        self.anim > 0.0 && x >= 0.0 && y >= 0.0 && x < self.visible_width(scale)
    }

    /// Scrolls the sidebar if the cursor is over it; returns whether it took the event.
    pub fn wheel(&mut self, delta: MouseScrollDelta, scale: f32) -> bool {
        if !self.contains(self.cursor, scale) {
            return false;
        }
        match delta {
            MouseScrollDelta::LineDelta(_, y) => self.scroll_to -= y * WHEEL_STEP,
            MouseScrollDelta::PixelDelta(p) => {
                self.scroll_to -= p.y as f32 / scale;
                self.scroll = self.scroll_to;
            }
        }
        true
    }

    /// Handles a left click inside the sidebar.
    pub fn click(&mut self, scale: f32, height: f32) -> Option<Action> {
        let (x, y) = (self.cursor.0 - (self.visible_width(scale) - WIDTH * scale), self.cursor.1);
        if x < 10.0 * scale || x > (WIDTH - 10.0) * scale {
            return None;
        }
        // Settings: the pinned footer row (feat/settings).
        if y >= height - FOOTER * scale {
            let mid = height / scale - FOOTER / 2.0;
            if (y / scale - mid).abs() <= ROW / 2.0 {
                self.selected = "settings".into();
                return Some(Action::Settings);
            }
            return None;
        }
        // The pinned search strip is the search field's (search.rs handles its clicks).
        if y < SEARCH * scale {
            return None;
        }
        let cy = y / scale + self.scroll;
        let i = self.items.iter().position(|(top, it)| matches!(it, Item::Row(_)) && cy >= *top && cy < top + ROW)?;
        self.activate(i, height / scale)
    }

    /// Selects a row the way a click would, for headless testing: "library", "favorites",
    /// "videos", "places", "people", "archive", "trash", "album:<name>" or "person:<name>".
    /// Highlights a row by key; keys that match no row clear the highlight.
    pub fn set_selected(&mut self, key: &str) {
        self.selected = key.to_string();
    }

    /// The highlighted row's key.
    pub fn selected(&self) -> &str {
        &self.selected
    }

    pub fn select(&mut self, spec: &str, height: f32, scale: f32) -> Result<Option<Action>> {
        if spec == "settings" {
            self.selected = "settings".into();
            return Ok(Some(Action::Settings));
        }
        let (kind, name) = spec.split_once(':').unwrap_or((spec, ""));
        let name = name.to_lowercase();
        let found = self.items.iter().position(|(_, it)| match it {
            Item::Row(r) if kind == "album" || kind == "person" => {
                r.key.starts_with(&format!("{kind}:")) && r.label.to_lowercase().contains(&name)
            }
            Item::Row(r) => r.key == kind,
            Item::Header(_) => false,
        });
        let Some(i) = found else { bail!("--select {spec}: no such sidebar row") };
        let view = height / scale;
        let top = self.items[i].0;
        if top < self.scroll || top + ROW > self.scroll + view {
            self.scroll_to = (top - view / 2.0).clamp(0.0, (self.content - view).max(0.0));
            self.scroll = self.scroll_to;
        }
        Ok(self.activate(i, view))
    }

    fn activate(&mut self, i: usize, view: f32) -> Option<Action> {
        let Item::Row(row) = &self.items[i].1 else { return None };
        match row.target.clone() {
            Target::Open(c) => {
                self.selected = row.key.clone();
                Some(Action::Open(c))
            }
            Target::Places => {
                self.selected = row.key.clone();
                Some(Action::Places)
            }
            Target::People => {
                let top = self.items.iter().find(|(_, it)| matches!(it, Item::Header("People"))).map(|(y, _)| *y)?;
                self.scroll_to = (top - SEARCH).min((self.content - view).max(0.0));
                None
            }
        }
    }

    fn rebuild(&mut self) {
        let row = |key: &str, label: &str, icon, target| {
            Item::Row(Row { key: key.into(), label: label.into(), look: Look::Icon(icon), count: None, target })
        };
        let open = |key: &str, label: &str, icon, c| row(key, label, icon, Target::Open(c));
        let mut items = vec![
            Item::Header("Photos"),
            open("library", "Library", "images", Collection::library()),
            Item::Header("Pinned"),
            open("favorites", "Favorites", "heart", Collection::favorites()),
            open("videos", "Videos", "video", Collection::videos()),
            row("places", "Places", "map", Target::Places),
            row("people", "People", "users", Target::People),
            Item::Header("Utilities"),
            open("archive", "Archive", "archive", Collection::archive()),
            open("trash", "Recently Deleted", "trash-2", Collection::trash()),
        ];
        if !self.albums.is_empty() {
            items.push(Item::Header("Albums"));
        }
        for a in &self.albums {
            let c = Collection::album(&a.id, &a.album_name);
            items.push(Item::Row(Row {
                key: c.key.clone(),
                label: a.album_name.clone(),
                look: Look::Square,
                count: Some(group(a.asset_count)),
                target: Target::Open(c),
            }));
        }
        if !self.people.is_empty() {
            items.push(Item::Header("People"));
        }
        for p in &self.people {
            let c = Collection::person(&p.id, &p.name);
            items.push(Item::Row(Row {
                key: c.key.clone(),
                label: p.name.clone(),
                look: Look::Circle,
                count: None,
                target: Target::Open(c),
            }));
        }
        let mut y = SEARCH;
        self.items = items
            .into_iter()
            .map(|it| {
                let top = y;
                y += if matches!(it, Item::Header(_)) { HEADER } else { ROW };
                (top, it)
            })
            .collect();
        self.content = y + 16.0;
        self.labels.clear();
    }
}

/// Pumps background results, animates, reserves the grid's left edge and draws the sidebar.
/// Returns true while anything is still moving.
pub fn update(core: &mut Core, now: Instant) -> bool {
    let (s, h) = (core.scale, core.size.1 as f32);
    let Core { sidebar: sb, renderer, ui, .. } = core;
    let dt = now.duration_since(sb.last).as_secs_f32().min(0.05);
    sb.last = now;

    let mut changed = false;
    while let Ok(msg) = sb.rx.try_recv() {
        match msg {
            Fetched::Albums(a) => (sb.albums, changed) = (a, true),
            Fetched::People(p) => (sb.people, changed) = (p, true),
            Fetched::Thumb(key, rgba) => {
                let id = renderer.ui.create_image(&renderer.device, &renderer.queue, THUMB_PX, THUMB_PX, &rgba);
                if let Some(old) = sb.images.insert(key, id) {
                    renderer.ui.free_image(old);
                }
            }
            Fetched::Done => sb.pending -= 1,
        }
    }
    if changed {
        sb.rebuild();
    }

    let goal = if sb.shown { 1.0 } else { 0.0 };
    let sliding = sb.anim != goal;
    sb.anim = if sb.anim < goal { (sb.anim + dt / SLIDE_TIME).min(goal) } else { (sb.anim - dt / SLIDE_TIME).max(goal) };
    let view = h / s - FOOTER;
    let max = (sb.content - view).max(0.0);
    sb.scroll_to = sb.scroll_to.clamp(0.0, max);
    let gliding = (sb.scroll_to - sb.scroll).abs() > 0.3;
    sb.scroll = if gliding { sb.scroll + (sb.scroll_to - sb.scroll) * (1.0 - (-dt * 20.0).exp()) } else { sb.scroll_to };

    let left = sb.visible_width(s);
    if sb.anim > 0.0 {
        draw(sb, &mut renderer.ui, &mut ui.base, s, left - WIDTH * s, h);
    }
    core.set_left(left);
    sliding || gliding
}

fn draw(sb: &mut Sidebar, uir: &mut crate::ui_render::UiRenderer, base: &mut crate::ui::Layer, s: f32, x0: f32, h: f32) {
    let width = WIDTH * s;
    base.quad(Quad::solid([x0, 0.0, width, h], rgb(theme::sidebar(), 0.99), 0.0));
    let line = s.round().max(1.0);
    base.quad(Quad::solid([x0 + width - line, 0.0, line, h], rgb(theme::separator(), 1.0), 0.0));
    let clip = [x0, SEARCH * s, width - line, h - (FOOTER + SEARCH) * s];
    let hover = sb.contains(sb.cursor, s).then_some(sb.cursor);

    for (top, item) in &sb.items {
        let y = (top - sb.scroll) * s;
        if y > h || y + ROW * s < 0.0 {
            continue;
        }
        let row = match item {
            Item::Header(label) => {
                base.text(
                    Text::new(*label, x0 + 20.0 * s, y + 15.0 * s, 11.5 * s).weight(600).color(theme::text_faint()).clip(clip),
                );
                continue;
            }
            Item::Row(r) => r,
        };
        let pill = [x0 + 10.0 * s, y, width - 20.0 * s, ROW * s];
        let selected = row.key == sb.selected;
        let hovered = hover.is_some_and(|c| crate::ui::contains(pill, c.0, c.1));
        let p = theme::palette();
        pill_bg(base, pill, selected, hovered, s);
        let mid = y + ROW * s / 2.0;
        match row.look {
            Look::Icon(icon) => row_icon(base, icon, x0, mid, selected, s),
            Look::Square | Look::Circle => {
                let r = if matches!(row.look, Look::Circle) { THUMB / 2.0 } else { 5.0 } * s;
                let rect = [x0 + 20.0 * s, mid - THUMB * s / 2.0, THUMB * s, THUMB * s];
                match sb.images.get(&row.key) {
                    Some(&img) => base.quad(Quad::image(rect, img, [0.0, 0.0, 1.0, 1.0], 1.0, r)),
                    None => base.quad(Quad::solid(rect, rgb(p.shade(p.surface, 0.1), 1.0), r)),
                }
            }
        }

        let (text_size, count_size) = (13.0 * s, 12.0 * s);
        let right = x0 + width - 22.0 * s;
        let mut avail = right - (x0 + 52.0 * s);
        if let Some(count) = &row.count {
            let color = if selected { theme::text_rgba(p.on_accent, 0.75) } else { theme::text_faint() };
            base.text(Text::new(count.clone(), right, mid - count_size * 0.625, count_size).weight(500).color(color).align(Align::Right).clip(clip));
            avail -= uir.measure(count, count_size, 500, Font::Sans) + 10.0 * s;
        }
        let bits = (s * 1000.0 + avail) as u32;
        let label = match sb.labels.get(&row.key) {
            Some((b, l)) if *b == bits => l.clone(),
            _ => {
                let l = ellipsize(uir, &row.label, text_size, avail);
                sb.labels.insert(row.key.clone(), (bits, l.clone()));
                l
            }
        };
        let color = if selected { theme::text_rgba(p.on_accent, 1.0) } else { theme::text() };
        base.text(Text::new(label, x0 + 52.0 * s, mid - text_size * 0.625, text_size).weight(500).color(color).clip(clip));
    }
    draw_footer(sb, base, s, x0, h);
    // The search strip, pinned over rows scrolled under it; search.rs draws the field on it.
    base.quad(Quad::solid([x0, 0.0, width - line, SEARCH * s], rgb(theme::sidebar(), 1.0), 0.0));
    if sb.scroll > 0.5 {
        base.quad(Quad::solid([x0, SEARCH * s - line, width - line, line], rgb(theme::separator(), 1.0), 0.0));
    }
}

/// The selected (accent) or hovered row background.
fn pill_bg(base: &mut crate::ui::Layer, pill: [f32; 4], selected: bool, hovered: bool, s: f32) {
    let p = theme::palette();
    if selected {
        base.quad(Quad::solid(pill, rgb(p.accent, 1.0), 7.0 * s));
    } else if hovered {
        // Opaque: a translucent white blended in linear space comes out far too bright.
        base.quad(Quad::solid(pill, rgb(p.shade(p.surface, if p.dark { 0.08 } else { 0.07 }), 1.0), 7.0 * s));
    }
}

/// A row's symbol, in the accent colour (or the accent's text colour on the selected pill).
fn row_icon(base: &mut crate::ui::Layer, icon: &'static str, x0: f32, mid: f32, selected: bool, s: f32) {
    let p = theme::palette();
    let color = if selected { p.on_accent } else { p.accent };
    let px = crate::widgets::even_with((ROW * s).round(), 17.0 * s);
    let style = crate::icons::IconStyle::weight(crate::widgets::ICON_WEIGHT);
    base.icon_styled(icon, [(x0 + 31.0 * s).round(), mid.round()], px, rgb(color, 1.0), style);
}

/// Settings row pinned under the scrolling list, over an opaque strip (feat/settings).
fn draw_footer(sb: &Sidebar, base: &mut crate::ui::Layer, s: f32, x0: f32, h: f32) {
    let (width, line) = (WIDTH * s, s.round().max(1.0));
    let top = h - FOOTER * s;
    base.quad(Quad::solid([x0, top, width - line, FOOTER * s], rgb(theme::sidebar(), 1.0), 0.0));
    if sb.content - sb.scroll > h / s - FOOTER {
        base.quad(Quad::solid([x0, top, width - line, line], rgb(theme::separator(), 1.0), 0.0));
    }
    let pill = [x0 + 10.0 * s, top + (FOOTER - ROW) / 2.0 * s, width - 20.0 * s, ROW * s];
    let selected = sb.selected == "settings";
    let hovered = sb.contains(sb.cursor, s) && crate::ui::contains(pill, sb.cursor.0, sb.cursor.1);
    pill_bg(base, pill, selected, hovered, s);
    let mid = pill[1] + ROW * s / 2.0;
    let text = 13.0 * s;
    row_icon(base, "settings", x0, mid, selected, s);
    let color = if selected { theme::text_rgba(theme::palette().on_accent, 1.0) } else { theme::text() };
    base.text(Text::new("Settings", x0 + 52.0 * s, mid - text * 0.625, text).weight(500).color(color));
}

/// Shortens `text` with a trailing ellipsis until it fits in `max` pixels.
fn ellipsize(uir: &mut crate::ui_render::UiRenderer, text: &str, size: f32, max: f32) -> String {
    if uir.measure(text, size, 500, Font::Sans) <= max {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let fits = |n: usize, uir: &mut crate::ui_render::UiRenderer| {
        let t: String = chars[..n].iter().collect::<String>().trim_end().to_string() + "…";
        (uir.measure(&t, size, 500, Font::Sans) <= max).then_some(t)
    };
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(mid, uir).is_some() { lo = mid } else { hi = mid - 1 }
    }
    fits(lo, uir).unwrap_or_else(|| "…".into())
}

fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

type Wake = Arc<dyn Fn() + Send + Sync>;

async fn fetch_albums(immich: Immich, tx: Sender<Fetched>, wake: Wake) {
    match immich.get_json::<Vec<Album>>("/albums").await {
        Ok(mut albums) => {
            let recent = |a: &Album| a.end_date.clone().or_else(|| a.updated_at.clone()).unwrap_or_default();
            albums.sort_by_key(|a| std::cmp::Reverse(recent(a)));
            let thumbs: Vec<(String, String)> = albums
                .iter()
                .filter_map(|a| Some((format!("album:{}", a.id), a.album_thumbnail_asset_id.clone()?)))
                .collect();
            let _ = tx.send(Fetched::Albums(albums));
            wake();
            load_thumbs(&tx, &wake, thumbs, |id| {
                let immich = immich.clone();
                async move { thumbnail_bytes(&immich, &id).await }
            })
            .await;
        }
        Err(e) => log::warn!("albums: {e:#}"),
    }
    let _ = tx.send(Fetched::Done);
    wake();
}

async fn fetch_people(immich: Immich, tx: Sender<Fetched>, wake: Wake) {
    match immich.get_json::<People>("/people?withHidden=false&size=500").await {
        Ok(People { people }) => {
            let people: Vec<Person> = people.into_iter().filter(|p| !p.is_hidden && !p.name.trim().is_empty()).collect();
            let thumbs = people.iter().map(|p| (format!("person:{}", p.id), p.id.clone())).collect();
            let _ = tx.send(Fetched::People(people));
            wake();
            load_thumbs(&tx, &wake, thumbs, |id| {
                let immich = immich.clone();
                async move { immich.get_bytes(&format!("/people/{id}/thumbnail")).await }
            })
            .await;
        }
        Err(e) => log::warn!("people: {e:#}"),
    }
    let _ = tx.send(Fetched::Done);
    wake();
}

/// Fetches, decodes and square-crops (row key, id) pairs a few at a time.
async fn load_thumbs<F, Fut>(tx: &Sender<Fetched>, wake: &Wake, items: Vec<(String, String)>, get: F)
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    stream::iter(items)
        .for_each_concurrent(6, |(key, id)| {
            let bytes = get(id);
            async move {
                let rgba = async {
                    let bytes = bytes.await?;
                    tokio::task::spawn_blocking(move || {
                        anyhow::Ok(
                            image::load_from_memory(&bytes)?
                                .resize_to_fill(THUMB_PX, THUMB_PX, FilterType::CatmullRom)
                                .to_rgba8()
                                .into_raw(),
                        )
                    })
                    .await?
                };
                match rgba.await {
                    Ok(rgba) => {
                        let _ = tx.send(Fetched::Thumb(key, rgba));
                        wake();
                    }
                    Err(e) => log::warn!("sidebar thumbnail {key}: {e:#}"),
                }
            }
        })
        .await;
}
