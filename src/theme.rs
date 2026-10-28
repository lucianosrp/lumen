//! The palette, following the user's Omarchy theme when there is one.
//!
//! Omarchy (and matugen, which regenerates it from the wallpaper) writes semantic colours to
//! `~/.local/state/omarchy/current/theme/colors.toml` (older installs: `~/.config/omarchy/...`).
//! We poll its mtime, so a theme or wallpaper change recolours the app within a second.
//! Without the file, a neutral Apple-like dark palette is used.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Grid and viewer background.
    pub bg: u32,
    /// Sidebar and panels.
    pub surface: u32,
    /// Cards, menus and hover rows above `surface`.
    pub raised: u32,
    pub separator: u32,
    pub fg: u32,
    pub fg_dim: u32,
    pub muted: u32,
    pub accent: u32,
    /// Readable text/icon colour on top of `accent`.
    pub on_accent: u32,
    pub selection: u32,
    pub red: u32,
    pub green: u32,
    /// Terminal hues (red, green, yellow, blue, magenta, cyan) for section tiles and charts.
    /// Under matugen these are tonal roles rather than true hues, but they belong to the theme.
    pub tints: [u32; 6],
}

impl Palette {
    pub const CLASSIC: Palette = Palette {
        dark: true,
        bg: 0x010101,
        surface: 0x161618,
        raised: 0x2a2a2e,
        separator: 0x2a2a2d,
        fg: 0xf2f2f7,
        fg_dim: 0xaeaeb2,
        muted: 0x8e8e93,
        accent: 0x0a84ff,
        on_accent: 0xffffff,
        selection: 0x0a84ff,
        red: 0xff453a,
        green: 0x30d158,
        tints: [0xff453a, 0x30d158, 0xff9f0a, 0x0a84ff, 0xbf5af2, 0x64d2ff],
    };

    /// Grouped-card background (Settings, sheets): a step up from `bg`.
    pub fn card(&self) -> u32 {
        if self.dark { self.surface } else { self.raised }
    }

    /// `base` moved towards the foreground by `t`: hover fills, tracks, hairlines.
    pub fn shade(&self, base: u32, t: f32) -> u32 {
        mix(base, self.fg, t)
    }
}

/// The grid header's gradient (and what leaving tiles fade to): the background itself, so
/// titles in `fg` stay readable over photos in light and dark themes.
pub fn scrim() -> u32 {
    palette().bg
}

/// Readable text/icon colour on a solid `bg`.
pub fn on(bg: u32) -> u32 {
    if luminance(bg) > 0.55 { 0x16171a } else { 0xffffff }
}

/// Colours for chrome drawn over photos and video (viewer bars, pills, HUDs). The viewer keeps a
/// dark surround in every theme, so this stays dark glass with light text even in light mode.
#[derive(Clone, Copy, Debug)]
pub struct Hud {
    /// The viewer's backdrop.
    pub backdrop: u32,
    /// Opaque-ish panels and bars over it.
    pub glass: u32,
    pub fg: u32,
    pub fg_dim: u32,
    /// The theme's red, or a bright one when the theme's is meant for light backgrounds.
    pub red: u32,
}

pub fn hud() -> Hud {
    let p = palette();
    if p.dark {
        Hud { backdrop: p.bg, glass: mix(p.surface, 0x000000, 0.15), fg: mix(p.fg, 0xffffff, 0.35), fg_dim: p.fg_dim, red: p.red }
    } else {
        Hud { backdrop: 0x0b0b0d, glass: 0x1c1c1e, fg: 0xf5f5f7, fg_dim: 0xc7c7cc, red: 0xff453a }
    }
}

static PALETTE: RwLock<Palette> = RwLock::new(Palette::CLASSIC);
static FOLLOW: AtomicBool = AtomicBool::new(true);
/// The palette in use came from an Omarchy theme file.
static OMARCHY: AtomicBool = AtomicBool::new(false);
/// Bumped on every change, so caches keyed on colour can notice.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn palette() -> Palette {
    *PALETTE.read().unwrap()
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// True while the colours come from Omarchy (false: classic, by choice or for want of a theme).
pub fn from_omarchy() -> bool {
    OMARCHY.load(Ordering::Relaxed)
}

/// The current Omarchy theme's name (`~/.local/state/omarchy/current/theme.name`), if any.
pub fn omarchy_name() -> Option<String> {
    if let Some(p) = std::env::var_os("LUMEN_THEME_FILE") {
        let p = PathBuf::from(p);
        return p.exists().then(|| p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    }
    let home = dirs::home_dir()?;
    [".local/state/omarchy/current/theme.name", ".config/omarchy/current/theme.name"]
        .into_iter()
        .find_map(|p| std::fs::read_to_string(home.join(p)).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| omarchy_path().map(|_| "Omarchy".to_string()))
}

/// Settings › Appearance: follow Omarchy, or keep the classic palette.
pub fn set_follow_omarchy(follow: bool) {
    if FOLLOW.swap(follow, Ordering::Relaxed) != follow {
        reload();
    }
}

pub fn omarchy_path() -> Option<PathBuf> {
    // For testing palettes without touching the real theme.
    if let Some(p) = std::env::var_os("LUMEN_THEME_FILE") {
        return Some(PathBuf::from(p));
    }
    let home = dirs::home_dir()?;
    [".local/state/omarchy/current/theme/colors.toml", ".config/omarchy/current/theme/colors.toml"]
        .into_iter()
        .map(|p| home.join(p))
        .find(|p| p.exists())
}

/// Re-reads the theme; returns true if the palette changed.
pub fn reload() -> bool {
    let theme = FOLLOW
        .load(Ordering::Relaxed)
        .then(|| omarchy_path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| parse(&s)))
        .flatten();
    OMARCHY.store(theme.is_some(), Ordering::Relaxed);
    let next = theme.unwrap_or(Palette::CLASSIC);
    let mut cur = PALETTE.write().unwrap();
    if *cur == next {
        return false;
    }
    *cur = next;
    GENERATION.fetch_add(1, Ordering::Relaxed);
    log::info!("theme: {}", if next == Palette::CLASSIC { "classic" } else { "omarchy" });
    true
}

/// Loads the theme now and keeps watching it; `wake` redraws the window after a change.
pub fn watch(wake: Arc<dyn Fn() + Send + Sync>) {
    reload();
    std::thread::Builder::new()
        .name("theme-watch".into())
        .spawn(move || {
            let mtime = || omarchy_path().and_then(|p| p.metadata().ok()?.modified().ok());
            let mut last: Option<SystemTime> = mtime();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let now = mtime();
                // matugen rewrites the file in place; also catch the symlink switching themes.
                if now != last {
                    last = now;
                    if reload() {
                        wake();
                    }
                }
            }
        })
        .ok();
}

fn parse(toml: &str) -> Option<Palette> {
    let get = |key: &str| -> Option<u32> {
        toml.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            if k.trim() != key {
                return None;
            }
            let v = v.trim().trim_matches('"').trim_start_matches('#');
            u32::from_str_radix(v.get(..6)?, 16).ok()
        })
    };
    let dark = !toml.lines().any(|l| l.replace(' ', "").starts_with("mode=\"light\""));
    let bg = get("background")?;
    let fg = get("foreground")?;
    let accent = get("accent").or(get("blue"))?;
    let darker = get("darker_background").unwrap_or(bg);
    let dark_bg = get("dark_background").unwrap_or(bg);
    let lighter = get("lighter_background").unwrap_or(mix(bg, fg, 0.12));
    Some(Palette {
        dark,
        // Photos look best on the deepest shade; panels sit one step up.
        bg: if dark { darker } else { bg },
        surface: if dark { dark_bg } else { darker },
        raised: lighter,
        separator: if dark { mix(lighter, bg, 0.35) } else { mix(bg, fg, 0.13) },
        fg,
        fg_dim: get("dark_foreground").unwrap_or(mix(fg, bg, 0.3)),
        muted: get("muted").unwrap_or(mix(fg, bg, 0.45)),
        accent,
        // A light accent needs dark text: the deepest background when dark, the text colour when light.
        on_accent: if luminance(accent) > 0.5 { if dark { darker } else { fg } } else { 0xffffff },
        selection: get("selection").unwrap_or(accent),
        red: get("red").unwrap_or(if dark { 0xff453a } else { 0xd70015 }),
        green: get("green").unwrap_or(if dark { 0x30d158 } else { 0x248a3d }),
        tints: {
            let keys = ["red", "green", "yellow", "blue", "magenta", "cyan"];
            std::array::from_fn(|i| get(keys[i]).unwrap_or(Palette::CLASSIC.tints[i]))
        },
    })
}

pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |c: u32, s: u32| ((c >> s) & 255) as f32;
    let m = |s: u32| ((ch(a, s) + (ch(b, s) - ch(a, s)) * t).round().clamp(0.0, 255.0) as u32) << s;
    m(16) | m(8) | m(0)
}

pub fn luminance(c: u32) -> f32 {
    let ch = |s: u32| ((c >> s) & 255) as f32 / 255.0;
    0.2126 * ch(16) + 0.7152 * ch(8) + 0.0722 * ch(0)
}

/// Palette colour as a straight-alpha sRGB quad colour.
pub fn rgba(hex: u32, a: f32) -> [f32; 4] {
    crate::ui::rgb(hex, a)
}

/// Palette colour as a text colour.
pub fn text_rgba(hex: u32, a: f32) -> [u8; 4] {
    [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8, (a.clamp(0.0, 1.0) * 255.0) as u8]
}

// Shorthands kept from the original constant palette (`ui::theme::*`).
pub fn bg() -> u32 {
    palette().bg
}
pub fn sidebar() -> u32 {
    palette().surface
}
pub fn separator() -> u32 {
    palette().separator
}
pub fn accent() -> u32 {
    palette().accent
}
// Dark text on light blends thinner (coverage is blended in linear light), so light themes
// get full opacity.
pub fn text() -> [u8; 4] {
    let p = palette();
    text_rgba(p.fg, if p.dark { 0.92 } else { 1.0 })
}
pub fn text_dim() -> [u8; 4] {
    let p = palette();
    text_rgba(p.fg_dim, if p.dark { 0.85 } else { 1.0 })
}
pub fn text_faint() -> [u8; 4] {
    let p = palette();
    text_rgba(p.muted, if p.dark { 0.8 } else { 1.0 })
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_omarchy_colors() {
        let p = super::parse(
            "mode = \"dark\"\naccent = \"#b5d17d\"\nbackground = \"#10131a\"\ndarker_background = \"#0b0e15\"\nforeground = \"#e0e2ec\"\n",
        )
        .unwrap();
        assert_eq!((p.accent, p.bg, p.fg), (0xb5d17d, 0x0b0e15, 0xe0e2ec));
        assert!(p.dark);
    }
}
