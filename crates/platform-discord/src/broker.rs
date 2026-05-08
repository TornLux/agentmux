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
    /// One-line "what is this session doing right now". Added in 0.3.4
    /// alongside the orchestrator work; older brokers omit the field
    /// (serde fills with empty string via the struct-level default).
    pub current_status: String,
}

/// Outcome of a `send_input` call. Splits the LocallyOwned case out
/// of the general error bag so the handler can render a friendlier
/// "session is local-only" message + the 5-min-window reaction
/// suppression.
#[derive(Debug)]
pub enum SendInputError {
    LocallyOwned { session: String, message: String },
    Other(anyhow::Error),
}

impl std::fmt::Display for SendInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendInputError::LocallyOwned { session, message } => {
                write!(f, "session '{session}' is locally-owned: {message}")
            }
            SendInputError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SendInputError {}

impl Default for SessionLite {
    fn default() -> Self {
        Self {
            name: String::new(),
            state: "idle".to_string(),
            viewers: 0,
            cwd: String::new(),
            current_status: String::new(),
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
    ///
    /// Returns a typed error so callers can render distinct UX for
    /// `LocallyOwned` (refused because the user demoted this session
    /// — show "this session is local-only now" guidance) vs generic
    /// failures (broker down, claude crash, etc.).
    pub async fn send_input(
        &self,
        session: &str,
        text: &str,
    ) -> std::result::Result<(), SendInputError> {
        let url = format!("{}/sessions/{}/input", self.base_http, session);
        let body = json!({ "text": text });
        let resp = match self.http.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return Err(SendInputError::Other(
                    anyhow::Error::new(e).context(format!("POST {url}")),
                ));
            }
        };
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        // Broker emits a structured 409 body for LocallyOwned with a
        // stable `error` discriminator — see locally_owned_409 in the
        // broker. Generic 409s (e.g. duplicate session create) won't
        // carry that key and fall through to Other.
        if status.as_u16() == 409 {
            if let Ok(parsed) = serde_json::from_str::<Value>(&body_text) {
                if parsed.get("error").and_then(|v| v.as_str()) == Some("locally_owned") {
                    let message = parsed
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("session is locally-owned")
                        .to_string();
                    return Err(SendInputError::LocallyOwned {
                        session: session.to_string(),
                        message,
                    });
                }
            }
        }
        Err(SendInputError::Other(anyhow::anyhow!(
            "/input → {status}: {body_text}"
        )))
    }

    /// Create a session. `auto_resume = None` lets the broker fall
    /// through to its `auto_resume_default` policy (intended path for
    /// general use); `Some(bool)` forces a per-session value.
    pub async fn create_session(
        &self,
        name: &str,
        cwd: Option<&str>,
        auto_resume: Option<bool>,
    ) -> Result<()> {
        let url = format!("{}/sessions", self.base_http);
        let mut body = serde_json::Map::new();
        body.insert("name".into(), json!(name));
        if let Some(c) = cwd {
            body.insert("cwd".into(), json!(c));
        }
        if let Some(ar) = auto_resume {
            body.insert("auto_resume".into(), json!(ar));
        }
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        check_ok(resp, "/sessions").await
    }

    /// POST a decision to a parked PreToolUse approval request. The
    /// hook waiting on `/tool-request` resolves and returns to claude
    /// with the same `allow` / `reason`.
    pub async fn tool_decision(&self, request_id: &str, allow: bool, reason: &str) -> Result<()> {
        let url = format!("{}/tool-decision/{}", self.base_http, request_id);
        let resp = self
            .http
            .post(&url)
            .json(&json!({ "allow": allow, "reason": reason }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        check_ok(resp, "tool-decision").await
    }

    pub async fn set_persist(&self, name: &str, on: bool) -> Result<()> {
        let url = format!("{}/sessions/{}/persist", self.base_http, name);
        let resp = self
            .http
            .post(&url)
            .json(&json!({ "auto_resume": on }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        check_ok(resp, "persist").await
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

    /// Fetch the raw PTY ringbuffer snapshot for a session. The bytes
    /// include ANSI escape sequences and TUI redraw artefacts; callers
    /// rendering for humans must strip those (see `ansi::strip` in the
    /// handler).
    pub async fn get_ring(&self, name: &str) -> Result<Vec<u8>> {
        let url = format!("{}/sessions/{}/ring", self.base_http, name);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("/ring → {status}: {text}");
        }
        let bytes = resp.bytes().await.context("read /ring body")?.to_vec();
        Ok(bytes)
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

    /// Trigger a whole-stack restart on the broker. Broker spawns a
    /// detached respawner then exits, so this call may return success
    /// while broker is mid-shutdown — the bot's WS subscriber will
    /// auto-reconnect once the new broker is up.
    pub async fn restart_agentmux(&self) -> Result<()> {
        let url = format!("{}/restart-agentmux", self.base_http);
        let resp = self
            .http
            .post(&url)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        check_ok(resp, "/restart-agentmux").await
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
