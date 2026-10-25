//! End-to-end MCP integration tests for the memory service (ADR-101, P1c).
//!
//! Boots the real service binary code path (`MemoryService::serve`, full
//! router: `/mcp` + `/health`) and drives it exactly like the Gateway does:
//! JSON-RPC over HTTP with the caller identity on `X-MCP-Actor`.
//!
//! What this locks down:
//! - the tool contract text an Agent reads (`Stored episode: …`, one
//!   `- [label] (score=…) …` line per hit);
//! - instance namespacing — two actors never see each other's store, and the
//!   data really is on disk (a second service over the same `data_dir` reads
//!   it back);
//! - no anonymous tier: a missing actor is the JSON-RPC permission code.

use std::net::SocketAddr;
use std::sync::Arc;

use acowork_memory::{MemoryService, MemoryServiceConfig};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

/// ADR-073: stands in for the runtime instance identity injected over
/// `X-MCP-Actor` by the Gateway catalog. UUID-shaped on purpose — the memory
/// service validates the id before turning it into a directory name.
const INSTANCE_A: &str = "11111111-1111-4111-8111-111111111111";
const INSTANCE_B: &str = "22222222-2222-4222-8222-222222222222";

struct TestServer {
    base: String,
    client: reqwest::Client,
    _data: TempDir,
}

impl TestServer {
    /// Boot the service on an ephemeral port inside `data` (kept alive by the
    /// returned handle so the instance directory is not deleted early).
    async fn spawn_in(data: TempDir) -> TestServer {
        let config = MemoryServiceConfig {
            data_dir: data.path().to_path_buf(),
            // These tests pin the MCP contract (identity, tool shapes,
            // isolation). Embedding behaviour is the REST harness's business, so
            // the backend is switched off here rather than left to reach whatever
            // happens to listen on the default sidecar port.
            embedding: acowork_memory::service::config::EmbeddingSection {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let svc = Arc::new(MemoryService::new(config).await.unwrap());
        let addr: SocketAddr = svc
            .clone()
            .serve(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        TestServer {
            base: format!("http://{addr}"),
            client: reqwest::Client::new(),
            _data: data,
        }
    }

    async fn spawn() -> TestServer {
        Self::spawn_in(TempDir::new().unwrap()).await
    }

    /// Raw JSON-RPC POST to `/mcp`, with the optional actor header.
    async fn rpc(&self, method: &str, params: Value, actor: Option<&str>) -> Value {
        let payload = json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params
        });
        let mut req = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("content-type", "application/json");
        if let Some(a) = actor {
            req = req.header("x-mcp-actor", a);
        }
        let resp = req.json(&payload).send().await.unwrap();
        // JSON-RPC errors ride in a 200 body — the client reads `error`.
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "MCP endpoint always answers 200"
        );
        resp.json::<Value>().await.unwrap()
    }

    /// `tools/call` unwrapped to the content text; panics on a JSON-RPC error.
    async fn tool(&self, actor: Option<&str>, name: &str, args: Value) -> String {
        let resp = self
            .rpc(
                "tools/call",
                json!({ "name": name, "arguments": args }),
                actor,
            )
            .await;
        assert!(resp.get("error").is_none(), "tool {name} failed: {resp}");
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// `tools/call` expected to fail: returns `(code, message)`.
    async fn tool_err(&self, actor: Option<&str>, name: &str, args: Value) -> (i64, String) {
        let resp = self
            .rpc(
                "tools/call",
                json!({ "name": name, "arguments": args }),
                actor,
            )
            .await;
        let code = resp["error"]["code"].as_i64().expect("error code");
        let msg = resp["error"]["message"].as_str().unwrap().to_string();
        (code, msg)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_and_tools_list_over_http() {
    let server = TestServer::spawn().await;

    let init = server.rpc("initialize", json!({}), None).await;
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "acowork-memory");

    let list = server.rpc("tools/list", json!({}), None).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["memory_store", "memory_recall"]);

    // The health route is merged alongside `/mcp` by the same router, so one
    // bound port serves both surfaces.
    let resp = server
        .client
        .get(format!("{}/health", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// The Agent-visible roundtrip: write with the exact builtin confirmation
/// text, read back with the exact builtin result-line format.
#[tokio::test(flavor = "multi_thread")]
async fn store_then_recall_roundtrip() {
    let server = TestServer::spawn().await;

    let stored = server
        .tool(
            Some(INSTANCE_A),
            "memory_store",
            json!({
                "content": "User lives in Beijing",
                "category": "fact",
                "confidence": 0.9,
                "keywords": ["beijing", "location"],
                "normalized": "User lives in Beijing.",
            }),
        )
        .await;
    assert_eq!(
        stored,
        "Stored episode: \"User lives in Beijing\" (confidence: 0.90, subtype: Fact)"
    );

    let found = server
        .tool(
            Some(INSTANCE_A),
            "memory_recall",
            json!({ "query": "Beijing" }),
        )
        .await;
    assert!(found.contains("User lives in Beijing"), "{found}");
    assert!(found.starts_with("- ["), "{found}");
    // Scores are formatted with two decimals, as the builtin did.
    assert!(found.contains("(score="), "{found}");
}

/// One process, many Agents: the actor header is the namespace key, so a write
/// under instance A is invisible to instance B (ADR-101 decision 3).
#[tokio::test(flavor = "multi_thread")]
async fn instance_namespaces_are_isolated() {
    let server = TestServer::spawn().await;

    server
        .tool(
            Some(INSTANCE_A),
            "memory_store",
            json!({ "content": "private to instance A", "category": "fact" }),
        )
        .await;

    let a = server
        .tool(
            Some(INSTANCE_A),
            "memory_recall",
            json!({ "query": "private" }),
        )
        .await;
    assert!(a.contains("private to instance A"), "{a}");

    let b = server
        .tool(
            Some(INSTANCE_B),
            "memory_recall",
            json!({ "query": "private" }),
        )
        .await;
    assert_eq!(b, "No relevant memories found.");
}

/// The store is per-instance on disk, so a restart (a second service over the
/// same `data_dir`) reads back what the first one wrote.
#[tokio::test(flavor = "multi_thread")]
async fn memories_survive_a_service_restart() {
    let data = TempDir::new().unwrap();
    let first = TestServer::spawn_in(data).await;
    first
        .tool(
            Some(INSTANCE_A),
            "memory_store",
            json!({ "content": "durable fact", "category": "fact" }),
        )
        .await;
    let db_path = first
        ._data
        .path()
        .join("instances")
        .join(INSTANCE_A)
        .join("memory.db");
    assert!(db_path.exists(), "one SQLite file per instance");

    // A second service over the same directory (the Gateway respawn case).
    let second = TestServer::spawn_in(first._data).await;
    let found = second
        .tool(
            Some(INSTANCE_A),
            "memory_recall",
            json!({ "query": "durable" }),
        )
        .await;
    assert!(found.contains("durable fact"), "{found}");
}

/// No anonymous tier: both tools answer the permission code, and the message
/// keeps the machine-readable prefix the client buckets on.
#[tokio::test(flavor = "multi_thread")]
async fn anonymous_calls_are_permission_errors() {
    let server = TestServer::spawn().await;

    let (code, msg) = server
        .tool_err(
            None,
            "memory_store",
            json!({ "content": "x", "category": "fact" }),
        )
        .await;
    assert_eq!(code, -32002, "{msg}");
    assert!(msg.starts_with("not_authorized: "), "{msg}");

    let (code, msg) = server
        .tool_err(None, "memory_recall", json!({ "query": "x" }))
        .await;
    assert_eq!(code, -32002, "{msg}");
}

/// An actor that cannot be a directory name is rejected as invalid params —
/// it must never be resolved against the filesystem.
#[tokio::test(flavor = "multi_thread")]
async fn path_traversal_actor_is_rejected() {
    let server = TestServer::spawn().await;

    let (code, msg) = server
        .tool_err(
            Some("../../escape"),
            "memory_store",
            json!({ "content": "x", "category": "fact" }),
        )
        .await;
    assert_eq!(code, -32602, "{msg}");
    assert!(msg.starts_with("invalid_instance_id: "), "{msg}");

    let data_dir = server._data.path();
    assert!(
        !data_dir.parent().unwrap().join("escape").exists(),
        "must not create a store outside the instance root"
    );
    assert_eq!(
        data_dir.join("instances").read_dir().unwrap().count(),
        0,
        "a rejected actor must not open anything"
    );
}

/// Validation failures keep the builtin's wording inside the JSON-RPC message.
#[tokio::test(flavor = "multi_thread")]
async fn validation_messages_are_preserved() {
    let server = TestServer::spawn().await;

    let cases = [
        (
            "memory_store",
            json!({ "category": "fact" }),
            "Missing required parameter 'content'",
        ),
        (
            "memory_store",
            json!({ "content": "x" }),
            "Missing required parameter 'category'",
        ),
        (
            "memory_store",
            json!({ "content": "x", "category": "gossip" }),
            "Invalid category 'gossip'",
        ),
        ("memory_recall", json!({}), "Provide at least 'query'"),
        (
            "memory_recall",
            json!({ "query": "x", "since": "last tuesday" }),
            "Invalid 'since' date",
        ),
    ];
    for (name, args, expected) in cases {
        let args_label = args.to_string();
        let (code, msg) = server.tool_err(Some(INSTANCE_A), name, args).await;
        assert_eq!(code, -32602, "{name} {args_label} -> {msg}");
        assert!(
            msg.contains(expected),
            "{name} expected {expected:?} in {msg:?}"
        );
    }
}

