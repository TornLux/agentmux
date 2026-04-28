//! Broker HTTP client + WebSocket subscriber.
//!
//! HTTP side: thin wrapper around `reqwest` for the three endpoints
//! the MVP exercises (`GET /sessions`, `POST /sessions/:k/input`).
//!
//! WS side: opens `broker_ws_url`, reads JSON-line events, and hands
//! them to a callback. Reconnects with backoff on disconnect — the
//! broker can be restarted underneath us without taking the bot down.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{info, warn};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SessionLite {
    pub name: String,
    pub state: String,
    pub viewers: usize,
    pub cwd: String,
}

impl Default for SessionLite {
    fn default() -> Self {
        Self {
            name: String::new(),
            state: "idle".to_string(),
            viewers: 0,
            cwd: String::new(),
        }
    }
}

pub struct BrokerClient {
    base_http: String,
    http: reqwest::Client,
}

impl BrokerClient {
    pub fn new(base_http: String) -> Self {
        // 30s timeout: /input on a hibernated session waits for claude
        // to finish startup (up to ~8s in practice) before writing.
        // 5s would race claude's TUI draw and cancel the handler.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("build reqwest client");
        Self { base_http, http }
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionLite>> {
        let url = format!("{}/sessions", self.base_http);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?;
        let v: Vec<SessionLite> = resp.json().await.context("parse /sessions json")?;
        Ok(v)
    }

    /// Forward one user message to a session as a typed prompt.
    /// `\r` is appended on the broker side so claude treats it as
    /// "Enter pressed".
    pub async fn send_input(&self, session: &str, text: &str) -> Result<()> {
        let url = format!("{}/sessions/{}/input", self.base_http, session);
        let body = json!({ "text": text });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("/input → {status}: {text}");
        }
        Ok(())
    }

    pub async fn create_session(&self, name: &str, cwd: Option<&str>) -> Result<()> {
        let url = format!("{}/sessions", self.base_http);
        let body = match cwd {
            Some(c) => json!({ "name": name, "cwd": c }),
            None => json!({ "name": name }),
        };
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        check_ok(resp, "/sessions").await
    }

    pub async fn delete_session(&self, name: &str) -> Result<()> {
        let url = format!("{}/sessions/{}?force=true", self.base_http, name);
        let resp = self
            .http
            .delete(&url)
            .send()
            .await
            .with_context(|| format!("DELETE {url}"))?;
        check_ok(resp, "DELETE /sessions/:k").await
    }

    pub async fn interrupt_session(&self, name: &str) -> Result<()> {
        self.post_action(name, "interrupt").await
    }

    pub async fn restart_session(&self, name: &str) -> Result<()> {
        self.post_action(name, "restart").await
    }

    pub async fn hibernate_session(&self, name: &str) -> Result<()> {
        self.post_action(name, "hibernate").await
    }

    async fn post_action(&self, name: &str, action: &str) -> Result<()> {
        let url = format!("{}/sessions/{}/{}", self.base_http, name, action);
        let resp = self
            .http
            .post(&url)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        check_ok(resp, action).await
    }
}

async fn check_ok(resp: reqwest::Response, label: &str) -> Result<()> {
    if resp.status().is_success() {
        return Ok(());
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    anyhow::bail!("{label} → {status}: {body}");
}

/// Run the WebSocket subscriber until cancelled. On any error the loop
/// sleeps with exponential backoff (capped at 30s) and reconnects.
/// `on_event` is called from this task; keep it cheap or spawn its
/// work — blocking it backs up the WS read loop.
pub async fn run_ws_subscriber<F>(ws_url: String, on_event: Arc<F>)
where
    F: Fn(Value) + Send + Sync + 'static,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_async(&ws_url).await {
            Ok((mut ws, _)) => {
                info!("broker ws connected: {ws_url}");
                backoff = Duration::from_secs(1);
                loop {
                    let msg = ws.next().await;
                    match msg {
                        Some(Ok(WsMessage::Text(t))) => match serde_json::from_str::<Value>(&t) {
                            Ok(v) => on_event(v),
                            Err(e) => warn!("ws: parse json: {e}"),
                        },
                        Some(Ok(WsMessage::Ping(p))) => {
                            let _ = ws.send(WsMessage::Pong(p)).await;
                        }
                        Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
            }
            Err(e) => {
                warn!("broker ws connect failed: {e}");
            }
        }
        warn!("broker ws disconnected — reconnecting in {:?}", backoff);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}
