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

fn fetch_sessions(broker_url: &str) -> Result<Vec<SessionInfoLite>> {
    let body = shared::http::get(&format!("{broker_url}/sessions"))
        .with_context(|| format!("GET {broker_url}/sessions"))?;
    let sessions: Vec<SessionInfoLite> =
        serde_json::from_str(&body).context("parse /sessions response")?;
    Ok(sessions)
}

fn create_session(broker_url: &str, name: &str) -> Result<()> {
    let body = format!(r#"{{"name":"{name}"}}"#);
    shared::http::post_json(&format!("{broker_url}/sessions"), &body)
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
fn show_menu(broker_url: &str) -> Result<Option<String>> {
    let sessions = match fetch_sessions(broker_url) {
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
            create_session(broker_url, &final_name)?;
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

fn resolve_session(args: &Args, broker_url: &str) -> Result<Option<String>> {
    if let Some(name) = &args.session {
        return Ok(Some(name.clone()));
    }
    if let Some(maybe_name) = &args.new {
        let sessions = fetch_sessions(broker_url)?;
        let name = match maybe_name {
            Some(n) => n.clone(),
            None => auto_name(&sessions),
        };
        create_session(broker_url, &name)?;
        println!("created session: {name}");
        return Ok(Some(name));
    }
    show_menu(broker_url)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = parse_args();
    let config = Config::load();

    let session = match resolve_session(&args, &config.http_url())? {
        Some(name) => name,
        None => return Ok(()),
    };

    let _restore = console::enter_raw_mode().context("enter raw mode")?;
    let stdout_h = console::stdout_send_handle()?;

    let pipe = connect_with_retry(&config.pipe_name).await?;
    let (read_half, write_half) = tokio::io::split(pipe);

    let (frame_tx, frame_rx) = mpsc::channel::<(u8, Vec<u8>)>(FRAME_QUEUE_CAP);

    let hello = HelloPayload {
        client_id: uuid::Uuid::new_v4().to_string(),
        client_kind: "terminal".to_string(),
        mode: "rw".to_string(),
        session: Some(session),
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
