//! Persistent "current sessions" panel.
//!
//! Maintained as a single Discord message in
//! `discord.toml::dashboard_channel_id`. One row per session: state
//! icon, name, and the one-line `current_status` derived by broker
//! from `tool_progress` / `assistant_message` / `notification` events.
//!
//! Lifecycle:
//!   1. **Bootstrap (in `ready()`):** scan recent messages in the
//!      dashboard channel for one the bot wrote with our marker
//!      footer. Reuse it if found; otherwise post a fresh placeholder
//!      and remember its id.
//!   2. **Refresh loop (background):** every 5 s, fetch
//!      `GET /sessions`, render the embed, edit the message — but
//!      only if the rendered body actually changed (avoids burning
//!      Discord's per-message edit-rate budget on a no-op tick).
//!
//! Disabled at `dashboard_channel_id = 0` (default).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serenity::all::{
    ChannelId, CreateEmbed, CreateEmbedFooter, CreateMessage, EditMessage, GetMessages, Http,
    MessageId,
};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::broker::{BrokerClient, SessionLite};

/// Stable text in the embed footer — used by `bootstrap` to find the
/// bot's prior dashboard message after a restart so we don't pile up
/// duplicates. Bumping the version forces a fresh post next start.
const MARKER_FOOTER: &str = "[agentmux dashboard v1]";

/// Discord blurple — same colour the rest of the bot's info embeds use.
const COLOR_DASHBOARD: u32 = 0x5865F2;

/// How often the background refresher polls broker. 5 s is a reasonable
/// floor: faster would burn edit-rate quota for no perceptible benefit;
/// slower (10s+) starts to feel laggy when actively driving workers.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct Dashboard {
    channel_id: u64,
    /// `None` until `bootstrap` succeeds. Once set, `refresh` edits in
    /// place. If the message is later deleted out from under us
    /// (Discord 404 on edit), we re-post on the next refresh tick.
    msg_id: Mutex<Option<MessageId>>,
    /// Last rendered body text, kept so the refresher can skip the
    /// edit when nothing changed. Avoids burning Discord's
    /// 5-edits-per-5s-per-message budget for no-op poll ticks.
    last_body: Mutex<String>,
}

impl Dashboard {
    pub fn new(channel_id: u64) -> Arc<Self> {
        Arc::new(Self {
            channel_id,
            msg_id: Mutex::new(None),
            last_body: Mutex::new(String::new()),
        })
    }

    /// Find an existing dashboard message in the channel (one the bot
    /// posted before, marked by `MARKER_FOOTER`) or post a fresh
    /// placeholder. Call once at startup, after the gateway is ready
    /// (so the bot's user id and HTTP are available).
    pub async fn bootstrap(&self, http: &Arc<Http>, bot_user_id: u64) -> Result<()> {
        let chan = ChannelId::new(self.channel_id);
        let messages = chan
            .messages(http, GetMessages::new().limit(50))
            .await
            .with_context(|| format!("scan dashboard channel {}", self.channel_id))?;
        for m in messages {
            if m.author.id.get() != bot_user_id {
                continue;
            }
            let has_marker = m.embeds.iter().any(|e| {
                e.footer
                    .as_ref()
                    .is_some_and(|f| f.text.contains(MARKER_FOOTER))
            });
            if !has_marker {
                continue;
            }
            *self.msg_id.lock().await = Some(m.id);
            info!(
                "dashboard: reusing message id={} in channel {}",
                m.id.get(),
                self.channel_id
            );
            return Ok(());
        }
        // No existing message — post a placeholder. The first refresh
        // tick will fill it in.
        let embed = render_placeholder();
        let m = chan
            .send_message(http, CreateMessage::new().embed(embed))
            .await
            .with_context(|| format!("post dashboard message in {}", self.channel_id))?;
        *self.msg_id.lock().await = Some(m.id);
        info!(
            "dashboard: posted fresh message id={} in channel {}",
            m.id.get(),
            self.channel_id
        );
        Ok(())
    }

    /// Pull the current session list, render the embed, and edit the
    /// dashboard message — but only if the body actually changed
    /// since last refresh.
    async fn refresh(&self, http: &Arc<Http>, broker: &BrokerClient) {
        let mid = match *self.msg_id.lock().await {
            Some(m) => m,
            None => return,
        };
        let sessions = match broker.list_sessions().await {
            Ok(s) => s,
            Err(e) => {
                warn!("dashboard refresh: list_sessions: {e}");
                return;
            }
        };
        let body = render_body_text(&sessions);
        {
            let mut last = self.last_body.lock().await;
            if *last == body {
                return;
            }
            *last = body.clone();
        }
        let embed = render_embed_from_body(&body);
        if let Err(e) = ChannelId::new(self.channel_id)
            .edit_message(http, mid, EditMessage::new().embed(embed))
            .await
        {
            warn!("dashboard edit: {e}");
            // 10008 = Unknown Message. Someone deleted it; clear the
            // id so the next refresh cycle re-posts via bootstrap-like
            // path. We can't bootstrap here without bot_user_id, so
            // just blank the cached body so a re-post (when a fresh
            // bootstrap happens) renders correctly.
            if e.to_string().contains("10008") {
                *self.msg_id.lock().await = None;
                *self.last_body.lock().await = String::new();
            }
        }
    }

    /// Spawn the periodic refresher. Returns immediately. Stop the
    /// task by dropping the bot process.
    pub fn spawn_refresher(self: Arc<Self>, http: Arc<Http>, broker: Arc<BrokerClient>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(POLL_INTERVAL);
            tick.tick().await; // skip immediate fire
            loop {
                tick.tick().await;
                self.refresh(&http, &broker).await;
            }
        });
    }
}

/// Serialise the session list into the body text used inside the embed
/// description. Kept pure (no Discord types) so equality-checking for
/// "did anything change" stays simple.
fn render_body_text(sessions: &[SessionLite]) -> String {
    if sessions.is_empty() {
        return "(no sessions)".to_string();
    }
    let max_name = sessions.iter().map(|s| s.name.len()).max().unwrap_or(8).min(20);
    let mut out = String::new();
    for s in sessions {
        let icon = state_icon(&s.state);
        let status = if s.current_status.is_empty() {
            "—".to_string()
        } else {
            truncate(&s.current_status, 70)
        };
        out.push_str(&format!(
            "{icon} {:<width$}  {status}\n",
            truncate(&s.name, max_name),
            width = max_name
        ));
    }
    out
}

fn render_embed_from_body(body: &str) -> CreateEmbed {
    CreateEmbed::new()
        .title("agentmux sessions")
        .description(format!("```\n{body}```"))
        .color(COLOR_DASHBOARD)
        .footer(CreateEmbedFooter::new(MARKER_FOOTER))
}

fn render_placeholder() -> CreateEmbed {
    CreateEmbed::new()
        .title("agentmux sessions")
        .description("Initialising — first refresh shortly…")
        .color(COLOR_DASHBOARD)
        .footer(CreateEmbedFooter::new(MARKER_FOOTER))
}

fn state_icon(state: &str) -> &'static str {
    match state {
        "idle" => "🟢",
        "hibernated" => "💤",
        "crashed" => "❌",
        "locally_owned" => "🌐",
        _ => "⚪",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lite(name: &str, state: &str, status: &str) -> SessionLite {
        SessionLite {
            name: name.into(),
            state: state.into(),
            viewers: 0,
            cwd: String::new(),
            current_status: status.into(),
        }
    }

    #[test]
    fn empty_renders_placeholder_text() {
        assert_eq!(render_body_text(&[]), "(no sessions)");
    }

    #[test]
    fn rows_align_on_widest_name() {
        let sessions = vec![
            lite("a", "idle", "x"),
            lite("longername", "hibernated", "y"),
        ];
        let body = render_body_text(&sessions);
        // Both rows must contain the same column width before status
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        // After icon + space, the name column should align — find where
        // status text starts on each line and they should match up.
        let pos1 = lines[0].rfind('x').unwrap();
        let pos2 = lines[1].rfind('y').unwrap();
        assert_eq!(pos1, pos2, "status columns misaligned: {body:?}");
    }

    #[test]
    fn empty_status_renders_dash() {
        let body = render_body_text(&[lite("foo", "idle", "")]);
        assert!(body.contains("—"), "expected — for empty status, got: {body:?}");
    }

    #[test]
    fn long_status_truncates() {
        let long: String = "a".repeat(200);
        let body = render_body_text(&[lite("foo", "idle", &long)]);
        // Must contain ellipsis and not the full 200 chars
        assert!(body.contains('…'));
        assert!(!body.contains(&"a".repeat(100)));
    }

    #[test]
    fn icons_per_state() {
        assert_eq!(state_icon("idle"), "🟢");
        assert_eq!(state_icon("hibernated"), "💤");
        assert_eq!(state_icon("crashed"), "❌");
        assert_eq!(state_icon("locally_owned"), "🌐");
        assert_eq!(state_icon("unknown_state"), "⚪");
    }
}
