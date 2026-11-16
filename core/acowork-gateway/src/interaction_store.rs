//! Per-agent last-interaction timestamp store.
//!
//! Persists `instance_id -> last_interaction_at` as a single JSON file under
//! `<data_dir>/agent_interactions.json`. Used by the front-end agent list
//! to surface a "most recently interacted" order. Atomic write via the
//! write-to-temp + rename pattern shared with `cron::store::CronStore`.
//!
//! ADR-073: the key is the agent `instance_id` (the install-time UUID, the
//! single identity an agent is addressed by on the wire). It is NOT a
//! stable per-package key — `instance_id` is regenerated on every install
//! / reinstall, so the timestamp does NOT survive an agent reinstall.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "agent_interactions.json";

/// Error type for [`InteractionStore`] operations.
#[derive(Debug, thiserror::Error)]
pub enum InteractionStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// On-disk schema wrapper. Kept private so the file format can evolve
/// without leaking into the public API.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    entries: HashMap<String, DateTime<Utc>>,
}

/// Disk-backed store of last-interaction timestamps, one entry per agent.
#[derive(Debug, Clone)]
pub struct InteractionStore {
    file_path: PathBuf,
}

impl InteractionStore {
    /// Build a store rooted at `<data_dir>/agent_interactions.json`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            file_path: data_dir.join(FILE_NAME),
        }
    }

    /// Load the persisted map. Returns an empty map if the file is absent
    /// or unreadable; errors are logged and swallowed so a corrupt store
    /// never blocks Gateway startup.
    pub fn load(&self) -> HashMap<String, DateTime<Utc>> {
        let raw = match std::fs::read_to_string(&self.file_path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return HashMap::new(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.file_path.display(),
                    "Failed to read interaction store; starting empty"
                );
                return HashMap::new();
            }
        };
        match serde_json::from_str::<OnDisk>(&raw) {
            Ok(d) => d.entries,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.file_path.display(),
                    "Failed to parse interaction store; starting empty"
                );
                HashMap::new()
            }
        }
    }

    /// Persist the entire map atomically (write-to-tmp + rename).
    pub fn save(&self, entries: &HashMap<String, DateTime<Utc>>) -> Result<(), InteractionStoreError> {
        let payload = OnDisk {
            entries: entries.clone(),
        };
        let json = serde_json::to_string_pretty(&payload)?;
        if let Some(parent) = self.file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.file_path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.file_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = crate::test_support::unique_temp_dir(format!("interaction-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_returns_empty_when_missing() {
        let dir = tempdir("missing");
        let store = InteractionStore::new(&dir);
        assert!(store.load().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = tempdir("roundtrip");
        let store = InteractionStore::new(&dir);

        let mut entries = HashMap::new();
        let t1 = Utc::now();
        let t2 = Utc::now() + chrono::Duration::seconds(60);
        entries.insert("com.acowork.a".to_string(), t1);
        entries.insert("com.acowork.b".to_string(), t2);

        store.save(&entries).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("com.acowork.a").copied(), Some(t1));
        assert_eq!(loaded.get("com.acowork.b").copied(), Some(t2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_falls_back_to_empty() {
        let dir = tempdir("corrupt");
        let store = InteractionStore::new(&dir);
        std::fs::write(store.file_path.clone(), "{not valid json").unwrap();
        assert!(store.load().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_overwrites_previous() {
        let dir = tempdir("overwrite");
        let store = InteractionStore::new(&dir);

        let mut entries = HashMap::new();
        entries.insert("a".to_string(), Utc::now());
        store.save(&entries).unwrap();

        entries.remove("a");
        entries.insert("b".to_string(), Utc::now());
        store.save(&entries).unwrap();

        let loaded = store.load();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("b"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
