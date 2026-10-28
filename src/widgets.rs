//! Small reusable controls for the immediate-mode UI: toggle switch, segmented control,
//! slider, button and macOS-style grouped cards.
//!
//! Each frame a feature builds a `Ctx` over its layer and calls the controls; they draw
//! themselves and report changes. Input is event-fed (`Input::press`/`release`) and consumed
//! once per frame (`Input::end_frame`). Animations live in `Ctx::anims`, keyed by id, and set
//! `Ctx::animating` while they move. Sizes are logical pixels, scaled by `Ctx::s`.

use std::collections::HashMap;

use crate::ui::{Align, Font, Layer, Quad, Text, contains, rgb, theme};
use crate::ui_render::UiRenderer;

/// Palette for controls on dark grouped cards (macOS dark appearance).
pub mod colors {
    pub const CARD: u32 = 0x1c1c1e;
    pub const SEPARATOR: u32 = 0x313134;
    pub const TRACK: u32 = 0x3a3a3d;
    pub const SEGMENTS: u32 = 0x2b2b2e;
    pub const SEGMENT_ON: u32 = 0x5c5c61;
    pub const SEGMENT_HOVER: u32 = 0x38383c;
    pub const BUTTON: u32 = 0x3a3a3d;
    pub const BUTTON_HOVER: u32 = 0x47474b;
    pub const BUTTON_DOWN: u32 = 0x2e2e31;
    pub const ACCENT_HOVER: u32 = 0x2e94ff;
    pub const ACCENT_DOWN: u32 = 0x0773e0;
    pub const RED: u32 = 0xff453a;
    pub const RED_DOWN: u32 = 0xd93a31;
    pub const GREEN: u32 = 0x30d158;
    pub const ORANGE: u32 = 0xff9f0a;
    pub const TEXT_SECONDARY: [u8; 4] = [235, 235, 245, 150];
}
use colors::*;

/// Mouse state as seen by controls. Physical pixels.
#[derive(Default)]
pub struct Input {
    pub cursor: (f32, f32),
    pub down: bool,
    /// Where the left button went down; cleared the frame after it's released.
    pub origin: Option<(f32, f32)>,
    /// Where it was released since the last frame.
    pub release: Option<(f32, f32)>,
}

impl Input {
    pub fn press(&mut self, at: (f32, f32)) {
        self.down = true;
        self.origin = Some(at);
        self.release = None;
    }

    pub fn release(&mut self, at: (f32, f32)) {
        self.down = false;
        if self.origin.is_some() {
            self.release = Some(at);
        }
    }

    /// Call once after drawing a frame, so a click fires exactly once.
    pub fn end_frame(&mut self) {
        if self.release.take().is_some() {
            self.origin = None;
        }
    }

    fn started_in(&self, r: [f32; 4]) -> bool {
        self.origin.is_some_and(|o| contains(r, o.0, o.1))
    }

    /// Under the cursor, and not while a press that started elsewhere is held.
    pub fn hovered(&self, r: [f32; 4]) -> bool {
        contains(r, self.cursor.0, self.cursor.1) && (!self.down || self.started_in(r))
    }

    pub fn pressing(&self, r: [f32; 4]) -> bool {
        self.down && self.started_in(r) && contains(r, self.cursor.0, self.cursor.1)
    }

    /// Pressed and released inside `r`.
    pub fn clicked(&self, r: [f32; 4]) -> bool {
        self.started_in(r) && self.release.is_some_and(|p| contains(r, p.0, p.1))
    }

    /// While a press that started in `r` is held (or was just released): the pointer position.
    pub fn dragging(&self, r: [f32; 4]) -> Option<(f32, f32)> {
        if !self.started_in(r) {
            return None;
        }
        if self.down { Some(self.cursor) } else { self.release }
    }

    /// True while any button is held (e.g. to defer saving until a slider is let go).
    pub fn busy(&self) -> bool {
        self.down
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    /// Red label on a neutral button.
    Destructive,
    /// Filled red, e.g. the "click again to confirm" state.
    DestructiveFilled,
}

pub struct Ctx<'a> {
    pub layer: &'a mut Layer,
    pub uir: &'a mut UiRenderer,
    pub input: &'a Input,
    pub anims: &'a mut HashMap<String, f32>,
    /// Physical pixels per logical pixel.
    pub s: f32,
    pub dt: f32,
    /// Texts are clipped to this rectangle.
    pub clip: [f32; 4],
    /// Set by controls whose animation hasn't settled; ask for another frame.
    pub animating: bool,
}

fn mix(a: u32, b: u32, t: f32) -> [f32; 4] {
    let (a, b) = (rgb(a, 1.0), rgb(b, 1.0));
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, 1.0]
}

fn dim(c: [f32; 4], enabled: bool) -> [f32; 4] {
    if enabled {
        return c;
    }
    let card = rgb(CARD, 1.0);
    [c[0] * 0.45 + card[0] * 0.55, c[1] * 0.45 + card[1] * 0.55, c[2] * 0.45 + card[2] * 0.55, c[3]]
}

fn dim_text(c: [u8; 4], enabled: bool) -> [u8; 4] {
    if enabled { c } else { [c[0], c[1], c[2], (c[3] as f32 * 0.4) as u8] }
}

impl Ctx<'_> {
    /// A single line of text vertically centred on `mid`.
    pub fn text(&mut self, text: impl Into<String>, x: f32, mid: f32, size: f32, weight: u16, color: [u8; 4], align: Align) {
        let size = size * self.s;
        let t = Text::new(text, x.round(), (mid - size * 0.625).round(), size).weight(weight).color(color).align(align);
        self.layer.text(t.clip(self.clip));
    }

    pub fn icon(&mut self, glyph: &str, x: f32, mid: f32, size: f32, color: [u8; 4]) {
        let size = size * self.s;
        let t = Text::new(glyph, x.round(), (mid - size * 0.625).round(), size).icon().color(color).align(Align::Center);
        self.layer.text(t.clip(self.clip));
    }

    /// Width in physical pixels of `text` at logical `size`.
    pub fn measure(&mut self, text: &str, size: f32, weight: u16) -> f32 {
        self.uir.measure(text, size * self.s, weight, Font::Sans)
    }

    pub fn quad(&mut self, rect: [f32; 4], color: [f32; 4], radius: f32) {
        self.layer.quad(Quad::solid(rect, color, radius));
    }

    /// Eases the value stored under `id` towards `goal`; starts at `goal` the first time.
    pub fn anim(&mut self, id: &str, goal: f32, speed: f32) -> f32 {
        let v = self.anims.entry(id.to_string()).or_insert(goal);
        *v += (goal - *v) * (1.0 - (-self.dt * speed).exp());
        if (goal - *v).abs() < 0.002 {
            *v = goal;
        } else {
            self.animating = true;
        }
        *v
    }

    /// Rounded tile with a white glyph, like System Settings' section icons.
    pub fn icon_tile(&mut self, x: f32, mid: f32, glyph: &str, color: u32) {
        let s = self.s;
        let size = 24.0 * s;
        self.quad([x, mid - size / 2.0, size, size], rgb(color, 1.0), 6.0 * s);
        self.icon(glyph, x + size / 2.0, mid, 13.0, [255, 255, 255, 255]);
    }

    /// On/off switch whose right edge is at `right`. Returns true when flipped.
    pub fn toggle(&mut self, id: &str, right: f32, mid: f32, on: &mut bool, enabled: bool) -> bool {
        let s = self.s;
        let (w, h) = (40.0 * s, 24.0 * s);
        let r = [right - w, mid - h / 2.0, w, h];
        let hit = [r[0] - 4.0 * s, r[1] - 4.0 * s, r[2] + 8.0 * s, r[3] + 8.0 * s];
        let clicked = enabled && self.input.clicked(hit);
        if clicked {
            *on = !*on;
        }
        let t = self.anim(id, if *on { 1.0 } else { 0.0 }, 18.0);
        let pressing = enabled && self.input.pressing(hit);
        let stretch = self.anim(&format!("{id}#press"), if pressing { 1.0 } else { 0.0 }, 24.0);
        let hover = enabled && self.input.hovered(hit);
        let off = if hover { 0x434347 } else { TRACK };
        let on_c = if hover { ACCENT_HOVER } else { theme::accent() };
        self.quad(r, dim(mix(off, on_c, t), enabled), h / 2.0);
        let pad = 2.0 * s;
        let k = h - pad * 2.0;
        let kw = k + 5.0 * s * stretch;
        let x = r[0] + pad + (w - pad * 2.0 - kw) * t;
        self.quad([x - 0.5 * s, r[1] + pad + 0.5 * s, kw + s, k + s], [0.0, 0.0, 0.0, 0.25], k / 2.0 + s);
        self.quad([x, r[1] + pad, kw, k], dim(rgb(0xffffff, 1.0), enabled), k / 2.0);
        clicked
    }

    /// Width of a segmented control with these labels.
    pub fn segmented_width(&mut self, options: &[&str]) -> f32 {
        let widest = options.iter().map(|o| self.measure(o, 12.5, 500)).fold(0.0, f32::max);
        (widest + 26.0 * self.s).max(64.0 * self.s) * options.len() as f32 + 4.0 * self.s
    }

    /// Mutually exclusive choice, right-aligned at `right`. Returns true when `selected` changed.
    pub fn segmented(&mut self, id: &str, right: f32, mid: f32, options: &[&str], selected: &mut usize) -> bool {
        let s = self.s;
        let w = self.segmented_width(options);
        let h = 26.0 * s;
        let r = [right - w, mid - h / 2.0, w, h];
        let pad = 2.0 * s;
        let seg = (w - pad * 2.0) / options.len() as f32;
        let mut changed = false;
        for i in 0..options.len() {
            let cell = [r[0] + pad + seg * i as f32, r[1], seg, h];
            if self.input.clicked(cell) && *selected != i {
                *selected = i;
                changed = true;
            }
        }
        self.quad(r, rgb(SEGMENTS, 1.0), 7.0 * s);
        let at = self.anim(id, *selected as f32, 20.0);
        for i in 0..options.len() {
            let cell = [r[0] + pad + seg * i as f32, r[1] + pad, seg, h - pad * 2.0];
            if i != *selected && self.input.hovered(cell) {
                let c = if self.input.pressing(cell) { 0x444448 } else { SEGMENT_HOVER };
                self.quad(cell, rgb(c, 1.0), 5.5 * s);
            }
        }
        // Hairlines between unselected neighbours.
        for i in 1..options.len() {
            if i != *selected && i != *selected + 1 {
                let x = r[0] + pad + seg * i as f32;
                self.quad([x - 0.5 * s, mid - 7.0 * s, s.max(1.0), 14.0 * s], rgb(0x48484c, 1.0), 0.0);
            }
        }
        let pill = [r[0] + pad + seg * at, r[1] + pad, seg, h - pad * 2.0];
        self.quad([pill[0], pill[1] + 0.5 * s, pill[2], pill[3]], [0.0, 0.0, 0.0, 0.3], 5.5 * s);
        self.quad(pill, rgb(SEGMENT_ON, 1.0), 5.5 * s);
        for (i, label) in options.iter().enumerate() {
            let cx = r[0] + pad + seg * (i as f32 + 0.5);
            let color = if i == *selected { [255, 255, 255, 255] } else { [255, 255, 255, 190] };
            self.text(*label, cx, mid, 12.5, if i == *selected { 600 } else { 500 }, color, Align::Center);
        }
        changed
    }

    /// Horizontal slider over `value` in 0..=1, `steps` > 1 snapping to that many positions.
    /// Returns true while the value changes.
    pub fn slider(&mut self, id: &str, x: f32, mid: f32, w: f32, value: &mut f32, steps: usize) -> bool {
        let s = self.s;
        let knob = 20.0 * s;
        let hit = [x - knob / 2.0, mid - knob, w + knob, knob * 2.0];
        let mut changed = false;
        if let Some((px, _)) = self.input.dragging(hit) {
            let mut v = ((px - x) / w).clamp(0.0, 1.0);
            if steps > 1 {
                let n = (steps - 1) as f32;
                v = (v * n).round() / n;
            }
            if (v - *value).abs() > 1e-4 {
                *value = v;
                changed = true;
            }
        }
        let held = self.input.dragging(hit).is_some() && self.input.down;
        // Follow the pointer exactly while dragging; glide on clicks and external changes.
        let shown = if held {
            self.anims.insert(id.to_string(), *value);
            *value
        } else {
            self.anim(id, *value, 22.0)
        };
        let th = 4.0 * s;
        self.quad([x, mid - th / 2.0, w, th], rgb(TRACK, 1.0), th / 2.0);
        if steps > 1 && steps <= 12 {
            for i in 0..steps {
                let tx = x + w * i as f32 / (steps - 1) as f32;
                let c = if (i as f32) / (steps - 1) as f32 <= shown + 1e-3 { rgb(0x5aa9ff, 1.0) } else { rgb(0x5a5a5f, 1.0) };
                self.quad([tx - 1.5 * s, mid + 7.0 * s, 3.0 * s, 3.0 * s], c, 1.5 * s);
            }
        }
        let kx = x + w * shown;
        self.quad([x, mid - th / 2.0, (kx - x).max(th), th], rgb(theme::accent(), 1.0), th / 2.0);
        let hover = self.input.hovered(hit);
        let grow = self.anim(&format!("{id}#hover"), if held { 1.0 } else if hover { 0.5 } else { 0.0 }, 20.0);
        let k = knob * (1.0 + 0.1 * grow);
        self.quad([kx - k / 2.0 - 0.5 * s, mid - k / 2.0 + 0.5 * s, k + s, k + s], [0.0, 0.0, 0.0, 0.3], k / 2.0 + s);
        self.quad([kx - k / 2.0, mid - k / 2.0, k, k], rgb(if held { 0xe6e6ea } else { 0xffffff }, 1.0), k / 2.0);
        changed
    }

    pub fn button_width(&mut self, label: &str) -> f32 {
        self.measure(label, 13.0, 500) + 28.0 * self.s
    }

    /// Push button with its left edge at `x`. Returns true when clicked.
    pub fn button(&mut self, x: f32, mid: f32, label: &str, kind: ButtonKind, enabled: bool) -> bool {
        let s = self.s;
        let (w, h) = (self.button_width(label), 28.0 * s);
        let r = [x, mid - h / 2.0, w, h];
        let clicked = enabled && self.input.clicked(r);
        let (hover, down) = (enabled && self.input.hovered(r), enabled && self.input.pressing(r));
        let (bg, fg) = match kind {
            ButtonKind::Primary => {
                (if down { ACCENT_DOWN } else if hover { ACCENT_HOVER } else { theme::accent() }, [255, 255, 255, 255])
            }
            ButtonKind::Secondary | ButtonKind::Destructive => {
                let bg = if down { BUTTON_DOWN } else if hover { BUTTON_HOVER } else { BUTTON };
                (bg, if kind == ButtonKind::Destructive { [255, 105, 97, 255] } else { [255, 255, 255, 240] })
            }
            ButtonKind::DestructiveFilled => (if down { RED_DOWN } else { RED }, [255, 255, 255, 255]),
        };
        self.quad(r, dim(rgb(bg, 1.0), enabled), 7.0 * s);
        self.text(label, r[0] + w / 2.0, mid, 13.0, 500, dim_text(fg, enabled), Align::Center);
        clicked
    }

    /// Same, right-aligned at `right`; returns (clicked, left edge).
    pub fn button_right(&mut self, right: f32, mid: f32, label: &str, kind: ButtonKind, enabled: bool) -> (bool, f32) {
        let x = right - self.button_width(label);
        (self.button(x, mid, label, kind, enabled), x)
    }

    /// Shortens `text` with an ellipsis to fit `max` physical pixels.
    pub fn ellipsize(&mut self, text: &str, size: f32, weight: u16, max: f32) -> String {
        if self.measure(text, size, weight) <= max {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        let (mut lo, mut hi) = (0, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let t: String = chars[..mid].iter().collect::<String>().trim_end().to_string() + "…";
            if self.measure(&t, size, weight) <= max { lo = mid } else { hi = mid - 1 }
        }
        chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
    }

    /// Title (and optional secondary line) at the left of a card row.
    pub fn row_label(&mut self, row: [f32; 4], title: &str, sub: Option<&str>, enabled: bool) {
        self.row_label_until(row, title, sub, enabled, row[0] + row[2]);
    }

    /// Same, ellipsized so it ends before `until` (the left edge of the row's control).
    pub fn row_label_until(&mut self, row: [f32; 4], title: &str, sub: Option<&str>, enabled: bool, until: f32) {
        let (x, mid) = (row[0] + 16.0 * self.s, row[1] + row[3] / 2.0);
        let max = until - x - 14.0 * self.s;
        let title = &self.ellipsize(title, 13.5, 500, max);
        let sub = sub.map(|t| self.ellipsize(t, 11.5, 400, max));
        let sub = sub.as_deref();
        let color = dim_text(theme::text(), enabled);
        match sub {
            Some(sub) => {
                self.text(title, x, mid - 8.5 * self.s, 13.5, 500, color, Align::Left);
                self.text(sub, x, mid + 9.5 * self.s, 11.5, 400, dim_text(TEXT_SECONDARY, enabled), Align::Left);
            }
            None => self.text(title, x, mid, 13.5, 500, color, Align::Left),
        }
    }
}

/// A rounded group of rows with inset hairline separators. Rows are laid out top to bottom;
/// the background is slipped underneath them when the card ends.
pub struct Card {
    x: f32,
    top: f32,
    w: f32,
    y: f32,
    rows: usize,
    first_quad: usize,
}

impl Card {
    pub fn begin(ctx: &Ctx, x: f32, y: f32, w: f32) -> Self {
        Self { x, top: y, w, y, rows: 0, first_quad: ctx.layer.quads.len() }
    }

    /// The next row's rectangle, `h` logical pixels tall.
    pub fn row(&mut self, ctx: &mut Ctx, h: f32) -> [f32; 4] {
        let s = ctx.s;
        if self.rows > 0 {
            let line = s.round().max(1.0);
            ctx.quad([self.x + 16.0 * s, self.y - line / 2.0, self.w - 16.0 * s, line], rgb(SEPARATOR, 1.0), 0.0);
        }
        self.rows += 1;
        let r = [self.x, self.y, self.w, h * s];
        self.y += h * s;
        r
    }

    /// Draws the background; returns the bottom edge.
    pub fn end(self, ctx: &mut Ctx) -> f32 {
        let bg = Quad::solid([self.x, self.top, self.w, self.y - self.top], rgb(CARD, 1.0), 10.0 * ctx.s);
        ctx.layer.quads.insert(self.first_quad, bg);
        self.y
    }
}
