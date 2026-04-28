//! agentmux platform-discord — Phase 6a (MVP).
//!
//! Two long-running tasks:
//!
//!   1. **serenity** — Discord gateway client. Inbound text from
//!      whitelisted users in whitelisted channels is forwarded to the
//!      bound session via `POST /sessions/:name/input`.
//!   2. **broker WS subscriber** — connects to `ws://broker/ws`, reads
//!      annotated hook events, and posts them to the primary Discord
//!      channel via the gateway client's `Http`.
//!
//! Multi-channel and per-channel binding (one session per channel) are
//! deferred until a second IM platform forces the abstraction.

mod broker;
mod config;
mod handler;

use std::sync::Arc;

use anyhow::{Context, Result};
use serenity::all::{ChannelId, CreateMessage, GatewayIntents, Http};
use serenity::Client;
use tracing::{info, warn};

use crate::broker::{run_ws_subscriber, BrokerClient};
use crate::config::DiscordConfig;
use crate::handler::Handler;

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

    // The WS subscriber pushes events into a queue; a small relay task
    // pops from the queue and uses serenity's `Http` to send to Discord.
    // This split is so the WS read loop never awaits a Discord HTTP
    // call (which would back-pressure the broadcast).
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
        | GatewayIntents::MESSAGE_CONTENT;

    let handler = Handler::new(config.clone(), broker_client.clone());

    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .await
        .context("build serenity client")?;

    let http_for_relay = client.http.clone();
    let relay_config = config.clone();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            relay_event_to_discord(&event, &http_for_relay, &relay_config).await;
        }
    });

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

/// Render one annotated hook event to Discord. MVP only handles
/// `assistant_message` and `notification`; everything else is silently
/// dropped (with a debug log) until a use case shows up.
async fn relay_event_to_discord(
    event: &serde_json::Value,
    http: &Arc<Http>,
    config: &DiscordConfig,
) {
    let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let session_name = event
        .get("session_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    let body = match kind {
        "assistant_message" => {
            let body = event.get("body").and_then(|v| v.as_str()).unwrap_or("").trim();
            if body.is_empty() {
                return;
            }
            format!("**[{session_name}]**\n{body}")
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
            format!("⚠️ **[{session_name}]** {m}")
        }
        other => {
            tracing::debug!("ws event ignored: type={other}");
            return;
        }
    };

    // Pick the first channel id as the destination. The PLAN's
    // per-channel binding model needs the second IM platform to drive
    // the design — single-channel use is the common case for now.
    let channel = match config.channel_ids.first() {
        Some(id) => ChannelId::new(*id),
        None => {
            warn!("no channel_ids configured — dropping event");
            return;
        }
    };

    for chunk in chunked(&body, config.max_message_chars) {
        let msg = CreateMessage::new().content(chunk);
        if let Err(e) = channel.send_message(http, msg).await {
            warn!("discord send: {e}");
            break;
        }
    }
}

/// Split `s` into chunks ≤ `max` characters, preferring to break on
/// newlines so a code block isn't split mid-line. Code-fence balancing
/// is not handled here — turn-level markdown that needs it can be
/// added when StreamingUpdate / Bulk intents land.
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
        // each line ≤ 8 chars but two together exceed; expect splits
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
}
