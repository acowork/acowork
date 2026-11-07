//! P4 远程场景端到端测试（T4-2 / T4-3）。
//!
//! 用**真实 HTTP server + reqwest 客户端**模拟「远程 Runtime 通过 advertise
//! endpoint 调用 pm MCP」的完整链路 —— 这是与现有 oneshot router 测试的本质区别：
//!
//! | 维度 | 现有 handlers_e2e / mcp tests | 本文件 remote_e2e |
//! |------|-------------------------------|-------------------|
//! | 传输 | axum `oneshot`（内存内，不走 TCP） | 真实 `TcpListener` + `axum::serve` |
//! | 客户端 | tower `ServiceExt` | reqwest HTTP 客户端 |
//! | 路径 | 内部路径 `/mcp` | 公开路径 `/api/pm/mcp`（Gateway `nest_service` 形态） |
//! | 身份 | 测试内构造 header | 真实 HTTP `X-MCP-Actor` header 往返 |
//!
//! 覆盖场景（对应开发计划 §3.5 T4-2 / T4-3）：
//!
//! 1. **T4-3 远程全链路**：人类 REST 建项目 + 建任务（指派远程 Agent）→ 远程
//!    Agent 经 MCP HTTP `pm_claim_task` → `pm_submit_task` → 人类 REST 查询
//!    看板确认任务已 submitted（看板刷新）。
//! 2. **T4-2 身份校验**（设计 §9.2）：非 assignee 调 `pm_claim_task` / 
//!    `pm_submit_task` 返回 JSON-RPC Forbidden（-32002）。
//! 3. **§9.3 匿名只读**：无 `X-MCP-Actor` 调 `pm_list_projects` 允许；调
//!    `pm_claim_task` 拒绝（Unauthenticated -32001）。

use std::sync::Arc;

use acowork_pm::{AgentDirectory, PmConfig, PmService};
use axum::Router;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// 白名单 Agent 目录：模拟 Gateway `installed_agents` 视图（设计 §9.1）。
struct WhitelistDir(Vec<String>);

#[async_trait::async_trait]
impl AgentDirectory for WhitelistDir {
    async fn agent_exists(&self, agent_id: &str) -> bool {
        self.0.iter().any(|a| a == agent_id)
    }
}

/// 启动真实 PM HTTP server，router 挂到 `/api/pm` 前缀下（与 Gateway
/// `nest_service("/api/pm", ...)` 生产形态一致）。
///
/// 返回 (公开 base URL, tempdir 句柄)。MCP 端点 = `{base}/mcp`。
async fn start_remote_server() -> (String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = PmConfig {
        data_dir: tmp.path().to_path_buf(),
        index_rebuild_on_start: false,
        ..Default::default()
    };

    // ADR-073: whitelist contains agent_instance_id (UUID)
    let agent_dir: Arc<dyn AgentDirectory> =
        Arc::new(WhitelistDir(vec!["3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d".to_string()]));
    let svc = PmService::with_agent_directory(cfg, agent_dir)
        .await
        .expect("PmService should start");

    let app = Router::new().nest_service("/api/pm", svc.router());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "remote e2e server exited");
        }
    });

    (format!("http://{addr}/api/pm"), tmp)
}

/// 模拟人类在 UI 添加项目成员（REST POST /members）。
async fn add_member_via_rest(
    client: &reqwest::Client,
    base: &str,
    pid: &str,
    instance_id: &str,
) {
    let resp = client
        .post(format!("{base}/projects/{pid}/members"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "instance_id": instance_id }).to_string())
        .send()
        .await
        .expect("add member http request");
    assert_eq!(
        resp.status(),
        200,
        "add member {instance_id} via REST should succeed"
    );
}

/// 发送一次 MCP JSON-RPC `tools/call`，返回完整 JSON-RPC 响应 Value。
async fn mcp_call(
    client: &reqwest::Client,
    base: &str,
    actor: Option<&str>,
    id: u64,
    tool: &str,
    args: Value,
) -> Value {
    let mut req = client
        .post(format!("{base}/mcp"))
        .header("content-type", "application/json")
        .body(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": tool, "arguments": args }
            })
            .to_string(),
        );
    if let Some(a) = actor {
        req = req.header("x-mcp-actor", a);
    }
    let resp = req.send().await.expect("mcp http request should succeed");
    assert_eq!(
        resp.status(),
        200,
        "MCP endpoint should return 200 for valid JSON-RPC"
    );
    resp.json::<Value>().await.expect("JSON-RPC response should parse")
}

/// 断言 tools/call 成功，并返回工具结果（`result.content[0].text` 解析后的 Value）。
fn tool_result(v: &Value) -> Value {
    assert!(v["error"].is_null(), "tool call should succeed: {v}");
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .expect("content[0].text should be a JSON string");
    serde_json::from_str(text).expect("tool result text should be valid JSON")
}

/// 断言 tools/call 失败并返回给定 JSON-RPC error code。
fn assert_rpc_error(v: &Value, code: i32) {
    assert!(
        v["error"].is_object(),
        "expected JSON-RPC error, got success: {v}"
    );
    assert_eq!(
        v["error"]["code"].as_i64().unwrap_or(0) as i32,
        code,
        "unexpected error code in {v}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// T4-3：远程 Agent 全链路 —— 人类建项目/任务 → 远程 Agent claim → submit →
// 看板刷新（人类 REST 确认任务状态 = submitted）。
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn remote_agent_claim_submit_full_chain() {
    let (base, _tmp) = start_remote_server().await;
    let client = reqwest::Client::new();
    let actor = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"; // ADR-073 instance_id

    // ── 1. 人类（REST，x-actor=human）建项目 ──────────────────────────
    let resp = client
        .post(format!("{base}/projects"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "远程协作项目", "description": "P4 e2e" }).to_string())
        .send()
        .await
        .expect("create project http request");
    assert_eq!(resp.status(), 200, "human creates project via REST");
    let proj: Value = resp.json().await.unwrap();
    let pid = proj["id"].as_str().unwrap().to_string();

    // ── 1.5 人类在 UI 添加远程 Agent 为项目成员（联动指派前置）─────────
    add_member_via_rest(&client, &base, &pid, actor).await;

    // ── 2. 人类建任务，指派给远程 Agent ────────────────────────────────
    let resp = client
        .post(format!("{base}/projects/{pid}/tasks"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(
            json!({
                "title": "远程任务",
                "description": "需要远程节点执行",
                "assignee": actor,
            })
            .to_string(),
        )
        .send()
        .await
        .expect("create task http request");
    assert_eq!(resp.status(), 200, "human creates task via REST");
    let task: Value = resp.json().await.unwrap();
    let tid = task["id"].as_str().unwrap().to_string();
    assert_eq!(task["status"], "pending", "human-created task starts as pending");

    // ── 3. 远程 Agent 自查（pm_list_tasks，过滤 assignee 自己）──────────
    let v = mcp_call(
        &client,
        &base,
        Some(actor),
        1,
        "pm_list_tasks",
        json!({ "project_id": pid, "assignee": actor }),
    )
    .await;
    let tasks = tool_result(&v);
    let tids: Vec<String> = tasks
        ["tasks"].as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["id"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        tids.contains(&tid),
        "remote agent should see its assigned task in pm_list_tasks: {tasks}"
    );

    // ── 4. 远程 Agent claim（pending → in_progress）─────────────────────
    let v = mcp_call(&client, &base, Some(actor), 2, "pm_claim_task", json!({ "task_id": tid }))
        .await;
    let claimed = tool_result(&v);
    assert_eq!(claimed["status"], "in_progress", "claim should move to in_progress");

    // ── 5. 远程 Agent submit（in_progress → submitted）──────────────────
    let v = mcp_call(
        &client,
        &base,
        Some(actor),
        3,
        "pm_submit_task",
        json!({ "task_id": tid, "text": "远程完成，结果已交付" }),
    )
    .await;
    let submitted = tool_result(&v);
    assert_eq!(submitted["status"], "submitted", "submit should move to submitted");

    // ── 6. 看板刷新：人类 REST 查询任务，确认远程结果已落盘 ─────────────
    let resp = client
        .get(format!("{base}/tasks/{tid}"))
        .send()
        .await
        .expect("get task http request");
    assert_eq!(resp.status(), 200, "human board refresh");
    let board: Value = resp.json().await.unwrap();
    assert_eq!(board["status"], "submitted", "board shows submitted");
    assert_eq!(board["result"]["text"], "远程完成，结果已交付");
    assert_eq!(board["result"]["submitted_by"], actor);
}

// ═══════════════════════════════════════════════════════════════════════
// T4-2：身份校验 —— 非 assignee 调用 claim/submit 一律 Forbidden。
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn non_assignee_mutation_rejected_over_http() {
    let (base, _tmp) = start_remote_server().await;
    let client = reqwest::Client::new();
    let owner = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"; // ADR-073 instance_id (UUID)
    let intruder = "5d2e1100-7e4b-4d2a-b6f1-1a91b07e4c2d"; // 另一个 instance，校验非 assignee 拒绝

    // 建项目 + 任务（assignee = owner instance_id，ADR-073）
    let resp = client
        .post(format!("{base}/projects"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "P" }).to_string())
        .send()
        .await
        .unwrap();
    let proj: Value = resp.json().await.unwrap();
    let pid = proj["id"].as_str().unwrap().to_string();

    // 联动指派：人类先添加 owner 为项目成员
    add_member_via_rest(&client, &base, &pid, owner).await;

    let resp = client
        .post(format!("{base}/projects/{pid}/tasks"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "T", "assignee": owner }).to_string())
        .send()
        .await
        .unwrap();
    let task: Value = resp.json().await.unwrap();
    let tid = task["id"].as_str().unwrap().to_string();

    // 入侵者 claim → Forbidden（-32002）
    let v = mcp_call(
        &client,
        &base,
        Some(intruder),
        1,
        "pm_claim_task",
        json!({ "task_id": tid }),
    )
    .await;
    assert_rpc_error(&v, -32002);

    // 任务未被改变（仍是 pending）
    let resp = client.get(format!("{base}/tasks/{tid}")).send().await.unwrap();
    let board: Value = resp.json().await.unwrap();
    assert_eq!(board["status"], "pending", "intruder claim must not mutate");

    // 入侵者 submit → Forbidden（即便 assignee 是别人）
    let v = mcp_call(
        &client,
        &base,
        Some(intruder),
        2,
        "pm_submit_task",
        json!({ "task_id": tid, "text": "hacked" }),
    )
    .await;
    assert_rpc_error(&v, -32002);

    // 拥有者仍可正常 claim → in_progress（身份边界正确）
    let v = mcp_call(&client, &base, Some(owner), 3, "pm_claim_task", json!({ "task_id": tid }))
        .await;
    let claimed = tool_result(&v);
    assert_eq!(claimed["status"], "in_progress");
}

// ═══════════════════════════════════════════════════════════════════════
// §9.3：匿名只读 —— 无 X-MCP-Actor 允许只读工具、拒绝状态变更工具。
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn anonymous_readonly_allowed_mutation_rejected() {
    let (base, _tmp) = start_remote_server().await;
    let client = reqwest::Client::new();

    // 匿名 list 允许
    let v = mcp_call(&client, &base, None, 1, "pm_list_projects", json!({})).await;
    tool_result(&v); // 不 panic 即成功

    // 匿名 get（任意 id）→ 允许只读，404 是业务错误而非鉴权错误
    let v = mcp_call(&client, &base, None, 2, "pm_get_task", json!({ "task_id": "t-nonexistent" }))
        .await;
    assert!(v["error"].is_object(), "anonymous get on missing task returns error");

    // 匿名 claim → Unauthenticated（-32001）
    let v = mcp_call(&client, &base, None, 3, "pm_claim_task", json!({ "task_id": "t-x" }))
        .await;
    assert_rpc_error(&v, -32001);
}

// ═══════════════════════════════════════════════════════════════════════
// ADR-073：assignee 必须是 agent_instance_id（UUID 形状）。
// pm_create_task 接受任意字符串，但 claim/submit 的身份校验按字符串比对
// 是否与 task.assignee 一致；这里验证两个不同 instance 互不干扰。
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn different_instance_ids_isolated() {
    let (base, _tmp) = start_remote_server().await;
    let client = reqwest::Client::new();
    let alice = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"; // 白名单内
    let bob = "5d2e1100-7e4b-4d2a-b6f1-1a91b07e4c2d";   // 不在白名单（pm_create_task 用 alice 建任务指给 bob 时应被拒）

    // ── Alice 建任务指派给自己 ─────────────────────────────────────
    let resp = client
        .post(format!("{base}/projects"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "P" }).to_string())
        .send().await.unwrap();
    let pid = resp.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_string();

    // 联动指派：人类先添加 alice 为项目成员
    add_member_via_rest(&client, &base, &pid, alice).await;

    let resp = client
        .post(format!("{base}/projects/{pid}/tasks"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "T", "assignee": alice }).to_string())
        .send().await.unwrap();
    let tid = resp.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_string();

    // ── Bob（非白名单）试图创建任务指派给自己 → BadRequest（§9.1，映射 -32603）────
    let v = mcp_call(
        &client, &base, Some(bob), 1, "pm_create_task",
        json!({ "project_id": pid, "title": "Bob task", "assignee": bob }),
    ).await;
    assert_rpc_error(&v, -32603 /* InternalError per PmError::mcp_error_code for non-{Unauth,Forbidden} */);

    // ── Bob（非 assignee）试图 claim alice 的任务 → Forbidden ─────────
    let v = mcp_call(
        &client, &base, Some(bob), 2, "pm_claim_task",
        json!({ "task_id": tid }),
    ).await;
    assert_rpc_error(&v, -32002);

    // ── Alice（合法 assignee）claim → in_progress ────────────────────
    let v = mcp_call(
        &client, &base, Some(alice), 3, "pm_claim_task",
        json!({ "task_id": tid }),
    ).await;
    let claimed = tool_result(&v);
    assert_eq!(claimed["status"], "in_progress");
    assert_eq!(claimed["assignee"], alice, "claim must persist actor as instance_id");
}

// ═══════════════════════════════════════════════════════════════════════
// ADR-073 invariant 1：同包多 instance 必须独立寻址、各自完成任务。
//
// 这是 ADR-073 的核心动机：把 agent_id（包身份）和 instance_id（实例身份）
// 拆开，支持"同一 package 在两个 workspace 各跑一份"。如果还按 agent_id
// 寻址，alice 和 bob claim 对方任务时 `ensure_assignee` 会通过（因为两侧
// 都是 agent_id 字符串）→ 数据竞争。本测试验证 instance_id 隔离正确。
// ═══════════════════════════════════════════════════════════════════════

async fn start_remote_server_with(whitelist: Vec<String>) -> (String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = PmConfig {
        data_dir: tmp.path().to_path_buf(),
        index_rebuild_on_start: false,
        ..Default::default()
    };
    let agent_dir: Arc<dyn AgentDirectory> = Arc::new(WhitelistDir(whitelist));
    let svc = PmService::with_agent_directory(cfg, agent_dir)
        .await
        .expect("PmService should start");
    let app = Router::new().nest_service("/api/pm", svc.router());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/api/pm"), tmp)
}

#[tokio::test]
async fn multi_instance_same_package_tasks_are_isolated() {
    // 两个 instance 都是 "com.acowork.architect" 包，但 instance_id 不同。
    let workspace_a = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
    let workspace_b = "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b";
    let (base, _tmp) = start_remote_server_with(vec![
        workspace_a.to_string(),
        workspace_b.to_string(),
    ])
    .await;
    let client = reqwest::Client::new();

    // 建项目
    let resp = client
        .post(format!("{base}/projects"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "P" }).to_string())
        .send().await.unwrap();
    let pid = resp.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_string();

    // 联动指派：人类先添加两个 instance 为项目成员
    add_member_via_rest(&client, &base, &pid, workspace_a).await;
    add_member_via_rest(&client, &base, &pid, workspace_b).await;

    // 人类建两个任务，分别指给 workspace-a 和 workspace-b
    let resp = client
        .post(format!("{base}/projects/{pid}/tasks"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "Task A", "assignee": workspace_a }).to_string())
        .send().await.unwrap();
    let tid_a = resp.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_string();

    let resp = client
        .post(format!("{base}/projects/{pid}/tasks"))
        .header("x-actor", "human")
        .header("content-type", "application/json")
        .body(json!({ "title": "Task B", "assignee": workspace_b }).to_string())
        .send().await.unwrap();
    let tid_b = resp.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_string();

    // ── workspace-a 自查：能看到 Task A，看不到 Task B ──────────────
    let v = mcp_call(
        &client, &base, Some(workspace_a), 1, "pm_list_my_tasks",
        json!({ "project_id": pid }),
    ).await;
    let my_tasks_a: Vec<String> = tool_result(&v)["tasks"]
        .as_array().unwrap().iter()
        .filter_map(|t| t["id"].as_str().map(String::from))
        .collect();
    assert!(my_tasks_a.contains(&tid_a), "workspace-a must see its own task");
    assert!(!my_tasks_a.contains(&tid_b), "workspace-a must NOT see workspace-b's task");

    // ── workspace-b 自查：能看到 Task B，看不到 Task A ──────────────
    let v = mcp_call(
        &client, &base, Some(workspace_b), 2, "pm_list_my_tasks",
        json!({ "project_id": pid }),
    ).await;
    let my_tasks_b: Vec<String> = tool_result(&v)["tasks"]
        .as_array().unwrap().iter()
        .filter_map(|t| t["id"].as_str().map(String::from))
        .collect();
    assert!(my_tasks_b.contains(&tid_b), "workspace-b must see its own task");
    assert!(!my_tasks_b.contains(&tid_a), "workspace-b must NOT see workspace-a's task");

    // ── workspace-a claim Task B → Forbidden（核心 ADR-073 不变量）──
    let v = mcp_call(
        &client, &base, Some(workspace_a), 3, "pm_claim_task",
        json!({ "task_id": tid_b }),
    ).await;
    assert_rpc_error(&v, -32002);

    // ── workspace-b claim Task A → Forbidden ─────────────────────────
    let v = mcp_call(
        &client, &base, Some(workspace_b), 4, "pm_claim_task",
        json!({ "task_id": tid_a }),
    ).await;
    assert_rpc_error(&v, -32002);

    // ── 各自 claim 自己的任务 → 互不干扰 ──────────────────────────
    let v = mcp_call(
        &client, &base, Some(workspace_a), 5, "pm_claim_task",
        json!({ "task_id": tid_a }),
    ).await;
    assert_eq!(tool_result(&v)["status"], "in_progress");

    let v = mcp_call(
        &client, &base, Some(workspace_b), 6, "pm_claim_task",
        json!({ "task_id": tid_b }),
    ).await;
    assert_eq!(tool_result(&v)["status"], "in_progress");
}
