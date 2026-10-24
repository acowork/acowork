//! Memory lifecycle end-to-end: backfill -> consolidation -> forgetting.
//!
//! The three chains were each verified separately (distiller semantics in
//! `memory_adr068_e2e`, the manual HTTP trigger in `memory/adr071_e2e`, storage
//! in `acowork-sqlite/tests/memory_chains_e2e`). What was never checked is the
//! sequence the user actually depends on: an agent that has been writing
//! episodes for weeks whose sediment layer is still empty. That is the incident
//! this rewrite exists to fix, and it is a *lifecycle* property, not a
//! distiller property.
//!
//! 1. **backfill** (M3) - episodes stored before the `normalized` field existed
//!    consolidate on the next run, with no LLM and no re-extraction. A
//!    permanent test, not a throwaway script: the `content` fallback is a code
//!    path, so it must stay covered.
//! 2. **consolidation** - the sediment is retrievable, and a second run does
//!    not duplicate it.
//! 3. **forgetting** - the episodic layer ages out *after* consolidation
//!    without taking the sediment with it. Forgetting an episode must not
//!    forget what was learned from it.
//!
//! Run: `cargo test -p acowork-memory --test memory_lifecycle_e2e`

use std::collections::HashMap;
use std::sync::Arc;

use acowork_core::EmbeddingProvider;
use acowork_memory::consolidation::{DefaultEpisodicDistiller, DistillerConfig, EpisodicDistiller};
use acowork_memory::types::{
    DEFAULT_EMBEDDING_DIM, Episode, EpisodicDecayConfig, KnowledgeSubType,
};
use acowork_memory::{
    MemoryManager, MemoryManagerConfig, MemoryProvider, MemoryQuery, NodeStatus, labels,
};
use acowork_memory::sqlite::SqliteStore;
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};

// ============================================================================
// Harness
// ============================================================================

/// The same fallback chain production uses, so a test vector and a production
/// vector for the same text are byte-identical.
struct DeterministicEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for DeterministicEmbedding {
    fn name(&self) -> &str {
        "deterministic-lifecycle-e2e"
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, acowork_core::EmbeddingError> {
        Ok(acowork_memory::manager::procedural_embedding_fallback(text))
    }
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, acowork_core::EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
    fn dimension(&self) -> usize {
        DEFAULT_EMBEDDING_DIM
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Lifecycle {
    dir: tempfile::TempDir,
    store: Arc<SqliteStore>,
}

impl Lifecycle {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            SqliteStore::open(dir.path().join("private.sqlite"), DEFAULT_EMBEDDING_DIM)
                .expect("open store"),
        );
        Self { dir, store }
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.dir.path().join("private.sqlite")
    }

    fn provider(&self) -> Arc<dyn MemoryProvider> {
        Arc::clone(&self.store) as Arc<dyn MemoryProvider>
    }

    /// Store an episode the way the current write path does, then strip the
    /// `normalized` key out of `props` so the row is shape-identical to one
    /// written before the field existed.
    ///
    /// Going through `store_episode` and then editing `props` keeps the FTS and
    /// `vectors` indexes consistent for free; hand-writing the `INSERT` would
    /// test a store shape production cannot produce.
    fn seed_legacy_episode(
        &self,
        content: &str,
        subtype: KnowledgeSubType,
        ts: DateTime<Utc>,
        with_embedding: bool,
    ) -> u64 {
        let embedding =
            with_embedding.then(|| acowork_memory::manager::procedural_embedding_fallback(content));
        let id = self
            .provider()
            .store_episode(&Episode {
                session_id: "com.test.lifecycle".to_string(),
                turn_index: 0,
                role: "assistant".to_string(),
                content: content.to_string(),
                embedding,
                timestamp: ts,
                consolidated: false,
                metadata: HashMap::new(),
                importance: 0.6,
                knowledge_subtype: Some(subtype),
                normalized: None,
            })
            .expect("store_episode ok");
        self.rewrite_props(id, "json_remove(props, '$.normalized')");
        id
    }

    /// `encode()` serializes `Option` fields, so `normalized: None` is stored
    /// as `"normalized": null`. The live legacy rows have no such key at all -
    /// removing it is what makes this a backfill test and not merely a `None`
    /// test.
    /// Age a node in place. The decay scan reads `props.created_at` and falls
    /// back to `props.timestamp` — episodes serialize the latter, semantic
    /// nodes the former — so "old" has to be written into both keys or the
    /// helper silently ages only one label. That difference is exactly what
    /// separates a forgetting test that bites from one that passes for free.
    fn age_node(&self, id: u64, days: i64) {
        let ts =
            (Utc::now() - ChronoDuration::days(days)).to_rfc3339_opts(SecondsFormat::Secs, true);
        self.rewrite_props(
            id,
            &format!("json_set(json_set(props, '$.timestamp', '{ts}'), '$.created_at', '{ts}')"),
        );
    }

    fn rewrite_props(&self, id: u64, expr: &str) {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open for props rewrite");
        conn.execute(
            &format!("UPDATE nodes SET props = {expr} WHERE id = ?1"),
            rusqlite::params![id as i64],
        )
        .expect("rewrite props");
    }

    /// Node ids of one label. Used to age the *sediment* as well as the
    /// episodes, so a test that asserts the sediment survived can only pass
    /// because the scan excludes semantic labels - not because the semantic
    /// nodes happened to be young.
    fn ids_by_label(&self, label: &str) -> Vec<u64> {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open for id scan");
        let mut stmt = conn
            .prepare("SELECT id FROM nodes WHERE label = ?1")
            .expect("prepare id scan");
        let ids = stmt
            .query_map(rusqlite::params![label], |r| r.get::<_, i64>(0))
            .expect("query ids")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect ids");
        ids.into_iter().map(|i| i as u64).collect()
    }

    fn sediment_count(&self) -> u64 {
        self.store
            .node_count_by_label(labels::KNOWLEDGE)
            .expect("knowledge count")
            + self
                .store
                .node_count_by_label(labels::PROCEDURAL)
                .expect("procedural count")
    }

    async fn status(&self, id: u64) -> Option<NodeStatus> {
        self.provider().get_node_status(id).expect("status read ok")
    }
    /// Rows copied into `purge_log`. Forgetting archives before it deletes, so
    /// this is how a test tells "forgotten" apart from "lost".
    fn purged_ids(&self) -> Vec<u64> {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open for purge scan");
        let mut stmt = conn
            .prepare("SELECT node_id FROM purge_log")
            .expect("prepare purge scan");
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .expect("query purge ids")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect purge ids");
        ids.into_iter().map(|i| i as u64).collect()
    }

    /// Move `dormant_since` to `days` in the past, i.e. how long a node has
    /// already been sitting in the Dormant grace period.
    fn set_dormant_since(&self, id: u64, days: i64) {
        let ts =
            (Utc::now() - ChronoDuration::days(days)).to_rfc3339_opts(SecondsFormat::Secs, true);
        self.rewrite_props(id, &format!("json_set(props, '$.dormant_since', '{ts}')"));
    }
}

// ============================================================================
// M3 - backfill
// ============================================================================

/// The live backlog's shape: 269 episodes, 262 with a `knowledge_subtype`, and
/// only 167 carrying a vector. A backfill must drain all of them - including
/// the unvectorised ones - without ever calling a model.
#[tokio::test]
async fn backfill_consolidates_legacy_episodes_without_a_model() {
    let lc = Lifecycle::new();
    let now = Utc::now();

    let shanghai =
        lc.seed_legacy_episode("User lives in Shanghai", KnowledgeSubType::Fact, now, true);
    let concise = lc.seed_legacy_episode(
        "User prefers concise answers",
        KnowledgeSubType::Preference,
        now - ChronoDuration::days(1),
        false,
    );
    let cargo = lc.seed_legacy_episode(
        "When editing Rust, run cargo check before committing",
        KnowledgeSubType::Procedure,
        now - ChronoDuration::days(2),
        true,
    );
    let tranxon = lc.seed_legacy_episode(
        "User works at Tranxon",
        KnowledgeSubType::Fact,
        now - ChronoDuration::days(3),
        false,
    );

    assert_eq!(
        lc.provider()
            .count_unconsolidated_episodes()
            .expect("count"),
        4,
        "legacy rows are visible to the distiller's scan"
    );

    let result = DefaultEpisodicDistiller
        .run(
            lc.provider().as_ref(),
            None,
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("distiller run ok");

    assert_eq!(result.episodes_scanned, 4);
    assert_eq!(result.funnel.llm_calls, 0, "backfill costs no tokens");
    assert_eq!(
        result.funnel.projected, 4,
        "every legacy episode takes the projection path: {:?}",
        result.funnel
    );
    assert_eq!(result.funnel.episodes_deferred, 0);
    assert!(
        result.funnel.errors.is_empty(),
        "backfill errors: {:?}",
        result.funnel.errors
    );

    // The statement came from `content`, because `props` has no `normalized`.
    let (_, node) = lc
        .store
        .find_knowledge_by_subject("user", "user_lives_in_shanghai")
        .expect("lookup ok")
        .expect("a Fact with no normalized field still projects from content");
    assert_eq!(node.object, "User lives in Shanghai");
    assert_eq!(
        node.source_episode_ids,
        vec![shanghai],
        "provenance survives the backfill"
    );
    assert_eq!(
        node.promotion_metadata
            .as_ref()
            .expect("promotion metadata")
            .evidence_episode_ids,
        vec![shanghai]
    );

    // Preference and Fact share the Knowledge label; Procedure does not.
    assert_eq!(
        lc.store
            .node_count_by_label(labels::KNOWLEDGE)
            .expect("count"),
        3,
        "two Facts + one Preference"
    );
    assert_eq!(
        lc.store
            .node_count_by_label(labels::PROCEDURAL)
            .expect("count"),
        1,
        "the Procedure episode projects into its own label"
    );
    let procedure = lc
        .store
        .get_all_procedural_nodes()
        .expect("procedural list ok")
        .into_iter()
        .find(|n| n.source_episode_ids.contains(&cargo))
        .expect("the Procedure episode projected into a ProceduralNode");
    assert_eq!(
        procedure.action_pattern,
        "When editing Rust, run cargo check before committing"
    );

    // The unvectorised episodes projected too - they simply had no candidate to
    // recall, which is the cheapest path, not a skipped one.
    let (_, pref) = lc
        .store
        .find_knowledge_by_subject("user", "user_prefers_concise_answers")
        .expect("lookup ok")
        .expect("the vectorless Preference episode still projected");
    assert_eq!(pref.source_episode_ids, vec![concise]);
    let (_, fact2) = lc
        .store
        .find_knowledge_by_subject("user", "user_works_at_tranxon")
        .expect("lookup ok")
        .expect("the vectorless Fact episode still projected");
    assert_eq!(fact2.source_episode_ids, vec![tranxon]);

    assert_eq!(
        lc.provider()
            .count_unconsolidated_episodes()
            .expect("count"),
        0,
        "the backlog drained"
    );
}

/// A backfill run that produced nothing is indistinguishable from an idle
/// agent, which is how the original incident stayed hidden for days. The empty
/// case must report zero, not error.
#[tokio::test]
async fn an_empty_backlog_reports_zero_rather_than_failing() {
    let lc = Lifecycle::new();
    let result = DefaultEpisodicDistiller
        .run(
            lc.provider().as_ref(),
            None,
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("an empty store is not an error");
    assert_eq!(result.episodes_scanned, 0);
    assert_eq!(result.funnel.projected, 0);
    assert_eq!(result.funnel.llm_calls, 0);
    assert!(result.funnel.errors.is_empty());
    assert_eq!(lc.sediment_count(), 0);
}

// ============================================================================
// Consolidation
// ============================================================================

/// Restating the same preference three times must not create three nodes. The
/// newest episode projects first, the older two recall it and fold in, and all
/// three episode ids stay on the one node.
///
/// The scripted model reads the candidate id out of its own request rather
/// than hard-coding one: the store assigns node ids, the test does not.
#[tokio::test]
async fn restatements_fold_into_one_recallable_node() {
    let lc = Lifecycle::new();
    let now = Utc::now();
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(lc.seed_legacy_episode(
            "User prefers dark mode over light mode",
            KnowledgeSubType::Preference,
            now - ChronoDuration::days(i * 7),
            true,
        ));
    }

    let llm = FoldIntoFirstCandidate;
    let embedding_fn: acowork_memory::consolidation::EmbeddingFn =
        Arc::new(acowork_memory::manager::procedural_embedding_fallback);

    let result = DefaultEpisodicDistiller
        .run(
            lc.provider().as_ref(),
            Some(&llm),
            Some(&embedding_fn),
            &DistillerConfig::default(),
        )
        .await
        .expect("distiller run ok");

    assert_eq!(result.episodes_scanned, 3);
    assert_eq!(
        lc.store
            .node_count_by_label(labels::KNOWLEDGE)
            .expect("count"),
        1,
        "three restatements, one node - funnel: {:?}",
        result.funnel
    );
    let (_, node) = lc
        .store
        .find_knowledge_by_subject("user", "user_prefers_dark_mode_over_light_mode")
        .expect("lookup ok")
        .expect("the folded node is addressable by its slug");
    for id in &ids {
        assert!(
            node.source_episode_ids.contains(id),
            "every folded episode must stay traceable, got {:?}",
            node.source_episode_ids
        );
    }
    assert_eq!(
        result.funnel.projected, 1,
        "only the newest was a lone voice"
    );
    assert_eq!(result.funnel.verdict_merged, 2);
}

/// The sediment is only worth having if `memory_recall` can surface it.
#[tokio::test]
async fn consolidated_sediment_is_recallable() {
    let lc = Lifecycle::new();
    lc.seed_legacy_episode(
        "User prefers concise answers over long explanations",
        KnowledgeSubType::Preference,
        Utc::now(),
        false,
    );
    DefaultEpisodicDistiller
        .run(
            lc.provider().as_ref(),
            None,
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("consolidate");

    let manager = MemoryManager::new(MemoryManagerConfig::default());
    let mut query = MemoryQuery::new("concise answers".to_string());
    let retrieved = manager
        .retrieve(&*lc.store, &mut query, Some(&DeterministicEmbedding))
        .await
        .expect("retrieve ok");
    assert!(
        retrieved
            .memories
            .iter()
            .any(|m| m.label == labels::KNOWLEDGE && m.content.contains("concise")),
        "the sediment must be recallable, got: {:?}",
        retrieved
            .memories
            .iter()
            .map(|m| (m.label.clone(), m.content.clone()))
            .collect::<Vec<_>>()
    );

    // A re-run over the drained backlog must not add a second copy.
    let second = DefaultEpisodicDistiller
        .run(
            lc.provider().as_ref(),
            None,
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("second run ok");
    assert_eq!(second.episodes_scanned, 0);
    assert_eq!(lc.sediment_count(), 1, "a re-run must not add sediment");
}

// ============================================================================
// Forgetting
// ============================================================================

/// The invariant forgetting has to hold: aging out the episodic evidence must
/// not take the consolidated knowledge with it. Without this test the two
/// chains are only ever verified in isolation, and "we deleted the episodes the
/// sediment was derived from" looks like a success in both of them.
#[tokio::test]
async fn forgetting_the_episode_keeps_the_sediment() {
    let lc = Lifecycle::new();
    let now = Utc::now();
    let hangzhou =
        lc.seed_legacy_episode("User lives in Hangzhou", KnowledgeSubType::Fact, now, false);
    let diff = lc.seed_legacy_episode(
        "Summarise the diff before proposing a refactor",
        KnowledgeSubType::Procedure,
        now,
        false,
    );

    DefaultEpisodicDistiller
        .run(
            lc.provider().as_ref(),
            None,
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("consolidate");
    assert_eq!(lc.sediment_count(), 2, "one Knowledge + one Procedural");

    // Age *everything*, including the sediment. If only the episodes were old,
    // "the sediment survived" would say nothing about whether the scan can
    // reach semantic labels at all.
    let sediment_ids = [
        lc.ids_by_label(labels::KNOWLEDGE),
        lc.ids_by_label(labels::PROCEDURAL),
    ]
    .concat();
    assert_eq!(sediment_ids.len(), 2);
    for id in [vec![hangzhou, diff], sediment_ids.clone()].concat() {
        lc.age_node(id, 4000);
    }

    // Active -> Dormant, then Dormant -> archived. Two scans, because the
    // archive step only ever touches nodes a previous scan already decayed.
    let decayed = lc
        .provider()
        .run_episodic_decay_scan(&EpisodicDecayConfig {
            enabled: true,
            half_life_days: 1,
            dormant_threshold: 0.9999,
            archive_days: 0,
        })
        .expect("decay to dormant");
    assert!(
        decayed.to_dormant >= 2,
        "both episodes decayed: {decayed:?}"
    );

    let archived = lc
        .provider()
        .run_episodic_decay_scan(&EpisodicDecayConfig {
            enabled: true,
            half_life_days: 1,
            dormant_threshold: 0.0,
            archive_days: 0,
        })
        .expect("decay to archive");
    assert!(archived.purged >= 2, "episodes archived: {archived:?}");

    assert_eq!(
        lc.status(hangzhou).await,
        None,
        "the episode is gone from the live set"
    );
    assert_eq!(
        lc.sediment_count(),
        2,
        "forgetting is episodic-only - the semantic layer must be untouched"
    );
    for id in &sediment_ids {
        assert_eq!(
            lc.status(*id).await,
            Some(NodeStatus::Active),
            "an aged sediment node must not have been decayed, id {id}"
        );
    }
}
/// Knowledge outlives its evidence *and* stays recallable afterwards. The
/// retrieval path is the user-visible half of the invariant: a sediment node
/// that survives a purge but cannot be found is the same bug with a different
/// symptom.
#[tokio::test]
async fn sediment_stays_recallable_after_its_episodes_are_forgotten() {
    let lc = Lifecycle::new();
    let id = lc.seed_legacy_episode(
        "User lives in Hangzhou",
        KnowledgeSubType::Fact,
        Utc::now(),
        false,
    );
    DefaultEpisodicDistiller
        .run(
            lc.provider().as_ref(),
            None,
            None,
            &DistillerConfig::default(),
        )
        .await
        .expect("consolidate");

    // Age the sediment too. If only the episode is old, a scan that wrongly
    // swept the semantic layer would still leave the node Active by luck of
    // its age - the assertion would pass without proving the exclusion.
    for sid in lc.ids_by_label(labels::KNOWLEDGE) {
        lc.age_node(sid, 4000);
    }
    lc.age_node(id, 4000);
    for cfg in [
        EpisodicDecayConfig {
            enabled: true,
            half_life_days: 1,
            dormant_threshold: 0.9999,
            archive_days: 0,
        },
        EpisodicDecayConfig {
            enabled: true,
            half_life_days: 1,
            dormant_threshold: 0.0,
            archive_days: 0,
        },
    ] {
        lc.provider()
            .run_episodic_decay_scan(&cfg)
            .expect("decay scan");
    }
    assert_eq!(lc.status(id).await, None, "the episode is gone");

    // The recall assertion alone is not enough: a Dormant node can still be
    // surfaced, so the semantic layer being *decayed at all* has to be caught
    // where it happens.
    let sediment = lc.ids_by_label(labels::KNOWLEDGE);
    assert!(!sediment.is_empty(), "sediment should exist to check");
    for sid in sediment {
        assert_eq!(
            lc.status(sid).await,
            Some(NodeStatus::Active),
            "forgetting must never touch the semantic layer, id {sid}"
        );
    }

    let manager = MemoryManager::new(MemoryManagerConfig::default());
    let mut query = MemoryQuery::new("which city does the user live in".to_string());
    let retrieved = manager
        .retrieve(&*lc.store, &mut query, Some(&DeterministicEmbedding))
        .await
        .expect("retrieve ok");
    assert!(
        retrieved
            .memories
            .iter()
            .any(|m| m.label == labels::KNOWLEDGE && m.content.contains("Hangzhou")),
        "knowledge outlives its evidence, got: {:?}",
        retrieved
            .memories
            .iter()
            .map(|m| (m.label.clone(), m.content.clone()))
            .collect::<Vec<_>>()
    );
}

/// A recent episode survives even an aggressive scan. This is the other half
/// of the forgetting contract: `dormant_threshold` is what stops memory from
/// evaporating before it has been useful. Every other forgetting test here
/// ages its input deliberately, so without this one a scan that ignored
/// retention entirely - dormancy on sight - would still pass.
#[tokio::test]
async fn a_recent_episode_survives_even_an_aggressive_decay_scan() {
    let lc = Lifecycle::new();
    let fresh = lc.seed_legacy_episode(
        "User is reviewing the consolidation plan tonight",
        KnowledgeSubType::Fact,
        Utc::now(),
        false,
    );
    // One day old against a 30-day half-life: retention is near 1.0.
    lc.age_node(fresh, 1);

    let result = lc
        .provider()
        .run_episodic_decay_scan(&EpisodicDecayConfig {
            enabled: true,
            half_life_days: 30,
            dormant_threshold: 0.1,
            archive_days: 90,
        })
        .expect("scan");
    assert_eq!(result.to_dormant, 0, "a one-day-old episode is not stale");
    assert_eq!(result.purged, 0);
    assert_eq!(
        lc.status(fresh).await,
        Some(NodeStatus::Active),
        "recent memory must not decay early"
    );
    assert!(lc.purged_ids().is_empty());
}

/// Dormancy is a grace period, not an instant delete. `archive_days` is the
/// user's undo window: an episode that just went Dormant must still be in the
/// live set, and must *not* have been copied into `purge_log`. Without this
/// assertion a scan that purges on sight of Dormant status passes every other
/// forgetting test, because they all run with `archive_days: 0`.
#[tokio::test]
async fn a_dormant_episode_waits_out_its_grace_period_before_being_forgotten() {
    let lc = Lifecycle::new();
    let id = lc.seed_legacy_episode(
        "User shipped the memory rewrite on a Tuesday",
        KnowledgeSubType::Fact,
        Utc::now(),
        false,
    );
    lc.age_node(id, 4000);

    // Step 1: past the dormancy threshold, but the archive window is 90 days
    // and the node has only just gone Dormant.
    lc.provider()
        .run_episodic_decay_scan(&EpisodicDecayConfig {
            enabled: true,
            half_life_days: 1,
            dormant_threshold: 0.9999,
            archive_days: 90,
        })
        .expect("decay to dormant");
    assert_eq!(
        lc.status(id).await,
        Some(NodeStatus::Dormant),
        "the aged episode should have entered the grace period"
    );
    assert!(
        lc.purged_ids().is_empty(),
        "nothing may be archived while the grace period is unexpired, got {:?}",
        lc.purged_ids()
    );

    // Step 2: still waiting at day 89.
    lc.set_dormant_since(id, 89);
    let mid = lc
        .provider()
        .run_episodic_decay_scan(&EpisodicDecayConfig {
            enabled: true,
            half_life_days: 1,
            dormant_threshold: 0.9999,
            archive_days: 90,
        })
        .expect("scan at day 89");
    assert_eq!(mid.purged, 0, "day 89 of 90 is not yet forgotten");
    assert_eq!(lc.status(id).await, Some(NodeStatus::Dormant));

    // Step 3: the window closes, and the copy precedes the delete.
    lc.set_dormant_since(id, 91);
    let done = lc
        .provider()
        .run_episodic_decay_scan(&EpisodicDecayConfig {
            enabled: true,
            half_life_days: 1,
            dormant_threshold: 0.9999,
            archive_days: 90,
        })
        .expect("scan past the deadline");
    assert_eq!(
        done.purged, 1,
        "the episode is forgotten once the window closes"
    );
    assert_eq!(lc.status(id).await, None, "gone from the live set");
    assert_eq!(
        lc.purged_ids(),
        vec![id],
        "forgotten, not lost - the row is recoverable from purge_log"
    );
}

/// Forgetting is opt-in. With the switch off an aged backlog stays put - the
/// same "the default does nothing surprising" contract the distiller has.
#[tokio::test]
async fn disabled_forgetting_leaves_an_aged_backlog_alone() {
    let lc = Lifecycle::new();
    let id = lc.seed_legacy_episode(
        "User has a quarterly review on Friday",
        KnowledgeSubType::Fact,
        Utc::now(),
        false,
    );
    lc.age_node(id, 4000);

    let result = lc
        .provider()
        .run_episodic_decay_scan(&EpisodicDecayConfig::default())
        .expect("a disabled scan is a no-op, not an error");
    assert_eq!(result.to_dormant, 0);
    assert_eq!(result.purged, 0);
    assert_eq!(
        lc.status(id).await,
        Some(NodeStatus::Active),
        "the episode is untouched"
    );
    assert_eq!(
        lc.provider()
            .count_unconsolidated_episodes()
            .expect("count"),
        1,
        "and still eligible for consolidation"
    );
}

// ============================================================================
// Scripted LLM
// ============================================================================

/// A model that merges into the first candidate its request names.
///
/// The store assigns node ids, the test does not, so a scripted `target_id`
/// would either be wrong or require a second run. Parsing the request keeps
/// this a single-run test and still exercises the real id round-trip: the id
/// the model echoes is the id `apply_merge` writes to.
struct FoldIntoFirstCandidate;

#[async_trait::async_trait]
impl acowork_memory::consolidation::ConsolidationLlm for FoldIntoFirstCandidate {
    async fn chat(
        &self,
        messages: Vec<acowork_memory::consolidation::LlmMessage>,
    ) -> std::result::Result<acowork_memory::consolidation::LlmResponse, String> {
        let user = messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .ok_or("no user message")?;
        let request: serde_json::Value =
            serde_json::from_str(user).map_err(|e| format!("unparsable request: {e}"))?;
        let target = request["candidates"][0]["id"]
            .as_u64()
            .ok_or("no candidates in the request")?;
        Ok(acowork_memory::consolidation::LlmResponse {
            content: serde_json::json!({
                "action": "merge",
                "target_id": target,
                "statement": request["new_statement"],
                "reasoning": "same preference restated",
            })
            .to_string(),
            usage_tokens: None,
            ..Default::default()
        })
    }
}

/// A model that always says "distinct" - the worst case for volume and the
/// best case for liveness. Every deferred episode must still reach a verdict,
/// so if a backlog can stall this mock is what unstalls it.
struct KeepDistinct;

#[async_trait::async_trait]
impl acowork_memory::consolidation::ConsolidationLlm for KeepDistinct {
    async fn chat(
        &self,
        _messages: Vec<acowork_memory::consolidation::LlmMessage>,
    ) -> std::result::Result<acowork_memory::consolidation::LlmResponse, String> {
        Ok(acowork_memory::consolidation::LlmResponse {
            content: serde_json::json!({
                "action": "no_merge",
                "target_id": null,
                "statement": null,
                "reasoning": "treat as distinct",
            })
            .to_string(),
            usage_tokens: None,
            ..Default::default()
        })
    }
}

// ============================================================================
// M3 - backfill against a real production store (manual, `#[ignore]`)
// ============================================================================

/// The question that started this rewrite: a live agent had 269 episodic rows,
/// the consolidation switch on for weeks, and zero sediment. Synthetic fixtures
/// prove the pipeline; only the real store proves *that* store drains.
///
/// Point it at a copy of a `private.sqlite` and it runs the production
/// distiller over the copy until the backlog is empty:
///
/// ```text
/// ACOWORK_BACKFILL_DB=/path/to/private.sqlite \
///   cargo test -p acowork-memory --test memory_lifecycle_e2e -- \
///   --ignored --nocapture backfill_on_a_real_store
/// ```
///
/// Deliberately `#[ignore]`: it needs a real user database, which is private
/// data and cannot live in CI. It never opens the path read-write in place -
/// the file is copied to a tempdir first, so a mistaken run cannot damage a
/// live agent's memory.
#[tokio::test]
#[ignore = "requires a real private.sqlite; pass ACOWORK_BACKFILL_DB to run it"]
async fn backfill_on_a_real_store_drains_the_backlog_without_a_model() {
    let src = std::env::var("ACOWORK_BACKFILL_DB")
        .expect("set ACOWORK_BACKFILL_DB to a path to a private.sqlite");
    let src = std::path::Path::new(&src);
    assert!(src.is_file(), "not a file: {src:?}");

    let dir = tempfile::tempdir().expect("tempdir");
    let dst = dir.path().join("private.sqlite");
    std::fs::copy(src, &dst).expect("copy the store out of the live location");

    let store = Arc::new(SqliteStore::open_dim_agnostic(&dst).expect("open the copied store"));
    let count = |label: &str| store.node_count_by_label(label).unwrap_or(0);

    let pending_before = store
        .count_unconsolidated_episodes()
        .expect("count pending");
    println!("store: {src:?}");
    println!("  episodic rows   : {}", count(labels::EPISODIC));
    println!("  pending backlog : {pending_before}");
    println!(
        "  sediment before : knowledge={} procedural={}",
        count(labels::KNOWLEDGE),
        count(labels::PROCEDURAL)
    );

    // No LLM, no embedding: a backfill of legacy rows must cost zero tokens.
    // An episode that does find a vector neighbour becomes a merge candidate
    // and is deferred rather than guessed at, which is the correct no-model
    // behaviour - so the loop is bounded, not "until the backlog is empty".
    let mut runs = 0_u32;
    let mut total_llm = 0_u64;
    let mut total_projected = 0_u64;
    let mut total_deferred = 0_u64;
    let mut last_pending = pending_before;

    for _ in 0..16 {
        let result = DefaultEpisodicDistiller
            .run(
                store.as_ref() as &dyn MemoryProvider,
                None,
                None,
                &DistillerConfig::default(),
            )
            .await
            .expect("distiller run");
        runs += 1;
        total_llm += result.funnel.llm_calls as u64;
        total_projected += result.funnel.projected as u64;
        total_deferred += result.funnel.episodes_deferred as u64;
        let pending = store
            .count_unconsolidated_episodes()
            .expect("count pending");
        println!(
            "  run {runs}: scanned={} projected={} deferred={} llm_calls={} pending_now={pending}",
            result.episodes_scanned,
            result.funnel.projected,
            result.funnel.episodes_deferred,
            result.funnel.llm_calls,
        );
        if !result.funnel.errors.is_empty() {
            println!("  errors: {:?}", result.funnel.errors);
        }
        if pending == 0 || pending == last_pending {
            break; // drained, or every remaining row is a deferred candidate
        }
        last_pending = pending;
    }

    println!(
        "  sediment after  : knowledge={} procedural={}",
        count(labels::KNOWLEDGE),
        count(labels::PROCEDURAL)
    );
    println!(
        "  totals: runs={runs} projected={total_projected} deferred={total_deferred} llm_calls={total_llm}"
    );

    assert_eq!(total_llm, 0, "a legacy backfill must not spend tokens");
    assert!(
        total_projected > 0,
        "nothing was projected; the backlog is still {last_pending}"
    );
    assert!(
        count(labels::KNOWLEDGE) + count(labels::PROCEDURAL) > 0,
        "the whole complaint was 'no sediment ever appeared' - {pending_before}          legacy episodes produced nothing."
    );

    // ---- phase 2: with a model the backlog must actually drain ------------
    //
    // Phase 1 leaves most episodes deferred, and that is the correct no-model
    // behaviour. But it is only correct if "deferred" means "waiting for a
    // model" and not "stuck". Production always passes one
    // (`run_episodic_distiller_step` takes `&dyn ConsolidationLlm`), so the
    // liveness property worth checking on real data is: given any model at
    // all, every episode reaches a verdict.
    //
    // `KeepDistinct` is the adversarial choice - it never merges, so it
    // maximises both node count and the number of fresh candidates the next
    // episode sees. A merge-happy model would drain faster and prove less.
    let mut model_runs = 0_u32;
    let mut pending = store
        .count_unconsolidated_episodes()
        .expect("count pending");
    while pending > 0 && model_runs < 24 {
        let result = DefaultEpisodicDistiller
            .run(
                store.as_ref() as &dyn MemoryProvider,
                Some(&KeepDistinct),
                None,
                &DistillerConfig::default(),
            )
            .await
            .expect("distiller run with a model");
        model_runs += 1;
        let next = store
            .count_unconsolidated_episodes()
            .expect("count pending");
        println!(
            "  model run {model_runs}: scanned={} projected={} deferred={} llm_calls={} pending_now={next}",
            result.episodes_scanned,
            result.funnel.projected,
            result.funnel.episodes_deferred,
            result.funnel.llm_calls,
        );
        assert!(
            next < pending,
            "run {model_runs} made no progress on a {pending}-episode backlog:              deferred episodes are stuck, not waiting"
        );
        pending = next;
    }

    println!(
        "  sediment with a model: knowledge={} procedural={} (model_runs={model_runs})",
        count(labels::KNOWLEDGE),
        count(labels::PROCEDURAL)
    );
    assert_eq!(
        pending, 0,
        "a real {pending_before}-episode backlog must drain once a model exists"
    );
}
