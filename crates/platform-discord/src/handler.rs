//! Discord-side event handler. Implements `serenity::EventHandler`,
//! forwarding allowed-user messages to the bound session via the
//! broker HTTP API and handling a small set of `!` meta-commands.

use std::sync::Arc;

use serenity::all::{Context, EventHandler, Message, Ready};
use serenity::async_trait;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::broker::BrokerClient;
use crate::config::DiscordConfig;

pub struct Handler {
    pub config: Arc<DiscordConfig>,
    pub broker: Arc<BrokerClient>,
    /// Name of the session the bot is currently bound to. Mutex
    /// because `!attach` mutates from message handlers concurrently.
    pub current_session: Mutex<String>,
}

impl Handler {
    pub fn new(config: Arc<DiscordConfig>, broker: Arc<BrokerClient>) -> Self {
        let initial = config.default_session.clone();
        Self {
            config,
            broker,
            current_session: Mutex::new(initial),
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(
            "discord ready as {} (id={}); bound to session={}",
            ready.user.name,
            ready.user.id.get(),
            self.config.default_session,
        );
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        let uid = msg.author.id.get();
        if !self.config.allowed_user_ids.contains(&uid) {
            // Loud only at debug level so a busy public Server doesn't
            // spam the log; the user already knows their whitelist.
            tracing::debug!("ignoring message from non-whitelisted user {uid}");
            return;
        }
        let cid = msg.channel_id.get();
        if !self.config.channel_ids.is_empty() && !self.config.channel_ids.contains(&cid) {
            tracing::debug!("ignoring message in non-whitelisted channel {cid}");
            return;
        }

        let text = msg.content.trim();
        if text.is_empty() {
            return;
        }

        if let Some(rest) = text.strip_prefix('!') {
            self.handle_command(&ctx, &msg, rest).await;
            return;
        }

        let session = self.current_session.lock().await.clone();
        info!(
            "forwarding to session={} from user={} channel={} chars={}",
            session,
            uid,
            cid,
            text.chars().count()
        );
        match self.broker.send_input(&session, text).await {
            Ok(_) => {
                info!("forward ok ({} → {session})", uid);
                let _ = msg
                    .react(&ctx.http, serenity::all::ReactionType::Unicode("✅".into()))
                    .await;
            }
            Err(e) => {
                warn!("forward to {session}: {e:#}");
                let _ = msg
                    .reply(&ctx.http, format!("❌ forward to `{session}`: {e}"))
                    .await;
            }
        }
    }
}

impl Handler {
    async fn handle_command(&self, ctx: &Context, msg: &Message, rest: &str) {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("").trim();
        let arg = parts.next().unwrap_or("").trim();

        match cmd {
            "ls" | "sessions" => self.cmd_ls(ctx, msg).await,
            "attach" => self.cmd_attach(ctx, msg, arg).await,
            "status" => self.cmd_status(ctx, msg).await,
            "help" => {
                let body = "**agentmux discord commands**\n\
                            `!ls` — list sessions and which one is bound\n\
                            `!attach <name>` — switch the binding\n\
                            `!status` — show the currently bound session\n\
                            `!help` — this message\n\
                            (any other text is forwarded to the bound session as a prompt)";
                let _ = msg.reply(&ctx.http, body).await;
            }
            other => {
                let _ = msg
                    .reply(&ctx.http, format!("unknown command `!{other}` — try `!help`"))
                    .await;
            }
        }
    }

    async fn cmd_ls(&self, ctx: &Context, msg: &Message) {
        let bound = self.current_session.lock().await.clone();
        let list = match self.broker.list_sessions().await {
            Ok(l) => l,
            Err(e) => {
                let _ = msg.reply(&ctx.http, format!("❌ broker: {e}")).await;
                return;
            }
        };
        let mut out = String::from("```\n");
        if list.is_empty() {
            out.push_str("(no sessions)\n");
        } else {
            for s in &list {
                let marker = if s.name == bound { "▶ " } else { "  " };
                out.push_str(&format!(
                    "{marker}{:<18}  {:<11}  viewers={}  cwd={}\n",
                    truncate(&s.name, 18),
                    s.state,
                    s.viewers,
                    truncate(&s.cwd, 40),
                ));
            }
        }
        out.push_str("```\n▶ = bot's current binding");
        let _ = msg.reply(&ctx.http, out).await;
    }

    async fn cmd_attach(&self, ctx: &Context, msg: &Message, name: &str) {
        if name.is_empty() {
            let _ = msg.reply(&ctx.http, "usage: `!attach <name>`").await;
            return;
        }
        let exists = match self.broker.list_sessions().await {
            Ok(l) => l.iter().any(|s| s.name == name),
            Err(e) => {
                let _ = msg.reply(&ctx.http, format!("❌ broker: {e}")).await;
                return;
            }
        };
        if !exists {
            let _ = msg
                .reply(&ctx.http, format!("❌ no session named `{name}`"))
                .await;
            return;
        }
        *self.current_session.lock().await = name.to_string();
        let _ = msg
            .reply(&ctx.http, format!("✅ bound to session `{name}`"))
            .await;
    }

    async fn cmd_status(&self, ctx: &Context, msg: &Message) {
        let bound = self.current_session.lock().await.clone();
        let _ = msg
            .reply(&ctx.http, format!("currently bound to session `{bound}`"))
            .await;
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}
