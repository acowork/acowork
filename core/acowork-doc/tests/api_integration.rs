//! End-to-end REST integration tests over a real TCP server (D1-10).
//!
//! Each test boots the full axum router (REST + `/health`) on an ephemeral
//! port with a fresh temp data dir, then drives it with reqwest:
//!
//! - docs: create / read / update (+409) / rename / move / delete-to-trash
//! - dirs: create / tree / cross-dir move + rollback
//! - review flow: submit → approve merges (version+1) / reject keeps note /
//!   pre-empted base → 409 / double review → 409
//! - trash: restore round-trip; purge
//! - guards: invalid ids are rejected with the JSON error envelope
//! - search: keyword hits title and body

use std::net::SocketAddr;
use std::sync::Arc;

use acowork_doc::config::DocConfig;
use acowork_doc::server::DocService;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use tempfile::TempDir;

/// A live doc server on `127.0.0.1:<ephemeral>` with an isolated data dir.
struct TestServer {
    base: String,
    client: reqwest::Client,
    _data: TempDir,
}

async fn spawn() -> TestServer {
    let data = TempDir::new().unwrap();
    let config = DocConfig {
        data_dir: data.path().to_path_buf(),
        ..DocConfig::default()
    };
    let svc = Arc::new(DocService::new(config).await.unwrap());
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

impl TestServer {
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.client.request(method, format!("{}{}", self.base, path));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(Method::POST, path, Some(body)).await
    }
    async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(Method::PUT, path, Some(body)).await
    }
    async fn patch(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(Method::PATCH, path, Some(body)).await
    }
    async fn delete(&self, path: &str) -> (StatusCode, Value) {
        self.send(Method::DELETE, path, None).await
    }

    /// Create a subdirectory and return its `dir_id`.
    async fn mkdir(&self, parent: &str, name: &str) -> String {
        let (status, body) = self
            .post("/dirs", json!({ "parent_dir_id": parent, "name": name }))
            .await;
        assert_eq!(status, StatusCode::CREATED, "mkdir {name}: {body}");
        body["dir_id"].as_str().unwrap().to_string()
    }

    /// Create a document under `dir_id` and return its `doc_id`.
    async fn mkdoc(&self, dir_id: &str, title: &str, content: &str) -> String {
        let (status, body) = self
            .post(
                "/docs",
                json!({ "parent_dir_id": dir_id, "title": title, "content": content }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "mkdoc {title}: {body}");
        body["doc_id"].as_str().unwrap().to_string()
    }

    fn error_code(body: &Value) -> String {
        body["error"]["code"].as_str().unwrap_or("").to_string()
    }
}

// ── docs: CRUD + optimistic-version 409 ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn doc_crud_lifecycle_with_version_conflicts() {
    let srv = spawn().await;

    // Create at root.
    let (status, body) = srv
        .post(
            "/docs",
            json!({ "parent_dir_id": "root", "title": "PRD", "content": "# v1" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let doc_id = body["doc_id"].as_str().unwrap().to_string();
    assert_eq!(body["version"], 1);

    // Read back.
    let (status, body) = srv.get(&format!("/docs/{doc_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"], "# v1");
    assert_eq!(body["meta"]["name"], "PRD");

    // Happy-path update: base_version matches → version 2.
    let (status, body) = srv
        .put(
            &format!("/docs/{doc_id}"),
            json!({ "base_version": 1, "content": "# v2" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], 2);

    // Stale update: base_version 1 no longer matches → 409.
    let (status, body) = srv
        .put(
            &format!("/docs/{doc_id}"),
            json!({ "base_version": 1, "content": "# stale" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(TestServer::error_code(&body), "version_conflict");
    assert_eq!(body["error"]["details"], Value::Null); // code + message contract

    // Concurrent last-writer-wins is safe: doc still v2.
    let (_, body) = srv.get(&format!("/docs/{doc_id}")).await;
    assert_eq!(body["meta"]["version"], 2);

    // Rename keeps content; then delete lands in trash (404 on read).
    let (status, _) = srv
        .patch(
            &format!("/docs/{doc_id}/title"),
            json!({ "base_version": 2, "new_title": "PRD-2026" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = srv.delete(&format!("/docs/{doc_id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = srv.get(&format!("/docs/{doc_id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn doc_create_rejects_invalid_name_and_duplicate() {
    let srv = spawn().await;
    // Duplicate name in the same dir → 409 name_conflict.
    srv.mkdoc("root", "计划", "a").await;
    let (status, body) = srv
        .post("/docs", json!({ "parent_dir_id": "root", "title": "计划", "content": "b" }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(TestServer::error_code(&body), "name_conflict");

    // Traversal-ish title → 400.
    let (status, body) = srv
        .post("/docs", json!({ "parent_dir_id": "root", "title": "../evil", "content": "x" }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

// ── dirs: tree + cross-dir move + rollback ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn dir_tree_and_cross_dir_move() {
    let srv = spawn().await;
    let proj_a = srv.mkdir("root", "项目A").await;
    let proj_b = srv.mkdir("root", "项目B").await;
    let doc = srv.mkdoc(&proj_a, "纪要", "hello").await;

    // Root tree lists both dirs.
    let (status, body) = srv.get("/tree?dir_id=root").await;
    assert_eq!(status, StatusCode::OK);
    let dirs = body["dirs"].as_array().unwrap();
    assert_eq!(dirs.len(), 2);
    assert!(dirs.iter().any(|d| d["dir_id"] == proj_a.as_str()));

    // Move doc A → B: path updates, read still works from the new home.
    let (status, body) = srv
        .post(
            &format!("/docs/{doc}/move"),
            json!({ "target_dir_id": proj_b, "overwrite": false }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = srv.get(&format!("/docs/{doc}/path")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["path"].as_str().unwrap().starts_with(&proj_b),
        "doc should now live under B: {body}"
    );

    // Move to a non-existent directory fails cleanly (404) and the source
    // document remains readable — no partial state.
    let ghost = "dir-ffffffffffff";
    let (status, body) = srv
        .post(
            &format!("/docs/{doc}/move"),
            json!({ "target_dir_id": ghost, "overwrite": false }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = srv.get(&format!("/docs/{doc}")).await;
    assert_eq!(status, StatusCode::OK, "source doc must survive a failed move");
}

// ── review flow: submit → approve / reject / pre-empted / double ────────

#[tokio::test(flavor = "multi_thread")]
async fn review_approve_merges_and_bumps_version() {
    let srv = spawn().await;
    let doc = srv.mkdoc("root", "会议纪要", "# 会议纪要 v1").await;

    // Agent submits an update based on v1.
    let (status, body) = srv
        .post(
            "/requests",
            json!({
                "doc_id": doc,
                "base_version": 1,
                "content": "# 会议纪要 v2（agent 提案）",
                "submitted_by": "agent:com.example.agent",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let request_id = body["request_id"].as_str().unwrap().to_string();
    assert_eq!(body["status"], "pending");

    // Review queue shows it under pending.
    let (status, body) = srv.get("/requests?status=pending").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Approve → merged into the doc at version 2.
    let (status, body) = srv
        .post(
            &format!("/requests/{request_id}/approve"),
            json!({ "reviewed_by": "human:zhang", "note": "合理" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["doc_version"], 2);
    assert_eq!(body["request"]["status"], "approved");

    let (_, body) = srv.get(&format!("/docs/{doc}")).await;
    assert_eq!(body["meta"]["version"], 2);
    assert_eq!(body["content"], "# 会议纪要 v2（agent 提案）");

    // Per-doc history lists the approved request.
    let (status, body) = srv.get(&format!("/docs/{doc}/requests")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["status"], "approved");

    // Double-review is refused.
    let (status, body) = srv
        .post(
            &format!("/requests/{request_id}/approve"),
            json!({ "reviewed_by": "human:zhang" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(TestServer::error_code(&body), "already_reviewed");
}

#[tokio::test(flavor = "multi_thread")]
async fn review_reject_keeps_note_and_doc_untouched() {
    let srv = spawn().await;
    let doc = srv.mkdoc("root", "API 设计", "v1").await;
    let (_, body) = srv
        .post(
            "/requests",
            json!({ "doc_id": doc, "base_version": 1, "content": "v2 提案", "submitted_by": "agent:x" }),
        )
        .await;
    let request_id = body["request_id"].as_str().unwrap().to_string();

    let (status, body) = srv
        .post(
            &format!("/requests/{request_id}/reject"),
            json!({ "reviewed_by": "human:li", "note": "设计有冲突" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "rejected");
    assert_eq!(body["review_note"], "设计有冲突");

    let (_, body) = srv.get(&format!("/docs/{doc}")).await;
    assert_eq!(body["meta"]["version"], 1, "reject must not touch the doc");
}

#[tokio::test(flavor = "multi_thread")]
async fn review_preempted_base_returns_conflict_and_stays_pending() {
    let srv = spawn().await;
    let doc = srv.mkdoc("root", "周报", "v1").await;

    // Agent A submits based on v1; human directly edits to v2 meanwhile.
    let (_, body) = srv
        .post(
            "/requests",
            json!({ "doc_id": doc, "base_version": 1, "content": "A 提案", "submitted_by": "agent:a" }),
        )
        .await;
    let request_id = body["request_id"].as_str().unwrap().to_string();
    let (status, _) = srv
        .put(&format!("/docs/{doc}"), json!({ "base_version": 1, "content": "人类 v2" }))
        .await;
    assert_eq!(status, StatusCode::OK);

    // Approving A now conflicts (base 1 ≠ current 2) → 409, stays pending.
    let (status, body) = srv
        .post(&format!("/requests/{request_id}/approve"), json!({ "reviewed_by": "human:zhang" }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(TestServer::error_code(&body), "version_conflict");
    let (_, body) = srv.get(&format!("/requests/{request_id}")).await;
    assert_eq!(body["status"], "pending");

    // Re-submission on the new base succeeds and merges.
    let (status, body) = srv
        .post(
            "/requests",
            json!({ "doc_id": doc, "base_version": 2, "content": "A 重提 v3", "submitted_by": "agent:a" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let request_id2 = body["request_id"].as_str().unwrap().to_string();
    let (status, _) = srv
        .post(&format!("/requests/{request_id2}/approve"), json!({ "reviewed_by": "human:zhang" }))
        .await;
    assert_eq!(status, StatusCode::OK);
}

// ── trash: delete → list → restore round-trip ──────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn trash_delete_list_restore_round_trip() {
    let srv = spawn().await;
    let sub = srv.mkdir("root", "归档").await;
    let doc = srv.mkdoc(&sub, "旧计划", "# 旧计划内容").await;

    let (status, _) = srv.delete(&format!("/docs/{doc}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = srv.get("/trash").await;
    assert_eq!(status, StatusCode::OK);
    let slot = body.as_array().unwrap().iter().find(|s| s["doc_id"] == doc).expect("slot in trash");
    let trash_id = slot["trash_id"].as_str().unwrap().to_string();
    assert_eq!(slot["original_dir_id"], sub);

    // Restore re-creates under the original directory with full content.
    let (status, body) = srv.post(&format!("/trash/{trash_id}/restore"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new_doc = body["doc_id"].as_str().unwrap().to_string();
    assert_ne!(new_doc, doc, "restore mints a fresh doc_id");
    let (status, body) = srv.get(&format!("/docs/{new_doc}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"], "# 旧计划内容");

    let (status, body) = srv.get("/trash").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.as_array().unwrap().iter().all(|s| s["doc_id"] != doc),
        "restored slot must be dropped"
    );
}

// ── guards: invalid ids → unified JSON error envelope ───────────────────

#[tokio::test(flavor = "multi_thread")]
async fn invalid_ids_return_json_error_envelope() {
    let srv = spawn().await;

    // Malformed doc id (not `doc-` + hex).
    let (status, body) = srv.get("/docs/not-a-real-id").await;
    assert!(
        status.is_client_error(),
        "malformed id must be a 4xx, got {status}"
    );
    assert_eq!(TestServer::error_code(&body), "not_found");

    // Unknown but well-formed ids → 404 not_found.
    let (status, body) = srv.get("/docs/doc-ffffffffffff").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(TestServer::error_code(&body), "not_found");

    // Unknown request id.
    let (status, body) = srv.get("/requests/r-ffffffffffff").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(TestServer::error_code(&body), "not_found");
}

// ── search: keyword hits title and body ─────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn search_hits_title_and_body_across_dirs() {
    let srv = spawn().await;
    let a = srv.mkdir("root", "组A").await;
    srv.mkdoc("root", "产品方案PRD", "# 产品方案 v1").await;
    srv.mkdoc(&a, "会议纪要", "讨论了发布方案与回滚策略").await;
    srv.mkdoc("root", "无关笔记", "今天天气不错").await;

    // "方案" hits the PRD title (10) + its body (1) and the 纪要 body (1).
    let (status, body) = srv.get("/search?keyword=方案").await;
    assert_eq!(status, StatusCode::OK);
    let hits = body.as_array().unwrap();
    assert_eq!(hits.len(), 2, "{body}");
    assert_eq!(hits[0]["name"], "产品方案PRD");
    assert_eq!(hits[0]["score"], 11, "title 10 + body 1: {body}");
    assert_eq!(hits[1]["name"], "会议纪要");
    assert_eq!(hits[1]["score"], 1);
    assert!(!hits[1]["snippet"].as_str().unwrap().is_empty());

    // Miss → empty array (not an error).
    let (status, body) = srv.get("/search?keyword=不存在的关键词xyz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

// ── request-size limits (413 payload_too_large) ─────────────────────────
//
// Two ceilings, two failure modes, both of which used to be lies:
//   * the wire ceiling (axum's `DefaultBodyLimit`, 2 MiB) rejected with a
//     plain-text body before any handler ran, so `max_doc_size_bytes` never
//     took effect and the caller could not tell "too large" from "bad JSON";
//   * the document ceiling had no enforcement at all on some paths.
// These tests pin the observable behaviour of both, through the real router.

/// A server with a deliberately tiny document limit so the ceilings are cheap
/// to cross (`DocConfig::max_doc_size_bytes` is the only knob).
async fn spawn_with_limit(max_doc_size_bytes: usize) -> TestServer {
    let data = TempDir::new().unwrap();
    let config = DocConfig {
        data_dir: data.path().to_path_buf(),
        max_doc_size_bytes,
        ..DocConfig::default()
    };
    let svc = Arc::new(DocService::new(config).await.unwrap());
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

#[tokio::test(flavor = "multi_thread")]
async fn wire_body_over_the_transport_ceiling_returns_the_json_envelope() {
    let limit = 1024usize;
    let srv = spawn_with_limit(limit).await;
    // Comfortably over `transport_ceiling(1024)` = 1024*6 + 64 KiB.
    let ceiling = acowork_doc::limit::transport_ceiling(limit);
    let body = json!({
        "parent_dir_id": "root",
        "title": "huge",
        "content": "x".repeat(ceiling + 1),
    });
    let (status, resp) = srv.post("/docs", body).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{resp}");
    assert_eq!(TestServer::error_code(&resp), "payload_too_large");
    // The envelope carries `code` / `message` / `required`: the numbers
    // ride in the message, so the caller can still report them.
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains(&limit.to_string()), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn document_over_the_decoded_limit_is_rejected_even_though_the_body_fits() {
    let limit = 1024usize;
    let srv = spawn_with_limit(limit).await;
    // One byte over the *document* limit. The JSON envelope is small (ASCII,
    // no escapes), so the wire ceiling is nowhere near tripped: this asserts
    // the decoded check, which is the one the config promises.
    let content = "x".repeat(limit + 1);
    assert!(content.len() < acowork_doc::limit::transport_ceiling(limit));
    let (status, resp) = srv
        .post(
            "/docs",
            json!({ "parent_dir_id": "root", "title": "big", "content": content }),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{resp}");
    assert_eq!(TestServer::error_code(&resp), "payload_too_large");
    let message = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains(&(limit + 1).to_string()), "{message}");
    assert!(message.contains(&limit.to_string()), "{message}");

    // Exactly at the limit is accepted — the boundary is inclusive.
    let doc_id = srv.mkdoc("root", "at-limit", &"y".repeat(limit)).await;
    let (status, body) = srv.get(&format!("/docs/{doc_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"].as_str().unwrap().len(), limit);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_and_submit_share_the_document_ceiling() {
    let limit = 1024usize;
    let srv = spawn_with_limit(limit).await;
    let doc_id = srv.mkdoc("root", "PRD", "small").await;

    // PUT (direct human update)
    let (status, resp) = srv
        .put(
            &format!("/docs/{doc_id}"),
            json!({ "base_version": 1, "content": "z".repeat(limit + 1) }),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{resp}");
    assert_eq!(TestServer::error_code(&resp), "payload_too_large");

    // POST /docs/{id}/requests (agent proposal → human review). The proposal
    // is stored, so it must obey the same ceiling rather than smuggling an
    // oversized body in through the review queue.
    let (status, resp) = srv
        .post(
            "/requests",
            json!({
                "doc_id": doc_id,
                "base_version": 1,
                "content": "w".repeat(limit + 1),
                "submitted_by": "inst-0000-0000-0000-0000-000000000001",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{resp}");
    assert_eq!(TestServer::error_code(&resp), "payload_too_large");
}

// ── refs/validate: batch reference validation (plan §7 G4) ─────────────

#[tokio::test(flavor = "multi_thread")]
async fn validate_refs_resolves_ids_and_paths_and_reports_missing() {
    let srv = spawn().await;
    let root = acowork_doc::types::ROOT_DIR_ID;
    let dir = srv.mkdir(root, "研发").await;
    let doc = srv.mkdoc(&dir, "纪要", "内容").await;

    // A doc id, a library path, a missing path, and a malformed ref go in
    // together; each gets its own honest answer, in input order.
    let (status, resp) = srv
        .post(
            "/refs/validate",
            json!({ "refs": [doc.as_str(), "研发/纪要.md", "研发/不存在.md", "无后缀"] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    let results = resp["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(results[0]["status"], "found");
    assert_eq!(results[0]["doc_id"], doc.as_str());
    assert_eq!(results[0]["path"], "研发/纪要.md");
    assert_eq!(results[1]["status"], "found");
    assert_eq!(results[1]["doc_id"], doc.as_str());
    // The path form reports the canonical path the panel links against.
    assert_eq!(results[1]["path"], "研发/纪要.md");
    assert_eq!(results[2]["status"], "not_found");
    assert!(results[2].get("doc_id").is_none());
    assert_eq!(results[3]["status"], "invalid");
    assert_eq!(resp["all_found"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn validate_refs_root_path_and_empty_batch() {
    let srv = spawn().await;
    let root = acowork_doc::types::ROOT_DIR_ID;
    srv.mkdoc(root, "顶层", "x").await;

    // A root-level path (no directory chain) resolves.
    let (status, resp) = srv
        .post("/refs/validate", json!({ "refs": ["顶层.md"] }))
        .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["results"][0]["status"], "found");
    assert_eq!(resp["results"][0]["path"], "顶层.md");
    assert_eq!(resp["all_found"], true);

    // An empty batch is legal and answers with an empty list — the PM side
    // never calls with zero refs, but the endpoint must not error.
    let (status, resp) = srv.post("/refs/validate", json!({ "refs": [] })).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    assert_eq!(resp["results"].as_array().unwrap().len(), 0);
    assert_eq!(resp["all_found"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn validate_refs_rejects_oversized_batch() {
    let srv = spawn().await;
    let refs: Vec<String> = (0..51).map(|i| format!("doc-{i}")).collect();
    let (status, resp) = srv.post("/refs/validate", json!({ "refs": refs })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
}
