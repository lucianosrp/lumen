//! Square-tile timeline layout with animated zoom between column counts.
//!
//! A zoom doesn't snap: the previous on-screen state is frozen into a `Spec::Blend` and every
//! tile interpolates from where it was to where it's going. Zooming again mid-animation nests
//! blends, so rapid pinches stay continuous.
//!
//! A touchpad pinch scales the grid continuously around the fingers (`Spec::Scaled`); each time
//! the scale reaches the next column count the tiles reflow into it, and on release the grid
//! settles on the nearest level.

use std::ops::Range;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Grid {
    pub cols: u32,
    pub scroll: f32,
    /// Content shorter than the screen sits at the top (search results) instead of centred.
    pub top: bool,
}

#[derive(Clone)]
pub enum Spec {
    Grid(Grid),
    Blend(Box<Spec>, Box<Spec>, f32),
    /// Uniform scale around an anchor point (grid-local px).
    Scaled(Box<Spec>, [f32; 2], f32),
}

#[derive(Clone, Copy)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    /// Space reserved at the top for the translucent header.
    pub top: f32,
}

impl Grid {
    pub fn tile(&self, vp: &Viewport) -> f32 {
        vp.width / self.cols as f32
    }

    pub fn rect(&self, i: usize, vp: &Viewport) -> [f32; 4] {
        let t = self.tile(vp);
        let (row, col) = (i as u32 / self.cols, i as u32 % self.cols);
        [col as f32 * t, row as f32 * t - self.scroll, t, t]
    }

    /// Indices whose rows overlap screen rows `y0..y1`.
    fn range(&self, n: usize, vp: &Viewport, y0: f32, y1: f32) -> Range<usize> {
        let t = self.tile(vp);
        let first = ((self.scroll + y0) / t).floor().max(0.0) as usize;
        let last = ((self.scroll + y1) / t).ceil().max(0.0) as usize;
        let c = self.cols as usize;
        (first * c).min(n)..((last + 1) * c).min(n)
    }

    pub fn content_height(&self, n: usize, vp: &Viewport) -> f32 {
        (n as u32).div_ceil(self.cols) as f32 * self.tile(vp)
    }

    /// Valid scroll range; content shorter than the viewport is centred below the header.
    pub fn clamp(&mut self, n: usize, vp: &Viewport) {
        let h = self.content_height(n, vp);
        let room = vp.height - vp.top;
        self.scroll = if h <= room && self.top {
            -vp.top
        } else if h <= room {
            -vp.top - (room - h) / 2.0
        } else {
            self.scroll.clamp(-vp.top, h - vp.height)
        };
    }
}

impl Spec {
    pub fn rect(&self, i: usize, vp: &Viewport) -> [f32; 4] {
        match self {
            Spec::Grid(g) => g.rect(i, vp),
            Spec::Blend(a, b, t) => {
                let (a, b) = (a.rect(i, vp), b.rect(i, vp));
                std::array::from_fn(|k| a[k] + (b[k] - a[k]) * t)
            }
            Spec::Scaled(inner, [ax, ay], s) => {
                let r = inner.rect(i, vp);
                [ax + (r[0] - ax) * s, ay + (r[1] - ay) * s, r[2] * s, r[3] * s]
            }
        }
    }

    fn ranges(&self, n: usize, vp: &Viewport, y0: f32, y1: f32, out: &mut Vec<Range<usize>>) {
        match self {
            Spec::Grid(g) => out.push(g.range(n, vp, y0, y1)),
            Spec::Blend(a, b, _) => {
                a.ranges(n, vp, y0, y1, out);
                b.ranges(n, vp, y0, y1, out);
            }
            Spec::Scaled(inner, [_, ay], s) => inner.ranges(n, vp, ay + (y0 - ay) / s, ay + (y1 - ay) / s, out),
        }
    }

    fn scroll_by(&mut self, dy: f32) {
        match self {
            Spec::Grid(g) => g.scroll += dy,
            Spec::Blend(a, b, _) => {
                a.scroll_by(dy);
                b.scroll_by(dy);
            }
            Spec::Scaled(inner, _, s) => inner.scroll_by(dy / *s),
        }
    }

    /// Bounds nesting from rapid zooms by snapping the oldest blends to their midpoint target.
    fn prune(self, depth: u32) -> Spec {
        match self {
            Spec::Blend(_, b, _) if depth == 0 => *b,
            Spec::Blend(a, b, t) => Spec::Blend(Box::new(a.prune(depth - 1)), b, t),
            Spec::Scaled(inner, a, s) => Spec::Scaled(Box::new(inner.prune(depth)), a, s),
            g => g,
        }
    }
}

pub struct Layout {
    pub vp: Viewport,
    pub n: usize,
    pub target: Grid,
    /// Frozen on-screen state being blended away from, when it started, and how long it takes.
    from: Option<(Spec, Instant, Duration)>,
    /// Pinch scale when `from` was frozen mid-pinch, so the old layout keeps following the fingers.
    from_scale: Option<f32>,
    levels: Vec<u32>,
    pub level: usize,
    /// Active touchpad pinch: anchor (grid-local px) and scale relative to `target`.
    pinch: Option<([f32; 2], f32)>,
    /// Column count a newly opened collection starts at (Settings → Thumbnail size).
    pub default_cols: u32,
    /// How long a mid-pinch reflow takes (Settings → Pinch zoom speed).
    pub reflow_time: Duration,
    /// The zoom to open collections at: the last run's at first, then whatever the previous
    /// collection was left at, so switching (Library → Favorites → Library) keeps it.
    pub restore: Option<Zoom>,
    /// The level the current collection opened at: if it's still there when the collection
    /// is left, the zoom wasn't touched and `restore` stays as it was.
    opened_at: Option<usize>,
}

/// A zoom that survives window and library size changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Zoom {
    /// The whole library on one screen.
    Fit,
    Cols(u32),
}

const ZOOM_TIME: Duration = Duration::from_millis(340);
/// Reflows during a pinch are quicker: the fingers are already driving the motion.
pub const PINCH_REFLOW_TIME: Duration = Duration::from_millis(180);
const LEVELS: [u32; 18] = [1, 3, 5, 7, 9, 13, 19, 27, 39, 55, 79, 113, 161, 229, 325, 461, 655, 931];

fn ease(t: f32) -> f32 {
    // Ease-out quint: quick to respond, soft landing.
    1.0 - (1.0 - t).powi(5)
}

impl Layout {
    pub fn new(vp: Viewport) -> Self {
        Self { vp, n: 0, target: Grid { cols: 5, scroll: 0.0, top: false }, from: None, levels: vec![5], level: 0, pinch: None, from_scale: None, default_cols: 9, reflow_time: PINCH_REFLOW_TIME, restore: None, opened_at: None }
    }

    /// Columns at which the whole library fits on one screen.
    fn fit_cols(&self) -> u32 {
        let room = (self.vp.height - self.vp.top).max(1.0);
        let max = (self.vp.width / 1.5).max(1.0) as u32;
        let mut c = 1;
        while c < max && (self.n as u32).div_ceil(c) as f32 * (self.vp.width / c as f32) > room {
            c += 1.max(c / 64);
        }
        c
    }

    fn rebuild_levels(&mut self) {
        let fit = self.fit_cols();
        let min_tile_cols = (self.vp.width / 900.0).ceil() as u32; // never bigger than ~900px
        self.levels = LEVELS.iter().copied().filter(|&c| c >= min_tile_cols && c < fit).collect();
        if self.levels.last().is_none_or(|&l| (fit as f32) > l as f32 * 1.15) {
            self.levels.push(fit);
        }
    }

    fn nearest_level(&self, cols: u32) -> usize {
        (0..self.levels.len())
            .min_by_key(|&i| (self.levels[i] as i64 - cols as i64).abs())
            .unwrap_or(0)
    }

    pub fn levels(&self) -> usize {
        self.levels.len()
    }

    pub fn is_fit_all(&self) -> bool {
        self.level + 1 == self.levels.len()
    }

    /// The current zoom, to remember for next time.
    pub fn zoom_state(&self) -> Zoom {
        // Only the extra "whole collection" level counts as fit: in a small collection a
        // regular size can be the last level, and that shouldn't zoom the library all the way out.
        let extra = self.levels.last().is_some_and(|c| !LEVELS.contains(c));
        if self.is_fit_all() && extra { Zoom::Fit } else { Zoom::Cols(self.target.cols) }
    }

    /// New library: start at the bottom (most recent), like Apple Photos, or at the top when
    /// `from_top` (search results, best match first). Keeping the place holds the bottom edge
    /// still, or the top one when `from_top`.
    pub fn reset(&mut self, n: usize, keep_from_bottom: bool) {
        // The zoom being left behind carries over to whatever opens next, if it was changed
        // here (so passing through a small collection doesn't lose "fully zoomed out").
        if self.n > 0 && self.levels.len() > 1 && self.opened_at != Some(self.level) {
            self.restore = Some(self.zoom_state());
        }
        let from_bottom = self.target.content_height(self.n, &self.vp) - self.target.scroll;
        // Results arriving in an empty (still searching) view start fresh too.
        let fresh = self.target.top && self.n == 0;
        self.n = n;
        self.rebuild_levels();
        if self.n == 0 || !keep_from_bottom || fresh {
            self.level = match (self.n, self.restore) {
                (1.., Some(z)) => {
                    match z {
                        Zoom::Fit => self.levels.len() - 1,
                        Zoom::Cols(c) => self.nearest_level(c),
                    }
                }
                _ => self.nearest_level(self.default_cols),
            };
            self.opened_at = (self.n > 0).then_some(self.level);
            self.target.cols = self.levels[self.level];
            self.target.scroll = if self.target.top { f32::MIN } else { f32::MAX };
        } else if self.target.top {
            self.level = self.nearest_level(self.target.cols);
            self.target.cols = self.levels[self.level];
        } else {
            self.target.scroll = self.target.content_height(n, &self.vp) - from_bottom;
        }
        self.target.clamp(n, &self.vp);
        self.from = None;
    }

    /// Whether new collections start at the top; see `reset`.
    pub fn set_from_top(&mut self, top: bool) {
        self.target.top = top;
    }

    pub fn resize(&mut self, vp: Viewport) {
        // Keep the tile in the middle of the screen in place.
        let mid = self.index_at(vp.width / 2.0, self.vp.height / 2.0);
        self.vp = vp;
        let cols = self.target.cols;
        self.rebuild_levels();
        self.level = self.nearest_level(cols);
        self.target.cols = self.levels[self.level];
        if let Some((i, _, fy)) = mid {
            let t = self.target.tile(&vp);
            self.target.scroll = (i as u32 / self.target.cols) as f32 * t + fy * t - vp.height / 2.0;
        }
        self.target.clamp(self.n, &vp);
        self.from = None;
    }

    /// Asset under a point in the target layout, plus the fractional position inside it.
    pub fn index_at(&self, x: f32, y: f32) -> Option<(usize, f32, f32)> {
        if self.n == 0 {
            return None;
        }
        let t = self.target.tile(&self.vp);
        let row = ((y + self.target.scroll) / t).floor().max(0.0);
        let col = (x / t).floor().clamp(0.0, self.target.cols as f32 - 1.0);
        let i = (row as usize * self.target.cols as usize + col as usize).min(self.n - 1);
        Some((i, (x / t).fract(), ((y + self.target.scroll) / t - row).clamp(0.0, 1.0)))
    }

    pub fn zoom(&mut self, steps: i32, ax: f32, ay: f32, now: Instant) {
        let level = (self.level as i32 + steps).clamp(0, self.levels.len() as i32 - 1) as usize;
        self.zoom_to(level, ax, ay, now);
    }

    pub fn zoom_to(&mut self, level: usize, ax: f32, ay: f32, now: Instant) {
        if level == self.level || self.n == 0 || self.pinch.is_some() {
            return;
        }
        let current = self.current(now);
        self.set_level(level, ax, ay);
        self.freeze(current, now, ZOOM_TIME);
    }

    pub fn pinching(&self) -> bool {
        self.pinch.is_some()
    }

    pub fn pinch_begin(&mut self, ax: f32, ay: f32) {
        if self.n > 0 {
            self.pinch = Some(([ax, ay], 1.0));
        }
    }

    /// Fingers moved: `ratio` is the change in finger distance since the last update.
    pub fn pinch_update(&mut self, ratio: f32, now: Instant) {
        let Some(([ax, ay], mut s)) = self.pinch else { return };
        s *= ratio;
        // Each time the tiles reach the next level's size, reflow into it and keep going.
        while s > 1.0 && self.level > 0 && s >= self.step(self.level - 1) {
            s /= self.step(self.level - 1);
            self.reflow(self.level - 1, ax, ay, s, now);
        }
        while s < 1.0 && self.level + 1 < self.levels.len() && s <= 1.0 / self.step(self.level) {
            s *= self.step(self.level);
            self.reflow(self.level + 1, ax, ay, s, now);
        }
        // Rubber-band past the first and last levels.
        if self.level == 0 {
            s = s.min(1.4);
        }
        if self.level + 1 == self.levels.len() {
            s = s.max(0.8);
        }
        self.pinch = Some(([ax, ay], s));
    }

    /// Fingers lifted: settle on whichever level is nearest.
    pub fn pinch_end(&mut self, now: Instant) {
        let Some(([ax, ay], s)) = self.pinch else { return };
        let current = self.current(now);
        self.pinch = None;
        let ln = s.ln();
        let level = if self.level > 0 && ln > 0.5 * self.step(self.level - 1).ln() {
            self.level - 1
        } else if self.level + 1 < self.levels.len() && -ln > 0.5 * self.step(self.level).ln() {
            self.level + 1
        } else {
            self.level
        };
        if level != self.level {
            self.set_level(level, ax, ay);
        }
        self.freeze(current, now, ZOOM_TIME);
    }

    fn freeze(&mut self, current: Spec, now: Instant, duration: Duration) {
        self.from = Some((current.prune(3), now, duration));
        self.from_scale = None;
    }

    /// Size ratio between `level` and the next, more zoomed-out level.
    fn step(&self, level: usize) -> f32 {
        self.levels[level + 1] as f32 / self.levels[level] as f32
    }

    /// Mid-pinch switch to `level`, already scaled by `s` so tile sizes don't jump.
    fn reflow(&mut self, level: usize, ax: f32, ay: f32, s: f32, now: Instant) {
        let current = self.current(now);
        self.set_level(level, ax, ay);
        self.pinch = Some(([ax, ay], s));
        self.freeze(current, now, self.reflow_time);
        self.from_scale = Some(s);
    }

    /// Switches column count keeping the tile under (ax, ay) in place.
    fn set_level(&mut self, level: usize, ax: f32, ay: f32) {
        let anchor = self.index_at(ax, ay);
        self.level = level;
        self.target.cols = self.levels[level];
        if let Some((i, _, fy)) = anchor {
            let t = self.target.tile(&self.vp);
            self.target.scroll = (i as u32 / self.target.cols) as f32 * t + fy * t - ay;
        }
        self.target.clamp(self.n, &self.vp);
    }

    pub fn scroll_by(&mut self, dy: f32) {
        let before = self.target.scroll;
        self.target.scroll += dy;
        self.target.clamp(self.n, &self.vp);
        let applied = self.target.scroll - before;
        if let Some((from, _, _)) = &mut self.from {
            from.scroll_by(applied);
        }
    }

    pub fn scroll_to_fraction(&mut self, f: f32) {
        let h = self.target.content_height(self.n, &self.vp) - self.vp.height;
        self.scroll_by(f * h - self.target.scroll);
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.pinch.is_some() || self.blending(now)
    }

    fn blending(&self, now: Instant) -> bool {
        self.from.as_ref().is_some_and(|(_, t, d)| now.duration_since(*t) < *d)
    }

    /// What's on screen right now.
    pub fn current(&self, now: Instant) -> Spec {
        let target = match self.pinch {
            Some((anchor, s)) => Spec::Scaled(Box::new(Spec::Grid(self.target)), anchor, s),
            None => Spec::Grid(self.target),
        };
        match &self.from {
            Some((from, start, d)) if self.blending(now) => {
                let t = ease(now.duration_since(*start).as_secs_f32() / d.as_secs_f32());
                let from = match (self.pinch, self.from_scale) {
                    (Some((anchor, s)), Some(s0)) => Spec::Scaled(Box::new(from.clone()), anchor, s / s0),
                    _ => from.clone(),
                };
                Spec::Blend(Box::new(from), Box::new(target), t)
            }
            _ => target,
        }
    }

    /// Sorted, merged index ranges that may be visible in `spec`, `margin` px beyond the edges.
    pub fn visible(&self, spec: &Spec, margin: f32) -> Vec<Range<usize>> {
        let mut rs = Vec::new();
        spec.ranges(self.n, &self.vp, -margin, self.vp.height + margin, &mut rs);
        rs.sort_by_key(|r| r.start);
        let mut out: Vec<Range<usize>> = Vec::new();
        for r in rs.into_iter().filter(|r| !r.is_empty()) {
            match out.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => out.push(r),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_carries_across_collections() {
        let vp = Viewport { width: 1360.0, height: 1000.0, top: 76.0 };
        let now = Instant::now();
        let mut l = Layout::new(vp);
        l.default_cols = 9;
        l.reset(30_000, false);
        assert_eq!(l.target.cols, 9);
        // Fully out in the library, then Favorites (small) and back.
        l.zoom_to(l.levels() - 1, 0.0, 0.0, now);
        assert_eq!(l.zoom_state(), Zoom::Fit);
        l.reset(120, false);
        assert!(l.is_fit_all());
        l.reset(30_000, false);
        assert!(l.is_fit_all(), "library should still be fully zoomed out");
        // Zooming in a small collection carries that size back.
        l.reset(400, false);
        l.zoom(-1, 0.0, 0.0, now);
        let cols = l.target.cols;
        assert!(LEVELS.contains(&cols));
        l.reset(30_000, false);
        assert_eq!(l.target.cols, cols);
        // Untouched, a collection hands on what it was given.
        l.zoom_to(l.nearest_level(19), 0.0, 0.0, now);
        l.reset(5, false);
        l.reset(30_000, false);
        assert_eq!(l.target.cols, 19);
    }
}
