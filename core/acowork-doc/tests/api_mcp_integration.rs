//! End-to-end MCP ↔ REST review-loop integration tests (D3-5).
//!
//! Boots the real doc server (full router: REST + `/mcp`), then drives the
//! **Agent side** over JSON-RPC (with `X-MCP-Actor`, as injected by the
//! Gateway catalog) and the **human side** over REST:
//!
//! ```text
//! AGENT doc_mkdir → doc_write(mode=create) → (REST GET 人类可见)
//!      → AGENT doc_write(mode=append)      = 自己改自己 → 直接生效
//!      → EDITOR doc_stat → doc_write(mode=overwrite, if_version)
//!                                          = 改别人的 → pending_review
//!      → (REST approve) → doc_check_request=approved → doc_read v3
//! ```
//!
//! Identity: anonymous callers get read-only tools; writes return JSON-RPC
//! `-32002` (permission), mirroring design §9.

use std::net::SocketAddr;
use std::sync::Arc;

use acowork_doc::config::DocConfig;
use acowork_doc::server::DocService;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

// ADR-073: this stands in for the runtime instance identity (UUID)
// injected over the `X-MCP-Actor` header by the Gateway catalog. The
// doc server itself doesn't enforce UUID format — the Gateway
// `doc_proxy` is the trust boundary — but the test fixture uses a
// UUID-shaped string so the wire JSON matches the production contract.
const AGENT: &str = "inst-aaaa-bbbb-cccc-dddd-000000000001";
// Second instance: owns nothing, so its writes hit the review gate.
const EDITOR: &str = "inst-aaaa-bbbb-cccc-dddd-000000000002";

struct TestServer {
    base: String,
    client: reqwest::Client,
    _data: TempDir,
}

async fn spawn() -> TestServer {
    let data = TempDir::new().unwrap();
    let config = DocConfig {
        data_dir: data.path().to_path_buf(),
        ..Default::default()
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
    /// JSON-RPC `tools/call`（模拟 Runtime MCP 客户端 + Gateway 注入身份）。
    async fn tool(&self, actor: Option<&str>, name: &str, args: Value) -> Value {
        let payload = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        let mut req = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("content-type", "application/json");
        if let Some(a) = actor {
            req = req.header("x-mcp-actor", a);
        }
        let resp = req.json(&payload).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "MCP endpoint always answers 200");
        resp.json::<Value>().await.unwrap()
    }

    /// 成功调用的解包内容（`result.content[0].text` JSON）；错误则 panic。
    async fn ok(&self, actor: Option<&str>, name: &str, args: Value) -> Value {
        let resp = self.tool(actor, name, args).await;
        assert!(resp.get("error").is_none(), "tool {name} failed: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    /// 期望 JSON-RPC error，返回 error code + message。
    async fn tool_err(&self, actor: Option<&str>, name: &str, args: Value) -> (i64, String) {
        let resp = self.tool(actor, name, args).await;
        let code = resp["error"]["code"].as_i64().expect("error code");
        let msg = resp["error"]["message"].as_str().unwrap().to_string();
        (code, msg)
    }

    async fn rest_get(&self, path: &str) -> Value {
        let resp = self
            .client
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "GET {path}");
        resp.json::<Value>().await.unwrap()
    }

    async fn rest_post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let resp = self
            .client
            .post(format!("{}{}", self.base, path))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let value = resp.json::<Value>().await.unwrap_or(Value::Null);
        (status, value)
    }
}

/// D3-5 核心闭环：Agent 经 MCP 建目录/加文档 → 人类 REST 可见 →
/// Agent pull/submit_update → 人类 approve → Agent 轮询到 approved +
/// 文档 v2 合并内容。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_full_review_loop_agent_human() {
    let srv = spawn().await;

    // ── Agent 建目录 + 建档（O_EXCL 语义，直接生效，带 import 来源）────
    let v = srv.ok(Some(AGENT), "doc_mkdir", json!({ "path": "研发" })).await;
    assert_eq!(v["name"], "研发");

    let v = srv
        .ok(
            Some(AGENT),
            "doc_write",
            json!({
                "ref": "研发/需求说明.md",
                "mode": "create",
                "content": "# v1 需求",
                "source_workspace": "ws-main",
                "source_path": "docs/req.md",
            }),
        )
        .await;
    assert_eq!(v["status"], "applied");
    assert_eq!(v["path"], "研发/需求说明.md", "path must be human-readable");
    let doc_id = v["doc_id"].as_str().unwrap().to_string();
    assert_eq!(v["version"], 1);

    // `create` 是可重放的 O_EXCL：同名再建必须冲突，不能静默覆盖。
    let (code, msg) = srv
        .tool_err(
            Some(AGENT),
            "doc_write",
            json!({ "ref": "研发/需求说明.md", "mode": "create", "content": "dup" }),
        )
        .await;
    assert!(code != 0 && msg.contains("name_conflict"), "{msg}");

    // ── 人类可见：REST tree 显示新目录与新文档，import 元数据完整 ──────
    let tree = srv.rest_get("/tree?dir_id=root").await;
    assert_eq!(tree["dirs"][0]["name"], "研发");
    let doc = srv.rest_get(&format!("/docs/{doc_id}")).await;
    assert_eq!(doc["meta"]["version"], 1);
    assert_eq!(doc["content"], "# v1 需求");
    assert_eq!(doc["meta"]["import"]["instance_id"], AGENT);
    assert_eq!(doc["meta"]["import"]["workspace_path"], "ws-main:docs/req.md");
    // ADR-073 wire-contract guard: the legacy `agent_id` key must NOT
    // appear in the JSON. A regression that re-introduces the old field
    // name would break desktop display-name resolution (which keys by
    // instance_id through agentStore) and would silently mask the
    // semantic drift.
    assert!(
        doc["meta"]["import"].get("agent_id").is_none(),
        "wire JSON must not contain legacy `agent_id` key: {}",
        doc
    );

    // ── 自己的文档：append 直接生效，不进审批 ─────────────────────────
    let v = srv
        .ok(
            Some(AGENT),
            "doc_write",
            json!({ "ref": doc_id, "mode": "append", "content": "\n\n## 补充" }),
        )
        .await;
    assert_eq!(v["status"], "applied");
    assert_eq!(v["version"], 2);

    // ── 别人的文档：同一个 doc_write，返回 pending_review ──────────────
    // 先 doc_stat 取 version（不拉正文），再用 if_version 做乐观并发守卫。
    let st = srv.ok(Some(EDITOR), "doc_stat", json!({ "ref": doc_id })).await;
    assert_eq!(st["version"], 2);
    assert!(st.get("content").is_none(), "doc_stat must not return a body");

    let v = srv
        .ok(
            Some(EDITOR),
            "doc_write",
            json!({
                "ref": doc_id,
                "mode": "overwrite",
                "if_version": 2,
                "content": "# v3 需求（agent 修订）",
            }),
        )
        .await;
    let request_id = v["request_id"].as_str().unwrap().to_string();
    assert_eq!(v["status"], "pending_review");
    assert_eq!(v["base_version"], 2);

    // ── 人类 REST approve → 合并，doc version+1 ────────────────────────
    let (status, body) = srv
        .rest_post(
            &format!("/requests/{request_id}/approve"),
            json!({ "reviewed_by": "human:zhang", "note": "可行" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["doc_version"], 3);

    // ── Agent 轮询：approved + note；read 看到合并后 v2 ────────────────
    let v = srv.ok(None, "doc_check_request", json!({ "request_id": request_id })).await;
    assert_eq!(v["status"], "approved");
    assert_eq!(v["reviewed_by"], "human:zhang");

    let v = srv.ok(None, "doc_read", json!({ "ref": doc_id })).await;
    assert_eq!(v["version"], 3);
    assert!(v["content"].as_str().unwrap().contains("agent 修订"));
    // 审阅后正文被整篇替换（overwrite 提案），追加的旧内容不再存在
    assert!(!v["content"].as_str().unwrap().contains("## 补充"));

    // 人类再次直接 PUT（版本并发仍生效）—— doc 现 v2，stale base 409
    let (status, body) = srv
        .rest_post(
            &format!("/docs/{doc_id}/move"),
            json!({ "target_dir_id": "root", "overwrite": false }),
        )
        .await;
    // move 不需要版本号：目标 root 同名检查 → root 无同名 → 成功
    assert_eq!(status, StatusCode::OK, "{body}");
    let doc = srv.rest_get(&format!("/docs/{doc_id}")).await;
    assert_eq!(doc["meta"]["version"], 3, "move must not bump version");
}

/// 匿名：只读工具可用；写工具 JSON-RPC -32002（permission 分类）。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_anonymous_read_only_writes_forbidden() {
    let srv = spawn().await;

    // 匿名只读（空库 list / search）→ 正常返回
    let v = srv.ok(None, "doc_list", json!({})).await;
    assert_eq!(v["total"], 0);
    let v = srv.ok(None, "doc_search", json!({ "keyword": "x" })).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 0);

    // 匿名写 → -32002 forbidden，message 带 forbidden 前缀
    let (code, msg) = srv.tool_err(None, "doc_mkdir", json!({ "path": "x" })).await;
    assert_eq!(code, -32002, "{msg}");
    assert!(msg.contains("forbidden"), "{msg}");

    let (code, msg) = srv
        .tool_err(None, "doc_write", json!({ "ref": "x.md", "content": "x" }))
        .await;
    assert_eq!(code, -32002, "{msg}");

    // 结构操作同样受保护（匿名连所有权判定都进不去）
    let (code, msg) = srv
        .tool_err(None, "doc_remove", json!({ "ref": "doc-ffffffffffff" }))
        .await;
    assert_eq!(code, -32002, "{msg}");
}

/// stale base_version 提交 → 业务错误（version_conflict 前缀，非协议错误）。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_submit_stale_base_reports_version_conflict() {
    let srv = spawn().await;
    let v = srv
        .ok(
            Some(AGENT),
            "doc_write",
            json!({ "ref": "周报.md", "mode": "create", "content": "v1" }),
        )
        .await;
    let doc_id = v["doc_id"].as_str().unwrap().to_string();

    let (code, msg) = srv
        .tool_err(
            Some(AGENT),
            "doc_write",
            json!({ "ref": doc_id, "mode": "overwrite", "if_version": 42, "content": "stale" }),
        )
        .await;
    assert!(code != 0, "{msg}");
    assert!(msg.contains("version_conflict"), "{msg}");
}


// ── ADR-073 invariant: doc wire JSON keys by `instance_id`, not `agent_id`
//
// The runtime identity that the Gateway injects via the `X-MCP-Actor`
// header is the **runtime instance id** (UUID), not the package id. We
// therefore persist that value into `ImportSource.instance_id` and
// `UpdateRequest.submitted_by`, and serialise the former as
// `meta.import.instance_id` on the wire. A regression that re-introduces
// the legacy `agent_id` field name would silently break the desktop
// badge (which resolves the UUID to a display name via `agentStore`).

/// `doc_write` 必须把 actor 写入 `instance_id`（包 id 是不允许的）。
///
/// Reuse [`mcp_full_review_loop_agent_human`] 风格的 server boot 但只测
/// import 字段，避免对其它 review-loop 路径的耦合。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_doc_write_records_instance_id_under_instance_id_field() {
    let srv = spawn().await;
    // act
    let v = srv
        .ok(
            Some(AGENT),
            "doc_write",
            json!({
                "ref": "ADR-073 回归文档.md",
                "mode": "create",
                "content": "记录 instance_id 语义",
                "source_workspace": "ws-main",
                "source_path": "notes/adr.md",
            }),
        )
        .await;
    let doc_id = v["doc_id"].as_str().unwrap().to_string();

    // assert: REST GET 显示 import.instance_id 是 actor UUID，不是包 id
    let doc = srv.rest_get(&format!("/docs/{doc_id}")).await;
    assert_eq!(
        doc["meta"]["import"]["instance_id"], AGENT,
        "doc server must persist `X-MCP-Actor` (instance id) into \
         `meta.import.instance_id` (ADR-073). Got: {}",
        doc
    );
    // assert: 明确禁止 `agent_id` 字段残留 — 包 ID 不再上 wire
    assert!(
        doc["meta"]["import"].get("agent_id").is_none(),
        "wire JSON must not contain legacy `agent_id` key (ADR-073): {}",
        doc
    );
}

/// `doc_write` 在 actor 是合法 UUID 的前提下必须成功；如果 actor 缺失则
/// 403 forbidden（不与 `agent_id` 字段语义混淆）。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_doc_write_rejects_anonymous_with_forbidden() {
    let srv = spawn().await;
    let (code, msg) = srv
        .tool_err(
            None,
            "doc_write",
            json!({ "ref": "匿名尝试.md", "mode": "create", "content": "x" }),
        )
        .await;
    assert_eq!(code, -32002, "{msg}");
    assert!(msg.contains("forbidden"), "{msg}");
}

/// 验证 doc 端不再有 `agent_id` 字段的接口契约：搜 `wire-keys-probe`
/// 工具上的每个 metadata 节点都不应暴露 `agent_id` key。
///
/// 这是一个「panic if you regress」式的回归：如果有谁把
/// `instance_id` 又改名回 `agent_id`，本测试会失败。
#[tokio::test(flavor = "multi_thread")]
async fn mcp_wire_contract_no_legacy_agent_id_key() {
    let srv = spawn().await;
    let v = srv
        .ok(
            Some(AGENT),
            "doc_write",
            json!({
                "ref": "wire 契约锁定.md",
                "mode": "create",
                "content": "x",
                "source_workspace": "ws-main",
                "source_path": "notes/probe.md",
            }),
        )
        .await;
    let doc_id = v["doc_id"].as_str().unwrap().to_string();

    // GET → `import.agent_id` 必须不存在；`import.instance_id` 必须存在
    let doc = srv.rest_get(&format!("/docs/{doc_id}")).await;
    let imp = &doc["meta"]["import"];
    assert!(imp.is_object(), "{imp}");
    assert!(
        imp.get("agent_id").is_none(),
        "ADR-073 regression: doc REST returned legacy `agent_id` field: {imp}"
    );
    assert!(
        imp.get("instance_id").is_some(),
        "ADR-073 contract: doc REST must expose `instance_id`: {imp}"
    );
    assert_eq!(imp["workspace_path"], "ws-main:notes/probe.md");

    // JSON-RPC `doc_read` 返回的 `import` 节点同样契约
    let via_mcp = srv.ok(None, "doc_read", json!({ "ref": doc_id })).await;
    let imp_mcp = &via_mcp["import"];
    assert!(
        imp_mcp.get("agent_id").is_none(),
        "ADR-073 regression: doc_read MCP returned legacy `agent_id` field: {imp_mcp}"
    );
    assert!(
        imp_mcp.get("instance_id").is_some(),
        "ADR-073 contract: doc_read MCP must expose `instance_id`: {imp_mcp}"
    );
}

// ── 请求体积上限（413 / payload_too_large） ─────────────────────────────
//
// MCP 面自己按 `transport_ceiling` 限长（不交给 `DefaultBodyLimit`），这样
// 超限时回的是与 REST 一致的 `payload_too_large` JSON 信封，而不是 axum 的
// 纯文本拒绝——否则调用方分不清「太大了」和「JSON 坏了」。解码后的正文则由
// 各工具用 `limit::ensure_document_size` 把关，包括 `append` 的**结果**大小。

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
async fn wire_body_over_the_transport_ceiling_answers_a_json_envelope() {
    let limit = 1024usize;
    let srv = spawn_with_limit(limit).await;
    let ceiling = acowork_doc::limit::transport_ceiling(limit);
    let payload = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "doc_write",
            "arguments": {
                "ref": "huge.md", "mode": "create",
                "content": "x".repeat(ceiling + 1),
            }
        }
    });
    let resp = srv
        .client
        .post(format!("{}/mcp", srv.base))
        .header("content-type", "application/json")
        .header("x-mcp-actor", AGENT)
        .json(&payload)
        .send()
        .await
        .unwrap();
    // Not 200: the body never became a JSON-RPC request.
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "payload_too_large");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains(&limit.to_string()), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_content_over_the_document_limit_is_refused() {
    let limit = 1024usize;
    let srv = spawn_with_limit(limit).await;
    // The envelope fits comfortably under the wire ceiling; only the decoded
    // document is too big — the limit that `max_doc_size_bytes` promises.
    let (code, msg) = srv
        .tool_err(
            Some(AGENT),
            "doc_write",
            json!({ "ref": "big.md", "mode": "create", "content": "x".repeat(limit + 1) }),
        )
        .await;
    assert_eq!(code, -32603, "{msg}");
    assert!(msg.starts_with("payload_too_large"), "{msg}");

    // At the limit: accepted.
    let v = srv
        .ok(
            Some(AGENT),
            "doc_write",
            json!({ "ref": "at-limit.md", "mode": "create", "content": "y".repeat(limit) }),
        )
        .await;
    assert_eq!(v["status"], "applied");
}

#[tokio::test(flavor = "multi_thread")]
async fn append_cannot_grow_a_document_past_the_limit() {
    let limit = 1024usize;
    let srv = spawn_with_limit(limit).await;
    srv.ok(
        Some(AGENT),
        "doc_write",
        json!({ "ref": "grow.md", "mode": "create", "content": "a".repeat(limit - 10) }),
    )
    .await;

    // The appended chunk alone fits; the resulting document does not. The
    // check is on the result, so the limit cannot be walked past 10 bytes at
    // a time.
    let (code, msg) = srv
        .tool_err(
            Some(AGENT),
            "doc_write",
            json!({ "ref": "grow.md", "mode": "append", "content": "b".repeat(20) }),
        )
        .await;
    assert_eq!(code, -32603, "{msg}");
    assert!(msg.starts_with("payload_too_large"), "{msg}");

    // A chunk that still lands under the limit applies.
    let v = srv
        .ok(
            Some(AGENT),
            "doc_write",
            json!({ "ref": "grow.md", "mode": "append", "content": "c".repeat(5) }),
        )
        .await;
    assert_eq!(v["status"], "applied");
    let read = srv
        .ok(None, "doc_read", json!({ "ref": "grow.md" }))
        .await;
    assert_eq!(read["content"].as_str().unwrap().len(), limit - 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_request_over_the_limit_is_refused_on_submit() {
    let limit = 1024usize;
    let srv = spawn_with_limit(limit).await;
    let v = srv
        .ok(
            Some(AGENT),
            "doc_write",
            json!({ "ref": "PRD.md", "mode": "create", "content": "small" }),
        )
        .await;
    let doc_id = v["doc_id"].as_str().unwrap().to_string();

    // Submit side: an oversized proposal must not be parked in the queue.
    let (status, body) = srv
        .rest_post(
            "/requests",
            json!({
                "doc_id": doc_id,
                "base_version": 1,
                "content": "w".repeat(limit + 1),
                "submitted_by": "inst-0000-0000-0000-0000-000000000001",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "payload_too_large");

    // A proposal that fits is accepted and approving it applies normally.
    // (The approve path re-checks the size too, covering a limit lowered
    // after submission; that branch is not reachable from one config here.)
    let (status, body) = srv
        .rest_post(
            "/requests",
            json!({
                "doc_id": doc_id,
                "base_version": 1,
                "content": "v".repeat(limit),
                "submitted_by": "inst-0000-0000-0000-0000-000000000001",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let request_id = body["request_id"].as_str().unwrap().to_string();
    let (status, body) = srv
        .rest_post(&format!("/requests/{request_id}/approve"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
