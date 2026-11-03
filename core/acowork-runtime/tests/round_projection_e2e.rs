//! ADR-106 D4/P2 — the round projection read over the real HTTP surface.
//!
//! The property this pins is the one the whole reference-projection design rests
//! on: a Chat block renders from a *round reference* plus a read, and that read
//! works against **disk** — no resident session, no live writer, no MQTT. If the
//! Runtime has since closed (or pruned, or restarted on another host) the round is
//! still there, because its coordinates and its rows both live in the session's own
//! directory.
//!
//! The second property is the boundary ADR-076 draws: a round you may not read the
//! session for is a 404, and a round the session does not have is an explicit hole
//! (`found: false`), never a silently dropped entry.
//!
//! Boots the real `RuntimeHttpServer` (same shape as `session_visibility_e2e`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acowork_core::providers::traits::UsageInfo;
use acowork_core::session_meta::{SessionMeta, SessionMetaStore, SessionVisibility};
use acowork_runtime::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use acowork_runtime::http::server::RuntimeHttpServer;
use acowork_runtime::usecases::agent_token::AgentTokenService;
use acowork_runtime::usecases::RuntimeSessionMetadataService;
use acowork_sqlite::{SqliteSessionMetaStore, SqliteStore};

const AGENT_ID: &str = "com.test.rounds";
const INSTANCE_ID: &str = "7a7a7a7a-1111-4bbb-8ccc-000000000002";
const SID: &str = "s-closed";

struct NoopTokens;
impl AgentTokenService for NoopTokens {
    fn accumulate_llm_usage(&self, _usage: &UsageInfo) {}
    fn merge_token_totals(&self, _scanned: (Option<u64>, Option<u64>, Option<u64>, Option<u64>)) {}
    fn agent_token_totals(&self) -> (u64, u64, u64, u64) {
        (0, 0, 0, 0)
    }
    fn session_token_totals(&self, _sid: &str) -> Option<(u64, u64)> {
        None
    }
}

struct NoopChatUsage;
impl acowork_runtime::usecases::chat_usage::ChatUsageService for NoopChatUsage {
    fn record_chat_llm_usage(
        &self,
        _chat_session_id: &str,
        _chat_kind: &str,
        _usage: &UsageInfo,
        _provider_label: &str,
        _model_id: &str,
        _history_tokens: Option<u64>,
    ) {
    }
    fn forget_chat(&self, _chat_session_id: &str) -> bool {
        false
    }
    fn get_chat(&self, _chat_session_id: &str) -> Option<acowork_sqlite::ChatUsageRow> {
        None
    }
    fn ledger_totals(&self) -> (u64, u64, u64, u64) {
        (0, 0, 0, 0)
    }
}

fn meta(sid: &str, owner: &str, visibility: Option<SessionVisibility>) -> SessionMeta {
    SessionMeta {
        version: 4,
        session_id: sid.to_string(),
        agent_id: AGENT_ID.to_string(),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        initiator: Some(owner.to_string()),
        visibility,
        title: Some(sid.to_string()),
        workspace_id: None,
        model: Some("test-model".into()),
        provider: Some("test-provider".into()),
        account_id: None,
        reasoning_effort: None,
        temperature: None,
        context_window: None,
        todos: None,
        message_count: 3,
        last_active_at: "2026-01-01T00:00:00.000Z".to_string(),
        tokens: None,
        llm_call_counter: None,
        perf: None,
        model_ratio: None,
        last_compaction_offset: None,
        corrupted: false,
        chat_session_id: Some("chat-1".to_string()),
        model_usage: None,
    }
}

/// One line of the seeded transcript. `round` is the ADR-106 D2 stamp — without it
/// the row belongs to no round and must not surface in any projection.
fn line(role: &str, content: &str, round: Option<&str>) -> String {
    let metadata = round.map(|r| serde_json::json!({ "round_id": r }));
    serde_json::to_string(&serde_json::json!({
        "id": format!("{role}-{}", content.len()),
        "ts": "2026-01-01T00:00:01.000Z",
        "role": role,
        "content": content,
        "metadata": metadata,
    }))
    .unwrap()
}

/// Seed `{work}/conversations/{sid}.jsonl` and its round side table by hand: this
/// test's whole point is that the read needs no session object to produce them.
fn seed_files(work_dir: &Path) {
    let conv = work_dir.join("conversations");
    std::fs::create_dir_all(&conv).unwrap();
    let transcript = [
        line("user", "hello", Some("r:m-1")),
        // Attachment row (ADR-046): a `system` row carrying a `file_upload` type,
        // written right after the user row. The projection folds it.
        line_upload(),
        line("tool_call", "execution detail that must never reach the chat", Some("r:m-1")),
        line("assistant", "the answer", Some("r:m-1")),
        line("system", "written outside any turn", None),
    ]
    .join("\n");
    std::fs::write(conv.join(format!("{SID}.jsonl")), format!("{transcript}\n")).unwrap();

    let round = serde_json::json!({
        "round_id": "r:m-1",
        "user": 0,
        "answer": 3,
        "card": { "kind": "pause", "request_id": "q-1" },
        "terminal": true,
        "status": "completed",
        "ts": "2026-01-01T00:00:02.000Z",
    });
    std::fs::write(
        // ADR-106 D4: the side table is `{sid}.jsonl.rounds` — a suffix on the
        // transcript's own name, so it is never mistaken for a session file.
        conv.join(format!("{SID}.jsonl.rounds")),
        format!("{}\n", serde_json::to_string(&round).unwrap()),
    )
    .unwrap();
}

/// The attachment row needs the typed metadata shape, so it is written separately.
fn line_upload() -> String {
    serde_json::to_string(&serde_json::json!({
        "id": "att-1",
        "ts": "2026-01-01T00:00:01.001Z",
        "role": "system",
        "content": "Uploaded file: spec.pdf",
        "metadata": {
            "round_id": "r:m-1",
            "type": "file_upload",
            "document_id": "d-1",
            "filename": "spec.pdf",
            "format": "pdf",
            "size_bytes": 1234,
        },
    }))
    .unwrap()
}

async fn boot(work_dir: PathBuf) -> u16 {
    std::fs::create_dir_all(work_dir.join("memory")).unwrap();
    std::fs::create_dir_all(work_dir.join("conversations")).unwrap();
    let db = SqliteStore::open_dim_agnostic(work_dir.join("memory").join("private.sqlite")).unwrap();
    let store = SqliteSessionMetaStore::new(Arc::new(db));
    store
        .upsert(&meta(SID, "u-alice", Some(SessionVisibility::Private)))
        .unwrap();
    store
        .upsert(&meta("s-other", "u-bob", Some(SessionVisibility::Private)))
        .unwrap();
    seed_files(&work_dir);

    let (_dispatch_tx, dispatch_rx) =
        tokio::sync::mpsc::unbounded_channel::<(
            String,
            acowork_runtime::agent::inbound::InboundMessage,
        )>();
    let snapshots: SharedSessionSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
    let session_metadata: Arc<
        tokio::sync::Mutex<
            Option<Arc<dyn acowork_runtime::usecases::session_metadata::SessionMetadataService>>,
        >,
    > = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(RuntimeSessionMetadataService::new(
        work_dir.clone(),
        Arc::new(NoopTokens),
        Arc::new(NoopChatUsage),
        snapshots.clone(),
        latest.clone(),
    )))));
    // Keep the dispatch receiver alive so the router's slots stay populated.
    drop(dispatch_rx);

    let server = RuntimeHttpServer::start_with_bind_port(
        0,
        work_dir.clone(),
        work_dir.clone(),
        AGENT_ID.to_string(),
        INSTANCE_ID.to_string(),
        snapshots,
        latest,
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(std::sync::RwLock::new(Vec::new())),
        Arc::new(tokio::sync::Mutex::new(None)),
        session_metadata,
        Arc::new(std::sync::RwLock::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(std::sync::RwLock::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(std::sync::RwLock::new(
            acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        )),
        Arc::new(tokio::sync::RwLock::new(None)),
        Arc::new(std::sync::RwLock::new(None)),
    )
    .await
    .expect("runtime http server should start");
    server.port
}

async fn get_round(port: u16, path: &str, user: &str) -> (u16, serde_json::Value) {
    get_round_scoped(port, path, user, "chat-1").await
}

/// Same read with a caller-chosen Chat scope. The binding a projection carries must
/// come from the session's own record, so this is the shape that proves it: a
/// caller naming other chats in the scope header changes nothing upstream.
async fn get_round_scoped(
    port: u16,
    path: &str,
    user: &str,
    scope: &str,
) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .header("x-user-id", user)
        .header("x-acowork-chat-scope", scope)
        .send()
        .await
        .expect("round request");
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// The single-round read resolves a closed session's round entirely off disk:
/// the user row, the folded attachment, the answer at its recorded coordinate —
/// and none of the execution detail in between.
#[tokio::test]
async fn round_projection_reads_a_session_that_is_not_open() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    let (status, body) = get_round(port, &format!("/sessions/{SID}/rounds/r:m-1"), "u-alice").await;
    assert_eq!(status, 200, "owner reads the round: {body}");
    assert_eq!(body["round_id"], "r:m-1");
    assert_eq!(body["user"]["body"], "hello");
    assert_eq!(body["answer"]["body"], "the answer");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["terminal"], true);
    assert_eq!(body["card"]["request_id"], "q-1");
    assert_eq!(
        body["user"]["attachments"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0),
        1,
        "the attachment row folds into the user half"
    );
    let rendered = body.to_string();
    assert!(
        !rendered.contains("execution detail"),
        "ADR-100 D3: execution detail never enters the projection"
    );
    assert!(
        !rendered.contains("outside any turn"),
        "rows stamped to no round stay out of the block"
    );
    // The Chat binding the chat-side cache is guarded by (ADR-106 D10). It is
    // read off the session's own record, so a caller holding a scope header
    // cannot steer it: the value below comes from disk even though the request
    // also claims `chat-OTHER`.
    assert_eq!(body["chat_session_id"], "chat-1", "binding is the session's own");
    let (status, body) = get_round_scoped(
        port,
        &format!("/sessions/{SID}/rounds/r:m-1"),
        "u-alice",
        "chat-OTHER,chat-1",
    )
    .await;
    assert_eq!(status, 200, "a wider scope still reads: {body}");
    assert_eq!(
        body["chat_session_id"], "chat-1",
        "the header cannot rewrite the binding — that is what makes the cache safe"
    );
}

/// Read ACL is the session's: a round of someone else's Private session is the same
/// 404 as one that does not exist, and a round the session does not have is a 404
/// too — never an empty projection the chat could render as "nothing happened".
#[tokio::test]
async fn round_read_gates_on_session_visibility_and_names_its_holes() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    assert_eq!(
        get_round(port, &format!("/sessions/{SID}/rounds/r:m-1"), "u-bob")
            .await
            .0,
        404,
        "no existence oracle for a private session"
    );
    assert_eq!(
        get_round(port, &format!("/sessions/{SID}/rounds/r:nope"), "u-alice")
            .await
            .0,
        404,
        "unknown round is a 404, not an empty body"
    );

    // Batch: the missing id comes back as an explicit hole, so the caller can cache
    // "this block is unreachable" instead of re-asking every render.
    let (status, body) =
        get_round(port, &format!("/sessions/{SID}/rounds?ids=r:m-1,r:gone"), "u-alice").await;
    assert_eq!(status, 200, "batch read: {body}");
    let rounds = body["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 2, "one entry per id asked");
    assert_eq!(rounds[0]["found"], true);
    assert_eq!(rounds[0]["answer"]["body"], "the answer");
    assert_eq!(rounds[0]["chat_session_id"], "chat-1", "batch stamps too");
    assert_eq!(rounds[1]["round_id"], "r:gone");
    assert_eq!(rounds[1]["found"], false, "a hole is stated, not dropped");
    assert!(
        rounds[1].get("chat_session_id").is_none(),
        "a hole carries no binding, so no caller can cache one as a fact"
    );

    // `ids` is required: an unbounded "give me every round" has no place on a
    // request path (ADR-096), and a silently-empty list would look like a success.
    assert_eq!(
        get_round(port, &format!("/sessions/{SID}/rounds"), "u-alice")
            .await
            .0,
        400,
        "ids is required"
    );
}

/// A session id is only ever used to name a file, so the surface must refuse one
/// that is not a file name — a `..` or a separator would let a caller read any
/// transcript under the Runtime's data directory.
#[tokio::test]
async fn path_shaped_ids_are_refused_before_the_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    for sid in ["..", "..%2F..", "a.b", "a b"] {
        let (status, _) = get_round(port, &format!("/sessions/{sid}/rounds/r:m-1"), "u-alice").await;
        assert_eq!(status, 404, "malformed session id {sid:?} must not reach disk");
    }
    let (status, _) = get_round(port, &format!("/sessions/{SID}/rounds/r:with space"), "u-alice").await;
    assert_eq!(status, 400, "a malformed round id is a client error, not a hole");
}
