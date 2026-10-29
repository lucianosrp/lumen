//! The account: the server's addresses (a local one, plus an optional fallback such as a
//! public domain) and the API key, kept in `~/.config/lumen/config.json` (mode 600).
//! `IMMICH_URL` / `IMMICH_FALLBACK_URL` / `IMMICH_API_KEY` override the file.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::immich::Immich;

#[derive(Default, Clone, PartialEq, Debug)]
pub struct Config {
    /// Tried first: usually the server's LAN address.
    pub url: String,
    /// Used when `url` doesn't answer, e.g. away from home.
    pub fallback_url: String,
    /// Whether to fall back to `fallback_url` at all (Settings › Server).
    pub use_fallback: bool,
    pub api_key: String,
}

#[derive(Deserialize)]
pub struct Me {
    pub name: String,
    pub email: String,
}

pub fn path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("lumen/config.json")
}

impl Config {
    /// What's saved, without environment overrides.
    pub fn saved() -> Self {
        let v: serde_json::Value =
            std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let get = |k: &str| v.get(k).and_then(|s| s.as_str()).unwrap_or_default().to_string();
        let use_fallback = v.get("use_fallback").and_then(|b| b.as_bool()).unwrap_or(true);
        Self { url: get("url"), fallback_url: get("fallback_url"), use_fallback, api_key: get("api_key") }
    }

    /// What to use: the file, with environment overrides.
    pub fn load() -> Self {
        let mut c = Self::saved();
        let env = |k: &str, v: &mut String| {
            if let Ok(e) = std::env::var(k) {
                *v = e;
            }
        };
        env("IMMICH_URL", &mut c.url);
        env("IMMICH_FALLBACK_URL", &mut c.fallback_url);
        env("IMMICH_API_KEY", &mut c.api_key);
        c
    }

    pub fn signed_in(&self) -> bool {
        !self.api_key.is_empty() && !self.url.is_empty()
    }

    /// Addresses in order of preference: the fallback only when it's switched on.
    pub fn urls(&self) -> Vec<&str> {
        let fallback = if self.use_fallback { self.fallback_url.as_str() } else { "" };
        [self.url.as_str(), fallback].into_iter().filter(|u| !u.trim().is_empty()).collect()
    }

    pub fn client(&self) -> Result<Immich> {
        Immich::new(&self.urls(), &self.api_key)
    }

    /// Writes the file, keeping keys it doesn't know about. Readable by the user only.
    pub fn save(&self) -> Result<()> {
        if crate::settings::READ_ONLY.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        let p = path();
        let mut v: serde_json::Value =
            std::fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}));
        let o = v.as_object_mut().context("config.json isn't an object")?;
        for (k, val) in [("url", &self.url), ("fallback_url", &self.fallback_url), ("api_key", &self.api_key)] {
            if val.is_empty() {
                o.remove(k);
            } else {
                o.insert(k.into(), val.trim().into());
            }
        }
        o.insert("use_fallback".into(), self.use_fallback.into());
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&v)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &p).with_context(|| format!("writing {}", p.display()))
    }
}

/// Checks a key against the server: finds which address answers, then asks who it belongs to.
/// Errors are short sentences for the sign-in screen.
pub async fn verify(config: &Config) -> Result<(Immich, Me), String> {
    let immich = config.client().map_err(|_| "Enter the server's address first".to_string())?;
    immich.connect().await.map_err(|_| match config.urls().as_slice() {
        [one] => format!("Can't reach {one}"),
        _ => "Can't reach the server at either address".to_string(),
    })?;
    match immich.get_json::<Me>("/users/me").await {
        Ok(me) => Ok((immich, me)),
        Err(e) => {
            let e = format!("{e:#}");
            Err(if e.contains("401") || e.contains("403") {
                "That key wasn't accepted".to_string()
            } else {
                format!("The server didn't answer properly ({})", e.rsplit(": ").next().unwrap_or(&e))
            })
        }
    }
}

/// Forgets the key (the addresses stay, so signing in again only needs a key) and the cached
/// timelines, which belong to this account. Thumbnails stay: they're only reachable by id.
pub fn sign_out() -> Result<()> {
    // Headless runs leave the real account alone.
    if crate::settings::READ_ONLY.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let mut c = Config::saved();
    c.api_key.clear();
    c.save()?;
    if let Ok(dir) = std::fs::read_dir(crate::loader::cache_dir()) {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("library-") && name.ends_with(".json") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}
