//! Small bits of app state remembered between runs (recent searches, the window's size):
//! `~/.config/lumen/state.json`, one top-level key per feature, written atomically.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::PathBuf;

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("lumen/state.json"))
}

fn read() -> Value {
    path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|v| v.is_object())
        .unwrap_or(json!({}))
}

pub fn get(key: &str) -> Value {
    read()[key].clone()
}

/// Stores `value` under `key`, keeping the other keys. Headless runs leave the file alone.
pub fn set(key: &str, value: Value) {
    if crate::settings::READ_ONLY.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Some(path) = path() else { return };
    let mut v = read();
    v[key] = value;
    let write = || -> Result<()> {
        let dir = path.parent().context("no config dir")?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".state.json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(&v)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    if let Err(e) = write() {
        log::warn!("saving {key}: {e:#}");
    }
}
