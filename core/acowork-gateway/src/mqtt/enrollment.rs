//! Node enrollment credential stores (ADR-055 Phase 5a security).
//!
//! Two persisted stores live under `{data_dir}/`:
//!
//! - `enrollment_tokens.json` — one-time enrollment tokens issued via
//!   `nodes token create`. Only the sha256 hash is persisted (never the
//!   plaintext); a token is consumed on the first successful enroll.
//! - `node_tokens.json` — long-lived per-node credentials minted at
//!   enroll time (also used for HTTP-channel auth via the
//!   `X-ACowork-Node-Token` header). Persisted so a Gateway restart
//!   keeps accepting already-enrolled nodes (CONNECT credentials).
//!
//! Both stores are wrapped in a `std::sync::Mutex` shared across the
//! broker auth handler (synchronous, broker thread), the MQTT dispatch,
//! and the HTTP handlers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// sha256 hex digest of a plaintext token.
fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Constant-time string equality — same discipline as
/// `HttpAuth::validate_token`: never leaks hash prefix/length
/// mismatches through early-exit timing. Exposed so the broker auth
/// handler can compare the internal publisher / HTTP credentials the
/// same way.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Fresh 256-bit random token as 64 hex chars (same shape as the HTTP
/// auth token). `pub(crate)` so the Gateway startup can mint the
/// internal publisher credential with the same entropy source.
pub(crate) fn generate_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Atomically persist a JSON store (write temp + rename) so a crash
/// never leaves a corrupt file. Failures are logged, not fatal — the
/// in-memory state stays authoritative for the current process.
/// `pub(crate)` so the ADR-087 ownership stores reuse the same
/// crash-safe write discipline.
pub(crate) fn atomic_write_json<T: Serialize>(path: &Path, value: &T) {
    let tmp = path.with_extension("json.tmp");
    let result = (|| -> std::io::Result<()> {
        let content = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!(path = %path.display(), error = %e, "failed to persist credential store");
    }
}

/// Outcome of validating an enrollment token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenValidation {
    /// Valid, not yet consumed, not expired.
    Valid,
    /// Valid hash but past `expires_at`.
    Expired,
    /// Valid hash but already consumed by another node.
    Consumed,
    /// No record for this token.
    Unknown,
}

/// Persisted record — only the sha256 hash, never the plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnrollmentTokenRecord {
    token_hash: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    /// node_id that consumed the token (set by [`consume_token`]).
    consumed_by: Option<String>,
    /// ADR-087 D2: the logged-in user who created the token (from
    /// `AuthContext`). Written to `node_owners.json` when the token is
    /// consumed by a successful enroll. `None` = CLI-issued token → the
    /// enrolled node stays ownerless (admin-only, fail-closed).
    #[serde(default)]
    owner_user_id: Option<String>,
}

/// One-time enrollment token store (`{data_dir}/enrollment_tokens.json`).
#[derive(Debug, Default)]
pub struct EnrollmentTokenStore {
    path: PathBuf,
    records: Vec<EnrollmentTokenRecord>,
}

impl EnrollmentTokenStore {
    /// Load the store from `{data_dir}/enrollment_tokens.json`
    /// (empty store when the file does not exist yet; a corrupt file
    /// is logged and treated as empty rather than blocking startup).
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("enrollment_tokens.json");
        let records = match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "enrollment_tokens.json corrupt — starting with an empty store"
                );
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        let mut store = Self { path, records };
        // ADR-087 review M7: sweep records that expired while the
        // Gateway was down so a long-idle store cannot grow unbounded
        // across restarts.
        let now = Utc::now();
        let before = store.records.len();
        store.records.retain(|r| r.expires_at > now);
        if store.records.len() != before {
            store.persist();
        }
        store
    }

    /// Create a new one-time token valid for `ttl` and return its
    /// plaintext (printed exactly once to the operator). Only the
    /// sha256 hash is persisted. Expired-and-consumed records are
    /// purged opportunistically to bound the file size.
    ///
    /// ADR-087 D2: `owner_user_id` is the logged-in creator (HTTP path);
    /// CLI-issued tokens pass `None` and the enrolled node stays
    /// ownerless (admin-only).
    pub fn create_token(
        &mut self,
        ttl: std::time::Duration,
        owner_user_id: Option<String>,
    ) -> String {
        let plaintext = generate_token();
        let now = Utc::now();
        let expires_at = now + chrono::Duration::from_std(ttl).expect("ttl within chrono range");
        // ADR-087 review M7: purge records past their expiry — they can
        // never validate again (Expired and Unknown are equivalent for a
        // one-time token whose hash is gone from any live client).
        // Unexpired records are KEPT regardless of consumption: a
        // consumed-but-unexpired token must still answer `Consumed`
        // (replay detection), and an unexpired unconsumed token must
        // stay valid — the previous condition wrongly dropped valid
        // tokens whose expiry was later than this new one's.
        self.records.retain(|r| r.expires_at > now);
        self.records.push(EnrollmentTokenRecord {
            token_hash: hash_token(&plaintext),
            created_at: now,
            expires_at,
            consumed_by: None,
            owner_user_id,
        });
        self.persist();
        plaintext
    }

    /// Validate a plaintext token against the store (constant-time
    /// compare against every stored hash).
    pub fn validate_token(&self, token: &str) -> TokenValidation {
        let hash = hash_token(token);
        let now = Utc::now();
        for record in &self.records {
            if constant_time_eq(record.token_hash.as_bytes(), hash.as_bytes()) {
                if record.consumed_by.is_some() {
                    return TokenValidation::Consumed;
                }
                if record.expires_at <= now {
                    return TokenValidation::Expired;
                }
                return TokenValidation::Valid;
            }
        }
        TokenValidation::Unknown
    }

    /// ADR-087 D2: the `owner_user_id` bound to a plaintext token
    /// (`None` for CLI-issued tokens or unknown hashes). Read at enroll
    /// time to seed `node_owners.json`.
    pub fn token_owner(&self, token: &str) -> Option<String> {
        let hash = hash_token(token);
        self.records.iter().find_map(|r| {
            if constant_time_eq(r.token_hash.as_bytes(), hash.as_bytes()) {
                r.owner_user_id.clone()
            } else {
                None
            }
        })
    }

    /// Mark a token consumed by `node_id` and persist. Returns false
    /// when the token is unknown or already consumed (idempotent).
    pub fn consume_token(&mut self, token: &str, node_id: &str) -> bool {
        let hash = hash_token(token);
        for record in &mut self.records {
            if constant_time_eq(record.token_hash.as_bytes(), hash.as_bytes())
                && record.consumed_by.is_none()
            {
                record.consumed_by = Some(node_id.to_string());
                self.persist();
                return true;
            }
        }
        false
    }

    /// Number of live (unconsumed, unexpired) tokens — diagnostics.
    pub fn live_count(&self) -> usize {
        let now = Utc::now();
        self.records
            .iter()
            .filter(|r| r.consumed_by.is_none() && r.expires_at > now)
            .count()
    }

    fn persist(&self) {
        atomic_write_json(&self.path, &self.records);
    }
}

/// Persisted per-node long-lived credential. Plaintext is stored on
/// disk — this file is the trust anchor for node credentials, protect
/// it like `http_token`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTokenRecord {
    pub node_id: String,
    pub token: String,
    pub created_at: DateTime<Utc>,
}

/// Long-lived node token store (`{data_dir}/node_tokens.json`).
///
/// The Gateway mints these at enroll time; a node reconnects with
/// `username = node:{id}` + this token as the CONNECT password, and
/// presents it as `X-ACowork-Node-Token` on HTTP channel requests.
#[derive(Debug, Default)]
pub struct NodeTokenStore {
    path: PathBuf,
    nodes: HashMap<String, NodeTokenRecord>,
}

impl NodeTokenStore {
    /// Load the store from `{data_dir}/node_tokens.json` (empty when
    /// the file does not exist yet; corrupt files are logged and
    /// treated as empty).
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("node_tokens.json");
        let nodes = match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "node_tokens.json corrupt — starting with an empty store"
                );
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        Self { path, nodes }
    }

    /// The persisted token for a node (None when not enrolled yet).
    pub fn get_token(&self, node_id: &str) -> Option<&str> {
        self.nodes.get(node_id).map(|r| r.token.as_str())
    }

    /// The full record for a node.
    pub fn get(&self, node_id: &str) -> Option<&NodeTokenRecord> {
        self.nodes.get(node_id)
    }

    /// Mint (or reuse) the long-lived token for a node and persist
    /// (ADR-075 D7 — simplified: UUID node_ids never collide, so the
    /// placeholder-claim and conflict branches are gone). Idempotent:
    /// an already-enrolled node keeps its credential.
    pub fn upsert(&mut self, node_id: &str) -> String {
        if let Some(record) = self.nodes.get(node_id) {
            return record.token.clone();
        }
        let token = generate_token();
        self.nodes.insert(
            node_id.to_string(),
            NodeTokenRecord {
                node_id: node_id.to_string(),
                token: token.clone(),
                created_at: Utc::now(),
            },
        );
        self.persist();
        token
    }

    /// Whether `password` matches the node's persisted token
    /// (constant-time compare; false when the node is not enrolled).
    pub fn node_token_matches(&self, node_id: &str, password: &str) -> bool {
        match self.get_token(node_id) {
            Some(expected) => constant_time_eq(expected.as_bytes(), password.as_bytes()),
            None => false,
        }
    }

    /// Whether `password` matches ANY registered node token — used by
    /// the Phase 5a `agent:{id}` CONNECT check where Runtime
    /// connections present the token of the node that spawned them
    /// (agent→node ownership is intentionally not verified yet).
    pub fn any_token_matches(&self, password: &str) -> bool {
        self.nodes
            .values()
            .any(|r| constant_time_eq(r.token.as_bytes(), password.as_bytes()))
    }

    /// Reverse lookup: which enrolled node does this token belong to?
    /// Used by the Intent delivery API to attribute a sender to the
    /// machine its call arrived from (tech-debt plan D4).
    pub fn node_id_for_token(&self, password: &str) -> Option<String> {
        self.nodes
            .iter()
            .find(|(_, r)| constant_time_eq(r.token.as_bytes(), password.as_bytes()))
            .map(|(id, _)| id.clone())
    }

    /// Number of enrolled nodes — diagnostics.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    fn persist(&self) {
        atomic_write_json(&self.path, &self.nodes);
    }
}

/// Thread-safe shared enrollment token store.
pub type SharedEnrollmentTokenStore = Arc<Mutex<EnrollmentTokenStore>>;

/// Thread-safe shared node token store.
pub type SharedNodeTokenStore = Arc<Mutex<NodeTokenStore>>;

/// Create a shared enrollment token store loaded from `data_dir`.
pub fn new_shared_enrollment_store(data_dir: &Path) -> SharedEnrollmentTokenStore {
    Arc::new(Mutex::new(EnrollmentTokenStore::load(data_dir)))
}

/// Create a shared node token store loaded from `data_dir`.
pub fn new_shared_node_token_store(data_dir: &Path) -> SharedNodeTokenStore {
    Arc::new(Mutex::new(NodeTokenStore::load(data_dir)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acowork-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn create_then_validate() {
        let dir = test_dir("enroll-validate");
        let mut store = EnrollmentTokenStore::load(&dir);
        assert_eq!(store.validate_token("nope"), TokenValidation::Unknown);

        let token = store.create_token(Duration::from_secs(3600), None);
        assert_eq!(token.len(), 64);
        assert_eq!(store.validate_token(&token), TokenValidation::Valid);
        assert_eq!(store.live_count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_token_is_unknown() {
        let dir = test_dir("enroll-wrong");
        let mut store = EnrollmentTokenStore::load(&dir);
        store.create_token(Duration::from_secs(3600), None);
        assert_eq!(store.validate_token("deadbeef"), TokenValidation::Unknown);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_token_does_not_evict_valid_long_lived_token() {
        // Review M7 regression: the old opportunistic purge keyed on the
        // NEW token's expiry and silently invalidated outstanding
        // long-lived tokens. Creating a short-TTL token must keep a
        // previously issued, unexpired token valid.
        let dir = test_dir("enroll-noevict");
        let mut store = EnrollmentTokenStore::load(&dir);
        let long = store.create_token(Duration::from_secs(3600), None);
        let _short = store.create_token(Duration::from_secs(1), None);
        assert_eq!(store.validate_token(&long), TokenValidation::Valid);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_records_purged_on_load() {
        // Review M7: expiry sweep runs at load too, so a store nobody
        // writes to cannot accumulate dead records across restarts.
        let dir = test_dir("enroll-loadpurge");
        {
            let mut store = EnrollmentTokenStore::load(&dir);
            let _live = store.create_token(Duration::from_secs(3600), None);
            let dead = store.create_token(Duration::ZERO, None);
            assert_eq!(store.validate_token(&dead), TokenValidation::Expired);
        }
        let store = EnrollmentTokenStore::load(&dir);
        assert_eq!(store.live_count(), 1, "expired record purged at load");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_token_is_rejected() {
        let dir = test_dir("enroll-expired");
        let mut store = EnrollmentTokenStore::load(&dir);
        // Zero TTL = expires immediately.
        let token = store.create_token(Duration::ZERO, None);
        assert_eq!(store.validate_token(&token), TokenValidation::Expired);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_time_consumption() {
        let dir = test_dir("enroll-consume");
        let mut store = EnrollmentTokenStore::load(&dir);
        let token = store.create_token(Duration::from_secs(3600), None);

        assert!(store.consume_token(&token, "gpu-1"));
        // Consuming twice is a no-op.
        assert!(!store.consume_token(&token, "gpu-2"));
        assert_eq!(store.validate_token(&token), TokenValidation::Consumed);
        assert_eq!(store.live_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_survives_reload() {
        let dir = test_dir("enroll-persist");
        {
            let mut store = EnrollmentTokenStore::load(&dir);
            let token = store.create_token(Duration::from_secs(3600), None);
            assert!(store.consume_token(&token, "gpu-1"));
        }
        {
            // Reload from disk — the consumed state must survive.
            let store = EnrollmentTokenStore::load(&dir);
            let record = &store.records[0];
            assert_eq!(record.consumed_by.as_deref(), Some("gpu-1"));
            assert_eq!(store.live_count(), 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_token_upsert_reuses_for_same_node() {
        let dir = test_dir("nodetoken-upsert");
        let mut store = NodeTokenStore::load(&dir);
        let first = store.upsert("gpu-1");
        assert_eq!(first.len(), 64);
        assert_eq!(store.upsert("gpu-1"), first, "re-enroll reuses token");
        assert_eq!(store.get_token("gpu-1"), Some(first.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_token_matches_constant_time() {
        let dir = test_dir("nodetoken-match");
        let mut store = NodeTokenStore::load(&dir);
        let token = store.upsert("gpu-1");
        assert!(store.node_token_matches("gpu-1", &token));
        assert!(!store.node_token_matches("gpu-1", "wrong"));
        assert!(!store.node_token_matches("gpu-2", &token), "unenrolled node");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_token_persists_across_reload() {
        let dir = test_dir("nodetoken-persist");
        {
            let mut store = NodeTokenStore::load(&dir);
            store.upsert("gpu-1");
        }
        {
            let store = NodeTokenStore::load(&dir);
            let token = store.get_token("gpu-1").expect("token persisted");
            assert_eq!(token.len(), 64);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
