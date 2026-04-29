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
    Context, CreateAutocompleteResponse, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage,
};

use crate::ansi;
use crate::handler::Handler;

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
                    "Session name",
                )
                .required(true)
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
                    "Session name (auto-generated if omitted)",
                ),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "cwd",
                    "Working directory (broker default if omitted)",
                ),
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
                    CommandOptionType::Boolean,
                    "on",
                    "true = persist (auto-resume), false = ephemeral",
                )
                .required(true),
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
                .set_autocomplete(true),
            ),
        CreateCommand::new("interrupt").description("Ctrl+C this channel's session"),
        CreateCommand::new("restart")
            .description("Restart claude in this channel's session (history preserved)"),
        CreateCommand::new("hibernate")
            .description("Hibernate this channel's session (next message wakes it)"),
        CreateCommand::new("help").description("Show command help"),
    ]
}

pub async fn handle_command(handler: &Handler, ctx: &Context, cmd: &CommandInteraction) {
    let cid = cmd.channel_id.get();
    let body = match cmd.data.name.as_str() {
        "ls" => slash_ls(handler, cid).await,
        "status" => slash_status(handler, cid).await,
        "cwd" => slash_cwd(handler, cid).await,
        "attach" => {
            let name = string_arg(cmd, "name").unwrap_or_default();
            slash_attach(handler, cid, &name).await
        }
        "logs" => {
            let n = int_arg(cmd, "n").unwrap_or(30) as usize;
            slash_logs(handler, cid, n).await
        }
        "new" => {
            slash_new(
                handler,
                cid,
                string_arg(cmd, "name"),
                string_arg(cmd, "cwd"),
                bool_arg(cmd, "persist"),
            )
            .await
        }
        "persist" => {
            let on = bool_arg(cmd, "on").unwrap_or(false);
            slash_persist(handler, cid, on).await
        }
        "kill" => {
            let name = string_arg(cmd, "name").unwrap_or_default();
            slash_kill(handler, &name).await
        }
        "interrupt" => slash_interrupt(handler, cid).await,
        "restart" => slash_restart(handler, cid).await,
        "hibernate" => slash_hibernate(handler, cid).await,
        "help" => slash_help(),
        other => format!("unknown command `/{other}`"),
    };

    let resp = CreateInteractionResponseMessage::new().content(body);
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

// -------- per-verb implementations (return reply body) --------

async fn slash_ls(handler: &Handler, cid: u64) -> String {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    let list = match handler.broker.list_sessions().await {
        Ok(l) => l,
        Err(e) => return format!("❌ broker: {e}"),
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
    let snap = handler.state.bindings_snapshot().await;
    let other: Vec<_> = snap.into_iter().filter(|(c, _)| *c != cid).collect();
    if !other.is_empty() {
        out.push_str("\nother bindings:");
        for (c, s) in other {
            out.push_str(&format!("\n  <#{c}> → `{s}`"));
        }
    }
    out
}

async fn slash_status(handler: &Handler, cid: u64) -> String {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    format!("this channel is bound to session `{bound}`")
}

async fn slash_cwd(handler: &Handler, cid: u64) -> String {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.list_sessions().await {
        Ok(list) => match list.iter().find(|s| s.name == bound) {
            Some(s) => format!("`{}` cwd: `{}`", s.name, s.cwd),
            None => format!("❌ session `{bound}` not found (no longer alive?)"),
        },
        Err(e) => format!("❌ broker: {e}"),
    }
}

async fn slash_attach(handler: &Handler, cid: u64, name: &str) -> String {
    if name.is_empty() {
        return "usage: `/attach name:<session>`".into();
    }
    let exists = match handler.broker.list_sessions().await {
        Ok(l) => l.iter().any(|s| s.name == name),
        Err(e) => return format!("❌ broker: {e}"),
    };
    if !exists {
        return format!("❌ no session named `{name}`");
    }
    handler.state.bind(cid, name.to_string()).await;
    format!("✅ this channel is now bound to session `{name}`")
}

async fn slash_logs(handler: &Handler, cid: u64, n: usize) -> String {
    let n = n.clamp(1, 100);
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    let bytes = match handler.broker.get_ring(&bound).await {
        Ok(b) => b,
        Err(e) => return format!("❌ fetch ring `{bound}`: {e}"),
    };
    let stripped = ansi::strip(&bytes);
    let mut tail = ansi::last_lines(&stripped, n);
    if tail.is_empty() {
        tail.push_str("(buffer is empty)");
    }
    let budget = handler.config.max_message_chars.saturating_sub(32);
    if tail.chars().count() > budget {
        let skip = tail.chars().count() - budget;
        tail = tail.chars().skip(skip).collect();
        if let Some(idx) = tail.find('\n') {
            tail = tail[idx + 1..].to_string();
        }
    }
    format!("**[{bound}] last {n} lines**\n```\n{tail}\n```")
}

async fn slash_new(
    handler: &Handler,
    cid: u64,
    name: Option<String>,
    cwd: Option<String>,
    persist: Option<bool>,
) -> String {
    let name = match name {
        Some(n) => n,
        None => match handler.broker.list_sessions().await {
            Ok(list) => auto_name(&list),
            Err(e) => return format!("❌ broker: {e}"),
        },
    };
    match handler.broker.create_session(&name, cwd.as_deref(), persist).await {
        Ok(_) => {
            handler.state.bind(cid, name.clone()).await;
            let cwd_extra = cwd
                .as_ref()
                .map(|c| format!(" (cwd: `{c}`)"))
                .unwrap_or_default();
            let persist_extra = match persist {
                Some(true) => " [persist=on]",
                Some(false) => " [ephemeral]",
                None => "",
            };
            format!("✅ created `{name}` and bound this channel to it{cwd_extra}{persist_extra}")
        }
        Err(e) => format!("❌ create `{name}`: {e}"),
    }
}

async fn slash_persist(handler: &Handler, cid: u64, on: bool) -> String {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.set_persist(&bound, on).await {
        Ok(_) => {
            let label = if on {
                "persist=on (restored on broker restart)"
            } else {
                "ephemeral (forgotten on broker restart)"
            };
            format!("✅ `{bound}` → {label}")
        }
        Err(e) => format!("❌ persist `{bound}`: {e}"),
    }
}

async fn slash_kill(handler: &Handler, name: &str) -> String {
    if name.is_empty() {
        return "usage: `/kill name:<session>`".into();
    }
    match handler.broker.delete_session(name).await {
        Ok(_) => {
            handler.state.unbind_all(name).await;
            format!(
                "✅ killed `{name}` (channels previously bound now fall back to `{}`)",
                handler.config.default_session
            )
        }
        Err(e) => format!("❌ kill `{name}`: {e}"),
    }
}

async fn slash_interrupt(handler: &Handler, cid: u64) -> String {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.interrupt_session(&bound).await {
        Ok(_) => format!("🛑 interrupted `{bound}`"),
        Err(e) => format!("❌ interrupt `{bound}`: {e}"),
    }
}

async fn slash_restart(handler: &Handler, cid: u64) -> String {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.restart_session(&bound).await {
        Ok(_) => format!("🔄 restarted claude in `{bound}` (history preserved)"),
        Err(e) => format!("❌ restart `{bound}`: {e}"),
    }
}

async fn slash_hibernate(handler: &Handler, cid: u64) -> String {
    let bound = handler
        .state
        .resolve_or_bind(cid, &handler.config.default_session)
        .await;
    match handler.broker.hibernate_session(&bound).await {
        Ok(_) => format!("💤 hibernated `{bound}` — next message wakes it"),
        Err(e) => format!("❌ hibernate `{bound}`: {e}"),
    }
}

fn slash_help() -> String {
    "**agentmux slash commands**\n\
     `/ls` — list sessions and bindings\n\
     `/status` — show this channel's binding\n\
     `/cwd` — show bound session's cwd\n\
     `/attach name` — rebind this channel (autocomplete)\n\
     `/logs [n]` — last n lines of session output\n\
     `/new [name] [cwd] [persist]` — create + bind\n\
     `/persist on:bool` — toggle restart-survival\n\
     `/kill name` — destroy a session (autocomplete)\n\
     `/interrupt` `/restart` `/hibernate` — lifecycle\n\
     plain text (or `!verb`) still works in whitelisted channels"
        .to_string()
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
