use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use serde::Deserialize;

use crate::library::Asset;

#[derive(Clone)]
pub struct Immich {
    base: String,
    key: String,
    http: reqwest::Client,
}

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
    is_favorite: Vec<bool>,
}

impl Immich {
    pub fn new(url: &str, key: &str) -> Result<Self> {
        let base = url.trim_end_matches('/').trim_end_matches("/api").to_string();
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(32)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self { base, key: key.to_string(), http })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// GET `/api{path}` with auth; errors on non-2xx.
    pub async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let url = format!("{}/api{}", self.base, path);
        let res = self
            .http
            .get(&url)
            .header("x-api-key", &self.key)
            .header("accept", "*/*")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !res.status().is_success() {
            bail!("GET {url}: {}", res.status());
        }
        Ok(res)
    }

    /// Fetches a timeline's metadata, e.g. `query` = "visibility=timeline" or "albumId=…".
    /// `progress` gets (assets so far, total).
    pub async fn timeline(&self, query: &str, progress: impl Fn(usize, usize)) -> Result<Vec<Asset>> {
        let buckets: Vec<Bucket> = self.get(&format!("/timeline/buckets?{query}")).await?.json().await?;
        let total = buckets.iter().map(|b| b.count).sum();
        let mut done = 0;
        let mut assets = Vec::with_capacity(total);
        let mut results = stream::iter(buckets)
            .map(|b| async move {
                let path = format!("/timeline/bucket?{query}&timeBucket={}", b.time_bucket);
                let cols: BucketAssets = self.get(&path).await?.json().await?;
                anyhow::Ok(cols)
            })
            .buffer_unordered(8);
        while let Some(cols) = results.next().await {
            let cols = cols?;
            done += cols.id.len();
            for (i, id) in cols.id.into_iter().enumerate() {
                assets.push(Asset {
                    id,
                    taken: cols.file_created_at.get(i).cloned().unwrap_or_default(),
                    ratio: cols.ratio.get(i).copied().flatten().unwrap_or(1.0) as f32,
                    video: !cols.is_image.get(i).copied().unwrap_or(true),
                    thumbhash: cols.thumbhash.get(i).cloned().flatten(),
                    favorite: cols.is_favorite.get(i).copied().unwrap_or(false),
                });
            }
            progress(done, total);
        }
        Ok(assets)
    }

    /// Sends a JSON body to `/api{path}` with auth; errors on non-2xx with the server's message.
    async fn send_json(&self, method: reqwest::Method, path: &str, body: &serde_json::Value) -> Result<reqwest::Response> {
        let url = format!("{}/api{}", self.base, path);
        let mut req = self.http.request(method.clone(), &url).header("x-api-key", &self.key);
        if !body.is_null() {
            req = req.json(body);
        }
        let res = req
            .send()
            .await
            .with_context(|| format!("{method} {url}"))?;
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
