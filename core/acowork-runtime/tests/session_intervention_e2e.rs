//! Plan D1/D2 — the intervention tier against the real Runtime HTTP surface.
//!
//! An agent-to-agent execution session has an **agent** initiator, so before
//! this tier existed the only party that could write it was the agent that
//! dispatched the work — which is told "failed" and never retries. A run whose
//! launch parameters were unusable (a model with no quota, say) was therefore
//! unrecoverable by construction: no human could change the model, send a
//! message, or press the continue the run was waiting on.
//!
//! What this file pins, over the same booted-server harness as
//! `session_visibility_e2e`:
//!
//! * the roster the Gateway seeds as `aware_users` is stamped as `interveners`
//!   too, and the listing advertises the tier (`authority` / `config_fields`)
//!   so a surface can enable exactly the controls the server accepts;
//! * the tier is **narrow**: model / provider / account / effort / temperature /
//!   context window. The task's own premise — workspace, tool set, title, who
//!   may read it, whether it exists — stays the owner's, and a refusal names
//!   the field instead of failing opaquely;
//! * a stranger is not merely refused, it is invisible (404 via the read
//!   axis), which is the ADR-076 §决策 4 rule unchanged.
//!
//! Tier refusals are asserted on the status/body the handler produces *before*
//! it touches the session-config service, so the assertions do not depend on a
//! live agent loop: this harness deliberately wires `session_config` to `None`,
//! and a `403` can only come from the authorization table.

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

const AGENT_ID: &str = "com.test.intervention";
const INSTANCE_ID: &str = "7a7a7a7a-2222-4bbb-8ccc-000000000002";
/// The dispatching agent's instance id — an agent, never a human, so the
/// session has an owner that can never type and can never be asked.
const DISPATCHER: &str = "8f7be9a6-3333-4bbb-8ccc-000000000003";
const INTERVENER: &str = "nicholas";
const STRANGER: &str = "someone-else";

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

fn meta(sid: &str, initiator: Option<&str>, interveners: &[&str]) -> SessionMeta {
    SessionMeta {
        version: 4,
        session_id: sid.to_string(),
        agent_id: AGENT_ID.to_string(),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        initiator: initiator.map(str::to_string),
        interveners: interveners.iter().map(|s| s.to_string()).collect(),
        // A created execution session is `Private` (ADR-076 §决策 4), which is
        // what makes the stranger's 404 the same answer as "it does not
        // exist" rather than a distinguishable "you may not".
        visibility: Some(SessionVisibility::Private),
        title: Some(sid.to_string()),
        workspace_id: None,
        model: Some("test-model".into()),
        provider: Some("test-provider".into()),
        account_id: None,
        reasoning_effort: None,
        temperature: None,
        context_window: None,
        todos: None,
        tool_policy: None,
        message_count: 3,
        last_active_at: "2026-01-01T00:10:00.000Z".to_string(),
        tokens: None,
        llm_call_counter: None,
        perf: None,
        model_ratio: None,
        last_compaction_offset: None,
        corrupted: false,
        chat_session_id: None,
        model_usage: None,
        aware_users: Vec::new(),
    }
}

async fn boot(work_dir: PathBuf) -> u16 {
    std::fs::create_dir_all(work_dir.join("memory")).unwrap();
    std::fs::create_dir_all(work_dir.join("conversations")).unwrap();
    let db = SqliteStore::open_dim_agnostic(work_dir.join("memory").join("private.sqlite")).unwrap();
    let store = SqliteSessionMetaStore::new(Arc::new(db));
    // The shape `create_intent_session` writes for an agent-to-agent intent:
    // an agent initiator plus the Gateway-seeded intervention roster.
    store.upsert(&meta("s-exec", Some(DISPATCHER), &[INTERVENER])).unwrap();
    store.upsert(&meta("s-exec-locked", Some(DISPATCHER), &[])).unwrap();

    let (dispatch_tx, dispatch_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, acowork_runtime::agent::inbound::InboundMessage)>();
    // The receiver half is dropped deliberately: nothing in this file dispatches
    // a turn, and a kept-alive receiver would make a queued action look accepted.
    drop(dispatch_rx);

    let snapshots: SharedSessionSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
    let session_metadata: Arc<
        tokio::sync::Mutex<Option<Arc<dyn acowork_runtime::usecases::session_metadata::SessionMetadataService>>>,
    > = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(RuntimeSessionMetadataService::new(
        work_dir.clone(),
        Arc::new(NoopTokens),
        Arc::new(NoopChatUsage),
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
    server.port
}

async fn list_row(port: u16, user: &str, sid: &str) -> Option<serde_json::Value> {
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/sessions"))
        .header("x-user-id", user)
        .send()
        .await
        .expect("list request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "list 200");
    let v: serde_json::Value = resp.json().await.unwrap();
    v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == sid)
        .cloned()
}

fn field_names(row: &serde_json::Value) -> Vec<String> {
    row.get("config_fields")
        .and_then(|f| f.as_array())
        .map(|a| a.iter().filter_map(|f| f.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

async fn put_config(port: u16, sid: &str, user: &str, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .put(format!("http://127.0.0.1:{port}/sessions/{sid}/config"))
        .header("x-user-id", user)
        .json(&body)
        .send()
        .await
        .expect("config request")
}

/// The seeded roster is what makes the session writable, and the listing says
/// so: `intervener`, not `read_only`, and the field list the server will honour.
#[tokio::test]
async fn the_intervention_tier_is_advertised_on_the_session_row() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    let row = list_row(port, INTERVENER, "s-exec").await.expect("intervener sees the run");
    assert_eq!(row["authority"], "intervener");
    assert_eq!(row["can_write"], true, "the composer must be enabled, not read-only");
    let fields = field_names(&row);
    for expected in ["model", "provider", "account_id", "reasoning_effort"] {
        assert!(
            fields.iter().any(|f| f == expected),
            "{expected} must be advertised as writable by an intervener: {fields:?}"
        );
    }
    for refused in ["workspace_id", "tool_policy", "delete", "visibility"] {
        assert!(
            !fields.iter().any(|f| f == refused),
            "{refused} re-premises the task and must not be offered: {fields:?}"
        );
    }
}

/// The tier is above `read_only` and below `owner`: an intervener may fix *how*
/// the run executes and may not rewrite *what* it was asked to do. The refusal
/// names the field so the surface can explain itself.
#[tokio::test]
async fn an_intervener_cannot_re_premise_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    let resp = put_config(
        port,
        "s-exec",
        INTERVENER,
        serde_json::json!({ "workspace_id": "some-other-workspace" }),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN, "workspace is the owner's");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["field"], "workspace_id");
    assert_eq!(body["authority"], "intervener");
    assert!(
        body["allowed"].as_array().unwrap().iter().any(|f| f == "model"),
        "the refusal must say what IS allowed: {body}"
    );
}

/// The point of the whole tier: changing the model is allowed. Asserted as
/// "not refused by the table" rather than 200 — this harness has no
/// session-config service, so the handler stops one step later with 503. A 403
/// here would mean the table is still locking the human out.
#[tokio::test]
async fn an_intervener_may_re_point_the_run_at_another_model() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    for body in [
        serde_json::json!({ "model": "another-model" }),
        serde_json::json!({ "provider": "another-provider", "model": "m" }),
        serde_json::json!({ "account_id": "acct-2" }),
        serde_json::json!({ "reasoning_effort": "low" }),
    ] {
        let resp = put_config(port, "s-exec", INTERVENER, body.clone()).await;
        assert_ne!(
            resp.status(),
            reqwest::StatusCode::FORBIDDEN,
            "an intervener must be able to change {body}"
        );
    }
}

/// Erasing the record and re-sharing it stay owner actions — and the
/// re-share case is the one that would otherwise bite: every agent-to-agent
/// session has an initiator (the dispatching agent), so a rule keyed on "has an
/// owner" would let an intervener hide the very run it was added to rescue.
#[tokio::test]
async fn an_intervener_cannot_delete_or_reshare_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    let resp = reqwest::Client::new()
        .delete(format!("http://127.0.0.1:{port}/sessions/s-exec"))
        .header("x-user-id", INTERVENER)
        .send()
        .await
        .expect("delete request");
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    let resp = reqwest::Client::new()
        .put(format!("http://127.0.0.1:{port}/sessions/s-exec/visibility"))
        .header("x-user-id", INTERVENER)
        .json(&serde_json::json!({ "visibility": "public" }))
        .send()
        .await
        .expect("visibility request");
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

/// A session with no intervention roster keeps the old behaviour: the agent
/// that dispatched the work is the only party that could ever write it, and no
/// human is offered a control that would 404.
#[tokio::test]
async fn without_a_roster_the_run_stays_an_agents_business() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    assert!(
        list_row(port, INTERVENER, "s-exec-locked").await.is_none(),
        "a Private session with no roster is not even listed for a human"
    );
    let resp = put_config(
        port,
        "s-exec-locked",
        INTERVENER,
        serde_json::json!({ "model": "another-model" }),
    )
    .await;
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "the write gate answers 404, not 403: the session must not be confirmed to exist"
    );
}

/// The stranger case: read is a separate axis (ADR-076 §决策 4) and the roster
/// does not widen it.
#[tokio::test]
async fn the_roster_does_not_widen_read_access() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    assert!(list_row(port, STRANGER, "s-exec").await.is_none());
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/sessions/s-exec/messages"))
        .header("x-user-id", STRANGER)
        .send()
        .await
        .expect("messages request");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

/// Every button a stuck run can offer must be pressable by the human who was
/// added to rescue it — otherwise the roster only moves the dead end one step
/// closer. This is the *action* axis, which is a different question from the
/// config axis: `Stop`, `Continue`, `Retry` and the chat write all go through
/// `authorize_write`, so one tier decision answers for all of them.
///
/// The harness wires no session manager and drops the dispatch receiver, so
/// what an authorized action returns here is 503 ("not ready" / "channel
/// closed") — the handler deliberately reports a failed *dispatch* rather than
/// a fake 202. The assertion is therefore "the tier let it through": a refusal
/// would be a 404 (invisible) or a 403 (wrong tier), and neither is acceptable
/// for a human the session was handed to.
#[tokio::test]
async fn every_control_the_run_can_offer_is_pressable_by_the_intervener() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;
    let client = reqwest::Client::new();

    for action in ["stop", "continue", "retry"] {
        let resp = client
            .post(format!("http://127.0.0.1:{port}/sessions/s-exec/{action}"))
            .header("x-user-id", INTERVENER)
            .json(&serde_json::json!({ "reason": "user_requested" }))
            .send()
            .await
            .expect("action request");
        assert_ne!(
            resp.status(),
            reqwest::StatusCode::FORBIDDEN,
            "`{action}` must not refuse the intervener by tier"
        );
        assert_ne!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "`{action}` must not hide the session from the intervener"
        );
        assert_ne!(resp.status(), reqwest::StatusCode::OK);
    }

    // Same action, no roster: the tier is what grants it, not the HTTP surface.
    let resp = client
        .post(format!("http://127.0.0.1:{port}/sessions/s-exec-locked/retry"))
        .header("x-user-id", INTERVENER)
        .send()
        .await
        .expect("locked retry request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "without a roster a human cannot even press Retry"
    );
}

/// The same for a stranger: the roster is personal, not "any authenticated
/// human may rescue any execution session".
#[tokio::test]
async fn a_stranger_cannot_press_the_controls_either() {
    let dir = tempfile::tempdir().unwrap();
    let port = boot(dir.path().to_path_buf()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/sessions/s-exec/retry"))
        .header("x-user-id", STRANGER)
        .send()
        .await
        .expect("retry request");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}
