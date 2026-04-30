//! `discord.toml` loader.
//!
//! Lookup order (first hit wins):
//!   1. `AGENT_DISCORD_CONFIG` env var → that file's path
//!   2. `%LOCALAPPDATA%\agentmux\discord.toml`
//!   3. baked-in defaults (which crash on startup because there's no
//!      sensible default for the channel/user whitelists)
//!
//! The bot token NEVER lives in this file. We store the *name* of the
//! env var that holds the token, and resolve it at startup. Keeps
//! tokens out of file backups and accidental commits.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    /// Name of the env var to read the bot token from. The token itself
    /// is intentionally not a config field.
    pub token_env: String,
    /// Broker HTTP base URL. Used for `/sessions` and
    /// `/sessions/:key/input` calls.
    pub broker_http_url: String,
    /// Broker WebSocket URL. Phase 6a wires WS at `/ws` on the same
    /// listener as HTTP rather than the separate `:8766` PLAN.md
    /// originally drafted — one less listener and one less port to
    /// configure.
    pub broker_ws_url: String,
    /// Whitelist of Discord channel IDs the bot will read from. Empty
    /// = listen in every channel where the bot can see messages
    /// (DM-friendly but loose); set this for safety on real Servers.
    pub channel_ids: Vec<u64>,
    /// Whitelist of Discord user IDs whose messages the bot will act
    /// on. MUST be non-empty — the bot refuses to start otherwise so
    /// a misconfig can't accidentally make claude take orders from
    /// strangers.
    pub allowed_user_ids: Vec<u64>,
    /// Session name the bot is initially bound to. `!attach <name>`
    /// switches it at runtime (in-memory only — bot restart resets).
    pub default_session: String,
    /// Discord caps single-message text at 2000 chars; we split at
    /// this many to leave headroom for per-message decoration.
    pub max_message_chars: usize,
    /// When true, accept messages in 1:1 DM channels with whitelisted
    /// users (`allowed_user_ids`). Off by default so the bot can't
    /// accidentally take orders from DMs that bypass the channel
    /// whitelist; flip this on for solo / mobile use.
    pub allow_dm: bool,
    /// When true, forward Claude Code's "Claude is waiting for your
    /// input" idle pings (the Notification hook fires these after a
    /// short period of TUI inactivity, which means roughly once after
    /// every reply). Off by default — most users find them noisy.
    /// Permission prompts and other Notification messages always pass
    /// through regardless of this flag.
    pub notify_on_idle: bool,
    /// When true, accept messages from non-whitelisted server channels
    /// **only if** the bot is @mentioned in them. The mention is
    /// stripped from the prompt before forwarding. Off by default —
    /// avoids the bot getting drawn into channels it wasn't told
    /// about.
    pub respond_to_mentions: bool,
    /// Optional guild id for slash-command registration. When set,
    /// commands are registered as guild commands (instant updates,
    /// scoped to one server). When `0` / unset, commands register
    /// globally — propagation can take up to an hour. Set this for
    /// dev / single-server use; leave 0 for multi-guild bots.
    pub slash_command_guild_id: u64,
    /// When true, posting a Discord reply prepends a one-line
    /// `[replying to: "..."]` header to the forwarded prompt so
    /// claude has the quoted text as context. The reply-target
    /// session routing (in `state.rs`) is independent of this flag.
    /// On by default — quoting helps when the referenced message is
    /// from a different speaker / channel and claude wouldn't have
    /// it in its own transcript.
    pub reply_quote_in_prompt: bool,
    /// When true, certain Unicode emoji reactions on bot-posted
    /// assistant messages are interpreted as broker actions on the
    /// session that produced the message:
    ///
    ///   * 🛑 / ❌  → interrupt
    ///   * 💤      → hibernate
    ///   * 🔄 / 🔁 → restart
    ///
    /// Only reactions from `allowed_user_ids` count. Other emojis
    /// and reactions on unknown messages are ignored silently.
    /// On by default.
    pub react_with_actions: bool,
    /// Where to find the broker config (only its `pipe_name` is needed,
    /// and only when the bot one day learns to attach as a viewer).
    /// Empty = ignore. Reserved for later phases.
    pub broker_config_path: String,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            token_env: "DISCORD_BOT_TOKEN".to_string(),
            broker_http_url: "http://127.0.0.1:8765".to_string(),
            broker_ws_url: "ws://127.0.0.1:8765/ws".to_string(),
            channel_ids: Vec::new(),
            allowed_user_ids: Vec::new(),
            default_session: "default".to_string(),
            max_message_chars: 1900,
            allow_dm: false,
            notify_on_idle: false,
            respond_to_mentions: false,
            slash_command_guild_id: 0,
            reply_quote_in_prompt: true,
            react_with_actions: true,
            broker_config_path: String::new(),
        }
    }
}

impl DiscordConfig {
    pub fn load() -> anyhow::Result<Self> {
        let path = if let Ok(p) = std::env::var("AGENT_DISCORD_CONFIG") {
            PathBuf::from(p)
        } else {
            default_config_path()
        };
        let cfg = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
            toml::from_str::<Self>(&content)
                .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?
        } else {
            tracing::warn!(
                "no discord.toml at {} — falling back to defaults (will fail validation)",
                path.display()
            );
            Self::default()
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.allowed_user_ids.is_empty() {
            anyhow::bail!(
                "discord.toml: allowed_user_ids must contain at least one Discord user id"
            );
        }
        if self.token_env.is_empty() {
            anyhow::bail!("discord.toml: token_env must be set");
        }
        if self.default_session.is_empty() {
            anyhow::bail!("discord.toml: default_session must be set");
        }
        Ok(())
    }
}

pub fn default_config_path() -> PathBuf {
    shared::config::local_appdata_dir().join("discord.toml")
}
