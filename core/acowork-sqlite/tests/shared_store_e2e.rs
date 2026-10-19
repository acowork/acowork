//! ADR-082 §4 step 3: memory nodes, session meta and the conversation index
//! must all live in one `.sqlite` file, sharing one connection.
//!
//! This test drives the three stores off a single `Arc<SqliteStore>` and
//! reads each back — proving the shared-connection design works end to end
//! (no second connection, no schema clash, no lock contention).

use std::sync::Arc;

use acowork_core::error::Result as AcoworkResult;
use acowork_memory::SessionMetaStore;
use acowork_memory::session_meta::SessionMeta;
use acowork_sqlite::{ConversationStore, SqliteSessionMetaStore, SqliteStore};

const DIM: usize = 4;

fn meta(sid: &str, title: &str) -> SessionMeta {
    SessionMeta {
        version: 4,
        session_id: sid.to_string(),
        agent_id: "agent".into(),
        created_at: "2026-01-01T00:00:00.000Z".into(),
        user_id: None,
        visibility: None,
        title: Some(title.into()),
        workspace_id: None,
        model: None,
        provider: None,
        account_id: None,
        reasoning_effort: None,
        temperature: None,
        context_window: None,
        todos: None,
        message_count: 1,
        last_active_at: "2026-02-02T00:00:00.000Z".into(),
        tokens: None,
        llm_call_counter: None,
        perf: None,
        model_ratio: None,
        last_compaction_offset: None,
        corrupted: false,
    chat_session_id: None,
    }
}

#[test]
fn memory_session_meta_and_conversation_index_share_one_file() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("memory").join("private.sqlite");

    // One store, one file, one connection — exactly what
    // `AgentCore::init_sqlite_backend` builds.
    let store = Arc::new(SqliteStore::open(&db_path, DIM)?);

    let conversation = ConversationStore::from_store(store.clone())?;
    let sessions = SqliteSessionMetaStore::new(store.clone());

    // ── conversation index write ───────────────────────────────────────
    let emb: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0];
    conversation.index_message("s1", 0, "user", "how do I migrate to sqlite?", &emb)?;
    conversation.index_message("s1", 1, "assistant", "open the sqlite store", &emb)?;

    // ── session meta write ─────────────────────────────────────────────
    sessions.upsert(&meta("s1", "migration talk"))?;

    // ── memory node write (through the ordinary write path) ─────────────
    let episode = acowork_memory::Episode {
        session_id: "s1".into(),
        turn_index: 0,
        role: "user".into(),
        content: "remember this fact".into(),
        embedding: None,
        timestamp: chrono::DateTime::parse_from_rfc3339("2026-02-02T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        consolidated: false,
        metadata: std::collections::HashMap::new(),
        importance: 1.0,
        knowledge_subtype: None,
        normalized: None,
    };
    // ── memory node write ──────────────────────────────────────────────
    let node_id = store.store_episode(&episode)?;

    // ── reads from each surface ────────────────────────────────────────
    let hits = conversation.search("migrate", None, 10)?;
    assert_eq!(hits.len(), 1, "conversation index reachable");
    assert_eq!(hits[0].session_id, "s1");

    let m = sessions.get("s1")?.expect("session meta reachable");
    assert_eq!(m.title.as_deref(), Some("migration talk"));

    let node = store.get_episode(node_id)?.expect("memory node reachable");
    assert_eq!(node.content, "remember this fact");

    // Everything is physically in the one file we opened.
    assert!(db_path.exists());

    // Reopen the same path with a second store: the rows are all there.
    drop(conversation);
    drop(sessions);
    drop(store);
    let reopened = SqliteStore::open(&db_path, DIM)?;
    assert!(reopened.get_episode(node_id)?.is_some());
    let reopened_meta = SqliteSessionMetaStore::new(Arc::new(reopened));
    assert_eq!(
        reopened_meta.get("s1")?.unwrap().title.as_deref(),
        Some("migration talk")
    );
    Ok(())
}
