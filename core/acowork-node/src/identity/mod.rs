//! Node identity (ADR-075) — persisted in `{node_data_dir}/identity.json`.
//!
//! | field             | format    | lifecycle    | used for                          |
//! |-------------------|-----------|--------------|-----------------------------------|
//! | `node_id`         | UUID v4   | never changes| EVERY topic / client_id / ACL /   |
//! |                   |           | (except      | NodeRegistry primary key          |
//! |                   |           |  reinstall)  |                                   |
//! | `node_name`       | slug      | renameable   | display only (UI / logs / CLI) —  |
//! |                   |           |              | never a routing key               |
//!
//! ADR-113: identity carries NO "who started me" marker. The launcher
//! (Gateway / Desktop / CLI / a remote machine) is a process-lifecycle
//! fact local to that launcher; putting it in identity.json would make
//! it observable in NodeInfo and let the protocol behave differently
//! depending on the launch path. Old identity.json files may still
//! contain such a field — serde ignores unknown keys.
//!
//! The identity must be finalized BEFORE the first MQTT CONNECT: the
//! LastWill topic (`acowork/nodes/{node_id}/status`) is part of the
//! CONNECT packet, so there is no "negotiate the name after connecting"
//! window. `load_or_create` is idempotent — re-running it with the same
//! home reuses the persisted identity (script-friendly).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::system_hostname;
use crate::error::{NodeError, Result};

/// Enrollment lifecycle of the node identity (ADR-055 §6.12).
///
/// Phase 2a: enrollment is token-free — `Enrolled` simply means the
/// identity has been persisted and the daemon has published its
/// identity to the Gateway. Phase 5a upgrades the state machine with
/// enrollment-token validation and Gateway-issued node tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentState {
    /// identity.json exists but the node has never connected to a
    /// Gateway.
    Created,
    /// The node has published its identity (info retained topic) to
    /// the Gateway at least once.
    Enrolled,
}

/// Persisted node identity (`{home}/identity.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    /// Stable routing key (UUID v4, ADR-075 D1). Used in every topic
    /// and client_id. Never changes for the life of the identity file.
    pub node_id: String,
    /// Display name (slug, ADR-075 D2). Renameable via
    /// `acowork-node rename`; never a routing key.
    pub node_name: String,
    /// Gateway-issued long-term node token (Phase 5a; `None` until
    /// the Gateway signs one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_token: Option<String>,
    /// Last known Gateway address (`host:port` of the MQTT broker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_addr: Option<String>,
    /// Enrollment lifecycle state.
    #[serde(default = "default_enrollment_state")]
    pub enrollment: EnrollmentState,
    /// When this identity file was created.
    pub created_at: DateTime<Utc>,
    /// When this identity was last enrolled with a Gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrolled_at: Option<DateTime<Utc>>,
}

fn default_enrollment_state() -> EnrollmentState {
    EnrollmentState::Created
}

impl NodeIdentity {
    /// Path of the identity file inside a node home directory.
    pub fn path(home: &Path) -> PathBuf {
        home.join("identity.json")
    }

    /// Load the persisted identity, if the home directory has one.
    pub fn load(home: &Path) -> Result<Option<Self>> {
        let path = Self::path(home);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| NodeError::Identity(format!("Failed to read '{}': {}", path.display(), e)))?;
        let identity: Self = serde_json::from_str(&content).map_err(|e| {
            NodeError::Identity(format!(
                "Failed to parse identity file '{}': {} — a pre-ADR-075 identity.json                  (machine_uid / slug node_id) is not migrated; delete the file and re-run                  so the Node mints a new UUID node_id and re-enrolls",
                path.display(),
                e
            ))
        })?;
        Self::validate(&identity)?;
        Ok(Some(identity))
    }

    /// Load the persisted identity or create + persist a new one.
    ///
    /// Idempotent: when identity.json already exists it is reused
    /// as-is (only `gateway_addr` is refreshed), so re-running
    /// `acowork-node start` / `enroll` never mints a new node_id.
    /// `--name` only takes effect at creation time; changing the name
    /// later is the `rename` command (ADR-075 D4 — it never changes
    /// node_id). ADR-113: no spawn-marker argument — the identity is
    /// identical no matter which process launched this node.
    pub fn load_or_create(
        home: &Path,
        explicit_name: Option<&str>,
        gateway_addr: Option<&str>,
    ) -> Result<Self> {
        if let Some(mut existing) = Self::load(home)? {
            if let Some(addr) = gateway_addr {
                existing.gateway_addr = Some(addr.to_string());
            }
            existing.save(home)?;
            return Ok(existing);
        }

        let node_name = match explicit_name {
            Some(name) => {
                if !acowork_core::node::node_name_is_valid(name) {
                    return Err(NodeError::Identity(format!(
                        "Invalid node name '{name}': must be 2-32 chars of [a-z0-9-], \
                         no consecutive '--', no leading/trailing hyphen, \
                         and must not be the reserved word 'local'"
                    )));
                }
                name.to_string()
            }
            None => acowork_core::node::node_name_from_hostname(&system_hostname()),
        };

        let identity = Self {
            node_id: Uuid::new_v4().to_string(),
            node_name,
            node_token: None,
            gateway_addr: gateway_addr.map(str::to_string),
            enrollment: EnrollmentState::Created,
            created_at: Utc::now(),
            enrolled_at: None,
        };
        identity.save(home)?;
        tracing::info!(
            node_id = %identity.node_id,
            node_name = %identity.node_name,
            "Created new node identity"
        );
        Ok(identity)
    }

    /// Persist the identity to `{home}/identity.json`.
    pub fn save(&self, home: &Path) -> Result<()> {
        std::fs::create_dir_all(home).map_err(|e| {
            NodeError::Identity(format!(
                "Failed to create node home '{}': {}",
                home.display(),
                e
            ))
        })?;
        let path = Self::path(home);
        let content = serde_json::to_string_pretty(self).map_err(|e| {
            NodeError::Identity(format!("Failed to serialize identity: {}", e))
        })?;
        std::fs::write(&path, content).map_err(|e| {
            NodeError::Identity(format!("Failed to write '{}': {}", path.display(), e))
        })
    }

    /// Mark the identity as enrolled with a Gateway (idempotent).
    pub fn mark_enrolled(&mut self, gateway_addr: &str) {
        if self.enrollment != EnrollmentState::Enrolled
            || self.enrolled_at.is_none()
            || self.gateway_addr.as_deref() != Some(gateway_addr)
        {
            self.enrollment = EnrollmentState::Enrolled;
            self.enrolled_at = Some(Utc::now());
            self.gateway_addr = Some(gateway_addr.to_string());
        }
    }

    /// Store a Gateway-issued node token (Phase 5a).
    #[allow(dead_code)]
    pub fn set_node_token(&mut self, token: Option<String>) {
        self.node_token = token;
    }

    fn validate(&self) -> Result<()> {
        if Uuid::parse_str(&self.node_id).is_err() {
            return Err(NodeError::Identity(format!(
                "Persisted node_id '{}' is not a valid UUID — refusing to use it",
                self.node_id
            )));
        }
        if !acowork_core::node::node_name_is_valid(&self.node_name) {
            return Err(NodeError::Identity(format!(
                "Persisted node_name '{}' is not a valid slug — refusing to use it",
                self.node_name
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_persists_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        let first = NodeIdentity::load_or_create(
            home,
            Some("gpu-server"),
            Some("10.0.0.1:19875"),
        )
        .unwrap();
        assert_eq!(first.node_name, "gpu-server");
        assert_eq!(first.enrollment, EnrollmentState::Created);
        assert!(Uuid::parse_str(&first.node_id).is_ok(), "node_id must be a UUID");
        assert!(NodeIdentity::path(home).exists());

        // Re-run: same node_id (UUID), same node_name (idempotent).
        let second = NodeIdentity::load_or_create(home, Some("ignored-name"), None).unwrap();
        assert_eq!(second.node_id, first.node_id, "node_id must never change");
        assert_eq!(second.node_name, "gpu-server");

        // gateway_addr refresh still happens on re-run.
        let third = NodeIdentity::load_or_create(home, None, Some("10.0.0.2:19875")).unwrap();
        assert_eq!(third.gateway_addr.as_deref(), Some("10.0.0.2:19875"));
    }

    /// ADR-113: identity.json carries no "who spawned me" marker. A
    /// legacy file that still has one is tolerated (serde ignores unknown
    /// keys) and the field is never re-emitted.
    #[test]
    fn identity_does_not_persist_a_spawn_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let identity = NodeIdentity::load_or_create(home, None, None).unwrap();
        identity.save(home).unwrap();
        let raw = std::fs::read_to_string(NodeIdentity::path(home)).unwrap();
        assert!(!raw.contains("gateway_managed"), "identity.json: {raw}");

        let legacy = format!(
            "{},\n  \"gateway_managed\": true\n}}\n",
            raw.trim_end().trim_end_matches('}').trim_end()
        );
        std::fs::write(NodeIdentity::path(home), legacy).unwrap();
        let loaded = NodeIdentity::load(home)
            .unwrap()
            .expect("legacy identity must still load");
        assert_eq!(loaded.node_id, identity.node_id);
        // Loading a legacy file must not re-emit the retired field on the
        // next save — otherwise an upgrade silently reintroduces it.
        loaded.save(home).unwrap();
        let rewritten = std::fs::read_to_string(NodeIdentity::path(home)).unwrap();
        assert!(
            !rewritten.contains("gateway_managed"),
            "identity.json re-emitted a launcher field: {rewritten}"
        );
    }

    #[test]
    fn invalid_explicit_name_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let err =
            NodeIdentity::load_or_create(tmp.path(), Some("Bad_Name"), None).unwrap_err();
        assert!(err.to_string().contains("Invalid node name"));
        let err =
            NodeIdentity::load_or_create(tmp.path(), Some("local"), None).unwrap_err();
        assert!(
            err.to_string().contains("reserved"),
            "reserved word 'local' must be rejected: {err}"
        );
        assert!(!NodeIdentity::path(tmp.path()).exists());
    }

    #[test]
    fn default_name_derives_from_hostname_slug_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let identity = NodeIdentity::load_or_create(tmp.path(), None, None).unwrap();
        assert!(acowork_core::node::node_name_is_valid(&identity.node_name));
    }

    #[test]
    fn corrupted_identity_file_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(NodeIdentity::path(tmp.path()), "{ not json").unwrap();
        let err = NodeIdentity::load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("parse identity file"));
    }

    #[test]
    fn tampered_node_id_is_rejected_on_load() {
        let tmp = tempfile::tempdir().unwrap();
        let mut identity =
            NodeIdentity::load_or_create(tmp.path(), Some("gpu-server"), None).unwrap();
        identity.node_id = "INVALID-UUID".to_string();
        identity.save(tmp.path()).unwrap();
        let err = NodeIdentity::load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not a valid UUID"));
    }

    #[test]
    fn tampered_node_name_is_rejected_on_load() {
        let tmp = tempfile::tempdir().unwrap();
        let mut identity =
            NodeIdentity::load_or_create(tmp.path(), Some("gpu-server"), None).unwrap();
        identity.node_name = "INVALID SLUG".to_string();
        identity.save(tmp.path()).unwrap();
        let err = NodeIdentity::load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not a valid slug"));
    }

    #[test]
    fn mark_enrolled_is_idempotent_but_stamps_first_time() {
        let tmp = tempfile::tempdir().unwrap();
        let mut identity =
            NodeIdentity::load_or_create(tmp.path(), Some("gpu-server"), None).unwrap();
        assert!(identity.enrolled_at.is_none());

        identity.mark_enrolled("10.0.0.1:19875");
        assert_eq!(identity.enrollment, EnrollmentState::Enrolled);
        let first = identity.enrolled_at.unwrap();

        identity.mark_enrolled("10.0.0.1:19875");
        assert_eq!(identity.enrolled_at, Some(first), "timestamp must be stable");
    }

    #[test]
    fn round_trip_serialization() {
        let tmp = tempfile::tempdir().unwrap();
        let identity = NodeIdentity::load_or_create(
            tmp.path(),
            Some("gpu-server"),
            Some("a:1"),
        )
        .unwrap();
        let loaded = NodeIdentity::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded.node_id, identity.node_id);
        assert_eq!(loaded.node_name, identity.node_name);
        assert_eq!(loaded.gateway_addr, Some("a:1".to_string()));
    }
}
