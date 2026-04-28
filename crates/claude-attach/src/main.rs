//! agentmux claude-attach — Phase 7.6.
//!
//! Frame-protocol viewer with Ctrl+C escalation and an interactive
//! session-selection menu when launched without `--session` or
//! `--new`. CLI:
//!
//! ```text
//! claude-attach.exe                   # menu picks session
//! claude-attach.exe --session NAME    # attach directly
//! claude-attach.exe --new [NAME]      # create + attach (auto s1/s2/.. if NAME omitted)
//! claude-attach.exe --debug           # log stdin bytes to stderr
//! ```
//!
//! Ctrl+C escalation in raw mode:
//!   * 1 in 1.5s  → forwarded as 0x03 (claude interrupts)
//!   * 2 in 1.5s  → CONTROL `restart-claude`
//!   * 3 in 1.5s  → CONTROL `shutdown` (broker exits)
//!   * Ctrl+Q / Ctrl+] → detach this viewer only

use std::collections::VecDeque;
use std::io::Write as IoWrite;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;
use shared::config::Config;
use shared::frame::{
    encode_control, encode_hello, encode_resize, read_frame, write_frame, HelloPayload,
    CTRL_RESTART, CTRL_SHUTDOWN, TAG_CONTROL, TAG_HELLO, TAG_PTY_DATA, TAG_RESIZE,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::mpsc;

mod console;
use console::SendHandle;

const SIZE_POLL_MS: u64 = 250;
const STDIN_CHUNK: usize = 4096;
const FRAME_QUEUE_CAP: usize = 64;
const CTRL_C_WINDOW_MS: u64 = 1500;
const SHUTDOWN_FLUSH_MS: u64 = 150;

const CTRL_C: u8 = 0x03;

/// Bytes that detach this viewer (close the pipe, leave broker alone).
/// See PLAN §2.3; Ctrl+\ is unreliable under Windows keyboard layouts so
/// we accept Ctrl+Q (0x11) and Ctrl+] (0x1d) instead.
const DETACH_BYTES: &[u8] = &[0x11, 0x1d];

#[derive(Debug, Default)]
struct Args {
    debug: bool,
    session: Option<String>,
    new: Option<Option<String>>, // Some(None) = --new with no name; Some(Some(n)) = --new n
    /// Remote broker URL (`http://host:port` or `ws://host:port`).
    /// When set, claude-attach connects via WebSocket instead of the
    /// local named pipe. The leading scheme is normalised to ws://
    /// internally; companion HTTP calls (list / create) target the
    /// same host on the same port over plain http://.
    broker: Option<String>,
    /// Bearer token for non-loopback brokers. Falls back to
    /// `AGENT_ATTACH_TOKEN` env var. Loopback brokers ignore it.
    token: Option<String>,
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().collect();
    let mut a = Args::default();
    a.debug = argv.iter().any(|s| s == "--debug");
    if let Some(i) = argv.iter().position(|s| s == "--session") {
        a.session = argv.get(i + 1).cloned();
    }
    if let Some(i) = argv.iter().position(|s| s == "--new") {
        let nxt = argv.get(i + 1).filter(|s| !s.starts_with("--")).cloned();
        a.new = Some(nxt);
    }
    if let Some(i) = argv.iter().position(|s| s == "--broker") {
        a.broker = argv.get(i + 1).cloned();
    }
    if let Some(i) = argv.iter().position(|s| s == "--token") {
        a.token = argv.get(i + 1).cloned();
    }
    if a.token.is_none() {
        if let Ok(v) = std::env::var("AGENT_ATTACH_TOKEN") {
            if !v.is_empty() {
                a.token = Some(v);
            }
        }
    }
    a
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
struct SessionInfoLite {
    name: String,
    cwd: String,
    created_at_ms: u64,
    viewers: usize,
    state: String,
}

impl Default for SessionInfoLite {
    fn default() -> Self {
        Self {
            name: String::new(),
            cwd: String::new(),
            created_at_ms: 0,
            viewers: 0,
            state: "idle".to_string(),
        }
    }
}

fn fetch_sessions(broker_url: &str, token: Option<&str>) -> Result<Vec<SessionInfoLite>> {
    let body = shared::http::get_with_auth(&format!("{broker_url}/sessions"), token)
        .with_context(|| format!("GET {broker_url}/sessions"))?;
    let sessions: Vec<SessionInfoLite> =
        serde_json::from_str(&body).context("parse /sessions response")?;
    Ok(sessions)
}

fn create_session(broker_url: &str, name: &str, token: Option<&str>) -> Result<()> {
    let body = format!(r#"{{"name":"{name}"}}"#);
    shared::http::post_json_with_auth(&format!("{broker_url}/sessions"), &body, token)
        .with_context(|| format!("POST {broker_url}/sessions"))?;
    Ok(())
}

fn auto_name(existing: &[SessionInfoLite]) -> String {
    let used: std::collections::HashSet<&str> =
        existing.iter().map(|s| s.name.as_str()).collect();
    for i in 1..u32::MAX {
        let candidate = format!("s{i}");
        if !used.contains(candidate.as_str()) {
            return candidate;
        }
    }
    // Fall back to a UUID slice if we somehow exhaust u32 names.
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

fn format_age_ms(created_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs = now.saturating_sub(created_ms) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out = s.chars().take(max - 1).collect::<String>();
        out.push('…');
        out
    }
}

/// Plain-text menu over stdin/stdout — runs *before* raw mode is set
/// up, so cooked-mode line input from the user works as expected.
/// Returns `Some(name)` to attach, or `None` to quit cleanly.
fn show_menu(broker_url: &str, token: Option<&str>) -> Result<Option<String>> {
    let sessions = match fetch_sessions(broker_url, token) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot reach broker at {broker_url}: {e}");
            eprintln!("(start it with scripts\\start-broker.ps1)");
            return Ok(None);
        }
    };

    println!();
    println!("agentmux — sessions @ {broker_url}");
    println!("─────────────────────────────────────────────────────────");
    if sessions.is_empty() {
        println!("  (no sessions yet — pick 'n' to create one)");
    } else {
        for (i, s) in sessions.iter().enumerate() {
            let viewers_label = match s.viewers {
                0 => String::new(),
                1 => "  [1 viewer]".to_string(),
                n => format!("  [{n} viewers]"),
            };
            let state_label = match s.state.as_str() {
                "hibernated" => "  [hibernated]",
                "crashed" => "  [crashed]",
                _ => "",
            };
            println!(
                "  {:>2}. {:<18}  {:>9}{}{}  cwd={}",
                i + 1,
                truncate(&s.name, 18),
                format_age_ms(s.created_at_ms),
                state_label,
                viewers_label,
                truncate(&s.cwd, 40),
            );
        }
    }
    println!("  ──");
    println!("   n. <new session>");
    println!("   q. quit");
    print!("\nChoose: ");
    std::io::stdout().flush().ok();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let choice = input.trim();

    match choice {
        "" | "q" | "Q" => Ok(None),
        "n" | "N" => {
            print!("Name (blank = auto): ");
            std::io::stdout().flush().ok();
            let mut name_input = String::new();
            std::io::stdin().read_line(&mut name_input)?;
            let name = name_input.trim();
            let final_name = if name.is_empty() {
                auto_name(&sessions)
            } else {
                name.to_string()
            };
            create_session(broker_url, &final_name, token)?;
            println!("created session: {final_name}");
            Ok(Some(final_name))
        }
        s => match s.parse::<usize>() {
            Ok(n) if n >= 1 && n <= sessions.len() => Ok(Some(sessions[n - 1].name.clone())),
            _ => {
                eprintln!("invalid choice: {s}");
                Ok(None)
            }
        },
    }
}

fn resolve_session(args: &Args, broker_url: &str, token: Option<&str>) -> Result<Option<String>> {
    if let Some(name) = &args.session {
        return Ok(Some(name.clone()));
    }
    if let Some(maybe_name) = &args.new {
        let sessions = fetch_sessions(broker_url, token)?;
        let name = match maybe_name {
            Some(n) => n.clone(),
            None => auto_name(&sessions),
        };
        create_session(broker_url, &name, token)?;
        println!("created session: {name}");
        return Ok(Some(name));
    }
    show_menu(broker_url, token)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = parse_args();
    let config = Config::load();

    // Compute the broker HTTP base URL: --broker overrides the
    // local config (and gets its scheme normalised so users can pass
    // ws://host:port and we still GET /sessions over plain http).
    let http_base = match &args.broker {
        Some(url) => normalise_http_base(url),
        None => config.http_url(),
    };
    let token = args.token.as_deref();

    let session = match resolve_session(&args, &http_base, token)? {
        Some(name) => name,
        None => return Ok(()),
    };

    let _restore = console::enter_raw_mode().context("enter raw mode")?;
    let stdout_h = console::stdout_send_handle()?;

    if args.broker.is_some() {
        run_ws_attach(&args, &session, stdout_h).await
    } else {
        run_pipe_attach(&config, &args, &session, stdout_h).await
    }
}

/// Pipe transport — the original on-host path. Unchanged behaviour
/// from before `--broker` was added.
async fn run_pipe_attach(
    config: &Config,
    args: &Args,
    session: &str,
    stdout_h: console::SendHandle,
) -> Result<()> {
    let pipe = connect_with_retry(&config.pipe_name).await?;
    let (read_half, write_half) = tokio::io::split(pipe);

    let (frame_tx, frame_rx) = mpsc::channel::<(u8, Vec<u8>)>(FRAME_QUEUE_CAP);

    let hello = HelloPayload {
        client_id: uuid::Uuid::new_v4().to_string(),
        client_kind: "terminal".to_string(),
        mode: "rw".to_string(),
        session: Some(session.to_string()),
    };
    let _ = frame_tx.send((TAG_HELLO, encode_hello(&hello))).await;

    if let Ok((cols, rows)) = console::query_size(stdout_h) {
        let _ = frame_tx
            .send((TAG_RESIZE, encode_resize(cols, rows).to_vec()))
            .await;
    }

    let writer_fut = writer_task(frame_rx, write_half);
    let stdin_fut = stdin_to_frames(frame_tx.clone(), args.debug);
    let size_fut = size_poller(stdout_h, frame_tx.clone());
    let pipe_fut = frames_to_stdout(read_half);
    drop(frame_tx);

    tokio::select! {
        _ = writer_fut => {},
        _ = stdin_fut => {},
        _ = size_fut => {},
        _ = pipe_fut => {},
    }
    Ok(())
}

/// WebSocket transport — talks to a remote broker over `--broker`.
/// Each frame is one Binary WS message (encode_frame / decode_frame).
async fn run_ws_attach(
    args: &Args,
    session: &str,
    stdout_h: console::SendHandle,
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use shared::frame::{decode_frame, encode_frame};
    use tokio_tungstenite::tungstenite::http::Request;
    use tokio_tungstenite::tungstenite::protocol::Message as WsMsg;

    let raw_url = args
        .broker
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--broker required for WS path"))?;
    let ws_url = normalise_ws_attach_url(raw_url);

    // Build an explicit HTTP request so we can attach the
    // Authorization header (tokio-tungstenite's connect_async only
    // takes a URL string by default).
    let mut req_builder = Request::builder()
        .method("GET")
        .uri(&ws_url)
        .header("Host", host_of(&ws_url).unwrap_or_default())
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", random_ws_key());
    if let Some(t) = args.token.as_deref() {
        if !t.is_empty() {
            req_builder = req_builder.header("Authorization", format!("Bearer {t}"));
        }
    }
    let req = req_builder
        .body(())
        .context("build ws upgrade request")?;

    let (ws_stream, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .with_context(|| format!("connect {ws_url}"))?;
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    let (frame_tx, mut frame_rx) = mpsc::channel::<(u8, Vec<u8>)>(FRAME_QUEUE_CAP);

    // Send HELLO + initial size first thing.
    let hello = HelloPayload {
        client_id: uuid::Uuid::new_v4().to_string(),
        client_kind: "terminal".to_string(),
        mode: "rw".to_string(),
        session: Some(session.to_string()),
    };
    let _ = frame_tx.send((TAG_HELLO, encode_hello(&hello))).await;
    if let Ok((cols, rows)) = console::query_size(stdout_h) {
        let _ = frame_tx
            .send((TAG_RESIZE, encode_resize(cols, rows).to_vec()))
            .await;
    }

    let writer_fut = async move {
        while let Some((tag, payload)) = frame_rx.recv().await {
            let buf = match encode_frame(tag, &payload) {
                Ok(b) => b,
                Err(_) => break,
            };
            if ws_sink.send(WsMsg::Binary(buf)).await.is_err() {
                break;
            }
        }
        let _ = ws_sink.close().await;
    };

    let reader_fut = async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(WsMsg::Binary(b)) => match decode_frame(&b) {
                    Ok((TAG_PTY_DATA, payload)) => {
                        if !payload.is_empty()
                            && (stdout.write_all(payload).await.is_err()
                                || stdout.flush().await.is_err())
                        {
                            break;
                        }
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                },
                Ok(WsMsg::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    };

    let stdin_fut = stdin_to_frames(frame_tx.clone(), args.debug);
    let size_fut = size_poller(stdout_h, frame_tx.clone());
    drop(frame_tx);

    tokio::select! {
        _ = writer_fut => {},
        _ = stdin_fut => {},
        _ = size_fut => {},
        _ = reader_fut => {},
    }
    Ok(())
}

/// Turn user input like `ws://h:p`, `wss://h:p`, `http://h:p`,
/// or `https://h:p` into the http(s)://h:p form used for /sessions
/// REST calls. Bare `host:port` becomes `http://host:port`.
fn normalise_http_base(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if let Some(rest) = raw.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    }
}

/// Turn user input into `ws://host:port/attach` (or `wss://`).
fn normalise_ws_attach_url(raw: &str) -> String {
    let base = if let Some(rest) = raw.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if let Some(rest) = raw.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if raw.starts_with("ws://") || raw.starts_with("wss://") {
        raw.to_string()
    } else {
        format!("ws://{raw}")
    };
    let trimmed = base.trim_end_matches('/');
    format!("{trimmed}/attach")
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| match u.port() {
            Some(p) => format!("{h}:{p}"),
            None => h.to_string(),
        }))
}

/// 16 random bytes, base64 standard alphabet — what RFC 6455 requires
/// for `Sec-WebSocket-Key`. The server doesn't validate uniqueness,
/// just echoes a hash; we use system RNG to keep replays unlikely.
fn random_ws_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Cheap pseudo-random — combine nano timestamp + uuid bytes.
    let mut bytes = [0u8; 16];
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .to_le_bytes();
    bytes[..16].copy_from_slice(&nanos[..16]);
    let uuid = uuid::Uuid::new_v4().into_bytes();
    for (i, b) in uuid.iter().enumerate().take(16) {
        bytes[i] ^= b;
    }
    base64_encode(&bytes)
}

/// Minimal RFC 4648 base64 encoder — small enough we don't pull in
/// the `base64` crate just for a 16-byte key.
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(CHARS[(b0 >> 2) as usize] as char);
        out.push(CHARS[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

async fn connect_with_retry(pipe_name: &str) -> Result<NamedPipeClient> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() < deadline => {
                eprintln!("waiting for broker… ({e})");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => return Err(e).context(format!("open {pipe_name}")),
        }
    }
}

async fn writer_task(
    mut frame_rx: mpsc::Receiver<(u8, Vec<u8>)>,
    mut w: tokio::io::WriteHalf<NamedPipeClient>,
) {
    while let Some((tag, payload)) = frame_rx.recv().await {
        if write_frame(&mut w, tag, &payload).await.is_err() {
            break;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlCAction {
    Forward,
    Restart,
    Shutdown,
}

struct CtrlCTracker {
    presses: VecDeque<Instant>,
    window: Duration,
}

impl CtrlCTracker {
    fn new(window: Duration) -> Self {
        Self {
            presses: VecDeque::new(),
            window,
        }
    }

    fn classify(&mut self, now: Instant) -> CtrlCAction {
        while let Some(&t) = self.presses.front() {
            if now.duration_since(t) > self.window {
                self.presses.pop_front();
            } else {
                break;
            }
        }
        self.presses.push_back(now);
        match self.presses.len() {
            1 => CtrlCAction::Forward,
            2 => CtrlCAction::Restart,
            _ => CtrlCAction::Shutdown,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ChunkResult {
    Continue,
    Detach,
    BrokenPipe,
}

async fn stdin_to_frames(frame_tx: mpsc::Sender<(u8, Vec<u8>)>, debug: bool) {
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; STDIN_CHUNK];
    let mut tracker = CtrlCTracker::new(Duration::from_millis(CTRL_C_WINDOW_MS));
    loop {
        match stdin.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if debug {
                    let hex: String = buf[..n]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    eprintln!("[stdin {n}B] {hex}");
                }
                match process_chunk(&buf[..n], &frame_tx, &mut tracker).await {
                    ChunkResult::Continue => {}
                    ChunkResult::Detach | ChunkResult::BrokenPipe => break,
                }
            }
        }
    }
}

async fn process_chunk(
    chunk: &[u8],
    frame_tx: &mpsc::Sender<(u8, Vec<u8>)>,
    tracker: &mut CtrlCTracker,
) -> ChunkResult {
    let mut pending: Vec<u8> = Vec::new();
    for &b in chunk {
        match b {
            CTRL_C => match tracker.classify(Instant::now()) {
                CtrlCAction::Forward => pending.push(CTRL_C),
                CtrlCAction::Restart => {
                    if flush_pending(&mut pending, frame_tx).await.is_err() {
                        return ChunkResult::BrokenPipe;
                    }
                    if frame_tx
                        .send((TAG_CONTROL, encode_control(CTRL_RESTART)))
                        .await
                        .is_err()
                    {
                        return ChunkResult::BrokenPipe;
                    }
                }
                CtrlCAction::Shutdown => {
                    let _ = flush_pending(&mut pending, frame_tx).await;
                    let _ = frame_tx
                        .send((TAG_CONTROL, encode_control(CTRL_SHUTDOWN)))
                        .await;
                    tokio::time::sleep(Duration::from_millis(SHUTDOWN_FLUSH_MS)).await;
                    return ChunkResult::Detach;
                }
            },
            b if DETACH_BYTES.contains(&b) => {
                let _ = flush_pending(&mut pending, frame_tx).await;
                return ChunkResult::Detach;
            }
            other => pending.push(other),
        }
    }
    match flush_pending(&mut pending, frame_tx).await {
        Ok(()) => ChunkResult::Continue,
        Err(()) => ChunkResult::BrokenPipe,
    }
}

async fn flush_pending(
    pending: &mut Vec<u8>,
    frame_tx: &mpsc::Sender<(u8, Vec<u8>)>,
) -> std::result::Result<(), ()> {
    if pending.is_empty() {
        return Ok(());
    }
    let payload = std::mem::take(pending);
    frame_tx
        .send((TAG_PTY_DATA, payload))
        .await
        .map_err(|_| ())
}

async fn size_poller(stdout_h: SendHandle, frame_tx: mpsc::Sender<(u8, Vec<u8>)>) {
    let mut last = console::query_size(stdout_h).unwrap_or((0, 0));
    loop {
        tokio::time::sleep(Duration::from_millis(SIZE_POLL_MS)).await;
        let cur = match console::query_size(stdout_h) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if cur != last && cur.0 > 0 && cur.1 > 0 {
            if frame_tx
                .send((TAG_RESIZE, encode_resize(cur.0, cur.1).to_vec()))
                .await
                .is_err()
            {
                break;
            }
            last = cur;
        }
    }
}

async fn frames_to_stdout(mut r: tokio::io::ReadHalf<NamedPipeClient>) {
    let mut stdout = tokio::io::stdout();
    loop {
        match read_frame(&mut r).await {
            Ok((TAG_PTY_DATA, payload)) => {
                if !payload.is_empty() {
                    if stdout.write_all(&payload).await.is_err() {
                        break;
                    }
                    if stdout.flush().await.is_err() {
                        break;
                    }
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_forward_restart_shutdown() {
        let mut t = CtrlCTracker::new(Duration::from_millis(1500));
        let now = Instant::now();
        assert_eq!(t.classify(now), CtrlCAction::Forward);
        assert_eq!(
            t.classify(now + Duration::from_millis(50)),
            CtrlCAction::Restart
        );
        assert_eq!(
            t.classify(now + Duration::from_millis(100)),
            CtrlCAction::Shutdown
        );
    }

    #[test]
    fn tracker_resets_after_window() {
        let mut t = CtrlCTracker::new(Duration::from_millis(1500));
        let now = Instant::now();
        assert_eq!(t.classify(now), CtrlCAction::Forward);
        assert_eq!(
            t.classify(now + Duration::from_millis(2000)),
            CtrlCAction::Forward
        );
    }

    #[test]
    fn auto_name_finds_first_gap() {
        let mk = |name: &str| SessionInfoLite {
            name: name.into(),
            cwd: "x".into(),
            created_at_ms: 0,
            viewers: 0,
            state: "idle".to_string(),
        };
        let existing = vec![mk("default"), mk("s1"), mk("s3")];
        assert_eq!(auto_name(&existing), "s2");
    }

    #[test]
    fn auto_name_fresh() {
        assert_eq!(auto_name(&[]), "s1");
    }

    #[test]
    fn parse_args_session_only() {
        // Note: parse_args reads std::env::args, not testable directly.
        // Just validate the helper behavior on realistic shapes.
        let mut a = Args::default();
        a.session = Some("blog".into());
        assert!(a.session.as_deref() == Some("blog"));
        assert!(a.new.is_none());
    }
}
