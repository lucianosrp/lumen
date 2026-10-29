//! Disk caches under `~/.cache/lumen/`: measuring, clearing, and keeping the thumbnail cache
//! under its size limit (least recently used first; a file's mtime is its last use).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use crate::loader::cache_dir;

/// Bytes; 0 = unlimited.
static LIMIT: AtomicU64 = AtomicU64::new(0);
/// Bytes currently on disk in `thumbs/`, or `UNKNOWN` until the first walk finishes.
static USED: AtomicU64 = AtomicU64::new(UNKNOWN);
const UNKNOWN: u64 = u64::MAX;
static MEASURING: AtomicBool = AtomicBool::new(false);
static TRIMMING: AtomicBool = AtomicBool::new(false);
/// Trims go a little below the limit so they don't run after every download.
const TRIM_TO: f64 = 0.9;

pub fn thumb_dir() -> PathBuf {
    cache_dir().join("thumbs")
}

pub fn tile_dir() -> PathBuf {
    cache_dir().join("tiles")
}

/// Library timelines and map markers: small JSON files at the top of the cache directory.
fn is_metadata(name: &str) -> bool {
    (name.starts_with("library-") || name.starts_with("map-markers-")) && name.ends_with(".json")
}

/// Files cost whole filesystem blocks; count that, like `du`.
pub fn on_disk(len: u64) -> u64 {
    len.next_multiple_of(4096)
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Usage {
    pub bytes: u64,
    pub files: u64,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Sizes {
    pub thumbs: Usage,
    pub metadata: Usage,
    pub tiles: Usage,
    /// Anything else in the cache directory.
    pub other: Usage,
}

impl Sizes {
    pub fn total(&self) -> u64 {
        self.thumbs.bytes + self.metadata.bytes + self.tiles.bytes + self.other.bytes
    }
}

fn walk(dir: &Path, out: &mut Vec<(PathBuf, SystemTime, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            walk(&e.path(), out);
        } else {
            out.push((e.path(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), on_disk(meta.len())));
        }
    }
}

fn usage(dir: &Path) -> Usage {
    let mut files = Vec::new();
    walk(dir, &mut files);
    Usage { bytes: files.iter().map(|f| f.2).sum(), files: files.len() as u64 }
}

/// Walks the whole cache. Blocking: call from a background thread.
pub fn measure() -> Sizes {
    let mut s = Sizes { thumbs: usage(&thumb_dir()), tiles: usage(&tile_dir()), ..Default::default() };
    if let Ok(entries) = std::fs::read_dir(cache_dir()) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                if name != "thumbs" && name != "tiles" {
                    let u = usage(&e.path());
                    s.other.bytes += u.bytes;
                    s.other.files += u.files;
                }
                continue;
            }
            let u = if is_metadata(&name) { &mut s.metadata } else { &mut s.other };
            u.bytes += on_disk(meta.len());
            u.files += 1;
        }
    }
    USED.store(s.thumbs.bytes, Ordering::Relaxed);
    s
}

/// Removes everything in `dir` (and the directory itself). Returns bytes freed.
fn clear_dir(dir: &Path) -> u64 {
    let freed = usage(dir).bytes;
    if let Err(e) = std::fs::remove_dir_all(dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("clearing {}: {e}", dir.display());
    }
    freed
}

pub fn clear_thumbs() -> u64 {
    let freed = clear_dir(&thumb_dir());
    USED.store(0, Ordering::Relaxed);
    log::info!("thumbnail cache cleared ({} MB)", freed >> 20);
    freed
}

pub fn clear_tiles() -> u64 {
    let freed = clear_dir(&tile_dir());
    log::info!("map tile cache cleared ({} MB)", freed >> 20);
    freed
}

pub fn clear_metadata() -> u64 {
    let mut freed = 0;
    if let Ok(entries) = std::fs::read_dir(cache_dir()) {
        for e in entries.flatten() {
            if is_metadata(&e.file_name().to_string_lossy())
                && let Ok(meta) = e.metadata()
                && std::fs::remove_file(e.path()).is_ok()
            {
                freed += on_disk(meta.len());
            }
        }
    }
    freed
}

pub fn clear_all() -> u64 {
    clear_thumbs() + clear_tiles() + clear_metadata()
}

/// Sets the thumbnail cache limit (0 = unlimited). The first call also measures the cache in
/// the background; both trim right away if it's over.
pub fn set_thumb_limit(bytes: u64) {
    LIMIT.store(bytes, Ordering::Relaxed);
    if USED.load(Ordering::Relaxed) == UNKNOWN {
        if !MEASURING.swap(true, Ordering::AcqRel) {
            std::thread::spawn(|| {
                USED.store(usage(&thumb_dir()).bytes, Ordering::Relaxed);
                MEASURING.store(false, Ordering::Release);
                maybe_trim();
            });
        }
    } else {
        maybe_trim();
    }
}

/// Whether background downloads may add `len` more bytes to the thumbnail cache without
/// making it trim (which would evict thumbnails to make room for others). Unlimited: always.
pub fn has_room(len: u64) -> bool {
    let (limit, used) = (LIMIT.load(Ordering::Relaxed), USED.load(Ordering::Relaxed));
    limit == 0 || (used != UNKNOWN && used + len < (limit as f64 * TRIM_TO * 0.95) as u64)
}

/// A thumbnail of `len` bytes was just written.
pub fn thumb_written(len: u64) {
    let used = USED.load(Ordering::Relaxed);
    if used != UNKNOWN {
        USED.fetch_add(on_disk(len), Ordering::Relaxed);
        maybe_trim();
    }
}

/// Marks a cached thumbnail as just used, for LRU trimming.
pub fn touch(path: &Path) {
    let _ = std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(SystemTime::now()));
}

pub fn is_trimming() -> bool {
    TRIMMING.load(Ordering::Relaxed)
}

fn maybe_trim() {
    let (limit, used) = (LIMIT.load(Ordering::Relaxed), USED.load(Ordering::Relaxed));
    if limit == 0 || used == UNKNOWN || used <= limit || TRIMMING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        let (before, after) = trim(limit);
        if before != after {
            log::info!("thumbnail cache trimmed {} → {} MB (limit {} MB)", before >> 20, after >> 20, limit >> 20);
        }
        TRIMMING.store(false, Ordering::Release);
    });
}

/// Deletes the least recently used thumbnails until the cache is under `TRIM_TO` of `limit`.
fn trim(limit: u64) -> (u64, u64) {
    let mut files = Vec::new();
    walk(&thumb_dir(), &mut files);
    files.sort_by_key(|f| f.1);
    let before: u64 = files.iter().map(|f| f.2).sum();
    let goal = (limit as f64 * TRIM_TO) as u64;
    let mut total = before;
    for (path, _, len) in &files {
        if total <= goal {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total -= len;
        }
    }
    USED.store(total, Ordering::Relaxed);
    (before, total)
}

/// "412 MB", "1.4 GB", "880 KB".
pub fn format_bytes(b: u64) -> String {
    let b = b as f64;
    const K: f64 = 1024.0;
    if b < K * K {
        format!("{:.0} KB", (b / K).max(if b > 0.0 { 1.0 } else { 0.0 }))
    } else if b < K * K * K {
        let mb = b / (K * K);
        if mb < 10.0 { format!("{mb:.1} MB") } else { format!("{mb:.0} MB") }
    } else if b < K * K * K * K {
        format!("{:.1} GB", b / (K * K * K))
    } else {
        format!("{:.2} TB", b / (K * K * K * K))
    }
}
