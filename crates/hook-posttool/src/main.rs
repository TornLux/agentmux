//! Claude Code `PostToolUse` hook: invoked **after** every tool call.
//!
//! Posts a `tool_progress` event so platform-discord (and any other
//! WS subscriber) can edit the per-message placeholder in place with
//! a human-readable progress narrative ("✏️ editing src/x.rs",
//! "🖥 cargo test", …) instead of leaving a static `💭 working…`
//! sitting there for minutes.
//!
//! Fail-open at every step: any error becomes silent exit 0 so a
//! broker outage never blocks claude. The hook is best-effort UX,
//! not load-bearing.

use std::io::{self, Read};
use std::process::ExitCode;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use shared::{http::post_json, DEFAULT_BROKER_URL};

fn main() -> ExitCode {
    if let Err(e) = run() {
        if std::env::var("AGENT_HOOK_DEBUG").is_ok() {
            eprintln!("hook-posttool: {e:#}");
        }
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    // Same sentinel as hook-stop / hook-notification: only fire under
    // a broker-spawned claude so unrelated `claude` invocations don't
    // pollute events.jsonl or spam Discord.
    let session_id = match std::env::var("AGENT_SESSION_ID") {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    let mut input = String::new();
    io::stdin().read_to_string(&mut input).context("read stdin")?;
    let hook: Value = serde_json::from_str(&input).context("parse hook json")?;

    let tool_name = hook
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if tool_name.is_empty() {
        return Ok(());
    }
    let tool_input = hook.get("tool_input").cloned().unwrap_or(Value::Null);
    let transcript_path = hook
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let broker_url =
        std::env::var("AGENT_BROKER_URL").unwrap_or_else(|_| DEFAULT_BROKER_URL.to_string());

    // Mirror hook-stop / hook-notification: when a local Terminal
    // viewer is attached the user is already watching the tool calls
    // happen in the TUI — don't fan out a parallel narrative to IM.
    if is_local_viewer_attached(&broker_url, &session_id) {
        return Ok(());
    }

    let event = json!({
        "session_id": session_id,
        "type": "tool_progress",
        "tool_name": tool_name,
        "tool_input": tool_input,
        "transcript_path": transcript_path,
    });

    let _ = post_json(&format!("{broker_url}/event"), &event.to_string());
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
