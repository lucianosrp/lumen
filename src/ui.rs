//! Immediate-mode drawing list for everything that isn't the photo grid.
//!
//! Each frame, features push rounded rects, images and text into a `Frame`; the renderer
//! draws `base` over the grid, then `overlay` (viewer), then `popup` on top of everything
//! (menus, sheets, toasts), so a popover's text never shows through from a layer below.
//! All coordinates are physical pixels, origin top-left.

pub type ImageId = u64;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Font {
    Sans,
}

#[derive(Clone, Debug)]
pub struct Quad {
    pub rect: [f32; 4],
    /// Straight-alpha sRGB colour; multiplies the image when there is one.
    pub color: [f32; 4],
    pub radius: f32,
    pub image: Option<ImageId>,
    /// Image sub-rectangle in 0..1 texture coordinates: [u0, v0, u1, v1].
    pub uv: [f32; 4],
    /// A vector icon to rasterise and draw (tinted by `color`), instead of `image`.
    pub icon: Option<crate::icons::IconKey>,
}

impl Quad {
    pub fn solid(rect: [f32; 4], color: [f32; 4], radius: f32) -> Self {
        Self { rect, color, radius, image: None, uv: [0.0, 0.0, 1.0, 1.0], icon: None }
    }

    pub fn image(rect: [f32; 4], image: ImageId, uv: [f32; 4], opacity: f32, radius: f32) -> Self {
        Self { rect, color: [1.0, 1.0, 1.0, opacity], radius, image: Some(image), uv, icon: None }
    }
}

#[derive(Clone, Debug)]
pub struct Text {
    pub text: String,
    /// Anchor point; `align` says whether x is the left edge, centre or right edge.
    pub x: f32,
    /// Top of the line box.
    pub y: f32,
    pub size: f32,
    pub weight: u16,
    pub color: [u8; 4],
    pub font: Font,
    pub align: Align,
    /// Wrap/ellipsis width; None for a single unbounded line.
    pub max_width: Option<f32>,
    /// Clip rectangle [x, y, w, h].
    pub clip: Option<[f32; 4]>,
}

impl Text {
    pub fn new(text: impl Into<String>, x: f32, y: f32, size: f32) -> Self {
        Self {
            text: text.into(),
            x,
            y,
            size,
            weight: 400,
            color: [255, 255, 255, 255],
            font: Font::Sans,
            align: Align::Left,
            max_width: None,
            clip: None,
        }
    }
    pub fn weight(mut self, w: u16) -> Self {
        self.weight = w;
        self
    }
    pub fn color(mut self, c: [u8; 4]) -> Self {
        self.color = c;
        self
    }
    pub fn align(mut self, a: Align) -> Self {
        self.align = a;
        self
    }
    pub fn max_width(mut self, w: f32) -> Self {
        self.max_width = Some(w);
        self
    }
    pub fn clip(mut self, r: [f32; 4]) -> Self {
        self.clip = Some(r);
        self
    }
}

#[derive(Default)]
pub struct Layer {
    pub quads: Vec<Quad>,
    pub texts: Vec<Text>,
}

impl Layer {
    pub fn quad(&mut self, q: Quad) {
        self.quads.push(q);
    }

    /// A vector icon centred on `center`, `size` physical px square, tinted `color`.
    pub fn icon(&mut self, name: &'static str, center: [f32; 2], size: f32, color: [f32; 4]) {
        self.icon_styled(name, center, size, color, crate::icons::IconStyle::default());
    }

    pub fn icon_styled(&mut self, name: &'static str, center: [f32; 2], size: f32, color: [f32; 4], style: crate::icons::IconStyle) {
        debug_assert!(crate::icons::exists(name), "unknown icon {name}");
        // Whole pixels: the raster is made at exactly this size, so it lands pixel-aligned.
        let px = size.round().max(1.0);
        let rect = [(center[0] - px / 2.0).round(), (center[1] - px / 2.0).round(), px, px];
        let key = crate::icons::IconKey { name, px: px as u16, weight: (style.weight * 100.0) as u16, filled: style.filled };
        self.quads.push(Quad { rect, color, radius: 0.0, image: None, uv: [0.0, 0.0, 1.0, 1.0], icon: Some(key) });
    }
    /// Clips the last quad to `clip`, adjusting its texture coordinates (for icons and images
    /// in scrolling lists); drops it when nothing is left.
    pub fn clip_last(&mut self, clip: [f32; 4]) {
        let Some(q) = self.quads.last_mut() else { return };
        let r = q.rect;
        let (x0, y0) = (r[0].max(clip[0]), r[1].max(clip[1]));
        let (x1, y1) = ((r[0] + r[2]).min(clip[0] + clip[2]), (r[1] + r[3]).min(clip[1] + clip[3]));
        if x1 <= x0 || y1 <= y0 {
            self.quads.pop();
            return;
        }
        let [u0, v0, u1, v1] = q.uv;
        let (fu, fv) = ((u1 - u0) / r[2], (v1 - v0) / r[3]);
        q.uv = [u0 + (x0 - r[0]) * fu, v0 + (y0 - r[1]) * fv, u0 + (x1 - r[0]) * fu, v0 + (y1 - r[1]) * fv];
        q.rect = [x0, y0, x1 - x0, y1 - y0];
    }

    pub fn text(&mut self, t: Text) {
        self.texts.push(t);
    }
    pub fn clear(&mut self) {
        self.quads.clear();
        self.texts.clear();
    }
}

#[derive(Default)]
pub struct Frame {
    pub base: Layer,
    pub overlay: Layer,
    pub popup: Layer,
}

impl Frame {
    pub fn clear(&mut self) {
        self.base.clear();
        self.overlay.clear();
        self.popup.clear();
    }
}

/// 0xRRGGBB plus alpha, as a straight-alpha sRGB colour.
pub fn rgb(hex: u32, a: f32) -> [f32; 4] {
    [((hex >> 16) & 255) as f32 / 255.0, ((hex >> 8) & 255) as f32 / 255.0, (hex & 255) as f32 / 255.0, a]
}

pub fn contains(r: [f32; 4], x: f32, y: f32) -> bool {
    x >= r[0] && y >= r[1] && x < r[0] + r[2] && y < r[1] + r[3]
}

/// Shared palette so features look like one app; live, see `crate::theme`.
pub mod theme {
    pub use crate::theme::*;
}
