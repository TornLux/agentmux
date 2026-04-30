//! Runtime configuration shared by broker and viewer.
//!
//! Lookup order (first hit wins):
//!   1. `AGENT_CONFIG` env var → that file's path
//!   2. `<local-appdata>/agentmux/config.toml`
//!      (Windows: `%LOCALAPPDATA%\agentmux\`; Linux:
//!      `~/.local/share/agentmux/`; macOS:
//!      `~/Library/Application Support/agentmux/`).
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
    /// Local-socket name shared by broker and viewer. Bare name (no
    /// `\\.\pipe\` prefix) — interprocess maps it to
    /// `\\.\pipe\Local\<name>` on Windows and `/tmp/<name>.sock` on
    /// Unix. Legacy `\\.\pipe\<name>` values from older configs are
    /// auto-stripped on load with a warning.
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
    /// Default working directory for newly-created sessions when the
    /// caller doesn't specify one (POST /sessions without `cwd`, the
    /// initial `default` session at first boot, etc.). Empty = use
    /// the broker process's startup cwd (legacy behaviour). Set this
    /// to make new-session cwd insensitive to which directory you
    /// happened to be in when you ran `.\agentmux start`.
    pub default_cwd: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:8765".to_string(),
            pipe_name: "claude-broker".to_string(),
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
            default_cwd: String::new(),
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
                Ok(mut cfg) => {
                    cfg.normalize_pipe_name();
                    cfg
                }
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

    /// Strip a legacy `\\.\pipe\` prefix from `pipe_name` if present.
    /// Pre-interprocess builds wrote the full Win32 pipe path into
    /// config; the new transport takes a bare namespace name and
    /// expands it to `\\.\pipe\Local\<name>` on Windows or
    /// `/tmp/<name>.sock` on Unix. Stripping is silent-but-warned so
    /// existing configs keep working without manual edits.
    fn normalize_pipe_name(&mut self) {
        const LEGACY_PREFIX: &str = r"\\.\pipe\";
        if let Some(stripped) = self.pipe_name.strip_prefix(LEGACY_PREFIX) {
            eprintln!(
                "config: pipe_name '{}' uses legacy \\\\.\\pipe\\ prefix — \
                 stripping to '{}'. Update your config.toml to silence this.",
                self.pipe_name, stripped
            );
            self.pipe_name = stripped.to_string();
        }
    }

    /// URL local clients (hooks, agentmux-tray, agentmux-cli probes,
    /// claude-attach in --broker mode against the same host) use to
    /// reach the broker.
    ///
    /// Substitutes the IPv4/IPv6 wildcard hosts with loopback because
    /// connecting to `0.0.0.0` / `[::]` is invalid on Windows
    /// (`WSAEADDRNOTAVAIL`, error 10049) — wildcard addresses are a
    /// listen-side concept ("any local interface"), not a valid
    /// destination. Without this substitution, switching `http_addr`
    /// to `0.0.0.0:8765` for LAN access silently breaks every hook
    /// (`AGENT_BROKER_URL` would inherit `http://0.0.0.0:8765` and
    /// every POST /event would fail).
    pub fn http_url(&self) -> String {
        let (host, port) = match self.http_addr.rsplit_once(':') {
            Some((h, p)) => (h, p),
            None => ("127.0.0.1", self.http_addr.as_str()),
        };
        let host = match host {
            "" | "0.0.0.0" => "127.0.0.1",
            "[::]" | "[::0]" | "[0:0:0:0:0:0:0:0]" => "[::1]",
            h => h,
        };
        format!("http://{host}:{port}")
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

    /// Resolved default cwd for newly-created sessions:
    ///   * config.default_cwd if non-empty AND points at an existing
    ///     directory,
    ///   * otherwise `fallback` (typically broker's startup cwd).
    /// Returns the path that was chosen plus a `bool` flagging
    /// whether the configured value was used (caller can log a
    /// startup warning when it had to fall back).
    pub fn resolve_default_cwd(&self, fallback: PathBuf) -> (PathBuf, DefaultCwdSource) {
        if self.default_cwd.is_empty() {
            return (fallback, DefaultCwdSource::Fallback);
        }
        let candidate = PathBuf::from(&self.default_cwd);
        if !candidate.is_dir() {
            return (fallback, DefaultCwdSource::ConfiguredButMissing);
        }
        (candidate, DefaultCwdSource::Configured)
    }
}

/// Where the resolved default-cwd actually came from. Plumbed up so
/// the caller (broker startup) can log a one-line notice — without
/// this, a typo in `default_cwd` would silently degrade to "broker's
/// startup cwd" and the user would chase a phantom bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultCwdSource {
    Configured,
    ConfiguredButMissing,
    Fallback,
}

pub fn default_config_path() -> PathBuf {
    local_appdata_dir().join("config.toml")
}

/// Per-user, per-machine app data directory for agentmux state files.
///
/// Resolution (via `dirs::data_local_dir`):
///   * Windows → `%LOCALAPPDATA%\agentmux\` — same path the
///     pre-cross-platform builds used.
///   * Linux   → `$XDG_DATA_HOME/agentmux/` (default `~/.local/share/agentmux/`).
///   * macOS   → `~/Library/Application Support/agentmux/`.
///   * If the OS lookup fails (very unusual — tests, sandboxes), we
///     fall back to `./agentmux/` under the current working directory
///     so a developer running from a checkout still gets a valid path.
pub fn local_appdata_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agentmux")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_legacy_constants() {
        let c = Config::default();
        assert_eq!(c.http_addr, "127.0.0.1:8765");
        assert_eq!(c.pipe_name, "claude-broker");
        assert_eq!(c.default_command[0], "claude");
        assert_eq!(c.ring_cap_bytes, 512 * 1024);
        assert_eq!(c.http_url(), "http://127.0.0.1:8765");
    }

    #[test]
    fn http_url_substitutes_wildcard_for_loopback() {
        // 0.0.0.0 as a destination is invalid on Windows (error 10049);
        // local clients (hooks etc.) need a real address.
        let mut c = Config::default();
        c.http_addr = "0.0.0.0:8765".to_string();
        assert_eq!(c.http_url(), "http://127.0.0.1:8765");

        c.http_addr = "[::]:9000".to_string();
        assert_eq!(c.http_url(), "http://[::1]:9000");

        // Non-wildcard hosts pass through unchanged.
        c.http_addr = "192.168.1.5:8765".to_string();
        assert_eq!(c.http_url(), "http://192.168.1.5:8765");
    }

    #[test]
    fn partial_toml_inherits_unspecified_fields() {
        let toml_src = r#"http_addr = "127.0.0.1:9999""#;
        let c: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(c.http_addr, "127.0.0.1:9999");
        assert_eq!(c.pipe_name, "claude-broker"); // default
        assert_eq!(c.default_command[0], "claude"); // default
    }

    #[test]
    fn full_toml_overrides_everything() {
        let toml_src = r#"
http_addr = "0.0.0.0:1234"
pipe_name = "test-pipe"
default_command = ["pwsh.exe", "-NoLogo"]
ring_cap_bytes = 65536
hibernate_idle_secs = 0
sessions_toml_path = "C:\\custom\\sessions.toml"
pid_file_path = "C:\\custom\\broker.pid"
log_dir = "C:\\custom\\logs"
auto_resume_default = true
attach_token = "k7Rj9_secrettoken"
default_cwd = "C:\\projects\\me"
"#;
        let c: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(c.http_addr, "0.0.0.0:1234");
        assert_eq!(c.pipe_name, "test-pipe");
        assert_eq!(c.default_command, vec!["pwsh.exe", "-NoLogo"]);
        assert_eq!(c.ring_cap_bytes, 65536);
        assert_eq!(c.hibernate_idle_secs, 0);
        assert_eq!(c.sessions_toml_path, "C:\\custom\\sessions.toml");
        assert_eq!(c.pid_file_path, "C:\\custom\\broker.pid");
        assert_eq!(c.log_dir, "C:\\custom\\logs");
        assert!(c.auto_resume_default);
        assert_eq!(c.attach_token, "k7Rj9_secrettoken");
        assert_eq!(c.default_cwd, "C:\\projects\\me");
    }

    #[test]
    fn resolve_default_cwd_falls_back_when_unset() {
        let c = Config::default();
        let fallback = std::env::temp_dir();
        let (resolved, src) = c.resolve_default_cwd(fallback.clone());
        assert_eq!(resolved, fallback);
        assert_eq!(src, DefaultCwdSource::Fallback);
    }

    #[test]
    fn resolve_default_cwd_uses_configured_when_present() {
        let mut c = Config::default();
        let real = std::env::temp_dir();
        c.default_cwd = real.to_string_lossy().to_string();
        let (resolved, src) = c.resolve_default_cwd(PathBuf::from(r"C:\never-used"));
        assert_eq!(resolved, real);
        assert_eq!(src, DefaultCwdSource::Configured);
    }

    #[test]
    fn resolve_default_cwd_warns_when_configured_path_missing() {
        let mut c = Config::default();
        // Path that almost certainly does not exist.
        c.default_cwd = r"C:\agentmux-missing-test-dir-9f4a".to_string();
        let fallback = std::env::temp_dir();
        let (resolved, src) = c.resolve_default_cwd(fallback.clone());
        assert_eq!(resolved, fallback);
        assert_eq!(src, DefaultCwdSource::ConfiguredButMissing);
    }

    #[test]
    fn normalize_strips_legacy_pipe_prefix() {
        let mut c = Config::default();
        c.pipe_name = r"\\.\pipe\old-style-name".to_string();
        c.normalize_pipe_name();
        assert_eq!(c.pipe_name, "old-style-name");

        // Idempotent — re-running on a bare name leaves it alone.
        c.normalize_pipe_name();
        assert_eq!(c.pipe_name, "old-style-name");

        // Bare names are unchanged.
        let mut bare = Config::default();
        bare.pipe_name = "already-bare".to_string();
        bare.normalize_pipe_name();
        assert_eq!(bare.pipe_name, "already-bare");
    }

    #[test]
    fn default_paths_fall_back_to_localappdata() {
        let c = Config::default();
        assert!(c.sessions_toml().ends_with("sessions.toml"));
        assert!(c.pid_file().ends_with("broker.pid"));
        assert!(c.log_dir().ends_with("logs"));
    }
}
