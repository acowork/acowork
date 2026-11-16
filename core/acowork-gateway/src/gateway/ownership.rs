//! ADR-087 — Node / Agent ownership policy stores (Gateway-side, fail-closed).
//!
//! Two persisted ownership tables live under `{data_dir}/`, mirroring the
//! "atomic-write JSON + in-memory mirror" pattern of the enrollment token
//! stores (`mqtt/enrollment.rs`):
//!
//! - `node_owners.json` — `node_id` (UUID v4, ADR-075) → [`OwnerRecord`]
//! - `agent_owners.json` — `instance_id` (UUID v4, ADR-073) → [`OwnerRecord`]
//!
//! Ownership is **Gateway policy data**, never echoed through the MQTT
//! data plane (ADR-087 D1): the Node's retained inventory stays the
//! existence authority, this table is the ownership authority.
//!
//! The decision functions below are the single source of truth for
//! `can_manage` / `can_use` / `can_transfer` (ADR-087 §4). Both the
//! permission middleware (`http/permission.rs`, 403/404 enforcement) and
//! the list/detail handlers (server-rendered `can_manage` booleans,
//! ADR-087 D8) call these — no client-side derivation, no drift.
//!
//! Fail-closed rule: a missing record means **ownerless** — only admins
//! can manage it (ADR-087 D7, upgrade migration).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::mqtt::enrollment::atomic_write_json;

/// Resource visibility (ADR-087 D6).
///
/// Agents use `Private`/`Shared`; nodes use `Private`/`Public` (metadata
/// visibility only — `Public` never grants manage or install rights).
/// A record without a visibility field deserializes to `Private`
/// (fail-closed default for pre-existing entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Private,
    /// Agent: any logged-in user may **view** (find it in lists/detail);
    /// `use` (chat) still requires the guest list and `manage` still
    /// requires owner/admin (ADR-087 D6 — shared widens view only).
    Shared,
    /// Node: basic metadata (id/name/online) visible to all logged-in
    /// users; manage still requires the owner/guest list.
    Public,
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Shared => "shared",
            Self::Public => "public",
        }
    }

    /// Parse the PATCH body value. Returns `None` for unknown strings so
    /// the handler can reject; `Public` is only valid on nodes, `Shared`
    /// only on agents — the callers gate that.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "private" => Some(Self::Private),
            "shared" => Some(Self::Shared),
            "public" => Some(Self::Public),
            _ => None,
        }
    }

    /// Is this resource deliberately published to every logged-in user?
    ///
    /// `Shared` (agents) and `Public` (nodes) are the two spellings of
    /// the same tier — the wire value differs per resource kind but the
    /// visibility rule is identical, so [`can_view`] keys off this rather
    /// than duplicating the match. `Public` never grants manage or
    /// install rights (ADR-087 D6); it only un-hides the row.
    pub fn is_published(self) -> bool {
        matches!(self, Self::Shared | Self::Public)
    }
}

/// One ownership row. Keyed by resource id (node_id or instance_id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerRecord {
    /// The single owner (ADR-087 D9b). `None` = ownerless → admin-only.
    pub owner_user_id: Option<String>,
    /// Users granted the `manage` tier (revocable authorization, not
    /// ownership — ADR-087 D9).
    #[serde(default)]
    pub guests: Vec<String>,
    #[serde(default)]
    pub visibility: Visibility,
    pub created_at: DateTime<Utc>,
    /// ADR-087 §4 cascade: the node hosting this instance, denormalized
    /// so the permission module can walk `agent → node` without joining
    /// the MQTT retained inventory.
    ///
    /// The inventory is the *existence* authority (ADR-055 §6.5) and the
    /// Gateway never reads it synchronously; `uninstall` needs BOTH the
    /// node's and the agent's manage list, so the mapping has to be
    /// resolvable from policy data alone. Written at install/ensure/clone
    /// time (the Gateway knows the target node when it dispatches).
    ///
    /// `None` on a row written before this field existed — see
    /// [`OwnershipStore::backfill_agent_node_ids`] for the one-shot
    /// migration and its failure mode (admin-only, fail-closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

impl OwnerRecord {
    pub fn new(owner_user_id: Option<String>) -> Self {
        Self {
            owner_user_id,
            guests: Vec::new(),
            visibility: Visibility::Private,
            created_at: Utc::now(),
            node_id: None,
        }
    }

    /// Attach the hosting node — ADR-087 §4 cascade needs it. Set once
    /// at install/ensure/clone; a later change of host is a reinstall,
    /// which writes a fresh row anyway.
    pub fn with_node(mut self, node_id: Option<String>) -> Self {
        self.node_id = node_id;
        self
    }
}

// ── Decision functions (ADR-087 §4, single source of truth) ──────────

/// `manage` = admin ∨ owner. Guests are NOT in the manage tier (ADR-087
/// D9: a guest is *use* authorization, not co-ownership — config /
/// workspace / files / lifecycle / install stay owner ∨ admin only).
/// Ownerless (`None` record or `owner = None`) → admin only (fail-closed).
pub fn can_manage(rec: Option<&OwnerRecord>, user_id: &str, is_admin: bool) -> bool {
    if is_admin {
        return true;
    }
    rec.and_then(|r| r.owner_user_id.as_deref()) == Some(user_id)
}

/// `transfer` (attribution operations: guests list, visibility) =
/// admin ∨ owner. Guests are NOT included (ADR-087 D9 R1).
pub fn can_transfer(rec: Option<&OwnerRecord>, user_id: &str, is_admin: bool) -> bool {
    if is_admin {
        return true;
    }
    rec.and_then(|r| r.owner_user_id.as_ref())
        .is_some_and(|o| o == user_id)
}

/// `use` = manage ∨ (caller is on the guest list). The guest list is the
/// use-authorization roster (ADR-087 D9): being a guest lets you open
/// your own session and chat, but grants no manage rights. Visibility
/// does NOT feed this predicate — `shared` only widens *view*, never
/// *use* (ADR-087 D6). Callers gate the session control plane on this.
pub fn can_use(rec: Option<&OwnerRecord>, user_id: &str, is_admin: bool) -> bool {
    if can_manage(rec, user_id, is_admin) {
        return true;
    }
    rec.is_some_and(|r| r.guests.iter().any(|g| g == user_id))
}

/// `view` = "may this resource APPEAR in a list read for this caller".
///
/// The single source of truth for every visibility-filtered read
/// (ADR-087 D5/D6). List and detail MUST NOT each roll their own
/// default — that is exactly how the two drifted: `list_agents`
/// filtered on `is_some_and(Private)` (fail-OPEN on a missing row)
/// while `get_agent` used `is_none_or` (also fail-open), so a row
/// that was absent from `agent_owners.json` leaked into every list
/// for every account.
///
/// Fail-closed on a missing record (ADR-087 D7): an ownerless
/// resource is admin-only, therefore it must not be *listed* for
/// anyone else either. `None ⇒ false` unless the caller is admin.
///
/// The three tiers, in the order a caller can satisfy them:
///   1. `is_admin` — admins see everything, including ownerless rows
///      (that is how an unclaimed resource gets found and claimed).
///   2. `can_use` — owner ∨ guest ∨ admin. A guest MUST keep seeing the
///      row they were granted: the sidebar is the only place a granted
///      resource is reachable, so hiding it would lock out the very
///      people the guest (use) grant exists for.
///   3. `Shared` / `Public` — the resource is deliberately published;
///      anyone logged in may *see* it. This widens view only — it never
///      grants use or manage (ADR-087 D6).
pub fn can_view(rec: Option<&OwnerRecord>, user_id: &str, is_admin: bool) -> bool {
    if is_admin {
        return true;
    }
    can_use(rec, user_id, false) || rec.is_some_and(|r| r.visibility.is_published())
}

/// May this caller actually *publish* the resource (agent `shared` /
/// node `public`)?
///
/// A separate question from [`can_transfer`] on purpose: passing the
/// `AgentTransfer` gate is a *permission* answer, while "the row has
/// an owner" is a *data* answer. `upsert_with` enforces the
/// `ownerless ⇒ not published` invariant by normalising the write away
/// (ADR-087 D6), so an ownerless row can never hold a published value.
/// Telling those two apart at the boundary is what keeps the Desktop
/// switch from offering a write that the store would silently undo.
///
/// Local mode is `true`: there is no attribution to speak of, and the
/// middleware never evaluates the tables in that mode (D8).
pub fn can_publish(rec: Option<&OwnerRecord>, is_local: bool) -> bool {
    is_local || rec.is_some_and(|r| r.owner_user_id.is_some())
}

/// Whether the record has no owner at all — the state D7 makes
/// admin-only. Surfaced to the client so the UI can explain *why* the
/// visibility switch is unavailable instead of letting the user flip it
/// into a 409.
pub fn is_ownerless(rec: Option<&OwnerRecord>) -> bool {
    rec.is_none_or(|r| r.owner_user_id.is_none())
}

/// Whether `user_id` is on the guest (manage-authorization) list —
/// surfaced to the client as `is_guest` (ADR-087 D7). The owner is not
/// a guest.
pub fn is_guest(rec: Option<&OwnerRecord>, user_id: Option<&str>) -> bool {
    match (rec, user_id) {
        (Some(r), Some(u)) => {
            r.owner_user_id.as_deref() != Some(u) && r.guests.iter().any(|g| g == u)
        }
        _ => false,
    }
}

// ── Persistence ───────────────────────────────────────────────────────

/// One JSON-backed ownership table (either file). Wrapped in
/// `Arc<Mutex<..>>` and shared between the HTTP handlers and the MQTT
/// dispatch (enroll writes node rows, inventory-clear removes agent rows).
#[derive(Debug)]
pub struct OwnershipStore {
    path: PathBuf,
    records: HashMap<String, OwnerRecord>,
}

impl OwnershipStore {
    /// Load `{data_dir}/{filename}` (empty store when absent; a corrupt
    /// file is logged and treated as empty rather than blocking startup —
    /// same discipline as the enrollment stores).
    pub fn load(data_dir: &Path, filename: &str) -> Self {
        let path = data_dir.join(filename);
        let records = match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "{filename} corrupt — starting with an empty ownership table (fail-closed: admin-only)"
                );
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        Self { path, records }
    }

    pub fn get(&self, id: &str) -> Option<&OwnerRecord> {
        self.records.get(id)
    }

    /// Insert/replace a row and persist (install/ensure/clone owner write).
    pub fn put(&mut self, id: &str, record: OwnerRecord) {
        self.records.insert(id.to_string(), record);
        self.persist();
    }

    /// Insert a row only when the resource has no ownership record yet
    /// (enroll binding — a re-enroll never changes the owner, ADR-087 D2.5).
    pub fn put_if_absent(&mut self, id: &str, record: OwnerRecord) {
        if self.records.contains_key(id) {
            return;
        }
        self.records.insert(id.to_string(), record);
        self.persist();
    }

    /// Update an existing row, or create one from `default` and apply `f`
    /// when the resource has no record yet (ADR-087 D7 admin claim: an
    /// ownerless resource must be editable by admin even when no row was
    /// ever written — legacy installs / CLI-enrolled nodes).
    pub fn upsert_with<F: FnOnce(&mut OwnerRecord)>(
        &mut self,
        id: &str,
        default: impl FnOnce() -> OwnerRecord,
        f: F,
    ) {
        let mut rec = match self.records.get_mut(id) {
            Some(existing) => {
                f(existing);
                existing.clone()
            }
            None => {
                let mut rec = default();
                f(&mut rec);
                rec
            }
        };
        // `f` already applied above; normalize ownerless-visibility
        // invariant (ADR-087 D6: ownerless ⇒ not Shared).
        if rec.owner_user_id.is_none() && rec.visibility == Visibility::Shared {
            rec.visibility = Visibility::Private;
        }
        self.records.insert(id.to_string(), rec);
        self.persist();
    }

    /// Remove a row and persist (uninstall / retained-inventory clear).
    /// Idempotent — removing an unknown key is a no-op.
    pub fn remove(&mut self, id: &str) {
        if self.records.remove(id).is_some() {
            self.persist();
        }
    }

    /// Mutable access to a row + persist. Returns false when unknown.
    pub fn update<F: FnOnce(&mut OwnerRecord)>(&mut self, id: &str, f: F) -> bool {
        let Some(rec) = self.records.get_mut(id) else {
            return false;
        };
        f(rec);
        self.persist();
        true
    }

    /// All rows (admin diagnostics / orphan audit, ADR-087 D7).
    pub fn iter(&self) -> impl Iterator<Item = (&String, &OwnerRecord)> {
        self.records.iter()
    }

    /// ADR-087 D7 orphan audit: rows whose resource has not been seen for
    /// longer than `max_age` are WARNed but kept — the store never
    /// auto-deletes a permission row (dangling revocation is worse than a
    /// stale audit line). `known` returns whether the live topology still
    /// contains the id.
    pub fn audit_orphans(&self, kind: &str, known: impl Fn(&str) -> bool, max_age: chrono::Duration) {
        for (id, rec) in self.iter() {
            if known(id) {
                continue;
            }
            let age = Utc::now() - rec.created_at;
            if age > max_age {
                tracing::warn!(
                    resource = %id,
                    kind,
                    owner = ?rec.owner_user_id,
                    age_days = age.num_days(),
                    "ownership row references a resource not seen since it was created (>30d) — \
                     uninstall likely missed the row; kept for audit (ADR-087 D7)"
                );
            }
        }
    }

    fn persist(&self) {
        atomic_write_json(&self.path, &self.records);
    }
}

/// Thread-safe shared ownership store (same shape as the token stores).
pub type SharedOwnershipStore = Arc<Mutex<OwnershipStore>>;

/// ADR-087 D7 / review M4: an install dispatched to a Node but not yet
/// confirmed by the node's retained inventory. The ownership row is NOT
/// written at dispatch time — it is committed when the instance first
/// appears in `installed_agents` (the authoritative existence signal) and
/// dropped when the Node reports the operation failed. This keeps
/// `agent_owners.json` free of rows for instances that never materialized
/// (the permanent-orphan case a dispatch-time write would leave behind).
#[derive(Debug, Clone)]
pub struct PendingInstall {
    /// The row to commit once the instance exists (caller — the HTTP
    /// layer — applies the D3 default-shared rule when building it).
    pub record: OwnerRecord,
    /// Registration clock, for TTL sweep of lost terminal events.
    pub dispatched_at: std::time::Instant,
}

/// Pending installs keyed by candidate instance id.
pub type SharedPendingInstalls = Arc<Mutex<HashMap<String, PendingInstall>>>;

/// Pending-install lifetime: an install operation deadline is 60s and a
/// node's inventory publish follows immediately; anything older than this
/// has lost its terminal signal and must not linger (it would block a
/// future instance from being claimed — the id itself never reuses, so
/// this is purely a memory sweep).
const PENDING_INSTALL_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

pub fn new_shared_pending_installs() -> SharedPendingInstalls {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Register (or replace) a pending install, sweeping expired entries.
pub fn register_pending(
    store: &SharedPendingInstalls,
    instance_id: &str,
    record: OwnerRecord,
) {
    let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|_, p| p.dispatched_at.elapsed() < PENDING_INSTALL_TTL);
    guard.insert(
        instance_id.to_string(),
        PendingInstall {
            record,
            dispatched_at: std::time::Instant::now(),
        },
    );
}

/// Commit the pending row for `instance_id` when its inventory lands:
/// write the owner record (idempotent — never overwrites an existing
/// row) and consume the entry. No-op when nothing is pending (retained
/// replay of a pre-existing instance).
pub fn commit_pending(
    store: &SharedPendingInstalls,
    owners: &SharedOwnershipStore,
    instance_id: &str,
) {
    let pending = {
        let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(instance_id)
    };
    let Some(p) = pending else { return };
    let mut guard = owners.lock().unwrap_or_else(|e| e.into_inner());
    guard.put_if_absent(instance_id, p.record);
}

/// Drop the pending entry without writing a row (Node reported the
/// operation failed — ADR-087 D7 rollback).
pub fn drop_pending(store: &SharedPendingInstalls, instance_id: &str) {
    let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
    guard.remove(instance_id);
}

pub fn new_shared_node_owners(data_dir: &Path) -> SharedOwnershipStore {
    Arc::new(Mutex::new(OwnershipStore::load(data_dir, "node_owners.json")))
}

pub fn new_shared_agent_owners(data_dir: &Path) -> SharedOwnershipStore {
    Arc::new(Mutex::new(OwnershipStore::load(
        data_dir,
        "agent_owners.json",
    )))
}

/// Lock a shared store, recovering from poisoning (a panic while holding
/// the lock must not deadlock the Gateway; the enrollment stores use the
/// same discipline).
pub fn lock(store: &SharedOwnershipStore) -> std::sync::MutexGuard<'_, OwnershipStore> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// ADR-087: one-shot adoption of ownerless rows by the default owner.
///
/// Covers the windows where the enroll/inventory convergence points could
/// not know an owner yet: first boot (the admin account did not exist when
/// the Gateway spawned its local node) and the local→multi_user migration
/// (pre-switch rows are ownerless by design). The Gateway gates this on a
/// marker file so a deliberately released row (`PATCH owner=null`) is not
/// silently re-adopted on the next restart. Returns the adopted count.
pub fn adopt_ownerless(
    node_owners: &SharedOwnershipStore,
    agent_owners: &SharedOwnershipStore,
    admin_user_id: &str,
) -> usize {
    let mut adopted = 0;
    for store in [&node_owners, &agent_owners] {
        let mut guard = lock(store);
        let ownerless: Vec<String> = guard
            .iter()
            .filter(|(_, rec)| rec.owner_user_id.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ownerless {
            if guard.update(&id, |rec| rec.owner_user_id = Some(admin_user_id.to_string())) {
                adopted += 1;
            }
        }
    }
    adopted
}

/// ADR-087: ownerless row counts per table, for the startup WARN.
pub fn count_ownerless(
    node_owners: &SharedOwnershipStore,
    agent_owners: &SharedOwnershipStore,
) -> (usize, usize) {
    let count = |store: &SharedOwnershipStore| {
        lock(store)
            .iter()
            .filter(|(_, rec)| rec.owner_user_id.is_none())
            .count()
    };
    (count(node_owners), count(agent_owners))
}

/// Same for the optional handle carried on `AppState` — `None` (feature
/// disabled / tests) yields an empty guard-equivalent `None` view.
pub fn lock_shared(
    store: &Option<SharedOwnershipStore>,
) -> Option<std::sync::MutexGuard<'_, OwnershipStore>> {
    store
        .as_ref()
        .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(owner: Option<&str>, guests: &[&str], vis: Visibility) -> OwnerRecord {
        OwnerRecord {
            owner_user_id: owner.map(str::to_string),
            guests: guests.iter().map(|g| g.to_string()).collect(),
            visibility: vis,
            created_at: Utc::now(),
            node_id: None,
        }
    }

    #[test]
    fn can_manage_matrix() {
        let owned = rec(Some("alice"), &["bob"], Visibility::Private);
        let ownerless = rec(None, &[], Visibility::Private);
        // owner / admin → true
        assert!(can_manage(Some(&owned), "alice", false));
        assert!(can_manage(Some(&owned), "admin", true));
        // guest is use-tier, NOT manage (ADR-087 D9)
        assert!(!can_manage(Some(&owned), "bob", false));
        // outsider / wrong-owner → false; ownerless → admin only (fail-closed)
        assert!(!can_manage(Some(&owned), "eve", false));
        assert!(!can_manage(Some(&ownerless), "alice", false));
        assert!(can_manage(Some(&ownerless), "admin", true));
        assert!(!can_manage(None, "alice", false));
        assert!(can_manage(None, "admin", true));
    }

    #[test]
    fn can_transfer_excludes_guests() {
        let owned = rec(Some("alice"), &["bob"], Visibility::Private);
        assert!(can_transfer(Some(&owned), "alice", false));
        assert!(!can_transfer(Some(&owned), "bob", false)); // D9 R1
        assert!(can_transfer(Some(&owned), "admin", true));
        assert!(!can_transfer(None, "alice", false));
    }

    #[test]
    fn can_use_is_manage_or_guest_not_visibility() {
        // shared widens VIEW only; use comes from the guest list (or owner/admin).
        let shared = rec(Some("alice"), &[], Visibility::Shared);
        let private = rec(Some("alice"), &[], Visibility::Private);
        let with_guest = rec(Some("alice"), &["bob"], Visibility::Private);
        // owner → use (manage ⊃ use)
        assert!(can_use(Some(&private), "alice", false));
        // a non-guest is NOT granted use even on a shared agent
        assert!(!can_use(Some(&shared), "carol", false));
        assert!(!can_use(Some(&private), "carol", false));
        // an explicit guest gets use, private or shared alike
        assert!(can_use(Some(&with_guest), "bob", false));
        assert!(can_use(Some(&rec(Some("alice"), &["bob"], Visibility::Shared)), "bob", false));
        // admin → use
        assert!(can_use(Some(&shared), "admin", true));
        // ownerless shared: no owner, no guest → not usable by a non-admin,
        // and certainly not manageable (fail-closed)
        let ownerless_shared = rec(None, &[], Visibility::Shared);
        assert!(!can_use(Some(&ownerless_shared), "carol", false));
        assert!(!can_manage(Some(&ownerless_shared), "carol", false));
    }

    /// ADR-087 D7 fail-closed: an ownerless / unrecorded resource is
    /// admin-only, therefore it must not be LISTED for anyone else.
    /// This is the regression that let every agent missing from
    /// `agent_owners.json` show up in every account's sidebar.
    #[test]
    fn can_view_is_fail_closed_on_missing_record() {
        // No row at all (legacy / unclaimed) — invisible to a normal
        // account, visible to admin so it can be found and claimed.
        assert!(!can_view(None, "carol", false));
        assert!(can_view(None, "admin", true));
        // A row that exists but has no owner behaves the same way
        // (ownerless == admin-only per D7).
        let ownerless = rec(None, &[], Visibility::Private);
        assert!(!can_view(Some(&ownerless), "carol", false));
        assert!(can_view(Some(&ownerless), "admin", true));
        // A guest on an ownerless row still sees it — guest grants use,
        // and can_view keys off can_use, so the granted row stays reachable.
        let ownerless_guest = rec(None, &["carol"], Visibility::Private);
        assert!(can_view(Some(&ownerless_guest), "carol", false));
    }

    /// The tier order: manage (owner ∨ guest) ∨ published.
    #[test]
    fn can_view_tiers() {
        let private = rec(Some("alice"), &["bob"], Visibility::Private);
        // owner and guest both keep the row despite `private`.
        assert!(can_view(Some(&private), "alice", false));
        assert!(can_view(Some(&private), "bob", false));
        // a stranger does not.
        assert!(!can_view(Some(&private), "carol", false));
        // published (shared agent / public node) opens it up to all.
        assert!(can_view(
            Some(&rec(Some("alice"), &[], Visibility::Shared)),
            "carol",
            false
        ));
        assert!(can_view(
            Some(&rec(Some("alice"), &[], Visibility::Public)),
            "carol",
            false
        ));
        // Published never implies manage — that stays owner/guest/admin.
        assert!(!can_manage(
            Some(&rec(Some("alice"), &[], Visibility::Shared)),
            "carol",
            false
        ));
    }

    /// `Shared` and `Public` are the same tier under different wire
    /// spellings (agents vs nodes) — `can_view` must not care which.
    #[test]
    fn is_published_covers_both_spellings() {
        assert!(!Visibility::Private.is_published());
        assert!(Visibility::Shared.is_published());
        assert!(Visibility::Public.is_published());
    }

    #[test]
    fn visibility_absent_deserializes_private() {
        let json = r#"{"owner_user_id":"alice","created_at":"2026-01-01T00:00:00Z"}"#;
        let rec: OwnerRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.visibility, Visibility::Private);
        assert!(rec.guests.is_empty());
    }

    #[test]
    fn store_roundtrip_and_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = OwnershipStore::load(dir.path(), "node_owners.json");
        store.put("n1", rec(Some("alice"), &[], Visibility::Private));
        // put_if_absent must not overwrite (re-enroll keeps owner)
        store.put_if_absent("n1", rec(Some("mallory"), &[], Visibility::Private));
        store.put_if_absent("n2", rec(Some("carol"), &[], Visibility::Private));
        assert_eq!(store.get("n1").unwrap().owner_user_id.as_deref(), Some("alice"));
        assert_eq!(store.get("n2").unwrap().owner_user_id.as_deref(), Some("carol"));
        // update persists and reload sees it
        assert!(store.update("n1", |r| r.guests.push("bob".to_string())));
        assert!(!store.update("nope", |_| {}));
        drop(store);
        let reloaded = OwnershipStore::load(dir.path(), "node_owners.json");
        assert_eq!(reloaded.get("n1").unwrap().guests, vec!["bob".to_string()]);
        // remove is idempotent
        let mut reloaded = reloaded;
        reloaded.remove("n1");
        reloaded.remove("n1");
        assert!(reloaded.get("n1").is_none());
    }

    #[test]
    fn adopt_ownerless_binds_only_ownerless_rows() {
        // ADR-087 one-shot adoption: ownerless rows go to the admin,
        // owned rows are untouched. (Restart protection against
        // re-adopting a deliberately released row is the marker file in
        // `gateway/mod.rs`, not the store's job.)
        let dir = crate::test_support::unique_temp_dir(format!(
            "acowork-test-adopt-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let nodes = new_shared_node_owners(&dir);
        let agents = new_shared_agent_owners(&dir);
        lock(&nodes).put("n-owned", OwnerRecord::new(Some("alice".into())));
        lock(&nodes).put("n-free", OwnerRecord::new(None));
        lock(&agents).put("a-free", OwnerRecord::new(None));
        let adopted = adopt_ownerless(&nodes, &agents, "admin-1");
        assert_eq!(adopted, 2);
        assert_eq!(
            lock(&nodes).get("n-owned").unwrap().owner_user_id.as_deref(),
            Some("alice"),
            "an owned row is never re-bound"
        );
        assert_eq!(
            lock(&nodes).get("n-free").unwrap().owner_user_id.as_deref(),
            Some("admin-1")
        );
        assert_eq!(
            lock(&agents).get("a-free").unwrap().owner_user_id.as_deref(),
            Some("admin-1")
        );
        let (n, a) = count_ownerless(&nodes, &agents);
        assert_eq!((n, a), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
