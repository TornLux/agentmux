//! agentmux broker — Phase 7.5.
//!
//! Manages N concurrent claude sessions. Each session owns its own
//! ConPTY, ring buffer, broadcast channel, and input mpsc; sessions
//! are addressed by UUID `id` or by a human-friendly `name`. The pipe
//! protocol now requires a `HELLO` frame at connect time selecting
//! which session the viewer attaches to (defaults to `default` if
//! unspecified).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write as IoWrite;
use std::path::{Path as StdPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use shared::config::{Config, DefaultCwdSource};
use shared::frame::{
    decode_control, decode_frame, decode_hello, decode_resize, encode_frame, read_frame,
    write_frame, HelloPayload, CTRL_INTERRUPT, CTRL_RESTART, CTRL_SHUTDOWN, TAG_CONTROL,
    TAG_HELLO, TAG_PTY_DATA, TAG_REPLAY_END, TAG_RESIZE,
};
use interprocess::local_socket::tokio::Stream as LocalStream;
use interprocess::local_socket::traits::tokio::Listener as TokioListenerTrait;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToNsName};
use tokio::sync::{broadcast, watch};
use tracing::{error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};

mod manager;
mod orchestrator;
mod ringbuf;
mod session;

use manager::Manager;
use session::{ClientInfo, Session, SessionInfo, SessionState};

const DEFAULT_SESSION_NAME: &str = "default";

/// Append-only JSONL sink for hook events. The Mutex protects file
/// ordering across concurrent /event requests; once a write fails we
/// drop the handle and stop trying rather than spam log noise.
/// Daily-rolling JSONL audit log. Files are written to
/// `<dir>/<stem>.YYYY-MM-DD.jsonl` (UTC dates). Files older than
/// `retention_days` are pruned at each day rollover.
struct EventsLog {
    dir: PathBuf,
    stem: String,
    inner: Mutex<EventsLogInner>,
    retention_days: u32,
}

struct EventsLogInner {
    file: Option<File>,
    current_date: String,
}

const EVENTS_RETENTION_DAYS: u32 = 7;

impl EventsLog {
    /// Open / create today's events file. The legacy single-file
    /// path (`events.jsonl`) is preserved on disk if it existed but
    /// new appends roll into dated siblings (`events.YYYY-MM-DD.jsonl`).
    fn open(legacy_path: PathBuf) -> Self {
        let dir = legacy_path
            .parent()
            .map(StdPath::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let stem = legacy_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("events")
            .to_string();
        let _ = std::fs::create_dir_all(&dir);

        let date = today_utc_date();
        let file = open_dated(&dir, &stem, &date);
        let log = Self {
            dir,
            stem,
            inner: Mutex::new(EventsLogInner {
                file,
                current_date: date,
            }),
            retention_days: EVENTS_RETENTION_DAYS,
        };
        log.prune();
        log
    }

    fn append(&self, mut event: serde_json::Value) {
        if let Some(obj) = event.as_object_mut() {
            obj.entry("ts").or_insert_with(|| {
                let ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                serde_json::json!(ms)
            });
        }
        let line = format!("{event}\n");

        // Detect day rollover and reopen if needed. Cheap: compares
        // a 10-byte string per event, no per-event stat() of the file.
        let today = today_utc_date();
        let mut g = self.inner.lock().unwrap();
        if today != g.current_date {
            // Drop yesterday's handle; open today's. Pruning is on
            // a separate path so failure here doesn't block writes.
            g.file = open_dated(&self.dir, &self.stem, &today);
            g.current_date = today.clone();
            // Release lock before pruning (which scans the dir).
            drop(g);
            self.prune();
            g = self.inner.lock().unwrap();
        }
        if let Some(f) = g.file.as_mut() {
            if let Err(e) = f.write_all(line.as_bytes()) {
                warn!(
                    "events.{}.jsonl write: {e}",
                    g.current_date
                );
                g.file = None;
                return;
            }
            let _ = f.flush();
        }
    }

    /// Delete dated event files older than `retention_days`. Silent
    /// on individual failures — best-effort, log on errors.
    fn prune(&self) {
        let cutoff_days = match self.retention_days as u64 {
            0 => return, // disabled
            n => n,
        };
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        let prefix = format!("{}.", self.stem);
        let suffix = ".jsonl";
        let today_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff_secs = today_secs.saturating_sub(cutoff_days * 86_400);

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            if !name_str.starts_with(&prefix) || !name_str.ends_with(suffix) {
                continue;
            }
            let prefix_len = prefix.len();
            let suffix_start = name_str.len() - suffix.len();
            // Reject names where prefix and suffix overlap or touch
            // (e.g. legacy `events.jsonl` matches both bookends but
            // has no date segment in between).
            if prefix_len >= suffix_start {
                continue;
            }
            let date_str = &name_str[prefix_len..suffix_start];
            let secs = match date_to_secs(date_str) {
                Some(s) => s,
                None => continue, // not a YYYY-MM-DD-shaped filename
            };
            if secs < cutoff_secs {
                let path = entry.path();
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!("prune {:?}: {e}", path);
                } else {
                    info!("pruned old events log: {:?}", path);
                }
            }
        }
    }
}

fn open_dated(dir: &StdPath, stem: &str, date: &str) -> Option<File> {
    let path = dir.join(format!("{stem}.{date}.jsonl"));
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| warn!("cannot open events log {:?}: {e}", path))
        .ok()
}

/// Civil-from-days algorithm (Howard Hinnant) — converts an integer
/// number of days since the Unix epoch into a (year, month, day)
/// gregorian triple. Avoids pulling in `chrono` for one date format.
fn ymd_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + (if m <= 2 { 1 } else { 0 });
    (y as i32, m as u32, d as u32)
}

fn today_utc_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = ymd_from_days((secs / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Inverse: parse "YYYY-MM-DD" back into seconds since epoch
/// (start of that day, UTC). Returns None for malformed input.
fn date_to_secs(s: &str) -> Option<u64> {
    let mut parts = s.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=9999).contains(&y) {
        return None;
    }
    // Inverse civil algorithm.
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;
    if days < 0 {
        return None;
    }
    Some((days as u64) * 86_400)
}

fn default_events_path() -> PathBuf {
    shared::config::local_appdata_dir().join("events.jsonl")
}

#[derive(Clone)]
struct AppState {
    manager: Arc<Manager>,
    shutdown_tx: watch::Sender<bool>,
    events: Arc<EventsLog>,
    default_cwd: PathBuf,
    config: Arc<Config>,
    /// Fan-out channel for hook events. `http_event` publishes after
    /// annotating; `/ws` subscribers receive a JSON line per event.
    /// Capacity is per-subscriber — laggy bots get a `Lagged` error
    /// and we just log it (PLAN §4.1: missing intermediate events is
    /// acceptable; bots resync on next event).
    event_bus: broadcast::Sender<serde_json::Value>,
    /// In-flight PreToolUse approval requests. The hook is parked on
    /// a long-poll HTTP request; when the bot posts a decision via
    /// `/tool-decision/:id` we fire the matching oneshot and unblock
    /// the hook. `request_id` is a UUID picked at /tool-request time.
    pending_decisions: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<ToolDecision>>>>,
    /// Cross-session task dispatch queue. Any session can register a
    /// callback against another session via `/sessions/:caller/dispatch`;
    /// when the target's next `assistant_message` lands, broker injects
    /// a synthetic `[SYSTEM: task-complete]` block back into the caller.
    /// Persisted to `dispatches.toml` on every push/pop.
    orchestrator: Arc<orchestrator::OrchestratorState>,
    /// One-line, human-readable description of what each session is
    /// currently doing — derived from `tool_progress` /
    /// `assistant_message` / `notification` events as they pass
    /// through `http_event`. Surfaced via `GET /sessions` and the
    /// `session_status_changed` WS event so the dashboard panel can
    /// render at-a-glance state without parsing transcripts.
    /// Keyed by broker session id. In-memory only — derived from the
    /// event stream, so a restart simply waits for the next event.
    session_statuses: Arc<Mutex<HashMap<String, String>>>,
}

const EVENT_BUS_CAP: usize = 256;

/// Default wait-time on `/tool-request`. Most decisions take seconds;
/// 5 minutes is "user is on their phone, walking, will get to it."
/// Past this, the hook treats the response as `deny` so claude isn't
/// stuck forever.
const PRETOOL_DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDecision {
    /// `allow` lets claude run the tool. `deny` blocks it; `reason`
    /// is shown to claude so it knows why.
    pub allow: bool,
    #[serde(default)]
    pub reason: String,
}

/// Owns the broker.pid file so it gets cleaned up when main returns.
/// Drop won't fire on SIGKILL / panic, but start-broker.ps1 detects
/// stale PID files at startup so a leak just delays cleanup until the
/// next launch.
struct PidGuard {
    path: PathBuf,
}

impl PidGuard {
    fn install(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&path, std::process::id().to_string())
            .with_context(|| format!("write pid file {path:?}"))?;
        Ok(Self { path })
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Initialise tracing with a daily-rolling file appender. Writes are
/// synchronous (no non_blocking wrapper) — broker emits a few log
/// lines per minute at most so the perf cost is invisible, and we
/// avoid losing the last few lines when the process is hard-killed
/// before a non_blocking flush worker can drain its queue.
fn init_logging(log_dir: &std::path::Path) {
    let env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info".into())
    };
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!("logging: cannot create log dir {log_dir:?}: {e}");
        tracing_subscriber::fmt().with_env_filter(env_filter()).init();
        return;
    }
    let appender = match RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("broker")
        .filename_suffix("log")
        .max_log_files(7)
        .build(log_dir)
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("logging: rolling appender at {log_dir:?}: {e}");
            tracing_subscriber::fmt().with_env_filter(env_filter()).init();
            return;
        }
    };
    tracing_subscriber::fmt()
        .with_writer(appender)
        .with_ansi(false)
        .with_env_filter(env_filter())
        .init();
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Config has to load before tracing init because the log dir lives
    // inside Config. Config uses eprintln! for any complaints, so it
    // doesn't need tracing.
    let config = Arc::new(Config::load());
    init_logging(&config.log_dir());

    let _pid_guard = PidGuard::install(config.pid_file()).context("install pid file")?;

    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    let mut cwd_override: Option<PathBuf> = None;
    if let Some(pos) = raw.iter().position(|s| s == "--cwd") {
        if pos + 1 >= raw.len() {
            anyhow::bail!("--cwd requires a path argument");
        }
        let path = raw.remove(pos + 1);
        raw.remove(pos);
        cwd_override = Some(PathBuf::from(path));
    }
    // CLI argv overrides config.default_command if any positional args
    // remain after stripping flags.
    let argv: Vec<String> = if raw.is_empty() {
        config.default_command.clone()
    } else {
        raw
    };
    // Three-layer cwd resolution:
    //   1. CLI `--cwd <path>` (highest — explicit per-launch override)
    //   2. config.toml `default_cwd` (per-machine pin — what most users want)
    //   3. broker's own startup directory (legacy fallback)
    // The chosen cwd becomes the `default_cwd` of the AppState, which
    // is the seed for the initial `default` session AND the fallback
    // for any POST /sessions that doesn't pass `cwd`.
    let runtime_cwd = match cwd_override {
        Some(c) => c,
        None => {
            let startup = std::env::current_dir().context("get cwd")?;
            let (resolved, src) = config.resolve_default_cwd(startup);
            match src {
                DefaultCwdSource::Configured => {
                    info!("using configured default_cwd: {}", resolved.display());
                }
                DefaultCwdSource::ConfiguredButMissing => {
                    warn!(
                        "config.toml default_cwd = {:?} does not exist; \
                         falling back to broker's startup cwd: {}",
                        config.default_cwd,
                        resolved.display()
                    );
                }
                DefaultCwdSource::Fallback => {} // boring; no log
            }
            resolved
        }
    };
    let cwd = runtime_cwd;
    if !cwd.is_dir() {
        anyhow::bail!("cwd does not exist or is not a directory: {:?}", cwd);
    }
    info!(
        "broker starting: cmd={:?} cwd={:?} http={} pipe={} hibernate_idle_secs={} sessions_toml={} log_dir={} pid_file={}",
        argv,
        cwd,
        config.http_addr,
        config.pipe_name,
        config.hibernate_idle_secs,
        config.sessions_toml().display(),
        config.log_dir().display(),
        config.pid_file().display(),
    );

    let events_path = std::env::var_os("AGENT_EVENTS_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(default_events_path);
    info!("events log: {:?}", events_path);
    let events = Arc::new(EventsLog::open(events_path));

    let manager = Arc::new(Manager::new(config.clone(), argv));
    // If sessions.toml restored a session named "default" we keep it
    // (Hibernated, will resume on first attach). Otherwise spin up a
    // fresh one so existing UX still works on a clean install.
    if manager.get_by_id_or_name(DEFAULT_SESSION_NAME).is_none() {
        let default_session = manager
            .create(DEFAULT_SESSION_NAME.to_string(), cwd.clone(), true, None)
            .context("create default session")?;
        info!(
            "default session id={} (name={}, fresh)",
            default_session.id, default_session.name
        );
    } else {
        info!("default session restored from sessions.toml");
    }

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let (event_bus, _) = broadcast::channel::<serde_json::Value>(EVENT_BUS_CAP);
    let dispatches_file = shared::config::local_appdata_dir().join("dispatches.toml");
    let orchestrator_state = Arc::new(orchestrator::OrchestratorState::new(dispatches_file));
    let app_state = AppState {
        manager: manager.clone(),
        shutdown_tx: shutdown_tx.clone(),
        events,
        default_cwd: cwd,
        config: config.clone(),
        event_bus,
        pending_decisions: Arc::new(Mutex::new(HashMap::new())),
        orchestrator: orchestrator_state,
        session_statuses: Arc::new(Mutex::new(HashMap::new())),
    };

    // Idle hibernate scanner. Disabled at 0; otherwise wakes every
    // 60s and hibernates Idle sessions with no attached viewers
    // whose last_activity exceeds the threshold.
    if config.hibernate_idle_secs > 0 {
        let m = manager.clone();
        let threshold = Duration::from_secs(config.hibernate_idle_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await; // skip immediate fire
            loop {
                tick.tick().await;
                for s in m.all() {
                    if s.state() != SessionState::Idle {
                        continue;
                    }
                    if !s.attached_clients().is_empty() {
                        continue;
                    }
                    if s.last_activity_age() < threshold {
                        continue;
                    }
                    info!(
                        "auto-hibernating idle session {} ({}) — idle for {:?}",
                        s.id,
                        s.name,
                        s.last_activity_age()
                    );
                    let to_hib = s.clone();
                    tokio::task::spawn_blocking(move || to_hib.hibernate());
                }
            }
        });
    }

    // Orchestrator bootstrap: inject the orchestrator system prompt
    // into `main_session` exactly once across that session's lifetime,
    // tracked via the persistent `orchestrator_bootstrapped` flag in
    // sessions.toml. Critically, this DOES NOT use claude_session_id
    // as the heuristic — a session can have a transcript yet predate
    // the user configuring main_session (the common case: user adds
    // main_session to config.toml after the default session has
    // already been chatting), in which case the prompt has never been
    // seen. The flag pivots correctly for both fresh and pre-existing
    // sessions: bootstrap iff the flag is unset.
    if !config.main_session.is_empty() {
        match manager.get_by_id_or_name(&config.main_session) {
            Some(main) if !main.orchestrator_bootstrapped() => {
                let main_clone = main.clone();
                let main_name = main.name.clone();
                let manager_for_save = manager.clone();
                tokio::spawn(async move {
                    // deliver_to_session → write_user_burst awaits the
                    // session's boot_ready watch; no need for a
                    // separate pre-wait here.
                    let prompt = format_orchestrator_bootstrap();
                    match deliver_to_session(&main_clone, &prompt).await {
                        Ok(_) => {
                            // Flip + persist so a future broker restart
                            // sees the flag and skips re-injection.
                            main_clone.mark_orchestrator_bootstrapped();
                            if let Err(e) = manager_for_save.save() {
                                warn!(
                                    "save sessions.toml after orchestrator bootstrap of {main_name}: {e}"
                                );
                            }
                            info!("orchestrator prompt injected into {main_name}");
                        }
                        Err(e) => {
                            warn!("orchestrator bootstrap inject to {main_name}: {e}");
                        }
                    }
                });
            }
            Some(main) => {
                info!(
                    "main_session={} already orchestrator_bootstrapped — skipping inject (prompt already in transcript)",
                    main.name
                );
            }
            None => {
                warn!(
                    "config.main_session={} but no such session exists — orchestrator bootstrap skipped. \
                     Either create the session or change main_session in config.toml.",
                    config.main_session
                );
            }
        }
    }

    // Dispatch timeout scanner. Wakes every 60s, drains any callbacks
    // whose deadline has passed, and injects a `[SYSTEM: task-timeout]`
    // block into each caller. Without this, a worker that crashes or
    // gets stuck would leave its caller waiting forever.
    if config.dispatch_timeout_secs > 0 {
        let app = app_state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await; // skip immediate fire
            loop {
                tick.tick().await;
                let now = orchestrator::now_unix_ms();
                let expired = app.orchestrator.drain_expired(now);
                if expired.is_empty() {
                    continue;
                }
                info!("dispatch timeout: {} callback(s) expired", expired.len());
                for cb in expired {
                    let target_name = app
                        .manager
                        .get_by_id_or_name(&cb.target_session_id)
                        .map(|t| t.name.clone())
                        .unwrap_or_else(|| cb.target_session_id.clone());
                    let synthesized = orchestrator::format_task_timeout(&cb, &target_name);
                    let caller_id = cb.caller_session_id.clone();
                    let task_id_log = cb.task_id.clone();
                    let app_inner = app.clone();
                    tokio::spawn(async move {
                        let caller = match app_inner.manager.get_by_id_or_name(&caller_id) {
                            Some(c) => c,
                            None => {
                                warn!(
                                    "timeout task_id={task_id_log}: caller {caller_id} \
                                     no longer exists, dropping"
                                );
                                return;
                            }
                        };
                        if let Err(e) = deliver_to_session(&caller, &synthesized).await {
                            warn!(
                                "timeout inject task_id={task_id_log} → caller={}: {e}",
                                caller.name
                            );
                        }
                    });
                }
            }
        });
    }

    let http_fut = run_http_server(app_state.clone());
    let pipe_fut = run_pipe_server(manager.clone(), app_state.clone());

    tokio::select! {
        _ = http_fut => warn!("http server stopped unexpectedly"),
        res = pipe_fut => {
            if let Err(e) = res { error!("pipe server: {e}"); }
        }
        _ = shutdown_rx.changed() => {
            info!("shutdown signal received");
        }
    }

    info!("tearing down");
    manager.shutdown_all();
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}

async fn run_http_server(app_state: AppState) {
    let addr = app_state.config.http_addr.clone();

    // Private routes — wrapped in auth middleware (loopback bypass + Bearer
    // / WS subprotocol token check for non-loopback peers).
    let private = Router::new()
        .route("/sessions", get(http_list).post(http_create))
        .route("/sessions/:key", get(http_get).delete(http_delete))
        .route("/sessions/:key/state", get(http_state))
        .route("/sessions/:key/interrupt", post(http_interrupt))
        .route("/sessions/:key/restart", post(http_restart))
        .route("/sessions/:key/hibernate", post(http_hibernate))
        .route("/sessions/:key/demote", post(http_demote))
        .route("/sessions/:key/adopt", post(http_adopt))
        .route("/sessions/:key/input", post(http_input))
        .route("/sessions/:caller/dispatch", post(http_dispatch))
        .route(
            "/sessions/:caller/spawn-and-dispatch",
            post(http_spawn_and_dispatch),
        )
        .route("/sessions/:key/persist", post(http_set_persist))
        .route("/sessions/:key/ring", get(http_ring_snapshot))
        .route("/shutdown", post(http_shutdown))
        .route("/restart-agentmux", post(http_restart_agentmux))
        .route("/event", post(http_event))
        .route("/tool-request", post(http_tool_request))
        .route("/tool-decision/:request_id", post(http_tool_decision))
        .route("/ws", get(http_ws_upgrade))
        .route("/attach", get(http_attach_upgrade))
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            auth_middleware,
        ));

    // Public routes — the web viewer's static page must be reachable
    // before the user has a chance to enter their token, so it's not
    // gated. The page itself only triggers privileged calls (/sessions,
    // /attach) once the user pastes a token, and those still go through
    // the auth middleware.
    let public = Router::new()
        .route("/", get(serve_web_index))
        .route("/web", get(serve_web_index))
        .route("/web/", get(serve_web_index))
        .route("/web/vendor/xterm.min.js", get(serve_xterm_js))
        .route("/web/vendor/xterm.min.css", get(serve_xterm_css))
        .route("/web/vendor/addon-fit.min.js", get(serve_addon_fit_js));

    let app = public.merge(private).with_state(app_state);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("bind {addr}: {e}");
            return;
        }
    };
    info!("http control plane on http://{addr}");
    // ConnectInfo lets the auth middleware see each peer's IP so it
    // can distinguish loopback (always allowed) from non-loopback
    // (must present `Authorization: Bearer <attach_token>`).
    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    {
        error!("axum serve: {e}");
    }
}

/// Token-gated middleware. Loopback peers (127.0.0.1, ::1) skip the
/// check entirely so existing localhost tooling (claude-attach via
/// pipe, platform-discord on the same host, hooks) keeps working
/// with no token configured. Non-loopback peers must:
///   1. have a non-empty `attach_token` configured in the broker, AND
///   2. send `Authorization: Bearer <token>` with a constant-time
///      match.
/// Failure logs source IP at warn level (never the attempted token).
async fn auth_middleware(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    State(s): State<AppState>,
    headers: axum::http::HeaderMap,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    if addr.ip().is_loopback() {
        return Ok(next.run(req).await);
    }
    let token = s.config.attach_token.as_str();
    if token.is_empty() {
        warn!(
            "denied non-loopback request from {addr}: broker has no attach_token configured"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Primary: Authorization: Bearer <token>. Native CLI viewer and
    // anyone who can set arbitrary headers (curl, claude-attach) use
    // this path.
    let presented_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    // Fallback: WebSocket subprotocol auth. Browsers cannot set
    // arbitrary headers on `new WebSocket(...)`, but they CAN offer
    // subprotocols. We accept any offered protocol formatted
    // `bearer.<token>`; the upgrade handler echoes it back so the
    // browser doesn't reject the upgrade response.
    let presented_subprotocol = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .find_map(|p| p.strip_prefix("bearer."));

    let presented = presented_header.or(presented_subprotocol).unwrap_or("");
    if !ct_eq_str(presented, token) {
        warn!("denied non-loopback request from {addr}: invalid token");
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

/// Length-checked, byte-by-byte XOR-OR comparison. Constant time
/// in the length of the longer string (the early `len() != len()`
/// short-circuit is fine — knowing the token's length doesn't
/// meaningfully reduce its 256-bit search space).
fn ct_eq_str(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn run_pipe_server(manager: Arc<Manager>, app_state: AppState) -> Result<()> {
    let pipe_name = app_state.config.pipe_name.clone();
    info!("listening on local socket {}", pipe_name);
    let ns_name = pipe_name
        .clone()
        .to_ns_name::<GenericNamespaced>()
        .with_context(|| format!("encode local-socket name {pipe_name}"))?;
    let listener = ListenerOptions::new()
        .name(ns_name)
        .create_tokio()
        .with_context(|| format!("bind local socket {pipe_name}"))?;
    let next_id = Arc::new(AtomicU64::new(1));

    loop {
        let connected = listener.accept().await.context("local socket accept")?;
        let viewer_id = next_id.fetch_add(1, Ordering::Relaxed);
        let manager = manager.clone();
        let app_state = app_state.clone();
        tokio::spawn(handle_client(viewer_id, connected, manager, app_state));
    }
}

async fn handle_client(
    viewer_id: u64,
    pipe: LocalStream,
    manager: Arc<Manager>,
    app_state: AppState,
) {
    let (mut read_half, write_half) = tokio::io::split(pipe);

    let (session, hello) = match wait_for_hello(&mut read_half, &manager, viewer_id).await {
        Some(s) => s,
        None => {
            return;
        }
    };

    info!(
        "viewer #{viewer_id} connected ({}) → session {} ({})",
        hello.client_kind, session.id, session.name
    );

    session.register_viewer(ClientInfo {
        viewer_id,
        client_id: hello.client_id.clone(),
        client_kind: hello.client_kind.clone(),
    });
    session.touch_activity();

    let (snapshot, out_rx) = {
        let ring = session.pty_out.ring.lock().unwrap();
        let snap = ring.snapshot();
        let rx = session.pty_out.tx.subscribe();
        (snap, rx)
    };

    // Three independent clones — one for each consumer — so neither
    // async block needs to share ownership of the original Arc. We
    // also stash the id (a String) for the post-select cleanup so we
    // don't need to keep the Arc around after the moves.
    let cleanup_size_table = session.size_table.clone();
    let cleanup_session_id = session.id.clone();
    let inbound_session = session.clone();
    let outbound_session = session;
    let inbound_app = app_state;

    let inbound = async move {
        let mut r = read_half;
        loop {
            match read_frame(&mut r).await {
                Ok((TAG_PTY_DATA, payload)) => {
                    if inbound_session
                        .input_tx
                        .send(Bytes::from(payload))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok((TAG_RESIZE, payload)) => match decode_resize(&payload) {
                    Some((c, r)) => inbound_session.size_table.update(viewer_id, c, r),
                    None => warn!("viewer #{viewer_id} bad RESIZE"),
                },
                Ok((TAG_CONTROL, payload)) => match decode_control(&payload) {
                    Some(cmd) => {
                        handle_control_cmd(viewer_id, &inbound_session, cmd, &inbound_app).await
                    }
                    None => warn!("viewer #{viewer_id} bad CONTROL"),
                },
                Ok((TAG_HELLO, _)) => {
                    warn!("viewer #{viewer_id} unexpected second HELLO");
                }
                Ok((tag, _)) => warn!("viewer #{viewer_id} unknown tag {tag:#x}"),
                Err(_) => break,
            }
        }
    };

    let outbound = async move {
        let mut w = write_half;
        if !snapshot.is_empty() {
            info!(
                "viewer #{viewer_id} replaying {} bytes (session {})",
                snapshot.len(),
                outbound_session.name
            );
            if write_frame(&mut w, TAG_PTY_DATA, &snapshot).await.is_err() {
                return;
            }
        }
        let _ = write_frame(&mut w, TAG_REPLAY_END, &[]).await;

        let mut out_rx = out_rx;
        loop {
            match out_rx.recv().await {
                Ok(bytes) => {
                    if write_frame(&mut w, TAG_PTY_DATA, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("viewer #{viewer_id} lagged: dropped {n}");
                    continue;
                }
            }
        }
        drop(outbound_session);
    };

    tokio::select! {
        _ = inbound => {},
        _ = outbound => {},
    }

    if let Some(s) = manager.get_by_id_or_name(&cleanup_session_id) {
        s.deregister_viewer(viewer_id);
    }
    cleanup_size_table.remove(viewer_id);
    info!("viewer #{viewer_id} disconnected");
}

async fn wait_for_hello(
    reader: &mut tokio::io::ReadHalf<LocalStream>,
    manager: &Arc<Manager>,
    viewer_id: u64,
) -> Option<(Arc<Session>, HelloPayload)> {
    match read_frame(reader).await {
        Ok((TAG_HELLO, payload)) => {
            let hello = decode_hello(&payload).unwrap_or_default();
            let target = hello.session.as_deref().unwrap_or(DEFAULT_SESSION_NAME);
            match manager.get_by_id_or_name(target) {
                Some(s) => {
                    // Hibernated or Crashed sessions get transparently
                    // resumed — viewer sees a brief blank screen until
                    // claude paints, but pty_out broadcast/ring are
                    // already wired so no extra glue is needed here.
                    // LocallyOwned is deliberately *not* auto-resumed:
                    // the user's local `claude --resume` owns the
                    // transcript and starting a second under broker
                    // would corrupt it. Viewer connects, sees an
                    // empty screen until the user runs `agentmux
                    // adopt <name>` to bring it back.
                    if matches!(s.state(), SessionState::Hibernated | SessionState::Crashed) {
                        info!(
                            "viewer #{viewer_id} attaching {:?} session {} ({}) — auto-resuming",
                            s.state(),
                            s.id,
                            s.name
                        );
                        let to_resume = s.clone();
                        tokio::task::spawn_blocking(move || {
                            if let Err(e) = to_resume.resume() {
                                error!("auto-resume on attach: {e}");
                            }
                        });
                    } else if s.state() == SessionState::LocallyOwned {
                        info!(
                            "viewer #{viewer_id} attaching LocallyOwned session {} ({}) — \
                             not auto-resuming (user's local claude owns the transcript)",
                            s.id, s.name
                        );
                    }
                    Some((s, hello))
                }
                None => {
                    warn!("viewer #{viewer_id} requested unknown session: {target}");
                    None
                }
            }
        }
        Ok((tag, _)) => {
            warn!("viewer #{viewer_id} first frame must be HELLO, got tag={tag:#x}");
            None
        }
        Err(_) => None,
    }
}

async fn handle_control_cmd(
    viewer_id: u64,
    session: &Arc<Session>,
    cmd: &str,
    app_state: &AppState,
) {
    info!("viewer #{viewer_id} CONTROL on {}: {cmd}", session.name);
    match cmd {
        CTRL_INTERRUPT => session.interrupt(),
        CTRL_RESTART => {
            let s = session.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = s.restart() {
                    error!("restart: {e}");
                }
            });
        }
        CTRL_SHUTDOWN => {
            let _ = app_state.shutdown_tx.send(true);
        }
        other => warn!("viewer #{viewer_id} unknown control cmd: {other}"),
    }
}

// --- HTTP handlers -------------------------------------------------------

#[derive(Serialize)]
struct SessionWithStatus {
    #[serde(flatten)]
    info: SessionInfo,
    /// One-line "what is this session doing right now" string. Empty
    /// for sessions that haven't emitted any event yet (broker just
    /// restarted, no activity since).
    current_status: String,
}

async fn http_list(State(s): State<AppState>) -> Json<Vec<SessionWithStatus>> {
    let statuses = s.session_statuses.lock().unwrap().clone();
    Json(
        s.manager
            .list()
            .into_iter()
            .map(|info| {
                let current_status = statuses.get(&info.id).cloned().unwrap_or_default();
                SessionWithStatus {
                    info,
                    current_status,
                }
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    cwd: Option<String>,
    /// Per-session override of `auto_resume`. None falls through to
    /// `Config.auto_resume_default` so the client doesn't have to know
    /// the system policy.
    #[serde(default)]
    auto_resume: Option<bool>,
    /// Adopt path: spawn claude with `--resume <id>` so the new
    /// process picks up an existing transcript. None = brand-new
    /// conversation. Used by `agentmux adopt --resume <id>` after the
    /// user exits a stand-alone `claude` and wants broker to take
    /// over the same conversation.
    #[serde(default)]
    resume_session_id: Option<String>,
}

async fn http_create(
    State(s): State<AppState>,
    Json(body): Json<CreateBody>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let cwd = body
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| s.default_cwd.clone());
    if !cwd.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("cwd does not exist: {cwd:?}"),
        ));
    }
    let auto_resume = body
        .auto_resume
        .unwrap_or(s.config.auto_resume_default);
    let session = s
        .manager
        .create(body.name, cwd, auto_resume, body.resume_session_id)
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;

    // Emit a `session_created` event so the Discord bot (and any
    // other listener) can build a thread / refresh dashboards / etc.
    // Mirrors the event spawn-and-dispatch already broadcasts so
    // both creation paths surface a worker the same way. The bot's
    // handle_session_created falls back to discord.toml's
    // worker_thread_parent when desired_thread_parent is missing
    // here — i.e. the orchestrator's two-step path
    // (POST /sessions → POST /dispatch) gets thread creation for free
    // as long as the global config knob is set.
    let event = serde_json::json!({
        "type": "session_created",
        "session_id": session.id,
        "session_name": session.name,
        "caller_session_id": serde_json::Value::Null,
        "desired_thread_parent": serde_json::Value::Null,
    });
    let _ = s.event_bus.send(event.clone());
    s.events.append(event);

    Ok(Json(session.info()))
}

#[derive(Serialize)]
struct DemoteResponse {
    claude_session_id: Option<String>,
    cwd: String,
    /// True iff claude exited within the `/exit` graceful window.
    /// False means broker had to TerminateProcess — transcript may
    /// be missing the last few lines (very rare; documented).
    graceful: bool,
    /// Ready-to-paste shell command for the user.
    suggested_command: String,
}

/// Demote a broker-owned session. Kills the claude child (graceful
/// `/exit\r` → 2 s wait → TerminateProcess fallback → 1 s wait;
/// returns 500 if claude survives all of that, leaving state intact),
/// flips state to LocallyOwned, returns the recorded
/// `claude_session_id` so the CLI can tell the user exactly which
/// command to run locally.
async fn http_demote(
    Path(key): Path<String>,
    State(s): State<AppState>,
) -> Result<Json<DemoteResponse>, (StatusCode, String)> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or((StatusCode::NOT_FOUND, format!("session not found: {key}")))?;
    let cur = session.state();
    if cur == SessionState::LocallyOwned {
        return Err((
            StatusCode::CONFLICT,
            "session is already locally-owned".to_string(),
        ));
    }
    if cur == SessionState::Hibernated {
        // Nothing to kill — but the state is still "broker owns this
        // record". Treat demote as "transition to LocallyOwned without
        // killing", since the previous claude already exited.
        // We deliberately use a separate code path rather than calling
        // demote() because /exit on an already-dead PTY is a no-op
        // and the wait loops would burn 3 seconds for no benefit.
        // Doing this inline keeps the logic local.
    }

    // Safe to demote. spawn_blocking because demote() does sync sleeps.
    let manager = s.manager.clone();
    let key2 = key.clone();
    let outcome = tokio::task::spawn_blocking(move || manager.demote(&key2))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let suggested = match (&outcome.claude_session_id, &outcome.cwd) {
        (Some(id), cwd) => format!(
            "cd \"{}\" ; claude --resume {}",
            cwd.replace('\\', "\\\\"),
            id
        ),
        (None, cwd) => format!(
            "cd \"{}\" ; claude   # no claude_session_id recorded — start fresh",
            cwd.replace('\\', "\\\\")
        ),
    };

    Ok(Json(DemoteResponse {
        claude_session_id: outcome.claude_session_id,
        cwd: outcome.cwd,
        graceful: outcome.graceful,
        suggested_command: suggested,
    }))
}

/// Re-adopt a LocallyOwned session: spawn claude under broker with
/// `--resume <stored-id>`. The user is responsible for having exited
/// their local `claude --resume` first; the broker can't detect a
/// third-party process holding the same conversation.
async fn http_adopt(
    Path(key): Path<String>,
    State(s): State<AppState>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let manager = s.manager.clone();
    let key2 = key.clone();
    let session = tokio::task::spawn_blocking(move || manager.re_adopt(&key2))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(session.info()))
}

/// Returns 409 + a stable error code in the body when a write
/// operation can't proceed because the session is locally-owned.
/// Discord/CLI parse the JSON to decide UX. Bare strings would force
/// substring matching on the next round of changes.
fn locally_owned_409(name: &str) -> (StatusCode, String) {
    (
        StatusCode::CONFLICT,
        serde_json::json!({
            "error": "locally_owned",
            "session": name,
            "message": format!(
                "session '{name}' is locally-owned; broker has no claude to act on. \
                 Run `agentmux adopt {name}` to bring it back."
            ),
        })
        .to_string(),
    )
}

async fn http_get(
    Path(key): Path<String>,
    State(s): State<AppState>,
) -> Result<Json<SessionInfo>, StatusCode> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(session.info()))
}

#[derive(Serialize)]
struct StateView {
    id: String,
    name: String,
    state: SessionState,
    local_viewer_attached: bool,
    attached_clients: Vec<ClientInfo>,
    claude_session_id: Option<String>,
    /// Seconds since the session last saw user activity (input,
    /// attach, resume). Diagnostic for the idle-hibernate timer.
    idle_secs: u64,
}

async fn http_state(
    Path(key): Path<String>,
    State(s): State<AppState>,
) -> Result<Json<StateView>, StatusCode> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or(StatusCode::NOT_FOUND)?;
    let attached = session.attached_clients();
    // "Local" here means "the user is actively watching this session
    // live" — covers both `claude-attach` (`terminal`) and the
    // browser-based web viewer (`web`). Discord-bot subscribers
    // (`discord`) explicitly do *not* count: they're a remote relay,
    // and silencing hooks while a Discord bot is connected would
    // defeat the whole IM bridge.
    let local_viewer_attached = attached
        .iter()
        .any(|c| c.client_kind == "terminal" || c.client_kind == "web");
    Ok(Json(StateView {
        id: session.id.clone(),
        name: session.name.clone(),
        state: session.state(),
        local_viewer_attached,
        attached_clients: attached,
        claude_session_id: session.claude_session_id(),
        idle_secs: session.last_activity_age().as_secs(),
    }))
}

async fn http_hibernate(
    Path(key): Path<String>,
    State(s): State<AppState>,
) -> Result<&'static str, (StatusCode, String)> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or((StatusCode::NOT_FOUND, "session not found".to_string()))?;
    if session.state() == SessionState::LocallyOwned {
        return Err(locally_owned_409(&session.name));
    }
    let to_hibernate = session.clone();
    tokio::task::spawn_blocking(move || to_hibernate.hibernate())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?;
    Ok("ok")
}

async fn http_delete(
    Path(key): Path<String>,
    State(s): State<AppState>,
) -> Result<&'static str, (StatusCode, String)> {
    s.manager
        .remove(&key)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    Ok("ok")
}

async fn http_interrupt(
    Path(key): Path<String>,
    State(s): State<AppState>,
) -> Result<&'static str, (StatusCode, String)> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or((StatusCode::NOT_FOUND, "session not found".to_string()))?;
    if session.state() == SessionState::LocallyOwned {
        return Err(locally_owned_409(&session.name));
    }
    session.interrupt();
    Ok("ok")
}

async fn http_restart(
    Path(key): Path<String>,
    State(s): State<AppState>,
) -> Result<&'static str, (StatusCode, String)> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or((StatusCode::NOT_FOUND, "session not found".to_string()))?;
    if session.state() == SessionState::LocallyOwned {
        return Err(locally_owned_409(&session.name));
    }
    let res = tokio::task::spawn_blocking(move || session.restart())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    res.map_err(|e| {
        error!("http restart: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    Ok("ok")
}

#[derive(Deserialize)]
struct PersistBody {
    auto_resume: bool,
}

/// Toggle the persistence (auto_resume) flag on an existing session.
/// `true` = restored on broker boot (default behaviour pre-change);
/// `false` = forgotten on next boot. Per-session — does not affect
/// config-level `auto_resume_default`.
async fn http_set_persist(
    Path(key): Path<String>,
    State(s): State<AppState>,
    Json(body): Json<PersistBody>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    s.manager
        .set_auto_resume(&key, body.auto_resume)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or((StatusCode::NOT_FOUND, "session not found".to_string()))?;
    Ok(Json(session.info()))
}

#[derive(Deserialize)]
struct ToolRequestBody {
    /// Broker session id (`AGENT_SESSION_ID` from the hook env). Used
    /// purely to annotate the broadcast event so bots route the
    /// approval prompt to the right channel.
    session_id: String,
    tool_name: String,
    /// Free-form. Pass-through to the bot so it can render whatever
    /// the user needs to make a decision (Bash command, file path, …).
    tool_input: serde_json::Value,
    /// Hook-side timeout in seconds. Caller can override; otherwise
    /// we apply `PRETOOL_DEFAULT_TIMEOUT`.
    #[serde(default)]
    timeout_secs: u64,
}

/// Long-poll endpoint. The PreToolUse hook POSTs here with a tool
/// request, the broker registers a oneshot, broadcasts an event so
/// the Discord bot (or any other approval surface) can prompt a
/// human, and the response body lands once `/tool-decision/:id`
/// fires the oneshot. On timeout the request is implicitly denied.
async fn http_tool_request(
    State(s): State<AppState>,
    Json(body): Json<ToolRequestBody>,
) -> Result<Json<ToolDecision>, (StatusCode, String)> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<ToolDecision>();
    s.pending_decisions
        .lock()
        .unwrap()
        .insert(request_id.clone(), tx);

    // Lookup human-friendly session name + check whether a local viewer
    // is currently watching. The latter rides on the broadcast event so
    // remote-IM subscribers (Discord) can self-suppress when the user is
    // already at a viewer; tray/web subscribers ignore the field and keep
    // showing approval UI as usual.
    let (session_name, local_viewer_attached) = s
        .manager
        .get_by_id_or_name(&body.session_id)
        .map(|sess| {
            let attached = sess.attached_clients();
            let local = attached
                .iter()
                .any(|c| c.client_kind == "terminal" || c.client_kind == "web");
            (sess.name.clone(), local)
        })
        .unwrap_or_else(|| (body.session_id.clone(), false));

    info!(
        "/tool-request id={} session={} tool={} local_viewer={}",
        request_id, session_name, body.tool_name, local_viewer_attached
    );

    // Broadcast so subscribers (Discord bot etc.) can prompt the user.
    let event = serde_json::json!({
        "type": "tool_request",
        "request_id": request_id,
        "session_id": body.session_id,
        "session_name": session_name,
        "tool_name": body.tool_name,
        "tool_input": body.tool_input,
        "local_viewer_attached": local_viewer_attached,
    });
    let _ = s.event_bus.send(event.clone());
    s.events.append(event);

    let timeout = if body.timeout_secs == 0 {
        PRETOOL_DEFAULT_TIMEOUT
    } else {
        Duration::from_secs(body.timeout_secs)
    };

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(decision)) => {
            info!(
                "/tool-request id={} resolved: allow={} reason={:?}",
                request_id, decision.allow, decision.reason
            );
            Ok(Json(decision))
        }
        Ok(Err(_)) => {
            // Sender dropped without sending — should be unreachable
            // (we own both ends), but treat as deny just in case.
            s.pending_decisions.lock().unwrap().remove(&request_id);
            warn!("/tool-request id={} sender dropped", request_id);
            Ok(Json(ToolDecision {
                allow: false,
                reason: "broker decision channel dropped".into(),
            }))
        }
        Err(_) => {
            // Timeout. Remove the entry so the broker doesn't leak a
            // sender if /tool-decision arrives later.
            s.pending_decisions.lock().unwrap().remove(&request_id);
            info!("/tool-request id={} timed out, denying", request_id);
            Ok(Json(ToolDecision {
                allow: false,
                reason: format!("no human decision within {}s", timeout.as_secs()),
            }))
        }
    }
}

async fn http_tool_decision(
    Path(request_id): Path<String>,
    State(s): State<AppState>,
    Json(decision): Json<ToolDecision>,
) -> Result<&'static str, (StatusCode, String)> {
    let sender = s.pending_decisions.lock().unwrap().remove(&request_id);
    let Some(sender) = sender else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no pending decision: {request_id}"),
        ));
    };
    let _ = sender.send(decision);
    Ok("ok")
}

async fn http_shutdown(State(s): State<AppState>) -> &'static str {
    let _ = s.shutdown_tx.send(true);
    "ok"
}

/// Whole-stack restart. Triggered from `agentmux restart` (CLI),
/// Discord `/reload`, or tray "Restart agentmux".
///
/// Mechanism: spawn a *detached* PowerShell respawner that sleeps a
/// few seconds (waiting for our PID file release + listener teardown)
/// then re-invokes the launcher script in `restart` mode, then signal
/// our own shutdown. Because the respawner is detached, it survives our
/// exit. Because broker is also stopped *and* started by the launcher,
/// every process reloads its config from disk — fixing the recurring
/// "I edited config.toml but nothing changed" papercut.
///
/// Returns 503 if `AGENTMUX_LAUNCHER` isn't set (broker started outside
/// the wrapper, e.g. on Linux where the wrapper doesn't exist yet).
async fn http_restart_agentmux(
    State(s): State<AppState>,
) -> Result<&'static str, (StatusCode, String)> {
    let launcher = match std::env::var("AGENTMUX_LAUNCHER") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "AGENTMUX_LAUNCHER not set — broker was started outside the wrapper script. \
                 Restart manually with: .\\agentmux restart"
                    .into(),
            ));
        }
    };

    if let Err(e) = spawn_respawner(&launcher) {
        warn!("/restart-agentmux: respawner spawn failed: {e}");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("respawner spawn failed: {e}"),
        ));
    }

    info!(
        "/restart-agentmux: respawner detached (launcher={}); shutting down",
        launcher
    );
    let _ = s.shutdown_tx.send(true);
    Ok("ok")
}

/// Spawn a detached helper that waits for broker to exit, then runs
/// `<launcher> restart`. Returns immediately so the HTTP handler can
/// reply 200 before broker tears down.
///
/// Diagnostics: every invocation transcribes its output (Start-Sleep
/// completion, agentmux restart's full stdout/stderr, any error) to
/// `%LOCALAPPDATA%\agentmux\respawner.log`. Without this, a failed
/// respawn was completely silent — broker died, the launcher's child
/// PowerShell silently exited, and the user saw no diagnostics
/// anywhere. The log is overwritten each restart (`-Force`) so it
/// stays small.
///
/// Robustness:
///   * `-ExecutionPolicy Bypass` — the script invocation
///     `& '<launcher>' restart` runs a .ps1 file, which a Restricted
///     or AllSigned policy would block. Bypass forces it through. We
///     could not just use `-File` because we also need the leading
///     `Start-Sleep` to give broker time to release its listener.
#[cfg(windows)]
fn spawn_respawner(launcher: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS so the helper isn't
    // killed when broker exits.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let log_path = shared::config::local_appdata_dir().join("respawner.log");
    let log_path_str = log_path.to_string_lossy().replace('\'', "''");
    let launcher_escaped = launcher.replace('\'', "''");
    let script = format!(
        "try {{ Start-Transcript -Path '{log_path_str}' -Force | Out-Null }} catch {{ }}; \
         Write-Host (\"respawner: launcher=$([char]39){launcher_escaped}$([char]39) ts=$(Get-Date -Format o)\"); \
         Start-Sleep -Seconds 2; \
         try {{ & '{launcher_escaped}' restart }} catch {{ Write-Host \"respawner: ERROR $_\" }}; \
         try {{ Stop-Transcript | Out-Null }} catch {{ }}"
    );
    std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_respawner(launcher: &str) -> std::io::Result<()> {
    // POSIX: nohup + setsid + bash via the launcher path. The launcher
    // is currently Windows-only so this branch is rarely hit; we still
    // accept it so a future agentmux.sh just works.
    use std::process::{Command, Stdio};
    Command::new("sh")
        .arg("-c")
        .arg(format!("(sleep 2 && \"{launcher}\" restart) &"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

async fn http_event(
    State(s): State<AppState>,
    Json(mut event): Json<serde_json::Value>,
) -> &'static str {
    let kind = event
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();

    // Capture claude's own session id from the transcript path the
    // first time we see it so future restart/resume/demote can pass
    // --resume. Runs *before* the kind-specific filtering below so
    // the dedicated "session_seen" pings hooks emit while a local
    // viewer is attached still teach broker the id, even though the
    // hooks bail out of fanning the user-visible event itself.
    if let Some(transcript_path) = event.get("transcript_path").and_then(|v| v.as_str()) {
        if let Some(broker_session_id) = event.get("session_id").and_then(|v| v.as_str()) {
            if let Some(session) = s.manager.get_by_id_or_name(broker_session_id) {
                if let Some(claude_id) = extract_claude_session_id(transcript_path) {
                    if session.set_claude_session_id_if_changed(claude_id.clone()) {
                        info!(
                            "session {} captured claude_session_id={}",
                            session.id, claude_id
                        );
                        if let Err(e) = s.manager.save() {
                            warn!("save after claude_session_id update: {e}");
                        }
                    }
                }
            }
        }
    }

    // session_seen is an internal capture-only nudge — hooks emit it
    // unconditionally so broker learns claude_session_id even when
    // the hook is about to bail-on-local-viewer for the user-facing
    // event. Don't broadcast to WS subscribers (Discord/tray would
    // see noisy duplicates) and don't persist to events.jsonl.
    if kind == "session_seen" {
        return "ok";
    }

    // Annotate session_name so WS subscribers don't need a second
    // round-trip to /sessions/<id> to resolve the human label.
    if let Some(sid) = event.get("session_id").and_then(|v| v.as_str()) {
        if let Some(sess) = s.manager.get_by_id_or_name(sid) {
            if let Some(obj) = event.as_object_mut() {
                obj.entry("session_name")
                    .or_insert_with(|| serde_json::json!(sess.name));
            }
        }
    }

    // Update per-session current_status from this event, broadcasting
    // a `session_status_changed` event when it actually changes.
    // Dashboard subscribers (Discord embed, tray submenu) listen for
    // the change event so they only re-render on transitions.
    if let Some(sid) = event.get("session_id").and_then(|v| v.as_str()) {
        if let Some(status) = derive_status(&kind, &event) {
            let changed = {
                let mut g = s.session_statuses.lock().unwrap();
                if g.get(sid) == Some(&status) {
                    false
                } else {
                    g.insert(sid.to_string(), status.clone());
                    true
                }
            };
            if changed {
                let session_name = s
                    .manager
                    .get_by_id_or_name(sid)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| sid.to_string());
                let evt = serde_json::json!({
                    "type": "session_status_changed",
                    "session_id": sid,
                    "session_name": session_name,
                    "current_status": status,
                });
                let _ = s.event_bus.send(evt);
            }
        }
    }

    // Cross-session callback delivery. If this assistant_message
    // resolves an outstanding /dispatch (target had a registered
    // callback), pop the front-of-queue entry and inject a
    // `[SYSTEM: task-complete]` block into the original caller. Done
    // before the broadcast so subscribers see both events in order.
    // The actual delivery is fire-and-forget so a slow caller (e.g.
    // hibernated, needs auto-resume) doesn't stall the /event handler.
    if kind == "assistant_message" {
        if let Some(target_id) = event.get("session_id").and_then(|v| v.as_str()) {
            if let Some(cb) = s.orchestrator.pop_for_target(target_id) {
                let target_name = s
                    .manager
                    .get_by_id_or_name(target_id)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| target_id.to_string());
                let body_text = event
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let synthesized =
                    orchestrator::format_task_complete(&cb, &target_name, &body_text);
                let task_id_log = cb.task_id.clone();
                let caller_id = cb.caller_session_id.clone();
                let app_clone = s.clone();
                tokio::spawn(async move {
                    let caller = match app_clone.manager.get_by_id_or_name(&caller_id) {
                        Some(c) => c,
                        None => {
                            warn!(
                                "callback task_id={task_id_log}: caller {caller_id} \
                                 no longer exists, dropping"
                            );
                            return;
                        }
                    };
                    if let Err(e) = deliver_to_session(&caller, &synthesized).await {
                        warn!(
                            "callback task_id={task_id_log} → caller={}: {e}",
                            caller.name
                        );
                    }
                });
                info!(
                    "callback queued task_id={} target={} → caller={}",
                    cb.task_id, target_name, cb.caller_session_id
                );
            }
        }
    }

    // Tee to WS subscribers before persisting — broadcast::send returns
    // Err only when there are zero receivers, which is fine.
    let _ = s.event_bus.send(event.clone());

    s.events.append(event);
    info!("event recorded: {kind}");
    "ok"
}

/// Diagnostic: returns the session's ring buffer snapshot as raw bytes.
/// Pipe through `od -c` / `xxd` to see exactly what claude is rendering
/// — useful when tracking down "input goes in but doesn't submit"
/// style bugs without firing up a viewer.
async fn http_ring_snapshot(
    Path(key): Path<String>,
    State(s): State<AppState>,
) -> Result<Vec<u8>, (StatusCode, String)> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or((StatusCode::NOT_FOUND, format!("session not found: {key}")))?;
    let snap = session.pty_out.ring.lock().unwrap().snapshot();
    Ok(snap)
}

/// Map a hook event into a one-line "what is this session doing right
/// now" string, for the dashboard panel. Returns None for events that
/// don't change observable status (e.g. session_seen, tool_request —
/// the user already sees those via other surfaces). Kept intentionally
/// terse: status text shows up in tooltips and embeds where width is
/// scarce.
fn derive_status(kind: &str, event: &serde_json::Value) -> Option<String> {
    let truncate = |s: &str, max: usize| -> String {
        if s.chars().count() <= max {
            s.to_string()
        } else {
            let mut out: String = s.chars().take(max - 1).collect();
            out.push('…');
            out
        }
    };
    let short_path = |p: &str| -> String {
        let normalized: String = p
            .chars()
            .map(|c| if c == '\\' { '/' } else { c })
            .collect();
        let parts: Vec<&str> = normalized.split('/').collect();
        if parts.len() <= 3 {
            return truncate(&normalized, 60);
        }
        let tail = parts[parts.len().saturating_sub(3)..].join("/");
        truncate(&format!(".../{tail}"), 60)
    };

    let tool_input = event.get("tool_input").cloned().unwrap_or_default();
    let s_field = |k: &str| -> String {
        tool_input
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    match kind {
        "tool_progress" => {
            let tool = event.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
            Some(match tool {
                "Read" => format!("reading {}", short_path(&s_field("file_path"))),
                "Edit" | "MultiEdit" => format!("editing {}", short_path(&s_field("file_path"))),
                "Write" => format!("writing {}", short_path(&s_field("file_path"))),
                "Bash" => format!("$ {}", truncate(&s_field("command"), 50)),
                "Glob" => format!("glob {}", truncate(&s_field("pattern"), 40)),
                "Grep" => format!("grep {}", truncate(&s_field("pattern"), 40)),
                "WebFetch" => format!("fetch {}", truncate(&s_field("url"), 50)),
                "WebSearch" => format!("search {}", truncate(&s_field("query"), 40)),
                "Task" => "delegating to subagent".to_string(),
                "TodoWrite" => "updating todos".to_string(),
                other if other.starts_with("mcp__") => {
                    let mut parts = other.splitn(3, "__");
                    let _ = parts.next();
                    let server = parts.next().unwrap_or("?");
                    let t = parts.next().unwrap_or("?");
                    format!("mcp {server}.{t}")
                }
                other if !other.is_empty() => format!("running {other}"),
                _ => "running tool".to_string(),
            })
        }
        "assistant_message" => Some("idle".to_string()),
        "notification" => Some("waiting on user".to_string()),
        "tool_request" => Some("awaiting tool approval".to_string()),
        "session_created" => Some("starting".to_string()),
        _ => None,
    }
}

/// Pulls the UUID-shaped basename out of a Claude Code transcript
/// path (`.../<uuid>.jsonl`). Returns None if the basename isn't a
/// valid UUID — that filters out unrelated files claude might write.
fn extract_claude_session_id(transcript_path: &str) -> Option<String> {
    let stem = StdPath::new(transcript_path).file_stem()?.to_str()?;
    uuid::Uuid::parse_str(stem).ok().map(|_| stem.to_string())
}

#[derive(Deserialize)]
struct InputBody {
    text: String,
    /// When true (default) a `\r` byte is appended so claude treats the
    /// text as a submitted prompt. Set false if the caller is feeding
    /// keystrokes mid-line and wants to control the Enter explicitly.
    #[serde(default = "default_append_enter")]
    append_enter: bool,
}

fn default_append_enter() -> bool {
    true
}

/// Inject text into a session's PTY stdin — the IM-side counterpart of
/// the named-pipe input path used by claude-attach. Hibernated/Crashed
/// sessions are auto-resumed first so a stale Discord channel binding
/// doesn't silently swallow input on a session that fell asleep.
async fn http_input(
    Path(key): Path<String>,
    State(s): State<AppState>,
    Json(body): Json<InputBody>,
) -> Result<&'static str, (StatusCode, String)> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or((StatusCode::NOT_FOUND, format!("session not found: {key}")))?;

    info!(
        "/input session={} text_chars={} append_enter={} state={:?}",
        session.name,
        body.text.chars().count(),
        body.append_enter,
        session.state()
    );

    // Locally-owned sessions have no broker-managed claude to write
    // into. Auto-resuming would race the user's local
    // `claude --resume <id>` and corrupt the transcript — exactly
    // what demote exists to prevent. Refuse with 409 + a structured
    // error code so Discord/CLI can render guidance instead of
    // silently dropping the message.
    if session.state() == SessionState::LocallyOwned {
        return Err(locally_owned_409(&session.name));
    }

    if matches!(
        session.state(),
        SessionState::Hibernated | SessionState::Crashed
    ) {
        let to_resume = session.clone();
        tokio::task::spawn_blocking(move || to_resume.resume())
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")))?
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("resume: {e}")))?;
        // resume() arms a fresh boot probe; write_user_burst's
        // await_ready picks that up before the first byte goes in.
    }

    let text_bytes = body.text.into_bytes();
    write_user_burst(&session, &text_bytes, body.append_enter).await;
    info!(
        "/input wrote text_bytes={} append_enter={}",
        text_bytes.len(),
        body.append_enter
    );
    Ok("ok")
}

/// Write a "user input burst" to a session's PTY. Shared between the
/// HTTP `/input` path, the orchestrator's dispatch delivery, and the
/// callback inject path so they all behave identically.
///
/// Two independent timing hazards motivate two independent gates:
///
/// 1. **Cold boot.** `await_ready` — claude's TUI may still be drawing
///    startup banners and not yet have taken over stdin. Writing into
///    that window leaves the prompt either eaten by banner output or
///    stranded in pre-input limbo. Hits primarily on spawn-and-dispatch
///    (caller created the session milliseconds ago) and on the first
///    deliver after a resume. Already-ready sessions pay a single
///    cheap atomic load.
///
/// 2. **Paste-burst grouping.** `wait_for_input_echo` — claude code's
///    TUI groups bytes arriving in the same read() (or within a short
///    timing window) into one paste. If that burst ends in `\r` while
///    containing embedded `\n`, the trailing `\r` is treated as a soft
///    newline rather than Enter, and input never submits. The fix is
///    to ensure `\r` lands as a temporally-separate event after the
///    text has finished echoing in the input box.
///
/// Earlier iterations used a fixed 30 ms sleep for #2 (broke on long
/// multi-line dispatches) and had no #1 gate at all (broke on the
/// spawn-and-dispatch path). Both are now adaptive: poll the PTY ring
/// for a quiet stretch, with a fallback timeout so a stuck TUI can't
/// strand callers indefinitely.
async fn write_user_burst(session: &Arc<Session>, text_bytes: &[u8], append_enter: bool) {
    // First gate: claude TUI must have finished booting (banners drawn,
    // input box has taken over stdin). Cheap for already-ready sessions
    // — watch::borrow returns immediately when boot_ready is true.
    // Critical for the spawn-and-dispatch path where deliver fires
    // milliseconds after the claude child was spawned and the TUI is
    // still rendering startup banners; without this the prompt would
    // race the input box and end up either eaten by banner output or
    // sitting unsubmitted with the trailing \r consumed by claude's
    // paste-burst grouping.
    session.await_ready().await;

    let has_text = !text_bytes.is_empty();
    if has_text {
        const CHUNK_SIZE: usize = 800;
        const CHUNK_GAP: Duration = Duration::from_millis(150);
        if text_bytes.len() <= CHUNK_SIZE {
            session.write_to_pty(text_bytes);
        } else {
            let mut start = 0;
            while start < text_bytes.len() {
                let end = (start + CHUNK_SIZE).min(text_bytes.len());
                session.write_to_pty(&text_bytes[start..end]);
                start = end;
                if start < text_bytes.len() {
                    tokio::time::sleep(CHUNK_GAP).await;
                }
            }
        }
    }
    if append_enter {
        if has_text {
            wait_for_input_echo(session).await;
        }
        session.write_to_pty(b"\r");
    }
}

/// Block until claude's TUI has finished echoing the just-written
/// input (output ring has been quiet for `STABLE`). The empty/
/// idle-but-cursor-blinking baseline emits no bytes, so a stretch of
/// no-growth = TUI is done rendering the input. `MAX_WAIT` bounds the
/// worst case so a stuck TUI can't strand a callback delivery
/// indefinitely.
async fn wait_for_input_echo(session: &Session) {
    use std::time::Instant;
    const POLL: Duration = Duration::from_millis(20);
    const STABLE: Duration = Duration::from_millis(80);
    const MAX_WAIT: Duration = Duration::from_millis(2000);

    let start = Instant::now();
    let mut last_size = session.pty_out.ring.lock().unwrap().len();
    let mut last_change = start;

    loop {
        tokio::time::sleep(POLL).await;
        let now = Instant::now();
        let size = session.pty_out.ring.lock().unwrap().len();
        if size != last_size {
            last_size = size;
            last_change = now;
        }
        let stable_for = now.duration_since(last_change);
        let elapsed = now.duration_since(start);
        if stable_for >= STABLE {
            return;
        }
        if elapsed >= MAX_WAIT {
            return;
        }
    }
}

#[derive(Deserialize)]
struct DispatchBody {
    /// Target session — runs the prompt as a new turn. Accepts session
    /// id or name.
    to: String,
    /// What to ask the worker to do.
    prompt: String,
    /// Caller-chosen label, echoed verbatim in the callback so the
    /// caller can correlate the result with the dispatch.
    #[serde(default)]
    tag: String,
    /// Override the broker-default deadline. 0 / missing = use
    /// `config.dispatch_timeout_secs`.
    #[serde(default)]
    timeout_secs: u64,
}

#[derive(Serialize)]
struct DispatchResponse {
    task_id: String,
    /// Resolved id of the target (caller may have used name).
    target_session_id: String,
}

/// Cross-session task dispatch. Caller registers a callback against
/// `to`, broker sends `prompt` to `to`, returns `task_id` immediately.
/// When `to`'s next assistant_message lands, broker injects a
/// `[SYSTEM: task-complete]` block back into the caller's input.
async fn http_dispatch(
    Path(caller_key): Path<String>,
    State(s): State<AppState>,
    Json(body): Json<DispatchBody>,
) -> Result<Json<DispatchResponse>, (StatusCode, String)> {
    let caller = s.manager.get_by_id_or_name(&caller_key).ok_or((
        StatusCode::NOT_FOUND,
        format!("caller session not found: {caller_key}"),
    ))?;

    let target = s.manager.get_by_id_or_name(&body.to).ok_or((
        StatusCode::NOT_FOUND,
        format!("target session not found: {}", body.to),
    ))?;

    if caller.id == target.id {
        return Err((
            StatusCode::BAD_REQUEST,
            "caller and target must be different sessions".into(),
        ));
    }

    if target.state() == SessionState::LocallyOwned {
        return Err(locally_owned_409(&target.name));
    }

    let cap = s.config.max_active_dispatches_per_session;
    if cap > 0 {
        let active = s.orchestrator.count_for_caller(&caller.id);
        if active as u32 >= cap {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "caller `{}` has {active} active dispatch(es); cap is {cap}. \
                     wait for some to complete or kill stuck workers",
                    caller.name
                ),
            ));
        }
    }

    let task_id = uuid::Uuid::new_v4().to_string();
    let timeout_secs = if body.timeout_secs == 0 {
        s.config.dispatch_timeout_secs
    } else {
        body.timeout_secs
    };
    let cb = orchestrator::PendingCallback {
        task_id: task_id.clone(),
        caller_session_id: caller.id.clone(),
        target_session_id: target.id.clone(),
        tag: body.tag.clone(),
        original_prompt: body.prompt.clone(),
        dispatched_at_unix_ms: orchestrator::now_unix_ms(),
        timeout_secs,
    };
    s.orchestrator.push(cb);

    info!(
        "/dispatch task_id={} caller={} target={} tag={} prompt_chars={} timeout_secs={}",
        task_id,
        caller.name,
        target.name,
        body.tag,
        body.prompt.chars().count(),
        timeout_secs
    );

    // Deliver the prompt in the background — hibernate auto-resume can
    // take 5-10s and the HTTP caller (the orchestrator's curl) shouldn't
    // wait. The callback is already registered, so the target's reply
    // will route back regardless of how late the prompt actually lands.
    let target_clone = target.clone();
    let prompt = body.prompt;
    let target_name = target.name.clone();
    let tid_for_log = task_id.clone();
    tokio::spawn(async move {
        if let Err(e) = deliver_to_session(&target_clone, &prompt).await {
            warn!("/dispatch task_id={tid_for_log} deliver to {target_name}: {e}");
        }
    });

    Ok(Json(DispatchResponse {
        task_id,
        target_session_id: target.id.clone(),
    }))
}

#[derive(Deserialize)]
struct SpawnDispatchBody {
    /// Optional explicit name. Missing/empty → broker auto-picks
    /// `w1`, `w2`, ... (lowest unused index).
    #[serde(default)]
    name: String,
    /// cwd for the new session. Empty → broker default_cwd.
    #[serde(default)]
    cwd: String,
    /// What to ask the new worker to do.
    prompt: String,
    /// Caller-chosen label, echoed in the callback.
    #[serde(default)]
    tag: String,
    /// Per-task deadline override; 0 = use broker default.
    #[serde(default)]
    timeout_secs: u64,
    /// Persist the new session across broker restarts? None → broker
    /// default policy. Workers spawned for one-shot tasks usually want
    /// `Some(false)` (ephemeral) so they don't accumulate.
    #[serde(default)]
    auto_resume: Option<bool>,
    /// Optional Discord-side hint: when set, the bot will create a
    /// thread under this channel id and bind it to the new session.
    /// Stored on the session as metadata; broker itself doesn't act on
    /// it. Phase 2 of the orchestrator rollout.
    #[serde(default)]
    desired_thread_parent: Option<u64>,
}

#[derive(Serialize)]
struct SpawnDispatchResponse {
    task_id: String,
    target_session_id: String,
    target_session_name: String,
}

/// Atomic spawn-then-dispatch: convenience endpoint so the orchestrator
/// doesn't have to do create + dispatch + worry about the gap. Useful
/// when the orchestrator decides "no existing session matches this
/// task; spin up a fresh worker just for it."
async fn http_spawn_and_dispatch(
    Path(caller_key): Path<String>,
    State(s): State<AppState>,
    Json(body): Json<SpawnDispatchBody>,
) -> Result<Json<SpawnDispatchResponse>, (StatusCode, String)> {
    let caller = s.manager.get_by_id_or_name(&caller_key).ok_or((
        StatusCode::NOT_FOUND,
        format!("caller session not found: {caller_key}"),
    ))?;

    let cap = s.config.max_active_dispatches_per_session;
    if cap > 0 {
        let active = s.orchestrator.count_for_caller(&caller.id);
        if active as u32 >= cap {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "caller `{}` has {active} active dispatch(es); cap is {cap}",
                    caller.name
                ),
            ));
        }
    }

    let cwd = if body.cwd.is_empty() {
        s.default_cwd.clone()
    } else {
        PathBuf::from(&body.cwd)
    };
    if !cwd.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("cwd does not exist: {cwd:?}"),
        ));
    }

    let auto_resume = body.auto_resume.unwrap_or(s.config.auto_resume_default);

    let name = if body.name.is_empty() {
        auto_worker_name(&s.manager.list())
    } else {
        body.name
    };

    let target = s
        .manager
        .create(name.clone(), cwd, auto_resume, None)
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;

    if caller.id == target.id {
        return Err((
            StatusCode::BAD_REQUEST,
            "spawn-and-dispatch can't target the caller (same session)".into(),
        ));
    }

    let task_id = uuid::Uuid::new_v4().to_string();
    let timeout_secs = if body.timeout_secs == 0 {
        s.config.dispatch_timeout_secs
    } else {
        body.timeout_secs
    };
    let cb = orchestrator::PendingCallback {
        task_id: task_id.clone(),
        caller_session_id: caller.id.clone(),
        target_session_id: target.id.clone(),
        tag: body.tag.clone(),
        original_prompt: body.prompt.clone(),
        dispatched_at_unix_ms: orchestrator::now_unix_ms(),
        timeout_secs,
    };
    s.orchestrator.push(cb);

    info!(
        "/spawn-and-dispatch task_id={} caller={} new_target={} tag={} thread_parent={:?}",
        task_id, caller.name, target.name, body.tag, body.desired_thread_parent
    );

    // Emit a session_created event so the Discord bot (or any other
    // listener) can act on `desired_thread_parent` and create a thread
    // for the new worker. Broker itself doesn't talk to Discord.
    let event = serde_json::json!({
        "type": "session_created",
        "session_id": target.id,
        "session_name": target.name,
        "caller_session_id": caller.id,
        "desired_thread_parent": body.desired_thread_parent,
    });
    let _ = s.event_bus.send(event.clone());
    s.events.append(event);

    let target_clone = target.clone();
    let prompt = body.prompt;
    let target_name = target.name.clone();
    let tid_for_log = task_id.clone();
    tokio::spawn(async move {
        if let Err(e) = deliver_to_session(&target_clone, &prompt).await {
            warn!("/spawn-and-dispatch task_id={tid_for_log} deliver to {target_name}: {e}");
        }
    });

    Ok(Json(SpawnDispatchResponse {
        task_id,
        target_session_id: target.id.clone(),
        target_session_name: target.name.clone(),
    }))
}

/// Embedded at compile time so a release zip doesn't need the docs
/// folder at runtime.
const ORCHESTRATOR_PROMPT: &str = include_str!("../../../docs/orchestrator-prompt.md");

/// Wrap the orchestrator-prompt.md content in a `[SYSTEM: ...]` envelope
/// matching the grammar workers see for callbacks. This way the main
/// session's first incoming "message" is unambiguously a broker-injected
/// system instruction, not a user turn.
fn format_orchestrator_bootstrap() -> String {
    format!(
        "[SYSTEM: orchestrator-bootstrap]\n\
         The following are your role and instructions for this session. They take \
         precedence over default claude behaviour. User messages from now on arrive \
         via the agentmux Discord bridge — they will look like ordinary user input.\n\
         \n\
         ---\n\
         \n\
         {ORCHESTRATOR_PROMPT}\n\
         \n\
         ---\n\
         [/SYSTEM]"
    )
}

/// Pick the lowest unused `wN` name that isn't already a session.
fn auto_worker_name(existing: &[SessionInfo]) -> String {
    let used: std::collections::HashSet<&str> = existing.iter().map(|s| s.name.as_str()).collect();
    for i in 1..u32::MAX {
        let candidate = format!("w{i}");
        if !used.contains(candidate.as_str()) {
            return candidate;
        }
    }
    "w_overflow".to_string()
}

/// Resume the session if hibernated/crashed, then write `text` followed
/// by Enter. The async equivalent of "as if the user typed `text` and
/// hit Enter in claude's TUI." Used by `/dispatch` to deliver prompts to
/// targets and by the orchestrator's callback path to inject
/// `[SYSTEM: ...]` blocks back to the caller. Refuses LocallyOwned
/// sessions since broker has no claude to write into.
async fn deliver_to_session(session: &Arc<Session>, text: &str) -> Result<()> {
    if session.state() == SessionState::LocallyOwned {
        anyhow::bail!("session `{}` is locally-owned", session.name);
    }
    if matches!(
        session.state(),
        SessionState::Hibernated | SessionState::Crashed
    ) {
        let to_resume = session.clone();
        tokio::task::spawn_blocking(move || to_resume.resume())
            .await
            .context("resume task join")?
            .context("resume")?;
        // resume() arms a fresh boot probe; write_user_burst's
        // await_ready picks that up before the first byte goes in.
    }
    write_user_burst(session, text.as_bytes(), true).await;
    Ok(())
}

/// Upgrade the connection to a WebSocket and stream every annotated
/// hook event published on `event_bus` to the subscriber as a JSON line.
/// Inbound frames are currently ignored (subscription filters can be
/// added when a second IM platform arrives).
async fn http_ws_upgrade(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let rx = s.event_bus.subscribe();
    ws.on_upgrade(move |socket| handle_ws_socket(socket, rx))
}

/// WebSocket-flavoured viewer attach. Mirrors the named-pipe path
/// (handle_client) but with each broker↔viewer frame riding on one
/// WebSocket Binary message instead of being length-framed over a
/// raw byte stream. Same protocol, different transport.
/// Single-file web viewer. The HTML inlines its CSS + JS and pulls
/// xterm.js + the fit addon from sibling `/web/vendor/` routes
/// (served from `crates/broker/web/vendor/`, baked into the binary
/// via `include_bytes!`). No runtime fetch from a CDN — works
/// fully offline / on isolated LANs.
const WEB_INDEX_HTML: &str = include_str!("../web/index.html");
const WEB_VENDOR_XTERM_JS: &[u8] = include_bytes!("../web/vendor/xterm.min.js");
const WEB_VENDOR_XTERM_CSS: &[u8] = include_bytes!("../web/vendor/xterm.min.css");
const WEB_VENDOR_FIT_JS: &[u8] = include_bytes!("../web/vendor/addon-fit.min.js");

async fn serve_web_index() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/html; charset=utf-8",
        )],
        WEB_INDEX_HTML,
    )
}

/// Generic vendor asset response. `Cache-Control` is generous (1 day)
/// because these files are content-addressed by the broker version
/// they shipped with — when the broker upgrades the binary, the bytes
/// change and `If-Modified-Since` would re-fetch on its own.
fn vendor_response(content_type: &'static str, body: &'static [u8]) -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
}

async fn serve_xterm_js() -> impl IntoResponse {
    vendor_response("application/javascript; charset=utf-8", WEB_VENDOR_XTERM_JS)
}
async fn serve_xterm_css() -> impl IntoResponse {
    vendor_response("text/css; charset=utf-8", WEB_VENDOR_XTERM_CSS)
}
async fn serve_addon_fit_js() -> impl IntoResponse {
    vendor_response("application/javascript; charset=utf-8", WEB_VENDOR_FIT_JS)
}

async fn http_attach_upgrade(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let manager = s.manager.clone();
    let app_state = s.clone();

    // Browsers reject the upgrade if the server doesn't echo back one
    // of the offered subprotocols. The auth middleware already
    // validated any `bearer.<token>` subprotocol; here we just need to
    // tell axum which protocol(s) to acknowledge so the handshake
    // completes. Native CLI viewer doesn't offer a subprotocol so the
    // list is empty and axum responds without the header (also fine).
    let offered: Vec<String> = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let ws = if offered.is_empty() {
        ws
    } else {
        ws.protocols(offered)
    };

    ws.on_upgrade(move |socket| handle_ws_attach(socket, manager, app_state))
}

async fn handle_ws_attach(
    mut socket: WebSocket,
    manager: Arc<Manager>,
    app_state: AppState,
) {
    static NEXT_VIEWER_ID: AtomicU64 = AtomicU64::new(1_000_000);
    let viewer_id = NEXT_VIEWER_ID.fetch_add(1, Ordering::Relaxed);

    // Wait for HELLO frame (must be the first inbound binary message).
    let hello_payload = loop {
        match socket.recv().await {
            Some(Ok(WsMessage::Binary(b))) => match decode_frame(&b) {
                Ok((TAG_HELLO, payload)) => break payload.to_vec(),
                Ok((tag, _)) => {
                    warn!("ws-attach #{viewer_id} first frame must be HELLO, got tag={tag:#x}");
                    return;
                }
                Err(e) => {
                    warn!("ws-attach #{viewer_id} decode HELLO: {e}");
                    return;
                }
            },
            Some(Ok(WsMessage::Text(_))) => {
                warn!("ws-attach #{viewer_id} unexpected text frame as HELLO");
                return;
            }
            Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => return,
            Some(Ok(_)) => continue, // Ping/Pong/etc.
        }
    };
    let hello = decode_hello(&hello_payload).unwrap_or_default();
    let target = hello.session.as_deref().unwrap_or(DEFAULT_SESSION_NAME);
    let session = match manager.get_by_id_or_name(target) {
        Some(s) => s,
        None => {
            warn!("ws-attach #{viewer_id} unknown session: {target}");
            return;
        }
    };
    if matches!(
        session.state(),
        SessionState::Hibernated | SessionState::Crashed
    ) {
        info!(
            "ws-attach #{viewer_id} attaching {:?} session {} ({}) — auto-resuming",
            session.state(),
            session.id,
            session.name
        );
        let to_resume = session.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = to_resume.resume() {
                error!("auto-resume on ws-attach: {e}");
            }
        });
    } else if session.state() == SessionState::LocallyOwned {
        info!(
            "ws-attach #{viewer_id} attaching LocallyOwned session {} ({}) — \
             not auto-resuming (user's local claude owns the transcript)",
            session.id, session.name
        );
    }

    info!(
        "ws-attach #{viewer_id} connected ({}) → session {} ({})",
        hello.client_kind, session.id, session.name
    );

    session.register_viewer(ClientInfo {
        viewer_id,
        client_id: hello.client_id.clone(),
        client_kind: hello.client_kind.clone(),
    });
    session.touch_activity();

    let (snapshot, mut out_rx) = {
        let ring = session.pty_out.ring.lock().unwrap();
        let snap = ring.snapshot();
        let rx = session.pty_out.tx.subscribe();
        (snap, rx)
    };
    let cleanup_size_table = session.size_table.clone();
    let cleanup_session_id = session.id.clone();
    let inbound_session = session.clone();
    let inbound_app = app_state;

    // Replay snapshot first, then REPLAY_END marker.
    if !snapshot.is_empty() {
        info!(
            "ws-attach #{viewer_id} replaying {} bytes (session {})",
            snapshot.len(),
            session.name
        );
        if let Ok(buf) = encode_frame(TAG_PTY_DATA, &snapshot) {
            if socket.send(WsMessage::Binary(buf)).await.is_err() {
                return;
            }
        }
    }
    if let Ok(buf) = encode_frame(TAG_REPLAY_END, &[]) {
        let _ = socket.send(WsMessage::Binary(buf)).await;
    }

    // Single-task select loop — no need to split read/write halves
    // because WebSocket recv/send are already independent on
    // axum's WebSocket type.
    loop {
        tokio::select! {
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(WsMessage::Binary(b))) => match decode_frame(&b) {
                        Ok((TAG_PTY_DATA, payload)) => {
                            if inbound_session
                                .input_tx
                                .send(Bytes::copy_from_slice(payload))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok((TAG_RESIZE, payload)) => match decode_resize(payload) {
                            Some((c, r)) => inbound_session.size_table.update(viewer_id, c, r),
                            None => warn!("ws-attach #{viewer_id} bad RESIZE"),
                        },
                        Ok((TAG_CONTROL, payload)) => match decode_control(payload) {
                            Some(cmd) => {
                                handle_control_cmd(viewer_id, &inbound_session, cmd, &inbound_app)
                                    .await
                            }
                            None => warn!("ws-attach #{viewer_id} bad CONTROL"),
                        },
                        Ok((TAG_HELLO, _)) => warn!("ws-attach #{viewer_id} unexpected second HELLO"),
                        Ok((tag, _)) => warn!("ws-attach #{viewer_id} unknown tag {tag:#x}"),
                        Err(e) => warn!("ws-attach #{viewer_id} decode: {e}"),
                    },
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {} // Ping/Pong/Text — ignore.
                }
            }
            ev = out_rx.recv() => match ev {
                Ok(bytes) => {
                    let buf = match encode_frame(TAG_PTY_DATA, &bytes) {
                        Ok(b) => b,
                        Err(e) => { warn!("ws-attach encode: {e}"); break; }
                    };
                    if socket.send(WsMessage::Binary(buf)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("ws-attach #{viewer_id} lagged: dropped {n}");
                    continue;
                }
            },
        }
    }

    if let Some(s) = manager.get_by_id_or_name(&cleanup_session_id) {
        s.deregister_viewer(viewer_id);
    }
    cleanup_size_table.remove(viewer_id);
    info!("ws-attach #{viewer_id} disconnected");
}

async fn handle_ws_socket(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<serde_json::Value>,
) {
    info!("ws subscriber connected");
    loop {
        tokio::select! {
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {
                        // Reserved for future {"type":"subscribe", ...}
                        // and bot→broker control messages.
                    }
                }
            }
            ev = rx.recv() => match ev {
                Ok(v) => {
                    let line = match serde_json::to_string(&v) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("ws: serialise event: {e}");
                            continue;
                        }
                    };
                    if socket.send(WsMessage::Text(line)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("ws subscriber lagged: dropped {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    info!("ws subscriber disconnected");
}
