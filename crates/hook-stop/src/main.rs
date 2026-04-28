//! Claude Code `Stop` hook: invoked after the assistant finishes a turn.
//!
//! Reads hook input from stdin, scans the transcript JSONL for the most
//! recent assistant text content, and POSTs an `assistant_message`
//! event to the broker. Failures are logged to stderr but never block
//! claude — exit code is always 0.
//!
//! Hook input shape (PLAN.md appendix A):
//! ```jsonc
//! { "hook_event_name": "Stop", "session_id": "...", "transcript_path": "..." }
//! ```

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::process::ExitCode;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use shared::{http::post_json, DEFAULT_BROKER_URL};

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("hook-stop: {e:#}");
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    // Bail silently if not running under a broker-spawned claude. The
    // user installs hooks at the user-global level (so any claude run
    // sees them), but only broker-spawned claudes set this env var, so
    // unrelated claude chats don't pollute our events log.
    let session_id = match std::env::var("AGENT_SESSION_ID") {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    let mut input = String::new();
    io::stdin().read_to_string(&mut input).context("read stdin")?;
    let hook: Value = serde_json::from_str(&input).context("parse hook json")?;

    let transcript_path = hook
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing transcript_path"))?;

    let broker_url =
        std::env::var("AGENT_BROKER_URL").unwrap_or_else(|_| DEFAULT_BROKER_URL.to_string());

    // PLAN §2.5 / Phase 9: when a local Terminal viewer is attached
    // the user is already watching the TUI live — don't double-notify
    // them by writing this event. We still wouldn't see the GET fail
    // back as anything the user would notice, so a stale state lookup
    // just falls through and posts as usual.
    if is_local_viewer_attached(&broker_url, &session_id) {
        return Ok(());
    }

    let body = wait_for_assistant_text(transcript_path);

    let event = json!({
        "session_id": session_id,
        "type": "assistant_message",
        "body": body,
        "transcript_path": transcript_path,
    });

    if std::env::var("AGENT_HOOK_DEBUG").is_ok() {
        eprintln!("hook-stop event: {event}");
    }

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

/// Waits for claude's transcript to finish flushing the latest
/// assistant turn before extracting its text.
///
/// Stop hook timing is racy: claude fires Stop the moment a turn
/// completes, but the transcript line for that turn may still be in
/// claude's stdio buffer and not yet on disk. We can NOT short-circuit
/// on "file already has assistant text" — from turn 2 onward the file
/// always has the *previous* turn's text, so an early read leaks stale
/// content one turn behind the actual answer.
///
/// Strategy:
///   * Poll size every 50ms. Whenever the file grows, reset a
///     "stability" timer.
///   * Once the file has been unchanged for `STABLE_FOR`, the flush
///     for this turn is done; read once and return.
///   * Hard cap at `MAX_WAIT` so a stuck flush never blocks claude
///     past the hook's own timeout.
///
/// Cost: every Stop adds ~`STABLE_FOR` of latency. That's tiny next to
/// claude's per-turn time and is the price of correctness.
fn wait_for_assistant_text(path: &str) -> String {
    use std::time::{Duration, Instant};
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    const STABLE_FOR: Duration = Duration::from_millis(300);
    const MAX_WAIT: Duration = Duration::from_secs(3);

    let file_size = |p: &str| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);

    let start = Instant::now();
    let mut last_size = file_size(path);
    let mut last_change = start;

    loop {
        std::thread::sleep(POLL_INTERVAL);
        let now = Instant::now();
        let size = file_size(path);
        if size != last_size {
            last_size = size;
            last_change = now;
        }
        let stable = now.duration_since(last_change) >= STABLE_FOR;
        let timed_out = now.duration_since(start) >= MAX_WAIT;
        if stable || timed_out {
            return read_last_assistant_text(path).unwrap_or_default();
        }
    }
}

/// Returns the text of the *latest* assistant entry in a Claude Code
/// transcript JSONL, concatenating all `text` content blocks of that
/// entry. Empty string if no assistant entry has any text content
/// (e.g. tool-only turn).
fn read_last_assistant_text(path: &str) -> Result<String> {
    let f = File::open(path).with_context(|| format!("open transcript {path}"))?;
    let reader = BufReader::new(f);
    let mut last_text = String::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !is_assistant(&v) {
            continue;
        }
        let text = extract_text(&v);
        if !text.is_empty() {
            last_text = text;
        }
    }
    Ok(last_text)
}

fn is_assistant(v: &Value) -> bool {
    if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
        return true;
    }
    v.get("message")
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        == Some("assistant")
}

fn extract_text(v: &Value) -> String {
    let content = v
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| v.get("content"));
    let Some(content) = content else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let mut out = String::new();
    if let Some(arr) = content.as_array() {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(s) = item.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(s);
                }
            }
        }
    }
    out
}
