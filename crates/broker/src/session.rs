//! Per-session PTY + ring + broadcast bundle.
//!
//! Phase 7.7 adds:
//!   * SessionState (Idle / Hibernated / Crashed) with manual hibernate()
//!     and resume() methods.
//!   * `claude_session_id` capture so we can spawn claude with
//!     `--resume <id>` after hibernate or broker restart.
//!   * `auto_resume` flag controlling whether the session is restored
//!     from sessions.toml at broker boot.
//!   * `persisted()` projection for sessions.toml round-trips.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use serde::{Deserialize, Serialize};
use shared::config::Config;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

use crate::ringbuf::RingBuffer;

const PTY_READ_CHUNK: usize = 8192;
const OUT_BROADCAST_CAP: usize = 1024;
const IN_QUEUE_CAP: usize = 256;
const INITIAL_SIZE: PtySize = PtySize {
    rows: 30,
    cols: 120,
    pixel_width: 0,
    pixel_height: 0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Hibernated,
    Crashed,
    /// User has explicitly handed control of this conversation back to a
    /// local terminal (`agentmux demote`). claude is **not** running
    /// under broker — the user's local `claude --resume <id>` owns the
    /// transcript. broker keeps id/name/cwd/claude_session_id so the
    /// user can later `agentmux adopt <name>` to re-spawn under broker.
    /// While in this state, broker refuses /input /interrupt /restart
    /// /hibernate /demote with 409, and Discord/tray surface the
    /// state distinctly. The hibernate scanner skips it.
    LocallyOwned,
}

/// Snapshot of a session as it appears on disk in `sessions.toml`.
/// Phase 7.7 persists enough to bring back a hibernated session and
/// continue claude with `--resume <claude_session_id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub id: String,
    pub name: String,
    pub cwd: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub claude_session_id: Option<String>,
    #[serde(default = "default_auto_resume")]
    pub auto_resume: bool,
    #[serde(default)]
    pub created_at_ms: u64,
    /// Was the session demoted at last save? Old sessions.toml files
    /// without this key default to `false` — i.e. classic
    /// "broker-owned, restore as Hibernated" behaviour.
    #[serde(default)]
    pub locally_owned: bool,
}

fn default_auto_resume() -> bool {
    true
}

/// Returned by `Session::demote`. Surfaced verbatim to the CLI so it
/// can print the precise resume command to the user; `graceful` lets
/// us warn when the `/exit` window timed out and we had to TerminateProcess.
#[derive(Debug, Clone, Serialize)]
pub struct DemoteOutcome {
    pub claude_session_id: Option<String>,
    pub cwd: String,
    pub graceful: bool,
}

pub struct PtyOutput {
    pub ring: Mutex<RingBuffer>,
    pub tx: broadcast::Sender<Bytes>,
}

pub struct SizeTable {
    pub master_handle: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    pub sizes: Mutex<HashMap<u64, (u16, u16)>>,
    pub last_applied: Mutex<PtySize>,
}

impl SizeTable {
    pub fn update(&self, viewer_id: u64, cols: u16, rows: u16) {
        let mut sizes = self.sizes.lock().unwrap();
        sizes.insert(viewer_id, (cols, rows));
        self.recompute(&sizes);
    }

    pub fn remove(&self, viewer_id: u64) {
        let mut sizes = self.sizes.lock().unwrap();
        sizes.remove(&viewer_id);
        self.recompute(&sizes);
    }

    pub fn last(&self) -> PtySize {
        *self.last_applied.lock().unwrap()
    }

    fn recompute(&self, sizes: &HashMap<u64, (u16, u16)>) {
        if sizes.is_empty() {
            return;
        }
        let cols = sizes
            .values()
            .map(|(c, _)| *c)
            .filter(|c| *c > 0)
            .min()
            .unwrap_or(80)
            .max(1);
        let rows = sizes
            .values()
            .map(|(_, r)| *r)
            .filter(|r| *r > 0)
            .min()
            .unwrap_or(24)
            .max(1);
        let new_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        *self.last_applied.lock().unwrap() = new_size;
        if let Some(m) = self.master_handle.lock().unwrap().as_ref() {
            if let Err(e) = m.resize(new_size) {
                warn!("pty resize: {e}");
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientInfo {
    pub viewer_id: u64,
    pub client_id: String,
    pub client_kind: String,
}

#[derive(Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub created_at_ms: u64,
    pub argv: Vec<String>,
    pub viewers: usize,
    pub state: SessionState,
    pub auto_resume: bool,
    pub claude_session_id: Option<String>,
}

pub struct Session {
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub config: Arc<Config>,
    pub created_at_ms: u64,
    pub pty_out: Arc<PtyOutput>,
    pub size_table: Arc<SizeTable>,
    pub input_tx: mpsc::Sender<Bytes>,
    pub attached: Mutex<HashMap<u64, ClientInfo>>,
    state: Mutex<SessionState>,
    claude_session_id: Mutex<Option<String>>,
    auto_resume: Mutex<bool>,
    last_activity: Mutex<Instant>,
    inner: Mutex<SessionInner>,
    /// `Arc::new_cyclic` plumbing: lets `&self` methods that need to
    /// hand the session arc to a thread (PTY reader, crash watcher,
    /// resume()) recover an Arc<Self> without callers having to thread
    /// it through every signature.
    self_weak: Weak<Session>,
}

struct SessionInner {
    writer: Option<Box<dyn Write + Send>>,
    child: Option<Box<dyn Child + Send>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Session {
    /// Build a Session and bring its claude up. When
    /// `resume_claude_session_id` is `Some`, claude is spawned with
    /// `--resume <id>` so it picks up an existing transcript — that's
    /// the path used by `agentmux adopt --resume <id>` to bring an
    /// independently-running claude conversation under broker control.
    /// `None` starts a brand-new conversation; the id is then captured
    /// from the first hook event.
    pub fn create_and_start(
        name: String,
        cwd: PathBuf,
        argv: Vec<String>,
        config: Arc<Config>,
        auto_resume: bool,
        resume_claude_session_id: Option<String>,
    ) -> Result<Arc<Self>> {
        let id = uuid::Uuid::new_v4().to_string();
        let use_resume = resume_claude_session_id.is_some();
        let session = Self::build_hibernated(
            id,
            name,
            cwd,
            argv,
            config,
            auto_resume,
            resume_claude_session_id,
            now_ms(),
        );
        session.start_pty(use_resume)?;
        *session.state.lock().unwrap() = SessionState::Idle;
        Ok(session)
    }

    /// Reconstruct a Session from `sessions.toml`. The session is
    /// returned in `Hibernated` state by default — the next attach (or
    /// explicit resume()) will spawn claude with `--resume`. If the
    /// stored record was demoted at last save (`locally_owned: true`),
    /// the state comes back as `LocallyOwned` instead, and broker will
    /// refuse to spawn claude until the user explicitly re-adopts.
    pub fn from_persisted(persisted: PersistedSession, config: Arc<Config>) -> Arc<Self> {
        let argv = if persisted.argv.is_empty() {
            config.default_command.clone()
        } else {
            persisted.argv.clone()
        };
        let was_locally_owned = persisted.locally_owned;
        let session = Self::build_hibernated(
            persisted.id,
            persisted.name,
            PathBuf::from(persisted.cwd),
            argv,
            config,
            persisted.auto_resume,
            persisted.claude_session_id,
            persisted.created_at_ms,
        );
        if was_locally_owned {
            *session.state.lock().unwrap() = SessionState::LocallyOwned;
        }
        session
    }

    fn build_hibernated(
        id: String,
        name: String,
        cwd: PathBuf,
        argv: Vec<String>,
        config: Arc<Config>,
        auto_resume: bool,
        claude_session_id: Option<String>,
        created_at_ms: u64,
    ) -> Arc<Self> {
        let (in_tx, in_rx) = mpsc::channel::<Bytes>(IN_QUEUE_CAP);
        let (out_tx, _) = broadcast::channel::<Bytes>(OUT_BROADCAST_CAP);

        let pty_out = Arc::new(PtyOutput {
            ring: Mutex::new(RingBuffer::new(config.ring_cap_bytes)),
            tx: out_tx,
        });
        let size_table = Arc::new(SizeTable {
            master_handle: Arc::new(Mutex::new(None)),
            sizes: Mutex::new(HashMap::new()),
            last_applied: Mutex::new(INITIAL_SIZE),
        });

        let session = Arc::new_cyclic(|weak: &Weak<Session>| Session {
            id,
            name,
            cwd,
            argv,
            config,
            created_at_ms,
            pty_out,
            size_table,
            input_tx: in_tx,
            attached: Mutex::new(HashMap::new()),
            state: Mutex::new(SessionState::Hibernated),
            claude_session_id: Mutex::new(claude_session_id),
            auto_resume: Mutex::new(auto_resume),
            last_activity: Mutex::new(Instant::now()),
            inner: Mutex::new(SessionInner {
                writer: None,
                child: None,
            }),
            self_weak: weak.clone(),
        });

        session.spawn_input_drain(in_rx);
        session
    }

    fn this(&self) -> Arc<Self> {
        self.self_weak
            .upgrade()
            .expect("session arc dropped while a method was running")
    }

    fn spawn_input_drain(self: &Arc<Self>, mut rx: mpsc::Receiver<Bytes>) {
        let s = self.clone();
        tokio::spawn(async move {
            while let Some(data) = rx.recv().await {
                s.write_to_pty(&data);
            }
        });
    }

    fn start_pty(&self, use_resume: bool) -> Result<()> {
        let mut effective_argv = self.argv.clone();
        if use_resume {
            if let Some(id) = self.claude_session_id.lock().unwrap().clone() {
                effective_argv.push("--resume".to_string());
                effective_argv.push(id);
            }
        }

        let pty_system = NativePtySystem::default();
        let size = self.size_table.last();
        let pair = pty_system.openpty(size).context("openpty")?;

        let mut cmd = CommandBuilder::new(&effective_argv[0]);
        for a in &effective_argv[1..] {
            cmd.arg(a);
        }
        cmd.cwd(&self.cwd);
        cmd.env("AGENT_SESSION_ID", &self.id);
        cmd.env("AGENT_BROKER_URL", self.config.http_url());
        // Trusted roots for hook-pretool's path-based auto-allow on
        // Edit/Write — the session's own cwd is implicitly safe to
        // edit. Future enhancement: add config.toml override allowing
        // additional roots system-wide.
        cmd.env(
            "AGENT_TRUSTED_ROOTS",
            self.cwd.to_string_lossy().to_string(),
        );
        // Tool-approval gate. "off" (default) lets hook-pretool
        // short-circuit; "ask" re-enables the classifier + Discord/tray
        // round-trip. Default-off as of 0.3.4.
        cmd.env("AGENT_TOOL_APPROVAL", self.config.tool_approval.as_env_str());

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn {:?}", effective_argv))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("clone pty reader")?;
        let writer = pair.master.take_writer().context("take pty writer")?;

        *self.size_table.master_handle.lock().unwrap() = Some(pair.master);
        {
            let mut g = self.inner.lock().unwrap();
            g.writer = Some(writer);
            g.child = Some(child);
        }
        self.size_table
            .recompute(&self.size_table.sizes.lock().unwrap());

        let session_for_reader = self.this();
        let sid = self.id.clone();
        let sname = self.name.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; PTY_READ_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // Note: last_activity is *not* touched here. claude
                        // typically emits periodic TUI updates (status
                        // line, cursor refresh) even when the user is
                        // away — letting those reset the idle timer
                        // would never let the session hibernate. We
                        // only mark activity for actual user input
                        // (write_to_pty), viewer attach, and resume.
                        let bytes = Bytes::copy_from_slice(&buf[..n]);
                        let mut ring = session_for_reader.pty_out.ring.lock().unwrap();
                        ring.append(&bytes);
                        let _ = session_for_reader.pty_out.tx.send(bytes);
                    }
                    Err(e) => {
                        error!("session {sid} ({sname}) pty read: {e}");
                        break;
                    }
                }
            }
            info!("session {sid} ({sname}) pty reader exited");
        });

        self.spawn_crash_watcher();

        info!(
            "session {} ({}) started: cmd={:?} cwd={:?}",
            self.id, self.name, effective_argv, self.cwd
        );
        Ok(())
    }

    /// Polls `try_wait()` on the current child once a second. If the
    /// child exits while state is still Idle, the session is marked
    /// Crashed (intentional exits set state→Hibernated/Idle *before*
    /// taking the child, so this branch is only reached on real
    /// claude crashes). Watcher exits as soon as the inner child slot
    /// goes None — meaning hibernate/restart/shutdown got there first.
    fn spawn_crash_watcher(&self) {
        let this = self.this();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let exited = {
                    let mut g = this.inner.lock().unwrap();
                    match g.child.as_mut() {
                        Some(c) => match c.try_wait() {
                            Ok(Some(_)) => {
                                let _ = g.child.take();
                                true
                            }
                            _ => false,
                        },
                        None => return,
                    }
                };
                if exited {
                    let cur = this.state();
                    if cur == SessionState::Idle {
                        *this.state.lock().unwrap() = SessionState::Crashed;
                        warn!(
                            "session {} ({}) crashed unexpectedly",
                            this.id, this.name
                        );
                    }
                    return;
                }
            }
        });
    }

    pub fn write_to_pty(&self, data: &[u8]) {
        *self.last_activity.lock().unwrap() = Instant::now();
        let mut g = self.inner.lock().unwrap();
        if let Some(w) = g.writer.as_mut() {
            if let Err(e) = w.write_all(data) {
                warn!("session {} pty write: {e}", self.id);
                return;
            }
            let _ = w.flush();
        }
    }

    pub fn last_activity_age(&self) -> Duration {
        self.last_activity.lock().unwrap().elapsed()
    }

    pub fn touch_activity(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    pub fn interrupt(&self) {
        info!("session {}: interrupt", self.id);
        self.write_to_pty(&[0x03]);
    }

    /// Outcome of a successful demote. Surfaced to the CLI so it can
    /// print the exact `claude --resume <id>` command the user should
    /// run in their terminal, and warn when the graceful `/exit`
    /// timed out (transcript may be missing the last few lines).
    pub fn demote(&self) -> Result<DemoteOutcome> {
        info!("session {} ({}): demote starting", self.id, self.name);

        // Step 1: graceful — type "/exit\r" into claude's TUI. claude
        // treats Ctrl+C (0x03) as "interrupt the current turn", NOT
        // exit, so we use the TUI's own /exit command which lets
        // claude flush its transcript before quitting.
        self.write_to_pty(b"/exit\r");

        // Step 2: wait up to 2s for the child to actually exit.
        let graceful = self.wait_for_child_exit(Duration::from_millis(2000));

        // Step 3: if still alive, escalate to TerminateProcess.
        if !graceful {
            warn!(
                "session {} ({}): /exit graceful window timed out, killing",
                self.id, self.name
            );
            let mut g = self.inner.lock().unwrap();
            if let Some(c) = g.child.as_mut() {
                if let Err(e) = c.kill() {
                    warn!("session {}: kill: {e}", self.id);
                }
            }
        }

        // Step 4: another wait window for the kill to take effect.
        // If we *still* see a live child at the end of this, we
        // refuse to transition state — handing back a "demoted"
        // status while a broker-owned claude is still running would
        // race the user's local `claude --resume <id>` and corrupt
        // the transcript, which is the whole reason demote exists.
        let killed = if graceful {
            true
        } else {
            self.wait_for_child_exit(Duration::from_millis(1000))
        };
        if !killed {
            anyhow::bail!(
                "claude failed to exit within graceful + kill windows; \
                 refusing to mark session locally-owned. \
                 Investigate via Task Manager (PID lookup in broker.log). \
                 Session left in current state."
            );
        }

        // Now safe to drop resources and flip state. The crash watcher
        // may have already taken the child slot and tried to mark
        // state=Crashed; either way we overwrite to LocallyOwned next.
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(mut c) = g.child.take() {
                let _ = c.wait();
            }
            g.writer = None;
        }
        *self.size_table.master_handle.lock().unwrap() = None;
        // Clear the ring so a future re-adopt + attach replays only
        // the new claude's output, not stale pre-demote frames.
        self.pty_out.ring.lock().unwrap().clear();
        *self.state.lock().unwrap() = SessionState::LocallyOwned;

        let claude_id = self.claude_session_id();
        info!(
            "session {} ({}) demoted (graceful={}, claude_session_id={:?})",
            self.id, self.name, graceful, claude_id
        );
        Ok(DemoteOutcome {
            claude_session_id: claude_id,
            cwd: self.cwd.to_string_lossy().to_string(),
            graceful,
        })
    }

    /// Polls `try_wait()` on the child every 100ms until it exits or
    /// the deadline passes. Returns true on observed exit (including
    /// "child slot already None" — crash watcher beat us). Releases
    /// the inner mutex between polls so other threads (the crash
    /// watcher, hooks) can make progress.
    fn wait_for_child_exit(&self, max_wait: Duration) -> bool {
        let start = Instant::now();
        loop {
            let exited = {
                let mut g = self.inner.lock().unwrap();
                match g.child.as_mut() {
                    Some(c) => matches!(c.try_wait(), Ok(Some(_)) | Err(_)),
                    None => true,
                }
            };
            if exited {
                return true;
            }
            if start.elapsed() >= max_wait {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Kill claude, drop the PTY, and mark Hibernated. The ring is
    /// cleared so the next attach doesn't replay stale frames before
    /// the resumed claude paints. Metadata (id/name/cwd/claude id)
    /// stays so `resume()` can `--resume` straight back into the
    /// conversation.
    pub fn hibernate(&self) {
        info!("session {}: hibernate", self.id);
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(mut c) = g.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            g.writer = None;
        }
        *self.size_table.master_handle.lock().unwrap() = None;
        self.pty_out.ring.lock().unwrap().clear();
        *self.state.lock().unwrap() = SessionState::Hibernated;
    }

    /// Spawn a fresh claude attached to the same conversation via
    /// `--resume <claude_session_id>`. If we never captured the id
    /// (session was hibernated before any turn completed), we fall
    /// back to a fresh claude — context is lost but it doesn't crash.
    pub fn resume(&self) -> Result<()> {
        info!(
            "session {}: resume (claude_session_id={:?})",
            self.id,
            self.claude_session_id.lock().unwrap()
        );
        // Reset the idle timer: a freshly-resumed session shouldn't
        // immediately re-hibernate just because its previous
        // last_activity timestamp is stale.
        self.touch_activity();
        self.start_pty(true)?;
        *self.state.lock().unwrap() = SessionState::Idle;
        Ok(())
    }

    pub fn restart(&self) -> Result<()> {
        info!("session {}: restart", self.id);
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(mut c) = g.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            g.writer = None;
        }
        *self.size_table.master_handle.lock().unwrap() = None;
        // Restart preserves continuity per PLAN §3.2: same claude
        // session id, just a fresh process.
        self.start_pty(true)?;
        *self.state.lock().unwrap() = SessionState::Idle;
        Ok(())
    }

    pub fn shutdown(&self) {
        info!("session {}: shutdown", self.id);
        let mut g = self.inner.lock().unwrap();
        if let Some(mut c) = g.child.take() {
            let _ = c.kill();
        }
    }

    pub fn state(&self) -> SessionState {
        *self.state.lock().unwrap()
    }

    pub fn claude_session_id(&self) -> Option<String> {
        self.claude_session_id.lock().unwrap().clone()
    }

    pub fn auto_resume(&self) -> bool {
        *self.auto_resume.lock().unwrap()
    }

    /// Returns true iff the value changed — caller passes that up to
    /// decide whether to call `manager.save()` (writing sessions.toml
    /// is cheap but pointless if nothing actually moved).
    pub fn set_auto_resume(&self, v: bool) -> bool {
        let mut g = self.auto_resume.lock().unwrap();
        if *g == v {
            return false;
        }
        *g = v;
        true
    }

    /// Returns true iff the stored id changed (caller may want to
    /// trigger persistence).
    pub fn set_claude_session_id_if_changed(&self, id: String) -> bool {
        let mut g = self.claude_session_id.lock().unwrap();
        if g.as_deref() == Some(id.as_str()) {
            return false;
        }
        *g = Some(id);
        true
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            cwd: self.cwd.to_string_lossy().to_string(),
            argv: self.argv.clone(),
            created_at_ms: self.created_at_ms,
            viewers: self.attached.lock().unwrap().len(),
            state: self.state(),
            auto_resume: self.auto_resume(),
            claude_session_id: self.claude_session_id(),
        }
    }

    pub fn persisted(&self) -> PersistedSession {
        PersistedSession {
            id: self.id.clone(),
            name: self.name.clone(),
            cwd: self.cwd.to_string_lossy().to_string(),
            argv: self.argv.clone(),
            claude_session_id: self.claude_session_id(),
            auto_resume: self.auto_resume(),
            created_at_ms: self.created_at_ms,
            locally_owned: self.state() == SessionState::LocallyOwned,
        }
    }

    pub fn register_viewer(&self, info: ClientInfo) {
        self.attached.lock().unwrap().insert(info.viewer_id, info);
    }

    pub fn deregister_viewer(&self, viewer_id: u64) {
        self.attached.lock().unwrap().remove(&viewer_id);
    }

    pub fn attached_clients(&self) -> Vec<ClientInfo> {
        let g = self.attached.lock().unwrap();
        let mut v: Vec<ClientInfo> = g.values().cloned().collect();
        v.sort_by_key(|c| c.viewer_id);
        v
    }
}
