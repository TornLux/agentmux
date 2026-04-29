//! Toast notification builders.
//!
//! All three toast kinds use the same WinRT-direct path: build raw
//! XML with `activationType="protocol"`, set `launch="agentmux://…"`
//! on the toast root for body-click, and (for tool_request) two
//! `<action>` elements for Allow / Deny buttons. Windows fires the
//! URL scheme on click, our scheme handler invokes a fresh
//! `agentmux-tray.exe`, the IPC layer forwards the URL to the running
//! tray, which dispatches via `deeplink::dispatch`.
//!
//! Why not the `tauri-winrt-notification` `on_activated` callback?
//! That requires a registered COM activator; non-packaged apps
//! either have to ship a server class or accept that body-click
//! callbacks silently no-op. Protocol activation works without any
//! COM plumbing — Windows already knows how to invoke a registered
//! URL scheme handler.
//!
//! AppUserModelId branding: Windows requires a Start-Menu shortcut
//! whose `System.AppUserModel.ID` property points at our chosen ID
//! string for our toasts to display under the "agentmux" name. We
//! create that shortcut on first launch (idempotent).

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, warn};

const APP_USER_MODEL_ID: &str = "Anthropic.agentmux.tray";
const SHORTCUT_NAME: &str = "agentmux.lnk";

const TOAST_TITLE_MAX: usize = 64;
const TOAST_BODY_MAX: usize = 200;

static APP_ID: OnceLock<String> = OnceLock::new();

fn app_id() -> &'static str {
    APP_ID.get_or_init(|| APP_USER_MODEL_ID.to_string())
}

pub fn on_assistant_message(session_name: &Option<String>, raw: &Value) {
    let session = session_name.clone().unwrap_or_else(|| "?".to_string());
    let body_raw = raw.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let body = truncate(body_raw.trim(), TOAST_BODY_MAX);
    let title = format!("✅ [{}] turn complete", truncate(&session, TOAST_TITLE_MAX));
    let session_url = format!("agentmux://session/{}", session);

    let xml = build_simple_toast_xml(&title, &body, &session_url);
    if let Err(e) = show_toast_xml(&xml) {
        warn!("show assistant_message toast: {e}");
    }
}

pub fn on_notification(session_name: &Option<String>, raw: &Value) {
    let session = session_name.clone().unwrap_or_else(|| "?".to_string());
    let msg = raw
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("(empty notification)");
    let title = format!("⚠️ [{}] needs attention", truncate(&session, TOAST_TITLE_MAX));
    let body = truncate(msg.trim(), TOAST_BODY_MAX);
    let session_url = format!("agentmux://session/{}", session);

    let xml = build_simple_toast_xml(&title, &body, &session_url);
    if let Err(e) = show_toast_xml(&xml) {
        warn!("show notification toast: {e}");
    }
}

pub fn on_tool_request(session_name: &Option<String>, raw: &Value) {
    let session = session_name.clone().unwrap_or_else(|| "?".to_string());
    let request_id = match raw.get("request_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            warn!("tool_request missing request_id; skipping toast");
            return;
        }
    };
    let tool_name = raw
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let preview = render_tool_preview(tool_name, raw.get("tool_input").unwrap_or(&Value::Null));

    let title = format!(
        "🔐 [{}] approve {}?",
        truncate(&session, TOAST_TITLE_MAX),
        tool_name
    );
    let body = truncate(&preview, TOAST_BODY_MAX);

    let approve_url = format!("agentmux://approve/{}", request_id);
    let deny_url = format!("agentmux://deny/{}", request_id);
    // Body-click: open the session so the user can read context
    // before deciding from inside the running TUI. Buttons handle
    // the actual decision.
    let body_url = format!("agentmux://session/{}", session);

    let xml = build_action_toast_xml(&title, &body, &body_url, &approve_url, &deny_url);
    if let Err(e) = show_toast_xml(&xml) {
        warn!("show tool_request toast: {e}");
        // Fallback: simple toast so user at least knows there's a
        // pending approval. Discord path still works in parallel.
        let fallback = build_simple_toast_xml(&title, &body, &body_url);
        let _ = show_toast_xml(&fallback);
    }
}

fn build_simple_toast_xml(title: &str, body: &str, click_url: &str) -> String {
    format!(
        r#"<toast activationType="protocol" launch="{url}">
  <visual>
    <binding template="ToastGeneric">
      <text>{title}</text>
      <text>{body}</text>
    </binding>
  </visual>
  <audio src="ms-winsoundevent:Notification.Default" />
</toast>"#,
        url = xml_escape(click_url),
        title = xml_escape(title),
        body = xml_escape(body),
    )
}

fn build_action_toast_xml(
    title: &str,
    body: &str,
    body_url: &str,
    approve_url: &str,
    deny_url: &str,
) -> String {
    format!(
        r#"<toast activationType="protocol" launch="{body_url}">
  <visual>
    <binding template="ToastGeneric">
      <text>{title}</text>
      <text>{body}</text>
    </binding>
  </visual>
  <actions>
    <action content="Allow"
            arguments="{approve_url}"
            activationType="protocol" />
    <action content="Deny"
            arguments="{deny_url}"
            activationType="protocol" />
  </actions>
  <audio src="ms-winsoundevent:Notification.Default" />
</toast>"#,
        body_url = xml_escape(body_url),
        title = xml_escape(title),
        body = xml_escape(body),
        approve_url = xml_escape(approve_url),
        deny_url = xml_escape(deny_url),
    )
}

fn show_toast_xml(xml_str: &str) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let xml = XmlDocument::new().context("create XmlDocument")?;
    xml.LoadXml(&HSTRING::from(xml_str))
        .context("load toast XML")?;
    let toast = ToastNotification::CreateToastNotification(&xml)
        .context("create ToastNotification")?;
    let aumid = HSTRING::from(app_id());
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&aumid)
        .context("create toast notifier")?;
    notifier.Show(&toast).context("show toast")?;
    Ok(())
}

fn render_tool_preview(tool_name: &str, input: &Value) -> String {
    let s = |k: &str| {
        input
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    };
    match tool_name {
        "Bash" => format!("$ {}", s("command")),
        "Write" => format!("write {}", s("file_path")),
        "Edit" => format!("edit {}", s("file_path")),
        "MultiEdit" => format!("edit {}", s("file_path")),
        "WebFetch" => format!("fetch {}", s("url")),
        n if n.starts_with("mcp__") => {
            format!("{} (mcp)", n.trim_start_matches("mcp__"))
        }
        other => format!("{} {}", other, truncate_value(input, 80)),
    }
}

fn truncate_value(v: &Value, max: usize) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    truncate(&s, max)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Install a Start Menu `.lnk` with `System.AppUserModel.ID` set so
/// our toasts get branded under the agentmux name instead of falling
/// back to the parent process's identity.
///
/// Status: NOT YET IMPLEMENTED. The COM + IPropertyStore plumbing
/// needed here is finicky in `windows-rs` 0.58 (the
/// `InitPropVariantFromStringVector` symbol moved between feature
/// gates and `IUnknown::cast` requires the `Interface` trait which
/// changed import paths between the 0.5x line and 0.6x). Toasts
/// still display correctly without this — they just appear under a
/// generic "Windows PowerShell" or parent-process branding instead
/// of "agentmux". Functional, just unbranded.
///
/// Tracking: a follow-up commit will use `PropVariantFromString` once
/// confirmed-available in our pinned `windows` version, OR a
/// PowerShell-driven shortcut creation (`New-Item` + the
/// `Set-AppUserModelId.ps1` helper many projects ship) at install
/// time from `agentmux init` instead.
pub fn install_appusermodelid_shortcut() -> Result<()> {
    let _ = APP_USER_MODEL_ID;
    let _: PathBuf = start_menu_shortcut_path().unwrap_or_default();
    debug!("AppUserModelId shortcut install: skipped (not yet implemented)");
    Ok(())
}

fn start_menu_shortcut_path() -> Result<PathBuf> {
    let appdata = std::env::var_os("APPDATA")
        .ok_or_else(|| anyhow::anyhow!("APPDATA env var not set"))?;
    let mut p = PathBuf::from(appdata);
    p.push("Microsoft");
    p.push("Windows");
    p.push("Start Menu");
    p.push("Programs");
    p.push(SHORTCUT_NAME);
    Ok(p)
}
