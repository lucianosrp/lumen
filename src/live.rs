//! Live updates: Immich pushes socket.io events when assets are uploaded, edited, trashed or
//! restored, so the grid can refresh without polling.
//!
//! Speaks just enough Engine.IO v4 / socket.io over a plain WebSocket: "0" open, "40" connect,
//! "2"/"3" ping/pong, `42["event", payload]` events.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use anyhow::{Result, bail};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

use crate::immich::Immich;

/// Bursts (e.g. a phone backing up 200 photos) are coalesced into one notification.
const SETTLE: Duration = Duration::from_millis(1500);

/// Starts listening on the loader's runtime. Each message on the receiver means "the library
/// changed on the server; refresh what's on screen".
pub fn start(immich: Immich, rt: &tokio::runtime::Handle, wake: Arc<dyn Fn() + Send + Sync>) -> Receiver<()> {
    let (tx, rx) = channel();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    rt.spawn(async move {
        let mut backoff = Duration::from_secs(2);
        loop {
            match listen(&immich, &event_tx).await {
                Ok(()) => backoff = Duration::from_secs(2),
                Err(e) => {
                    log::info!("live updates: {e:#}; retrying in {}s", backoff.as_secs());
                    // The address may have gone away (left the home network): find another.
                    let _ = immich.connect().await;
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });

    rt.spawn(async move {
        while let Some(first) = event_rx.recv().await {
            let mut events = vec![first];
            // Keep collecting until the server has been quiet for a moment.
            while let Ok(Some(e)) = tokio::time::timeout(SETTLE, event_rx.recv()).await {
                events.push(e);
            }
            log::info!("live updates: {} event(s) ({}), refreshing", events.len(), events[0]);
            if tx.send(()).is_err() {
                break;
            }
            wake();
        }
    });
    rx
}

/// One connection's lifetime; returns when the server closes it.
async fn listen(immich: &Immich, events: &tokio::sync::mpsc::UnboundedSender<String>) -> Result<()> {
    let ws_base = immich.base().replacen("http", "ws", 1);
    let mut req = format!("{ws_base}/api/socket.io/?EIO=4&transport=websocket").into_client_request()?;
    req.headers_mut().insert("x-api-key", HeaderValue::from_str(immich.api_key())?);
    // An address that doesn't answer would otherwise hang until the OS gives up (minutes).
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(5), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| anyhow::anyhow!("no answer from {ws_base}"))??;

    let mut connected = false;
    while let Some(msg) = ws.next().await {
        let Message::Text(text) = msg? else { continue };
        let text = text.as_str();
        match text.as_bytes().first() {
            Some(b'0') => ws.send(Message::text("40")).await?,
            Some(b'2') => ws.send(Message::text("3")).await?,
            Some(b'4') => match text.get(..2) {
                Some("40") if !connected => {
                    connected = true;
                    log::info!("live updates: connected");
                }
                Some("41") | Some("44") => bail!("server refused the socket.io session"),
                Some("42") => {
                    if let Some(name) = event_name(&text[2..]).filter(|n| is_asset_event(n)) {
                        let _ = events.send(name);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

fn event_name(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    v.get(0)?.as_str().map(String::from)
}

/// Uploads, edits, trash/restore/delete, visibility changes; not server-version or config pings.
fn is_asset_event(name: &str) -> bool {
    name.starts_with("on_upload") || name.starts_with("on_asset") || name.starts_with("Asset")
}
