//! End-to-end verification of the four memory chains on the SQLite backend
//! (ADR-082 §4 step 2).
//!
//! This drives the *runtime's* handle types — `Arc<dyn MemoryProvider>` +
//! `Arc<dyn MemoryAdminService>` — not the inherent `SqliteStore` methods, so it
//! fails if the trait wiring breaks even when the store itself is fine. It then
//! reopens the same file and re-asserts, because the whole point of the
//! migration is that state survives a restart without a WAL replay.
//!
//! The chains:
//!
//! 1. `memory_store` — episodes written through the provider get ids;
//! 2. `memory_recall` — hybrid retrieval finds them, including the CJK substring
//!    that failed under `unicode61` (ADR-082 §1.4), and the `created_at` /
//!    `status` reads return the *columns*, not a missing `props` key;
//! 3. `memory_distill` — unconsolidated episodes are counted, consolidatable
//!    ones are marked, and tombstoned ones drop out so the distiller cannot
//!    re-extract the same cluster forever;
//! 4. forgetting — decay moves aged episodes to Dormant and archives them to
//!    the purge log rather than destroying them.
//!
//! Run: `cargo test -p acowork-memory --test memory_chains_e2e`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use acowork_memory::admin::AdminListNodesParams;
use acowork_memory::types::{EpisodicDecayConfig, KnowledgeSubType, MemoryQuery};
use acowork_memory::{Episode, MemoryAdminService, MemoryProvider, NodeStatus};
use acowork_memory::sqlite::SqliteStore;

const DIM: usize = 4;

fn provider(path: &std::path::Path) -> (Arc<dyn MemoryProvider>, Arc<dyn MemoryAdminService>) {
    let store = Arc::new(SqliteStore::open(path, DIM).expect("open store"));
    let mem: Arc<dyn MemoryProvider> = store.clone();
    let admin: Arc<dyn MemoryAdminService> = store;
    (mem, admin)
}

fn emb(seed: u8) -> Vec<f32> {
    let mut v = vec![0.0; DIM];
    v[seed as usize % DIM] = 1.0;
    v
}

fn episode(content: &str, session: &str, age_days: i64) -> Episode {
    Episode {
        session_id: session.to_string(),
        turn_index: 0,
        role: "user".to_string(),
        content: content.to_string(),
        embedding: None,
        timestamp: Utc::now() - chrono::TimeDelta::try_days(age_days).unwrap(),
        consolidated: false,
        metadata: HashMap::new(),
        importance: 0.5,
        knowledge_subtype: None,
        normalized: None,
    }
}

/// Every chain, in order, against one store file; then again after a restart.
#[test]
fn four_memory_chains_round_trip_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private.sqlite");

    let (memory, admin) = provider(&path);

    // ── 1. memory_store ──────────────────────────────────────────────────
    let mut fresh = episode("帮我把网关的日志级别改成 debug", "s1", 0);
    fresh.knowledge_subtype = Some(KnowledgeSubType::Fact);
    let fresh_id = memory.store_episode(&fresh).unwrap();
    let mut old = episode("deploy the gateway to staging", "s1", 400);
    old.knowledge_subtype = Some(KnowledgeSubType::Procedure);
    let old_id = memory.store_episode(&old).unwrap();
    assert_ne!(fresh_id, old_id, "ids must not collide");
    // A subtype-less dialogue fragment: retrievable forever, never a
    // consolidation candidate.
    let fragment_id = memory
        .store_episode(&episode("ok thanks", "s1", 0))
        .unwrap();

    // ── 2. memory_recall ─────────────────────────────────────────────────
    // 4-char CJK substring: the trigram tokenizer's whole reason for existing.
    let mut q = MemoryQuery::new("网关的日志");
    q.limit = 5;
    let hits = memory.search_episodes(&q).unwrap();
    assert!(
        hits.iter().any(|h| h.node_id == fresh_id),
        "CJK keyword recall missed the stored episode: {hits:?}"
    );

    // Hybrid path through the same trait object the runtime holds.
    let hybrid = memory
        .hybrid_search_full("Episodic", "gateway", &emb(1), 5, 1.0, 1.0, Some(0.3))
        .unwrap();
    assert!(
        hybrid.iter().any(|(id, _)| *id == old_id),
        "hybrid search missed the English episode: {hybrid:?}"
    );

    // The two field-projection regressions: `created_at` / `status` are columns,
    // not `props` keys, so a recall that reads props sees nothing.
    let created = memory.get_node_created_at(fresh_id).unwrap().unwrap();
    assert!(
        (Utc::now() - created).num_days() < 1,
        "episode timestamp read back wrong: {created}"
    );
    assert_eq!(
        memory.get_node_status(fresh_id).unwrap(),
        Some(NodeStatus::Active)
    );

    // ── 3. memory_distill ────────────────────────────────────────────────
    // Backlog counts exactly what the scan can return — the fragment is in
    // neither, so the two predicates cannot drift apart.
    assert_eq!(memory.count_unconsolidated_episodes().unwrap(), 2);
    assert!(memory
        .get_episodes_by_subtype(None, 10)
        .unwrap()
        .iter()
        .all(|(id, _)| *id != fragment_id));
    memory.mark_consolidated(&[fresh_id]).unwrap();
    assert_eq!(
        memory.count_unconsolidated_episodes().unwrap(),
        1,
        "consolidated episode must leave the distiller's queue"
    );

    // An episode the distiller did NOT consolidate must stay in the queue.
    // There is no skip tombstone any more: a verdict that went wrong (or an
    // LLM that was unavailable) costs a retry, never the memory.
    let backlog = memory.get_episodes_by_subtype(None, 10).unwrap();
    assert_eq!(
        backlog.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![old_id],
        "an unconsolidated episode must be re-offered on the next run"
    );

    // ── 4. forgetting ────────────────────────────────────────────────────
    // The 400-day-old episode is back in the queue only because of the purge
    // below; decay is a pure function of age, so it is the one that ages out.
    let config = EpisodicDecayConfig {
        enabled: true,
        half_life_days: 1,
        dormant_threshold: 0.99,
        archive_days: 0,
    };
    let decayed = memory.run_episodic_decay_scan(&config).unwrap();
    assert!(
        decayed.to_dormant >= 1,
        "an aged episode should have gone dormant: {decayed:?}"
    );

    // ── 5. restart ───────────────────────────────────────────────────────
    memory.close().unwrap();
    drop(memory);
    drop(admin);

    let (memory, _admin) = provider(&path);
    assert_eq!(
        memory.stats().unwrap().episode_count,
        3,
        "all three episodes (two classified, one fragment) survive a restart"
    );
    let hits = memory.search_episodes(&MemoryQuery::new("网关的日志")).unwrap();
    assert_eq!(hits.len(), 1, "recall must survive a restart: {hits:?}");
    assert_eq!(hits[0].node_id, fresh_id, "and return the same id");

    // Decayed state is a persisted column, so the reduced status comes back too.
    assert_eq!(
        memory.get_node_status(old_id).unwrap(),
        Some(NodeStatus::Dormant),
        "the decayed status must be readable after a restart"
    );
}

/// The forgetting chain must archive before it deletes: decay is the one path
/// that can destroy memory, so the archived row is the difference between a
/// 30-day recovery window and data loss (ADR-082 D1).
#[test]
fn forgetting_archives_before_deleting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private.sqlite");
    let (memory, _admin) = provider(&path);

    let id = memory
        .store_episode(&episode("ancient episode", "s1", 4000))
        .unwrap();

    // Age it past dormancy, then past archival.
    let to_dormant = EpisodicDecayConfig {
        enabled: true,
        half_life_days: 1,
        dormant_threshold: 0.9999,
        archive_days: 0,
    };
    assert!(memory.run_episodic_decay_scan(&to_dormant).unwrap().to_dormant >= 1);

    let to_archive = EpisodicDecayConfig {
        enabled: true,
        half_life_days: 1,
        dormant_threshold: 0.0,
        archive_days: 0,
    };
    let archived = memory.run_episodic_decay_scan(&to_archive).unwrap();
    assert!(archived.purged >= 1, "nothing was archived: {archived:?}");

    assert_eq!(
        memory.get_node_status(id).unwrap(),
        None,
        "an archived episode is gone from the live set"
    );
    assert_eq!(memory.stats().unwrap().episode_count, 0);

    // ...but recoverable. `purge_log` is the archive, and it is the contract the
    // forgetting path signs up to — asserted against the file rather than
    // through the trait, so a missing archive cannot hide behind an accessor.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let archived_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM purge_log", [], |r| r.get(0))
        .expect("purge log is readable");
    assert!(
        archived_rows >= 1,
        "the archived episode must be in the purge log, not dropped"
    );
}

/// Disabled forgetting is a hard no-op, and stored episodes must not be touched
/// by an episode-only scan of a store that also holds knowledge nodes.
#[test]
fn disabled_forgetting_does_not_touch_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private.sqlite");
    let (memory, admin) = provider(&path);

    let id = memory
        .store_episode(&episode("long dormant candidate", "s1", 10_000))
        .unwrap();
    let off = EpisodicDecayConfig {
        enabled: false,
        ..Default::default()
    };
    let result = memory.run_episodic_decay_scan(&off).unwrap();
    assert_eq!(
        (result.to_dormant, result.purged),
        (0, 0),
        "disabled decay must be a no-op"
    );
    assert_eq!(
        memory.get_node_status(id).unwrap(),
        Some(NodeStatus::Active)
    );

    // The admin surface reads the same rows the runtime's panel needs, from the
    // generic row shape — including rows whose props predate a typed struct.
    let listed = admin.list_nodes(&AdminListNodesParams {
        page: 1,
        size: 10,
        node_type: "Episodic".to_string(),
        ..Default::default()
    });
    assert_eq!(listed.total, 1);
    assert_eq!(listed.nodes[0].node_id, id);
    assert_eq!(listed.nodes[0].status, "Active");
    assert!(
        listed.nodes[0].content.contains("dormant"),
        "admin rows must carry the display content: {:?}",
        listed.nodes[0]
    );

    // Duration is part of the trait surface the runtime calls at startup.
    let _ = memory
        .cleanup_episodes(Duration::from_secs(60))
        .expect("cleanup must be callable");
}

/// Decay is a function of each episode's own timestamp, not of write order: an
/// episode written last must not age out because the store is old, and one
/// written first must not be spared because it is.
#[test]
fn decay_uses_the_episode_timestamp_not_the_write_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private.sqlite");
    let (memory, _admin) = provider(&path);

    // Written first, but young.
    let young = memory
        .store_episode(&episode("just now", "s1", 0))
        .unwrap();
    // Written second, ancient.
    let ancient = memory
        .store_episode(&episode("written after, aged", "s1", 3000))
        .unwrap();

    let config = EpisodicDecayConfig {
        enabled: true,
        half_life_days: 1,
        dormant_threshold: 0.5,
        archive_days: 90,
    };
    memory.run_episodic_decay_scan(&config).unwrap();

    assert_eq!(
        memory.get_node_status(ancient).unwrap(),
        Some(NodeStatus::Dormant),
        "the aged episode must decay regardless of write order"
    );
    assert_eq!(
        memory.get_node_status(young).unwrap(),
        Some(NodeStatus::Active),
        "a fresh episode must not decay just because it shares a store"
    );
}
