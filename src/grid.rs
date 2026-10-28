//! Square-tile timeline layout with animated zoom between column counts.
//!
//! A zoom doesn't snap: the previous on-screen state is frozen into a `Spec::Blend` and every
//! tile interpolates from where it was to where it's going. Zooming again mid-animation nests
//! blends, so rapid pinches stay continuous.

use std::ops::Range;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Grid {
    pub cols: u32,
    pub scroll: f32,
}

#[derive(Clone)]
pub enum Spec {
    Grid(Grid),
    Blend(Box<Spec>, Box<Spec>, f32),
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

    fn range(&self, n: usize, vp: &Viewport, margin: f32) -> Range<usize> {
        let t = self.tile(vp);
        let first = ((self.scroll - margin) / t).floor().max(0.0) as usize;
        let last = ((self.scroll + vp.height + margin) / t).ceil().max(0.0) as usize;
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
        self.scroll = if h <= room {
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
        }
    }

    fn ranges(&self, n: usize, vp: &Viewport, margin: f32, out: &mut Vec<Range<usize>>) {
        match self {
            Spec::Grid(g) => out.push(g.range(n, vp, margin)),
            Spec::Blend(a, b, _) => {
                a.ranges(n, vp, margin, out);
                b.ranges(n, vp, margin, out);
            }
        }
    }

    fn scroll_by(&mut self, dy: f32) {
        match self {
            Spec::Grid(g) => g.scroll += dy,
            Spec::Blend(a, b, _) => {
                a.scroll_by(dy);
                b.scroll_by(dy);
            }
        }
    }

    /// Bounds nesting from rapid zooms by snapping the oldest blends to their midpoint target.
    fn prune(self, depth: u32) -> Spec {
        match self {
            Spec::Blend(_, b, _) if depth == 0 => *b,
            Spec::Blend(a, b, t) => Spec::Blend(Box::new(a.prune(depth - 1)), b, t),
            g => g,
        }
    }
}

pub struct Layout {
    pub vp: Viewport,
    pub n: usize,
    pub target: Grid,
    from: Option<(Spec, Instant)>,
    levels: Vec<u32>,
    pub level: usize,
}

const ZOOM_TIME: Duration = Duration::from_millis(340);
const LEVELS: [u32; 18] = [1, 3, 5, 7, 9, 13, 19, 27, 39, 55, 79, 113, 161, 229, 325, 461, 655, 931];

fn ease(t: f32) -> f32 {
    // Ease-out quint: quick to respond, soft landing.
    1.0 - (1.0 - t).powi(5)
}

impl Layout {
    pub fn new(vp: Viewport) -> Self {
        Self { vp, n: 0, target: Grid { cols: 5, scroll: 0.0 }, from: None, levels: vec![5], level: 0 }
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

    /// New library: start at the bottom (most recent), like Apple Photos.
    pub fn reset(&mut self, n: usize, keep_from_bottom: bool) {
        let from_bottom = self.target.content_height(self.n, &self.vp) - self.target.scroll;
        self.n = n;
        self.rebuild_levels();
        if self.n == 0 || !keep_from_bottom {
            self.level = self.nearest_level(9);
            self.target.cols = self.levels[self.level];
            self.target.scroll = f32::MAX;
        } else {
            self.target.scroll = self.target.content_height(n, &self.vp) - from_bottom;
        }
        self.target.clamp(n, &self.vp);
        self.from = None;
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
        if level == self.level || self.n == 0 {
            return;
        }
        let anchor = self.index_at(ax, ay);
        let current = self.current(now);
        self.level = level;
        self.target.cols = self.levels[level];
        if let Some((i, _, fy)) = anchor {
            let t = self.target.tile(&self.vp);
            self.target.scroll = (i as u32 / self.target.cols) as f32 * t + fy * t - ay;
        }
        self.target.clamp(self.n, &self.vp);
        self.from = Some((current.prune(3), now));
    }

    pub fn scroll_by(&mut self, dy: f32) {
        let before = self.target.scroll;
        self.target.scroll += dy;
        self.target.clamp(self.n, &self.vp);
        let applied = self.target.scroll - before;
        if let Some((from, _)) = &mut self.from {
            from.scroll_by(applied);
        }
    }

    pub fn scroll_to_fraction(&mut self, f: f32) {
        let h = self.target.content_height(self.n, &self.vp) - self.vp.height;
        self.scroll_by(f * h - self.target.scroll);
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.from.as_ref().is_some_and(|(_, t)| now.duration_since(*t) < ZOOM_TIME)
    }

    /// What's on screen right now.
    pub fn current(&self, now: Instant) -> Spec {
        match &self.from {
            Some((from, start)) if self.animating(now) => {
                let t = ease(now.duration_since(*start).as_secs_f32() / ZOOM_TIME.as_secs_f32());
                Spec::Blend(Box::new(from.clone()), Box::new(Spec::Grid(self.target)), t)
            }
            _ => Spec::Grid(self.target),
        }
    }

    /// Sorted, merged index ranges that may be visible in `spec`, `margin` px beyond the edges.
    pub fn visible(&self, spec: &Spec, margin: f32) -> Vec<Range<usize>> {
        let mut rs = Vec::new();
        spec.ranges(self.n, &self.vp, margin, &mut rs);
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
