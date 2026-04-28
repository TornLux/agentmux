//! Discord-side event handler. Implements `serenity::EventHandler`,
//! forwarding allowed-user messages to the channel-bound session via the
//! broker HTTP API and handling a small set of `!` meta-commands.
//!
//! Two non-trivial behaviours sit here:
//!
//!  * **Per-channel binding** — each Discord channel maps to one session.
//!    First-time access in a channel binds it to `default_session`;
//!    `!attach <name>` rebinds the *current* channel. Outbound routing
//!    in `main.rs` uses the same map so an `assistant_message` event for
//!    session `s` lands in whichever channel(s) bind it.
//!  * **Placeholder + edit-in-place** — every forwarded user message
//!    posts a `💭 working...` reply immediately, registers the message
//!    id in `pending_replies[session]`, and the WS relay edits that
//!    message in place when the matching `assistant_message` arrives.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serenity::all::{
    Context, EditMessage, EventHandler, Message, ReactionType, Ready,
};
use serenity::async_trait;
use tracing::{info, warn};

use crate::ansi;
use crate::attachments;
use crate::broker::{BrokerClient, SessionLite};
use crate::config::DiscordConfig;
use crate::state::{BotState, PendingReply};

/// How long a placeholder waits for `assistant_message` before being
/// considered abandoned (and therefore freely overwritable by the next
/// turn). Real claude turns top out around a couple of minutes; 10
/// minutes is "user has clearly given up" territory.
const PENDING_TTL: Duration = Duration::from_secs(600);

pub struct Handler {
    pub config: Arc<DiscordConfig>,
    pub broker: Arc<BrokerClient>,
    pub state: Arc<BotState>,
}

impl Handler {
    pub fn new(
        config: Arc<DiscordConfig>,
        broker: Arc<BrokerClient>,
        state: Arc<BotState>,
    ) -> Self {
        Self {
            config,
            broker,
            state,
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(
            "discord ready as {} (id={}); default_session={} channels={} (per-channel binding)",
            ready.user.name,
            ready.user.id.get(),
            self.config.default_session,
            self.config.channel_ids.len(),
        );
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        let uid = msg.author.id.get();
        if !self.config.allowed_user_ids.contains(&uid) {
            tracing::debug!("ignoring message from non-whitelisted user {uid}");
            return;
        }
        let cid = msg.channel_id.get();
        let is_dm = msg.guild_id.is_none();
        if is_dm {
            // DMs always require explicit opt-in: even an empty
            // channel_ids ("loose mode" for guilds) shouldn't accidentally
            // open a private side-channel into claude.
            if !self.config.allow_dm {
                tracing::debug!("ignoring DM (allow_dm=false) from user {uid}");
                return;
            }
        } else if !self.config.channel_ids.is_empty() && !self.config.channel_ids.contains(&cid) {
            tracing::debug!("ignoring message in non-whitelisted channel {cid}");
            return;
        }

        let text = msg.content.trim().to_string();

        // Commands: ignore attachments, dispatch by verb.
        if let Some(rest) = text.strip_prefix('!') {
            self.handle_command(&ctx, &msg, rest).await;
            return;
        }

        // Plain message + (optional) attachments.
        if text.is_empty() && msg.attachments.is_empty() {
            return;
        }

        let session = self
            .state
            .resolve_or_bind(cid, &self.config.default_session)
            .await;

        let processed = attachments::process(&text, &msg.attachments, msg.id.get()).await;
        if !processed.skipped.is_empty() {
            warn!("skipped attachments: {:?}", processed.skipped);
            let note = format!(
                "⚠️ skipped attachment(s): {} (unsupported type or download failed)",
                processed.skipped.join(", ")
            );
            let _ = msg.reply(&ctx.http, note).await;
        }
        if processed.prompt.trim().is_empty() {
            // All attachments skipped and no text — nothing meaningful to forward.
            return;
        }
        let prompt = processed.prompt;

        // Post placeholder REPLY first so we have a message_id to edit
        // when the assistant_message event arrives. Done before
        // send_input so the user sees acknowledgment even if the broker
        // takes a while to settle.
        let placeholder = match msg.reply(&ctx.http, "💭 working…").await {
            Ok(m) => m,
            Err(e) => {
                warn!("post placeholder: {e}");
                return;
            }
        };

        info!(
            "forwarding to session={} from user={} channel={} chars={} attachments={}",
            session,
            uid,
            cid,
            prompt.chars().count(),
            msg.attachments.len(),
        );

        match self.broker.send_input(&session, &prompt).await {
            Ok(_) => {
                let pending = PendingReply {
                    channel_id: placeholder.channel_id.get(),
                    message_id: placeholder.id.get(),
                    deadline: Instant::now() + PENDING_TTL,
                };
                self.state.push_pending(&session, pending).await;
            }
            Err(e) => {
                warn!("forward to {session}: {e:#}");
                let body = format!("❌ forward to `{session}`: {e}");
                let _ = placeholder
                    .channel_id
                    .edit_message(&ctx.http, placeholder.id, EditMessage::new().content(body))
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
            "cwd" => self.cmd_cwd(ctx, msg).await,
            "logs" | "tail" => self.cmd_logs(ctx, msg, arg).await,
            "new" => self.cmd_new(ctx, msg, arg).await,
            "persist" => self.cmd_persist(ctx, msg, arg).await,
            "kill" => self.cmd_kill(ctx, msg, arg).await,
            "interrupt" => self.cmd_interrupt(ctx, msg).await,
            "restart" => self.cmd_restart(ctx, msg).await,
            "hibernate" => self.cmd_hibernate(ctx, msg).await,
            "help" => {
                let body = "**agentmux discord commands**\n\
                            `!ls` — list sessions; ▶ marks the one bound to **this** channel\n\
                            `!attach <name>` — bind THIS channel to a session\n\
                            `!status` — show this channel's binding\n\
                            `!cwd` — show the working directory of this channel's session\n\
                            `!logs [n]` — last n lines of this channel's session output (default 30, max 100)\n\
                            `!new [name] [-cwd path] [-ephemeral|-persist]` — create a session and bind this channel to it\n\
                            `!persist on|off` — toggle whether THIS channel's session survives broker restart\n\
                            `!kill <name>` — destroy a session (force; channels lose binding)\n\
                            `!interrupt` — Ctrl+C this channel's session\n\
                            `!restart` — restart claude in this channel's session\n\
                            `!hibernate` — put this channel's session to sleep\n\
                            `!help` — this message\n\
                            (any other text + attachments are forwarded to this channel's session)";
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
        let cid = msg.channel_id.get();
        let bound = self
            .state
            .resolve_or_bind(cid, &self.config.default_session)
            .await;
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
        out.push_str("```\n▶ = bound to this channel");

        // Show other channels' bindings so the user knows what's wired
        // up across the server. Cheap (HashMap of channel ids) and
        // useful when a session is bound elsewhere than where you're
        // looking.
        let snap = self.state.bindings_snapshot().await;
        let other: Vec<_> = snap
            .into_iter()
            .filter(|(c, _)| *c != cid)
            .collect();
        if !other.is_empty() {
            out.push_str("\nother bindings:");
            for (c, s) in other {
                out.push_str(&format!("\n  <#{c}> → `{s}`"));
            }
        }
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
        self.state
            .bind(msg.channel_id.get(), name.to_string())
            .await;
        let _ = msg
            .reply(
                &ctx.http,
                format!("✅ this channel is now bound to session `{name}`"),
            )
            .await;
    }

    async fn cmd_status(&self, ctx: &Context, msg: &Message) {
        let bound = self
            .state
            .resolve_or_bind(msg.channel_id.get(), &self.config.default_session)
            .await;
        let _ = msg
            .reply(&ctx.http, format!("this channel is bound to session `{bound}`"))
            .await;
    }

    async fn cmd_cwd(&self, ctx: &Context, msg: &Message) {
        let bound = self
            .state
            .resolve_or_bind(msg.channel_id.get(), &self.config.default_session)
            .await;
        match self.broker.list_sessions().await {
            Ok(list) => match list.iter().find(|s| s.name == bound) {
                Some(s) => {
                    let _ = msg
                        .reply(&ctx.http, format!("`{}` cwd: `{}`", s.name, s.cwd))
                        .await;
                }
                None => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!("❌ session `{bound}` not found (no longer alive?)"),
                        )
                        .await;
                }
            },
            Err(e) => {
                let _ = msg.reply(&ctx.http, format!("❌ broker: {e}")).await;
            }
        }
    }

    /// Render the bound session's recent terminal output. Useful for
    /// "is claude alive / what's currently on screen" remote diagnosis.
    /// The PTY ringbuffer is full of TUI redraw chrome; we strip ANSI,
    /// take the last N non-blank lines, and ship as a plain code block.
    async fn cmd_logs(&self, ctx: &Context, msg: &Message, arg: &str) {
        const DEFAULT_LINES: usize = 30;
        const MAX_LINES: usize = 100;
        let n = arg
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_LINES)
            .min(MAX_LINES)
            .max(1);

        let bound = self
            .state
            .resolve_or_bind(msg.channel_id.get(), &self.config.default_session)
            .await;
        let bytes = match self.broker.get_ring(&bound).await {
            Ok(b) => b,
            Err(e) => {
                let _ = msg
                    .reply(&ctx.http, format!("❌ fetch ring `{bound}`: {e}"))
                    .await;
                return;
            }
        };
        let stripped = ansi::strip(&bytes);
        let mut tail = ansi::last_lines(&stripped, n);
        if tail.is_empty() {
            tail.push_str("(buffer is empty)");
        }
        // Discord caps a single message at 2000 chars including the
        // code-fence wrapper; trim from the front (keep most recent)
        // until we fit. max_message_chars in config is the body
        // budget — leave headroom for the ```\n…\n``` decoration.
        let budget = self.config.max_message_chars.saturating_sub(16);
        if tail.chars().count() > budget {
            let skip = tail.chars().count() - budget;
            tail = tail.chars().skip(skip).collect();
            // Don't start mid-line — drop until next newline.
            if let Some(idx) = tail.find('\n') {
                tail = tail[idx + 1..].to_string();
            }
        }
        let body = format!("**[{}] last {n} lines**\n```\n{tail}\n```", bound);
        let _ = msg.reply(&ctx.http, body).await;
    }

    async fn cmd_new(&self, ctx: &Context, msg: &Message, arg: &str) {
        // Accepted shapes:
        //   !new                            — auto-name, default cwd, default persist policy
        //   !new myname                     — given name
        //   !new myname C:\path             — given cwd (positional, after name)
        //   !new myname -cwd C:\path        — given cwd (explicit flag, anywhere)
        //   !new -cwd C:\path               — auto-name + cwd
        //   !new myname -ephemeral          — auto_resume=false (forgotten on broker restart)
        //   !new myname -persist            — auto_resume=true (always restored)
        //   !new -ephemeral -cwd C:\path    — flags compose freely
        // Names with whitespace are not supported.
        let (name_part, cwd, auto_resume) = parse_new_args(arg);

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

        match self
            .broker
            .create_session(&name, cwd.as_deref(), auto_resume)
            .await
        {
            Ok(_) => {
                self.state
                    .bind(msg.channel_id.get(), name.clone())
                    .await;
                let cwd_extra = match &cwd {
                    Some(c) => format!(" (cwd: `{c}`)"),
                    None => String::new(),
                };
                let persist_extra = match auto_resume {
                    Some(true) => " [persist=on]",
                    Some(false) => " [ephemeral]",
                    None => "",
                };
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("✅ created `{name}` and bound this channel to it{cwd_extra}{persist_extra}"),
                    )
                    .await;
            }
            Err(e) => {
                let _ = msg
                    .reply(&ctx.http, format!("❌ create `{name}`: {e}"))
                    .await;
            }
        }
    }

    async fn cmd_persist(&self, ctx: &Context, msg: &Message, arg: &str) {
        let on = match arg.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "y" | "1" => true,
            "off" | "false" | "no" | "n" | "0" => false,
            "" => {
                let _ = msg
                    .reply(&ctx.http, "usage: `!persist on|off`")
                    .await;
                return;
            }
            other => {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("❌ `{other}` — expected `on` or `off`"),
                    )
                    .await;
                return;
            }
        };
        let bound = self
            .state
            .resolve_or_bind(msg.channel_id.get(), &self.config.default_session)
            .await;
        match self.broker.set_persist(&bound, on).await {
            Ok(_) => {
                let label = if on { "persist=on (restored on broker restart)" } else { "ephemeral (forgotten on broker restart)" };
                let _ = msg
                    .reply(&ctx.http, format!("✅ `{bound}` → {label}"))
                    .await;
            }
            Err(e) => {
                let _ = msg
                    .reply(&ctx.http, format!("❌ persist `{bound}`: {e}"))
                    .await;
            }
        }
    }

    async fn cmd_kill(&self, ctx: &Context, msg: &Message, name: &str) {
        if name.is_empty() {
            let _ = msg.reply(&ctx.http, "usage: `!kill <name>`").await;
            return;
        }

        match self.broker.delete_session(name).await {
            Ok(_) => {
                // Wipe every channel that was bound to the killed session.
                self.state.unbind_all(name).await;
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!(
                            "✅ killed `{name}` (channels previously bound to it now fall back to `{}`)",
                            self.config.default_session
                        ),
                    )
                    .await;
            }
            Err(e) => {
                let _ = msg
                    .reply(&ctx.http, format!("❌ kill `{name}`: {e}"))
                    .await;
            }
        }
    }

    async fn cmd_interrupt(&self, ctx: &Context, msg: &Message) {
        let bound = self
            .state
            .resolve_or_bind(msg.channel_id.get(), &self.config.default_session)
            .await;
        match self.broker.interrupt_session(&bound).await {
            Ok(_) => {
                let _ = msg
                    .react(&ctx.http, ReactionType::Unicode("🛑".into()))
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
        let bound = self
            .state
            .resolve_or_bind(msg.channel_id.get(), &self.config.default_session)
            .await;
        match self.broker.restart_session(&bound).await {
            Ok(_) => {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("🔄 restarted claude in `{bound}` (history preserved)"),
                    )
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
        let bound = self
            .state
            .resolve_or_bind(msg.channel_id.get(), &self.config.default_session)
            .await;
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

/// Token-based parser for `!new` arguments. Pulls boolean flags
/// (`-ephemeral`, `-persist`) and the keyed `-cwd <path>` flag out of
/// the token stream, leaving a residue of positional words. The first
/// residue word is treated as the session name; any remaining words
/// joined back together act as a positional cwd (so users can type
/// `!new myname C:\some path` without quoting).
///
/// Returns `(name, cwd, auto_resume)` where `auto_resume`:
///   * `Some(false)` if `-ephemeral` was seen
///   * `Some(true)`  if `-persist`   was seen
///   * `None`        if neither (broker falls through to its default)
///
/// `-ephemeral` and `-persist` together is a user error; the LAST one
/// wins (consistent with shell convention).
fn parse_new_args(arg: &str) -> (String, Option<String>, Option<bool>) {
    let mut tokens = arg.split_whitespace().peekable();
    let mut positionals: Vec<String> = Vec::new();
    let mut cwd: Option<String> = None;
    let mut auto_resume: Option<bool> = None;

    while let Some(tok) = tokens.next() {
        match tok {
            "-cwd" => {
                if let Some(rest) = tokens.next() {
                    // Greedy: consume remaining tokens into cwd so paths
                    // with spaces work (the caller's regex doesn't see
                    // the original quoting). Stop only if we hit another
                    // recognised flag.
                    let mut collected = vec![rest.to_string()];
                    while let Some(peek) = tokens.peek() {
                        if matches!(*peek, "-ephemeral" | "-persist" | "-cwd") {
                            break;
                        }
                        collected.push(tokens.next().unwrap().to_string());
                    }
                    cwd = Some(collected.join(" "));
                }
            }
            "-ephemeral" => auto_resume = Some(false),
            "-persist" => auto_resume = Some(true),
            _ => positionals.push(tok.to_string()),
        }
    }

    let (name, fallback_cwd) = match positionals.len() {
        0 => (String::new(), None),
        1 => (positionals.remove(0), None),
        _ => {
            let n = positionals.remove(0);
            (n, Some(positionals.join(" ")))
        }
    };
    let cwd = cwd.or(fallback_cwd);
    (name, cwd, auto_resume)
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

#[cfg(test)]
mod tests {
    use super::parse_new_args;

    #[test]
    fn empty_arg() {
        let (n, c, a) = parse_new_args("");
        assert!(n.is_empty());
        assert_eq!(c, None);
        assert_eq!(a, None);
    }

    #[test]
    fn name_only() {
        let (n, c, a) = parse_new_args("myname");
        assert_eq!(n, "myname");
        assert_eq!(c, None);
        assert_eq!(a, None);
    }

    #[test]
    fn name_and_positional_cwd() {
        let (n, c, a) = parse_new_args("myname C:\\path");
        assert_eq!(n, "myname");
        assert_eq!(c.as_deref(), Some("C:\\path"));
        assert_eq!(a, None);
    }

    #[test]
    fn name_and_explicit_cwd_flag() {
        let (n, c, a) = parse_new_args("myname -cwd C:\\some path");
        assert_eq!(n, "myname");
        assert_eq!(c.as_deref(), Some("C:\\some path"));
        assert_eq!(a, None);
    }

    #[test]
    fn ephemeral_only() {
        let (n, c, a) = parse_new_args("-ephemeral");
        assert!(n.is_empty());
        assert_eq!(c, None);
        assert_eq!(a, Some(false));
    }

    #[test]
    fn persist_after_name_and_cwd() {
        let (n, c, a) = parse_new_args("myname -cwd C:\\x -persist");
        assert_eq!(n, "myname");
        assert_eq!(c.as_deref(), Some("C:\\x"));
        assert_eq!(a, Some(true));
    }

    #[test]
    fn flags_compose_freely() {
        let (n, c, a) = parse_new_args("-ephemeral myname -cwd D:\\y");
        assert_eq!(n, "myname");
        assert_eq!(c.as_deref(), Some("D:\\y"));
        assert_eq!(a, Some(false));
    }

    #[test]
    fn last_persist_flag_wins() {
        let (_, _, a) = parse_new_args("foo -ephemeral -persist");
        assert_eq!(a, Some(true));
        let (_, _, a) = parse_new_args("foo -persist -ephemeral");
        assert_eq!(a, Some(false));
    }
}
