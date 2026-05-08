//! TOML load/save for the GUI.
//!
//! Round-trip is format-preserving (`toml_edit`): the user's comments,
//! blank lines, and field ordering survive a Save. The pattern is:
//!
//!   * **load**: read the file with `toml_edit` into a `DocumentMut`,
//!     then walk the doc to populate a typed `*Form` struct that the
//!     UI binds against.
//!   * **save**: walk the `Form` and assign each value back into the
//!     `DocumentMut` via `doc["key"] = value(...)`. `toml_edit`
//!     preserves the surrounding trivia (comments, whitespace), so
//!     existing fields keep their context. New fields appended at end.
//!
//! Atomic write via tmp + rename so a crash mid-save can't leave a
//! half-written config that the next broker startup chokes on.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use toml_edit::{value, Array, DocumentMut, Item, Value};

/// Wrapper so the rest of the app can ask "does this file exist on
/// disk?" without re-checking the filesystem. Always Some for
/// loaded-or-fresh; None means we'll create on first save.
pub struct Doc {
    pub doc: DocumentMut,
    pub existed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalChoice {
    Off,
    Ask,
}

impl ToolApprovalChoice {
    pub fn as_toml_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ask => "ask",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "ask" => Self::Ask,
            _ => Self::Off,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BrokerForm {
    pub http_addr: String,
    pub pipe_name: String,
    pub default_command: Vec<String>,
    pub ring_cap_bytes: i64,
    pub hibernate_idle_secs: i64,
    pub sessions_toml_path: String,
    pub pid_file_path: String,
    pub log_dir: String,
    pub auto_resume_default: bool,
    pub attach_token: String,
    pub default_cwd: String,
    pub tool_approval: ToolApprovalChoice,
    pub main_session: String,
    pub max_active_dispatches_per_session: i64,
    pub dispatch_timeout_secs: i64,
}

impl Default for BrokerForm {
    fn default() -> Self {
        // Mirror shared::config::Config::default(). Keep in sync when
        // the broker defaults change.
        Self {
            http_addr: "127.0.0.1:8765".into(),
            pipe_name: "claude-broker".into(),
            default_command: vec!["claude".into(), "--dangerously-skip-permissions".into()],
            ring_cap_bytes: 524_288,
            hibernate_idle_secs: 86_400,
            sessions_toml_path: String::new(),
            pid_file_path: String::new(),
            log_dir: String::new(),
            auto_resume_default: false,
            attach_token: String::new(),
            default_cwd: String::new(),
            tool_approval: ToolApprovalChoice::Off,
            main_session: String::new(),
            max_active_dispatches_per_session: 5,
            dispatch_timeout_secs: 1800,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscordForm {
    pub token_env: String,
    pub broker_http_url: String,
    pub broker_ws_url: String,
    pub channel_ids: Vec<String>,
    pub allowed_user_ids: Vec<String>,
    pub default_session: String,
    pub max_message_chars: i64,
    pub allow_dm: bool,
    pub notify_on_idle: bool,
    pub respond_to_mentions: bool,
    pub slash_command_guild_id: String,
    pub reply_quote_in_prompt: bool,
    pub react_with_actions: bool,
    pub broker_config_path: String,
    pub main_session: String,
    pub worker_thread_parent: String,
    pub dashboard_channel_id: String,
}

impl Default for DiscordForm {
    fn default() -> Self {
        // Mirror DiscordConfig::default(). Channel/user id lists are
        // stored as strings so the UI can render them in editable text
        // boxes; serialised as TOML integers on save.
        Self {
            token_env: "DISCORD_BOT_TOKEN".into(),
            broker_http_url: "http://127.0.0.1:8765".into(),
            broker_ws_url: "ws://127.0.0.1:8765/ws".into(),
            channel_ids: Vec::new(),
            allowed_user_ids: Vec::new(),
            default_session: "default".into(),
            max_message_chars: 1900,
            allow_dm: false,
            notify_on_idle: false,
            respond_to_mentions: false,
            slash_command_guild_id: "0".into(),
            reply_quote_in_prompt: true,
            react_with_actions: true,
            broker_config_path: String::new(),
            main_session: String::new(),
            worker_thread_parent: "0".into(),
            dashboard_channel_id: "0".into(),
        }
    }
}

// ---- load -------------------------------------------------------------

pub fn load_broker(path: &Path) -> (Doc, BrokerForm) {
    let (doc, existed) = read_doc(path);
    let form = BrokerForm {
        http_addr: read_str(&doc, "http_addr").unwrap_or_else(|| "127.0.0.1:8765".into()),
        pipe_name: read_str(&doc, "pipe_name").unwrap_or_else(|| "claude-broker".into()),
        default_command: read_string_array(&doc, "default_command").unwrap_or_else(|| {
            vec!["claude".into(), "--dangerously-skip-permissions".into()]
        }),
        ring_cap_bytes: read_int(&doc, "ring_cap_bytes").unwrap_or(524_288),
        hibernate_idle_secs: read_int(&doc, "hibernate_idle_secs").unwrap_or(86_400),
        sessions_toml_path: read_str(&doc, "sessions_toml_path").unwrap_or_default(),
        pid_file_path: read_str(&doc, "pid_file_path").unwrap_or_default(),
        log_dir: read_str(&doc, "log_dir").unwrap_or_default(),
        auto_resume_default: read_bool(&doc, "auto_resume_default").unwrap_or(false),
        attach_token: read_str(&doc, "attach_token").unwrap_or_default(),
        default_cwd: read_str(&doc, "default_cwd").unwrap_or_default(),
        tool_approval: ToolApprovalChoice::from_str(
            &read_str(&doc, "tool_approval").unwrap_or_default(),
        ),
        main_session: read_str(&doc, "main_session").unwrap_or_default(),
        max_active_dispatches_per_session: read_int(&doc, "max_active_dispatches_per_session")
            .unwrap_or(5),
        dispatch_timeout_secs: read_int(&doc, "dispatch_timeout_secs").unwrap_or(1800),
    };
    (Doc { doc, existed }, form)
}

pub fn load_discord(path: &Path) -> (Doc, DiscordForm) {
    let (doc, existed) = read_doc(path);
    let form = DiscordForm {
        token_env: read_str(&doc, "token_env").unwrap_or_else(|| "DISCORD_BOT_TOKEN".into()),
        broker_http_url: read_str(&doc, "broker_http_url")
            .unwrap_or_else(|| "http://127.0.0.1:8765".into()),
        broker_ws_url: read_str(&doc, "broker_ws_url")
            .unwrap_or_else(|| "ws://127.0.0.1:8765/ws".into()),
        channel_ids: read_int_array_as_strings(&doc, "channel_ids"),
        allowed_user_ids: read_int_array_as_strings(&doc, "allowed_user_ids"),
        default_session: read_str(&doc, "default_session").unwrap_or_else(|| "default".into()),
        max_message_chars: read_int(&doc, "max_message_chars").unwrap_or(1900),
        allow_dm: read_bool(&doc, "allow_dm").unwrap_or(false),
        notify_on_idle: read_bool(&doc, "notify_on_idle").unwrap_or(false),
        respond_to_mentions: read_bool(&doc, "respond_to_mentions").unwrap_or(false),
        slash_command_guild_id: read_int(&doc, "slash_command_guild_id")
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0".into()),
        reply_quote_in_prompt: read_bool(&doc, "reply_quote_in_prompt").unwrap_or(true),
        react_with_actions: read_bool(&doc, "react_with_actions").unwrap_or(true),
        broker_config_path: read_str(&doc, "broker_config_path").unwrap_or_default(),
        main_session: read_str(&doc, "main_session").unwrap_or_default(),
        worker_thread_parent: read_int(&doc, "worker_thread_parent")
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0".into()),
        dashboard_channel_id: read_int(&doc, "dashboard_channel_id")
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0".into()),
    };
    (Doc { doc, existed }, form)
}

fn read_doc(path: &Path) -> (DocumentMut, bool) {
    if let Ok(raw) = fs::read_to_string(path) {
        if let Ok(parsed) = raw.parse::<DocumentMut>() {
            return (parsed, true);
        }
    }
    (DocumentMut::new(), false)
}

fn read_str(doc: &DocumentMut, key: &str) -> Option<String> {
    doc.get(key)
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
}

fn read_int(doc: &DocumentMut, key: &str) -> Option<i64> {
    doc.get(key).and_then(|i| i.as_integer())
}

fn read_bool(doc: &DocumentMut, key: &str) -> Option<bool> {
    doc.get(key).and_then(|i| i.as_bool())
}

fn read_string_array(doc: &DocumentMut, key: &str) -> Option<Vec<String>> {
    let arr = doc.get(key)?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
    )
}

fn read_int_array_as_strings(doc: &DocumentMut, key: &str) -> Vec<String> {
    let Some(arr) = doc.get(key).and_then(|i| i.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_integer().map(|n| n.to_string()))
        .collect()
}

// ---- save -------------------------------------------------------------

pub fn save_broker(path: &Path, doc: &mut Doc, form: &BrokerForm) -> Result<()> {
    let d = &mut doc.doc;
    d["http_addr"] = value(form.http_addr.clone());
    d["pipe_name"] = value(form.pipe_name.clone());
    d["default_command"] = string_array_value(&form.default_command);
    d["ring_cap_bytes"] = value(form.ring_cap_bytes);
    d["hibernate_idle_secs"] = value(form.hibernate_idle_secs);
    d["sessions_toml_path"] = value(form.sessions_toml_path.clone());
    d["pid_file_path"] = value(form.pid_file_path.clone());
    d["log_dir"] = value(form.log_dir.clone());
    d["auto_resume_default"] = value(form.auto_resume_default);
    d["attach_token"] = value(form.attach_token.clone());
    d["default_cwd"] = value(form.default_cwd.clone());
    d["tool_approval"] = value(form.tool_approval.as_toml_str());
    d["main_session"] = value(form.main_session.clone());
    d["max_active_dispatches_per_session"] = value(form.max_active_dispatches_per_session);
    d["dispatch_timeout_secs"] = value(form.dispatch_timeout_secs);
    write_atomic(path, &d.to_string())?;
    doc.existed = true;
    Ok(())
}

pub fn save_discord(path: &Path, doc: &mut Doc, form: &DiscordForm) -> Result<()> {
    let d = &mut doc.doc;
    d["token_env"] = value(form.token_env.clone());
    d["broker_http_url"] = value(form.broker_http_url.clone());
    d["broker_ws_url"] = value(form.broker_ws_url.clone());
    d["channel_ids"] = string_list_to_int_array(&form.channel_ids);
    d["allowed_user_ids"] = string_list_to_int_array(&form.allowed_user_ids);
    d["default_session"] = value(form.default_session.clone());
    d["max_message_chars"] = value(form.max_message_chars);
    d["allow_dm"] = value(form.allow_dm);
    d["notify_on_idle"] = value(form.notify_on_idle);
    d["respond_to_mentions"] = value(form.respond_to_mentions);
    d["slash_command_guild_id"] = value(parse_u64_or_zero(&form.slash_command_guild_id) as i64);
    d["reply_quote_in_prompt"] = value(form.reply_quote_in_prompt);
    d["react_with_actions"] = value(form.react_with_actions);
    d["broker_config_path"] = value(form.broker_config_path.clone());
    d["main_session"] = value(form.main_session.clone());
    d["worker_thread_parent"] = value(parse_u64_or_zero(&form.worker_thread_parent) as i64);
    d["dashboard_channel_id"] = value(parse_u64_or_zero(&form.dashboard_channel_id) as i64);
    write_atomic(path, &d.to_string())?;
    doc.existed = true;
    Ok(())
}

fn string_array_value(items: &[String]) -> Item {
    let mut arr = Array::new();
    for s in items {
        arr.push(Value::from(s.as_str()));
    }
    Item::Value(Value::Array(arr))
}

fn string_list_to_int_array(items: &[String]) -> Item {
    let mut arr = Array::new();
    for s in items {
        if let Ok(n) = s.trim().parse::<i64>() {
            arr.push(Value::from(n));
        }
    }
    Item::Value(Value::Array(arr))
}

fn parse_u64_or_zero(s: &str) -> u64 {
    s.trim().parse::<u64>().unwrap_or(0)
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create dir {parent:?}"))?;
    }
    let tmp_ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => format!("{e}.tmp"),
        None => "tmp".to_string(),
    };
    let mut tmp: PathBuf = path.to_path_buf();
    tmp.set_extension(tmp_ext);
    fs::write(&tmp, content).with_context(|| format!("write {tmp:?}"))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        anyhow::bail!("rename {tmp:?} -> {path:?}: {e}");
    }
    Ok(())
}

// ---- restart-agentmux trigger -----------------------------------------

/// POST `/restart-agentmux` to the broker. Broker spawns a detached
/// respawner then exits — we'll be talking to a fresh broker on the
/// next request. 503 means the broker was started outside the wrapper
/// script and AGENTMUX_LAUNCHER isn't set; surface the message so the
/// user knows to restart from the CLI manually.
pub fn trigger_restart() -> Result<()> {
    let url = "http://127.0.0.1:8765/restart-agentmux";
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("build reqwest client")?;
    let resp = client.post(url).send().context("POST /restart-agentmux")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("{status}: {body}");
    }
    Ok(())
}

// ---- discord token helpers --------------------------------------------

/// Read the user-scope env var the discord config points at. Empty
/// string when unset. Used for the "current token" indicator on the
/// Discord tab.
pub fn read_token_from_env(token_env: &str) -> String {
    std::env::var(token_env).unwrap_or_default()
}

/// HEAD https://discord.com/api/v10/users/@me with the bot token —
/// returns Ok(bot_username) on 200, Err with the API message on
/// anything else. Synchronous since the GUI thread is already blocking
/// on the click anyway; the call is cheap (~200ms).
pub fn verify_discord_token(token: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build reqwest client")?;
    let resp = client
        .get("https://discord.com/api/v10/users/@me")
        .header("Authorization", format!("Bot {token}"))
        .send()
        .context("GET /users/@me")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        anyhow::bail!("{status}: {body}");
    }
    let body: serde_json::Value = resp.json().context("parse Discord response")?;
    let username = body
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    Ok(format!("{username} (id={id})"))
}

/// Set a User-scope env var on Windows. Linux/macOS: writes to
/// ~/.bashrc-style files would be too invasive; surface an instruction
/// instead. Returns the platform-appropriate verb (or an error).
pub fn save_token_to_env(token_env: &str, token: &str) -> Result<()> {
    #[cfg(windows)]
    {
        // Use PowerShell since [Environment]::SetEnvironmentVariable
        // with "User" scope persists to HKCU\Environment + broadcasts
        // WM_SETTINGCHANGE so freshly-spawned processes see it.
        let script = format!(
            "[Environment]::SetEnvironmentVariable('{name}', '{value}', 'User')",
            name = token_env.replace('\'', "''"),
            value = token.replace('\'', "''"),
        );
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .context("spawn powershell")?;
        if !status.success() {
            anyhow::bail!("powershell exited {status}");
        }
        // Also update the current process so the test button works
        // immediately without a relaunch.
        std::env::set_var(token_env, token);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        // Best we can do without rewriting shell rc files: set in
        // current process and warn the user.
        std::env::set_var(token_env, token);
        anyhow::bail!(
            "set in this process only; persist by adding to your shell rc:\n\
             export {token_env}='...'"
        )
    }
}

// ---- broker /sessions snapshot for orchestrator tab dropdowns ---------

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct SessionRow {
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub cwd: String,
}

pub fn fetch_sessions(broker_http: &str) -> Result<Vec<SessionRow>> {
    let url = format!("{}/sessions", broker_http.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("build reqwest client")?;
    let resp = client.get(&url).send().context(format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("{}: {}", resp.status(), resp.text().unwrap_or_default());
    }
    let rows: Vec<SessionRow> = resp.json().context("parse /sessions")?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips_broker() {
        let dir = std::env::temp_dir().join("agentmux-config-test-broker");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let mut doc = Doc {
            doc: DocumentMut::new(),
            existed: false,
        };
        let form = BrokerForm {
            http_addr: "0.0.0.0:9999".into(),
            main_session: "boss".into(),
            tool_approval: ToolApprovalChoice::Ask,
            ..BrokerForm::default()
        };
        save_broker(&path, &mut doc, &form).unwrap();

        let (_doc2, loaded) = load_broker(&path);
        assert_eq!(loaded.http_addr, "0.0.0.0:9999");
        assert_eq!(loaded.main_session, "boss");
        assert_eq!(loaded.tool_approval, ToolApprovalChoice::Ask);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_preserves_user_comments() {
        let dir = std::env::temp_dir().join("agentmux-config-test-comments");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // User-authored content with comments.
        fs::write(
            &path,
            "# my carefully written config\n\
             # do not delete\n\
             http_addr = \"127.0.0.1:8765\"\n\
             # main_session controls the orchestrator\n\
             main_session = \"default\"\n",
        )
        .unwrap();

        let (mut doc, mut form) = load_broker(&path);
        form.main_session = "boss".into();
        save_broker(&path, &mut doc, &form).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# my carefully written config"),
            "leading comment lost: {after}"
        );
        assert!(
            after.contains("# main_session controls the orchestrator"),
            "field comment lost: {after}"
        );
        assert!(after.contains("main_session = \"boss\""));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discord_int_arrays_round_trip() {
        let dir = std::env::temp_dir().join("agentmux-config-test-discord");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("discord.toml");

        let mut doc = Doc {
            doc: DocumentMut::new(),
            existed: false,
        };
        let form = DiscordForm {
            channel_ids: vec!["123456789012345678".into(), "987654321098765432".into()],
            allowed_user_ids: vec!["111111111111111111".into()],
            ..DiscordForm::default()
        };
        save_discord(&path, &mut doc, &form).unwrap();

        let (_doc2, loaded) = load_discord(&path);
        assert_eq!(loaded.channel_ids.len(), 2);
        assert!(loaded.channel_ids.contains(&"123456789012345678".to_string()));
        assert_eq!(loaded.allowed_user_ids, vec!["111111111111111111"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_int_strings_drop_silently() {
        // User typed nothing in a freshly-added channel field; we
        // shouldn't write an empty string into a TOML int array.
        let dir = std::env::temp_dir().join("agentmux-config-test-empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("discord.toml");

        let mut doc = Doc {
            doc: DocumentMut::new(),
            existed: false,
        };
        let form = DiscordForm {
            channel_ids: vec!["12345".into(), "".into(), "  ".into(), "67890".into()],
            ..DiscordForm::default()
        };
        save_discord(&path, &mut doc, &form).unwrap();

        let (_doc2, loaded) = load_discord(&path);
        assert_eq!(loaded.channel_ids, vec!["12345", "67890"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
