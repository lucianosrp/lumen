//! The zoom slider at the top right of the grid: − and + around a track with a stop per zoom
//! level, like Apple Photos'. It appears when the pointer moves over the grid or the zoom
//! changes, and fades away after a moment unless it's hovered or dragged.

use std::time::{Duration, Instant};

use winit::event::{ElementState, MouseButton, WindowEvent};

use crate::Core;
use crate::icons::IconStyle;
use crate::ui::{Quad, rgb, theme};

/// Logical pixels.
const WIDTH: f32 = 196.0;
const HEIGHT: f32 = 30.0;
const MARGIN: f32 = 20.0;
const BUTTON: f32 = 28.0;
/// Shown this long after the last pointer move or zoom.
const IDLE: Duration = Duration::from_millis(1800);

#[derive(Default)]
pub struct ZoomBar {
    shown_at: Option<Instant>,
    alpha: f32,
    /// The knob's position along the track, 0 = zoomed out, eased towards the level.
    knob: f32,
    dragging: bool,
    cursor: (f32, f32),
    /// Pill, − and + buttons, and the track's ends, from the last frame (physical px).
    pill: [f32; 4],
    minus: [f32; 4],
    plus: [f32; 4],
    track: [f32; 2],
    last_level: Option<usize>,
    /// Headless: stay shown.
    pub hold: bool,
}

impl ZoomBar {
    /// Wakes the bar (pointer moved over the grid, a zoom happened).
    pub fn poke(&mut self, now: Instant) {
        self.shown_at = Some(now);
    }

    /// Room it takes at the right of the header while showing (physical px), so the status
    /// line can sit beside it.
    pub fn reserve(&self, s: f32) -> f32 {
        if self.alpha > 0.01 { (WIDTH + 12.0) * s } else { 0.0 }
    }

    fn over(&self, at: (f32, f32)) -> bool {
        self.alpha > 0.3 && contains(self.pill, at)
    }
}

fn contains(r: [f32; 4], (x, y): (f32, f32)) -> bool {
    x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]
}

/// Only over the plain grid: not in the viewer, map, settings or sign-in.
fn active(core: &Core) -> bool {
    core.login.is_none()
        && core.map.is_none()
        && core.settings_page.is_none()
        && !core.viewer.is_open()
        && core.layout.n > 0
        && core.layout.levels() > 1
}

/// Level for a point on the track: left is zoomed out (the last level), right zoomed in.
fn level_at(core: &Core, x: f32) -> usize {
    let bar = &core.zoombar;
    let n = core.layout.levels();
    let t = ((x - bar.track[0]) / (bar.track[1] - bar.track[0]).max(1.0)).clamp(0.0, 1.0);
    let from_out = (t * (n - 1) as f32).round() as usize;
    n - 1 - from_out
}

fn zoom_to(core: &mut Core, level: usize, now: Instant) {
    let (cx, cy) = (core.layout.vp.width / 2.0, core.layout.vp.height / 2.0);
    core.velocity = 0.0;
    core.layout.zoom_to(level, cx, cy, now);
}

/// Window input. Returns true when the bar took it.
pub fn input(core: &mut Core, event: &WindowEvent, cursor: (f32, f32)) -> bool {
    let now = Instant::now();
    if !active(core) {
        core.zoombar.dragging = false;
        return false;
    }
    let bar = &mut core.zoombar;
    bar.cursor = cursor;
    match event {
        WindowEvent::CursorMoved { .. } => {
            if cursor.0 > core.left {
                bar.poke(now);
            }
            if bar.dragging {
                let level = level_at(core, cursor.0);
                zoom_to(core, level, now);
            }
            false
        }
        WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } if bar.over(cursor) => {
            bar.poke(now);
            let level = core.layout.level;
            if contains(bar.minus, cursor) {
                zoom_to(core, (level + 1).min(core.layout.levels() - 1), now);
            } else if contains(bar.plus, cursor) {
                zoom_to(core, level.saturating_sub(1), now);
            } else {
                bar.dragging = true;
                let level = level_at(core, cursor.0);
                zoom_to(core, level, now);
            }
            true
        }
        WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } if bar.dragging || bar.over(cursor) => {
            bar.dragging = false;
            true
        }
        _ => false,
    }
}

/// Draws the bar at the top right of the header. Returns true while it animates.
pub fn update(core: &mut Core, now: Instant, dt: f32) -> bool {
    let on = active(core);
    let level = core.layout.level;
    let bar = &mut core.zoombar;
    // A zoom from anywhere (keys, pinch, wheel) brings it up too.
    if on && bar.last_level.is_some_and(|l| l != level) {
        bar.poke(now);
    }
    bar.last_level = Some(level);
    let hovered = contains(bar.pill, bar.cursor);
    let recent = bar.shown_at.is_some_and(|t| now.duration_since(t) < IDLE);
    let visible = on && (bar.hold || bar.dragging || recent || (hovered && bar.alpha > 0.01));
    let goal = if visible { 1.0 } else { 0.0 };
    bar.alpha += (goal - bar.alpha) * (1.0 - (-dt * 14.0).exp());
    if (goal - bar.alpha).abs() < 0.01 {
        bar.alpha = goal;
    }
    let n = core.layout.levels().max(2);
    let knob_goal = (n - 1 - level.min(n - 1)) as f32 / (n - 1) as f32;
    bar.knob += (knob_goal - bar.knob) * (1.0 - (-dt * 18.0).exp());
    if (knob_goal - bar.knob).abs() < 0.002 || bar.alpha < 0.01 {
        bar.knob = knob_goal;
    }
    let moving = bar.alpha != goal || bar.knob != knob_goal;
    // Wake up again when it's due to fade.
    if visible && !bar.dragging && !hovered && !bar.hold {
        if let Some(t) = bar.shown_at {
            let left = IDLE.saturating_sub(now.duration_since(t));
            let wake = core.loader.waker();
            std::thread::spawn(move || {
                std::thread::sleep(left + Duration::from_millis(20));
                wake();
            });
        }
    }
    if bar.alpha <= 0.0 {
        return moving;
    }

    let s = core.scale;
    let a = bar.alpha;
    let pal = theme::palette();
    let (w, h) = ((WIDTH * s).round(), (HEIGHT * s).round());
    let x = (core.size.0 as f32 - MARGIN * s - w).round();
    let y = ((core.layout.vp.top - h) / 2.0 - 2.0 * s).round();
    bar.pill = [x, y, w, h];
    let b = (BUTTON * s).round();
    bar.minus = [x + 1.0 * s, y + (h - b) / 2.0, b, b];
    bar.plus = [x + w - b - 1.0 * s, y + (h - b) / 2.0, b, b];
    let (t0, t1) = (bar.minus[0] + b + 4.0 * s, bar.plus[0] - 4.0 * s);
    bar.track = [t0, t1];
    let mid = y + h / 2.0;

    let out = &mut core.ui.base;
    // The pill: a raised surface with a hairline edge, readable over photos.
    let edge = s.round().max(1.0);
    out.quad(Quad::solid([x - edge, y - edge, w + 2.0 * edge, h + 2.0 * edge], rgb(pal.shade(pal.surface, 0.16), 0.9 * a), h / 2.0 + edge));
    out.quad(Quad::solid([x, y, w, h], rgb(pal.surface, 0.94 * a), h / 2.0));
    for (r, name) in [(bar.minus, "minus"), (bar.plus, "plus")] {
        let hot = contains(r, bar.cursor);
        if hot {
            out.quad(Quad::solid(r, rgb(pal.shade(pal.surface, 0.12), a), r[3] / 2.0));
        }
        let c = if hot { pal.fg } else { pal.fg_dim };
        out.icon_styled(name, [r[0] + r[2] / 2.0, r[1] + r[3] / 2.0], (15.0 * s).round(), rgb(c, a), IconStyle::weight(2.0));
    }
    // Track, a dot per level, the filled part and the knob.
    let th = (3.0 * s).round().max(2.0);
    out.quad(Quad::solid([t0, mid - th / 2.0, t1 - t0, th], rgb(pal.shade(pal.surface, 0.2), a), th / 2.0));
    let kx = t0 + (t1 - t0) * bar.knob;
    out.quad(Quad::solid([t0, mid - th / 2.0, kx - t0, th], rgb(pal.accent, a), th / 2.0));
    if (t1 - t0) / (n - 1) as f32 > 6.0 * s {
        let d = (2.0 * s).round().max(1.0);
        for i in 0..n {
            let dx = t0 + (t1 - t0) * i as f32 / (n - 1) as f32;
            let c = if dx <= kx { pal.on_accent } else { pal.muted };
            out.quad(Quad::solid([dx - d / 2.0, mid - d / 2.0, d, d], rgb(c, 0.7 * a), d / 2.0));
        }
    }
    let k = (if bar.dragging { 16.0 } else { 14.0 } * s).round();
    out.quad(Quad::solid([kx - k / 2.0 - edge, mid - k / 2.0 - edge, k + 2.0 * edge, k + 2.0 * edge], [0.0, 0.0, 0.0, 0.25 * a], k / 2.0 + edge));
    out.quad(Quad::solid([kx - k / 2.0, mid - k / 2.0, k, k], rgb(if pal.dark { pal.fg } else { 0xffffff }, a), k / 2.0));
    moving
}
