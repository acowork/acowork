//! Storage isolation between two agent stores (ADR-009 S2.11.2 / S2.11.3).
//!
//! Supersedes the grafeo-based `test_storage_isolation_two_grafeo_stores` and
//! `test_cross_agent_isolation` in the Gateway's `intent::privacy` — those
//! constructed stores directly, so the invariant they assert is a property of
//! the storage backend, not of Gateway code, and belongs here.
//!
//! The property is stronger on file-backed stores than it was on in-memory
//! stores: two agents are isolated because they open two different files, and
//! a secret written by one must never become retrievable by the other.
//!
//! Run: `cargo test -p acowork-memory --test store_isolation`

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;

use acowork_memory::admin::AdminListNodesParams;
use acowork_memory::types::MemoryQuery;
use acowork_memory::{Episode, MemoryAdminService, MemoryProvider};
use acowork_memory::sqlite::SqliteStore;

const DIM: usize = 4;
const SECRET: &str = "API key: sk-live-abc123";

struct Agent {
    memory: Arc<dyn MemoryProvider>,
    admin: Arc<dyn MemoryAdminService>,
}

fn agent(path: &std::path::Path) -> Agent {
    let store = Arc::new(SqliteStore::open(path, DIM).expect("open store"));
    let memory: Arc<dyn MemoryProvider> = store.clone();
    let admin: Arc<dyn MemoryAdminService> = store;
    Agent { memory, admin }
}

fn episode(content: &str) -> Episode {
    Episode {
        session_id: "s1".to_string(),
        turn_index: 0,
        role: "user".to_string(),
        content: content.to_string(),
        embedding: None,
        timestamp: Utc::now(),
        consolidated: false,
        metadata: HashMap::new(),
        importance: 0.5,
        knowledge_subtype: None,
        normalized: None,
    }
}

fn recall(agent: &Agent, query: &str) -> Vec<u64> {
    let mut q = MemoryQuery::new(query);
    q.limit = 10;
    agent
        .memory
        .search_episodes(&q)
        .expect("search must succeed")
        .into_iter()
        .map(|h| h.node_id)
        .collect()
}

fn total_episodes(agent: &Agent) -> u64 {
    agent
        .admin
        .list_nodes(&AdminListNodesParams {
            page: 1,
            size: 10,
            node_type: "Episodic".to_string(),
            ..Default::default()
        })
        .total
}

/// Agent A writes a secret; agent B — a different file — must not see it,
/// not by id and not by content, and each store counts only its own rows.
#[test]
fn two_agent_stores_do_not_share_data() {
    let dir = tempfile::tempdir().unwrap();
    let agent_a = agent(&dir.path().join("a").join("private.sqlite"));
    let agent_b = agent(&dir.path().join("b").join("private.sqlite"));

    let id_a = agent_a
        .memory
        .store_episode(&episode(SECRET))
        .expect("store into A");
    assert_eq!(total_episodes(&agent_a), 1, "A holds exactly its own row");
    assert_eq!(total_episodes(&agent_b), 0, "B starts empty");

    // B cannot retrieve A's node — by id or via content search.
    assert!(
        recall(&agent_b, "API key").is_empty(),
        "B must not recall A's secret by content"
    );
    assert!(
        !recall(&agent_b, SECRET).contains(&id_a),
        "B must not recall A's node id"
    );

    // B writes its own data; A stays untouched.
    let id_b = agent_b
        .memory
        .store_episode(&episode("B likes rust"))
        .expect("store into B");
    assert!(!recall(&agent_a, "rust").contains(&id_b));
    assert_eq!(
        recall(&agent_a, "API key"),
        vec![id_a],
        "A still sees only its own row"
    );
    assert_eq!(total_episodes(&agent_a), 1);
    assert_eq!(total_episodes(&agent_b), 1);
}

/// Reopening the same file is not isolation: state written before the restart
/// is still the same agent's state (guards against a "fresh store per open"
/// regression that would make the isolation test above pass vacuously).
#[test]
fn reopening_a_store_preserves_its_own_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a").join("private.sqlite");

    let id = agent(&path)
        .memory
        .store_episode(&episode(SECRET))
        .expect("store");

    let reopened = agent(&path);
    assert_eq!(total_episodes(&reopened), 1);
    assert!(recall(&reopened, "API key").contains(&id));
}
