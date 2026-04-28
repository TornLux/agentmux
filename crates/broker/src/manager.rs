//! Multi-session lifecycle manager + sessions.toml persistence.
//!
//! Sessions are addressed by UUID id with a secondary name → id index.
//! On construction the manager reads `sessions.toml` (path from Config)
//! and reconstructs every entry as a Hibernated session — none of them
//! spawn claude until first attach (lazy resume).
//!
//! Mutating ops (create, remove, set_claude_session_id, hibernate,
//! resume) call `save()` after to keep the file in sync.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use shared::config::Config;
use tracing::{info, warn};

use crate::session::{PersistedSession, Session, SessionInfo};

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedRoot {
    #[serde(default)]
    sessions: Vec<PersistedSession>,
}

pub struct Manager {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    name_index: RwLock<HashMap<String, String>>,
    argv_template: Vec<String>,
    config: Arc<Config>,
    persist_path: PathBuf,
    save_lock: Mutex<()>,
}

impl Manager {
    pub fn new(config: Arc<Config>, argv_template: Vec<String>) -> Self {
        let persist_path = config.sessions_toml();
        let m = Self {
            sessions: RwLock::new(HashMap::new()),
            name_index: RwLock::new(HashMap::new()),
            argv_template,
            config,
            persist_path,
            save_lock: Mutex::new(()),
        };
        m.restore_from_disk();
        m
    }

    fn restore_from_disk(&self) {
        let path = &self.persist_path;
        if !path.exists() {
            return;
        }
        let content = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                warn!("sessions.toml read {:?}: {e}", path);
                return;
            }
        };
        let root: PersistedRoot = match toml::from_str(&content) {
            Ok(r) => r,
            Err(e) => {
                warn!("sessions.toml parse {:?}: {e}", path);
                return;
            }
        };
        let mut restored = 0usize;
        for entry in root.sessions {
            // Phase 7.7 only auto-restores entries flagged auto_resume.
            // Others are forgotten on broker restart by design — user
            // can opt in/out per-session.
            if !entry.auto_resume {
                continue;
            }
            let session = Session::from_persisted(entry, self.config.clone());
            let id = session.id.clone();
            let name = session.name.clone();
            self.sessions.write().unwrap().insert(id.clone(), session);
            self.name_index.write().unwrap().insert(name, id);
            restored += 1;
        }
        if restored > 0 {
            info!("restored {restored} session(s) from {:?}", path);
        }
    }

    pub fn save(&self) -> Result<()> {
        let _guard = self.save_lock.lock().unwrap();
        let persisted: Vec<PersistedSession> = self
            .sessions
            .read()
            .unwrap()
            .values()
            .map(|s| s.persisted())
            .collect();
        let root = PersistedRoot {
            sessions: persisted,
        };
        let content = toml::to_string_pretty(&root).context("serialise sessions.toml")?;
        if let Some(parent) = self.persist_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        save_atomic(&self.persist_path, &content).with_context(|| {
            format!("write sessions.toml {:?}", self.persist_path)
        })?;
        Ok(())
    }

    pub fn create(
        &self,
        name: String,
        cwd: PathBuf,
        auto_resume: bool,
    ) -> Result<Arc<Session>> {
        if name.is_empty() {
            anyhow::bail!("session name must not be empty");
        }
        if self.name_index.read().unwrap().contains_key(&name) {
            anyhow::bail!("session name already exists: {name}");
        }
        let session = Session::create_and_start(
            name.clone(),
            cwd,
            self.argv_template.clone(),
            self.config.clone(),
            auto_resume,
        )?;
        {
            let mut sessions = self.sessions.write().unwrap();
            let mut names = self.name_index.write().unwrap();
            sessions.insert(session.id.clone(), session.clone());
            names.insert(name, session.id.clone());
        }
        if let Err(e) = self.save() {
            warn!("sessions.toml save after create: {e}");
        }
        Ok(session)
    }

    pub fn get_by_id_or_name(&self, key: &str) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().unwrap();
        if let Some(s) = sessions.get(key) {
            return Some(s.clone());
        }
        let names = self.name_index.read().unwrap();
        if let Some(id) = names.get(key) {
            return sessions.get(id).cloned();
        }
        None
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().unwrap();
        let mut v: Vec<SessionInfo> = sessions.values().map(|s| s.info()).collect();
        v.sort_by_key(|s| s.created_at_ms);
        v
    }

    /// Snapshot of all live session arcs — used by the idle scanner
    /// which needs to hold each Arc across an await without keeping
    /// the manager's RwLock guard alive.
    pub fn all(&self) -> Vec<Arc<Session>> {
        self.sessions.read().unwrap().values().cloned().collect()
    }

    /// Toggle the persistence flag on an existing session. Returns the
    /// effective new value (= the input). Saves sessions.toml when the
    /// flag actually changed.
    pub fn set_auto_resume(&self, key: &str, value: bool) -> Result<bool> {
        let session = self
            .get_by_id_or_name(key)
            .ok_or_else(|| anyhow::anyhow!("session not found: {key}"))?;
        if session.set_auto_resume(value) {
            if let Err(e) = self.save() {
                warn!("sessions.toml save after set_auto_resume: {e}");
            }
        }
        Ok(value)
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        let session = self
            .get_by_id_or_name(key)
            .ok_or_else(|| anyhow::anyhow!("session not found: {key}"))?;
        session.shutdown();
        self.sessions.write().unwrap().remove(&session.id);
        self.name_index.write().unwrap().remove(&session.name);
        if let Err(e) = self.save() {
            warn!("sessions.toml save after remove: {e}");
        }
        Ok(())
    }

    pub fn shutdown_all(&self) {
        let sessions = self.sessions.read().unwrap();
        for s in sessions.values() {
            s.shutdown();
        }
    }
}

fn save_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let mut tmp = path.to_path_buf();
    tmp.set_extension("toml.tmp");
    fs::write(&tmp, content)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}
