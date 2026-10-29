use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use serde::Deserialize;

use crate::library::Asset;

/// A client for one server reachable at several addresses: a local one (LAN IP) first and
/// fallbacks (a public domain) after it. Requests go to the current route; when it stops
/// answering, the others are probed and the first that answers takes over. Clones share the
/// route, so the whole app moves together.
#[derive(Clone)]
pub struct Immich {
    routes: Arc<Routes>,
    key: String,
    http: reqwest::Client,
}

struct Routes {
    /// Base URLs in order of preference (no trailing `/api`).
    urls: RwLock<Vec<String>>,
    current: AtomicUsize,
    /// One probe at a time; requests failing meanwhile wait for its verdict.
    probing: tokio::sync::Mutex<()>,
}

/// How long a probe waits for `/server/ping` on each address.
const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);
/// While on a fallback, how often to check whether the preferred address is back (home again).
const RECHECK: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    time_bucket: String,
    count: usize,
}

/// The timeline endpoint answers in columns, one array per field.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BucketAssets {
    id: Vec<String>,
    ratio: Vec<Option<f64>>,
    thumbhash: Vec<Option<String>>,
    file_created_at: Vec<String>,
    is_image: Vec<bool>,
    #[serde(default)]
    live_photo_video_id: Vec<Option<String>>,
    /// Milliseconds on current servers; "0:00:11.001" on older ones.
    #[serde(default)]
    duration: Vec<Option<serde_json::Value>>,
    is_favorite: Vec<bool>,
}

impl Immich {
    /// `urls`: addresses in order of preference; empty ones are skipped.
    pub fn new(urls: &[&str], key: &str) -> Result<Self> {
        let urls: Vec<String> = urls.iter().map(|u| normalize(u)).filter(|u| !u.is_empty()).collect();
        if urls.is_empty() {
            bail!("no server address");
        }
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(32)
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(30))
            .build()?;
        let routes = Routes { urls: RwLock::new(urls), current: AtomicUsize::new(0), probing: Default::default() };
        Ok(Self { routes: Arc::new(routes), key: key.to_string(), http })
    }

    /// The same server, key and current route on a new connection pool: pooled connections
    /// belong to the runtime that opened them, so a client moving to another runtime (the
    /// sign-in screen's to the app's) needs its own.
    pub fn fresh(&self) -> Result<Self> {
        let urls = self.urls();
        let fresh = Self::new(&urls.iter().map(String::as_str).collect::<Vec<_>>(), &self.key)?;
        fresh.routes.current.store(self.routes.current.load(Ordering::Relaxed), Ordering::Relaxed);
        Ok(fresh)
    }

    /// The address requests go to right now.
    pub fn base(&self) -> String {
        let urls = self.routes.urls.read().unwrap();
        urls[self.routes.current.load(Ordering::Relaxed).min(urls.len() - 1)].clone()
    }

    /// The preferred address: names this server in cache keys, whichever route is in use.
    pub fn identity(&self) -> String {
        self.routes.urls.read().unwrap()[0].clone()
    }

    /// All configured addresses, preferred first.
    pub fn urls(&self) -> Vec<String> {
        self.routes.urls.read().unwrap().clone()
    }

    /// True when requests currently go to a fallback address.
    pub fn on_fallback(&self) -> bool {
        self.routes.current.load(Ordering::Relaxed) > 0
    }

    /// Replaces the addresses (Settings › Server) and finds the best one again.
    pub async fn set_urls(&self, urls: &[&str]) -> Result<()> {
        let urls: Vec<String> = urls.iter().map(|u| normalize(u)).filter(|u| !u.is_empty()).collect();
        if urls.is_empty() {
            bail!("no server address");
        }
        *self.routes.urls.write().unwrap() = urls;
        self.routes.current.store(0, Ordering::Relaxed);
        self.connect().await.map(|_| ())
    }

    /// Probes every address at once and switches to the most preferred one that answers.
    /// Returns its index. Errors (keeping the current route) when none does.
    pub async fn connect(&self) -> Result<usize> {
        let _probe = self.routes.probing.lock().await;
        self.probe().await
    }

    async fn probe(&self) -> Result<usize> {
        let urls = self.urls();
        let pings = urls.iter().map(|u| {
            let req = self.http.get(format!("{u}/api/server/ping")).timeout(PROBE_TIMEOUT);
            async move { req.send().await.is_ok_and(|r| r.status().is_success()) }
        });
        let up = futures::future::join_all(pings).await;
        let Some(best) = up.iter().position(|&ok| ok) else {
            bail!("no answer from {}", urls.join(" or "));
        };
        if self.routes.current.swap(best, Ordering::Relaxed) != best {
            log::info!("server: using {}{}", urls[best], if best > 0 { " (fallback)" } else { "" });
        }
        Ok(best)
    }

    /// After a request to `failed` couldn't connect: probes again unless another request
    /// already moved on. Returns whether a retry should go somewhere else.
    async fn reroute(&self, failed: &str) -> bool {
        if self.routes.urls.read().unwrap().len() < 2 {
            return false;
        }
        let _probe = self.routes.probing.lock().await;
        if self.base() != failed {
            return true;
        }
        self.probe().await.is_ok() && self.base() != failed
    }

    /// Background task: while on a fallback, returns to the preferred address once it
    /// answers again (e.g. back on the home network).
    pub async fn watch_routes(self) {
        loop {
            tokio::time::sleep(RECHECK).await;
            if self.on_fallback() {
                let _ = self.connect().await;
            }
        }
    }

    /// Sends a request built for the current address; if it can't connect and another address
    /// answers, sends it once more there.
    async fn send(&self, build: impl Fn(&str) -> reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
        let base = self.base();
        match build(&base).send().await {
            Err(e) if (e.is_connect() || e.is_timeout()) && self.reroute(&base).await => build(&self.base()).send().await,
            r => r,
        }
    }

    /// GET `/api{path}` with auth; errors on non-2xx.
    pub async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let res = self
            .send(|base| self.http.get(format!("{base}/api{path}")).header("x-api-key", &self.key).header("accept", "*/*"))
            .await
            .with_context(|| format!("GET {path}"))?;
        if !res.status().is_success() {
            bail!("GET {path}: {}", res.status());
        }
        Ok(res)
    }

    /// A timeline's months and how many assets each holds, newest first, e.g. `query` =
    /// "visibility=timeline" or "albumId=…".
    pub async fn buckets(&self, query: &str) -> Result<Vec<(String, usize)>> {
        let buckets: Vec<Bucket> = self.get(&format!("/timeline/buckets?{query}")).await?.json().await?;
        Ok(buckets.into_iter().map(|b| (b.time_bucket, b.count)).collect())
    }

    /// The assets of the given months, eight at a time. `progress` gets (assets so far, total).
    pub async fn bucket_assets(
        &self,
        query: &str,
        buckets: &[(String, usize)],
        progress: impl Fn(usize, usize),
    ) -> Result<Vec<(String, Vec<Asset>)>> {
        let total = buckets.iter().map(|b| b.1).sum();
        let mut done = 0;
        let mut out = Vec::with_capacity(buckets.len());
        let names: Vec<String> = buckets.iter().map(|b| b.0.clone()).collect();
        let mut results = stream::iter(names)
            .map(|name| async move {
                let path = format!("/timeline/bucket?{query}&timeBucket={name}");
                let cols: BucketAssets = self.get(&path).await?.json().await?;
                anyhow::Ok((name, cols))
            })
            .buffer_unordered(8);
        while let Some(r) = results.next().await {
            let (name, cols) = r?;
            done += cols.id.len();
            let mut assets = Vec::with_capacity(cols.id.len());
            for (i, id) in cols.id.into_iter().enumerate() {
                assets.push(Asset {
                    id,
                    taken: cols.file_created_at.get(i).cloned().unwrap_or_default(),
                    ratio: cols.ratio.get(i).copied().flatten().unwrap_or(1.0) as f32,
                    video: !cols.is_image.get(i).copied().unwrap_or(true),
                    thumbhash: cols.thumbhash.get(i).cloned().flatten(),
                    live_video: cols.live_photo_video_id.get(i).cloned().flatten(),
                    duration_ms: cols.duration.get(i).and_then(|d| duration_ms(d.as_ref()?)),
                    favorite: cols.is_favorite.get(i).copied().unwrap_or(false),
                });
            }
            out.push((name, assets));
            progress(done, total);
        }
        Ok(out)
    }

    /// Sends a JSON body to `/api{path}` with auth; errors on non-2xx with the server's message.
    async fn send_json(&self, method: reqwest::Method, path: &str, body: &serde_json::Value) -> Result<reqwest::Response> {
        self.send_json_within(method, path, body, None).await
    }

    async fn send_json_within(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &serde_json::Value,
        timeout: Option<std::time::Duration>,
    ) -> Result<reqwest::Response> {
        let res = self
            .send(|base| {
                let mut req = self.http.request(method.clone(), format!("{base}/api{path}")).header("x-api-key", &self.key);
                if let Some(t) = timeout {
                    req = req.timeout(t);
                }
                if !body.is_null() {
                    req = req.json(body);
                }
                req
            })
            .await
            .with_context(|| format!("{method} {path}"))?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("message").map(|m| m.to_string().trim_matches('"').to_string()))
                .unwrap_or(text);
            bail!("{method} {path}: {status} {msg}");
        }
        Ok(res)
    }

    /// Parses the reply as JSON, treating an empty body (204) as `null`.
    async fn reply(res: reqwest::Response) -> Result<serde_json::Value> {
        let bytes = res.bytes().await?;
        Ok(if bytes.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&bytes)? })
    }

    pub async fn put_json(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        Self::reply(self.send_json(reqwest::Method::PUT, path, &body).await?).await
    }

    pub async fn post_json(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        Self::reply(self.send_json(reqwest::Method::POST, path, &body).await?).await
    }

    /// POST with a longer deadline than the client's default, e.g. a smart search that has to
    /// wait for the server to load its CLIP model first.
    pub async fn post_json_within(&self, path: &str, body: serde_json::Value, timeout: std::time::Duration) -> Result<serde_json::Value> {
        Self::reply(self.send_json_within(reqwest::Method::POST, path, &body, Some(timeout)).await?).await
    }

    pub async fn delete_json(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        Self::reply(self.send_json(reqwest::Method::DELETE, path, &body).await?).await
    }

    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(self.get(path).await?.json().await?)
    }

    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self.get(path).await?.bytes().await?.to_vec())
    }

    pub fn api_key(&self) -> &str {
        &self.key
    }

    /// The server-made ~250px WebP thumbnail.
    pub async fn thumbnail(&self, id: &str) -> Result<Vec<u8>> {
        let res = self.get(&format!("/assets/{id}/thumbnail?size=thumbnail")).await?;
        Ok(res.bytes().await?.to_vec())
    }
}

/// "192.168.1.5:2283" → "http://192.168.1.5:2283"; "photos.example.com/" → "https://photos.example.com".
/// Bare IPs and hosts with a port default to http, domains to https.
pub fn normalize(url: &str) -> String {
    let url = url.trim().trim_end_matches('/').trim_end_matches("/api").trim_end_matches('/');
    if url.is_empty() || url.contains("://") {
        return url.to_string();
    }
    let host = url.split(['/', ':']).next().unwrap_or("");
    let plain = url.contains(':') || host == "localhost" || host.parse::<std::net::Ipv4Addr>().is_ok() || host.ends_with(".local") || host.ends_with(".lan");
    format!("{}://{url}", if plain { "http" } else { "https" })
}

/// 11001 (ms) or "0:00:11.001" → 11001; None for zero or unparsable.
pub fn duration_ms(d: &serde_json::Value) -> Option<u32> {
    let secs = match d {
        serde_json::Value::Number(ms) => ms.as_f64()? / 1000.0,
        serde_json::Value::String(s) => {
            s.split(':').try_fold(0.0, |acc, p| p.trim().parse::<f64>().ok().map(|v| acc * 60.0 + v))?
        }
        _ => return None,
    };
    (secs > 0.0).then(|| (secs * 1000.0).round().min(u32::MAX as f64) as u32)
}
