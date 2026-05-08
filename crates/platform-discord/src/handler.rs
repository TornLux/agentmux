//! Discord-side event handler. Implements `serenity::EventHandler`,
//! forwarding allowed-user messages to the channel-bound session via the
//! broker HTTP API and handling `!`-prefix meta-commands as well as
//! slash-command interactions.
//!
//! Non-trivial behaviours implemented here:
//!
//!  * **Per-channel binding** — each Discord channel maps to one session.
//!    First-time access in a channel binds it to `default_session`;
//!    `!attach <name>` (or `/attach`) rebinds the *current* channel.
//!    Outbound routing in `main.rs` uses the same map so an
//!    `assistant_message` event for session `s` lands in whichever
//!    channel(s) bind it.
//!  * **Mention wake** — when `respond_to_mentions = true`, an inbound
//!    message in a non-whitelisted server channel is accepted iff the
//!    bot is `@`-mentioned. The mention is stripped from the prompt
//!    before forwarding.
//!  * **Reply-thread routing** — when the user posts a Discord
//!    message_reference (Reply UI) targeting a previously-relayed
//!    assistant message, this turn is forwarded to *that* session,
//!    overriding the channel binding for this one message.
//!  * **Placeholder + edit-in-place** — every forwarded user message
//!    posts a `💭 working…` reply immediately, registers the message
//!    id in `pending_replies[session]`, and the WS relay edits that
//!    message in place when the matching `assistant_message` arrives.
//!  * **Typing indicator** — while a placeholder is unresolved, a
//!    background task pings Discord's "user is typing" every ~7s so
//!    long claude turns feel responsive in the client.
//!  * **Error reaction** — when a forward fails (broker unreachable,
//!    session 404, etc.), the placeholder is deleted and the user's
//!    original message gets a ❌ reaction, plus a short error reply.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serenity::all::{
    ChannelId, Context, EventHandler, GuildId, Interaction, Message, Reaction, ReactionType,
    Ready,
};
use serenity::async_trait;
use tracing::{info, warn};

use crate::ansi;
use crate::attachments;
use crate::broker::{BrokerClient, SendInputError, SessionLite};
use crate::config::DiscordConfig;
use crate::slash;
use crate::state::{now_unix_ms, BotState, PendingReply};

/// How long a placeholder waits for `assistant_message` before being
/// considered abandoned (and therefore freely overwritable by the next
/// turn). Real claude turns top out around a couple of minutes; 10
/// minutes is "user has clearly given up" territory.
const PENDING_TTL: Duration = Duration::from_secs(600);

/// Actions reachable via reactions on bot-posted assistant messages.
#[derive(Copy, Clone, Debug)]
enum ReactAction {
    Interrupt,
    Hibernate,
    Restart,
}

impl std::fmt::Display for ReactAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ReactAction::Interrupt => "interrupt",
            ReactAction::Hibernate => "hibernate",
            ReactAction::Restart => "restart",
        })
    }
}

pub struct Handler {
    pub config: Arc<DiscordConfig>,
    pub broker: Arc<BrokerClient>,
    pub state: Arc<BotState>,
    /// Bot's own user id, captured on `ready` so the message handler
    /// can detect `@`-mentions of itself without polling the cache.
    /// `OnceLock` because we only ever set it once and reads should
    /// be lock-free.
    bot_user_id: OnceLock<u64>,
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
            bot_user_id: OnceLock::new(),
        }
    }

    /// Best-effort getter — returns `0` before `ready` fires, which
    /// effectively disables mention-wake until the bot is online (a
    /// non-issue since we wouldn't be receiving messages yet).
    fn bot_user_id(&self) -> u64 {
        self.bot_user_id.get().copied().unwrap_or(0)
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        let _ = self.bot_user_id.set(ready.user.id.get());
        info!(
            "discord ready as {} (id={}); default_session={} channels={} respond_to_mentions={} allow_dm={}",
            ready.user.name,
            ready.user.id.get(),
            self.config.default_session,
            self.config.channel_ids.len(),
            self.config.respond_to_mentions,
            self.config.allow_dm,
        );

        // Register slash commands. Guild registration is instant;
        // global registration takes up to an hour to propagate but
        // requires no extra config. Errors are logged and non-fatal —
        // the bot still works via `!`-prefix commands.
        let cmds = slash::definitions();
        let result = if self.config.slash_command_guild_id != 0 {
            let gid = GuildId::new(self.config.slash_command_guild_id);
            gid.set_commands(&ctx.http, cmds).await.map(|v| v.len())
        } else {
            serenity::all::Command::set_global_commands(&ctx.http, cmds)
                .await
                .map(|v| v.len())
        };
        match result {
            Ok(n) => info!(
                "registered {n} slash command(s) ({})",
                if self.config.slash_command_guild_id != 0 {
                    "guild-scoped, instant"
                } else {
                    "global, may take up to 1h to appear"
                }
            ),
            Err(e) => warn!("slash command registration failed: {e}"),
        }
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
        let bot_id = self.bot_user_id();
        let mentioned = bot_id != 0 && msg.mentions.iter().any(|u| u.id.get() == bot_id);

        // Whitelist gate.
        // - DM:                   require allow_dm.
        // - Server, in list:      always accept.
        // - Server, not in list:  accept only if respond_to_mentions
        //                         AND bot is mentioned. Else drop.
        if is_dm {
            if !self.config.allow_dm {
                tracing::debug!("ignoring DM (allow_dm=false) from user {uid}");
                return;
            }
        } else if !self.config.channel_ids.is_empty() && !self.config.channel_ids.contains(&cid) {
            if !(self.config.respond_to_mentions && mentioned) {
                tracing::debug!("ignoring message in non-whitelisted channel {cid}");
                return;
            }
        }

        // Strip our own mention markers from the prompt — we don't want
        // claude to see literal "<@1234567890>" garbage when it was
        // really just the user paging the bot.
        let text = if bot_id != 0 {
            strip_bot_mentions(msg.content.trim(), bot_id)
        } else {
            msg.content.trim().to_string()
        };

        // Commands: ignore attachments, dispatch by verb.
        if let Some(rest) = text.strip_prefix('!') {
            self.handle_command(&ctx, &msg, rest).await;
            return;
        }

        // Plain message + (optional) attachments.
        if text.is_empty() && msg.attachments.is_empty() {
            return;
        }

        // Reply-thread override. If the user replied (Discord UI)
        // to a previously-relayed assistant message, forward this
        // turn to that session — does NOT change channel binding.
        let reply_target_id = msg.message_reference.as_ref().and_then(|r| r.message_id);
        let replied_session = match reply_target_id {
            Some(mid) => self.state.lookup_replied_session(mid.get()).await,
            None => None,
        };
        let session = match &replied_session {
            Some(s) => {
                tracing::debug!("reply-thread route: msg {} -> session {s}", msg.id.get());
                s.clone()
            }
            None => {
                self.state
                    .resolve_or_bind(cid, &self.config.default_session)
                    .await
            }
        };

        // If this is a reply, optionally fetch the quoted text and
        // prepend it to the prompt as a context header so claude
        // sees what the user is responding to. Cheap when serenity
        // populated `referenced_message` inline; falls back to a
        // single API call.
        let quote = if reply_target_id.is_some() && self.config.reply_quote_in_prompt {
            fetch_reply_quote(&ctx, &msg).await
        } else {
            None
        };

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
            return;
        }
        let prompt = match quote {
            Some(q) => format!("{}\n{}", format_reply_quote(&q), processed.prompt),
            None => processed.prompt,
        };

        // Post placeholder REPLY first so we have a message_id to edit
        // when the assistant_message event arrives. Done before
        // send_input so the user sees acknowledgment even if the
        // broker takes a while to settle.
        let placeholder = match msg.reply(&ctx.http, "💭 working…").await {
            Ok(m) => m,
            Err(e) => {
                warn!("post placeholder: {e}");
                return;
            }
        };

        // Spawn typing immediately — *before* send_input — so the
        // "agentmux is typing…" indicator covers the full broker-await
        // window. For hibernated sessions, send_input blocks 5–10 s
        // waiting for `claude --resume` to settle; without this early
        // spawn the user sees the placeholder but no typing presence
        // during that gap. Cancel flag is flipped if forwarding errors
        // out, so a failed forward doesn't leave a ghost indicator.
        let typing_cancel = Arc::new(AtomicBool::new(false));
        spawn_typing_task(
            ctx.http.clone(),
            placeholder.channel_id,
            typing_cancel.clone(),
        );

        info!(
            "forwarding to session={} from user={} channel={} chars={} attachments={} reply_route={}",
            session,
            uid,
            cid,
            prompt.chars().count(),
            msg.attachments.len(),
            msg.message_reference.is_some(),
        );

        match self.broker.send_input(&session, &prompt).await {
            Ok(_) => {
                let pending = PendingReply::new(
                    placeholder.channel_id.get(),
                    placeholder.id.get(),
                    now_unix_ms().saturating_add(PENDING_TTL.as_millis() as u64),
                    typing_cancel,
                );
                self.state.push_pending(&session, pending).await;
            }
            Err(SendInputError::LocallyOwned { session, message }) => {
                // The session was demoted: broker has no claude to
                // write into, and auto-resuming would race the user's
                // local `claude --resume` and corrupt the transcript.
                // UX: drop the placeholder (no work to track), react
                // 💤 on the user's message, and post an explanation
                // ONLY on the first rejection in a 5-min window per
                // channel — repeated attempts get just the reaction
                // so the channel doesn't fill with the same notice.
                info!("forward to {session}: locally-owned, refused");
                typing_cancel.store(true, Ordering::Release);
                let _ = placeholder.delete(&ctx.http).await;
                let _ = msg
                    .react(&ctx.http, ReactionType::Unicode("💤".into()))
                    .await;
                if self
                    .state
                    .should_post_full_locally_owned_notice(cid)
                    .await
                {
                    let body = format!(
                        "💤 {}\n\nRun `\\agentmux adopt {}` on the broker host to bring it back.",
                        message, session,
                    );
                    let _ = msg.reply(&ctx.http, body).await;
                }
            }
            Err(SendInputError::Other(e)) => {
                warn!("forward to {session}: {e:#}");
                // Switch to react-on-original-message UX: delete our
                // placeholder, react ❌ on user's msg, post a brief
                // error reply with the diagnostic so the user can
                // see why it failed.
                typing_cancel.store(true, Ordering::Release);
                let _ = placeholder.delete(&ctx.http).await;
                let _ = msg
                    .react(&ctx.http, ReactionType::Unicode("❌".into()))
                    .await;
                let _ = msg
                    .reply(&ctx.http, format!("forward to `{session}`: {e}"))
                    .await;
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(cmd) => {
                slash::handle_command(self, &ctx, &cmd).await;
            }
            Interaction::Autocomplete(ac) => {
                slash::handle_autocomplete(self, &ctx, &ac).await;
            }
            Interaction::Component(comp) => {
                handle_component(self, &ctx, &comp).await;
            }
            _ => {}
        }
    }

    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        if !self.config.react_with_actions {
            return;
        }
        // Reactor must be whitelisted.
        let user_id = match reaction.user_id {
            Some(u) => u.get(),
            None => return,
        };
        if !self.config.allowed_user_ids.contains(&user_id) {
            return;
        }
        // Don't react to our own ack reactions (would loop).
        if user_id == self.bot_user_id() {
            return;
        }
        // Map emoji → broker action.
        let emoji = match &reaction.emoji {
            ReactionType::Unicode(s) => s.clone(),
            _ => return,
        };
        let action = match emoji.as_str() {
            "🛑" | "❌" => ReactAction::Interrupt,
            "💤" => ReactAction::Hibernate,
            "🔄" | "🔁" => ReactAction::Restart,
            _ => return,
        };
        // Target message must be one we recorded as a session reply
        // (we don't dispatch on arbitrary user messages or third-party
        // bot messages).
        let session = match self
            .state
            .lookup_replied_session(reaction.message_id.get())
            .await
        {
            Some(s) => s,
            None => return,
        };

        let result = match action {
            ReactAction::Interrupt => self.broker.interrupt_session(&session).await,
            ReactAction::Hibernate => self.broker.hibernate_session(&session).await,
            ReactAction::Restart => self.broker.restart_session(&session).await,
        };

        let ack = match (action, &result) {
            (_, Err(e)) => format!("❌ react `{action}` on `{session}`: {e}"),
            (ReactAction::Interrupt, Ok(_)) => format!("🛑 interrupted `{session}` (via react)"),
            (ReactAction::Hibernate, Ok(_)) => {
                format!("💤 hibernated `{session}` (via react)")
            }
            (ReactAction::Restart, Ok(_)) => {
                format!("🔄 restarted `{session}` (via react, history preserved)")
            }
        };
        // Short ack as a fresh post in the same channel — better
        // signal than another reaction (no echo loop, visible in
        // mobile notifications). Falls back silently on failure
        // since the action itself already happened.
        if let Err(e) = reaction
            .channel_id
            .say(&ctx.http, ack)
            .await
        {
            tracing::debug!("react ack send: {e}");
        }
    }
}

/// Dispatch a Discord component interaction (button click). Today
/// only the PreToolUse approval buttons are wired here — their
/// `custom_id` is shaped `tool:allow:<request_id>` or
/// `tool:deny:<request_id>` so we extract the verb and id, route the
/// decision to the broker via `BrokerClient::tool_decision`, then
/// edit the original message to a settled "approved by X" / "denied
/// by X" line so it's clear from the channel history that the
/// request is no longer pending.
async fn handle_component(
    handler: &Handler,
    ctx: &Context,
    comp: &serenity::all::ComponentInteraction,
) {
    let user_id = comp.user.id.get();
    if !handler.config.allowed_user_ids.contains(&user_id) {
        // Acknowledge silently so Discord doesn't show "this
        // interaction failed" but don't act on it.
        let _ = comp
            .create_response(
                &ctx.http,
                serenity::all::CreateInteractionResponse::Acknowledge,
            )
            .await;
        return;
    }

    let parts: Vec<&str> = comp.data.custom_id.splitn(3, ':').collect();
    if parts.len() != 3 || parts[0] != "tool" {
        return;
    }
    let allow = match parts[1] {
        "allow" => true,
        "deny" => false,
        _ => return,
    };
    let request_id = parts[2].to_string();

    // Note who decided in the reason so claude can attribute
    // server-side if needed (and so the channel history shows it).
    let actor = comp.user.name.clone();
    let reason = if allow {
        format!("approved by {actor} via Discord")
    } else {
        format!("denied by {actor} via Discord")
    };

    let result = handler
        .broker
        .tool_decision(&request_id, allow, &reason)
        .await;

    // Update the prompt message in place so it doesn't keep the
    // buttons live for additional clicks.
    let label = if allow {
        format!("✅ Approved by {actor}")
    } else {
        format!("❌ Denied by {actor}")
    };
    let mut new_content = comp.message.content.clone();
    new_content.push_str(&format!("\n\n— {label}"));
    let edit = serenity::all::EditMessage::new()
        .content(new_content)
        .components(vec![]);
    let _ = comp
        .channel_id
        .edit_message(&ctx.http, comp.message.id, edit)
        .await;

    // Ack the interaction. If the broker call failed we still ack
    // (otherwise Discord shows a generic error) but follow up with
    // a visible reply so the user sees the failure.
    let _ = comp
        .create_response(
            &ctx.http,
            serenity::all::CreateInteractionResponse::Acknowledge,
        )
        .await;
    if let Err(e) = result {
        warn!("tool_decision id={request_id}: {e:#}");
        let _ = comp
            .channel_id
            .say(
                &ctx.http,
                format!("⚠️ couldn't deliver decision to broker: {e}"),
            )
            .await;
    }
}

/// Spawn a background task that pokes Discord's "user is typing"
/// indicator every ~7 s on `channel`. Exits when the cancel flag is
/// set (the placeholder got edited / deleted) or after ~10 minutes
/// (matches PENDING_TTL — guard against an orphaned task hanging
/// around forever if the cancel flag never fires).
fn spawn_typing_task(
    http: Arc<serenity::http::Http>,
    channel: ChannelId,
    cancel: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        const TICK: Duration = Duration::from_secs(7);
        let deadline = Instant::now() + PENDING_TTL;
        loop {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            // Discord's typing indicator visibly lasts ~10 s in the
            // client — repinging at 7 s keeps it on continuously.
            let _ = channel.broadcast_typing(&http).await;
            // Sleep in small increments so cancel propagates fast
            // (no 7 s residual ghost typing after edit).
            for _ in 0..14 {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                tokio::time::sleep(TICK / 14).await;
            }
        }
    });
}

/// Reply-quote header length cap (chars). Long enough for a useful
/// excerpt; short enough that the bulk of the prompt is still the
/// user's actual question.
const REPLY_QUOTE_MAX_CHARS: usize = 300;

/// Best-effort fetch of the text body of the message a Discord reply
/// targets. Tries serenity's inline `referenced_message` first (no
/// API call) and falls back to `channel.message(http, mid)` only if
/// that's missing. Returns None on missing reference or API failure.
async fn fetch_reply_quote(ctx: &Context, msg: &Message) -> Option<String> {
    if let Some(referenced) = msg.referenced_message.as_ref() {
        let s = referenced.content.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    let mid = msg.message_reference.as_ref()?.message_id?;
    match msg.channel_id.message(&ctx.http, mid).await {
        Ok(m) => {
            let s = m.content.trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        Err(e) => {
            tracing::debug!("fetch reply target {mid}: {e}");
            None
        }
    }
}

/// Render the quoted body as a single-line `[replying to: "..."]`
/// header. Newlines collapsed to spaces (avoids the broker having to
/// split-write a multi-line header on top of the user's payload),
/// then capped at `REPLY_QUOTE_MAX_CHARS` with an ellipsis.
fn format_reply_quote(quote: &str) -> String {
    let single_line: String = quote
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let excerpt: String = if single_line.chars().count() > REPLY_QUOTE_MAX_CHARS {
        let truncated: String = single_line.chars().take(REPLY_QUOTE_MAX_CHARS).collect();
        format!("{truncated}…")
    } else {
        single_line
    };
    format!("[replying to: \"{excerpt}\"]")
}

/// Strip `<@id>` and `<@!id>` mention markers (where id matches our
/// bot) and the trailing whitespace they leave behind. Returns a
/// trimmed prompt suitable for forwarding.
fn strip_bot_mentions(text: &str, bot_id: u64) -> String {
    let m1 = format!("<@{bot_id}>");
    let m2 = format!("<@!{bot_id}>");
    let mut out = text.to_string();
    while let Some(idx) = out.find(&m1) {
        out.replace_range(idx..idx + m1.len(), "");
    }
    while let Some(idx) = out.find(&m2) {
        out.replace_range(idx..idx + m2.len(), "");
    }
    // Collapse the run of whitespace the strip likely left.
    let collapsed: String = out
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed
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
                    "{marker}{:<18}  {:<14}  viewers={}  cwd={}\n",
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
    use super::{parse_new_args, strip_bot_mentions};

    #[test]
    fn strip_bot_mentions_basic() {
        assert_eq!(strip_bot_mentions("<@123> hello", 123), "hello");
        assert_eq!(strip_bot_mentions("<@!123> hello", 123), "hello");
        assert_eq!(strip_bot_mentions("hi <@123> there", 123), "hi there");
        assert_eq!(strip_bot_mentions("plain text", 123), "plain text");
        // Mentions for OTHER ids are left alone.
        assert_eq!(strip_bot_mentions("<@999> ignore me", 123), "<@999> ignore me");
    }

    #[test]
    fn reply_quote_short() {
        let q = super::format_reply_quote("hello world");
        assert_eq!(q, "[replying to: \"hello world\"]");
    }

    #[test]
    fn reply_quote_collapses_newlines() {
        let q = super::format_reply_quote("line one\nline two\n  line three");
        assert_eq!(q, "[replying to: \"line one line two line three\"]");
    }

    #[test]
    fn reply_quote_truncates_long() {
        let long: String = "x".repeat(500);
        let q = super::format_reply_quote(&long);
        let inside = q.trim_start_matches("[replying to: \"").trim_end_matches("\"]");
        // Should be 300 chars + the ellipsis.
        assert_eq!(inside.chars().count(), super::REPLY_QUOTE_MAX_CHARS + 1);
        assert!(inside.ends_with('…'));
    }

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
