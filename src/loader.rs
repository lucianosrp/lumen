//! Background work: syncing the library and streaming thumbnails into memory.
//!
//! The UI thread publishes a priority-ordered want list every frame; workers pop from its
//! front, so anything that scrolled out of view is simply never fetched.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use image::imageops::FilterType;

use crate::immich::Immich;
use crate::library::{self, Asset, Library};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tier {
    Small,
    Large,
    /// Server preview (~1440px), for the closest grid zoom. Not disk-cached.
    Xl,
}

impl Tier {
    pub const fn slot(self) -> u32 {
        match self {
            Tier::Small => 64,
            Tier::Large => 256,
            Tier::Xl => 1024,
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
    /// Came from the disk cache: shown at once, without fading in.
    pub cached: bool,
}

pub enum Msg {
    Library { library: Arc<Library>, generation: u64, collection: Arc<Collection> },
    Status(String),
    Thumb(Thumb),
}

#[derive(Clone)]
pub enum Source {
    Demo(usize),
    Immich(Immich),
}

/// Which assets a collection shows on top of what its query returns.
#[derive(Clone, Debug)]
pub enum Filter {
    All,
    VideosOnly,
    Ids(Arc<HashSet<String>>),
}

/// A filtered timeline the grid can show: the library, an album, a person…
#[derive(Clone, Debug)]
pub struct Collection {
    /// Stable identity, e.g. "library", "favorites", "album:<id>".
    pub key: String,
    pub title: String,
    /// Query string for the `/timeline/bucket(s)` endpoints.
    pub query: String,
    pub filter: Filter,
    /// Assets given up front (search results) instead of a server timeline; shown as they are.
    pub fixed: Option<Arc<Vec<Asset>>>,
    /// Keep `fixed`'s order (best match first) and start the grid at the top.
    pub ordered: bool,
}

const LIBRARY_QUERY: &str = "visibility=timeline";
const FAVORITES_QUERY: &str = "visibility=timeline&isFavorite=true";
const ARCHIVE_QUERY: &str = "visibility=archive";
const TRASH_QUERY: &str = "isTrashed=true";
/// Timelines whose membership follows from an asset's favorite/archive/trash state alone.
const KNOWN_QUERIES: [&str; 4] = [LIBRARY_QUERY, FAVORITES_QUERY, ARCHIVE_QUERY, TRASH_QUERY];

impl Collection {
    fn new(key: impl Into<String>, title: impl Into<String>, query: impl Into<String>, filter: Filter) -> Self {
        Self { key: key.into(), title: title.into(), query: query.into(), filter, fixed: None, ordered: false }
    }
    /// Search results: exactly these assets, in this order, grid starting at the top.
    pub fn results(key: &str, title: &str, assets: Vec<Asset>) -> Self {
        Self { fixed: Some(Arc::new(assets)), ordered: true, ..Self::new(key, title, "", Filter::All) }
    }
    pub fn library() -> Self {
        Self::new("library", "Library", LIBRARY_QUERY, Filter::All)
    }
    pub fn favorites() -> Self {
        Self::new("favorites", "Favorites", FAVORITES_QUERY, Filter::All)
    }
    pub fn videos() -> Self {
        Self::new("videos", "Videos", LIBRARY_QUERY, Filter::VideosOnly)
    }
    pub fn archive() -> Self {
        Self::new("archive", "Archive", ARCHIVE_QUERY, Filter::All)
    }
    pub fn trash() -> Self {
        Self::new("trash", "Recently Deleted", TRASH_QUERY, Filter::All)
    }
    pub fn album(id: &str, name: &str) -> Self {
        Self::new(format!("album:{id}"), name, format!("albumId={id}"), Filter::All)
    }
    pub fn person(id: &str, name: &str) -> Self {
        Self::new(format!("person:{id}"), name, format!("personId={id}"), Filter::All)
    }
    /// A hand-picked set of Library assets (e.g. everything inside a map region).
    #[allow(dead_code)]
    pub fn ids(key: &str, title: &str, ids: HashSet<String>) -> Self {
        Self::new(key, title, LIBRARY_QUERY, Filter::Ids(Arc::new(ids)))
    }

    /// Whether this collection's filter lets `a` in (its query is the server's business).
    pub fn admits(&self, a: &Asset) -> bool {
        match &self.filter {
            Filter::All => true,
            Filter::VideosOnly => a.video,
            Filter::Ids(ids) => ids.contains(&a.id),
        }
    }

    fn apply(&self, assets: &[Asset]) -> Vec<Asset> {
        match &self.filter {
            Filter::All => assets.to_vec(),
            Filter::VideosOnly => assets.iter().filter(|a| a.video).cloned().collect(),
            Filter::Ids(ids) => assets.iter().filter(|a| ids.contains(&a.id)).cloned().collect(),
        }
    }
}

#[derive(Default)]
struct Queue {
    /// Bumped by every `open`; a sync only publishes while it is still the latest request.
    request: u64,
    syncing: bool,
    generation: u64,
    library: Option<Arc<Library>>,
    want: VecDeque<Job>,
    inflight: HashSet<Job>,
    failed: HashSet<Job>,
}

struct Shared {
    queue: Mutex<Queue>,
    /// Timelines synced this session, by query, so filtered views and quick revisits skip the network.
    fresh: Mutex<HashMap<String, (Instant, Arc<Vec<Asset>>)>>,
    tx: Mutex<Sender<Msg>>,
    wake: Box<dyn Fn() + Send + Sync>,
    /// Cache patches still writing.
    patching: std::sync::atomic::AtomicUsize,
    /// The background download of every thumbnail in the library.
    prefetch: Mutex<Option<tokio::task::AbortHandle>>,
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
    sync: Mutex<Option<tokio::task::AbortHandle>>,
}

const WORKERS: usize = 24;
/// How long a synced timeline is reused before `open` asks the server again.
const FRESH_FOR: Duration = Duration::from_secs(120);

impl Loader {
    pub fn start(source: Source, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            queue: Mutex::default(),
            fresh: Mutex::default(),
            tx: Mutex::new(tx),
            wake: Box::new(wake),
            patching: Default::default(),
            prefetch: Mutex::new(None),
        });
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        for _ in 0..WORKERS {
            rt.spawn(worker(source.clone(), shared.clone()));
        }
        let loader = Self { rx, shared, rt, source, sync: Mutex::new(None) };
        loader.open(Collection::library());
        loader
    }

    /// Re-syncs `collection` from the server even if it was fetched recently (the server told
    /// us something changed). Any collection may be affected, so every memo goes.
    pub fn refresh(&self, collection: Collection) {
        // Search results aren't a server timeline; local edits already keep them current.
        if collection.fixed.is_some() {
            return;
        }
        self.shared.fresh.lock().unwrap().clear();
        // Something changed, maybe in an old month without changing its count (a favorite
        // set elsewhere): check every month, quietly.
        self.begin(collection, true);
    }

    /// Switches the grid to `collection`: cached version first, then a refresh. Supersedes
    /// any sync still running for an earlier `open`.
    pub fn open(&self, collection: Collection) {
        self.begin(collection, false);
    }

    /// `full`: fetch every month rather than only those whose counts changed.
    fn begin(&self, collection: Collection, full: bool) {
        let request = {
            let mut q = self.shared.queue.lock().unwrap();
            q.request += 1;
            q.syncing = true;
            q.request
        };
        let task = self.rt.spawn(sync(self.source.clone(), self.shared.clone(), Arc::new(collection), request, full));
        if let Some(old) = self.sync.lock().unwrap().replace(task.abort_handle()) {
            old.abort();
        }
    }

    /// Forgets `collection`'s cached timeline (in memory and on disk) and syncs it again
    /// from the server, reporting progress through the status line.
    pub fn reload(&self, collection: Collection) {
        self.shared.fresh.lock().unwrap().remove(&collection.query);
        if let Source::Immich(immich) = &self.source {
            let _ = std::fs::remove_file(library_cache_path(immich, &collection.query));
        }
        self.open(collection);
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    /// Runtime for feature-specific background work (fetch albums, previews, map tiles…).
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
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

    /// Makes a locally edited library (see `Library::edit`) current, as if a sync had published
    /// it, and returns its generation. Old want lists and failures referred to old indices.
    pub fn adopt(&self, library: Arc<Library>) -> u64 {
        let mut q = self.shared.queue.lock().unwrap();
        q.generation += 1;
        q.library = Some(library);
        q.want.clear();
        q.failed.clear();
        q.generation
    }

    /// Applies a confirmed change to every cached timeline: `edit` gets each one's query and
    /// assets (in memory and on disk) and says what to do with them. Forgotten timelines are
    /// asked of the server on their next open; their disk copy only shows until that sync lands.
    /// Runs in the background: the library's cache file is megabytes of JSON.
    pub fn patch_caches(&self, edit: impl Fn(&str, &[Asset]) -> Patch + Send + 'static) {
        use std::sync::atomic::Ordering::SeqCst;
        let (shared, source) = (self.shared.clone(), self.source.clone());
        shared.patching.fetch_add(1, SeqCst);
        self.rt.spawn_blocking(move || {
            patch_caches(&shared, &source, edit);
            shared.patching.fetch_sub(1, SeqCst);
        });
    }
}

fn patch_caches(shared: &Shared, source: &Source, edit: impl Fn(&str, &[Asset]) -> Patch) {
    let mut fresh = shared.fresh.lock().unwrap();
    let mut queries: Vec<String> = fresh.keys().cloned().collect();
    for q in KNOWN_QUERIES {
        if !queries.iter().any(|k| k == q) {
            queries.push(q.to_string());
        }
    }
    for query in queries {
        let path = match source {
            Source::Immich(immich) => Some(library_cache_path(immich, &query)),
            Source::Demo(_) => None,
        };
        let mem = fresh.get(&query).map(|f| f.1.clone());
        let disk = || -> Option<Vec<Asset>> { serde_json::from_slice(&std::fs::read(path.as_ref()?).ok()?).ok() };
        let Some(current) = mem.as_deref().cloned().or_else(disk) else { continue };
        match edit(&query, &current) {
            Patch::Keep => {}
            Patch::Replace(mut assets) => {
                assets.sort_by(|a, b| a.taken.cmp(&b.taken));
                if let Some(path) = &path
                    && path.exists()
                {
                    let _ = std::fs::write(path, serde_json::to_vec(&assets).unwrap());
                }
                if mem.is_some() {
                    fresh.insert(query, (Instant::now(), Arc::new(assets)));
                }
            }
            Patch::Forget => {
                fresh.remove(&query);
            }
        }
    }
}

/// What `Loader::patch_caches` does with one cached timeline.
pub enum Patch {
    Keep,
    Forget,
    Replace(Vec<Asset>),
}

impl Loader {
    pub fn is_idle(&self) -> bool {
        let q = self.shared.queue.lock().unwrap();
        q.want.is_empty() && q.inflight.is_empty() && !q.syncing && self.shared.patching.load(std::sync::atomic::Ordering::SeqCst) == 0
    }
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("lumen")
}

impl Shared {
    fn current(&self, request: u64) -> bool {
        self.queue.lock().unwrap().request == request
    }

    fn status(&self, request: u64, s: String) {
        if self.current(request) {
            self.send(Msg::Status(s));
        }
    }
}

async fn publish(shared: &Arc<Shared>, collection: &Arc<Collection>, request: u64, assets: Arc<Vec<Asset>>) {
    let c = collection.clone();
    let build = move || if c.ordered { Library::ordered(c.apply(&assets), true) } else { Library::new(c.apply(&assets)) };
    let library = Arc::new(tokio::task::spawn_blocking(build).await.unwrap());
    let generation = {
        let mut q = shared.queue.lock().unwrap();
        if q.request != request {
            return;
        }
        q.generation += 1;
        q.library = Some(library.clone());
        q.want.clear();
        q.failed.clear();
        q.generation
    };
    shared.send(Msg::Library { library, generation, collection: collection.clone() });
}

async fn sync(source: Source, shared: Arc<Shared>, collection: Arc<Collection>, request: u64, full: bool) {
    let query = collection.query.clone();
    if let Some(assets) = collection.fixed.clone() {
        publish(&shared, &collection, request, assets).await;
        let mut q = shared.queue.lock().unwrap();
        if q.request == request {
            q.syncing = false;
            drop(q);
            shared.send(Msg::Status(String::new()));
        }
        return;
    }
    let fresh = shared.fresh.lock().unwrap().get(&query).filter(|(t, _)| t.elapsed() < FRESH_FOR).map(|f| f.1.clone());
    let mut ok = true;
    if let Some(assets) = fresh {
        publish(&shared, &collection, request, assets).await;
    } else {
        match &source {
            Source::Demo(n) => {
                let n = if query == LIBRARY_QUERY { *n } else { 0 };
                shared.status(request, format!("Generating {n} demo photos…"));
                let assets = Arc::new(tokio::task::spawn_blocking(move || library::demo_assets(n)).await.unwrap());
                shared.fresh.lock().unwrap().insert(query, (Instant::now(), assets.clone()));
                publish(&shared, &collection, request, assets).await;
            }
            Source::Immich(immich) => ok = sync_immich(immich, &shared, &collection, request, full).await,
        }
    }
    let mut q = shared.queue.lock().unwrap();
    if q.request == request {
        q.syncing = false;
        drop(q);
        if ok {
            shared.send(Msg::Status(String::new()));
        }
    }
}

/// Which assets each month of a cached timeline held, so a later sync only fetches the
/// months whose counts changed. Kept next to the timeline's cache file.
#[derive(Serialize, Deserialize, Default)]
struct Index {
    /// Unix seconds of the last sync that fetched every month.
    full_at: u64,
    /// Month → (count the server reported, asset ids).
    buckets: HashMap<String, (usize, Vec<String>)>,
}

/// Every month is fetched again at least this often, catching edits that kept counts the same.
const FULL_EVERY: Duration = Duration::from_secs(24 * 3600);

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Returns false if the refresh failed (the error stays in the status line).
///
/// The cached timeline shows at once. Then the server's month counts (one request) say which
/// months changed; only those, plus the newest, are fetched again. With a cache this all
/// happens without a word in the status line.
async fn sync_immich(immich: &Immich, shared: &Arc<Shared>, collection: &Arc<Collection>, request: u64, full: bool) -> bool {
    let query = &collection.query;
    let path = library_cache_path(immich, query);
    let index_path = path.with_extension("index.json");
    let cached: Option<Arc<Vec<Asset>>> =
        std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).map(Arc::new);
    let index: Option<Index> = cached
        .as_ref()
        .and_then(|_| serde_json::from_slice(&std::fs::read(&index_path).ok()?).ok());
    if let Some(assets) = cached.clone() {
        publish(shared, collection, request, assets).await;
    } else if query == LIBRARY_QUERY {
        shared.status(request, format!("Connecting to {}…", immich.base()));
    } else {
        shared.status(request, format!("Loading {}…", collection.title));
    }
    let quiet = cached.is_some();
    let s = shared.clone();
    let progress = move |done: usize, total: usize| {
        if !quiet {
            s.status(request, format!("Syncing {} of {} photos…", group(done), group(total)))
        }
    };

    let result = async {
        let server = immich.buckets(query).await?;
        let stale = full || index.as_ref().is_none_or(|i| unix_now().saturating_sub(i.full_at) > FULL_EVERY.as_secs());
        let (Some(cached), Some(old), false) = (cached.as_ref(), index.as_ref(), stale) else {
            // Everything, from scratch.
            let months = immich.bucket_assets(query, &server, progress).await?;
            let mut idx = Index { full_at: unix_now(), buckets: HashMap::new() };
            let mut assets = Vec::new();
            for (name, list) in months {
                let count = server.iter().find(|b| b.0 == name).map_or(list.len(), |b| b.1);
                idx.buckets.insert(name, (count, list.iter().map(|a| a.id.clone()).collect()));
                assets.extend(list);
            }
            return anyhow::Ok((assets, idx));
        };
        // Months that are new or changed size, and always the newest (recent edits).
        let newest = server.first().map(|b| b.0.clone());
        let changed: Vec<(String, usize)> = server
            .iter()
            .filter(|(name, count)| old.buckets.get(name).is_none_or(|o| o.0 != *count) || Some(name) == newest.as_ref())
            .cloned()
            .collect();
        let gone: Vec<&String> = old.buckets.keys().filter(|k| !server.iter().any(|b| &b.0 == *k)).collect();
        let mut drop: HashSet<&str> = HashSet::new();
        for name in changed.iter().map(|b| &b.0).chain(gone.iter().copied()) {
            if let Some((_, ids)) = old.buckets.get(name) {
                drop.extend(ids.iter().map(String::as_str));
            }
        }
        log::info!("sync {}: {} of {} months changed", collection.key, changed.len(), server.len());
        let months = immich.bucket_assets(query, &changed, progress).await?;
        let mut idx = Index { full_at: old.full_at, buckets: old.buckets.clone() };
        for name in &gone {
            idx.buckets.remove(*name);
        }
        let mut assets: Vec<Asset> = cached.iter().filter(|a| !drop.contains(a.id.as_str())).cloned().collect();
        for (name, list) in months {
            let count = changed.iter().find(|b| b.0 == name).map_or(list.len(), |b| b.1);
            idx.buckets.insert(name, (count, list.iter().map(|a| a.id.clone()).collect()));
            assets.extend(list);
        }
        Ok((assets, idx))
    }
    .await;

    match result {
        Ok((mut assets, idx)) => {
            assets.sort_by(|a, b| a.taken.cmp(&b.taken));
            // An asset listed in two months (a date edited mid-sync) shows once.
            let mut seen = HashSet::new();
            assets.retain(|a| seen.insert(a.id.clone()));
            let assets = Arc::new(assets);
            shared.fresh.lock().unwrap().insert(query.clone(), (Instant::now(), assets.clone()));
            if query == LIBRARY_QUERY {
                let task = tokio::spawn(prefetch_all(immich.clone(), shared.clone(), assets.clone()));
                if let Some(old) = shared.prefetch.lock().unwrap().replace(task.abort_handle()) {
                    old.abort();
                }
            }
            let _ = std::fs::create_dir_all(cache_dir());
            let _ = std::fs::write(&index_path, serde_json::to_vec(&idx).unwrap());
            if cached.as_ref().is_none_or(|c| *c != assets) {
                let _ = std::fs::write(&path, serde_json::to_vec(&*assets).unwrap());
                publish(shared, collection, request, assets).await;
            }
            true
        }
        Err(e) => {
            log::error!("sync {} failed: {e:#}", collection.key);
            // With a cache on screen, a failed check isn't worth a red line.
            if !quiet {
                shared.status(request, format!("Sync failed: {e:#}"));
                return false;
            }
            true
        }
    }
}

/// A per-server cache file, e.g. `server_cache_path(immich, "albums")` → `albums-<server>.json`.
pub fn server_cache_path(immich: &Immich, stem: &str) -> PathBuf {
    cache_dir().join(format!("{stem}-{:016x}.json", fxhash(&immich.identity())))
}

fn library_cache_path(immich: &Immich, query: &str) -> PathBuf {
    let key = format!("{:016x}", fxhash(&immich.identity()));
    let name = if query == LIBRARY_QUERY { format!("library-{key}.json") } else { format!("library-{key}-{:016x}.json", fxhash(query)) };
    cache_dir().join(name)
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
            Ok((mips, cached)) => shared.send(Msg::Thumb(Thumb { generation, index, tier, mips, cached })),
            Err(e) => log::warn!("thumbnail {}: {e:#}", lib.assets[index].id),
        }
    }
}

/// Decoded mips, and whether the image came from the disk cache.
async fn load_thumb(source: &Source, asset: &Asset, index: usize, tier: Tier) -> Result<(Vec<Vec<u8>>, bool)> {
    let mut cached = false;
    let image = match source {
        Source::Demo(_) => {
            // Pretend there's a network in between.
            tokio::time::sleep(Duration::from_millis(5 + (index % 7) as u64 * 6)).await;
            image::DynamicImage::ImageRgba8(library::demo_image(index as u64, tier.slot()))
        }
        Source::Immich(immich) => {
            let (bytes, from_disk) = match tier {
                Tier::Xl => preview_bytes(immich, &asset.id).await?,
                _ => thumbnail_bytes_from(immich, &asset.id).await?,
            };
            cached = from_disk;
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
    Ok((mips, cached))
}

/// An asset's ~250px thumbnail, from the shared disk cache or the server.
pub async fn thumbnail_bytes(immich: &Immich, id: &str) -> Result<Vec<u8>> {
    Ok(thumbnail_bytes_from(immich, id).await?.0)
}

/// Same, and whether it came from disk.
async fn thumbnail_bytes_from(immich: &Immich, id: &str) -> Result<(Vec<u8>, bool)> {
    cached(cache_dir().join("thumbs").join(id), immich.thumbnail(id)).await
}

/// The ~1440px server preview the grid's largest tiles use, cached alongside the thumbnails
/// (and under the same size limit, least recently used first).
async fn preview_bytes(immich: &Immich, id: &str) -> Result<(Vec<u8>, bool)> {
    let path = cache_dir().join("thumbs").join("previews").join(id);
    cached(path, immich.get_bytes(&format!("/assets/{id}/thumbnail?size=preview"))).await
}

/// Reads `path`, or fetches and writes it. Returns the bytes and whether they were on disk.
async fn cached(path: PathBuf, fetch: impl Future<Output = Result<Vec<u8>>>) -> Result<(Vec<u8>, bool)> {
    if let Ok(b) = tokio::fs::read(&path).await {
        // mtime doubles as last-used time for the cache size limit.
        crate::storage::touch(&path);
        return Ok((b, true));
    }
    let b = fetch.await?;
    let _ = tokio::fs::create_dir_all(path.parent().unwrap()).await;
    if tokio::fs::write(&path, &b).await.is_ok() {
        crate::storage::thumb_written(b.len() as u64);
    }
    Ok((b, false))
}

/// Background thumbnail downloads in flight at once: enough to hide a remote link's latency.
const PREFETCH_AT_ONCE: usize = 12;
/// Typical thumbnail size, for deciding whether the cache has room for more.
const THUMB_GUESS: u64 = 24 << 10;

/// Downloads (without decoding) every thumbnail of the library that isn't on disk yet, newest
/// first, several at a time, pausing whenever the grid wants something. Stops when the cache
/// limit is near, rather than evicting thumbnails to fit others.
async fn prefetch_all(immich: Immich, shared: Arc<Shared>, assets: Arc<Vec<Asset>>) {
    let dir = cache_dir().join("thumbs");
    let missing: Vec<String> = assets.iter().rev().map(|a| a.id.clone()).filter(|id| !dir.join(id).exists()).collect();
    if missing.is_empty() {
        return;
    }
    log::info!("downloading {} thumbnails in the background", missing.len());
    let busy = || {
        let q = shared.queue.lock().unwrap();
        !q.want.is_empty() || !q.inflight.is_empty() || q.syncing
    };
    let full = std::sync::atomic::AtomicBool::new(false);
    let results: Vec<bool> = futures::stream::iter(missing)
        .map(|id| {
            let (immich, busy, full) = (&immich, &busy, &full);
            async move {
                // Whatever the grid is waiting for goes first.
                while busy() {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                if full.load(std::sync::atomic::Ordering::Relaxed) || !crate::storage::has_room(THUMB_GUESS * PREFETCH_AT_ONCE as u64) {
                    full.store(true, std::sync::atomic::Ordering::Relaxed);
                    return false;
                }
                thumbnail_bytes_from(immich, &id).await.inspect_err(|e| log::debug!("background thumbnail: {e:#}")).is_ok()
            }
        })
        .buffer_unordered(PREFETCH_AT_ONCE)
        .collect()
        .await;
    let done = results.iter().filter(|&&ok| ok).count();
    if full.load(std::sync::atomic::Ordering::Relaxed) {
        log::info!("background thumbnails: stopped at the cache limit after {done}");
        return;
    }
    log::info!("background thumbnails: {done} downloaded, library complete");
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
