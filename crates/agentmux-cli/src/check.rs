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
    let auto_resume_default = doc
        .get("auto_resume_default")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if auto_resume_default {
        println!("✓ auto_resume_default = true (new sessions persist by default)");
    } else {
        println!("✓ auto_resume_default = false (new sessions are ephemeral by default)");
    }

    let token = doc
        .get("attach_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if token.is_empty() {
        println!("✓ attach_token = (unset — LAN access disabled; loopback only)");
    } else {
        println!(
            "✓ attach_token = (set, {} chars — non-loopback requests require Bearer)",
            token.chars().count()
        );
    }

    // default_cwd: a typo here is annoying because broker quietly
    // falls back to the launch cwd. Catch missing-dir at config-check
    // time so the user sees a ✗ instead of "huh, my new sessions
    // keep landing in the wrong folder".
    let default_cwd = doc
        .get("default_cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if default_cwd.is_empty() {
        println!("✓ default_cwd = (unset — new sessions inherit broker's launch cwd)");
    } else if Path::new(default_cwd).is_dir() {
        println!("✓ default_cwd = {default_cwd}");
    } else {
        println!(
            "✗ default_cwd = {default_cwd:?} does not exist — broker will fall back to launch cwd"
        );
        *had_failure = true;
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
        _ => println!("⚠ channel_ids is empty — bot will listen in every visible server channel"),
    }

    let allow_dm = doc
        .get("allow_dm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if allow_dm {
        println!("✓ allow_dm = true (1:1 DMs from whitelisted users accepted)");
    } else {
        println!("✓ allow_dm = false (DMs ignored)");
    }

    let notify_on_idle = doc
        .get("notify_on_idle")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if notify_on_idle {
        println!("✓ notify_on_idle = true (idle pings forwarded)");
    } else {
        println!("✓ notify_on_idle = false (idle pings dropped, permission prompts still pass)");
    }

    let respond_to_mentions = doc
        .get("respond_to_mentions")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if respond_to_mentions {
        println!("✓ respond_to_mentions = true (@mention bypasses channel whitelist)");
    } else {
        println!("✓ respond_to_mentions = false (channel whitelist enforced strictly)");
    }

    let guild_id = doc
        .get("slash_command_guild_id")
        .and_then(|v| v.as_integer())
        .unwrap_or(0);
    if guild_id > 0 {
        println!("✓ slash_command_guild_id = {guild_id} (instant per-guild registration)");
    } else {
        println!("✓ slash_command_guild_id = 0 (global registration, ~1h propagation)");
    }

    let reply_quote = doc
        .get("reply_quote_in_prompt")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if reply_quote {
        println!("✓ reply_quote_in_prompt = true (Discord replies prepend quoted text)");
    } else {
        println!("✓ reply_quote_in_prompt = false (replies forwarded as-is)");
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
    let mut found_pretool = false;

    if let Some(events) = hooks.and_then(|h| h.as_object()) {
        for kind in ["Stop", "Notification", "PreToolUse"] {
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
                            "PreToolUse" => found_pretool = true,
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
    if !found_pretool {
        println!("ℹ no PreToolUse hook entry — tool-use approval flow disabled (claude runs all tools without ask)");
    }
    Ok(())
}
