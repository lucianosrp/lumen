//! Background work: syncing the library and streaming thumbnails into memory.
//!
//! The UI thread publishes a priority-ordered want list every frame; workers pop from its
//! front, so anything that scrolled out of view is simply never fetched.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use image::imageops::FilterType;

use crate::immich::Immich;
use crate::library::{self, Asset, Library};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tier {
    Small,
    Large,
}

impl Tier {
    pub const fn slot(self) -> u32 {
        match self {
            Tier::Small => 64,
            Tier::Large => 256,
        }
    }
    pub const MIPS: u32 = 4;
}

pub type Job = (usize, Tier);

pub struct Thumb {
    pub generation: u64,
    pub index: usize,
    pub tier: Tier,
    /// Square RGBA8 image per mip level, largest first.
    pub mips: Vec<Vec<u8>>,
}

pub enum Msg {
    Library { library: Arc<Library>, generation: u64 },
    Status(String),
    Thumb(Thumb),
}

#[derive(Clone)]
pub enum Source {
    Demo(usize),
    Immich(Immich),
}

#[derive(Default)]
struct Queue {
    generation: u64,
    library: Option<Arc<Library>>,
    want: VecDeque<Job>,
    inflight: HashSet<Job>,
    failed: HashSet<Job>,
}

struct Shared {
    queue: Mutex<Queue>,
    tx: Mutex<Sender<Msg>>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn send(&self, msg: Msg) {
        let _ = self.tx.lock().unwrap().send(msg);
        (self.wake)();
    }
}

pub struct Loader {
    pub rx: Receiver<Msg>,
    shared: Arc<Shared>,
    rt: tokio::runtime::Runtime,
    source: Source,
}

const WORKERS: usize = 24;

impl Loader {
    pub fn start(source: Source, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            queue: Mutex::default(),
            tx: Mutex::new(tx),
            wake: Box::new(wake),
        });
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.spawn(sync_library(source.clone(), shared.clone()));
        for _ in 0..WORKERS {
            rt.spawn(worker(source.clone(), shared.clone()));
        }
        let source = source.clone();
        Self { rx, shared, rt, source }
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    /// Runtime for feature-specific background work (fetch albums, previews, map tiles…).
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }

    /// Call from background work after sending a result, so the UI redraws.
    pub fn wake(&self) {
        (self.shared.wake)();
    }

    /// A clonable, Send waker for moving into spawned tasks.
    pub fn waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let shared = self.shared.clone();
        Arc::new(move || (shared.wake)())
    }

    /// Replaces the want list. Jobs already in flight or known to fail are skipped.
    pub fn set_wants(&self, generation: u64, wants: impl IntoIterator<Item = Job>) {
        let mut q = self.shared.queue.lock().unwrap();
        if q.generation != generation {
            return;
        }
        let Queue { want, inflight, failed, .. } = &mut *q;
        want.clear();
        want.extend(wants.into_iter().filter(|j| !inflight.contains(j) && !failed.contains(j)));
    }

    pub fn is_idle(&self) -> bool {
        let q = self.shared.queue.lock().unwrap();
        q.want.is_empty() && q.inflight.is_empty()
    }
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("lumen")
}

async fn sync_library(source: Source, shared: Arc<Shared>) {
    let publish = |assets: Vec<Asset>| {
        let shared = shared.clone();
        async move {
            let library = Arc::new(tokio::task::spawn_blocking(move || Library::new(assets)).await.unwrap());
            let generation = {
                let mut q = shared.queue.lock().unwrap();
                q.generation += 1;
                q.library = Some(library.clone());
                q.want.clear();
                q.failed.clear();
                q.generation
            };
            shared.send(Msg::Library { library, generation });
        }
    };

    match source {
        Source::Demo(n) => {
            shared.send(Msg::Status(format!("Generating {n} demo photos…")));
            let assets = tokio::task::spawn_blocking(move || library::demo_assets(n)).await.unwrap();
            publish(assets).await;
            shared.send(Msg::Status(String::new()));
        }
        Source::Immich(immich) => {
            // Show the last known library instantly, then refresh from the server.
            let key = format!("{:016x}", fxhash(immich.base()));
            let path = cache_dir().join(format!("library-{key}.json"));
            let cached: Option<Vec<Asset>> =
                std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
            if let Some(assets) = cached.clone() {
                publish(assets).await;
            } else {
                shared.send(Msg::Status(format!("Connecting to {}…", immich.base())));
            }
            let s = shared.clone();
            let progress = move |done: usize, total: usize| {
                s.send(Msg::Status(format!("Syncing {} of {} photos…", group(done), group(total))))
            };
            match immich.timeline(progress).await {
                Ok(mut assets) => {
                    assets.sort_by(|a, b| a.taken.cmp(&b.taken));
                    let changed = cached.as_ref().is_none_or(|c| *c != assets);
                    if changed {
                        let _ = std::fs::create_dir_all(cache_dir());
                        let _ = std::fs::write(&path, serde_json::to_vec(&assets).unwrap());
                        publish(assets).await;
                    }
                    shared.send(Msg::Status(String::new()));
                }
                Err(e) => {
                    log::error!("sync failed: {e:#}");
                    shared.send(Msg::Status(format!("Sync failed: {e:#}")));
                }
            }
        }
    }
}

async fn worker(source: Source, shared: Arc<Shared>) {
    loop {
        let job = {
            let mut q = shared.queue.lock().unwrap();
            match (q.want.pop_front(), q.library.clone()) {
                (Some(job), Some(lib)) => {
                    q.inflight.insert(job);
                    Some((job, lib, q.generation))
                }
                _ => None,
            }
        };
        let Some(((index, tier), lib, generation)) = job else {
            tokio::time::sleep(Duration::from_millis(8)).await;
            continue;
        };
        let result = load_thumb(&source, &lib.assets[index], index, tier).await;
        {
            let mut q = shared.queue.lock().unwrap();
            q.inflight.remove(&(index, tier));
            if result.is_err() && q.generation == generation {
                q.failed.insert((index, tier));
            }
        }
        match result {
            Ok(mips) => shared.send(Msg::Thumb(Thumb { generation, index, tier, mips })),
            Err(e) => log::warn!("thumbnail {}: {e:#}", lib.assets[index].id),
        }
    }
}

async fn load_thumb(source: &Source, asset: &Asset, index: usize, tier: Tier) -> Result<Vec<Vec<u8>>> {
    let image = match source {
        Source::Demo(_) => {
            // Pretend there's a network in between.
            tokio::time::sleep(Duration::from_millis(5 + (index % 7) as u64 * 6)).await;
            image::DynamicImage::ImageRgba8(library::demo_image(index as u64, 256))
        }
        Source::Immich(immich) => {
            let bytes = thumbnail_bytes(immich, &asset.id).await?;
            tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await??
        }
    };
    let mips = tokio::task::spawn_blocking(move || {
        let s = tier.slot();
        let mut level = image.resize_to_fill(s, s, FilterType::CatmullRom);
        let mut mips = Vec::with_capacity(Tier::MIPS as usize);
        for l in 0..Tier::MIPS {
            if l > 0 {
                level = level.resize_exact(s >> l, s >> l, FilterType::Triangle);
            }
            mips.push(level.to_rgba8().into_raw());
        }
        mips
    })
    .await?;
    Ok(mips)
}

/// An asset's ~250px thumbnail, from the shared disk cache or the server.
pub async fn thumbnail_bytes(immich: &Immich, id: &str) -> Result<Vec<u8>> {
    let path = cache_dir().join("thumbs").join(id);
    if let Ok(b) = tokio::fs::read(&path).await {
        return Ok(b);
    }
    let b = immich.thumbnail(id).await?;
    let _ = tokio::fs::create_dir_all(path.parent().unwrap()).await;
    let _ = tokio::fs::write(&path, &b).await;
    Ok(b)
}

fn fxhash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

pub fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
