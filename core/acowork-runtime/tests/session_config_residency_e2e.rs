//! `PUT/GET /sessions/{sid}/config` against a session this process does **not**
//! hold open.
//!
//! # The defect this file pins
//!
//! `RuntimeSessionConfigService` resolved a session through `session_configs`
//! — the table of *resident* sessions — and answered
//! `Config error: Session not found: <sid>` (HTTP 500) for everything else.
//! Residency is an implementation detail of one Runtime process; the history
//! list is full of sessions nobody has open, so "rename this session" failed
//! with an error that reads like a permission or routing fault while the
//! session sat right there in the list. The desktop swallowed that 500
//! (`agentStore.renameSession`), so a rename looked like it worked and was
//! gone by the next refresh.
//!
//! Every field the config surface writes is a fact about the session's
//! `SessionMeta` row, so the fix routes the non-resident case at the row
//! instead of the live map. This test walks the real HTTP surface with the
//! real service and asserts *both* directions and *both* residencies, because
//! the two paths must be indistinguishable from the client's side.
//!
//! Boot shape mirrors `session_visibility_e2e` (Tier 1: real
//! `RuntimeHttpServer`, every unrelated slot `None`), with the one difference
//! that matters here — the session-config service is wired for real.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use acowork_core::providers::traits::UsageInfo;
use acowork_core::session_meta::{SessionMeta, SessionMetaStore, TitleSource};
use acowork_runtime::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use acowork_runtime::conversation::{ConversationSession, SessionConfig};
use acowork_runtime::http::server::RuntimeHttpServer;
use acowork_runtime::usecases::agent_token::AgentTokenService;
use acowork_runtime::usecases::{
    RuntimeSessionConfigService, RuntimeSessionMetadataService, SharedSessionConfigs,
};
use acowork_sqlite::{SqliteSessionMetaStore, SqliteStore};

const AGENT_ID: &str = "com.test.config-residency";
const INSTANCE_ID: &str = "7a7a7a7a-2222-4bbb-8ccc-000000000002";
const OWNER: &str = "u-alice";

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

fn meta(sid: &str, title: Option<&str>, source: Option<TitleSource>) -> SessionMeta {
    SessionMeta {
        version: 4,
        session_id: sid.to_string(),
        agent_id: AGENT_ID.to_string(),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        initiator: Some(OWNER.to_string()),
        visibility: None,
        chat_session_id: None,
        aware_users: Vec::new(),
        interveners: Vec::new(),
        title: title.map(str::to_string),
        title_source: source,
        workspace_id: None,
        model: Some("test-model".into()),
        provider: Some("test-provider".into()),
        account_id: None,
        reasoning_effort: None,
        temperature: None,
        context_window: None,
        todos: None,
        tool_policy: None,
        message_count: 1,
        last_active_at: "2026-01-01T00:00:00.000Z".to_string(),
        tokens: None,
        llm_call_counter: None,
        perf: None,
        model_ratio: None,
        last_compaction_offset: None,
        corrupted: false,
        model_usage: None,
    }
}

/// The runtime's session-meta store for a work dir — the same file
/// `read_session_meta` opens on demand.
fn meta_store(work_dir: &Path) -> SqliteSessionMetaStore {
    std::fs::create_dir_all(work_dir.join("memory")).unwrap();
    let db = SqliteStore::open_dim_agnostic(work_dir.join("memory").join("private.sqlite")).unwrap();
    SqliteSessionMetaStore::new(Arc::new(db))
}

/// Boot the HTTP surface with a session-config service over `resident`.
///
/// `resident` is the process's live-session table: empty = "every session in
/// the history list", which is the state the defect was reported from.
async fn boot(
    work_dir: PathBuf,
    resident: Vec<Arc<ConversationSession>>,
) -> u16 {
    std::fs::create_dir_all(work_dir.join("conversations")).unwrap();
    let snapshots: SharedSessionSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));

    let mut map = HashMap::new();
    for conv in resident {
        map.insert(conv.session_id().to_string(), conv);
    }
    let shared_configs: SharedSessionConfigs = Arc::new(std::sync::RwLock::new(map));

    let resolver = Arc::new(std::sync::RwLock::new(
        acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
    ));
    let config_svc: Arc<dyn acowork_runtime::usecases::SessionConfigService> = Arc::new(
        RuntimeSessionConfigService::new(
            shared_configs,
            work_dir.clone(),
            Some(resolver.clone()),
            Arc::new(std::sync::RwLock::new(None)),
        ),
    );

    let session_metadata: Arc<
        tokio::sync::Mutex<
            Option<Arc<dyn acowork_runtime::usecases::session_metadata::SessionMetadataService>>,
        >,
    > = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
        RuntimeSessionMetadataService::new(
            work_dir.clone(),
            Arc::new(NoopTokens),
            Arc::new(NoopChatUsage),
            snapshots.clone(),
            latest.clone(),
        ),
    ))));

    let (dispatch_tx, _dispatch_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = RuntimeHttpServer::start_with_bind_port(
        0,
        work_dir.clone(),
        work_dir,
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
        Arc::new(tokio::sync::Mutex::new(Some(config_svc))),
        Arc::new(std::sync::RwLock::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        resolver,
        Arc::new(tokio::sync::RwLock::new(None)),
        Arc::new(std::sync::RwLock::new(None)),
    )
    .await
    .expect("runtime http server should start");
    server.port
}

async fn put_title(port: u16, sid: &str, title: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .put(format!("http://127.0.0.1:{port}/sessions/{sid}/config"))
        .header("x-user-id", OWNER)
        .json(&serde_json::json!({ "title": title }))
        .send()
        .await
        .expect("config PUT should reach the server");
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn get_config(port: u16, sid: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/sessions/{sid}/config"))
        .header("x-user-id", OWNER)
        .send()
        .await
        .expect("config GET should reach the server");
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// The listed title for `sid`, straight off `GET /sessions` — what the user
/// sees after a refresh, and the surface that showed the old name back.
async fn listed_title(port: u16, sid: &str) -> Option<String> {
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/sessions"))
        .header("x-user-id", OWNER)
        .send()
        .await
        .expect("list should reach the server");
    assert_eq!(resp.status().as_u16(), 200, "GET /sessions must stay 200");
    let v: serde_json::Value = resp.json().await.unwrap();
    v["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .find(|s| s["session_id"].as_str() == Some(sid))
        .and_then(|s| s["title"].as_str().map(str::to_string))
}

/// M3: renaming a session nobody has open is a rename, not a 500.
///
/// Four assertions, one per broken link in the old chain: the write is
/// accepted, the row carries the new title *and* the human as its author, the
/// config read agrees, and the list the user refreshes shows the new name.
#[tokio::test]
async fn renaming_a_non_resident_session_succeeds_and_shows_up_in_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().to_path_buf();
    let store = meta_store(&work_dir);
    store
        .upsert(&meta("s-history", Some("第一条消息"), None))
        .unwrap();

    // No live sessions at all: exactly the reported situation.
    let port = boot(work_dir.clone(), Vec::new()).await;

    let (status, body) = put_title(port, "s-history", "分支出重构").await;
    assert_eq!(status, 200, "non-resident rename must be accepted: {body}");

    let row = store.get("s-history").unwrap().expect("row survives");
    assert_eq!(row.title.as_deref(), Some("分支出重构"), "title persisted");
    assert_eq!(
        row.title_source,
        Some(TitleSource::User),
        "a rename claims the title for the human, so the automatic path cannot take it back"
    );
    assert_eq!(row.model.as_deref(), Some("test-model"), "untouched fields survive");
    assert_eq!(row.message_count, 1);
    assert_ne!(
        row.last_active_at, "2026-01-01T00:00:00.000Z",
        "rename stamps activity like the resident path does"
    );

    let (status, config) = get_config(port, "s-history").await;
    assert_eq!(status, 200, "config read of a non-resident session: {config}");
    assert_eq!(config["title"].as_str(), Some("分支出重构"));

    assert_eq!(
        listed_title(port, "s-history").await.as_deref(),
        Some("分支出重构"),
        "the refreshed list shows the rename"
    );
}

/// The same delta through the **resident** path must land in the same place.
///
/// This is the control: the fix moved the non-resident case onto the row, it
/// did not move the resident case off it. A rename on an open session has to
/// keep updating the live conversation (the running loop reads it) *and* end
/// up on the row — otherwise the two residencies would disagree after a
/// restart.
#[tokio::test]
async fn renaming_a_resident_session_updates_the_live_session_and_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let work_dir = dir.path().to_path_buf();
    let store = meta_store(&work_dir);

    let sid = "s-live";
    let (conv, _config_rx, _state_rx) = ConversationSession::new(
        &work_dir,
        sid,
        SessionConfig {
            agent_id: AGENT_ID.to_string(),
            workspace_id: None,
            model: None,
            provider: None,
        },
        0,
        Arc::new(AtomicUsize::new(0)),
    )
    .unwrap();
    let conv = Arc::new(conv);
    assert!(
        conv.set_generated_title("第一条消息"),
        "the automatic path titles the fresh session"
    );
    assert_eq!(conv.title().as_deref(), Some("第一条消息"));

    let port = boot(work_dir, vec![conv.clone()]).await;

    let (status, body) = put_title(port, sid, "人工标题").await;
    assert_eq!(status, 200, "resident rename: {body}");
    assert_eq!(conv.title().as_deref(), Some("人工标题"), "live session renamed");
    assert_eq!(
        conv.title_source(),
        TitleSource::User,
        "live session remembers who owns the title"
    );
    assert_eq!(
        store.get(sid).unwrap().unwrap().title.as_deref(),
        Some("人工标题"),
        "the row a restart would read carries the rename"
    );
}