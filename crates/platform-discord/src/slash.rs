//! Slash-command surface for the Discord bot — same verbs as the
//! `!`-prefix text commands in `handler.rs`, but invoked via Discord
//! application commands so users get autocomplete and typed options.
//!
//! Layout:
//!  * [`definitions`] returns the full Vec<CreateCommand> registered
//!    on `ready`. Edit here when adding/removing slash verbs.
//!  * [`handle_command`] dispatches a chat-input invocation to the
//!    matching `slash_*` function; the returned String is sent back
//!    as the interaction response.
//!  * [`handle_autocomplete`] serves session-name suggestions for
//!    `/attach` and `/kill`.
//!
//! Slash and `!` commands deliberately share the underlying broker /
//! state calls so behaviour is identical regardless of invocation
//! style. Response *format* differs slightly because Discord's
//! interaction reply UX is itself a "reply" to the command (no need
//! to duplicate the user's request), so headers / context lines that
//! make sense in `!`-mode (e.g. listing other channel bindings) are
//! omitted in slash responses where it would be redundant.

use serenity::all::{
    AutocompleteChoice, CommandDataOptionValue, CommandInteraction, CommandOptionType,
    Context, CreateAutocompleteResponse, CreateCommand, CreateCommandOption, CreateEmbed,
    CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage,
};

use crate::ansi;
use crate::handler::Handler;

// Discord-style color palette so embed side bars are visually consistent
// with the platform itself. Picked from Discord's brand guide.
const COLOR_OK: u32 = 0x57F287; // green
const COLOR_ERR: u32 = 0xED4245; // red
const COLOR_INFO: u32 = 0x5865F2; // blurple

fn ok_embed(title: impl Into<String>, body: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new()
        .title(title)
        .description(body)
        .color(COLOR_OK)
}

fn err_embed(title: impl Into<String>, body: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new()
        .title(title)
        .description(body)
        .color(COLOR_ERR)
}

fn info_embed(title: impl Into<String>, body: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new()
        .title(title)
        .description(body)
        .color(COLOR_INFO)
}

pub fn definitions() -> Vec<CreateCommand> {
    vec![
        CreateCommand::new("ls").description("List all sessions and per-channel bindings"),
        CreateCommand::new("status").description("Show this channel's session binding"),
        CreateCommand::new("cwd").description("Show the working directory of this channel's session"),
        CreateCommand::new("attach")
            .description("Bind THIS channel to an existing session")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "name",
                    "Existing session to bind",
                )
                .required(true)
                .min_length(1)
                .max_length(32)
                .set_autocomplete(true),
            ),
        CreateCommand::new("logs")
            .description("Show last N lines of this channel's session output")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "n",
                    "Number of lines (1-100, default 30)",
                )
                .min_int_value(1)
                .max_int_value(100),
            ),
        CreateCommand::new("new")
            .description("Create a new session and bind THIS channel to it")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "name",
                    "Session name, no whitespace (auto-generated if omitted)",
                )
                .min_length(1)
                .max_length(32),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "cwd",
                    "Working directory (broker default if omitted)",
                )
                .min_length(1)
                .max_length(260),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::Boolean,
                    "persist",
                    "Survive broker restart? (overrides global default)",
                ),
            ),
        CreateCommand::new("persist")
            .description("Toggle whether this channel's session survives broker restart")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "on",
                    "on = restored on broker restart, off = forgotten",
                )
                .required(true)
                .add_string_choice("on", "on")
                .add_string_choice("off", "off"),
            ),
        CreateCommand::new("kill")
            .description("Destroy a session (channels lose binding)")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "name",
                    "Session to kill",
                )
                .required(true)
                .min_length(1)
                .max_length(32)
                .set_autocomplete(true),
            ),
        CreateCommand::new("interrupt").description("Ctrl+C this channel's session"),
        CreateCommand::new("restart")
            .description("Restart claude in this channel's session (history preserved)"),
        CreateCommand::new("hibernate")
            .description("Hibernate this channel's session (next message wakes it)"),
        CreateCommand::new("reload")
            .description("Restart the whole agentmux stack (reloads config.toml / discord.toml)"),
        CreateCommand::new("help").description("Show command help"),
    ]
}

pub async fn handle_command(handler: &Handler, ctx: &Context, cmd: &CommandInteraction) {
    let cid = cmd.channel_id.get();
    // (embed, ephemeral) — read-only / status-class verbs reply
    // ephemerally so the channel doesn't fill up with one-user diagnostics;
    // mutating verbs stay public so other channel members see what changed.
    let (embed, ephemeral) = match cmd.data.name.as_str() {
        "ls" => (slash_ls(handler, cid).await, true),
        "status" => (slash_status(handler, cid).await, true),
        "cwd" => (slash_cwd(handler, cid).await, true),
        "logs" => {
            let n = int_arg(cmd, "n").unwrap_or(30) as usize;
            (slash_logs(handler, cid, n).await, true)
        }
        "attach" => {
            let name = string_arg(cmd, "name").unwrap_or_default();
            (slash_attach(handler, cid, &name).await, false)
        }
        "new" => (
            slash_new(
                handler,
                cid,
                string_arg(cmd, "name"),
                string_arg(cmd, "cwd"),
                bool_arg(cmd, "persist"),
            )
            .await,
            false,
        ),
        "persist" => {
            // Choices guarantee the value is exactly "on" or "off"; we
            // fall through to off on any unexpected/missing value rather
            // than panic so an out-of-sync client can't crash the bot.
            let on = matches!(string_arg(cmd, "on").as_deref(), Some("on"));
            (slash_persist(handler, cid, on).await, false)
        }
        "kill" => {
            let name = string_arg(cmd, "name").unwrap_or_default();
            (slash_kill(handler, &name).await, false)
        }
        "interrupt" => (slash_interrupt(handler, cid).await, false),
        "restart" => (slash_restart(handler, cid).await, false),
        "hibernate" => (slash_hibernate(handler, cid).await, false),
        "reload" => (slash_reload(handler).await, true),
        "help" => (slash_help(), true),
        other => (
            err_embed("Unknown command", format!("`/{other}` is not a known command")),
            true,
        ),
    };

    let mut resp = CreateInteractionResponseMessage::new().add_embed(embed);
    if ephemeral {
        resp = resp.ephemeral(true);
    }
    if let Err(e) = cmd
        .create_response(&ctx.http, CreateInteractionResponse::Message(resp))
        .await
    {
        tracing::warn!("slash response: {e}");
    }
}

pub async fn handle_autocomplete(
    handler: &Handler,
    ctx: &Context,
    ac: &CommandInteraction,
) {
    let cmd_name = ac.data.name.as_str();
    if !matches!(cmd_name, "attach" | "kill") {
        return;
    }
    let Some(focused) = ac.data.autocomplete() else {
        return;
    };
    let typed = focused.value.to_ascii_lowercase();

    let list = handler.broker.list_sessions().await.unwrap_or_default();
    let choices: Vec<AutocompleteChoice> = list
        .into_iter()
        .filter(|s| {
            typed.is_empty() || s.name.to_ascii_lowercase().contains(&typed)
        })
        .take(25) // Discord caps autocomplete at 25 choices.
        .map(|s| AutocompleteChoice::new(s.name.clone(), s.name))
        .collect();

    let resp = CreateAutocompleteResponse::new().set_choices(choices);
    let _ = ac
        .create_response(&ctx.http, CreateInteractionResponse::Autocomplete(resp))
        .await;
}

// -------- option extractors --------

fn string_arg(cmd: &CommandInteraction, name: &str) -> Option<String> {
    cmd.data.options.iter().find(|o| o.name == name).and_then(|o| {
        if let CommandDataOptionValue::String(s) = &o.value {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        } else {
            None
        }
    })
}

fn int_arg(cmd: &CommandInteraction, name: &str) -> Option<i64> {
    cmd.data.options.iter().find(|o| o.name == name).and_then(|o| {
        if let CommandDataOptionValue::Integer(n) = &o.value {
            Some(*n)
        } else {
            None
        }
    })
}

fn bool_arg(cmd: &CommandInteraction, name: &str) -> Option<bool> {
    cmd.data.options.iter().find(|o| o.name == name).and_then(|o| {
        if let CommandDataOptionValue::Boolean(b) = &o.value {
            Some(*b)
        } else {
            None
        }
    })
}

// -------- per-verb implementations (return embed) --------

async fn slash_ls(handler: &Handler, cid: u64) -> CreateEmbed {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    let list = match handler.broker.list_sessions().await {
        Ok(l) => l,
        Err(e) => return err_embed("Broker error", format!("`{e}`")),
    };
    let table = if list.is_empty() {
        "(no sessions)".to_string()
    } else {
        let mut out = String::new();
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
        out
    };
    let mut embed = info_embed("Sessions", format!("```\n{table}```"))
        .footer(CreateEmbedFooter::new("▶ = bound to this channel"));

    let snap = handler.state.bindings_snapshot().await;
    let other: Vec<_> = snap.into_iter().filter(|(c, _)| *c != cid).collect();
    if !other.is_empty() {
        let other_text = other
            .into_iter()
            .map(|(c, s)| format!("<#{c}> → `{s}`"))
            .collect::<Vec<_>>()
            .join("\n");
        embed = embed.field("Other channel bindings", other_text, false);
    }
    embed
}

async fn slash_status(handler: &Handler, cid: u64) -> CreateEmbed {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    info_embed(
        "Channel binding",
        format!("This channel is bound to session `{bound}`"),
    )
}

async fn slash_cwd(handler: &Handler, cid: u64) -> CreateEmbed {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.list_sessions().await {
        Ok(list) => match list.iter().find(|s| s.name == bound) {
            Some(s) => info_embed("Working directory", format!("`{}`", s.cwd))
                .field("Session", format!("`{}`", s.name), true)
                .field("State", format!("`{}`", s.state), true),
            None => err_embed(
                "Session not found",
                format!("`{bound}` is no longer alive (broker dropped it?)"),
            ),
        },
        Err(e) => err_embed("Broker error", format!("`{e}`")),
    }
}

async fn slash_attach(handler: &Handler, cid: u64, name: &str) -> CreateEmbed {
    if name.is_empty() {
        return err_embed("Missing argument", "Usage: `/attach name:<session>`");
    }
    let exists = match handler.broker.list_sessions().await {
        Ok(l) => l.iter().any(|s| s.name == name),
        Err(e) => return err_embed("Broker error", format!("`{e}`")),
    };
    if !exists {
        return err_embed("No such session", format!("No session named `{name}`"));
    }
    handler.state.bind(cid, name.to_string()).await;
    ok_embed(
        "Channel rebound",
        format!("This channel is now bound to session `{name}`"),
    )
}

async fn slash_logs(handler: &Handler, cid: u64, n: usize) -> CreateEmbed {
    let n = n.clamp(1, 100);
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    let bytes = match handler.broker.get_ring(&bound).await {
        Ok(b) => b,
        Err(e) => return err_embed(format!("logs `{bound}`"), format!("`{e}`")),
    };
    let stripped = ansi::strip(&bytes);
    let mut tail = ansi::last_lines(&stripped, n);
    if tail.is_empty() {
        tail.push_str("(buffer is empty)");
    }
    // Embed description max is 4096; cap below that with headroom for
    // the code-fence wrapper. max_message_chars from config still
    // honoured for users who tightened it.
    let budget = handler.config.max_message_chars.saturating_sub(32).min(4000);
    if tail.chars().count() > budget {
        let skip = tail.chars().count() - budget;
        tail = tail.chars().skip(skip).collect();
        if let Some(idx) = tail.find('\n') {
            tail = tail[idx + 1..].to_string();
        }
    }
    info_embed(
        format!("[{bound}] last {n} lines"),
        format!("```\n{tail}\n```"),
    )
}

async fn slash_new(
    handler: &Handler,
    cid: u64,
    name: Option<String>,
    cwd: Option<String>,
    persist: Option<bool>,
) -> CreateEmbed {
    let name = match name {
        Some(n) => n,
        None => match handler.broker.list_sessions().await {
            Ok(list) => auto_name(&list),
            Err(e) => return err_embed("Broker error", format!("`{e}`")),
        },
    };
    match handler.broker.create_session(&name, cwd.as_deref(), persist).await {
        Ok(_) => {
            handler.state.bind(cid, name.clone()).await;
            let mut embed = ok_embed(
                "Session created",
                format!("`{name}` is now bound to this channel"),
            );
            if let Some(c) = &cwd {
                embed = embed.field("cwd", format!("`{c}`"), false);
            }
            if let Some(p) = persist {
                embed = embed.field(
                    "persist",
                    if p {
                        "on (restored on broker restart)"
                    } else {
                        "ephemeral (forgotten on broker restart)"
                    },
                    false,
                );
            }
            embed
        }
        Err(e) => err_embed(format!("create `{name}`"), format!("`{e}`")),
    }
}

async fn slash_persist(handler: &Handler, cid: u64, on: bool) -> CreateEmbed {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.set_persist(&bound, on).await {
        Ok(_) => {
            let label = if on {
                "persist = on (restored on broker restart)"
            } else {
                "ephemeral (forgotten on broker restart)"
            };
            ok_embed(format!("Persist updated for `{bound}`"), label)
        }
        Err(e) => err_embed(format!("persist `{bound}`"), format!("`{e}`")),
    }
}

async fn slash_kill(handler: &Handler, name: &str) -> CreateEmbed {
    if name.is_empty() {
        return err_embed("Missing argument", "Usage: `/kill name:<session>`");
    }
    match handler.broker.delete_session(name).await {
        Ok(_) => {
            handler.state.unbind_all(name).await;
            ok_embed(
                format!("Killed `{name}`"),
                format!(
                    "Channels previously bound now fall back to `{}`",
                    handler.config.default_session
                ),
            )
        }
        Err(e) => err_embed(format!("kill `{name}`"), format!("`{e}`")),
    }
}

async fn slash_interrupt(handler: &Handler, cid: u64) -> CreateEmbed {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.interrupt_session(&bound).await {
        Ok(_) => ok_embed(
            format!("🛑 interrupted `{bound}`"),
            "Sent Ctrl+C to claude",
        ),
        Err(e) => err_embed(format!("interrupt `{bound}`"), format!("`{e}`")),
    }
}

async fn slash_restart(handler: &Handler, cid: u64) -> CreateEmbed {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.restart_session(&bound).await {
        Ok(_) => ok_embed(
            format!("🔄 Restarted claude in `{bound}`"),
            "Conversation history preserved (resumed via stored session id)",
        ),
        Err(e) => err_embed(format!("restart `{bound}`"), format!("`{e}`")),
    }
}

async fn slash_reload(handler: &Handler) -> CreateEmbed {
    match handler.broker.restart_agentmux().await {
        Ok(_) => ok_embed(
            "♻️ Restarting agentmux",
            "Bot will lose its WS connection for a few seconds, then reconnect to the fresh broker. \
             config.toml / discord.toml are re-read.",
        ),
        Err(e) => {
            // Most common failure: broker started outside the wrapper
            // script so AGENTMUX_LAUNCHER isn't set — broker returns 503
            // with explicit guidance, surface it verbatim.
            err_embed("Reload failed", format!("`{e}`"))
        }
    }
}

async fn slash_hibernate(handler: &Handler, cid: u64) -> CreateEmbed {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.hibernate_session(&bound).await {
        Ok(_) => ok_embed(
            format!("💤 Hibernated `{bound}`"),
            "Next message in this channel will wake it",
        ),
        Err(e) => err_embed(format!("hibernate `{bound}`"), format!("`{e}`")),
    }
}

fn slash_help() -> CreateEmbed {
    info_embed(
        "agentmux slash commands",
        "Plain text (or `!verb`) still works in whitelisted channels.",
    )
    .field(
        "Status",
        "`/ls` list sessions + bindings\n\
         `/status` channel binding\n\
         `/cwd` show bound session's cwd\n\
         `/logs [n]` last n lines of output",
        false,
    )
    .field(
        "Lifecycle",
        "`/interrupt` Ctrl+C this channel's session\n\
         `/restart` restart claude (history preserved)\n\
         `/hibernate` next message wakes it\n\
         `/reload` restart the whole stack (reloads config files)",
        false,
    )
    .field(
        "Sessions",
        "`/new [name] [cwd] [persist]` create + bind\n\
         `/attach name` rebind this channel\n\
         `/persist on:on|off` toggle restart-survival\n\
         `/kill name` destroy a session",
        false,
    )
}

fn auto_name(existing: &[crate::broker::SessionLite]) -> String {
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
