//! G4 regression: PM's `doc_refs` validation must reach the **real** doc
//! route (plan §7 G4, design 32 §3).
//!
//! ## The bug this test exists for
//!
//! `pm_submit_task` validates its `doc_refs` by asking the doc service
//! whether each ref resolves. PM built that URL as
//! `/api/doc/api/refs/validate`, but the doc service registers its REST
//! routes **without** an `/api` prefix (`/refs/validate`), and the Gateway's
//! doc proxy strips only its own `/api/doc` public prefix — so the request
//! arrived at the doc service as `/api/refs/validate` and 404'd. Every
//! submission carrying `doc_refs` failed.
//!
//! ## Why the old tests stayed green
//!
//! The PM test stub mounted **the same wrong path the implementation used**,
//! so the mock and the code agreed with each other and the suite never
//! noticed. A stub that answers "any path" is worse: it cannot fail at all.
//!
//! ## What this test does instead
//!
//! It starts the **real `acowork-doc` service** (its own router, its own
//! route table) behind a proxy that mimics the Gateway's doc proxy: the doc
//! service's router is mounted under the Gateway's `/api/doc` public
//! prefix, so the prefix is stripped on the way in and the remainder
//! reaches the doc service verbatim — exactly what
//! `acowork-gateway`'s `doc_proxy` does. PM then talks to that proxy, so
//! the path PM builds is checked against the path the doc service actually
//! registered: no shared string, no mock, nothing to drift.
//!
//! The test therefore goes red the moment PM's URL loses the `/api/doc`
//! prefix or regains a stray `/api` segment: the real doc router answers
//! 404 and the submission is refused.

use std::sync::Arc;

use acowork_doc::config::DocConfig;
use acowork_doc::server::DocService;
use acowork_pm::store::tree::TreePmStore;
use acowork_pm::types::TaskId;
use acowork_pm::{AgentDirectory, PmConfig, PmStore};
use axum::Router;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::util::ServiceExt;

/// The Gateway's public prefix for the doc service. The doc proxy strips
/// exactly this and forwards the rest to the doc process unchanged.
const GATEWAY_DOC_PREFIX: &str = "/api/doc";

/// The test's agent instance id (ADR-073 — `assignee` is a UUID instance id).
const AGENT: &str = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";

/// Whitelist agent directory: the test's own instance id is the only
/// assignable agent.
struct OneAgent;

#[async_trait::async_trait]
impl AgentDirectory for OneAgent {
    async fn agent_exists(&self, agent_id: &str) -> bool {
        agent_id == AGENT
    }
}

/// A doc service behind a Gateway-shaped proxy: the doc's own router is
/// mounted at the Gateway's public prefix, so the prefix is stripped on the
/// way in — the same transformation `acowork-gateway`'s `doc_proxy`
/// performs.
async fn spawn_doc_behind_gateway_prefix() -> (String, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let config = DocConfig {
        data_dir: data.path().to_path_buf(),
        ..DocConfig::default()
    };
    let svc = Arc::new(DocService::new(config).await.expect("doc service starts"));
    let app = Router::new().nest(GATEWAY_DOC_PREFIX, svc.router());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "doc server exited");
        }
    });
    (format!("http://{addr}"), data)
}

/// PM's MCP router wired to the doc proxy base URL, plus the store handle
/// (the human-side approval the MCP surface deliberately has no tool for).
async fn pm_mcp(gateway_base: String) -> (Router, Arc<TreePmStore>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = PmConfig {
        data_dir: tmp.path().to_path_buf(),
        index_rebuild_on_start: false,
        ..Default::default()
    };
    let store = Arc::new(TreePmStore::new(cfg).await.unwrap());
    let router = acowork_pm::mcp::mcp_router(
        store.clone(),
        Arc::new(OneAgent) as Arc<dyn AgentDirectory>,
        Some((gateway_base, None)),
    );
    (router, store, tmp)
}

/// One JSON-RPC `tools/call` against the PM MCP router.
async fn call(router: &Router, id: u64, name: &str, args: Value) -> Value {
    let payload = json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("x-mcp-actor", AGENT)
        .body(axum::body::Body::from(payload.to_string()))
        .unwrap();
    let resp = router
        .clone()
        .oneshot(req)
        .await
        .expect("router responds");
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("JSON-RPC response")
}

/// The `text` payload of a successful tool result (the PM tools return
/// their value as a JSON string in `result.content[0].text`).
fn tool_text(v: &Value) -> Value {
    serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// Create a document in the real doc service and return its `doc_id`.
///
/// Goes through the doc service's own REST API over the same prefix the
/// PM client uses, so the document is a real library artifact with a real
/// canonical id.
async fn seed_doc(doc_base: &str) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{doc_base}{GATEWAY_DOC_PREFIX}/docs"))
        .json(&json!({
            "parent_dir_id": "root",
            "title": "G4 交付物",
            "content": "# 交付物",
        }))
        .send()
        .await
        .expect("doc create");
    assert_eq!(resp.status(), 201, "doc create must succeed");
    let body: Value = resp.json().await.unwrap();
    body["doc_id"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn pm_doc_refs_validation_reaches_the_real_doc_route() {
    let (doc_base, _doc_data) = spawn_doc_behind_gateway_prefix().await;
    let (router, store, _pm_data) = pm_mcp(doc_base.clone()).await;

    // A real document in the real doc service — the deliverable the
    // submission will name.
    let doc_id = seed_doc(&doc_base).await;

    // ── The setup ──────────────────────────────────────────────────
    let project = call(
        &router,
        1,
        "pm_create_project",
        json!({ "title": "G4 route e2e" }),
    )
    .await;
    assert!(project["error"].is_null(), "create project: {project}");
    let pid = tool_text(&project)["id"].as_str().unwrap().to_string();

    let created = call(
        &router,
        2,
        "pm_create_task",
        json!({
            "project_id": pid,
            "title": "G4 route e2e task",
            "assignee": AGENT,
        }),
    )
    .await;
    assert!(created["error"].is_null(), "create task: {created}");
    let tid = tool_text(&created)["id"].as_str().unwrap().to_string();

    // An agent-created task waits for human approval before it may start.
    store
        .review_task(&TaskId(tid.clone()), true, "human", None)
        .await
        .unwrap();

    let claimed = call(&router, 3, "pm_claim_task", json!({ "task_id": tid })).await;
    assert!(claimed["error"].is_null(), "claim task: {claimed}");

    // ── The regression itself ──────────────────────────────────────
    // A submission carrying `doc_refs` must be validated against the doc
    // service's REAL route. Before the fix PM called
    // `/api/doc/api/refs/validate`, which the doc router does not
    // register → 404 → the submission was refused with
    // `doc validation rejected: 404`.
    let submitted = call(
        &router,
        4,
        "pm_submit_task",
        json!({
            "task_id": tid,
            "text": "delivered",
            "doc_refs": [doc_id],
        }),
    )
    .await;
    assert!(
        submitted["error"].is_null(),
        "pm_submit_task with doc_refs must succeed against the real doc route: {submitted}"
    );

    // The recorded ref is the **canonical doc id** the doc service
    // resolved — not the submitted path. The desktop panel links against
    // the id, so a stored path would rot on the next rename.
    let refs = tool_text(&submitted)["result"]["doc_refs"]
        .as_array()
        .expect("doc_refs recorded")
        .clone();
    assert_eq!(refs, vec![json!(doc_id)], "canonical doc id recorded");

    // Read back through `pm_get_task`: the submission's `result.doc_refs`
    // carries the canonical id too.
    let read_back = call(&router, 5, "pm_get_task", json!({ "task_id": tid })).await;
    let chk = tool_text(&read_back);
    assert_eq!(chk["status"], "submitted");
    assert_eq!(
        chk["result"]["doc_refs"][0], doc_id,
        "the canonical id survives the round trip"
    );
}

/// A ref that does not resolve must still be refused — the route being
/// reachable must not turn the validation into a rubber stamp. The doc
/// service answers `not_found` for a missing document, and PM refuses the
/// submission on that answer.
#[tokio::test(flavor = "multi_thread")]
async fn an_unresolvable_ref_is_still_refused() {
    let (doc_base, _doc_data) = spawn_doc_behind_gateway_prefix().await;
    let (router, store, _pm_data) = pm_mcp(doc_base).await;

    let project = call(
        &router,
        1,
        "pm_create_project",
        json!({ "title": "G4 route e2e (missing ref)" }),
    )
    .await;
    let pid = tool_text(&project)["id"].as_str().unwrap().to_string();
    let created = call(
        &router,
        2,
        "pm_create_task",
        json!({
            "project_id": pid,
            "title": "G4 route e2e task",
            "assignee": AGENT,
        }),
    )
    .await;
    let tid = tool_text(&created)["id"].as_str().unwrap().to_string();
    store
        .review_task(&TaskId(tid.clone()), true, "human", None)
        .await
        .unwrap();
    let claimed = call(&router, 3, "pm_claim_task", json!({ "task_id": tid })).await;
    assert!(claimed["error"].is_null(), "claim task: {claimed}");

    let submitted = call(
        &router,
        4,
        "pm_submit_task",
        json!({
            "task_id": tid,
            "text": "delivered",
            "doc_refs": ["研发/不存在的文档.md"],
        }),
    )
    .await;
    // The doc service answered (it was reached!) — and its answer was
    // "not found", so PM refuses the submission.
    let err = submitted["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err.contains("doc_refs_unresolved"),
        "a missing document must refuse the submission, not be recorded: {submitted}"
    );
    assert!(
        !err.contains("404"),
        "the refusal must come from the doc service's answer, not from a 404: {err}"
    );
}
