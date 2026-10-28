//! Vector icons: Lucide outlines (ISC, see `assets/icons/LICENSE`) plus a Live Photo mark,
//! rasterised at the exact pixel size they're drawn at so strokes stay crisp and centred.
//!
//! Icons are white alpha masks tinted by the quad colour; `Layer::icon` queues them and the UI
//! renderer rasterises each (name, size, weight, fill) once.

use resvg::{tiny_skia, usvg};

/// Name → SVG source. Names are the file names in `assets/icons/`.
const ICONS: &[(&str, &str)] = &[
    ("aperture", include_str!("../assets/icons/aperture.svg")),
    ("archive", include_str!("../assets/icons/archive.svg")),
    ("archive-restore", include_str!("../assets/icons/archive-restore.svg")),
    ("book-image", include_str!("../assets/icons/book-image.svg")),
    ("calendar", include_str!("../assets/icons/calendar.svg")),
    ("camera", include_str!("../assets/icons/camera.svg")),
    ("check", include_str!("../assets/icons/check.svg")),
    ("chevron-left", include_str!("../assets/icons/chevron-left.svg")),
    ("chevron-right", include_str!("../assets/icons/chevron-right.svg")),
    ("circle-alert", include_str!("../assets/icons/circle-alert.svg")),
    ("circle-check", include_str!("../assets/icons/circle-check.svg")),
    ("cpu", include_str!("../assets/icons/cpu.svg")),
    ("ellipsis", include_str!("../assets/icons/ellipsis.svg")),
    ("film", include_str!("../assets/icons/film.svg")),
    ("gauge", include_str!("../assets/icons/gauge.svg")),
    ("hand", include_str!("../assets/icons/hand.svg")),
    ("hard-drive", include_str!("../assets/icons/hard-drive.svg")),
    ("heart", include_str!("../assets/icons/heart.svg")),
    ("heart-off", include_str!("../assets/icons/heart-off.svg")),
    ("image-plus", include_str!("../assets/icons/image-plus.svg")),
    ("images", include_str!("../assets/icons/images.svg")),
    ("info", include_str!("../assets/icons/info.svg")),
    ("layout-grid", include_str!("../assets/icons/layout-grid.svg")),
    ("live-photo", include_str!("../assets/icons/live-photo.svg")),
    ("loader-circle", include_str!("../assets/icons/loader-circle.svg")),
    ("map", include_str!("../assets/icons/map.svg")),
    ("map-pin", include_str!("../assets/icons/map-pin.svg")),
    ("maximize-2", include_str!("../assets/icons/maximize-2.svg")),
    ("minimize-2", include_str!("../assets/icons/minimize-2.svg")),
    ("palette", include_str!("../assets/icons/palette.svg")),
    ("pause", include_str!("../assets/icons/pause.svg")),
    ("play", include_str!("../assets/icons/play.svg")),
    ("plus", include_str!("../assets/icons/plus.svg")),
    ("rotate-ccw", include_str!("../assets/icons/rotate-ccw.svg")),
    ("search", include_str!("../assets/icons/search.svg")),
    ("server", include_str!("../assets/icons/server.svg")),
    ("settings", include_str!("../assets/icons/settings.svg")),
    ("share", include_str!("../assets/icons/share.svg")),
    ("sparkles", include_str!("../assets/icons/sparkles.svg")),
    ("trash-2", include_str!("../assets/icons/trash-2.svg")),
    ("triangle-alert", include_str!("../assets/icons/triangle-alert.svg")),
    ("undo-2", include_str!("../assets/icons/undo-2.svg")),
    ("users", include_str!("../assets/icons/users.svg")),
    ("video", include_str!("../assets/icons/video.svg")),
    ("volume-1", include_str!("../assets/icons/volume-1.svg")),
    ("volume-2", include_str!("../assets/icons/volume-2.svg")),
    ("volume-x", include_str!("../assets/icons/volume-x.svg")),
    ("x", include_str!("../assets/icons/x.svg")),
];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct IconKey {
    pub name: &'static str,
    /// Physical pixel size (square).
    pub px: u16,
    /// Stroke width ×100, in the 24-unit icon grid (Lucide's default is 2.0).
    pub weight: u16,
    pub filled: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct IconStyle {
    /// Stroke width in the 24-unit grid; lighter reads more like SF Symbols at large sizes.
    pub weight: f32,
    /// Fill closed shapes (a solid heart, play/pause).
    pub filled: bool,
}

impl Default for IconStyle {
    fn default() -> Self {
        Self { weight: 2.0, filled: false }
    }
}

impl IconStyle {
    pub fn filled() -> Self {
        Self { filled: true, ..Self::default() }
    }
    pub fn weight(w: f32) -> Self {
        Self { weight: w, ..Self::default() }
    }
}

pub fn exists(name: &str) -> bool {
    ICONS.iter().any(|(n, _)| *n == name)
}

/// White-on-transparent straight-alpha RGBA, `px`×`px`.
pub fn rasterize(key: IconKey) -> Option<Vec<u8>> {
    let (_, svg) = ICONS.iter().find(|(n, _)| *n == key.name)?;
    let weight = format!("stroke-width=\"{}\"", key.weight as f32 / 100.0);
    let mut svg = svg.replace("currentColor", "#ffffff").replacen("stroke-width=\"2\"", &weight, 1);
    if key.filled {
        svg = svg.replacen("fill=\"none\"", "fill=\"#ffffff\"", 1);
    }
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
    let px = key.px.max(1) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(px, px)?;
    let s = px as f32 / tree.size().width();
    resvg::render(&tree, tiny_skia::Transform::from_scale(s, s), &mut pixmap.as_mut());
    // Premultiplied white → straight white with the same coverage.
    Some(pixmap.data().chunks_exact(4).flat_map(|p| [255, 255, 255, p[3]]).collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn all_icons_rasterize() {
        for (name, _) in super::ICONS {
            let key = super::IconKey { name, px: 24, weight: 200, filled: false };
            let px = super::rasterize(key).unwrap_or_else(|| panic!("{name} failed"));
            assert!(px.chunks(4).any(|p| p[3] > 0), "{name} is empty");
        }
    }
}
