//! Broker `/ws` event-bus subscriber. Pushes interesting event kinds
//! (`assistant_message`, `notification`, `tool_request`) into the tray's
//! event loop so the toast layer can render them.
//!
//! Reconnect with capped exponential backoff: the broker may restart
//! out from under us, and we'd rather come back quickly than spam.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use shared::config::Config;
use tao::event_loop::EventLoopProxy;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tracing::{debug, info, warn};

use crate::UserEvent;

const KINDS_OF_INTEREST: &[&str] = &["assistant_message", "notification", "tool_request"];
const RECONNECT_INITIAL: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

pub async fn run_subscriber(cfg: Arc<Config>, proxy: EventLoopProxy<UserEvent>) {
    let mut backoff = RECONNECT_INITIAL;
    loop {
        match run_once(&cfg, &proxy).await {
            Ok(()) => {
                // Clean disconnect (server closed). Reset backoff.
                backoff = RECONNECT_INITIAL;
            }
            Err(e) => {
                warn!("ws session ended: {e}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

async fn run_once(
    cfg: &Config,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<()> {
    let url = ws_url(cfg);
    debug!("ws connecting: {url}");

    // Use IntoClientRequest so tungstenite generates the WebSocket
    // handshake headers (Sec-WebSocket-Key etc.) for us — then layer
    // on the optional bearer token. Loopback brokers exempt auth so
    // the header is harmless when not configured.
    let mut req = url.as_str().into_client_request()?;
    if !cfg.attach_token.is_empty() {
        req.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", cfg.attach_token))?,
        );
    }

    let (ws_stream, _resp) = tokio_tungstenite::connect_async(req).await?;
    info!("ws connected to {url}");
    let _ = proxy.send_event(UserEvent::BrokerConnectivity { connected: true });

    let (_write, mut read) = ws_stream.split();
    while let Some(msg) = read.next().await {
        let msg = msg?;
        let text = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                debug!("non-json ws frame ignored: {e}");
                continue;
            }
        };
        let kind = v
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if !KINDS_OF_INTEREST.iter().any(|k| *k == kind) {
            continue;
        }
        let session_name = v
            .get("session_name")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        let _ = proxy.send_event(UserEvent::BrokerEvent {
            kind,
            session_name,
            raw: v,
        });
    }

    let _ = proxy.send_event(UserEvent::BrokerConnectivity { connected: false });
    Ok(())
}

fn ws_url(cfg: &Config) -> String {
    // Config's http_addr is `host:port`; the WS endpoint is `/ws`.
    // For LAN-bound brokers (`0.0.0.0:port`) the user's loopback /
    // host portion in browsers / clients is whatever they like; for
    // our local-process tray we hardwire 127.0.0.1 to match the
    // expected loopback connection.
    let port = cfg
        .http_addr
        .rsplit_once(':')
        .map(|(_, p)| p)
        .unwrap_or("8765");
    format!("ws://127.0.0.1:{port}/ws")
}
