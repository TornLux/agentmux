//! Runtime configuration shared by broker and viewer.
//!
//! Lookup order (first hit wins):
//!   1. `AGENT_CONFIG` env var → that file's path
//!   2. `%LOCALAPPDATA%\agentmux\config.toml`
//!   3. baked-in defaults (matches Phase 1-7.6 hard-coded behaviour)
//!
//! Missing fields fall through to defaults, so a partial config file
//! is fine. Parse errors emit a stderr warning and degrade to defaults
//! rather than failing startup.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// `host:port` the broker's HTTP control plane binds to. Both
    /// broker (listen) and viewer (resolve via http_url()) read this.
    pub http_addr: String,
    /// Win32 named pipe path. Must match between broker and viewer.
    pub pipe_name: String,
    /// argv used when broker starts a fresh session. Overridden by
    /// `broker.exe ... <override>` on the broker CLI.
    pub default_command: Vec<String>,
    /// Per-session ring buffer cap in bytes.
    pub ring_cap_bytes: usize,
    /// Auto-hibernate sessions whose PTY has been idle longer than
    /// this many seconds. 0 = never auto-hibernate (Phase 7.7 Part B
    /// wires up the periodic timer; Part A only honours manual /hibernate).
    pub hibernate_idle_secs: u64,
    /// Override path for sessions.toml. Empty string = use the default
    /// `%LOCALAPPDATA%\agentmux\sessions.toml`.
    pub sessions_toml_path: String,
    /// Override path for broker.pid. Empty = default
    /// `%LOCALAPPDATA%\agentmux\broker.pid`.
    pub pid_file_path: String,
    /// Override directory for broker logs (one file per day, kept 7
    /// days). Empty = default `%LOCALAPPDATA%\agentmux\logs`.
    pub log_dir: String,
    /// Default value of `auto_resume` on newly-created sessions when
    /// the create request doesn't specify one. Per-session value still
    /// wins; this only changes the default for unspecified creates.
    /// Set `false` for "ephemeral by default — opt in to persist";
    /// `true` (legacy behaviour) for "always persist by default".
    pub auto_resume_default: bool,
    /// Bearer token required on **non-loopback** HTTP/WS requests.
    /// Loopback (127.0.0.1, ::1) is always exempt so existing
    /// localhost tooling (claude-attach, platform-discord on the
    /// same host, hooks) keeps working without any token. Empty =
    /// LAN access disabled (non-loopback connections rejected with
    /// 401). Generate via `agentmux config token`.
    pub attach_token: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:8765".to_string(),
            pipe_name: r"\\.\pipe\claude-broker".to_string(),
            default_command: vec![
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ],
            ring_cap_bytes: 512 * 1024,
            hibernate_idle_secs: 86_400,
            sessions_toml_path: String::new(),
            pid_file_path: String::new(),
            log_dir: String::new(),
            auto_resume_default: false,
            attach_token: String::new(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        if let Ok(p) = std::env::var("AGENT_CONFIG") {
            eprintln!("config: AGENT_CONFIG → {}", p);
            return Self::load_path(&PathBuf::from(p));
        }
        let default_path = default_config_path();
        if default_path.exists() {
            eprintln!("config: loaded {}", default_path.display());
            return Self::load_path(&default_path);
        }
        eprintln!(
            "config: no file at {} — using built-in defaults",
            default_path.display()
        );
        Self::default()
    }

    fn load_path(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<Self>(&content) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("config parse error in {}: {e}", path.display());
                    eprintln!("  → falling back to defaults");
                    Self::default()
                }
            },
            Err(e) => {
                eprintln!("config read error in {}: {e}", path.display());
                Self::default()
            }
        }
    }

    pub fn http_url(&self) -> String {
        format!("http://{}", self.http_addr)
    }

    /// Resolved sessions.toml path: explicit override if set, otherwise
    /// `%LOCALAPPDATA%\agentmux\sessions.toml`.
    pub fn sessions_toml(&self) -> PathBuf {
        if !self.sessions_toml_path.is_empty() {
            return PathBuf::from(&self.sessions_toml_path);
        }
        local_appdata_dir().join("sessions.toml")
    }

    /// Resolved broker.pid path: explicit override if set, otherwise
    /// `%LOCALAPPDATA%\agentmux\broker.pid`.
    pub fn pid_file(&self) -> PathBuf {
        if !self.pid_file_path.is_empty() {
            return PathBuf::from(&self.pid_file_path);
        }
        local_appdata_dir().join("broker.pid")
    }

    /// Resolved log directory: explicit override if set, otherwise
    /// `%LOCALAPPDATA%\agentmux\logs`.
    pub fn log_dir(&self) -> PathBuf {
        if !self.log_dir.is_empty() {
            return PathBuf::from(&self.log_dir);
        }
        local_appdata_dir().join("logs")
    }
}

pub fn default_config_path() -> PathBuf {
    local_appdata_dir().join("config.toml")
}

fn local_appdata_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("agentmux")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_legacy_constants() {
        let c = Config::default();
        assert_eq!(c.http_addr, "127.0.0.1:8765");
        assert_eq!(c.pipe_name, r"\\.\pipe\claude-broker");
        assert_eq!(c.default_command[0], "claude");
        assert_eq!(c.ring_cap_bytes, 512 * 1024);
        assert_eq!(c.http_url(), "http://127.0.0.1:8765");
    }

    #[test]
    fn partial_toml_inherits_unspecified_fields() {
        let toml_src = r#"http_addr = "127.0.0.1:9999""#;
        let c: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(c.http_addr, "127.0.0.1:9999");
        assert_eq!(c.pipe_name, r"\\.\pipe\claude-broker"); // default
        assert_eq!(c.default_command[0], "claude"); // default
    }

    #[test]
    fn full_toml_overrides_everything() {
        let toml_src = r#"
http_addr = "0.0.0.0:1234"
pipe_name = '\\.\pipe\test-pipe'
default_command = ["pwsh.exe", "-NoLogo"]
ring_cap_bytes = 65536
hibernate_idle_secs = 0
sessions_toml_path = "C:\\custom\\sessions.toml"
pid_file_path = "C:\\custom\\broker.pid"
log_dir = "C:\\custom\\logs"
auto_resume_default = true
attach_token = "k7Rj9_secrettoken"
"#;
        let c: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(c.http_addr, "0.0.0.0:1234");
        assert_eq!(c.pipe_name, r"\\.\pipe\test-pipe");
        assert_eq!(c.default_command, vec!["pwsh.exe", "-NoLogo"]);
        assert_eq!(c.ring_cap_bytes, 65536);
        assert_eq!(c.hibernate_idle_secs, 0);
        assert_eq!(c.sessions_toml_path, "C:\\custom\\sessions.toml");
        assert_eq!(c.pid_file_path, "C:\\custom\\broker.pid");
        assert_eq!(c.log_dir, "C:\\custom\\logs");
        assert!(c.auto_resume_default);
        assert_eq!(c.attach_token, "k7Rj9_secrettoken");
    }

    #[test]
    fn default_paths_fall_back_to_localappdata() {
        let c = Config::default();
        assert!(c.sessions_toml().ends_with("sessions.toml"));
        assert!(c.pid_file().ends_with("broker.pid"));
        assert!(c.log_dir().ends_with("logs"));
    }
}
