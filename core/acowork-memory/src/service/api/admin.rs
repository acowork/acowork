//! Admin write entrypoints (ADR-101 P1e) — the manifest bootstrap import.
//!
//! This is **write entrypoint D** ([`docs/memory-write-entrypoints.md`]): the
//! only memory writes that are not a tool call, a panel edit or a distiller run.
//! The Runtime used to call
//! `acowork-runtime::agent::bootstrap_autobiographical_from_manifest` with the
//! parsed manifest in hand; once memory left the Runtime (P3) the derivation had
//! to come along with the data it writes, because "which manifest fields are
//! memories" *is* memory business logic. The caller therefore sends the declared
//! facts and this route decides what becomes a node.
//!
//! Scope stays narrow on purpose (ADR-068 M8): only categories **statically
//! declared** in the manifest — Identity and Capability — are bootstrapped.
//! Relationship, Limitation, Preference and History need observational evidence
//! that only emerges at runtime and belong to the distiller.
//!
//! The sibling entrypoint, `POST /admin/import` (the one-shot upload of a legacy
//! `{work_dir}/memory/private.sqlite`, ADR-101 decision 10), is in
//! [`crate::service::api::import_legacy`]: it moves whole stored histories
//! instead of deriving nodes from declared facts, so it has its own shape and
//! its own idempotency gate.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::service::api::{InstanceQuery, instance_store};
use crate::service::error::Result;
use crate::sqlite::SqliteStore;
use crate::service::state::MemoryState;
use crate::{AutobioCategory, AutobiographicalNode, NodeStatus};

/// The `source` stamp every bootstrapped node carries.
///
/// It is what distinguishes an authoritative manifest fact from a user
/// statement on the panel and in the distiller's conflict handling, so the
/// string is part of the contract, not a label.
const MANIFEST_SOURCE: &str = "manifest";

/// One declared capability — only its description is bootstrapped.
#[derive(Debug, Deserialize)]
pub struct CapabilityDecl {
    #[serde(default)]
    pub description: String,
}

/// Request body of `POST /admin/bootstrap`: the manifest facts the caller
/// declares about itself.
///
/// Deliberately **not** the whole `AgentManifest` — the service must not grow a
/// dependency on packaging surface it does not read, and a narrower contract
/// keeps P3's caller honest about what it is handing over. Unknown fields are
/// accepted rather than rejected: this runs on the agent startup path, where a
/// 400 because a newer Runtime gained a manifest field would be pure noise.
#[derive(Debug, Deserialize)]
pub struct BootstrapBody {
    /// Package identity (ADR-073: not an instance id — the namespace comes from
    /// `?instance_id=`).
    pub agent_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub role: Option<String>,
    /// Declared capabilities. A `BTreeMap` so node insertion order is stable
    /// across runs (the manifest's own `HashMap` is not).
    #[serde(default)]
    pub capabilities: BTreeMap<String, CapabilityDecl>,
    /// The package author's `[memory]` policy.
    ///
    /// Applied **only** when this request actually imports (see the handler),
    /// so an agent restart can never overwrite the panel's saved choices in
    /// `consolidation.json`. `None` when the manifest declares no policy.
    #[serde(default)]
    pub memory: Option<ManifestPolicy>,
}

/// Policy half of the bootstrap body — the service's own patch shape, so the
/// caller cannot invent keys the pipeline would ignore.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPolicy {
    #[serde(default)]
    pub distiller: super::consolidation::DistillerPatch,
}

/// Identity `(key, value)` pairs, in the fixed order the Runtime wrote them.
///
/// Blank values are dropped: an empty statement is not a memory
/// (`docs/memory-write-entrypoints.md` core decision), and the old code writing
/// whatever the manifest happened to contain was an accident rather than a rule.
fn identity_entries(body: &BootstrapBody) -> Vec<(&'static str, String)> {
    let mut entries = vec![
        ("agent_id", body.agent_id.clone()),
        ("name", body.name.clone()),
        ("description", body.description.clone()),
    ];
    if let Some(role) = body.role.clone() {
        entries.push(("role", role));
    }
    entries
        .into_iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .collect()
}

/// A bootstrapped node: full confidence, no source episode, `manifest` source.
fn autobio_node(
    category: AutobioCategory,
    key: String,
    value: String,
    now: DateTime<Utc>,
) -> AutobiographicalNode {
    AutobiographicalNode {
        id: None,
        category,
        key,
        value,
        confidence: 1.0,
        source_episode_id: None,
        source_episode_ids: Vec::new(),
        promotion_metadata: None,
        // The direct write path does not embed: self-knowledge is promoted and
        // re-embedded by the consolidation pipeline (P1e), not here.
        embedding: None,
        status: NodeStatus::Active,
        created_at: now,
        updated_at: now,
        source: MANIFEST_SOURCE.to_string(),
        metadata: Default::default(),
    }
}

/// Merge the manifest policy into the instance's saved settings.
///
/// Best-effort: an invalid patch is logged and skipped, because the bootstrap
/// itself already succeeded and the agent must stay usable with the service
/// defaults. Called only on first import, so the authoritative source stays the
/// panel once the user has touched the cards.
fn apply_policy(state: &MemoryState, instance_id: &str, policy: &ManifestPolicy) {
    let pipeline = state.pipeline();
    let mut settings = pipeline.settings(instance_id);
    policy.distiller.apply(&mut settings.distiller);
    if let Err(e) = super::consolidation::validate_settings(&settings) {
        tracing::warn!(instance_id, error = %e, "manifest memory policy rejected");
        return;
    }
    if let Err(e) = pipeline.save_settings(instance_id, &settings) {
        tracing::warn!(instance_id, error = %e, "failed to persist manifest memory policy");
        return;
    }
    tracing::info!(instance_id, "applied manifest memory policy");
}

/// `POST /admin/bootstrap?instance_id=` — sync the declared static
/// self-knowledge into one instance namespace.
///
/// **Idempotent by convergence, not by seeding once.** The caller sends the
/// same facts on every start and this route makes the store agree with them:
/// creating what is missing and updating what changed. Seeding once is not
/// enough, because some of these facts are owned elsewhere — the instance's
/// `name` lives in its `agent_config.json` and its owner may rename it between
/// two starts, so a write-once policy would leave the agent recognizing itself
/// under a name nobody uses any more (`docs/memory-write-entrypoints.md`,
/// ADR-111 D7).
///
/// Individual write failures are logged and skipped rather than aborting the
/// run — a partial bootstrap still leaves the agent usable, and the panel shows
/// what is missing.
pub async fn bootstrap(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
    Json(body): Json<BootstrapBody>,
) -> Result<Json<Value>> {
    let (instance_id, store) = instance_store(&state, query.as_deref())?;

    let now = Utc::now();
    let existing_identity = store.find_autobiographical_by_category(AutobioCategory::Identity)?;
    // "First import" is the one condition under which the package's declared
    // memory policy is still the authority over the panel's saved settings.
    let first_import = existing_identity.is_empty();
    let (identity_written, identity_updated) = sync_declared(
        &store,
        &existing_identity,
        AutobioCategory::Identity,
        identity_entries(&body)
            .into_iter()
            .map(|(key, value)| (key.to_string(), value)),
        now,
    );

    let capabilities: Vec<(String, String)> = body
        .capabilities
        .iter()
        .filter(|(key, decl)| !key.trim().is_empty() && !decl.description.trim().is_empty())
        .map(|(key, decl)| (key.clone(), decl.description.clone()))
        .collect();
    let existing_capabilities =
        store.find_autobiographical_by_category(AutobioCategory::Capability)?;
    let (capability_written, capability_updated) = sync_declared(
        &store,
        &existing_capabilities,
        AutobioCategory::Capability,
        capabilities,
        now,
    );

    if first_import && let Some(policy) = body.memory.as_ref() {
        apply_policy(&state, &instance_id, policy);
    }

    tracing::info!(
        instance_id = %instance_id,
        identity_written,
        identity_updated,
        capability_written,
        capability_updated,
        "synced Autobiographical/Identity+Capability nodes from the manifest"
    );
    Ok(Json(json!({
        "identity_written": identity_written,
        "identity_updated": identity_updated,
        "capability_written": capability_written,
        "capability_updated": capability_updated,
        // Nothing had to be written: the store already agreed with the caller.
        "skipped_existing": identity_written == 0 && identity_updated == 0,
    })))
}

/// Make the store agree with one category of declared facts.
///
/// Returns `(created, updated)`. Per declared key:
///
/// - **absent** → create it. The caller declares it, so it is ours to write.
/// - **present with [`MANIFEST_SOURCE`], value changed** → update in place. The
///   declaration is the younger fact: it carries the name the instance has now.
/// - **present with [`MANIFEST_SOURCE`], value identical** → leave it, so a
///   restart does not bump `updated_at` for nothing.
/// - **present with any other source** → leave it. Evidence-derived and
///   imported facts (the distiller's `self_evaluation`, a legacy import) are not
///   the caller's to overwrite; its declaration is only the *default*.
///
/// A key the caller stops declaring is **not** deleted. Bootstrap owns the keys
/// it declares, not their absence: a manifest that drops `role` is not a
/// statement that the agent has no role, and retiring a self-fact is a decision
/// for a human on the panel.
fn sync_declared(
    store: &SqliteStore,
    existing: &[AutobiographicalNode],
    category: AutobioCategory,
    declared: impl IntoIterator<Item = (String, String)>,
    now: DateTime<Utc>,
) -> (usize, usize) {
    let mut created = 0usize;
    let mut updated = 0usize;
    for (key, value) in declared {
        match existing.iter().find(|node| node.key == key) {
            Some(node) if node.source == MANIFEST_SOURCE => {
                if node.value == value {
                    continue;
                }
                let mut next = node.clone();
                next.value = value;
                if let Err(e) = store.update_autobiographical(&next) {
                    tracing::warn!(key = %key, error = %e, "failed to refresh a declared self-fact");
                    continue;
                }
                updated += 1;
            }
            Some(node) => {
                tracing::debug!(
                    key = %key,
                    source = %node.source,
                    "self-fact has a non-manifest source, leaving it alone"
                );
            }
            None => {
                let node = autobio_node(category.clone(), key.clone(), value, now);
                match store.store_autobiographical(&node) {
                    Ok(_) => created += 1,
                    Err(e) => {
                        tracing::warn!(key = %key, error = %e, "failed to bootstrap a declared self-fact");
                    }
                }
            }
        }
    }
    (created, updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::config::{EmbeddingSection, MemoryServiceConfig};

    fn body() -> BootstrapBody {
        serde_json::from_value(json!({
            "agent_id": "com.acowork.senior-engineer",
            "name": "Senior Engineer",
            "description": "Writes and reviews production Rust",
            "role": "Engineer",
            "capabilities": {
                "code_review": { "description": "Reviews diffs for defects" },
                "empty_one": { "description": "   " }
            }
        }))
        .unwrap()
    }

    fn state_in(dir: &std::path::Path) -> MemoryState {
        MemoryState::new(MemoryServiceConfig {
            data_dir: dir.to_path_buf(),
            // Nothing here embeds, but keep the test off the network in case
            // that changes (see the note in `service::mcp`'s test harness).
            embedding: EmbeddingSection {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap()
    }

    async fn run(state: &MemoryState, id: &str) -> Value {
        let Json(value) = bootstrap(
            State(state.clone()),
            Query(InstanceQuery {
                instance_id: Some(id.to_string()),
            }),
            Json(body()),
        )
        .await
        .expect("bootstrap");
        value
    }

    #[test]
    fn identity_entries_keep_the_manifest_order_and_drop_blanks() {
        let mut b = body();
        b.description = "  ".to_string();
        b.role = None;
        let keys: Vec<&str> = identity_entries(&b).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["agent_id", "name"]);
    }

    #[tokio::test]
    async fn writes_identity_and_capability_nodes() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = state_in(dir.path());
        let id = "12121212-1212-4122-8122-121212121212";

        let out = run(&state, id).await;
        assert_eq!(out["identity_written"], 4);
        // The blank-description capability is not a memory.
        assert_eq!(out["capability_written"], 1);
        assert_eq!(out["skipped_existing"], false);

        let store = state.store(id).unwrap();
        let identity = store
            .find_autobiographical_by_category(AutobioCategory::Identity)
            .unwrap();
        assert_eq!(identity.len(), 4);
        assert!(
            identity.iter().all(|n| n.source == MANIFEST_SOURCE),
            "bootstrapped nodes are stamped as authoritative"
        );
        assert!(
            identity
                .iter()
                .any(|n| n.key == "agent_id" && n.value == "com.acowork.senior-engineer")
        );
        let caps = store
            .find_autobiographical_by_category(AutobioCategory::Capability)
            .unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].key, "code_review");
    }

    #[tokio::test]
    async fn a_second_bootstrap_is_skipped_and_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = state_in(dir.path());
        let id = "13131313-1313-4133-8133-131313131313";

        run(&state, id).await;
        let out = run(&state, id).await;
        assert_eq!(out["skipped_existing"], true);
        assert_eq!(out["identity_written"], 0);

        let store = state.store(id).unwrap();
        assert_eq!(
            store
                .find_autobiographical_by_category(AutobioCategory::Identity)
                .unwrap()
                .len(),
            4,
            "no duplicate identity facts on the second agent start"
        );
    }

    /// The instance name is owned by `agent_config.json`, and the owner may
    /// rename the agent between two starts. Seeding once would leave the agent
    /// answering to a name nobody uses — the exact mismatch ADR-073 forbids.
    #[tokio::test]
    async fn a_rename_converges_the_declared_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = state_in(dir.path());
        let id = "16161616-1616-4166-8166-161616161616";

        run(&state, id).await;
        let mut renamed = body();
        renamed.name = "Senior.Engineer".to_string();
        let Json(out) = bootstrap(
            State(state.clone()),
            Query(InstanceQuery {
                instance_id: Some(id.to_string()),
            }),
            Json(renamed),
        )
        .await
        .expect("second bootstrap");
        assert_eq!(out["identity_updated"], 1, "{out}");
        assert_eq!(out["identity_written"], 0, "{out}");
        assert_eq!(out["skipped_existing"], false, "{out}");

        let store = state.store(id).unwrap();
        let names: Vec<String> = store
            .find_autobiographical_by_category(AutobioCategory::Identity)
            .unwrap()
            .into_iter()
            .filter(|n| n.key == "name")
            .map(|n| n.value)
            .collect();
        assert_eq!(names, vec!["Senior.Engineer"], "refreshed in place, not appended");
    }

    /// A fact the store holds on other evidence is not the manifest's to
    /// rewrite: the declaration is the default, the observation is the finding.
    #[tokio::test]
    async fn a_fact_from_another_source_survives_the_sync() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = state_in(dir.path());
        let id = "17171717-1717-4177-8177-171717171717";

        let store = state.store(id).unwrap();
        store
            .store_autobiographical(&AutobiographicalNode {
                category: AutobioCategory::Identity,
                key: "name".to_string(),
                value: "What They Call Me".to_string(),
                source: "self_evaluation".to_string(),
                ..autobio_node(AutobioCategory::Identity, "name".into(), "x".into(), Utc::now())
            })
            .unwrap();

        let Json(out) = bootstrap(
            State(state.clone()),
            Query(InstanceQuery {
                instance_id: Some(id.to_string()),
            }),
            Json(body()),
        )
        .await
        .expect("bootstrap");
        assert_eq!(out["identity_updated"], 0, "{out}");

        let kept: Vec<String> = store
            .find_autobiographical_by_category(AutobioCategory::Identity)
            .unwrap()
            .into_iter()
            .filter(|n| n.key == "name")
            .map(|n| n.value)
            .collect();
        assert_eq!(kept, vec!["What They Call Me"]);
    }

    /// Convergence is per key: an unchanged fact is not rewritten just because
    /// a sibling changed (that would bump `updated_at` on every start).
    #[tokio::test]
    async fn an_unchanged_fact_is_not_rewritten() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = state_in(dir.path());
        let id = "18181818-1818-4188-8188-181818181818";

        run(&state, id).await;
        let out = run(&state, id).await;
        assert_eq!(out["identity_updated"], 0, "{out}");
        assert_eq!(out["capability_updated"], 0, "{out}");
        assert_eq!(out["skipped_existing"], true, "{out}");
    }

    #[tokio::test]
    async fn bootstraps_one_instance_without_touching_another() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = state_in(dir.path());
        let a = "14141414-1414-4144-8144-141414141414";
        let b = "15151515-1515-4155-8155-151515151515";

        run(&state, a).await;
        let store_b = state.store(b).unwrap();
        assert!(
            store_b
                .find_autobiographical_by_category(AutobioCategory::Identity)
                .unwrap()
                .is_empty(),
            "the bootstrap is namespaced, not global"
        );
    }

    #[tokio::test]
    async fn an_unusable_instance_id_never_reaches_a_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = state_in(dir.path());
        let err = bootstrap(
            State(state.clone()),
            Query(InstanceQuery {
                instance_id: Some("../escape".to_string()),
            }),
            Json(body()),
        )
        .await
        .expect_err("must reject");
        assert_eq!(
            err.http_status(),
            axum::http::StatusCode::BAD_REQUEST,
            "{err:?}"
        );
        assert!(state.instances().list().is_empty());
    }
}

