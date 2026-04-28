//! Mutable bot state shared between the gateway handler and the WS-relay
//! task in `main.rs`.
//!
//! Two pieces of state, both behind tokio `Mutex` because they are read
//! and written from independent tasks:
//!
//!  * **channel_bindings** — `channel_id -> session_name`. Each channel
//!    remembers which session it is talking to. First-time access in a
//!    channel lazily binds it to `default_session` so a brand-new
//!    Discord channel doesn't need a manual `!attach`.
//!  * **pending_replies** — `session_name -> FIFO<PendingReply>`. When
//!    a user message is forwarded to a session we post a placeholder
//!    Discord message and push a record here; the WS relay pops the
//!    oldest record on `assistant_message` and edits the placeholder
//!    in place. Falls back to a fresh post if the queue is empty (e.g.
//!    bot restart between forward and reply).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy)]
pub struct PendingReply {
    pub channel_id: u64,
    pub message_id: u64,
    /// Drop-after timestamp. If a placeholder sits past its deadline we
    /// stop trying to edit it — the user has long since given up too.
    pub deadline: Instant,
}

#[derive(Default)]
pub struct BotState {
    channel_bindings: Mutex<HashMap<u64, String>>,
    pending_replies: Mutex<HashMap<String, VecDeque<PendingReply>>>,
}

impl BotState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Get-or-bind: returns the session name for `channel_id`, lazily
    /// inserting `default_session` if this channel hasn't been seen.
    pub async fn resolve_or_bind(&self, channel_id: u64, default_session: &str) -> String {
        self.channel_bindings
            .lock()
            .await
            .entry(channel_id)
            .or_insert_with(|| default_session.to_string())
            .clone()
    }

    pub async fn bind(&self, channel_id: u64, session: String) {
        self.channel_bindings.lock().await.insert(channel_id, session);
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

    /// Snapshot of all bindings, for `!ls` rendering.
    pub async fn bindings_snapshot(&self) -> Vec<(u64, String)> {
        self.channel_bindings
            .lock()
            .await
            .iter()
            .map(|(c, s)| (*c, s.clone()))
            .collect()
    }

    /// Forget all bindings to `session` — call when a session is
    /// `!kill`ed so stale channels don't keep posting into the void.
    pub async fn unbind_all(&self, session: &str) {
        self.channel_bindings
            .lock()
            .await
            .retain(|_, s| s != session);
    }

    pub async fn push_pending(&self, session: &str, reply: PendingReply) {
        self.pending_replies
            .lock()
            .await
            .entry(session.to_string())
            .or_default()
            .push_back(reply);
    }

    /// Pop the oldest non-expired placeholder for `session`. Expired
    /// entries at the front are discarded silently.
    pub async fn pop_pending(&self, session: &str) -> Option<PendingReply> {
        let mut map = self.pending_replies.lock().await;
        let queue = map.get_mut(session)?;
        let now = Instant::now();
        while let Some(front) = queue.front() {
            if front.deadline < now {
                queue.pop_front();
            } else {
                return queue.pop_front();
            }
        }
        None
    }
}
