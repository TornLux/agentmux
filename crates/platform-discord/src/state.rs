//! Mutable bot state shared between the gateway handler and the
//! WS-relay task in `main.rs`. Four pieces, all behind tokio `Mutex`
//! because they are read and written from independent tasks:
//!
//!  * **channel_bindings** — `channel_id -> session_name`. Each
//!    channel remembers which session it is talking to. First-time
//!    access lazily binds the channel to `default_session`. **Persisted
//!    to `discord-bindings.toml`** so a bot restart doesn't reset the
//!    per-channel topology.
//!  * **pending_replies** — `session_name -> FIFO<PendingReply>`.
//!    When a user message is forwarded we post a placeholder Discord
//!    message and push its id; the WS relay pops the oldest entry on
//!    `assistant_message` and edits the placeholder in place. Falls
//!    back to a fresh post if the queue is empty (e.g. bot restart
//!    between forward and reply, or unsolicited claude turn).
//!  * **typing_cancels** — `placeholder_msg_id -> cancel_flag`. The
//!    typing-indicator background task watches this flag; the moment
//!    a placeholder gets edited (assistant_message landed) we set the
//!    flag and the typing loop exits.
//!  * **recent_messages** — `discord_msg_id -> session_name`, bounded
//!    LRU. Used by reply-thread routing: when an inbound message has
//!    a `message_reference` pointing to a known assistant message,
//!    the forward overrides the channel binding for that one turn.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::warn;

/// Cap on `recent_messages`. Oldest entries are evicted FIFO once the
/// map reaches this many records — far more than you'd ever scroll
/// back to reply against.
const RECENT_MESSAGES_CAP: usize = 1024;

#[derive(Debug, Clone)]
pub struct PendingReply {
    pub channel_id: u64,
    pub message_id: u64,
    /// Wall-clock unix-ms drop-after timestamp. Wall-clock (not
    /// `Instant`) so the same value can survive a process restart in
    /// `discord-pending.toml`. Past-deadline entries are dropped at
    /// `pop_pending`.
    pub deadline_unix_ms: u64,
    /// Flipped to `true` when the placeholder is consumed (edited or
    /// abandoned). The typing-indicator task watches this flag and
    /// exits as soon as it's set. Shared between the pending entry
    /// and the typing task so neither has to know about the other.
    pub typing_cancel: Arc<AtomicBool>,
    /// Per-placeholder progress narrative — a list of "✏️ editing
    /// src/x.rs", "🖥 cargo test" … strings, one per PostToolUse hook
    /// firing during this turn. The WS relay edits the placeholder
    /// content from this list (throttled). On `assistant_message` the
    /// pop_pending consumer drops the entry and the final answer
    /// replaces the progress lines wholesale. Std mutex is fine: every
    /// critical section is a push/clone with no `.await` inside.
    pub progress: Arc<StdMutex<ProgressState>>,
}

#[derive(Debug, Default)]
pub struct ProgressState {
    pub history: Vec<String>,
    pub last_edit_at: Option<Instant>,
}

impl PendingReply {
    /// Build a fresh `PendingReply`. `progress` is initialised empty.
    pub fn new(
        channel_id: u64,
        message_id: u64,
        deadline_unix_ms: u64,
        typing_cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            channel_id,
            message_id,
            deadline_unix_ms,
            typing_cancel,
            progress: Arc::new(StdMutex::new(ProgressState::default())),
        }
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedBindings {
    /// Map keyed by stringified channel_id (TOML doesn't accept
    /// integer table keys).
    #[serde(default)]
    bindings: HashMap<String, String>,
}

/// On-disk shape for one outstanding placeholder. Recovered on
/// startup so the user doesn't see eternal `💭 working…` after a
/// bot crash. Wall-clock deadline (Instant doesn't survive reboot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanedPending {
    pub session: String,
    pub channel_id: u64,
    pub message_id: u64,
    /// Wall-clock unix-ms; `Instant` is monotonic so it can't survive
    /// a process restart. We re-derive an Instant on load by diffing
    /// against the current wall-clock.
    pub deadline_unix_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedPending {
    #[serde(default)]
    pending: Vec<OrphanedPending>,
}

pub struct BotState {
    channel_bindings: Mutex<HashMap<u64, String>>,
    pending_replies: Mutex<HashMap<String, VecDeque<PendingReply>>>,
    /// FIFO queue of (msg_id, session_name) so we can evict the oldest
    /// entry when capacity is reached. The HashMap mirrors it for O(1)
    /// lookup.
    recent_messages: Mutex<RecentMessages>,
    /// Path of `discord-bindings.toml`. Empty `PathBuf` → persistence
    /// disabled (used in tests).
    state_file: PathBuf,
    /// Path of `discord-pending.toml` — outstanding placeholder
    /// records, persisted on push/pop so a crashed bot can clean up
    /// orphaned `💭 working…` messages on next start.
    pending_file: PathBuf,
}

#[derive(Default)]
struct RecentMessages {
    map: HashMap<u64, String>,
    order: VecDeque<u64>,
}

impl RecentMessages {
    fn record(&mut self, msg_id: u64, session: String) {
        if self.map.insert(msg_id, session).is_none() {
            self.order.push_back(msg_id);
            while self.order.len() > RECENT_MESSAGES_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
    fn lookup(&self, msg_id: u64) -> Option<String> {
        self.map.get(&msg_id).cloned()
    }
}

impl BotState {
    pub fn new(state_file: PathBuf, pending_file: PathBuf) -> Arc<Self> {
        let mut bindings = HashMap::new();
        if !state_file.as_os_str().is_empty() && state_file.exists() {
            match std::fs::read_to_string(&state_file) {
                Ok(s) => match toml::from_str::<PersistedBindings>(&s) {
                    Ok(p) => {
                        for (k, v) in p.bindings {
                            if let Ok(id) = k.parse::<u64>() {
                                bindings.insert(id, v);
                            }
                        }
                        if !bindings.is_empty() {
                            tracing::info!(
                                "loaded {} channel binding(s) from {}",
                                bindings.len(),
                                state_file.display()
                            );
                        }
                    }
                    Err(e) => warn!("parse {}: {e}", state_file.display()),
                },
                Err(e) => warn!("read {}: {e}", state_file.display()),
            }
        }
        Arc::new(Self {
            channel_bindings: Mutex::new(bindings),
            pending_replies: Mutex::new(HashMap::new()),
            recent_messages: Mutex::new(RecentMessages::default()),
            state_file,
            pending_file,
        })
    }

    /// Read and **delete** any persisted pending records. Called once
    /// at startup so the bot can edit each orphaned placeholder
    /// (e.g. `💭 working…` from a crash) into a clean error state.
    /// Returns an empty Vec if the file is missing or unreadable.
    pub fn take_orphans(&self) -> Vec<OrphanedPending> {
        if self.pending_file.as_os_str().is_empty() {
            return Vec::new();
        }
        if !self.pending_file.exists() {
            return Vec::new();
        }
        let raw = match std::fs::read_to_string(&self.pending_file) {
            Ok(s) => s,
            Err(e) => {
                warn!("read {}: {e}", self.pending_file.display());
                return Vec::new();
            }
        };
        let parsed = match toml::from_str::<PersistedPending>(&raw) {
            Ok(p) => p,
            Err(e) => {
                warn!("parse {}: {e}", self.pending_file.display());
                return Vec::new();
            }
        };
        // Whether or not we successfully edit the placeholders later,
        // the file is meant to be one-shot — the records describe a
        // PREVIOUS process's pending. Clear it so we don't reprocess
        // on any subsequent restart.
        if let Err(e) = std::fs::remove_file(&self.pending_file) {
            warn!("remove {}: {e}", self.pending_file.display());
        }
        parsed.pending
    }

    /// Get-or-bind: returns the session name for `channel_id`, lazily
    /// inserting `default_session` if this channel hasn't been seen.
    /// Lazy binds are persisted to disk.
    pub async fn resolve_or_bind(&self, channel_id: u64, default_session: &str) -> String {
        let mut map = self.channel_bindings.lock().await;
        let inserted = !map.contains_key(&channel_id);
        let name = map
            .entry(channel_id)
            .or_insert_with(|| default_session.to_string())
            .clone();
        let snapshot = if inserted { Some(map.clone()) } else { None };
        drop(map);
        if let Some(s) = snapshot {
            self.save_bindings(&s).await;
        }
        name
    }

    pub async fn bind(&self, channel_id: u64, session: String) {
        let mut map = self.channel_bindings.lock().await;
        map.insert(channel_id, session);
        let snapshot = map.clone();
        drop(map);
        self.save_bindings(&snapshot).await;
    }

    /// All channels currently bound to `session` (may be empty).
    pub async fn channels_for(&self, session: &str) -> Vec<u64> {
        self.channel_bindings
            .lock()
            .await
            .iter()
            .filter_map(|(c, s)| if s == session { Some(*c) } else { None })
            .collect()
    }

    /// Snapshot of all bindings, for `!ls` / `/ls` rendering.
    pub async fn bindings_snapshot(&self) -> Vec<(u64, String)> {
        self.channel_bindings
            .lock()
            .await
            .iter()
            .map(|(c, s)| (*c, s.clone()))
            .collect()
    }

    /// Forget all bindings to `session` — call when a session is
    /// killed so stale channels don't keep posting into the void.
    pub async fn unbind_all(&self, session: &str) {
        let mut map = self.channel_bindings.lock().await;
        let before = map.len();
        map.retain(|_, s| s != session);
        if map.len() != before {
            let snapshot = map.clone();
            drop(map);
            self.save_bindings(&snapshot).await;
        }
    }

    pub async fn push_pending(&self, session: &str, reply: PendingReply) {
        let snapshot = {
            let mut map = self.pending_replies.lock().await;
            map.entry(session.to_string())
                .or_default()
                .push_back(reply);
            snapshot_pending(&map)
        };
        self.save_pending(snapshot).await;
    }

    /// Clone the oldest non-expired placeholder for `session` without
    /// removing it. Used by `tool_progress` events: they want to update
    /// the in-flight placeholder's content, not consume it. Expired
    /// entries at the front are discarded as a side effect, mirroring
    /// `pop_pending` so the queue's head invariant ("front is current")
    /// holds for both readers.
    pub async fn peek_pending(&self, session: &str) -> Option<PendingReply> {
        let mut map = self.pending_replies.lock().await;
        let queue = map.get_mut(session)?;
        let now_ms = now_unix_ms();
        while let Some(front) = queue.front() {
            if front.deadline_unix_ms < now_ms {
                if let Some(expired) = queue.pop_front() {
                    expired.typing_cancel.store(true, Ordering::Release);
                }
            } else {
                return queue.front().cloned();
            }
        }
        None
    }

    /// Pop the oldest non-expired placeholder for `session`. Expired
    /// entries at the front are discarded silently. Caller MUST flip
    /// the returned PendingReply's `typing_cancel` flag once the
    /// placeholder has been finalised so the typing-indicator task
    /// exits.
    pub async fn pop_pending(&self, session: &str) -> Option<PendingReply> {
        let (returned, snapshot) = {
            let mut map = self.pending_replies.lock().await;
            let now_ms = now_unix_ms();
            let returned = (|| {
                let queue = map.get_mut(session)?;
                while let Some(front) = queue.front() {
                    if front.deadline_unix_ms < now_ms {
                        if let Some(expired) = queue.pop_front() {
                            expired.typing_cancel.store(true, Ordering::Release);
                        }
                    } else {
                        return queue.pop_front();
                    }
                }
                None
            })();
            (returned, snapshot_pending(&map))
        };
        // Persist after every state change — push and pop both
        // narrow the orphan window equally.
        self.save_pending(snapshot).await;
        returned
    }

    /// Record an assistant Discord message id so a future user reply
    /// to it can be routed back to the original session.
    pub async fn record_assistant_message(&self, msg_id: u64, session: String) {
        self.recent_messages.lock().await.record(msg_id, session);
    }

    /// If `msg_id` was previously recorded, return the session it
    /// belonged to.
    pub async fn lookup_replied_session(&self, msg_id: u64) -> Option<String> {
        self.recent_messages.lock().await.lookup(msg_id)
    }

    async fn save_pending(&self, snapshot: Vec<OrphanedPending>) {
        if self.pending_file.as_os_str().is_empty() {
            return;
        }
        // Empty queue → remove the file entirely so a clean shutdown
        // doesn't trigger orphan cleanup on next start.
        if snapshot.is_empty() {
            let path = self.pending_file.clone();
            tokio::task::spawn_blocking(move || {
                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
            })
            .await
            .ok();
            return;
        }
        let body = match toml::to_string_pretty(&PersistedPending { pending: snapshot }) {
            Ok(s) => s,
            Err(e) => {
                warn!("serialise pending: {e}");
                return;
            }
        };
        let path = self.pending_file.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp = path.with_extension("toml.tmp");
            if let Err(e) = std::fs::write(&tmp, body) {
                warn!("write {}: {e}", tmp.display());
                return;
            }
            if let Err(e) = std::fs::rename(&tmp, &path) {
                warn!("rename to {}: {e}", path.display());
            }
        })
        .await
        .ok();
    }

    async fn save_bindings(&self, snapshot: &HashMap<u64, String>) {
        if self.state_file.as_os_str().is_empty() {
            return;
        }
        let mut p = PersistedBindings::default();
        for (c, s) in snapshot {
            p.bindings.insert(c.to_string(), s.clone());
        }
        let body = match toml::to_string_pretty(&p) {
            Ok(s) => s,
            Err(e) => {
                warn!("serialise bindings: {e}");
                return;
            }
        };
        let path = self.state_file.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp = path.with_extension("toml.tmp");
            if let Err(e) = std::fs::write(&tmp, body) {
                warn!("write {}: {e}", tmp.display());
                return;
            }
            if let Err(e) = std::fs::rename(&tmp, &path) {
                warn!("rename to {}: {e}", path.display());
            }
        })
        .await
        .ok();
    }
}

/// Build a serialisable snapshot of every queued PendingReply across
/// all sessions. Caller holds the pending_replies lock when invoking.
fn snapshot_pending(map: &HashMap<String, VecDeque<PendingReply>>) -> Vec<OrphanedPending> {
    let mut out = Vec::new();
    for (session, queue) in map {
        for p in queue {
            out.push(OrphanedPending {
                session: session.clone(),
                channel_id: p.channel_id,
                message_id: p.message_id,
                deadline_unix_ms: p.deadline_unix_ms,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_messages_evicts_oldest() {
        let mut r = RecentMessages::default();
        for i in 0..(RECENT_MESSAGES_CAP as u64 + 5) {
            r.record(i, format!("s{i}"));
        }
        assert!(r.lookup(0).is_none(), "first entry should be evicted");
        assert!(r.lookup(4).is_none(), "early entries should be evicted");
        assert!(r.lookup(RECENT_MESSAGES_CAP as u64 + 4).is_some());
    }

    #[test]
    fn recent_messages_dedupe_keeps_first() {
        let mut r = RecentMessages::default();
        r.record(1, "a".into());
        r.record(1, "b".into());
        // dedupe: same id should not push twice into order, but the
        // value gets overwritten — we keep latest mapping.
        assert_eq!(r.lookup(1).as_deref(), Some("b"));
        assert_eq!(r.order.len(), 1);
    }
}
