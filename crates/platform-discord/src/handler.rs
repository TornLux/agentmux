//! Discord-side event handler. Implements `serenity::EventHandler`,
//! forwarding allowed-user messages to the bound session via the
//! broker HTTP API and handling a small set of `!` meta-commands.

use std::sync::Arc;

use serenity::all::{Context, EventHandler, Message, Ready};
use serenity::async_trait;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::broker::{BrokerClient, SessionLite};
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
            "new" => self.cmd_new(ctx, msg, arg).await,
            "kill" => self.cmd_kill(ctx, msg, arg).await,
            "interrupt" => self.cmd_interrupt(ctx, msg).await,
            "restart" => self.cmd_restart(ctx, msg).await,
            "hibernate" => self.cmd_hibernate(ctx, msg).await,
            "help" => {
                let body = "**agentmux discord commands**\n\
                            `!ls` — list sessions and which is bound\n\
                            `!attach <name>` — switch the binding\n\
                            `!status` — show the currently bound session\n\
                            `!new [name] [-cwd path]` — create a session and bind to it (auto-names if omitted)\n\
                            `!kill <name>` — destroy a session (force)\n\
                            `!interrupt` — Ctrl+C the bound session (interrupt current turn)\n\
                            `!restart` — restart claude in the bound session (preserves history via --resume)\n\
                            `!hibernate` — put the bound session to sleep (next message wakes it)\n\
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

    async fn cmd_new(&self, ctx: &Context, msg: &Message, arg: &str) {
        // Accepted shapes:
        //   !new                       — auto-name, broker default cwd
        //   !new myname                — given name, broker default cwd
        //   !new myname C:\path        — given name, given cwd (positional)
        //   !new myname -cwd C:\path   — given name, given cwd (explicit flag)
        //   !new -cwd C:\path          — auto-name, given cwd
        // Names with whitespace are NOT supported — they break URL
        // routing on subsequent /input calls. The positional shape
        // splits on the first whitespace, so anything after the name
        // is treated as the cwd.
        let (name_part, cwd) = if let Some(idx) = arg.find(" -cwd ") {
            (
                arg[..idx].trim().to_string(),
                Some(arg[idx + 6..].trim().to_string()),
            )
        } else if let Some(rest) = arg.strip_prefix("-cwd ") {
            (String::new(), Some(rest.trim().to_string()))
        } else {
            let trimmed = arg.trim();
            match trimmed.split_once(char::is_whitespace) {
                Some((name, rest)) => (name.to_string(), Some(rest.trim().to_string())),
                None => (trimmed.to_string(), None),
            }
        };

        let name = if name_part.is_empty() {
            match self.broker.list_sessions().await {
                Ok(list) => auto_name(&list),
                Err(e) => {
                    let _ = msg.reply(&ctx.http, format!("❌ broker: {e}")).await;
                    return;
                }
            }
        } else {
            name_part
        };

        match self.broker.create_session(&name, cwd.as_deref()).await {
            Ok(_) => {
                *self.current_session.lock().await = name.clone();
                let extra = match cwd {
                    Some(c) => format!(" (cwd: `{c}`)"),
                    None => String::new(),
                };
                let _ = msg
                    .reply(&ctx.http, format!("✅ created and bound to `{name}`{extra}"))
                    .await;
            }
            Err(e) => {
                let _ = msg
                    .reply(&ctx.http, format!("❌ create `{name}`: {e}"))
                    .await;
            }
        }
    }

    async fn cmd_kill(&self, ctx: &Context, msg: &Message, name: &str) {
        if name.is_empty() {
            let _ = msg.reply(&ctx.http, "usage: `!kill <name>`").await;
            return;
        }
        let bound = self.current_session.lock().await.clone();
        let killing_bound = name == bound;

        match self.broker.delete_session(name).await {
            Ok(_) => {
                let mut reply = format!("✅ killed `{name}`");
                if killing_bound {
                    // Bound session is gone — pick another so the next
                    // chat message doesn't 404. Prefer a remaining
                    // session over the configured default (which may
                    // also be the one we just killed).
                    let next = match self.broker.list_sessions().await {
                        Ok(list) if !list.is_empty() => list[0].name.clone(),
                        _ => self.config.default_session.clone(),
                    };
                    *self.current_session.lock().await = next.clone();
                    reply.push_str(&format!(" (was bound — switched to `{next}`)"));
                }
                let _ = msg.reply(&ctx.http, reply).await;
            }
            Err(e) => {
                let _ = msg
                    .reply(&ctx.http, format!("❌ kill `{name}`: {e}"))
                    .await;
            }
        }
    }

    async fn cmd_interrupt(&self, ctx: &Context, msg: &Message) {
        let bound = self.current_session.lock().await.clone();
        match self.broker.interrupt_session(&bound).await {
            Ok(_) => {
                let _ = msg
                    .react(&ctx.http, serenity::all::ReactionType::Unicode("🛑".into()))
                    .await;
            }
            Err(e) => {
                let _ = msg
                    .reply(&ctx.http, format!("❌ interrupt `{bound}`: {e}"))
                    .await;
            }
        }
    }

    async fn cmd_restart(&self, ctx: &Context, msg: &Message) {
        let bound = self.current_session.lock().await.clone();
        match self.broker.restart_session(&bound).await {
            Ok(_) => {
                let _ = msg
                    .reply(&ctx.http, format!("🔄 restarted claude in `{bound}` (history preserved)"))
                    .await;
            }
            Err(e) => {
                let _ = msg
                    .reply(&ctx.http, format!("❌ restart `{bound}`: {e}"))
                    .await;
            }
        }
    }

    async fn cmd_hibernate(&self, ctx: &Context, msg: &Message) {
        let bound = self.current_session.lock().await.clone();
        match self.broker.hibernate_session(&bound).await {
            Ok(_) => {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("💤 hibernated `{bound}` — next message wakes it"),
                    )
                    .await;
            }
            Err(e) => {
                let _ = msg
                    .reply(&ctx.http, format!("❌ hibernate `{bound}`: {e}"))
                    .await;
            }
        }
    }
}

fn auto_name(existing: &[SessionLite]) -> String {
    let used: std::collections::HashSet<&str> = existing.iter().map(|s| s.name.as_str()).collect();
    for i in 1..u32::MAX {
        let candidate = format!("s{i}");
        if !used.contains(candidate.as_str()) {
            return candidate;
        }
    }
    "s_overflow".to_string()
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
