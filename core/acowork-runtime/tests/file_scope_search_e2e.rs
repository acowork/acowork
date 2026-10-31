//! End-to-end test for the **file scope** of the global search
//! (ADR-081 D2: "ripgrep full text **+** filename fuzzy").
//!
//! ## Why this file exists
//!
//! The reported bug: searching the global search by **file name** returned
//! nothing, while the workspace file search (a different endpoint) found the
//! same file. The cause was that the file tab only ever called
//! `/workspaces/search`, which matched file *bodies* — a file whose name
//! matches but whose body does not was unreachable through the global search.
//!
//! This file pins the **wire contract** of that fix: the Desktop renders
//! straight from these bytes, so the shape of a file-name hit (a `kind`
//! discriminator, and *no* line/column) has to be asserted here rather than
//! left to a unit test that could drift from the JSON.
//!
//! It talks to a real `RuntimeHttpServer` over real HTTP — the same path the
//! Gateway reverse-proxies — so the assertion covers the serde attributes on
//! `SearchMatchDto`, not just the Rust types.
//!
//! ## Scope discipline
//!
//! Only the file scope is exercised. The conversation / memory / git scopes
//! are covered by `global_search_e2e.rs`; this file stays out of their way so
//! a change to the file scope cannot break an unrelated suite.

use std::collections::HashMap;
use std::sync::Arc;

use acowork_runtime::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use acowork_runtime::http::server::RuntimeHttpServer;
use acowork_runtime::usecases::workspace_query_impl::RuntimeWorkspaceQueryService;
use acowork_runtime::usecases::workspace_query::WorkspaceQueryService;

const AGENT_ID: &str = "com.test.filescope";
const INSTANCE_ID: &str = "00000000-0000-4000-8000-0000000000ff";

/// A booted server over a workspace tree with one named file whose body does
/// not mention the query, plus one body that does.
struct Harness {
    port: u16,
    _server: RuntimeHttpServer,
    /// Held so the workspace outlives every assertion.
    _dir: tempfile::TempDir,
}

async fn boot() -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(work_dir.join("config")).unwrap();
    std::fs::create_dir_all(work_dir.join("src")).unwrap();
    std::fs::create_dir_all(work_dir.join("docs")).unwrap();

    // The file whose NAME matches the query, but whose body does not. This is
    // the case the bug made unreachable.
    std::fs::write(
        work_dir.join("src/context_compression.rs"),
        "fn compress() { /* no query term here */ }\n",
    )
    .unwrap();
    // A file whose BODY matches the query, but whose name does not.
    std::fs::write(
        work_dir.join("docs/readme.md"),
        "a needle inside the haystack\n",
    )
    .unwrap();

    let snapshots: SharedSessionSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
    let workspace_query: Arc<tokio::sync::Mutex<Option<Arc<dyn WorkspaceQueryService>>>> =
        Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
            RuntimeWorkspaceQueryService::new(work_dir.clone(), AGENT_ID.to_string()),
        ))));

    let server = RuntimeHttpServer::start_with_bind_port(
        0,
        work_dir.clone(),
        work_dir.clone(),
        AGENT_ID.to_string(),
        INSTANCE_ID.to_string(),
        snapshots,
        latest,
        Arc::new(tokio::sync::Mutex::new(None)), // dispatch_tx
        Arc::new(std::sync::RwLock::new(Vec::new())), // degraded_reasons
        Arc::new(tokio::sync::Mutex::new(None)), // mqtt_client
        Arc::new(tokio::sync::Mutex::new(None)), // session_metadata
        Arc::new(std::sync::RwLock::new(None)),  // conversation_index
        workspace_query,
        Arc::new(tokio::sync::Mutex::new(None)), // workspace_mutation
        Arc::new(tokio::sync::Mutex::new(None)), // git_query
        Arc::new(tokio::sync::Mutex::new(None)), // agent_tools
        Arc::new(tokio::sync::Mutex::new(None)), // agent_config
        Arc::new(tokio::sync::Mutex::new(None)), // attachment
        Arc::new(tokio::sync::Mutex::new(None)), // session_config
        Arc::new(std::sync::RwLock::new(None)),  // rag_provider
        Arc::new(tokio::sync::Mutex::new(None)), // debug_service
        Arc::new(std::sync::RwLock::new(
            acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        )),
        Arc::new(tokio::sync::RwLock::new(None)), // session_manager_slot
        Arc::new(std::sync::RwLock::new(None)), // agent_core
    )
    .await
    .expect("runtime http server should start");

    Harness {
        port: server.port,
        _server: server,
        _dir: dir,
    }
}

/// `GET /workspaces/search` as raw JSON — the wire contract is what the
/// Desktop renders, so the body is not decoded into a convenient struct first.
async fn search(port: u16, query: &str) -> (u16, serde_json::Value) {
    let url = format!(
        "http://127.0.0.1:{port}/workspaces/search?q={}&max_results=50",
        urlencoding(query)
    );
    let resp = reqwest::get(url).await.expect("request should reach server");
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.expect("json body");
    (status, body)
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "%20".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn matches(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body.get("matches")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// The regression: a query that matches a file NAME but no file BODY must
/// return that file.
#[tokio::test]
async fn file_name_query_finds_the_file_over_http() {
    let h = boot().await;
    let (status, body) = search(h.port, "context_compression").await;

    assert_eq!(status, 200, "the endpoint must answer: {body}");
    let hits = matches(&body);
    assert!(
        hits.iter()
            .any(|m| m["file"] == "src/context_compression.rs"),
        "the named file must be returned: {body}"
    );
}

/// The name hit must be distinguishable from a content hit on the wire, and
/// must carry NO line/column — a file name has no line, and a `0` sentinel
/// would make the Desktop jump to line 0.
#[tokio::test]
async fn file_name_hit_carries_kind_and_no_line() {
    let h = boot().await;
    let (_, body) = search(h.port, "context_compression").await;

    let name_hit = matches(&body)
        .into_iter()
        .find(|m| m["file"] == "src/context_compression.rs")
        .expect("the named file must be returned");

    assert_eq!(name_hit["kind"], "name", "the hit must say it is a name hit");
    assert!(
        name_hit.get("line").is_none(),
        "a name hit has no line: {name_hit}"
    );
    assert!(
        name_hit.get("column").is_none(),
        "a name hit has no column: {name_hit}"
    );
    assert_eq!(name_hit["text"], "context_compression.rs");
}

/// The content half must be untouched: same shape as before the fix.
#[tokio::test]
async fn content_hit_still_carries_kind_line_and_column() {
    let h = boot().await;
    let (_, body) = search(h.port, "needle").await;

    let content_hit = matches(&body)
        .into_iter()
        .find(|m| m["file"] == "docs/readme.md")
        .expect("the body match must be returned");

    assert_eq!(content_hit["kind"], "content");
    assert_eq!(content_hit["line"], 1);
    assert!(content_hit["column"].is_number());
}

/// `match_file_names=false` restores the pre-fix content-only behaviour.
#[tokio::test]
async fn match_file_names_false_restores_content_only_results() {
    let h = boot().await;
    let url = format!(
        "http://127.0.0.1:{}/workspaces/search?q={}&match_file_names=false",
        h.port,
        urlencoding("context_compression")
    );
    let resp = reqwest::get(url).await.expect("request should reach server");
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.expect("json body");

    assert!(
        matches(&body).is_empty(),
        "with name matching off, a name-only query finds nothing: {body}"
    );
}
