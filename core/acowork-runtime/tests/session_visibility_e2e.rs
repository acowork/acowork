//! Design 27 §8.2/§8.3 — `Members` visibility against the real Runtime
//! HTTP surface.
//!
//! Wave 4 unit-tested `is_readable_by` and the SQLite encoding; this file
//! closes the chain the Gateway proxy actually walks: a `chat:{id}`-bound
//! execution session is listed/readable for a caller whose injected
//! `x-acowork-chat-scope` contains that Chat session, and is a hidden 404
//! the moment membership is revoked (§8.3 asymmetry: the chat rows the
//! kicked member wrote stay, the execution record does not).
//!
//! Boots the real `RuntimeHttpServer` (same shape as `global_search_e2e`
//! Tier 1, every unrelated slot `None`) over a seeded session-meta store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use acowork_core::providers::traits::UsageInfo;
use acowork_core::session_meta::{SessionMeta, SessionMetaStore, SessionVisibility};
use acowork_runtime::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use acowork_runtime::http::server::RuntimeHttpServer;
use acowork_runtime::usecases::agent_token::AgentTokenService;
use acowork_runtime::usecases::RuntimeSessionMetadataService;
use acowork_sqlite::{SqliteSessionMetaStore, SqliteStore};

const AGENT_ID: &str = "com.test.visibility";
const INSTANCE_ID: &str = "7a7a7a7a-1111-4bbb-8ccc-000000000001";

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

fn meta(sid: &str, owner: &str, visibility: Option<SessionVisibility>, chat: Option<&str>) -> SessionMeta {
    SessionMeta {
        version: 4,
        session_id: sid.to_string(),
        agent_id: AGENT_ID.to_string(),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        user_id: Some(owner.to_string()),
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
        message_count: 1,
        last_active_at: "2026-01-01T00:00:00.000Z".to_string(),
        tokens: None,
        llm_call_counter: None,
        perf: None,
        model_ratio: None,
        last_compaction_offset: None,
        corrupted: false,
        chat_session_id: chat.map(str::to_string),
        model_usage: None,
    }
}

/// Boot the server over a seeded workspace; returns the port and the
/// inbound dispatch receiver (keep it alive: dropping it makes every
/// queued action fail with 503 instead of reaching the loop).
async fn boot(work_dir: PathBuf) -> (u16, tokio::sync::mpsc::UnboundedReceiver<(String, acowork_runtime::agent::inbound::InboundMessage)>) {
    std::fs::create_dir_all(work_dir.join("memory")).unwrap();
    std::fs::create_dir_all(work_dir.join("conversations")).unwrap();
    let db = SqliteStore::open_dim_agnostic(work_dir.join("memory").join("private.sqlite")).unwrap();
    let store = SqliteSessionMetaStore::new(Arc::new(db));
    store.upsert(&meta("s-group", "u-alice", Some(SessionVisibility::Members), Some("chat-1"))).unwrap();
    store.upsert(&meta("s-private", "u-alice", Some(SessionVisibility::Private), None)).unwrap();
    store.upsert(&meta("s-other-chat", "u-bob", Some(SessionVisibility::Members), Some("chat-2"))).unwrap();
    store.upsert(&meta("s-default", "u-alice", None, None)).unwrap();
    // The shape chat routing actually creates: no owner, `Members`, bound
    // to a Chat the answering human is a participant of.
    let mut unowned = meta("s-unowned", "unused", Some(SessionVisibility::Members), Some("chat-1"));
    unowned.user_id = None;
    store.upsert(&unowned).unwrap();

    let (dispatch_tx, dispatch_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, acowork_runtime::agent::inbound::InboundMessage)>();

    let snapshots: SharedSessionSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
    let session_metadata: Arc<
        tokio::sync::Mutex<Option<Arc<dyn acowork_runtime::usecases::session_metadata::SessionMetadataService>>>,
    > = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(RuntimeSessionMetadataService::new(
        work_dir.clone(),
        Arc::new(NoopTokens),
        snapshots.clone(),
        latest.clone(),
    )))));

    let server = RuntimeHttpServer::start_with_bind_port(
        0,
        work_dir.clone(),
        work_dir.clone(),
        AGENT_ID.to_string(),
        INSTANCE_ID.to_string(),
        snapshots,
        latest,
        Arc::new(tokio::sync::Mutex::new(Some(dispatch_tx))),
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
    (server.port, dispatch_rx)
}

async fn list(port: u16, user: Option<&str>, chats: Option<&str>) -> Vec<String> {
    let mut req = reqwest::Client::new().get(format!("http://127.0.0.1:{port}/sessions"));
    if let Some(u) = user {
        req = req.header("x-user-id", u);
    }
    if let Some(c) = chats {
        req = req.header("x-acowork-chat-scope", c);
    }
    let resp = req.send().await.expect("list request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "list 200");
    let v: serde_json::Value = resp.json().await.unwrap();
    v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap().to_string())
        .collect()
}

async fn detail_status(port: u16, sid: &str, user: &str, chats: &str) -> u16 {
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/sessions/{sid}"))
        .header("x-user-id", user)
        .header("x-acowork-chat-scope", chats)
        .send()
        .await
        .expect("detail request");
    resp.status().as_u16()
}

#[tokio::test]
async fn members_visibility_follows_chat_membership() {
    let dir = tempfile::tempdir().unwrap();
    let (port, _rx) = boot(dir.path().to_path_buf()).await;

    // Owner reads own sessions regardless; membership adds the group's
    // execution session for *other* chat members (§8.2: the participant
    // set is the read ACL, no materialised grant table).
    let alice = list(port, Some("u-alice"), Some("chat-1")).await;
    assert!(alice.contains(&"s-group".to_string()), "owner + member: {alice:?}");
    assert!(alice.contains(&"s-private".to_string()));
    assert!(alice.contains(&"s-default".to_string()), "absent visibility = public (ADR-076)");
    assert!(!alice.contains(&"s-other-chat".to_string()), "not in chat-2: {alice:?}");

    // Bob owns s-other-chat; chat-1 membership grants him s-group too.
    let bob = list(port, Some("u-bob"), Some("chat-1")).await;
    assert!(bob.contains(&"s-group".to_string()), "chat member reads the bound session: {bob:?}");
    assert!(bob.contains(&"s-other-chat".to_string()));
    assert!(!bob.contains(&"s-private".to_string()), "someone else's private stays hidden");

    // §8.3 revocation: kicked from the chat (scope no longer carries
    // chat-1) → the bound execution session vanishes from list AND detail,
    // indistinguishable from "does not exist".
    let kicked = list(port, Some("u-bob"), Some("")).await;
    assert!(!kicked.contains(&"s-group".to_string()), "revoked membership drops it: {kicked:?}");
    assert_eq!(detail_status(port, "s-group", "u-bob", "").await, 404, "hidden 404 after kick");
    assert_eq!(detail_status(port, "s-group", "u-bob", "chat-1").await, 200, "member reads detail");

    // Owner fallback: alice keeps reading s-group with an empty scope.
    assert_eq!(detail_status(port, "s-group", "u-alice", "").await, 200, "owner fallback");

    // No identity header at all = Unfiltered (local mode / admin).
    let all = list(port, None, None).await;
    assert_eq!(all.len(), 5, "unfiltered sees everything: {all:?}");
}

/// Design 27 §5.5.4 regression: chat-routed execution sessions are
/// ownerless + `Members`, and the Gateway's card resolve injects the
/// answering human's real uid with the chat-membership set — never the
/// admin wildcard. These endpoints gated on `authorize_write`, whose
/// predicate ignores membership, so **every** chat-card answer 404'd and
/// silently degraded into a plain new turn while the tool kept waiting.
/// They gate on `authorize_read` now; real session mutations stay gated
/// on write.
#[tokio::test]
async fn card_answers_gate_on_membership_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let (port, mut rx) = boot(dir.path().to_path_buf()).await;

    let post = |sid: &str, path: &str, user: &str, chats: &str, body: serde_json::Value| {
        let sid = sid.to_string();
        let path = path.to_string();
        let user = user.to_string();
        let chats = chats.to_string();
        tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/sessions/{sid}/{path}"))
                .header("x-user-id", user)
                .header("x-acowork-chat-scope", chats)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        })
    };

    // Chat member (not the owner — there is none) answers the question
    // card: 202, and the answer reaches the dispatch channel.
    let status = post(
        "s-unowned",
        "answer",
        "u-bob",
        "chat-1",
        serde_json::json!({"request_id": "q-1", "answer": "A"}),
    )
    .await
    .unwrap();
    assert_eq!(status, 202, "member may answer ask_question on an ownerless Members session");
    let (sid, msg) = rx.recv().await.expect("answer dispatched");
    assert_eq!(sid, "s-unowned");
    assert!(matches!(msg, acowork_runtime::agent::inbound::InboundMessage::QuestionAnswer { .. }));

    // Same for the approval card.
    let status = post(
        "s-unowned",
        "approval",
        "u-bob",
        "chat-1",
        serde_json::json!({"request_id": "a-1", "approved": true}),
    )
    .await
    .unwrap();
    assert_eq!(status, 202, "member may decide approval on an ownerless Members session");
    let (_, msg) = rx.recv().await.expect("approval dispatched");
    assert!(matches!(msg, acowork_runtime::agent::inbound::InboundMessage::ApprovalDecision { .. }));

    // Fail-closed halves: revoked membership still 404 (hidden, not 403 —
    // no existence oracle), and an unrelated account cannot reach a
    // Private session's answer channel either.
    assert_eq!(
        post("s-unowned", "answer", "u-bob", "", serde_json::json!({"request_id": "q-1", "answer": "A"}))
            .await
            .unwrap(),
        404,
        "non-member answer stays 404"
    );
    assert_eq!(
        post("s-private", "answer", "u-bob", "chat-1", serde_json::json!({"request_id": "q-1", "answer": "A"}))
            .await
            .unwrap(),
        404,
        "answer to someone else's Private session stays 404"
    );

    // The boundary of the relaxation: `stop` is a real session mutation
    // and stays write-gated — an ownerless Members session is still not
    // interruptible by a member through the direct session surface.
    let stop = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/sessions/s-unowned/stop"))
            .header("x-user-id", "u-bob".to_string())
            .header("x-acowork-chat-scope", "chat-1".to_string())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    })
    .await
    .unwrap();
    assert_eq!(stop, 404, "stop remains write-gated (§8.2)");
}
