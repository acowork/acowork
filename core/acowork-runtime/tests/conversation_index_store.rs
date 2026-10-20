//! Conversation index store + watermark recovery (ADR-081 §4.2,
//! ADR-082 §4 step 3).
//!
//! Guards two properties that a boot-time check cannot see:
//!
//! 1. the index lives in the memory backend's `.sqlite` file — memory nodes,
//!    session meta and message vectors share one file and one write lock;
//! 2. re-opening the store recovers the per-session watermark (and the
//!    incremental byte-offset tail pointer) from the `conversation_state` table,
//!    so the tailer resumes from the recorded offset instead of re-embedding the
//!    whole JSONL history. §11: the old boot-time rebuild from the persisted
//!    message rows — a full-table `MAX(message_index)` scan plus a `NOT IN`
//!    junk/dedup pass — is gone; the indexer commits the tail after every line.

use std::sync::Arc;

use acowork_runtime::conversation_index::ConversationIndex;
use acowork_sqlite::SqliteStore;

const DIM: usize = 8;

fn embedding(seed: f32) -> Vec<f32> {
    (0..DIM).map(|k| seed + k as f32 / 100.0).collect()
}

/// Open the workspace the way the runtime does: the shared store first, the
/// index on top of it.
fn open(work_dir: &std::path::Path) -> ConversationIndex {
    let db = work_dir.join("memory").join("private.sqlite");
    let store = Arc::new(SqliteStore::open(&db, DIM).unwrap());
    ConversationIndex::from_store(store, work_dir).unwrap()
}

#[test]
fn index_shares_the_memory_file_and_reopen_recovers_watermarks() {
    let dir = tempfile::tempdir().unwrap();

    {
        let index = open(dir.path());
        index
            .index_message("s1", 0, "user", "how do I search memory", &embedding(0.1))
            .unwrap();
        index
            .index_message("s1", 1, "assistant", "use the sqlite index", &embedding(0.2))
            .unwrap();
        index.record_state("s1", 2, 512);
        index
            .index_message("s2", 7, "user", "unrelated session", &embedding(0.3))
            .unwrap();
        index.record_state("s2", 8, 256);
        assert_eq!(index.next_line("s1"), 2);
        // No close(): a Runtime is normally killed by the Node Agent rather than
        // shut down, so every write must already be durable when it lands.
    }

    assert!(
        dir.path().join("memory").join("private.sqlite").is_file(),
        "the index must live in the shared memory file, not in one of its own"
    );

    let index = open(dir.path());
    // Without recovery this is 0 for every session and the tailer re-embeds
    // and re-inserts all history on every restart.
    assert_eq!(index.next_line("s1"), 2, "watermark must survive a reopen");
    assert_eq!(index.next_line("s2"), 8, "watermark must survive a reopen");
    assert_eq!(
        index.state_for("s1"),
        (2, 512),
        "the byte-offset tail pointer must survive a reopen"
    );
    assert_eq!(index.message_count(), 3, "rows must survive a reopen");

    // The restored store is still searchable (rows + text index came back).
    let hits = index.search("sqlite index", None, 5);
    assert!(
        hits.iter().any(|h| h.session_id == "s1"),
        "text search over the reopened store found nothing: {hits:?}"
    );
}
