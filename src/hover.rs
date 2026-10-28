//! Grid hover previews and tile badges, Apple-style.
//!
//! Resting the mouse on a video or Live Photo tile for a moment plays a muted, looping,
//! tile-sized preview over its thumbnail. Pipelines are built and torn down off the UI
//! thread; at most one preview plays at a time. Videos show their duration and Live Photos
//! a small "LIVE" glyph once tiles are big enough to read them.
//!
//! `Clip` (one video pipeline + its crossfading frame texture) is shared with the viewer,
//! which uses it to play a Live Photo's motion over the still.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use crate::Core;
use crate::immich::Immich;
use crate::library::{Asset, Library};
use crate::loader::Source;
use crate::render::{Instance, Renderer};
use crate::icons::IconStyle;
use crate::ui::{Align, ImageId, Layer, Quad, Text, contains, rgb};
use crate::video::{self, Options, Player};

/// How long the mouse must rest on a tile before its preview starts.
const HOVER_DELAY: Duration = Duration::from_millis(300);
/// Smallest tile (logical px) that plays previews / shows badges.
const HOVER_MIN: f32 = 120.0;
const BADGE_MIN: f32 = 90.0;
/// Preview frames never exceed this per side, however big the tile.
const PREVIEW_MAX: u32 = 1280;
const FADE_IN: f32 = 0.25;
const FADE_OUT: f32 = 0.2;

// ---------------------------------------------------------------------------------------------
// Clip

/// One video pipeline playing for an asset, plus the texture its frames land in. The texture
/// outlives the pipeline so the last frame can fade out after `stop`.
pub struct Clip {
    /// The asset this plays for: the grid tile or the photo in the viewer.
    pub id: String,
    uri: String,
    key: String,
    opts: Options,
    wake: Arc<dyn Fn() + Send + Sync>,
    /// The pipeline being built on a helper thread.
    pending: Option<Receiver<Option<Player>>>,
    player: Option<Player>,
    tex: Option<(ImageId, u32, u32)>,
    shown_at: Option<Instant>,
    stopped_at: Option<Instant>,
    started: Instant,
}

impl Clip {
    /// Starts streaming `/api/assets/{video}/video/playback` for asset `id`. Returns at once.
    pub fn start(immich: &Immich, video: &str, id: String, opts: Options, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let uri = format!("{}/api/assets/{video}/video/playback", immich.base());
        let mut clip = Self {
            id,
            uri,
            key: immich.api_key().to_string(),
            opts,
            wake,
            pending: None,
            player: None,
            tex: None,
            shown_at: None,
            stopped_at: None,
            started: Instant::now(),
        };
        clip.build();
        clip
    }

    /// Builds the pipeline on a helper thread (GStreamer init and PAUSED can block). If the
    /// clip is gone by the time it's ready, the send fails and dropping the player tears it down.
    fn build(&mut self) {
        let (tx, rx) = channel();
        let (uri, key, opts, wake) = (self.uri.clone(), self.key.clone(), self.opts.clone(), self.wake.clone());
        std::thread::spawn(move || {
            let player = video::available()
                .then(|| Player::new(&uri, &key, opts, wake.clone()).map_err(|e| log::warn!("video: {e:#}")).ok())
                .flatten();
            if tx.send(player).is_ok() {
                wake();
            }
        });
        self.pending = Some(rx);
    }

    /// Drains the pipeline and uploads its newest frame. Returns true while fading.
    pub fn tick(&mut self, now: Instant, r: &mut Renderer) -> bool {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(Some(p)) => {
                    self.pending = None;
                    self.player = Some(p);
                }
                Ok(None) | Err(TryRecvError::Disconnected) => self.stop(now),
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(p) = &mut self.player {
            p.tick();
            if p.retry {
                // Hardware decoding broke; GStreamer is now on software decoders.
                self.player = None;
                self.build();
            } else if p.failed || p.ended {
                self.stop(now);
            } else {
                let (tex, shown) = (&mut self.tex, &mut self.shown_at);
                p.take_frame(|w, h, rgba| {
                    match *tex {
                        Some((id, tw, th)) if (tw, th) == (w, h) => r.ui.update_image(&r.queue, id, rgba),
                        old => {
                            if let Some((id, _, _)) = old {
                                r.ui.free_image(id);
                            }
                            *tex = Some((r.ui.create_frame_image(&r.device, &r.queue, w, h, rgba), w, h));
                        }
                    }
                    shown.get_or_insert(now);
                });
            }
        }
        let fading_in = self.shown_at.is_some_and(|t| now.duration_since(t).as_secs_f32() < FADE_IN);
        let fading_out = self.stopped_at.is_some() && self.tex.is_some();
        fading_in || fading_out
    }

    /// Tears the pipeline down (asynchronously) and fades the last frame out.
    pub fn stop(&mut self, now: Instant) {
        self.stopped_at.get_or_insert(now);
        self.pending = None;
        self.player = None;
    }

    pub fn stopped(&self) -> bool {
        self.stopped_at.is_some()
    }

    /// Opacity over the still: eases in from the first frame, out after `stop`.
    pub fn alpha(&self, now: Instant) -> f32 {
        let fin = self.shown_at.map_or(0.0, |t| ease_out((now.duration_since(t).as_secs_f32() / FADE_IN).min(1.0)));
        let fout = self.stopped_at.map_or(1.0, |t| 1.0 - (now.duration_since(t).as_secs_f32() / FADE_OUT).min(1.0));
        fin * fout
    }

    /// The fade-out is over; `release` may free the texture.
    pub fn finished(&self, now: Instant) -> bool {
        self.stopped_at.is_some_and(|t| now.duration_since(t).as_secs_f32() >= FADE_OUT || self.tex.is_none())
    }

    /// Frees the frame texture (the clip stays usable as a "done" marker).
    pub fn release(&mut self, r: &mut Renderer) {
        if let Some((id, _, _)) = self.tex.take() {
            r.ui.free_image(id);
        }
    }

    pub fn image(&self) -> Option<(ImageId, u32, u32)> {
        self.tex
    }

    /// How long the first frame has been up.
    pub fn shown_for(&self, now: Instant) -> Option<Duration> {
        self.shown_at.map(|t| now.duration_since(t))
    }

    /// Headless: a frame is up, fully faded in, and the pipeline reached PLAYING.
    pub fn ready(&self, now: Instant) -> bool {
        self.shown_for(now).is_some_and(|d| d.as_secs_f32() >= FADE_IN)
            && self.player.as_ref().is_some_and(|p| p.reached_playing())
    }

    /// One line for test logs.
    pub fn status(&self) -> String {
        let size = self.tex.map_or("none".into(), |(_, w, h)| format!("{w}x{h}"));
        match &self.player {
            Some(p) => format!(
                "clip {}: frame {size} pts {:.2}s, {} frames, playing {}, failed {}, after {:.0} ms",
                self.id,
                p.last_pts,
                p.frames,
                p.reached_playing(),
                p.failed,
                self.started.elapsed().as_secs_f64() * 1000.0
            ),
            None if self.pending.is_some() => format!("clip {}: starting", self.id),
            None => format!("clip {}: stopped, frame {size}", self.id),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Grid hover

#[derive(Default)]
pub struct Hover {
    /// Mouse position in physical px; None while it's outside the window.
    pub cursor: Option<(f32, f32)>,
    /// Play previews with sound (Settings › hover sound).
    pub sound: bool,
    /// Never make noise, whatever `sound` says (headless).
    pub silent: bool,
    /// Headless: skip the resting delay.
    pub instant: bool,
    /// Asset under the mouse that could preview, and since when.
    candidate: Option<(String, Instant)>,
    /// The preview for the tile under the mouse (kept after it stops, so it doesn't restart).
    clip: Option<(usize, Clip)>,
    /// Previews fading out after the mouse left.
    fading: Vec<(usize, Clip)>,
}

impl Hover {
    pub fn new() -> Self {
        Self::default()
    }

    /// Headless: the active preview, once it shows a frame.
    pub fn clip(&self) -> Option<&Clip> {
        self.clip.as_ref().map(|c| &c.1)
    }
}

struct Target {
    index: usize,
    id: String,
    /// The asset whose stream plays: the video itself, or the Live Photo's motion clip.
    video: String,
    ratio: f32,
    size: f32,
}

/// Badges, then hover previews, into `ui.base` under the sidebar. Call after the grid
/// instances are built. Returns true while anything fades.
pub fn update(core: &mut Core, now: Instant) -> bool {
    let before = core.ui.base.quads.len();
    let busy = run(core, now);
    // The sidebar drew first; slide our quads underneath it (same as the map does).
    let added = core.ui.base.quads.len() - before;
    core.ui.base.quads.rotate_right(added);
    busy
}

fn run(core: &mut Core, now: Instant) -> bool {
    let s = core.scale;
    // Content scrolls under the header's gradient; things we draw over tiles follow it too.
    let scrim_h = core.layout.vp.top * 1.6;
    let lib = core.library.clone();

    let target = lib.as_ref().and_then(|lib| target(core, lib, now));
    let settled = core.velocity == 0.0 && core.smooth_scroll == 0.0;
    let immich = match core.loader.source() {
        Source::Immich(i) => Some(i.clone()),
        Source::Demo(_) => None,
    };
    let wake = core.loader.waker();
    let h = &mut core.hover;
    let r = &mut core.renderer;

    let tid = target.as_ref().map(|t| &t.id);
    if h.candidate.as_ref().map(|c| &c.0) != tid {
        h.candidate = tid.map(|id| (id.clone(), now));
        // Wake up when the resting delay is over, rather than redrawing until then.
        if tid.is_some() && !h.instant {
            let wake = wake.clone();
            std::thread::spawn(move || {
                std::thread::sleep(HOVER_DELAY + Duration::from_millis(5));
                wake();
            });
        }
    }
    if h.clip.as_ref().is_some_and(|c| Some(&c.1.id) != tid) {
        let (i, mut c) = h.clip.take().unwrap();
        c.stop(now);
        h.fading.push((i, c));
    }
    let rested = h.instant || h.candidate.as_ref().is_some_and(|c| now.duration_since(c.1) >= HOVER_DELAY);
    if let (Some(t), None, true, true, Some(immich)) = (&target, &h.clip, settled, rested, &immich) {
        // Big enough that the frame's short side covers the tile's square crop.
        let long = t.size * t.ratio.max(1.0 / t.ratio.max(0.01));
        let mut opts = Options::preview((long.ceil() as u32).clamp(64, PREVIEW_MAX));
        if h.sound && !h.silent {
            (opts.no_audio, opts.muted, opts.silent, opts.volume) = (false, false, false, 1.0);
        }
        log::info!("hover: preview {} ({}px tile)", t.id, t.size as u32);
        h.clip = Some((t.index, Clip::start(immich, &t.video, t.id.clone(), opts, wake)));
    }

    let mut busy = false;
    if let Some((_, c)) = &mut h.clip {
        busy |= c.tick(now, r);
        if c.finished(now) {
            c.release(r);
        }
    }
    h.fading.retain_mut(|(_, c)| {
        busy |= c.tick(now, r);
        let done = c.finished(now);
        if done {
            c.release(r);
        }
        !done
    });

    // Draw over the tiles they belong to (if those are still on screen), then the badges.
    let Some(lib) = lib.filter(|_| core.map.is_none()) else { return busy };
    let out = &mut core.ui.base;
    for (i, c) in h.fading.iter().chain(h.clip.iter()) {
        let Some((tex, w, hh)) = c.image() else { continue };
        if lib.assets.get(*i).is_none_or(|a| a.id != c.id) {
            continue;
        }
        let Some(tile) = core.instances.iter().find(|t| t.th as usize == *i) else { continue };
        let alpha = c.alpha(now);
        if alpha <= 0.0 {
            continue;
        }
        out.quad(Quad::image(tile.rect, tex, square_crop(w, hh), alpha, 0.0));
        scrim(out, tile.rect, alpha, scrim_h);
    }
    badges(out, &core.instances, &lib, s, scrim_h);
    busy
}

/// The playable tile under the mouse, if previews are allowed right now.
fn target(core: &Core, lib: &Library, now: Instant) -> Option<Target> {
    if !core.hover_play || core.viewer.is_open() || core.map.is_some() || core.settings_page.is_some() || core.layout.animating(now) {
        return None;
    }
    let (x, y) = core.hover.cursor?;
    if x < core.left || core.sidebar.contains((x, y), core.scale) {
        return None;
    }
    let tile = core.instances.iter().find(|t| contains(t.rect, x, y))?;
    let size = tile.rect[2];
    if size < HOVER_MIN * core.scale {
        return None;
    }
    let a = lib.assets.get(tile.th as usize)?;
    let video = playable(a)?;
    Some(Target { index: tile.th as usize, id: a.id.clone(), video, ratio: a.ratio, size })
}

/// The stream a tile previews: a video's own, or a Live Photo's motion clip.
fn playable(a: &Asset) -> Option<String> {
    if a.video { Some(a.id.clone()) } else { a.live_video.clone() }
}

/// Duration (videos) and LIVE (Live Photos) on tiles at least `BADGE_MIN` big.
fn badges(out: &mut Layer, tiles: &[Instance], lib: &Library, s: f32, scrim_h: f32) {
    let min = BADGE_MIN * s;
    for t in tiles.iter().filter(|t| t.rect[2] >= min) {
        let Some(a) = lib.assets.get(t.th as usize) else { continue };
        if !a.video && a.live_video.is_none() {
            continue;
        }
        let [x, y, w, h] = t.rect;
        let size = if w >= 220.0 * s { 13.0 * s } else { 12.0 * s };
        let pad = 6.0 * s;
        let shadow = 1.0f32.max(s);
        if a.video {
            // A soft gradient along the bottom edge keeps the label readable on bright frames.
            let gh = (size * 2.8).min(h * 0.5);
            let top = y + h - gh;
            let k = 1.0 - scrim_alpha(top, scrim_h) / 0.86;
            let bands = 6;
            for b in 0..bands {
                let f = (b as f32 + 0.5) / bands as f32;
                let band = [x, top + gh * b as f32 / bands as f32, w, gh / bands as f32 + 0.5];
                out.quad(Quad::solid(band, rgb(0x000000, 0.32 * f * f * k), 0.0));
            }
            let ty = y + h - pad - size * 1.25;
            let k = 1.0 - scrim_alpha(ty, scrim_h) / 0.86;
            match a.duration_ms.map(clock) {
                Some(label) => {
                    let text = |dx: f32, color: [u8; 4]| {
                        Text::new(label.clone(), x + w - pad + dx, ty + dx, size).weight(600).align(Align::Right).color(color)
                    };
                    out.text(text(shadow, [0, 0, 0, (120.0 * k) as u8]));
                    out.text(text(0.0, [255, 255, 255, (240.0 * k) as u8]));
                }
                None => {
                    // No duration known: a small play mark in the same corner.
                    let px = size.round();
                    let c = [(x + w - pad - px / 2.0).round(), (ty + size * 0.625).round()];
                    out.icon_styled("play", [c[0] + shadow, c[1] + shadow], px, [0.0, 0.0, 0.0, 0.47 * k], IconStyle::filled());
                    out.icon_styled("play", c, px, [1.0, 1.0, 1.0, 0.94 * k], IconStyle::filled());
                }
            }
        } else {
            let px = (size * 1.4).round();
            let k = 1.0 - scrim_alpha(y + pad, scrim_h) / 0.86;
            let c = [(x + pad + px / 2.0).round(), (y + pad + px / 2.0).round()];
            let style = IconStyle::weight(2.25);
            out.icon_styled("live-photo", [c[0] + shadow, c[1] + shadow], px, [0.0, 0.0, 0.0, 0.47 * k], style);
            out.icon_styled("live-photo", c, px, [1.0, 1.0, 1.0, 0.92 * k], style);
        }
    }
}

/// The header gradient's opacity at screen row `y` (see `fs_header` in grid.wgsl).
fn scrim_alpha(y: f32, h: f32) -> f32 {
    if h <= 0.0 || y >= h {
        return 0.0;
    }
    let t = (y / h).clamp(0.0, 1.0);
    0.86 * (1.0 - t * t * (3.0 - 2.0 * t))
}

/// Re-applies the header gradient over what we drew in `rect` (it was drawn before us).
fn scrim(out: &mut Layer, rect: [f32; 4], alpha: f32, h: f32) {
    let (y0, y1) = (rect[1].max(0.0), (rect[1] + rect[3]).min(h));
    if y1 <= y0 {
        return;
    }
    let bands = ((y1 - y0) / 4.0).ceil().clamp(1.0, 24.0) as usize;
    let bh = (y1 - y0) / bands as f32;
    for b in 0..bands {
        let y = y0 + bh * b as f32;
        let a = scrim_alpha(y + bh / 2.0, h) * alpha;
        out.quad(Quad::solid([rect[0], y, rect[2], bh + 0.5], rgb(crate::theme::scrim(), a), 0.0));
    }
}

/// Texture coordinates of the centred square, matching the grid's square thumbnails.
pub fn square_crop(w: u32, h: u32) -> [f32; 4] {
    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    if w >= h {
        let k = h / w;
        [(1.0 - k) / 2.0, 0.0, (1.0 + k) / 2.0, 1.0]
    } else {
        let k = w / h;
        [0.0, (1.0 - k) / 2.0, 1.0, (1.0 + k) / 2.0]
    }
}

/// 11001 ms → "0:11"; 3_725_000 → "1:02:05".
fn clock(ms: u32) -> String {
    let secs = (ms as u64 + 500) / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}
