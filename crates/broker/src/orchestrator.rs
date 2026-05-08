//! Cross-session task dispatch — the "main agent / worker" primitive.
//!
//! Any session can `POST /sessions/:caller/dispatch { to, prompt, tag }`.
//! Broker registers a [`PendingCallback`] keyed by the *target* session,
//! sends the prompt to that target via the existing input path, and
//! returns a `task_id` immediately.
//!
//! When the target finishes its turn (broker observes its
//! `assistant_message` event), the front-of-queue callback for that
//! target is consumed and broker injects a synthetic
//! `[SYSTEM: task-complete]` block into the *caller* session — waking
//! the caller's claude with the worker's reply as context.
//!
//! Persistence: the queue is mirrored to `dispatches.toml` next to
//! `sessions.toml` on every push/pop, so a broker crash mid-orchestration
//! doesn't leave the caller waiting forever for callbacks the broker
//! has already forgotten.
//!
//! This module is broker-internal — sessions don't have to be flagged as
//! "boss" or "worker"; *any* session can be on either side of a
//! dispatch. The orchestrator role is purely a system-prompt
//! convention (see `docs/orchestrator-prompt.md`).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// One outstanding cross-session task. Stored under the *target*
/// session's id so consuming on the target's `assistant_message` is a
/// front-of-queue pop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCallback {
    pub task_id: String,
    /// Session that issued the dispatch — receives the
    /// `[SYSTEM: task-complete]` injection on completion.
    pub caller_session_id: String,
    /// Session running the task — its next `assistant_message` resolves
    /// this callback.
    pub target_session_id: String,
    /// Caller-supplied label, echoed back in the callback so the caller
    /// can correlate without remembering the task_id (which it likely
    /// won't — context compaction).
    pub tag: String,
    /// Original prompt; included verbatim in the callback because the
    /// caller may have lost it from context by the time the worker
    /// finishes.
    pub original_prompt: String,
    /// Wall-clock dispatched time; used for timeout pruning. ms since
    /// UNIX epoch so the value survives the toml round-trip and a
    /// process restart.
    pub dispatched_at_unix_ms: u64,
    /// Per-task deadline. Broker auto-injects a `[SYSTEM: task-timeout]`
    /// to the caller when wall-clock exceeds `dispatched_at + timeout`.
    pub timeout_secs: u64,
}

impl PendingCallback {
    pub fn deadline_unix_ms(&self) -> u64 {
        self.dispatched_at_unix_ms
            .saturating_add(self.timeout_secs.saturating_mul(1000))
    }

    pub fn is_expired(&self, now_unix_ms: u64) -> bool {
        now_unix_ms >= self.deadline_unix_ms()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedDispatches {
    #[serde(default)]
    callbacks: Vec<PendingCallback>,
}

/// Owns the in-memory queue + the persistence file.
///
/// Concurrency: `std::sync::Mutex` is fine here — every critical
/// section is a small map operation followed by a `save_blocking`
/// **outside** the lock. We never hold the lock across `.await`.
pub struct OrchestratorState {
    /// `target_session_id -> FIFO of pending callbacks targeting it`.
    queues: Mutex<HashMap<String, VecDeque<PendingCallback>>>,
    /// Empty path = persistence disabled (tests).
    file: PathBuf,
}

impl OrchestratorState {
    pub fn new(file: PathBuf) -> Self {
        let queues = if file.as_os_str().is_empty() || !file.exists() {
            HashMap::new()
        } else {
            load_dispatches(&file)
        };
        Self {
            queues: Mutex::new(queues),
            file,
        }
    }

    /// Number of in-flight callbacks initiated by `caller_session_id`.
    /// Used by the dispatch endpoint to enforce the per-caller cap.
    pub fn count_for_caller(&self, caller_session_id: &str) -> usize {
        let g = self.queues.lock().unwrap();
        g.values()
            .flat_map(|q| q.iter())
            .filter(|c| c.caller_session_id == caller_session_id)
            .count()
    }

    /// Register a new outstanding callback, then mirror to disk.
    pub fn push(&self, cb: PendingCallback) {
        let snapshot = {
            let mut g = self.queues.lock().unwrap();
            g.entry(cb.target_session_id.clone())
                .or_default()
                .push_back(cb);
            snapshot(&g)
        };
        self.persist(snapshot);
    }

    /// Pop the front-of-queue callback for `target_session_id` (the one
    /// matching the target's most recent assistant_message). Returns
    /// `None` if no dispatch is pending against that session.
    pub fn pop_for_target(&self, target_session_id: &str) -> Option<PendingCallback> {
        let (popped, snapshot) = {
            let mut g = self.queues.lock().unwrap();
            let q = g.get_mut(target_session_id)?;
            let popped = q.pop_front();
            if q.is_empty() {
                g.remove(target_session_id);
            }
            (popped, snapshot(&g))
        };
        if popped.is_some() {
            self.persist(snapshot);
        }
        popped
    }

    /// Sweep expired callbacks. Returns the popped entries so the
    /// caller can inject `[SYSTEM: task-timeout]` for each.
    pub fn drain_expired(&self, now_unix_ms: u64) -> Vec<PendingCallback> {
        let (expired, snapshot) = {
            let mut g = self.queues.lock().unwrap();
            let mut out = Vec::new();
            // Drain from each queue's front (FIFO ordering matches
            // pop_for_target so a real assistant_message can't sneak
            // past an expired callback ahead of it).
            let keys: Vec<String> = g.keys().cloned().collect();
            for k in keys {
                if let Some(q) = g.get_mut(&k) {
                    while q.front().is_some_and(|c| c.is_expired(now_unix_ms)) {
                        if let Some(cb) = q.pop_front() {
                            out.push(cb);
                        }
                    }
                    if q.is_empty() {
                        g.remove(&k);
                    }
                }
            }
            (out, snapshot(&g))
        };
        if !expired.is_empty() {
            self.persist(snapshot);
        }
        expired
    }

    fn persist(&self, snapshot: Vec<PendingCallback>) {
        if self.file.as_os_str().is_empty() {
            return;
        }
        if snapshot.is_empty() {
            // Empty queue → remove the file so next-startup load is fast.
            if self.file.exists() {
                let _ = std::fs::remove_file(&self.file);
            }
            return;
        }
        let body = match toml::to_string_pretty(&PersistedDispatches { callbacks: snapshot }) {
            Ok(s) => s,
            Err(e) => {
                warn!("serialise dispatches: {e}");
                return;
            }
        };
        if let Some(parent) = self.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.file.with_extension("toml.tmp");
        if let Err(e) = std::fs::write(&tmp, body) {
            warn!("write {}: {e}", tmp.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.file) {
            warn!("rename to {}: {e}", self.file.display());
        }
    }
}

fn snapshot(map: &HashMap<String, VecDeque<PendingCallback>>) -> Vec<PendingCallback> {
    map.values().flat_map(|q| q.iter().cloned()).collect()
}

fn load_dispatches(path: &PathBuf) -> HashMap<String, VecDeque<PendingCallback>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            warn!("read {}: {e}", path.display());
            return HashMap::new();
        }
    };
    let parsed: PersistedDispatches = match toml::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            warn!("parse {}: {e}", path.display());
            return HashMap::new();
        }
    };
    let mut out: HashMap<String, VecDeque<PendingCallback>> = HashMap::new();
    for cb in parsed.callbacks {
        out.entry(cb.target_session_id.clone())
            .or_default()
            .push_back(cb);
    }
    out
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Format a `[SYSTEM: task-complete]` block for injection into the
/// caller's input stream. Caller's system prompt teaches claude to
/// recognise these tags (see docs/orchestrator-prompt.md).
pub fn format_task_complete(cb: &PendingCallback, target_name: &str, reply_body: &str) -> String {
    format!(
        "[SYSTEM: task-complete]\n\
         tag: {tag}\n\
         worker: {worker}\n\
         original_prompt:\n\
         {prompt}\n\
         result:\n\
         {result}\n\
         [/SYSTEM]",
        tag = cb.tag,
        worker = target_name,
        prompt = cb.original_prompt,
        result = reply_body,
    )
}

/// Format a `[SYSTEM: task-timeout]` block — sent when broker's
/// timeout scanner fires before the worker emits an assistant_message.
pub fn format_task_timeout(cb: &PendingCallback, target_name: &str) -> String {
    format!(
        "[SYSTEM: task-timeout]\n\
         tag: {tag}\n\
         worker: {worker}\n\
         elapsed_secs: {elapsed}\n\
         original_prompt:\n\
         {prompt}\n\
         [/SYSTEM]",
        tag = cb.tag,
        worker = target_name,
        elapsed = cb.timeout_secs,
        prompt = cb.original_prompt,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cb(target: &str, caller: &str, tag: &str) -> PendingCallback {
        PendingCallback {
            task_id: format!("task-{tag}"),
            caller_session_id: caller.into(),
            target_session_id: target.into(),
            tag: tag.into(),
            original_prompt: "p".into(),
            dispatched_at_unix_ms: now_unix_ms(),
            timeout_secs: 60,
        }
    }

    #[test]
    fn push_pop_fifo_per_target() {
        let s = OrchestratorState::new(PathBuf::new());
        s.push(cb("w1", "boss", "a"));
        s.push(cb("w1", "boss", "b"));
        s.push(cb("w2", "boss", "c"));

        let first = s.pop_for_target("w1").unwrap();
        assert_eq!(first.tag, "a");
        let second = s.pop_for_target("w1").unwrap();
        assert_eq!(second.tag, "b");
        assert!(s.pop_for_target("w1").is_none(), "queue drained");

        let other = s.pop_for_target("w2").unwrap();
        assert_eq!(other.tag, "c");
    }

    #[test]
    fn count_for_caller_only_counts_caller() {
        let s = OrchestratorState::new(PathBuf::new());
        s.push(cb("w1", "boss", "a"));
        s.push(cb("w2", "boss", "b"));
        s.push(cb("w3", "other", "c"));

        assert_eq!(s.count_for_caller("boss"), 2);
        assert_eq!(s.count_for_caller("other"), 1);
        assert_eq!(s.count_for_caller("nobody"), 0);
    }

    #[test]
    fn drain_expired_pops_only_past_deadline() {
        let s = OrchestratorState::new(PathBuf::new());
        let mut a = cb("w1", "boss", "a");
        a.dispatched_at_unix_ms = 1000;
        a.timeout_secs = 10; // deadline = 11000
        let mut b = cb("w1", "boss", "b");
        b.dispatched_at_unix_ms = 5000;
        b.timeout_secs = 100; // deadline = 105000
        s.push(a);
        s.push(b);

        // now = 12000 → only `a` is expired (front of queue).
        let expired = s.drain_expired(12_000);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].tag, "a");

        // `b` still pending.
        let remaining = s.pop_for_target("w1").unwrap();
        assert_eq!(remaining.tag, "b");
    }

    #[test]
    fn drain_expired_stops_at_first_unexpired() {
        // FIFO invariant: don't skip past a non-expired front entry.
        let s = OrchestratorState::new(PathBuf::new());
        let mut alive = cb("w1", "boss", "alive");
        alive.dispatched_at_unix_ms = 5000;
        alive.timeout_secs = 100;
        let mut dead = cb("w1", "boss", "dead");
        dead.dispatched_at_unix_ms = 1000;
        dead.timeout_secs = 5; // deadline 6000, now=12000 → expired
        s.push(alive); // front
        s.push(dead); // tail (despite being older, it's behind)

        let expired = s.drain_expired(12_000);
        assert!(
            expired.is_empty(),
            "front not expired → must not pop tail past it"
        );
    }

    #[test]
    fn format_callback_renders_all_fields() {
        let c = cb("w1", "boss", "research");
        let s = format_task_complete(&c, "research-worker", "found 5 results");
        assert!(s.contains("tag: research"));
        assert!(s.contains("worker: research-worker"));
        assert!(s.contains("found 5 results"));
        assert!(s.contains("[/SYSTEM]"));
    }
}
