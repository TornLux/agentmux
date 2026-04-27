//! Claude Code `Notification` hook: invoked when claude needs the
//! user's attention (waiting for input, etc.). Forwards the message to
//! the broker as a `notification` event.

use std::io::{self, Read};
use std::process::ExitCode;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use shared::{http::post_json, DEFAULT_BROKER_URL};

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("hook-notification: {e:#}");
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    // Sentinel: only fire when running under a broker-spawned claude.
    // See hook-stop for rationale.
    let session_id = match std::env::var("AGENT_SESSION_ID") {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    let mut input = String::new();
    io::stdin().read_to_string(&mut input).context("read stdin")?;
    let hook: Value = serde_json::from_str(&input).context("parse hook json")?;

    let message = hook
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let transcript_path = hook
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let broker_url =
        std::env::var("AGENT_BROKER_URL").unwrap_or_else(|_| DEFAULT_BROKER_URL.to_string());

    // PLAN §2.5 / Phase 9: skip the event when a local Terminal
    // viewer is attached — they're already watching claude's
    // notification on the TUI.
    if is_local_viewer_attached(&broker_url, &session_id) {
        return Ok(());
    }

    let event = json!({
        "session_id": session_id,
        "type": "notification",
        "message": message,
        "transcript_path": transcript_path,
    });

    post_json(&format!("{broker_url}/event"), &event.to_string()).context("post /event")?;
    Ok(())
}

fn is_local_viewer_attached(broker_url: &str, session_id: &str) -> bool {
    let url = format!("{broker_url}/sessions/{session_id}/state");
    let body = match shared::http::get(&url) {
        Ok(b) => b,
        Err(_) => return false,
    };
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("local_viewer_attached")
                .and_then(|x| x.as_bool())
        })
        .unwrap_or(false)
}
