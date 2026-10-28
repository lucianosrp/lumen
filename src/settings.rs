//! User preferences, persisted to `~/.config/lumen/settings.json` and applied live.
//!
//! Every field has a default (`#[serde(default)]`), so files written by older or newer
//! versions keep loading. The server address and key stay in `config.json`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::Core;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThumbSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl ThumbSize {
    /// Column count the grid starts at when a collection opens.
    pub fn cols(self) -> u32 {
        match self {
            ThumbSize::Small => 13,
            ThumbSize::Medium => 9,
            ThumbSize::Large => 5,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// Smaller downloads: thumbnails switch up later and server previews are never used.
    Fast,
    #[default]
    Balanced,
    /// Sharper sooner: bigger thumbnails and ~1440px previews at smaller tile sizes.
    Sharp,
}

/// Tile edge (physical px) at which each thumbnail tier kicks in.
#[derive(Clone, Copy, Debug)]
pub struct Tiers {
    /// Below this, tiles show only their thumbhash.
    pub small: f32,
    /// 256px thumbnail instead of the 64px one.
    pub large: f32,
    /// The server's ~1440px preview.
    pub xl: f32,
}

impl Quality {
    pub fn tiers(self) -> Tiers {
        match self {
            Quality::Fast => Tiers { small: 18.0, large: 110.0, xl: f32::INFINITY },
            Quality::Balanced => Tiers { small: 18.0, large: 88.0, xl: 400.0 },
            Quality::Sharp => Tiers { small: 14.0, large: 60.0, xl: 260.0 },
        }
    }
}

/// How much video memory the thumbnail atlases take (see `render::atlas_dims`).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum GpuMemory {
    /// ~250 MB, for integrated graphics.
    Low,
    #[default]
    Normal,
    High,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    // Library / grid
    pub thumb_size: ThumbSize,
    pub gaps: bool,
    pub quality: Quality,
    // Scrolling & gestures
    /// Multiplier on touchpad and mouse-wheel scroll distance (0.5–2).
    pub scroll_speed: f32,
    /// Multiplier on how quickly tiles reflow mid-pinch (0.5–2, higher is quicker).
    pub pinch_speed: f32,
    /// Touchpad swipes coast after the fingers lift.
    pub momentum: bool,
    // Playback
    pub autoplay: bool,
    /// Clips shorter than 15 s loop, like Live Photos.
    pub loop_short: bool,
    /// Viewer volume videos start at (0–1).
    pub volume: f32,
    /// Grid hover playback of videos and Live Photos (wired by feat/hover).
    pub hover_play: bool,
    pub hover_sound: bool,
    // Storage
    /// Thumbnail disk cache limit in MB; 0 means unlimited.
    pub thumb_cache_mb: u32,
    // GPU
    pub gpu_memory: GpuMemory,
    // Appearance
    /// Colours follow the Omarchy theme (and wallpaper, via matugen); off keeps the classic palette.
    pub follow_omarchy: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            thumb_size: ThumbSize::Medium,
            gaps: true,
            quality: Quality::Balanced,
            scroll_speed: 1.0,
            pinch_speed: 1.0,
            momentum: true,
            autoplay: true,
            loop_short: true,
            volume: 1.0,
            hover_play: true,
            hover_sound: false,
            thumb_cache_mb: 2048,
            gpu_memory: GpuMemory::Normal,
            follow_omarchy: true,
        }
    }
}

/// Headless screenshots never write the file unless asked (`--save-settings`).
pub static READ_ONLY: AtomicBool = AtomicBool::new(false);

pub fn path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("lumen/settings.json")
}

impl Settings {
    /// The saved settings, or defaults if there are none (or they don't parse).
    pub fn load() -> Self {
        let p = path();
        match std::fs::read(&p) {
            Ok(b) => serde_json::from_slice::<Settings>(&b)
                .inspect_err(|e| log::warn!("{}: {e}; using defaults", p.display()))
                .unwrap_or_default()
                .sanitized(),
            Err(_) => Self::default(),
        }
    }

    /// Writes atomically: a temp file next to the real one, then a rename over it.
    pub fn save(&self) -> Result<()> {
        if READ_ONLY.load(Ordering::Relaxed) {
            return Ok(());
        }
        let p = path();
        let dir = p.parent().unwrap();
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".settings.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &p).with_context(|| format!("saving {}", p.display()))?;
        Ok(())
    }

    /// Clamps values a hand-edited file could get out of range.
    fn sanitized(mut self) -> Self {
        let fix = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
        self.scroll_speed = fix(self.scroll_speed, 0.5, 2.0, 1.0);
        self.pinch_speed = fix(self.pinch_speed, 0.5, 2.0, 1.0);
        self.volume = fix(self.volume, 0.0, 1.0, 1.0);
        self
    }

    /// `key=value` from the command line, e.g. `gaps=false`, `quality=fast`, `volume=0.5`.
    pub fn set(&mut self, kv: &str) -> Result<()> {
        let Some((key, value)) = kv.split_once('=') else { bail!("--set {kv}: expected key=value") };
        let mut json = serde_json::to_value(&*self)?;
        let obj = json.as_object_mut().unwrap();
        if !obj.contains_key(key) {
            let keys: Vec<&String> = obj.keys().collect();
            bail!("--set: unknown setting {key:?}; known: {keys:?}");
        }
        let v = match value {
            "true" | "on" => serde_json::Value::Bool(true),
            "false" | "off" => serde_json::Value::Bool(false),
            "unlimited" => serde_json::json!(0),
            v => v.parse::<f64>().map(|n| serde_json::json!(n)).unwrap_or_else(|_| v.to_lowercase().into()),
        };
        // Integers must stay integers for u32 fields.
        let v = match (&obj[key], &v) {
            (serde_json::Value::Number(old), serde_json::Value::Number(n)) if old.is_u64() => {
                serde_json::json!(n.as_f64().unwrap_or(0.0).max(0.0) as u64)
            }
            _ => v,
        };
        obj.insert(key.to_string(), v);
        *self = serde_json::from_value::<Settings>(json).with_context(|| format!("--set {kv}"))?.sanitized();
        Ok(())
    }

    pub fn tiers(&self) -> Tiers {
        self.quality.tiers()
    }

    pub fn thumb_cache_bytes(&self) -> u64 {
        self.thumb_cache_mb as u64 * (1 << 20)
    }
}

/// Pushes `core.settings` into everything that caches a copy. Cheap; call after any change.
/// Values read every frame (tiers, gaps, scroll speed, momentum) need nothing here.
pub fn apply(core: &mut Core) {
    let s = &core.settings;
    core.layout.default_cols = s.thumb_size.cols();
    core.layout.reflow_time = Duration::from_secs_f32(0.18 / s.pinch_speed.clamp(0.25, 4.0));
    // Headless runs keep videos still and silent regardless of preferences.
    if !core.viewer.silent {
        core.viewer.autoplay = s.autoplay;
    }
    core.viewer.set_default_volume(s.volume as f64);
    crate::video::LOOP_SHORT.store(s.loop_short, Ordering::Relaxed);
    crate::storage::set_thumb_limit(s.thumb_cache_bytes());
    core.renderer.set_gpu_memory(s.gpu_memory);
    crate::theme::set_follow_omarchy(s.follow_omarchy);
}
