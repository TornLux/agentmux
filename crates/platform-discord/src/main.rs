//! agentmux platform-discord.
//!
//! Two long-running tasks:
//!
//!   1. **serenity** — Discord gateway client. Inbound text from
//!      whitelisted users in whitelisted channels (per-channel binding,
//!      see `state.rs`) is forwarded to the bound session via
//!      `POST /sessions/:name/input`. Attachments are processed in
//!      `attachments.rs`.
//!   2. **broker WS subscriber** — connects to `ws://broker/ws`, reads
//!      annotated hook events, and either edits the placeholder posted
//!      by the gateway handler (`assistant_message`) or posts a fresh
//!      message (`notification`, or fallback when no placeholder is
//!      pending).

mod ansi;
mod attachments;
mod broker;
mod config;
mod handler;
mod progress;
mod slash;
mod state;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{Context, Result};
use serenity::all::{
    ChannelId, CreateMessage, EditMessage, GatewayIntents, Http, MessageId,
};
use serenity::Client;
use tracing::{info, warn};

use crate::broker::{run_ws_subscriber, BrokerClient};
use crate::config::DiscordConfig;
use crate::handler::Handler;
use crate::state::BotState;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    init_logging();

    let config = Arc::new(DiscordConfig::load().context("load discord.toml")?);
    let token = std::env::var(&config.token_env).with_context(|| {
        format!(
            "env var {} is not set — store the bot token there, not in discord.toml",
            config.token_env
        )
    })?;

    info!(
        "starting platform-discord: broker_http={} broker_ws={} default_session={} channels={} allowed_users={}",
        config.broker_http_url,
        config.broker_ws_url,
        config.default_session,
        config.channel_ids.len(),
        config.allowed_user_ids.len(),
    );

    let broker_client = Arc::new(BrokerClient::new(config.broker_http_url.clone()));
    let bot_state = BotState::new(default_bindings_path(), default_pending_path());

    // The WS subscriber pushes events into a queue; a small relay task
    // pops from the queue and uses serenity's `Http` to send / edit on
    // Discord. The split keeps the WS read loop from awaiting Discord
    // HTTP latency (which would back-pressure the broadcast).
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
    let event_tx_for_ws = event_tx.clone();
    let ws_url = config.broker_ws_url.clone();
    tokio::spawn(async move {
        let cb = Arc::new(move |v: serde_json::Value| {
            // Drop on backpressure rather than block — Discord rate
            // limits will hurt us long before this queue depth matters.
            let _ = event_tx_for_ws.try_send(v);
        });
        run_ws_subscriber(ws_url, cb).await;
    });

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MESSAGE_REACTIONS
        | GatewayIntents::DIRECT_MESSAGE_REACTIONS;

    let handler = Handler::new(config.clone(), broker_client.clone(), bot_state.clone());

    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .await
        .context("build serenity client")?;

    let http_for_relay = client.http.clone();
    let relay_config = config.clone();
    let relay_state = bot_state.clone();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            relay_event_to_discord(&event, &http_for_relay, &relay_config, &relay_state).await;
        }
    });

    // Crash-recovery: clean up `💭 working…` placeholders left by a
    // previous bot instance that didn't get to edit them. We don't
    // try to recover the in-flight turn (the assistant_message event
    // for it likely flew past before our WS subscriber attached) —
    // just put a clear error in the message so the user knows what
    // happened. Done before client.start() so it doesn't race with
    // new turns.
    cleanup_orphans(&client.http, &bot_state).await;

    info!("connecting to Discord gateway...");
    if let Err(e) = client.start().await {
        anyhow::bail!("serenity client error: {e}");
    }
    Ok(())
}

fn init_logging() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,serenity=warn".into());
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

/// Render one annotated hook event to Discord.
///
/// `assistant_message` first tries to **edit** the placeholder posted
/// by the gateway handler when the user prompted this turn (looked up
/// via `BotState::pop_pending`). If no placeholder is queued (e.g. the
/// turn was triggered by a non-Discord viewer, or the bot restarted
/// between the prompt and the reply) we fall back to posting a fresh
/// message — into a channel currently bound to this session, otherwise
/// into `channel_ids[0]`.
async fn relay_event_to_discord(
    event: &serde_json::Value,
    http: &Arc<Http>,
    config: &DiscordConfig,
    state: &Arc<BotState>,
) {
    let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let session_name = event
        .get("session_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();

    match kind {
        "assistant_message" => {
            let body = event
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if body.is_empty() {
                return;
            }
            relay_assistant(http, config, state, &session_name, &body).await;
        }
        "notification" => {
            let m = event
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if m.is_empty() {
                return;
            }
            // Drop idle pings unless the user opted into them. Claude
            // Code's idle hook fires shortly after every reply (TUI
            // sits at the prompt → Notification with this message),
            // which most users hate. Permission prompts have a
            // different shape and pass through.
            if !config.notify_on_idle && is_idle_ping(m) {
                tracing::debug!("suppressing idle notification: {m}");
                return;
            }
            let body = format!("⚠️ **[{session_name}]** {m}");
            // Notifications never edit a placeholder — they're
            // unsolicited and may arrive between turns.
            send_fresh(http, config, state, &session_name, &body).await;
        }
        "tool_request" => {
            relay_tool_request(http, config, state, &session_name, event).await;
        }
        "tool_progress" => {
            let tool_name = event
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if tool_name.is_empty() {
                return;
            }
            let tool_input = event
                .get("tool_input")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            relay_tool_progress(http, state, &session_name, tool_name, &tool_input).await;
        }
        other => {
            tracing::debug!("ws event ignored: type={other}");
        }
    }
}

/// Edit the in-flight placeholder with a running narrative of tool
/// calls. We **peek** rather than pop: the placeholder stays in the
/// queue until `assistant_message` arrives and `pop_pending` consumes
/// it for the final answer. Throttled (`PROGRESS_EDIT_THROTTLE`) so
/// fast bursts of tools don't melt Discord's per-message edit budget;
/// skipped events still mutate the in-memory history, so the next
/// allowed edit shows the cumulative state.
async fn relay_tool_progress(
    http: &Arc<Http>,
    state: &Arc<BotState>,
    session_name: &str,
    tool_name: &str,
    tool_input: &serde_json::Value,
) {
    let pending = match state.peek_pending(session_name).await {
        Some(p) => p,
        None => {
            // No Discord-originated turn in flight for this session
            // (e.g. user is driving from Terminal and hook-posttool's
            // local-viewer bail didn't trigger because the viewer
            // detached mid-turn). Silently drop.
            tracing::debug!("tool_progress session={session_name} dropped: no pending");
            return;
        }
    };
    let line = progress::render_tool_progress(tool_name, tool_input);
    let body = {
        let mut prog = match pending.progress.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!("progress mutex poisoned for session={session_name}: {e}");
                return;
            }
        };
        prog.history.push(line);
        let now = std::time::Instant::now();
        if let Some(last) = prog.last_edit_at {
            if now.duration_since(last) < progress::PROGRESS_EDIT_THROTTLE {
                // Throttled: skip the edit but keep the line in
                // history so the next non-throttled event renders the
                // cumulative state.
                return;
            }
        }
        prog.last_edit_at = Some(now);
        progress::render_placeholder(&prog.history)
    };

    let channel = ChannelId::new(pending.channel_id);
    let mid = MessageId::new(pending.message_id);
    if let Err(e) = channel
        .edit_message(http, mid, EditMessage::new().content(body))
        .await
    {
        warn!("edit progress for session={session_name}: {e}");
    }
}

async fn relay_assistant(
    http: &Arc<Http>,
    config: &DiscordConfig,
    state: &Arc<BotState>,
    session_name: &str,
    body: &str,
) {
    let chunks = chunked(body, config.max_message_chars);
    if let Some(pending) = state.pop_pending(session_name).await {
        // Stop the typing-indicator background task immediately so
        // the bot doesn't keep "is typing" flickering after the
        // answer landed.
        pending.typing_cancel.store(true, Ordering::Release);

        let channel = ChannelId::new(pending.channel_id);
        let mid = MessageId::new(pending.message_id);
        let first = chunks.first().cloned().unwrap_or_default();
        if let Err(e) = channel
            .edit_message(http, mid, EditMessage::new().content(first))
            .await
        {
            warn!("edit placeholder for session={session_name}: {e}");
            // Fall through and post the entire body fresh — placeholder
            // edit failed, but the user still needs to see the answer.
            send_fresh_chunks(
                http,
                channel,
                session_name,
                &chunks,
                /*include_first=*/ true,
                Some(state),
            )
            .await;
            return;
        }
        // Record the placeholder's message id → session so future
        // user replies (Discord Reply UI) targeting this answer get
        // routed back to the same session.
        state
            .record_assistant_message(pending.message_id, session_name.to_string())
            .await;
        for chunk in chunks.iter().skip(1) {
            match channel
                .send_message(http, CreateMessage::new().content(chunk))
                .await
            {
                Ok(m) => {
                    state
                        .record_assistant_message(m.id.get(), session_name.to_string())
                        .await;
                }
                Err(e) => {
                    warn!("follow-up send for session={session_name}: {e}");
                    break;
                }
            }
        }
        return;
    }

    // No placeholder waiting — post fresh, pre-pending the session
    // header so the user knows which session woke up.
    let display = format!("**[{session_name}]**\n{body}");
    send_fresh(http, config, state, session_name, &display).await;
}

/// Render a `tool_request` event to Discord with Allow / Deny
/// buttons whose `custom_id`s carry the broker's `request_id` so the
/// click handler in `handler.rs::interaction_create` can POST the
/// decision back to `/tool-decision/:request_id`.
async fn relay_tool_request(
    http: &Arc<Http>,
    config: &DiscordConfig,
    state: &Arc<BotState>,
    session_name: &str,
    event: &serde_json::Value,
) {
    use serenity::all::{ButtonStyle, CreateActionRow, CreateButton};
    let request_id = match event.get("request_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            warn!("tool_request without request_id, dropping");
            return;
        }
    };
    // When a local viewer (claude-attach or web) is watching the session,
    // suppress the Discord prompt — broker still broadcasts the event so
    // tray/web surfaces handle the approval; Discord would just be noise.
    if event
        .get("local_viewer_attached")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        info!(
            "tool_request id={} skipped: local viewer attached to session {}",
            request_id, session_name
        );
        return;
    }
    let tool_name = event
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let summary = format_tool_request(tool_name, event.get("tool_input"));
    let body = format!(
        "⚠️ **[{session_name}]** approval needed\n**{tool_name}**\n```\n{summary}\n```"
    );

    let Some(channel) = pick_channel(config, state, session_name).await else {
        warn!("no destination for tool_request id={request_id}");
        return;
    };

    let row = CreateActionRow::Buttons(vec![
        CreateButton::new(format!("tool:allow:{request_id}"))
            .style(ButtonStyle::Success)
            .label("Allow")
            .emoji('✅'),
        CreateButton::new(format!("tool:deny:{request_id}"))
            .style(ButtonStyle::Danger)
            .label("Deny")
            .emoji('❌'),
    ]);

    let chunks = chunked(&body, config.max_message_chars);
    let first = chunks.first().cloned().unwrap_or_default();
    let msg_builder = CreateMessage::new().content(first).components(vec![row]);
    if let Err(e) = channel.send_message(http, msg_builder).await {
        warn!("send tool_request: {e}");
    }
}

/// Best-effort one-line excerpt of the tool's input for the Discord
/// prompt. Bash → command; Edit/Write → path; everything else falls
/// back to a compact JSON dump capped at 300 chars.
fn format_tool_request(tool_name: &str, input: Option<&serde_json::Value>) -> String {
    let Some(input) = input else {
        return "(no input)".to_string();
    };
    let pick = match tool_name {
        "Bash" => input.get("command").and_then(|v| v.as_str()),
        "Edit" | "Write" | "MultiEdit" => input.get("file_path").and_then(|v| v.as_str()),
        "NotebookEdit" => input.get("notebook_path").and_then(|v| v.as_str()),
        _ => None,
    };
    if let Some(s) = pick {
        let truncated: String = s.chars().take(800).collect();
        return truncated;
    }
    let dump = serde_json::to_string(input).unwrap_or_default();
    if dump.chars().count() > 300 {
        let mut t: String = dump.chars().take(300).collect();
        t.push('…');
        t
    } else {
        dump
    }
}

async fn send_fresh(
    http: &Arc<Http>,
    config: &DiscordConfig,
    state: &Arc<BotState>,
    session_name: &str,
    body: &str,
) {
    let Some(channel) = pick_channel(config, state, session_name).await else {
        warn!("no destination channel for session={session_name}");
        return;
    };
    let chunks = chunked(body, config.max_message_chars);
    send_fresh_chunks(http, channel, session_name, &chunks, /*include_first=*/ true, Some(state)).await;
}

async fn send_fresh_chunks(
    http: &Arc<Http>,
    channel: ChannelId,
    session_name: &str,
    chunks: &[String],
    include_first: bool,
    state: Option<&Arc<BotState>>,
) {
    let start = if include_first { 0 } else { 1 };
    for chunk in chunks.iter().skip(start) {
        match channel
            .send_message(http, CreateMessage::new().content(chunk))
            .await
        {
            Ok(m) => {
                // Record so a future user reply targeting this
                // message routes back to the same session.
                if let Some(s) = state {
                    s.record_assistant_message(m.id.get(), session_name.to_string())
                        .await;
                }
            }
            Err(e) => {
                warn!("discord send for session={session_name}: {e}");
                break;
            }
        }
    }
}

/// Prefer a channel currently bound to this session (any of them, if
/// multiple are bound — first wins). Fall back to `channel_ids[0]` so
/// unsolicited events from never-bound sessions still surface.
async fn pick_channel(
    config: &DiscordConfig,
    state: &Arc<BotState>,
    session_name: &str,
) -> Option<ChannelId> {
    let bound = state.channels_for(session_name).await;
    if let Some(c) = bound.first() {
        return Some(ChannelId::new(*c));
    }
    config.channel_ids.first().copied().map(ChannelId::new)
}

/// True if a Notification hook message is the Claude Code "idle"
/// ping — TUI is at the prompt with no user input, fired roughly
/// once after every reply. Substring match (case-insensitive) so
/// minor wording tweaks in claude code releases don't unseat us.
fn is_idle_ping(m: &str) -> bool {
    let lower = m.to_ascii_lowercase();
    lower.contains("waiting for your input") || lower.contains("waiting for input")
}

/// Split `s` into chunks ≤ `max` characters, preferring to break on
/// newlines so a code block isn't split mid-line. Code-fence balancing
/// is not handled here — true streaming will need it when chunked
/// `assistant_message` events land.
fn chunked(s: &str, max: usize) -> Vec<String> {
    if s.chars().count() <= max {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in s.split_inclusive('\n') {
        if cur.chars().count() + line.chars().count() > max && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if line.chars().count() > max {
            // Single line longer than max — hard split by chars.
            let mut buf = String::new();
            for c in line.chars() {
                if buf.chars().count() + 1 > max {
                    out.push(std::mem::take(&mut buf));
                }
                buf.push(c);
            }
            cur.push_str(&buf);
        } else {
            cur.push_str(line);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Resolved path of `discord-bindings.toml` under the shared per-user
/// app-data dir (Windows: `%LOCALAPPDATA%\agentmux\`; Linux:
/// `~/.local/share/agentmux/`).
fn default_bindings_path() -> PathBuf {
    shared::config::local_appdata_dir().join("discord-bindings.toml")
}

/// Resolved path of `discord-pending.toml` — sibling of bindings,
/// rewritten on every push/pop and removed on clean shutdown of an
/// empty queue. Presence of a non-empty file at startup ⇔ previous
/// bot instance exited mid-turn.
fn default_pending_path() -> PathBuf {
    shared::config::local_appdata_dir().join("discord-pending.toml")
}

/// Edit each orphaned `💭 working…` placeholder left over from a
/// previous bot run into a clear error state, then clear the
/// pending file. Best-effort: failures (deleted message, no perms)
/// are logged at debug level since the user's experience already
/// degraded once and we don't want the recovery itself to be noisy.
async fn cleanup_orphans(http: &Arc<Http>, state: &Arc<BotState>) {
    let orphans = state.take_orphans();
    if orphans.is_empty() {
        return;
    }
    info!("cleaning up {} orphan placeholder(s) from previous run", orphans.len());
    for o in orphans {
        let channel = ChannelId::new(o.channel_id);
        let mid = MessageId::new(o.message_id);
        let body = format!(
            "❌ this turn was interrupted by a bot restart (session: `{}`)",
            o.session
        );
        if let Err(e) = channel
            .edit_message(http, mid, EditMessage::new().content(body))
            .await
        {
            tracing::debug!("orphan edit {mid} (session={}): {e}", o.session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_short_returns_single() {
        assert_eq!(chunked("hello", 100), vec!["hello".to_string()]);
    }

    #[test]
    fn chunked_splits_on_newline() {
        let parts = chunked("aaaa\nbbbb\ncccc\n", 8);
        assert!(parts.len() >= 2);
        for p in &parts {
            assert!(p.chars().count() <= 8, "{p:?} exceeds limit");
        }
    }

    #[test]
    fn chunked_hard_splits_long_line() {
        let parts = chunked("xxxxxxxxxxxxxxxxxxxx", 5);
        assert!(parts.len() >= 4);
        for p in &parts {
            assert!(p.chars().count() <= 5);
        }
    }

    #[test]
    fn idle_ping_matches() {
        assert!(is_idle_ping("Claude is waiting for your input"));
        assert!(is_idle_ping("CLAUDE IS WAITING FOR INPUT"));
        assert!(is_idle_ping("  claude is waiting for your input  "));
    }

    #[test]
    fn idle_ping_does_not_match_permission() {
        assert!(!is_idle_ping("Claude needs your permission to use Bash"));
        assert!(!is_idle_ping("Claude wants to read /etc/passwd"));
    }
}
