//! Per-kind config validation. Output is line-oriented so the wrapper
//! script can pass it through unchanged. Lines start with one of:
//!   `✓ ` ok / informational
//!   `⚠ ` warning (non-fatal but surface to user)
//!   `✗ ` failure (causes non-zero exit)

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::Value as Json;
use toml_edit::DocumentMut;

pub fn run(args: &[String]) -> Result<()> {
    // Args: <file> --kind <kind>
    let mut file: Option<&str> = None;
    let mut kind: Option<&str> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--kind" => {
                kind = args.get(i + 1).map(|s| s.as_str());
                i += 2;
            }
            other if file.is_none() => {
                file = Some(other);
                i += 1;
            }
            other => anyhow::bail!("unexpected arg: {other}"),
        }
    }

    let file = file.ok_or_else(|| anyhow!("usage: config check <file> --kind <broker|discord|hooks>"))?;
    let kind = kind.unwrap_or("broker");
    let path = Path::new(file);

    if !path.exists() {
        // Not having the file is fine for optional configs; the
        // wrapper decides whether that's an error or "use defaults".
        // We surface it as ⊘ and exit 0 — the wrapper can colour-code.
        println!("⊘ {} not found at {}", kind, path.display());
        return Ok(());
    }

    let mut had_failure = false;
    match kind {
        "broker" => check_broker(path, &mut had_failure)?,
        "discord" => check_discord(path, &mut had_failure)?,
        "hooks" => check_hooks(path, &mut had_failure)?,
        other => anyhow::bail!("unknown --kind: {other} (broker|discord|hooks)"),
    }

    if had_failure {
        std::process::exit(2);
    }
    Ok(())
}

fn load_toml(path: &Path) -> Result<DocumentMut> {
    let content = fs::read_to_string(path).with_context(|| format!("read {path:?}"))?;
    content
        .parse::<DocumentMut>()
        .with_context(|| format!("parse TOML in {path:?}"))
}

fn check_broker(path: &Path, had_failure: &mut bool) -> Result<()> {
    let doc = match load_toml(path) {
        Ok(d) => d,
        Err(e) => {
            println!("✗ broker: {e:#}");
            *had_failure = true;
            return Ok(());
        }
    };
    println!("✓ broker config parses ({})", path.display());

    if let Some(addr) = doc.get("http_addr").and_then(|v| v.as_str()) {
        if !addr.contains(':') {
            println!("⚠ broker.http_addr={addr:?} should look like host:port");
        } else {
            println!("✓ http_addr = {addr}");
        }
    }
    if let Some(pipe) = doc.get("pipe_name").and_then(|v| v.as_str()) {
        if !pipe.starts_with(r"\\.\pipe\") {
            println!(r"⚠ broker.pipe_name={pipe:?} should start with \\.\pipe\");
        } else {
            println!("✓ pipe_name = {pipe}");
        }
    }
    if let Some(n) = doc.get("ring_cap_bytes").and_then(|v| v.as_integer()) {
        if n <= 0 {
            println!("✗ broker.ring_cap_bytes must be > 0 (got {n})");
            *had_failure = true;
        } else {
            println!("✓ ring_cap_bytes = {n}");
        }
    }
    Ok(())
}

fn check_discord(path: &Path, had_failure: &mut bool) -> Result<()> {
    let doc = match load_toml(path) {
        Ok(d) => d,
        Err(e) => {
            println!("✗ discord: {e:#}");
            *had_failure = true;
            return Ok(());
        }
    };
    println!("✓ discord config parses ({})", path.display());

    let token_env = doc
        .get("token_env")
        .and_then(|v| v.as_str())
        .unwrap_or("DISCORD_BOT_TOKEN");
    println!("✓ token_env = {token_env}");

    let users = doc.get("allowed_user_ids").and_then(|v| v.as_array());
    match users {
        Some(arr) if !arr.is_empty() => {
            println!("✓ allowed_user_ids: {} user(s)", arr.len());
        }
        _ => {
            println!("✗ allowed_user_ids is empty — bot refuses to start without at least one entry");
            *had_failure = true;
        }
    }

    let channels = doc.get("channel_ids").and_then(|v| v.as_array());
    match channels {
        Some(arr) if !arr.is_empty() => println!("✓ channel_ids: {} channel(s)", arr.len()),
        _ => println!("⚠ channel_ids is empty — bot will listen in every visible channel"),
    }

    let default_session = doc
        .get("default_session")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    println!("✓ default_session = {default_session}");
    Ok(())
}

/// Validate `~/.claude/settings.json`. JSON, not TOML; we use serde_json.
fn check_hooks(path: &Path, had_failure: &mut bool) -> Result<()> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            println!("✗ hooks: read {path:?}: {e}");
            *had_failure = true;
            return Ok(());
        }
    };
    let doc: Json = match serde_json::from_str(&content) {
        Ok(d) => d,
        Err(e) => {
            println!("✗ hooks: parse JSON: {e}");
            *had_failure = true;
            return Ok(());
        }
    };
    println!("✓ hooks file parses ({})", path.display());

    let hooks = doc.get("hooks");
    let mut found_stop = false;
    let mut found_notif = false;

    if let Some(events) = hooks.and_then(|h| h.as_object()) {
        for kind in ["Stop", "Notification"] {
            let entries = events.get(kind).and_then(|v| v.as_array());
            let entries = match entries {
                Some(e) => e,
                None => continue,
            };
            for outer in entries {
                let inner = outer.get("hooks").and_then(|h| h.as_array());
                let inner = match inner {
                    Some(i) => i,
                    None => continue,
                };
                for hook in inner {
                    let cmd = hook.get("command").and_then(|c| c.as_str());
                    if let Some(cmd) = cmd {
                        let p = Path::new(cmd);
                        if p.exists() {
                            println!("✓ {kind} hook → {cmd}");
                        } else {
                            println!("⚠ {kind} hook → {cmd} (file not found)");
                        }
                        match kind {
                            "Stop" => found_stop = true,
                            "Notification" => found_notif = true,
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    if !found_stop {
        println!("⚠ no Stop hook entry — IM bots won't see assistant_message events");
    }
    if !found_notif {
        println!("⚠ no Notification hook entry — auto-resume readiness wait won't see the ready signal");
    }
    Ok(())
}
