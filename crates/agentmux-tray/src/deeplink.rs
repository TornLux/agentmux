//! `agentmux://` URL scheme: handler registration in HKCU + parser +
//! runtime dispatcher.
//!
//! Three URL forms we care about:
//!
//!   * `agentmux://session/<name>` — open the named session in a
//!     fresh `claude-attach.exe` window. Triggered by the user
//!     clicking the toast body.
//!   * `agentmux://approve/<request_id>` — POST `/tool-decision/<id>`
//!     with `{"allow": true}`. Triggered by the `[Allow]` action
//!     button on a `tool_request` toast.
//!   * `agentmux://deny/<request_id>` — same with `allow: false`.
//!
//! Registration happens in `HKCU\Software\Classes\agentmux` so we
//! never need admin rights. Idempotent: re-running just rewrites the
//! command path to the current exe.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tracing::{debug, info};
use winreg::enums::*;
use winreg::RegKey;

use crate::broker::BrokerClient;

/// Parsed deeplink. Variants line up 1-1 with URL forms above.
#[derive(Debug, Clone)]
pub enum DeepLink {
    OpenSession { name: String },
    Approve { request_id: String },
    Deny { request_id: String },
}

impl DeepLink {
    /// Parse `agentmux://<verb>/<arg>`. Returns `None` for malformed or
    /// unknown verbs — callers log + ignore rather than crash on a
    /// stray click of an old/typo URL.
    pub fn parse(raw: &str) -> Option<Self> {
        // The url crate's `Url::parse` would canonicalise some chars
        // we'd rather not touch (e.g. percent-decode the request id),
        // so do a tiny hand parse instead.
        let body = raw.strip_prefix("agentmux://")?;
        let body = body.trim_end_matches('/');
        let mut parts = body.splitn(2, '/');
        let verb = parts.next()?;
        let arg = parts.next().unwrap_or("");
        if arg.is_empty() {
            return None;
        }
        match verb {
            "session" => Some(DeepLink::OpenSession { name: arg.to_string() }),
            "approve" => Some(DeepLink::Approve { request_id: arg.to_string() }),
            "deny" => Some(DeepLink::Deny { request_id: arg.to_string() }),
            _ => None,
        }
    }
}

/// Run the action a deeplink describes. Async because two of the
/// three POST to the broker.
pub async fn dispatch(broker: Arc<BrokerClient>, link: DeepLink) -> Result<()> {
    match link {
        DeepLink::OpenSession { name } => spawn_claude_attach(&name),
        DeepLink::Approve { request_id } => {
            broker
                .post_tool_decision(&request_id, true, "approved via toast")
                .await?;
            info!("approved tool request {request_id} via toast");
            Ok(())
        }
        DeepLink::Deny { request_id } => {
            broker
                .post_tool_decision(&request_id, false, "denied via toast")
                .await?;
            info!("denied tool request {request_id} via toast");
            Ok(())
        }
    }
}

/// Launch `claude-attach.exe --session <name>` in a fresh Windows
/// Terminal window. We *do not* try to attach a console to ourselves —
/// the tray is a no-window background app. Falls back to a bare
/// `cmd /c start` if `wt.exe` isn't installed (rare on modern Win).
fn spawn_claude_attach(session: &str) -> Result<()> {
    let claude_attach = locate_claude_attach()?;
    let claude_attach_str = claude_attach.to_string_lossy().to_string();

    // Prefer Windows Terminal. The `wt` flags here open a new window
    // with our session as the only tab — no profile munging needed.
    let wt = Command::new("wt.exe")
        .args([
            "new-tab",
            "--title",
            &format!("agentmux: {session}"),
            &claude_attach_str,
            "--session",
            session,
        ])
        .spawn();

    match wt {
        Ok(child) => {
            debug!("spawned wt.exe pid={} for session={session}", child.id());
            Ok(())
        }
        Err(_) => {
            // `wt.exe` missing → fall back to bare cmd. Less pretty
            // but always works.
            Command::new("cmd")
                .args([
                    "/C",
                    "start",
                    "agentmux",
                    &claude_attach_str,
                    "--session",
                    session,
                ])
                .spawn()
                .with_context(|| format!("spawn claude-attach for session={session}"))?;
            Ok(())
        }
    }
}

/// Best-effort locate `claude-attach.exe` next to our own exe (release
/// zip layout) or in the cargo target dir (dev layout).
fn locate_claude_attach() -> Result<PathBuf> {
    let our_exe = std::env::current_exe().context("locate current exe")?;
    let dir = our_exe
        .parent()
        .ok_or_else(|| anyhow!("current exe has no parent dir"))?;
    let candidate = dir.join("claude-attach.exe");
    if candidate.exists() {
        return Ok(candidate);
    }
    Err(anyhow!(
        "claude-attach.exe not found alongside agentmux-tray.exe at {}",
        dir.display()
    ))
}

/// Register `agentmux://` URL scheme in HKCU. No admin needed. Safe
/// to call every startup — just rewrites the launcher path to the
/// current exe so re-installing into a new folder Just Works.
pub fn register_url_scheme() -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let scheme_path = r"Software\Classes\agentmux";

    let exe = std::env::current_exe().context("locate current exe for scheme registration")?;
    let exe_str = exe.to_string_lossy().to_string();
    // Quoted path + URL placeholder. Windows substitutes `%1` with the
    // full URL when launching us via protocol activation.
    let command = format!(r#""{exe_str}" "%1""#);

    let (root, _) = hkcu
        .create_subkey(scheme_path)
        .context("create HKCU\\Software\\Classes\\agentmux")?;
    root.set_value("", &"URL:agentmux Protocol")?;
    root.set_value("URL Protocol", &"")?;

    let (icon_key, _) = root
        .create_subkey("DefaultIcon")
        .context("create DefaultIcon subkey")?;
    icon_key.set_value("", &format!("{exe_str},0"))?;

    let (shell_open_command, _) = root
        .create_subkey(r"shell\open\command")
        .context("create shell\\open\\command subkey")?;
    shell_open_command.set_value("", &command)?;

    debug!("agentmux:// URL scheme registered → {command}");
    Ok(())
}
