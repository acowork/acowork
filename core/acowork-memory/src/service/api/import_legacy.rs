//! `POST /admin/import` — the one-shot legacy store upload (ADR-101 P5).
//!
//! The write entrypoint that is not a tool call, a panel edit, a distiller run
//! or the manifest bootstrap: it moves an instance's existing memories out of
//! its node-local workspace file and into the service namespace, once. The
//! caller is always the Node Agent (ADR-055 — it owns the node-local
//! filesystem; the Gateway must never read agent-private data, ADR-009 §5),
//! which posts the raw bytes of `{workspace}/memory/private.sqlite` as
//! `application/x-sqlite3`.
//!
//! ## Why raw bytes and not multipart
//!
//! The payload is one opaque file with no sibling fields to carry alongside it,
//! so a multipart envelope would only add parsing surface (and a `reqwest`
//! feature the Node does not enable) for zero expressiveness. The instance
//! namespace arrives the same way every non-MCP route gets it —
//! `?instance_id=` injected and authorized upstream — and the size ceiling is
//! the shared [`acowork_core::workspace::STORE_IMPORT_MAX_BYTES`], which is the
//! Gateway's own global body limit: a bigger store cannot reach this handler at
//! all, so the Node refuses it up front and tells the operator what to do
//! instead of streaming 64 MiB into a 413.
//!
//! ## Idempotency
//!
//! A namespace that already holds memories is **not merged into** — the call
//! returns `skipped_existing: true` and writes nothing, mirroring
//! [`crate::service::api::admin::bootstrap`]. Merging two memory histories
//! (id renumbering, dedup, conflict resolution) is a different problem with a
//! much worse failure mode, and an un-merged legacy file stays exactly where it
//! was: this route deletes nothing, the *caller* does, and only after a real
//! import.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use serde_json::{Value, json};

use crate::service::api::{InstanceQuery, instance_store};
use crate::service::error::{Result, ServiceError};
use crate::service::state::MemoryState;
use crate::sqlite::has_sqlite_magic;

/// Ceiling enforced on the request body, in bytes.
///
/// Spelled from the shared constant so the Node's pre-flight, the Gateway's
/// reverse-proxy cap and this handler cannot drift apart.
pub const MAX_BODY_BYTES: u64 = acowork_core::workspace::STORE_IMPORT_MAX_BYTES;

/// `POST /admin/import?instance_id=` — import one legacy workspace store.
///
/// Errors are a JSON body (not the bare status codes the panel routes use):
/// this response is read by the Node's migration pass, which logs it, and a
/// 400 with no reason would send an operator to the service log for a refusal
/// this handler already knows why it made.
pub async fn import_legacy(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
    body: Bytes,
) -> Result<Json<Value>> {
    if body.is_empty() {
        return Err(ServiceError::BadRequest(
            "empty upload: post the legacy private.sqlite bytes".to_string(),
        ));
    }
    if (body.len() as u64) > MAX_BODY_BYTES {
        return Err(ServiceError::BadRequest(format!(
            "upload is {} bytes, above the {} byte migration ceiling",
            body.len(),
            MAX_BODY_BYTES
        )));
    }
    // Reject a non-store payload before it can make us open (and create) a
    // namespace: `?instance_id=` is only resolved after the body is plausible.
    if !has_sqlite_magic(&body) {
        return Err(ServiceError::BadRequest(
            "upload is not a SQLite database file".to_string(),
        ));
    }

    let (instance_id, store) = instance_store(&state, query.as_deref())?;

    let zeroed = json!({
        "imported_nodes": 0,
        "imported_vectors": 0,
        "imported_lexical_rows": 0,
        "imported_purge_log_rows": 0,
        "skipped_existing": true,
    });
    if store.memory_node_count()? > 0 {
        tracing::info!(
            instance_id = %instance_id,
            "legacy import skipped: this instance already has memories"
        );
        return Ok(Json(zeroed));
    }

    let counts = store.import_legacy_store(&body)?;
    tracing::info!(
        instance_id = %instance_id,
        nodes = counts.nodes,
        vectors = counts.vectors,
        lexical_rows = counts.lexical_rows,
        purge_log_rows = counts.purge_log_rows,
        "imported a legacy workspace memory store"
    );
    Ok(Json(json!({
        "imported_nodes": counts.nodes,
        "imported_vectors": counts.vectors,
        "imported_lexical_rows": counts.lexical_rows,
        "imported_purge_log_rows": counts.purge_log_rows,
        "skipped_existing": false,
    })))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::http::StatusCode;
    use rusqlite::Connection;

    use super::*;
    use crate::service::api::admin;
    use crate::service::api::admin::BootstrapBody;
    use crate::service::config::{EmbeddingSection, MemoryServiceConfig};
    use crate::{
        AutobioCategory, AutobiographicalNode, CONVERSATION_LABEL, Episode, NodeStatus, labels,
    };

    fn state_in(dir: &Path) -> MemoryState {
        MemoryState::new(MemoryServiceConfig {
            data_dir: dir.to_path_buf(),
            embedding: EmbeddingSection {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap()
    }

    /// Build a legacy workspace store the way the Runtime used to: open the
    /// file through the production writers and seed memory rows **plus** the
    /// conversation / session rows that share the file (ADR-082).
    struct Legacy {
        bytes: Vec<u8>,
        episode_id: u64,
        autobio_id: u64,
    }

    fn legacy_store(db: &Path) -> Legacy {
        let store = crate::sqlite::SqliteStore::open(db, 2).expect("create legacy");
        let episode_id = store
            .store_episode(&Episode {
                session_id: "s1".into(),
                turn_index: 0,
                role: "user".into(),
                content: "the user prefers rust".into(),
                embedding: Some(vec![0.25, 0.75]),
                timestamp: chrono::Utc::now(),
                consolidated: false,
                metadata: Default::default(),
                importance: 0.8,
                knowledge_subtype: None,
                normalized: None,
            })
            .unwrap();
        // A promoted node that *references* the episode by id — the reason the
        // import preserves primary keys.
        let now = chrono::Utc::now();
        let autobio_id = store
            .store_autobiographical(&AutobiographicalNode {
                id: None,
                category: AutobioCategory::Preference,
                key: "language".into(),
                value: "prefers Rust".into(),
                confidence: 0.7,
                source_episode_id: Some(episode_id),
                source_episode_ids: vec![episode_id],
                promotion_metadata: None,
                // No embedding: the one vector in this store must be the only
                // vector that arrives, which is what proves the conversation
                // row's vector was filtered (copying it would also break the
                // `vectors.node_id` foreign key).
                embedding: None,
                status: NodeStatus::Active,
                created_at: now,
                updated_at: now,
                source: "distiller".into(),
                metadata: Default::default(),
            })
            .unwrap();
        // The Runtime-side subsystems that share this file: these must NOT be
        // copied into a memory namespace.
        {
            let conn = store.lock();
            conn.execute(
                "INSERT INTO nodes(id, label, status, props) VALUES (900, ?1, 'Active', '{}')",
                rusqlite::params![CONVERSATION_LABEL],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO vectors(node_id, dim, embedding) VALUES (900, 2, ?1)",
                rusqlite::params![vec![1u8; 8]],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions(session_id, agent_id, created_at, last_active_at) \
                 VALUES ('sess-1', 'com.acowork.x', 1, 2)",
                [],
            )
            .unwrap();
        }
        store.close().unwrap();
        drop(store);

        // Read the file back once the last handle is gone: no `-wal` remains,
        // which is exactly the state the Node uploads.
        let bytes = std::fs::read(db).expect("read legacy bytes");
        assert!(has_sqlite_magic(&bytes));
        Legacy {
            bytes,
            episode_id,
            autobio_id,
        }
    }

    async fn run(state: &MemoryState, id: &str, body: Vec<u8>) -> Result<Json<Value>> {
        import_legacy(
            State(state.clone()),
            Query(InstanceQuery {
                instance_id: Some(id.to_string()),
            }),
            Bytes::from(body),
        )
        .await
    }

    #[tokio::test]
    async fn copies_memories_and_leaves_the_runtime_tables_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let legacy_dir = tempfile::TempDir::new().unwrap();
        let legacy = legacy_store(&legacy_dir.path().join("private.sqlite"));
        let state = state_in(dir.path());
        let id = "21212121-2121-4121-8121-212121212121";

        let Json(out) = run(&state, id, legacy.bytes.clone()).await.unwrap();
        assert_eq!(out["imported_nodes"], 2);
        assert_eq!(out["imported_vectors"], 1);
        assert_eq!(out["imported_lexical_rows"], 2);
        assert_eq!(out["skipped_existing"], false);

        let store = state.store(id).unwrap();
        // Ids are preserved, so the reference inside `props` still resolves.
        let got = store
            .get_autobiographical(legacy.autobio_id)
            .unwrap()
            .unwrap();
        assert_eq!(got.source_episode_id, Some(legacy.episode_id));
        assert_eq!(
            got.source_episode_ids,
            vec![legacy.episode_id],
            "the multi-source list is preserved too"
        );
        assert_eq!(
            store
                .get_episode(legacy.episode_id)
                .unwrap()
                .unwrap()
                .content,
            "the user prefers rust"
        );
        // The conversation node, its vector and the session row did not travel.
        assert_eq!(store.node_count_by_label(CONVERSATION_LABEL).unwrap(), 0);
        assert_eq!(store.node_count().unwrap(), 2, "only the memory rows");
        assert_eq!(store.memory_node_count().unwrap(), 2);
        let vectors: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vectors, 1);
        let sessions: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 0);
    }

    /// A second upload of the same file is refused as already-imported and
    /// writes nothing — the caller only deletes local rows on a real import.
    #[tokio::test]
    async fn a_second_import_is_skipped_and_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let legacy_dir = tempfile::TempDir::new().unwrap();
        let legacy = legacy_store(&legacy_dir.path().join("private.sqlite"));
        let state = state_in(dir.path());
        let id = "22222222-2222-4222-8222-222222222222";

        let Json(first) = run(&state, id, legacy.bytes.clone()).await.unwrap();
        assert_eq!(first["skipped_existing"], false);
        let Json(out) = run(&state, id, legacy.bytes).await.unwrap();
        assert_eq!(out["skipped_existing"], true);
        assert_eq!(out["imported_nodes"], 0);
        assert_eq!(state.store(id).unwrap().memory_node_count().unwrap(), 2);
    }

    /// The bootstrap path can get there first; the migration then declines
    /// rather than duplicating a life history.
    #[tokio::test]
    async fn a_bootstrapped_instance_is_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        let legacy_dir = tempfile::TempDir::new().unwrap();
        let legacy = legacy_store(&legacy_dir.path().join("private.sqlite"));
        let state = state_in(dir.path());
        let id = "23232323-2323-4233-8233-232323232323";

        let Json(built) = admin::bootstrap(
            State(state.clone()),
            Query(InstanceQuery {
                instance_id: Some(id.to_string()),
            }),
            Json(
                serde_json::from_value::<BootstrapBody>(json!({
                    "agent_id": "com.acowork.x",
                    "name": "X",
                }))
                .unwrap(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(built["skipped_existing"], false);

        let Json(out) = run(&state, id, legacy.bytes).await.unwrap();
        assert_eq!(out["skipped_existing"], true);
    }

    #[tokio::test]
    async fn refuses_a_file_that_is_not_a_sqlite_database() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = state_in(dir.path());
        let id = "24242424-2424-4244-8244-242424242424";

        let err = run(&state, id, b"grafeo-engine v1 not a sqlite file".to_vec())
            .await
            .expect_err("must reject");
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
        // No namespace was created for a payload we cannot read.
        assert!(!state.instances().exists(id));
        assert!(state.instances().list().is_empty());
    }

    #[tokio::test]
    async fn refuses_an_empty_upload_and_an_over_limit_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = state_in(dir.path());
        let id = "25252525-2525-4255-8255-252525252525";

        assert_eq!(
            run(&state, id, Vec::new())
                .await
                .expect_err("empty")
                .http_status(),
            StatusCode::BAD_REQUEST
        );
        let mut oversized = Vec::new();
        // Only the size is checked against the ceiling, so a real header plus
        // padding is enough to be over the line.
        oversized.extend_from_slice(b"SQLite format 3\0");
        oversized.resize(MAX_BODY_BYTES as usize + 1, 0);
        let err = run(&state, id, oversized)
            .await
            .expect_err("over the ceiling");
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
        assert!(err.to_string().contains("ceiling"), "{err}");
    }

    /// A database that is valid SQLite but holds no memory tables is not an
    /// error: there is simply nothing to migrate, and the caller gets a zero
    /// count it can record so the scan does not repeat.
    #[tokio::test]
    async fn a_store_without_memory_tables_imports_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let other = tempfile::TempDir::new().unwrap();
        let path = other.path().join("unrelated.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE things(id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(has_sqlite_magic(&bytes));

        let state = state_in(dir.path());
        let Json(out) = run(&state, "26262626-2626-4266-8266-262626262626", bytes)
            .await
            .unwrap();
        assert_eq!(out["imported_nodes"], 0);
        assert_eq!(out["skipped_existing"], false);
    }

    /// Without a usable namespace the upload must not be written anywhere.
    #[tokio::test]
    async fn an_unusable_instance_id_never_reaches_a_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let legacy_dir = tempfile::TempDir::new().unwrap();
        let legacy = legacy_store(&legacy_dir.path().join("private.sqlite"));
        let state = state_in(dir.path());

        let err = import_legacy(
            State(state.clone()),
            Query(InstanceQuery {
                instance_id: Some("../escape".to_string()),
            }),
            Bytes::from(legacy.bytes),
        )
        .await
        .expect_err("must reject");
        assert_eq!(err.http_status(), StatusCode::BAD_REQUEST);
        assert!(state.instances().list().is_empty());
    }

    /// The copy scope must stay exactly the memory labels: the same list
    /// drives the deletion half in `legacy_import`, and a widened `ALL` would
    /// silently start copying a subsystem the service does not own.
    #[test]
    fn the_migration_scope_is_the_four_memory_labels() {
        assert_eq!(
            labels::ALL,
            &[
                labels::EPISODIC,
                labels::KNOWLEDGE,
                labels::PROCEDURAL,
                labels::AUTOBIOGRAPHICAL,
            ]
        );
    }
}

