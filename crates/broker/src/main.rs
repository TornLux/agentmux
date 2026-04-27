//! agentmux broker — Phase 7.5.
//!
//! Manages N concurrent claude sessions. Each session owns its own
//! ConPTY, ring buffer, broadcast channel, and input mpsc; sessions
//! are addressed by UUID `id` or by a human-friendly `name`. The pipe
//! protocol now requires a `HELLO` frame at connect time selecting
//! which session the viewer attaches to (defaults to `default` if
//! unspecified).

use std::fs::{File, OpenOptions};
use std::io::Write as IoWrite;
use std::path::{Path as StdPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use shared::config::Config;
use shared::frame::{
    decode_control, decode_hello, decode_resize, read_frame, write_frame, HelloPayload,
    CTRL_INTERRUPT, CTRL_RESTART, CTRL_SHUTDOWN, TAG_CONTROL, TAG_HELLO, TAG_PTY_DATA,
    TAG_REPLAY_END, TAG_RESIZE,
};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{broadcast, watch};
use tracing::{error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};

mod manager;
mod ringbuf;
mod session;

use manager::Manager;
use session::{ClientInfo, Session, SessionInfo, SessionState};

const DEFAULT_SESSION_NAME: &str = "default";

/// Append-only JSONL sink for hook events. The Mutex protects file
/// ordering across concurrent /event requests; once a write fails we
/// drop the handle and stop trying rather than spam log noise.
struct EventsLog {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl EventsLog {
    fn open(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                warn!("cannot open events log {:?}: {e}", path);
                e
            })
            .ok();
        Self {
            path,
            file: Mutex::new(file),
        }
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
        let mut g = self.file.lock().unwrap();
        if let Some(f) = g.as_mut() {
            if let Err(e) = f.write_all(line.as_bytes()) {
                warn!("events.jsonl write {:?}: {e}", self.path);
                *g = None;
                return;
            }
            let _ = f.flush();
        }
    }
}

fn default_events_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("agentmux").join("events.jsonl")
}

#[derive(Clone)]
struct AppState {
    manager: Arc<Manager>,
    shutdown_tx: watch::Sender<bool>,
    events: Arc<EventsLog>,
    default_cwd: PathBuf,
    config: Arc<Config>,
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
    let cwd = match cwd_override {
        Some(c) => c,
        None => std::env::current_dir().context("get cwd")?,
    };
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
            .create(DEFAULT_SESSION_NAME.to_string(), cwd.clone(), true)
            .context("create default session")?;
        info!(
            "default session id={} (name={}, fresh)",
            default_session.id, default_session.name
        );
    } else {
        info!("default session restored from sessions.toml");
    }

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let app_state = AppState {
        manager: manager.clone(),
        shutdown_tx: shutdown_tx.clone(),
        events,
        default_cwd: cwd,
        config: config.clone(),
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
    let app = Router::new()
        .route("/sessions", get(http_list).post(http_create))
        .route("/sessions/:key", get(http_get).delete(http_delete))
        .route("/sessions/:key/state", get(http_state))
        .route("/sessions/:key/interrupt", post(http_interrupt))
        .route("/sessions/:key/restart", post(http_restart))
        .route("/sessions/:key/hibernate", post(http_hibernate))
        .route("/shutdown", post(http_shutdown))
        .route("/event", post(http_event))
        .with_state(app_state);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("bind {addr}: {e}");
            return;
        }
    };
    info!("http control plane on http://{addr}");
    if let Err(e) = axum::serve(listener, app).await {
        error!("axum serve: {e}");
    }
}

async fn run_pipe_server(manager: Arc<Manager>, app_state: AppState) -> Result<()> {
    let pipe_name = app_state.config.pipe_name.clone();
    info!("listening on {}", pipe_name);
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .with_context(|| format!("create pipe {pipe_name}"))?;
    let next_id = Arc::new(AtomicU64::new(1));

    loop {
        server.connect().await.context("pipe connect")?;
        let connected = server;
        server = ServerOptions::new()
            .create(&pipe_name)
            .context("re-create pipe")?;

        let viewer_id = next_id.fetch_add(1, Ordering::Relaxed);
        let manager = manager.clone();
        let app_state = app_state.clone();
        tokio::spawn(handle_client(viewer_id, connected, manager, app_state));
    }
}

async fn handle_client(
    viewer_id: u64,
    pipe: NamedPipeServer,
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
    reader: &mut tokio::io::ReadHalf<NamedPipeServer>,
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

async fn http_list(State(s): State<AppState>) -> Json<Vec<SessionInfo>> {
    Json(s.manager.list())
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    cwd: Option<String>,
    #[serde(default = "default_auto_resume")]
    auto_resume: bool,
}

fn default_auto_resume() -> bool {
    true
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
    let session = s
        .manager
        .create(body.name, cwd, body.auto_resume)
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(session.info()))
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
    let local_viewer_attached = attached.iter().any(|c| c.client_kind == "terminal");
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
) -> Result<&'static str, StatusCode> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or(StatusCode::NOT_FOUND)?;
    let to_hibernate = session.clone();
    tokio::task::spawn_blocking(move || to_hibernate.hibernate())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
) -> Result<&'static str, StatusCode> {
    let session = s
        .manager
        .get_by_id_or_name(&key)
        .ok_or(StatusCode::NOT_FOUND)?;
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
    let res = tokio::task::spawn_blocking(move || session.restart())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    res.map_err(|e| {
        error!("http restart: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    Ok("ok")
}

async fn http_shutdown(State(s): State<AppState>) -> &'static str {
    let _ = s.shutdown_tx.send(true);
    "ok"
}

async fn http_event(
    State(s): State<AppState>,
    Json(event): Json<serde_json::Value>,
) -> &'static str {
    let kind = event
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();

    // Capture claude's own session id from the transcript path the
    // first time we see it so future restart/resume can pass --resume.
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

    s.events.append(event);
    info!("event recorded: {kind}");
    "ok"
}

/// Pulls the UUID-shaped basename out of a Claude Code transcript
/// path (`.../<uuid>.jsonl`). Returns None if the basename isn't a
/// valid UUID — that filters out unrelated files claude might write.
fn extract_claude_session_id(transcript_path: &str) -> Option<String> {
    let stem = StdPath::new(transcript_path).file_stem()?.to_str()?;
    uuid::Uuid::parse_str(stem).ok().map(|_| stem.to_string())
}
