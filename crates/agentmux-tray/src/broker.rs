//! Loopback HTTP client for the broker control plane + a periodic
//! `/sessions` poller that keeps the tray menu / icon colour in sync.
//!
//! We always speak to `127.0.0.1` (auth middleware exempts loopback),
//! so no token plumbing here.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use shared::config::Config;
use tao::event_loop::EventLoopProxy;
use tracing::warn;

use crate::UserEvent;

/// Refresh cadence for the menu/icon. Quick enough that newly created
/// sessions pop up "soon", but slow enough to keep Discord-style
/// chatter to a whisper. WS events still drive the high-fidelity
/// signals (toast on assistant_message etc.) — this poll is just for
/// "list the sessions" UI state.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Subset of `GET /sessions` that we render in the tray menu.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub state: String,
    #[serde(default)]
    pub viewers: u32,
    #[serde(default)]
    pub cwd: String,
}

pub struct BrokerClient {
    cfg: Arc<Config>,
    http: reqwest::Client,
}

impl BrokerClient {
    pub fn new(cfg: Arc<Config>) -> Self {
        // Short timeouts everywhere — broker is on loopback, anything
        // taking >2s either means the broker is wedged or down. We'd
        // rather show "no broker" in the tray than hang menu rebuilds.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build reqwest client");
        Self { cfg, http }
    }

    pub fn http_url(&self) -> String {
        self.cfg.http_url()
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let url = format!("{}/sessions", self.cfg.http_url());
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?;
        let v: Vec<SessionInfo> = resp.json().await.context("parse /sessions json")?;
        Ok(v)
    }

    pub async fn interrupt(&self, key: &str) -> Result<()> {
        let url = format!("{}/sessions/{key}/interrupt", self.cfg.http_url());
        self.http.post(&url).send().await?.error_for_status()?;
        Ok(())
    }

    pub async fn hibernate(&self, key: &str) -> Result<()> {
        let url = format!("{}/sessions/{key}/hibernate", self.cfg.http_url());
        self.http.post(&url).send().await?.error_for_status()?;
        Ok(())
    }

    pub async fn restart(&self, key: &str) -> Result<()> {
        let url = format!("{}/sessions/{key}/restart", self.cfg.http_url());
        self.http.post(&url).send().await?.error_for_status()?;
        Ok(())
    }

    pub async fn kill(&self, key: &str) -> Result<()> {
        let url = format!(
            "{}/sessions/{key}?force=true",
            self.cfg.http_url()
        );
        self.http.delete(&url).send().await?.error_for_status()?;
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let url = format!("{}/shutdown", self.cfg.http_url());
        self.http.post(&url).send().await?.error_for_status()?;
        Ok(())
    }

    /// Resolve a parked `/tool-request` long-poll. The broker's
    /// `/tool-decision/:id` returns 404 if the request already
    /// timed out or another consumer (Discord) resolved it first —
    /// we map that to "fine, somebody else got there".
    pub async fn post_tool_decision(
        &self,
        request_id: &str,
        allow: bool,
        reason: &str,
    ) -> Result<()> {
        let url = format!("{}/tool-decision/{request_id}", self.cfg.http_url());
        let body = serde_json::json!({ "allow": allow, "reason": reason });
        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            tracing::debug!("tool decision {request_id} already resolved by someone else");
            return Ok(());
        }
        resp.error_for_status()?;
        Ok(())
    }
}

/// Long-running task on the tokio runtime: poll `/sessions` and
/// forward snapshots through the event-loop proxy. Connection-state
/// transitions (broker comes back / drops) also fan out as
/// `BrokerConnectivity` so the tray can flip its tooltip.
pub async fn run_session_poller(
    broker: Arc<BrokerClient>,
    proxy: EventLoopProxy<UserEvent>,
) {
    let mut connected_last = None::<bool>;
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        match broker.list_sessions().await {
            Ok(list) => {
                let _ = proxy.send_event(UserEvent::SessionsRefreshed(Arc::new(list)));
                if connected_last != Some(true) {
                    let _ = proxy.send_event(UserEvent::BrokerConnectivity { connected: true });
                    connected_last = Some(true);
                }
            }
            Err(e) => {
                if connected_last != Some(false) {
                    warn!("/sessions poll failed: {e}");
                    let _ = proxy.send_event(UserEvent::BrokerConnectivity { connected: false });
                    connected_last = Some(false);
                }
            }
        }
    }
}
