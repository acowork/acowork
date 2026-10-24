//! End-to-end panel REST integration tests (ADR-100, P1d).
//!
//! Boots the real service (`MemoryService::serve`, full router) and drives the
//! `/memory/*` endpoints over HTTP the way the Gateway reverse proxy will: the
//! instance namespace arrives as `?instance_id=`, the MCP surface writes with
//! `X-MCP-Actor`.
//!
//! Locked down here:
//! - the **wire contract** the Desktop memory panel parses — field names, the
//!   `sub_type` omission, the bare-status errors, `{"error": …}` bodies;
//! - MCP write → REST read round trip (the two surfaces must be one store);
//! - instance isolation, and the 400 for a missing / unusable `instance_id`;
//! - the rebuild lifecycle (202 → 409 while running → terminal progress) and
//!   the 404 for an instance that never rebuilt;
//! - the bootstrap import (write entry D): declared manifest facts become
//!   Autobiographical nodes, exactly once per instance.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use acowork_memory::service::config::EmbeddingSection;
use acowork_memory::{DEFAULT_EMBEDDING_DIM, MemoryService, MemoryServiceConfig};
use axum::Json;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

const INSTANCE_A: &str = "11111111-1111-4111-8111-111111111111";
const INSTANCE_B: &str = "22222222-2222-4222-8222-222222222222";

struct TestServer {
    base: String,
    client: reqwest::Client,
    _data: TempDir,
}

impl TestServer {
    async fn spawn() -> TestServer {
        let data = TempDir::new().unwrap();
        // Since P1e the service installs an embedding backend at startup, so the
        // harness points `[embedding].base_url` at a stub rather than leaving the
        // default sidecar port — that port may be occupied by a developer's own
        // running acowork-embed, and the tests would then observe the machine
        // instead of the service.
        let endpoint = stub_embed_server(false).await;
        let config = MemoryServiceConfig {
            data_dir: data.path().to_path_buf(),
            embedding: EmbeddingSection {
                base_url: endpoint,
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

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let resp = self.client.get(self.url(path)).send().await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn post(&self, path: &str, payload: Value) -> (StatusCode, Value) {
        let resp = self
            .client
            .post(self.url(path))
            .json(&payload)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn put(&self, path: &str, payload: Value) -> (StatusCode, Value) {
        let resp = self
            .client
            .put(self.url(path))
            .json(&payload)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn delete(&self, path: &str) -> (StatusCode, Value) {
        let resp = self.client.delete(self.url(path)).send().await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    /// Write one memory through the **MCP** surface, so a test can prove the
    /// panel endpoints read the same store the tools write.
    async fn mcp_store(&self, actor: &str, content: &str) {
        let resp = self
            .client
            .post(self.url("/mcp"))
            .header("x-mcp-actor", actor)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "memory_store",
                    "arguments": {"content": content, "category": "fact"}
                }
            }))
            .send()
            .await
            .unwrap();
        let body = resp.json::<Value>().await.unwrap();
        assert!(body.get("error").is_none(), "memory_store failed: {body}");
        assert_eq!(body["result"]["isError"], false, "{body}");
    }
}

/// JSON body when there is one, `Value::Null` for the Runtime's bare-status
/// answers (those routes really do send an empty body).
async fn body_json(resp: reqwest::Response) -> Value {
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// Stub OpenAI-compatible embedding endpoint. `slow` sleeps inside the handler
/// so a rebuild is still in flight when the test checks the 409 guard.
///
/// The reply is the full `{"data": [{"index", "embedding"}]}` shape: a bare
/// array of numbers parses as *no embedding* on the client side, and the
/// rebuild path treats an unembeddable row as a skip rather than a failure, so
/// an unrealistic stub quietly turns "nothing was re-embedded" into a passing
/// test.
async fn stub_embed_server(slow: bool) -> String {
    let handler = axum::routing::post(move |Json(body): Json<Value>| async move {
        if slow {
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        let n = body["input"].as_array().map(|a| a.len()).unwrap_or(1);
        let data: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "object": "embedding",
                    "index": i,
                    "embedding": (0..DEFAULT_EMBEDDING_DIM)
                        .map(|d| d as f64 / DEFAULT_EMBEDDING_DIM as f64)
                        .collect::<Vec<f64>>(),
                })
            })
            .collect();
        Json(json!({"data": data, "model": body["model"]}))
    });
    let app = axum::Router::new().route("/v1/embeddings", handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/v1")
}

// ── Identity ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_or_unusable_instance_id_is_a_400_on_every_route() {
    let server = TestServer::spawn().await;

    for path in [
        "/memory/nodes",
        "/memory/stats",
        "/memory/graph",
        "/memory/search?q=x",
        "/memory/rebuild-progress",
    ] {
        let (status, _) = server.get(path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path} without id");
        let (status, _) = server.get(&format!("{path}?instance_id=../escape")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path} with traversal id");
    }

    let (status, _) = server
        .post("/memory/rebuild-embeddings", json!({"dimension": 4}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The bootstrap route is on the same router and the same namespace rule
    // applies: an unusable id must not create a store.
    let (status, _) = server
        .post("/admin/bootstrap", json!({"agent_id": "a", "name": "A"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bootstrap without id");
    let (status, _) = server
        .post(
            "/admin/bootstrap?instance_id=../escape",
            json!({"agent_id": "a", "name": "A"}),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "bootstrap with traversal id"
    );
}

// ── Bootstrap import (write entrypoint D) ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_imports_manifest_facts_once_per_instance() {
    let server = TestServer::spawn().await;
    let q = format!("instance_id={INSTANCE_A}");
    let manifest = json!({
        "agent_id": "com.acowork.senior-engineer",
        "name": "Senior Engineer",
        "description": "Writes and reviews production Rust",
        "display_name": "SE",
        "role": "Engineer",
        "capabilities": {
            "code_review": { "description": "Reviews diffs for defects" }
        }
    });

    let (status, out) = server
        .post(&format!("/admin/bootstrap?{q}"), manifest.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["identity_written"], 5);
    assert_eq!(out["capability_written"], 1);
    assert_eq!(out["skipped_existing"], false);

    // The panel sees them as self-knowledge, in the bootstrapped instance only.
    let (_, stats) = server.get(&format!("/memory/stats?{q}")).await;
    assert_eq!(stats["by_type"]["Autobiographical"], 6, "{stats}");
    let (_, other) = server
        .get(&format!("/memory/stats?instance_id={INSTANCE_B}"))
        .await;
    assert_eq!(
        other["by_type"]["Autobiographical"], 0,
        "{other} — B must not have gained A's identity"
    );

    // Idempotent: an agent restart must not double the identity facts.
    let (status, again) = server
        .post(&format!("/admin/bootstrap?{q}"), manifest)
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["skipped_existing"], true, "{again}");
    let (_, stats) = server.get(&format!("/memory/stats?{q}")).await;
    assert_eq!(stats["by_type"]["Autobiographical"], 6, "{stats}");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_instance_never_reads_another() {
    let server = TestServer::spawn().await;
    server.mcp_store(INSTANCE_A, "User lives in Beijing").await;

    let (status, mine) = server
        .get(&format!("/memory/nodes?instance_id={INSTANCE_A}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(mine["total"].as_u64().unwrap() >= 1, "{mine}");

    let (_, other) = server
        .get(&format!(
            "/memory/nodes?instance_id={INSTANCE_B}&keyword=Beijing"
        ))
        .await;
    assert_eq!(other["total"], 0, "B must not see A's memory: {other}");
    assert!(other["nodes"].as_array().unwrap().is_empty());
}

// ── Node listing / detail contract ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn mcp_written_memory_is_visible_and_shaped_over_rest() {
    let server = TestServer::spawn().await;
    server
        .mcp_store(INSTANCE_A, "The deploy target is prod-ams")
        .await;

    let (status, list) = server
        .get(&format!(
            "/memory/nodes?instance_id={INSTANCE_A}&keyword=prod-ams&page=1&size=10"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    for key in ["nodes", "total", "page", "size", "model_dim"] {
        assert!(
            list.get(key).is_some(),
            "list response missing {key}: {list}"
        );
    }
    assert_eq!(list["page"], 1);
    assert_eq!(list["size"], 10);
    // The installed backend reports the width new vectors must have, which is
    // what the panel's "Rebuild Index" banner compares against `stored_dim`.
    assert_eq!(list["model_dim"], DEFAULT_EMBEDDING_DIM as u64);

    let nodes = list["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1, "{list}");
    let node = &nodes[0];
    for key in [
        "node_id",
        "node_type",
        "content",
        "confidence",
        "importance",
        "decay_score",
        "created_at",
        "last_accessed_at",
        "access_count",
        "status",
    ] {
        assert!(node.get(key).is_some(), "node missing {key}: {node}");
    }
    assert!(node["content"].as_str().unwrap().contains("prod-ams"));
    // `memory_store` always classifies (ADR-068 `category` becomes the
    // episodic `knowledge_subtype` property), so the sub-classification key is
    // present. The omission case is asserted on a Knowledge node in
    // `create_update_delete_round_trip_keeps_the_error_bodies`.
    assert_eq!(node["sub_type"], "Fact", "{node}");

    // The same row through the detail route, with the full property bag.
    let nid = node["node_id"].as_u64().unwrap();
    let (status, detail) = server
        .get(&format!("/memory/nodes/{nid}?instance_id={INSTANCE_A}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["found"], true, "{detail}");
    assert_eq!(detail["node_id"], nid);
    assert_eq!(detail["sub_type"], "Fact", "{detail}");
    assert!(detail.get("properties").is_some(), "{detail}");
    assert!(detail.get("message").is_some(), "{detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn node_id_must_be_numeric_on_read_and_delete() {
    let server = TestServer::spawn().await;
    let q = format!("instance_id={INSTANCE_A}");

    let (status, body) = server.get(&format!("/memory/nodes/abc?{q}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body was {body}");
    // Bare status code, empty body — the Runtime's `Result<Json, StatusCode>`.
    assert_eq!(body, Value::Null);

    let (status, _) = server.delete(&format!("/memory/nodes/abc?{q}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_update_delete_round_trip_keeps_the_error_bodies() {
    let server = TestServer::spawn().await;
    let q = format!("instance_id={INSTANCE_A}");

    let (status, created) = server
        .post(
            &format!("/memory/nodes?{q}"),
            json!({"label": "Knowledge", "properties": {"content": "handwritten note", "confidence": 0.4}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let nid = created["node_id"].as_u64().unwrap();
    assert_eq!(created["label"], "Knowledge");

    // Merge, not replace.
    let (status, updated) = server
        .put(
            &format!("/memory/nodes/{nid}?{q}"),
            json!({"properties": {"confidence": 0.9}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["updated"], true);
    let (_, detail) = server.get(&format!("/memory/nodes/{nid}?{q}")).await;
    assert_eq!(detail["properties"]["confidence"], 0.9);
    assert_eq!(
        detail["properties"]["content"], "handwritten note",
        "a patch must not drop untouched properties"
    );
    // A Knowledge node carries no `sub_type` property, and the projection
    // drops the key instead of answering null.
    assert_eq!(detail["node_type"], "Knowledge", "{detail}");
    assert!(
        detail.get("sub_type").is_none(),
        "sub_type must be omitted when absent: {detail}"
    );

    // Unknown node: 404 **with** a body the panel reads.
    let (status, missing) = server
        .put(
            &format!("/memory/nodes/999999?{q}"),
            json!({"properties": {"confidence": 0.1}}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    assert!(
        missing["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not found"),
        "{missing}"
    );
    assert_eq!(missing["node_id"], 999999);

    // Non-numeric id on the body-carrying route also answers with a body.
    let (status, bad) = server
        .put("/memory/nodes/abc?x=1", json!({"properties": {}}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(bad["error"], "node_id must be a non-negative integer");

    let (status, deleted) = server.delete(&format!("/memory/nodes/{nid}?{q}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["node_id"], nid);
    // The delete is not conditional on the row existing (the Runtime reported
    // `deleted: true` unconditionally), so a repeat is still a 200.
    let (status, again) = server.delete(&format!("/memory/nodes/{nid}?{q}")).await;
    assert_eq!(status, StatusCode::OK, "{again}");
}

// ── Stats / graph ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn stats_carries_the_whole_index_diagnostic_contract() {
    let server = TestServer::spawn().await;
    server.mcp_store(INSTANCE_A, "one memory").await;

    let (status, stats) = server
        .get(&format!("/memory/stats?instance_id={INSTANCE_A}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    for key in [
        "total_nodes",
        "storage_bytes",
        "by_type",
        "by_status",
        "index_health",
        "stored_dim",
        "nodes_with_embedding",
        "vectors_of_other_dim",
        "rebuild_rows_all",
        "rebuild_rows_repair",
        "model_dim",
        "schema_version",
    ] {
        assert!(stats.get(key).is_some(), "stats missing {key}: {stats}");
    }
    assert_eq!(stats["stored_dim"], DEFAULT_EMBEDDING_DIM as u64);
    assert_eq!(
        stats["model_dim"], DEFAULT_EMBEDDING_DIM as u64,
        "the startup backend is what the banner reads"
    );
    // The store path embeds what the model can embed, so a fresh memory is not
    // waiting for a rebuild.
    assert!(
        stats["nodes_with_embedding"].as_u64().unwrap() >= 1,
        "{stats}"
    );
    assert_eq!(stats["vectors_of_other_dim"], 0);
    // The rebuild promise and the store's own total come from one definition.
    assert!(stats["rebuild_rows_all"].as_u64().unwrap() >= 1);
    assert_ne!(stats["index_health"], "no_store", "a store was opened");
}

#[tokio::test(flavor = "multi_thread")]
async fn graph_is_edgeless_with_the_nine_field_node_shape() {
    let server = TestServer::spawn().await;
    server.mcp_store(INSTANCE_A, "graph node").await;

    let (status, graph) = server
        .get(&format!("/memory/graph?instance_id={INSTANCE_A}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(graph["agent_id"], INSTANCE_A);
    assert_eq!(graph["edges"].as_array().unwrap().len(), 0);
    assert_eq!(
        graph["node_count"],
        graph["nodes"].as_array().unwrap().len()
    );
    let first = &graph["nodes"][0];
    // The graph projection is deliberately narrower than the list one.
    assert!(first.get("importance").is_none(), "{first}");
    assert!(first.get("sub_type").is_none(), "{first}");
    assert!(first.get("content").is_some());
}

// ── Search ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn nodes_semantic_mode_reports_the_engine_page_shape() {
    let server = TestServer::spawn().await;
    server
        .mcp_store(INSTANCE_A, "the user prefers dark mode")
        .await;

    // Empty `q` on the semantic path is the literal zero shape, not the
    // caller's page numbers.
    let (status, empty) = server
        .get(&format!(
            "/memory/nodes?instance_id={INSTANCE_A}&mode=hybrid&page=7&size=3"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        empty,
        json!({"nodes": [], "total": 0, "page": 1, "size": 0, "model_dim": 0})
    );

    let (_, hit) = server
        .get(&format!(
            "/memory/nodes?instance_id={INSTANCE_A}&mode=hybrid&q=dark+mode&size=200"
        ))
        .await;
    assert_eq!(hit["page"], 1);
    // `size` echoes the ceiling the engine applied, not the 200 asked for.
    assert_eq!(hit["size"], 100);
    assert_eq!(hit["total"], hit["nodes"].as_array().unwrap().len() as u64);
    assert!(hit["total"].as_u64().unwrap() >= 1, "{hit}");

    // `mode=keyword` (and no mode) keeps the substring path.
    let (_, kw) = server
        .get(&format!(
            "/memory/nodes?instance_id={INSTANCE_A}&mode=keyword&q=dark&keyword=dark"
        ))
        .await;
    assert!(kw["total"].as_u64().unwrap() >= 1, "{kw}");
}

#[tokio::test(flavor = "multi_thread")]
async fn search_returns_locate_keys_plus_evidence() {
    let server = TestServer::spawn().await;
    server
        .mcp_store(INSTANCE_A, "the deploy target is prod-ams")
        .await;

    let (status, resp) = server
        .get(&format!(
            "/memory/search?instance_id={INSTANCE_A}&q=prod-ams&limit=5"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["indexing"], false);
    assert_eq!(resp["scopes"]["memory"]["status"], "ok");
    assert_eq!(
        resp["search_meta"]["memory"]["embedded"], true,
        "a live backend embeds the query text"
    );

    let hits = resp["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "{resp}");
    assert_eq!(resp["scopes"]["memory"]["count"], hits.len());
    let hit = &hits[0];
    assert_eq!(hit["scope"], "memory");
    assert_eq!(hit["title"], hit["payload"]["node_type"]);
    assert!(hit["snippet"].as_str().unwrap().contains("prod-ams"));
    // Locate keys survive the evidence merge (a missing node_id made the
    // preview panel fetch node 0).
    assert!(
        hit["payload"].get("node_id").is_some(),
        "payload lost node_id: {hit}"
    );
    assert!(hit["payload"].get("matched_by").is_some(), "{hit}");
    // Uncalibrated signals stay private.
    assert!(hit["payload"].get("cosine").is_none(), "{hit}");
    assert!(hit["payload"].get("fused_score").is_none(), "{hit}");

    // An empty query is not an error and reports no scopes at all.
    let (status, none) = server
        .get(&format!("/memory/search?instance_id={INSTANCE_A}&q="))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(none, json!({"hits": [], "scopes": {}, "indexing": false}));
}

// ── Rebuild lifecycle ─────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn rebuild_progress_is_404_until_a_run_has_been_started() {
    let server = TestServer::spawn().await;
    let (status, body) = server
        .get(&format!(
            "/memory/rebuild-progress?instance_id={INSTANCE_B}"
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body, Value::Null, "bare status, like the Runtime");
    // Polling must not create a store file for an unknown instance.
    assert!(
        !server
            ._data
            .path()
            .join("instances")
            .join(INSTANCE_B)
            .join("memory.db")
            .exists(),
        "a read-only poll opened the store"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rebuild_starts_conflicts_and_reaches_a_terminal_progress() {
    let server = TestServer::spawn().await;
    server.mcp_store(INSTANCE_A, "needs re-embedding").await;
    let endpoint = stub_embed_server(true).await;
    let q = format!("instance_id={INSTANCE_A}");
    let payload = json!({
        "endpoint": endpoint,
        "model_id": "bge-test",
        "dimension": DEFAULT_EMBEDDING_DIM,
        "scope": "all",
    });

    let (status, started) = server
        .post(&format!("/memory/rebuild-embeddings?{q}"), payload.clone())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    assert_eq!(started["status"], "started");

    // The slot is reserved before the task runs, so the very first poll already
    // sees a live run rather than a 404.
    let (status, live) = server.get(&format!("/memory/rebuild-progress?{q}")).await;
    assert_eq!(status, StatusCode::OK, "{live}");
    assert_eq!(live["done"], false);

    // A second rebuild of the same instance while one is in flight: 409.
    let (status, _) = server
        .post(&format!("/memory/rebuild-embeddings?{q}"), payload)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let mut final_progress = live;
    for _ in 0..50 {
        let (_, p) = server.get(&format!("/memory/rebuild-progress?{q}")).await;
        if p["done"] == json!(true) {
            final_progress = p;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        final_progress["done"], true,
        "rebuild never reported completion: {final_progress}"
    );
    assert_eq!(
        final_progress["error"],
        Value::Null,
        "rebuild failed: {final_progress}"
    );
    assert!(final_progress["total"].as_u64().unwrap() >= 1);
    // Every scanned row must come back embedded — a row the endpoint could not
    // serve is a skip with `errors += 1`, not a rebuild.
    assert_eq!(
        final_progress["rebuilt"], final_progress["total"],
        "the endpoint answered but nothing was re-embedded: {final_progress}"
    );
    let (_, stats) = server
        .get(&format!("/memory/stats?instance_id={INSTANCE_A}"))
        .await;
    assert_eq!(stats["index_health"], "healthy", "{stats}");

    // The other instance is untouched by this one's run.
    let (status, _) = server
        .get(&format!(
            "/memory/rebuild-progress?instance_id={INSTANCE_B}"
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
