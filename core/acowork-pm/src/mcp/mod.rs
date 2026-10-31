//! MCP (Model Context Protocol) HTTP Server —— JSON-RPC over streamable HTTP。
//!
//! ## 协议
//!
//! 服务端实现 [MCP streamable HTTP](https://modelcontextprotocol.io/) 子集，
//! 单一 `POST /mcp` 端点，请求/响应均为 JSON-RPC 2.0：
//!
//! | JSON-RPC method | 说明 |
//! |-----------------|------|
//! | `initialize` | 握手：返回协议版本 + `capabilities.tools` + `serverInfo` |
//! | `notifications/initialized` | 客户端初始化完成通知（**不**回响应，返回 202） |
//! | `tools/list` | 列出工具（见 [`manifest::PM_TOOL_MANIFEST`]） |
//! | `tools/call` | 调用工具（见 [`tools::dispatch`]） |
//!
//! 服务端**无状态**（不维护 `Mcp-Session-Id`），每次请求独立鉴权。
//! 响应始终为 `application/json`（客户端 `HttpTransport` 同时支持 JSON 与
//! SSE 响应，取 JSON 即可）。
//!
//! ## 与 REST API 的关系
//!
//! MCP 是 REST 的**语义等价**子集——所有 `pm_*` 工具背后调用同一个
//! [`PmStore`] trait。Agent 经 MCP 调用时服务端自动：
//!
//! - 从 `X-MCP-Actor` header 读取调用方 `agent_id`（由 Gateway catalog
//!   注入，见设计 §6.1 / T3-4）
//! - 执行身份校验（设计 §9.2 / §9.3）：匿名只读；状态变更工具校验
//!   调用者 == 任务 `assignee`；`pm_create_task` 校验 assignee 存在
//! - 返回精简 JSON（避免向 LLM 暴露内部 details）
//!
//! ## 错误语义
//!
//! - 协议级错误（parse / invalid request / method not found / invalid
//!   params）→ 标准 JSON-RPC error code（-32700 / -32600 / -32601 / -32602）
//! - 工具级业务错误（`PmError`，含 `Forbidden` / `DependencyNotSatisfied`
//!   等）→ JSON-RPC error，code 见 [`crate::error::PmError::mcp_error_code`]，
//!   message 为 `"{error_code}: {detail}"`。
//!
//!   之所以走 JSON-RPC error 而非 `isError:true` content：acowork-mcp
//!   `McpToolWrapper` 会把 error message 分类为 `[permission]` / `[permanent]`
//!   / `[transient]` 前缀返回给 LLM，Agent 才能做出正确决策（如 403 不重试、
//!   依赖未满足稍后重试）。
//!
//! ## 挂载
//!
//! `mcp_router` 返回 `Router<()>`（内部 `.with_state(McpState)` 注入 state），
//! 由 [`crate::server::PmService::router`] 与 REST `pm_router` 合并后，经
//! Gateway `nest_service("/api/pm", ...)` 挂载。公开端点：
//! `http://{gw}/api/pm/mcp`（设计 §6 / §8）。
//!
//! ## 设计参考
//!
//! [`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §6 / §8 / §9

pub mod agent_dir;
pub mod manifest;
pub mod tools;

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::error::PmError;
use crate::store::tree::TreePmStore;

// ── 状态与契约 ────────────────────────────────────────────────────────────

/// MCP 服务端状态（axum `State<McpState>`）。
#[derive(Clone)]
pub struct McpState {
    pub store: Arc<TreePmStore>,
    pub agent_dir: Arc<dyn AgentDirectory>,
}

/// Agent 目录契约（设计 §9.1）：查询某 `agent_id` 是否已安装/存在。
///
/// **依赖方向**：acowork-pm 只定义契约；Gateway 提供实现（基于其
/// `installed_agents`）。pm 侧不反向依赖 gateway。
///
/// `pm_create_task` 指派 `assignee` 时校验其存在；目录不可用（`Noop`）
/// 时跳过校验（保持当前宽松行为）。
#[async_trait]
pub trait AgentDirectory: Send + Sync {
    async fn agent_exists(&self, agent_id: &str) -> bool;

    /// 查 `instance_id` 对应的 Agent 元信息（包 ID + 显示名）。
    /// 找不到返回 `None`。`pm_get_project` 用它把 `ProjectMember` 投影成
    /// 包含 `agent_id` / `name` 的对象，让调用方（LLM）能从 agent name
    /// 反查到派任务需要的 `instance_id`。
    ///
    /// 默认实现返回 `None`（`Noop` 不持元信息；宽松模式仍允许派任务，
    /// 只是 `pm_get_project` 返回的成员不附 name/agent_id）。
    async fn agent_info(&self, _instance_id: &str) -> Option<AgentInfo> {
        None
    }
}

/// Agent 元信息（`AgentDirectory::agent_info` 返回）。
///
/// 字段刻意保持最少：`pm_get_project` 投影成员时只需要 `agent_id`（包 ID，
/// 用于展示/日志）+ `name`（显示名，用于 LLM 识别）+ `role`（来自
/// manifest 顶层 `role = "..."`，人类用户 / PM agent 派活时判断"这个
/// 实例适合干什么"）。`instance_id` 是查询 key，不重复放在 value 里。
///
/// **`role` 来源**：Gateway `AgentListResponse.role`（已从 `manifest.role`
/// 透出，ADR-073 唯一真相源 = Gateway `installed_agents`）。`HttpAgentDirectory`
/// 周期刷新时一并拉取并缓存；缺字段 / 未声明 → `None`，不报错。
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_id: String,
    pub name: String,
    /// Agent 角色（manifest 顶层 `role`，例：`"Senior Software Engineer"`）。
    /// Gateway 列表响应缺字段或 `.agent` 包未声明 → `None`。
    pub role: Option<String>,
}

/// 默认（宽松）Agent 目录：不校验存在性。`PmService::new` 未注入目录时使用。
pub struct NoopAgentDirectory;

#[async_trait]
impl AgentDirectory for NoopAgentDirectory {
    async fn agent_exists(&self, _agent_id: &str) -> bool {
        true
    }
}

/// 构建 MCP HTTP Server 路由（JSON-RPC，`POST /mcp`）。
///
/// 返回 `Router<()>`（内部 `.with_state(McpState)`），与 REST `pm_router`
/// 合并后由 Gateway `nest_service("/api/pm", ...)` 挂载。
pub fn mcp_router(store: Arc<TreePmStore>, agent_dir: Arc<dyn AgentDirectory>) -> Router {
    Router::new()
        .route("/mcp", post(jsonrpc_endpoint))
        .with_state(McpState { store, agent_dir })
}

// ── JSON-RPC 协议常量 ────────────────────────────────────────────────────

/// MCP 协议版本（与 acowork-mcp `McpClient::connect` 握手版本一致）。
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC 2.0 协议版本。
pub const JSONRPC_VERSION: &str = "2.0";

// JSON-RPC 标准错误码
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

// 自定义服务错误码（-32000..-32099 是协议保留的服务端错误区段）
const CODE_UNAUTHENTICATED: i32 = -32001;
const CODE_FORBIDDEN: i32 = -32002;

impl PmError {
    /// MCP JSON-RPC error code（协议级之外的业务错误映射）。
    ///
    /// - `Forbidden` / `Unauthenticated` → 自定义 -3200x，供客户端 `[permission]` 分类
    /// - 其余 `PmError` → -32603（内部错误）
    pub fn mcp_error_code(&self) -> i32 {
        match self {
            PmError::Unauthenticated(_) => CODE_UNAUTHENTICATED,
            PmError::Forbidden(_) => CODE_FORBIDDEN,
            _ => INTERNAL_ERROR,
        }
    }
}

// ── JSON-RPC 请求结构 ─────────────────────────────────────────────────────

/// 入站 JSON-RPC 请求（MCP 客户端 → 服务端）。
#[derive(Debug, serde::Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

// ── 端点 handler ──────────────────────────────────────────────────────────

/// `POST /mcp` —— JSON-RPC 单一入口。
///
/// 请求体：`{"jsonrpc":"2.0","id":1,"method":"...","params":{...}}`
/// 身份：`X-MCP-Actor` header（可选；缺失视为匿名）。
#[tracing::instrument(skip(state, body))]
pub async fn jsonrpc_endpoint(
    State(state): State<McpState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let actor = extract_actor(&headers);

    // 解析失败 → -32700 Parse error
    let text = match std::str::from_utf8(&body) {
        Ok(t) => t,
        Err(e) => {
            return rpc_error(None, PARSE_ERROR, format!("request body is not UTF-8: {e}"));
        }
    };
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return rpc_error(None, PARSE_ERROR, format!("parse error: {e}"));
        }
    };

    // 批处理不支持（客户端单发）→ -32600 Invalid Request
    if parsed.is_array() {
        return rpc_error(None, INVALID_REQUEST, "batch requests are not supported".into());
    }

    let req: JsonRpcRequest = match serde_json::from_value(parsed) {
        Ok(r) => r,
        Err(e) => return rpc_error(None, INVALID_REQUEST, format!("invalid request: {e}")),
    };

    if req.jsonrpc != JSONRPC_VERSION {
        return rpc_error(req.id.clone(), INVALID_REQUEST, "invalid jsonrpc version".into());
    }

    handle_request(state, actor, req).await
}

/// 从 header 提取调用方 agent_id（`X-MCP-Actor`）。缺失 → 匿名（None）。
fn extract_actor(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-mcp-actor")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn handle_request(state: McpState, actor: Option<String>, req: JsonRpcRequest) -> Response {
    let id = req.id.clone();

    match req.method.as_str() {
        "initialize" => {
            let result = json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "acowork-pm",
                    "version": env!("CARGO_PKG_VERSION")
                }
            });
            rpc_result(id, result)
        }

        // 客户端初始化完成通知——按协议不回响应（202 空体）
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),

        "tools/list" => {
            let tools = manifest::manifest_tools();
            rpc_result(id, json!({ "tools": tools }))
        }

        "tools/call" => {
            let params = req.params.clone().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string);
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(Value::Null);

            let Some(name) = name else {
                return rpc_error(id, INVALID_PARAMS, "tools/call requires a `name` field".into());
            };

            match tools::dispatch(&state, actor.as_deref(), &name, args).await {
                Ok(result) => {
                    // 规范化 MCP content 块：数据序列化为 JSON 文本
                    let text = serde_json::to_string(&result)
                        .unwrap_or_else(|_| "null".to_string());
                    let envelope = json!({
                        "content": [ { "type": "text", "text": text } ],
                        "isError": false
                    });
                    rpc_result(id, envelope)
                }
                Err(e) => {
                    // 业务错误 → JSON-RPC error，message 带错误码前缀
                    // 使客户端 [permission]/[transient] 分类生效
                    let msg = format!("{}: {}", e.error_code(), e);
                    rpc_error(id, e.mcp_error_code(), msg)
                }
            }
        }

        // 未知 method：通知（无 id）按协议静默；请求回 -32601
        other => {
            if id.is_none() {
                StatusCode::ACCEPTED.into_response()
            } else {
                rpc_error(id, METHOD_NOT_FOUND, format!("method not found: {other}"))
            }
        }
    }
}

// ── 响应构造 ──────────────────────────────────────────────────────────────

/// 成功响应：`{"jsonrpc":"2.0","id":..,"result":..}`（application/json）。
fn rpc_result(id: Option<Value>, result: Value) -> Response {
    let body = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// 错误响应：`{"jsonrpc":"2.0","id":..,"error":{code,message}}`。
fn rpc_error(id: Option<Value>, code: i32, message: String) -> Response {
    let body = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": { "code": code, "message": message },
    });
    (StatusCode::OK, Json(body)).into_response()
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PmConfig;
    use crate::store::tree::PmStore;
    use crate::types::{ProjectId, ProjectStatus, TaskId, UpdateProject};
    use axum::routing::get;
    use std::time::Duration;
    use tower::ServiceExt;

    /// 将响应体 `Bytes` 解析为 `Value`。
    async fn body_to_value(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    use axum::body::Body;

    /// 测试用最小状态：tempdir store + Noop AgentDirectory。
    pub(crate) async fn test_mcp_state() -> (McpState, tempfile::TempDir) {
        test_mcp_state_with_dir(Arc::new(NoopAgentDirectory)).await
    }

    /// 测试用最小状态（可注入自定义 Agent 目录）。
    pub(crate) async fn test_mcp_state_with_dir(
        agent_dir: Arc<dyn AgentDirectory>,
    ) -> (McpState, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = PmConfig {
            data_dir: tmp.path().to_path_buf(),
            index_rebuild_on_start: false,
            ..Default::default()
        };
        let store = Arc::new(TreePmStore::new(cfg).await.unwrap());
        (McpState { store, agent_dir }, tmp)
    }

    #[tokio::test]
    async fn initialize_handshake_returns_server_info() {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test"}}}"#,
            ))
            .unwrap();

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_value(resp).await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(v["result"]["serverInfo"]["name"], "acowork-pm");
    }

    #[tokio::test]
    async fn notification_initialized_returns_202_empty() {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            ))
            .unwrap();

        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn tools_list_returns_manifest_tools() {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            ))
            .unwrap();

        let resp = router.clone().oneshot(req).await.unwrap();
        let v = body_to_value(resp).await;
        let tools = v["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":3,"method":"bogus/method","params":{}}"#,
            ))
            .unwrap();

        let resp = router.clone().oneshot(req).await.unwrap();
        let v = body_to_value(resp).await;
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
    }

    // ── P3 e2e：全生命周期 + 鉴权 ─────────────────────────────────────

    /// 便捷：向 router 发一次 `tools/call`，返回完整 JSON-RPC 响应。
    async fn call_tool(
        router: &Router,
        actor: Option<&str>,
        id: u64,
        name: &str,
        args: Value,
    ) -> Value {
        let mut builder = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");
        if let Some(a) = actor {
            builder = builder.header("x-mcp-actor", a);
        }
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        let req = builder.body(Body::from(body.to_string())).unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        body_to_value(resp).await
    }

    /// 便捷：解析 `tools/call` 成功响应中的 `result.content[0].text`（JSON 文本）。
    fn tool_text(v: &Value) -> Value {
        let text = v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("content[0].text should be a JSON string, got: {v}"));
        serde_json::from_str(text).expect("tool result text should be valid JSON")
    }

    /// 建 router + 空状态（Noop AgentDirectory：assignee 存在性恒真）。
    async fn test_router() -> Router {
        let (state, _tmp) = test_mcp_state().await;
        Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state)
    }

    /// 建 router（可注入自定义 Agent 目录）。
    async fn test_router_with_dir(agent_dir: Arc<dyn AgentDirectory>) -> Router {
        let (state, _tmp) = test_mcp_state_with_dir(agent_dir).await;
        Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state)
    }

    /// 建 router + 暴露 store（注入自定义 Agent 目录），用于需要走
    /// `store.add_project_member(...)` 等 store 接口的前置准备。
    async fn test_router_store_with_dir(
        agent_dir: Arc<dyn AgentDirectory>,
    ) -> (Router, Arc<TreePmStore>) {
        let (state, _tmp) = test_mcp_state_with_dir(agent_dir).await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state.clone());
        (router, state.store)
    }

    /// 建 router + 暴露 store（供 human review / archive 等 MCP 之外的
    /// 前置状态构造，模拟人类审批侧）。
    async fn test_router_and_store() -> (Router, Arc<TreePmStore>) {
        let (state, _tmp) = test_mcp_state().await;
        let router = Router::new()
            .route("/mcp", post(jsonrpc_endpoint))
            .with_state(state.clone());
        (router, state.store)
    }

    /// 人类批准一个 Agent 创建的任务（`review_status: pending → approved`）。
    /// Agent 侧任何"建完就 claim / 改状态"的测试都必须先过这一关，否则撞审批闸门。
    async fn approve_creation(store: &TreePmStore, tid: &str) {
        store.review_task(&TaskId(tid.to_string()), true, "human", None).await.unwrap();
    }

    /// 白名单 Agent 目录（测试桩）：仅当 agent_id 在白名单内才返回 true。
    /// 用于验证 `pm_create_task` 的 assignee 存在性校验（设计 §9.1）。
    struct WhitelistAgentDirectory {
        allowed: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    }

    impl WhitelistAgentDirectory {
        fn new(ids: &[&str]) -> Self {
            Self {
                allowed: std::sync::Arc::new(std::sync::Mutex::new(
                    ids.iter().map(|s| s.to_string()).collect(),
                )),
            }
        }
    }

    #[async_trait::async_trait]
    impl AgentDirectory for WhitelistAgentDirectory {
        async fn agent_exists(&self, agent_id: &str) -> bool {
            self.allowed.lock().unwrap().contains(agent_id)
        }
    }

    /// 元信息 Agent 目录（测试桩）：同时支持存在性 + 元信息查询。
    /// 用于验证 `pm_get_project` 把 `instance_id` 投影成 `agent_id` + `name`。
    struct MapAgentDirectory {
        infos: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, AgentInfo>>>,
    }

    impl MapAgentDirectory {
        fn new(entries: &[(&str, &str, &str)]) -> Self {
            // entries: (instance_id, agent_id, name)
            // role 默认为 None — 仅当测试关心 role 时用 `with_role` / `set` 注入。
            let map = entries
                .iter()
                .map(|(iid, aid, n)| {
                    (
                        (*iid).to_string(),
                        AgentInfo {
                            agent_id: (*aid).to_string(),
                            name: (*n).to_string(),
                            role: None,
                        },
                    )
                })
                .collect();
            Self {
                infos: std::sync::Arc::new(std::sync::Mutex::new(map)),
            }
        }

        /// 测试用：注入带 role 的元信息条目，覆盖已存在的 instance_id。
        fn set(&self, instance_id: &str, info: AgentInfo) {
            self.infos
                .lock()
                .unwrap()
                .insert(instance_id.to_string(), info);
        }
    }

    #[async_trait::async_trait]
    impl AgentDirectory for MapAgentDirectory {
        async fn agent_exists(&self, instance_id: &str) -> bool {
            self.infos.lock().unwrap().contains_key(instance_id)
        }
        async fn agent_info(&self, instance_id: &str) -> Option<AgentInfo> {
            self.infos.lock().unwrap().get(instance_id).cloned()
        }
    }

    /// 断言 JSON-RPC 错误：返回 (code, message 前缀, 完整 message)。
    fn assert_rpc_error(v: &Value) -> (i32, String, String) {
        let msg = v["error"]["message"].as_str().unwrap_or_default().to_string();
        let prefix = msg
            .split_once(':')
            .map(|(p, _)| p.trim().to_string())
            .unwrap_or_else(|| msg.clone());
        (
            v["error"]["code"].as_i64().unwrap_or(0) as i32,
            prefix,
            msg,
        )
    }

    /// e2e：Agent 完整生命周期 —— create_project → create_task(assignee+due_at)
    /// → claim → submit → check。断言审核语义（agent 创建 → pending review，
    /// 人类批准前不可 claim）。
    #[tokio::test]
    async fn e2e_full_lifecycle_claim_submit_check() {
        let (router, store) = test_router_and_store().await;
        let agent = "agent-alpha";

        // 1. 创建项目
        let v = call_tool(
            &router,
            Some(agent),
            10,
            "pm_create_project",
            json!({ "title": "P1", "description": "desc" }),
        )
        .await;
        assert!(v["error"].is_null(), "create_project failed: {v}");
        let proj = tool_text(&v);
        let pid = proj["id"].as_str().unwrap().to_string();

        // 2. 创建任务（assignee = 自己 + due_at）
        let v = call_tool(
            &router,
            Some(agent),
            11,
            "pm_create_task",
            json!({
                "project_id": pid,
                "title": "T1",
                "description": "do it",
                "assignee": agent,
                "due_at": "2026-01-01T00:00:00Z",
            }),
        )
        .await;
        assert!(v["error"].is_null(), "create_task failed: {v}");
        let task = tool_text(&v);
        let tid = task["id"].as_str().unwrap().to_string();
        assert_eq!(task["status"], "pending");
        assert_eq!(task["review_status"], "pending"); // agent 创建 → pending 待人类审核
        assert_eq!(task["assignee"], agent);

        // 3. 人类尚未批准 → claim 409 review_not_approved
        let v = call_tool(&router, Some(agent), 12, "pm_claim_task", json!({ "task_id": tid }))
            .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(prefix, "review_not_approved", "unapproved claim msg: {msg}");
        assert_eq!(code, INTERNAL_ERROR, "unapproved claim code: {msg}");

        // 人类批准创建 → review_status=approved，status 仍为 pending
        approve_creation(&store, &tid).await;
        let v = call_tool(&router, Some(agent), 13, "pm_check_task", json!({ "task_id": tid }))
            .await;
        let chk = tool_text(&v);
        assert_eq!(chk["approved"], true, "approved after human review: {chk}");
        assert_eq!(chk["status"], "pending", "creation approval keeps status: {chk}");

        // 4. claim（assignee 本人，批准后放行）
        let v = call_tool(&router, Some(agent), 14, "pm_claim_task", json!({ "task_id": tid }))
            .await;
        assert!(v["error"].is_null(), "claim failed: {v}");
        assert_eq!(tool_text(&v)["status"], "in_progress");

        // 5. submit
        let v = call_tool(
            &router,
            Some(agent),
            15,
            "pm_submit_task",
            json!({ "task_id": tid, "text": "done" }),
        )
        .await;
        assert!(v["error"].is_null(), "submit failed: {v}");
        assert_eq!(tool_text(&v)["status"], "submitted");

        // 6. check（创建者可查；结果审核尚未发生，review_status 仍为已批准）
        let v = call_tool(&router, Some(agent), 16, "pm_check_task", json!({ "task_id": tid }))
            .await;
        assert!(v["error"].is_null(), "check failed: {v}");
        let chk = tool_text(&v);
        assert_eq!(chk["status"], "submitted");
        assert_eq!(chk["review_status"], "approved");
    }

    /// 鉴权 403：非 assignee 调用 claim/submit/update 一律拒绝。
    #[tokio::test]
    async fn e2e_non_assignee_mutation_forbidden() {
        let router = test_router().await;
        let owner = "agent-owner";
        let intruder = "agent-intruder";

        let v = call_tool(
            &router,
            Some(owner),
            20,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        let v = call_tool(
            &router,
            Some(owner),
            21,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": owner }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 非 assignee：claim / submit / update 均 403
        let v = call_tool(&router, Some(intruder), 22, "pm_claim_task", json!({ "task_id": tid }))
            .await;
        assert_eq!(v["error"]["code"], CODE_FORBIDDEN, "claim by non-assignee: {v}");

        let v = call_tool(
            &router,
            Some(intruder),
            23,
            "pm_submit_task",
            json!({ "task_id": tid, "text": "x" }),
        )
        .await;
        assert_eq!(v["error"]["code"], CODE_FORBIDDEN, "submit by non-assignee: {v}");

        let v = call_tool(
            &router,
            Some(intruder),
            24,
            "pm_update_task",
            json!({ "task_id": tid, "title": "hijack" }),
        )
        .await;
        assert_eq!(v["error"]["code"], CODE_FORBIDDEN, "update by non-assignee: {v}");

        // 无 assignee 的任务也无法被行动（设计 §9.2）
        let v = call_tool(
            &router,
            Some(owner),
            25,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T2" }),
        )
        .await;
        let tid2 = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(&router, Some(owner), 26, "pm_claim_task", json!({ "task_id": tid2 }))
            .await;
        assert_eq!(v["error"]["code"], CODE_FORBIDDEN, "claim unassigned: {v}");
    }

    /// 匿名只读：list/get 允许；写操作 / 自查工具要求身份（401 语义）。
    #[tokio::test]
    async fn e2e_anonymous_read_only_mutations_rejected() {
        let router = test_router().await;
        let agent = "agent-a";

        // 预置数据（以 agent 身份创建）
        let v = call_tool(
            &router,
            Some(agent),
            30,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            31,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": agent }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 匿名只读 OK
        for (name, args) in [
            ("pm_list_projects", json!({})),
            ("pm_get_project", json!({ "project_id": pid })),
            ("pm_list_tasks", json!({ "project_id": pid })),
            ("pm_get_task", json!({ "task_id": tid })),
        ] {
            let v = call_tool(&router, None, 32, name, args).await;
            assert!(v["error"].is_null(), "anonymous {name} should be allowed: {v}");
        }

        // 匿名写操作 → 未认证（CODE_UNAUTHENTICATED）
        for (name, args) in [
            ("pm_create_project", json!({ "title": "X" })),
            ("pm_create_task", json!({ "project_id": pid, "title": "X" })),
            ("pm_claim_task", json!({ "task_id": tid })),
            ("pm_submit_task", json!({ "task_id": tid, "text": "x" })),
            ("pm_update_task", json!({ "task_id": tid })),
        ] {
            let v = call_tool(&router, None, 33, name, args).await;
            assert_eq!(
                v["error"]["code"],
                CODE_UNAUTHENTICATED,
                "anonymous {name} should be rejected: {v}"
            );
        }

        // 匿名自查工具（list_my_tasks / check_task）也要身份
        let v = call_tool(&router, None, 34, "pm_list_my_tasks", json!({})).await;
        assert_eq!(v["error"]["code"], CODE_UNAUTHENTICATED);
        let v = call_tool(&router, None, 35, "pm_check_task", json!({ "task_id": tid })).await;
        assert_eq!(v["error"]["code"], CODE_UNAUTHENTICATED);
    }

    // ── P3 e2e 补齐：剩余工具 happy path + 边界场景 ───────────────────

    /// e2e：pm_update_task happy path —— 改 title/status/priority/assignee。
    #[tokio::test]
    async fn e2e_update_task_happy_path() {
        let (router, store) = test_router_and_store().await;
        let agent = "agent-updater";

        let v = call_tool(
            &router,
            Some(agent),
            40,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            41,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": agent }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 审批闸门：Agent 不得用 pm_update_task 绕过 claim 直接进 in_progress
        let v = call_tool(
            &router,
            Some(agent),
            42,
            "pm_update_task",
            json!({ "task_id": tid, "status": "in_progress" }),
        )
        .await;
        let (_, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(prefix, "review_not_approved", "update bypass msg: {msg}");
        approve_creation(&store, &tid).await;

        // 改 title + priority + status（pending → in_progress 合法）
        let v = call_tool(
            &router,
            Some(agent),
            43,
            "pm_update_task",
            json!({ "task_id": tid, "title": "renamed", "priority": "high", "status": "in_progress" }),
        )
        .await;
        assert!(v["error"].is_null(), "update failed: {v}");
        let up = tool_text(&v);
        assert_eq!(up["title"], "renamed");
        assert_eq!(up["priority"], "high");
        assert_eq!(up["status"], "in_progress");

        // 清空 assignee（assignee=null）
        let v = call_tool(
            &router,
            Some(agent),
            44,
            "pm_update_task",
            json!({ "task_id": tid, "assignee": null }),
        )
        .await;
        assert!(v["error"].is_null(), "clear assignee failed: {v}");
        assert_eq!(tool_text(&v)["assignee"], Value::Null);
    }

    /// e2e：pm_update_task 非法状态流转（done → in_progress 虽合法，但
    /// pending → submitted 非法）返回 400 invalid_state_transition。
    #[tokio::test]
    async fn e2e_update_task_invalid_transition() {
        let router = test_router().await;
        let agent = "agent-bad";

        let v = call_tool(
            &router,
            Some(agent),
            44,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            45,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": agent }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // pending → submitted 非法（必须先 claim）
        let v = call_tool(
            &router,
            Some(agent),
            46,
            "pm_update_task",
            json!({ "task_id": tid, "status": "submitted" }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "transition error msg: {msg}");
        assert_eq!(prefix, "invalid_state_transition", "unexpected: {msg}");
    }

    /// e2e：pm_reparent_task —— 移动到新父 + 提升根 + 防环 409。
    #[tokio::test]
    async fn e2e_reparent_task_happy_and_cycle() {
        let router = test_router().await;
        let agent = "agent-reparent";

        let v = call_tool(
            &router,
            Some(agent),
            47,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // A 根任务，B 为 A 的子任务
        let v = call_tool(
            &router,
            Some(agent),
            48,
            "pm_create_task",
            json!({ "project_id": pid, "title": "A", "assignee": agent }),
        )
        .await;
        let tid_a = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            49,
            "pm_create_task",
            json!({ "project_id": pid, "title": "B", "assignee": agent, "parent_task_id": tid_a }),
        )
        .await;
        let tid_b = tool_text(&v)["id"].as_str().unwrap().to_string();
        // B 深度 = 1，parent = A
        let v = call_tool(&router, Some(agent), 50, "pm_get_task", json!({ "task_id": tid_b }))
            .await;
        assert_eq!(tool_text(&v)["depth"], 1);
        assert_eq!(tool_text(&v)["parent_id"], tid_a);

        // 防环：把 A 移到自己的子任务 B 下 → cycle_detected 409（B 仍是 A 子树成员）
        let v = call_tool(
            &router,
            Some(agent),
            51,
            "pm_reparent_task",
            json!({ "task_id": tid_a, "new_parent": tid_b }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "cycle error msg: {msg}");
        assert_eq!(prefix, "cycle_detected", "unexpected: {msg}");
        // A 层级不变
        let v = call_tool(&router, Some(agent), 52, "pm_get_task", json!({ "task_id": tid_a }))
            .await;
        assert_eq!(tool_text(&v)["depth"], 0);

        // 提升 B 为根（new_parent=null）
        let v = call_tool(
            &router,
            Some(agent),
            53,
            "pm_reparent_task",
            json!({ "task_id": tid_b, "new_parent": null }),
        )
        .await;
        assert!(v["error"].is_null(), "reparent to root failed: {v}");
        let v = call_tool(&router, Some(agent), 54, "pm_get_task", json!({ "task_id": tid_b }))
            .await;
        assert_eq!(tool_text(&v)["depth"], 0);
        assert_eq!(tool_text(&v)["parent_id"], Value::Null);
    }

    /// e2e：pm_list_my_tasks —— Agent 自查指派给自己的任务 + status 过滤。
    #[tokio::test]
    async fn e2e_list_my_tasks_happy_path() {
        let (router, store) = test_router_and_store().await;
        let agent = "agent-self";
        let other = "agent-other";

        let v = call_tool(
            &router,
            Some(agent),
            54,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 联动指派：agent-other 也要是成员才能被指派
        store
            .add_project_member(&pid.parse().unwrap(), other, crate::types::MemberKind::Agent)
            .await
            .unwrap();

        // 两个给自己的任务 + 一个给别人的任务
        for i in 0..3 {
            let (title, assignee) = if i < 2 { ("mine", agent) } else { ("others", other) };
            let v = call_tool(
                &router,
                Some(agent),
                55,
                "pm_create_task",
                json!({ "project_id": pid, "title": title, "assignee": assignee }),
            )
            .await;
            assert!(v["error"].is_null(), "create task {i} failed: {v}");
        }

        // 自查：只返回自己的 2 个任务
        let v = call_tool(&router, Some(agent), 56, "pm_list_my_tasks", json!({})).await;
        let mine = tool_text(&v);
        assert_eq!(mine.as_array().unwrap().len(), 2, "my_tasks: {mine}");

        // status 过滤：把第一个 claim 后，pending 只剩 1 个
        let my_tid = mine[0]["id"].as_str().unwrap().to_string();
        approve_creation(&store, &my_tid).await;
        let v = call_tool(&router, Some(agent), 57, "pm_claim_task", json!({ "task_id": my_tid }))
            .await;
        assert!(v["error"].is_null(), "claim failed: {v}");
        let v = call_tool(
            &router,
            Some(agent),
            58,
            "pm_list_my_tasks",
            json!({ "status": "pending" }),
        )
        .await;
        let pending = tool_text(&v);
        assert_eq!(pending.as_array().unwrap().len(), 1, "pending my_tasks: {pending}");
    }

    /// e2e：pm_check_task 非创建者 403（设计 §6：仅创建者可查审核状态）。
    #[tokio::test]
    async fn e2e_check_task_non_creator_forbidden() {
        let router = test_router().await;
        let creator = "agent-creator";
        let other = "agent-other";

        let v = call_tool(
            &router,
            Some(creator),
            59,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(creator),
            60,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": creator }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // 非创建者 check → 403 forbidden
        let v = call_tool(&router, Some(other), 61, "pm_check_task", json!({ "task_id": tid }))
            .await;
        let (code, prefix, _msg) = assert_rpc_error(&v);
        assert_eq!(code, CODE_FORBIDDEN, "non-creator check should be forbidden");
        assert_eq!(prefix, "forbidden");

        // 创建者本人可查
        let v = call_tool(&router, Some(creator), 62, "pm_check_task", json!({ "task_id": tid }))
            .await;
        assert!(v["error"].is_null());
        assert_eq!(tool_text(&v)["id"], tid);
    }

    /// e2e：pm_create_task assignee 不在 Agent 目录 → 400 bad_request
    /// （设计 §9.1；用白名单目录验证存在性校验，ADR-073 按 instance_id）。
    #[tokio::test]
    async fn e2e_create_task_assignee_not_in_directory() {
        let router = test_router_with_dir(Arc::new(WhitelistAgentDirectory::new(&[
            "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d",
        ])))
        .await;
        let agent = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"; // ADR-073 instance_id
        let ghost = "5d2e1100-7e4b-4d2a-b6f1-1a91b07e4c2d"; // 不存在的 instance

        let v = call_tool(
            &router,
            Some(agent),
            63,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // assignee 不存在 → 400 bad_request（错误 message 提及 assignee）
        let v = call_tool(
            &router,
            Some(agent),
            64,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": ghost }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR);
        assert_eq!(prefix, "bad_request", "unexpected: {msg}");
        assert!(msg.contains("assignee instance not found"), "msg: {msg}");

        // assignee 存在于目录 → 成功
        let v = call_tool(
            &router,
            Some(agent),
            65,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T2", "assignee": agent }),
        )
        .await;
        assert!(v["error"].is_null(), "known assignee failed: {v}");
        assert_eq!(tool_text(&v)["assignee"], agent);
    }

    /// e2e：`pm_get_project` 返回的成员列表附 `agent_id` + `name`，让 LLM
    /// 拿到项目成员后能用 agent name 反查到 `instance_id` 用于派任务。
    /// （用户场景：人类说"把这个任务派给 Senior Engineer"，LLM 通过 `members`
    /// 找到 `name=Senior Engineer` 的 `instance_id`，再调 `pm_create_task`。）
    ///
    /// 覆盖两条路径：
    /// 1. 缓存命中：MapAgentDirectory 目录里有元信息 → 返回完整 agent_id/name
    /// 2. 缓存 miss / NoopAgentDirectory → `agent_id` / `name` 为 null（仍能拿到 instance_id）
    #[tokio::test]
    async fn e2e_get_project_returns_members_with_agent_id_and_name() {
        let creator = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";

        // ── 路径 1：MapAgentDirectory（缓存命中） ─────────────────────────
        let router = test_router_with_dir(Arc::new(MapAgentDirectory::new(&[(
            creator,
            "com.acowork.senior-engineer",
            "Senior Engineer",
        )])))
        .await;
        let v = call_tool(
            &router,
            Some(creator),
            90,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        assert!(v["error"].is_null(), "create_project failed: {v}");
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        let v = call_tool(
            &router,
            Some(creator),
            91,
            "pm_get_project",
            json!({ "project_id": pid }),
        )
        .await;
        assert!(v["error"].is_null(), "get_project failed: {v}");
        let resp = tool_text(&v);
        let members = resp["members"].as_array().expect("members is array");
        assert_eq!(members.len(), 1, "creator auto-joined");
        let m = &members[0];
        assert_eq!(m["instance_id"], creator);
        assert_eq!(m["agent_id"], "com.acowork.senior-engineer");
        assert_eq!(m["name"], "Senior Engineer");
        assert!(m["added_at"].is_string());

        // ── 路径 2：NoopAgentDirectory（元信息全 null） ───────────────────
        let router_noop = test_router_with_dir(Arc::new(NoopAgentDirectory)).await;
        let v = call_tool(
            &router_noop,
            Some(creator),
            92,
            "pm_create_project",
            json!({ "title": "P2" }),
        )
        .await;
        let pid2 = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router_noop,
            Some(creator),
            93,
            "pm_get_project",
            json!({ "project_id": pid2 }),
        )
        .await;
        let resp = tool_text(&v);
        let members = resp["members"].as_array().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0]["instance_id"], creator);
        assert!(members[0]["agent_id"].is_null());
        assert!(members[0]["name"].is_null());
    }

    /// PM task t-2ee347c3 — `pm_get_project` members 必须把 `role` 字段带出来。
    ///
    /// 人类用户在 PM UI 排任务 / PM agent 自身派活时，需要从 members 列表一眼
    /// 看出"这个实例适合干什么"（manifest 顶层 `role`，例：`"Senior Software
    /// Engineer"` / `"Project Manager"` / `"Product Manager"`），而不是只看
    /// display_name 猜。
    ///
    /// 三条路径必须各自正确：
    /// 1. **role 存在** → JSON `"role": "Senior Software Engineer"`（不为 null）
    /// 2. **role 缺失**（legacy manifest / 未声明） → JSON `"role": null`，
    ///    **不**省略字段（旧调用方 schema 不漂移；null = "未声明 / 不可用"
    ///    是契约稳定的最小面）
    /// 3. **`NoopAgentDirectory`**（宽松模式） → `"role": null`（与
    ///    `agent_id` / `name` 同语义）
    #[tokio::test]
    async fn e2e_get_project_members_include_role_field() {
        let creator = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        let member_with_role = "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b";
        let member_without_role = "b07e4c2d-34f8-4b91-b07e-4c2d34f8b91";

        // ── 路径 1 + 2：MapAgentDirectory 同时含 role 存在 / 缺失 ─────────
        let dir = Arc::new(MapAgentDirectory::new(&[
            (creator, "com.acowork.senior-engineer", "SSE"),
            (member_with_role, "com.acowork.project-manager", "PM"),
            (member_without_role, "com.acowork.legacy", "Legacy"),
        ]));
        // 仅 member_with_role 注入 role;member_without_role 保持 None
        dir.set(
            member_with_role,
            crate::mcp::AgentInfo {
                agent_id: "com.acowork.project-manager".to_string(),
                name: "PM".to_string(),
                role: Some("Project Manager".to_string()),
            },
        );
        // `pm_add_member` MCP tool 不存在 — 直接走 store API 加 member
        // （与 `e2e_meta_fields_attach_to_tasks_and_projects` 同模式）。
        let (router, store) = test_router_store_with_dir(dir.clone()).await;

        let v = call_tool(
            &router,
            Some(creator),
            100,
            "pm_create_project",
            json!({ "title": "P-role" }),
        )
        .await;
        assert!(v["error"].is_null(), "create_project failed: {v}");
        let pid_str = tool_text(&v)["id"].as_str().unwrap().to_string();
        let pid: ProjectId = pid_str.parse().expect("valid project id");

        store
            .add_project_member(&pid, member_with_role, crate::types::MemberKind::Agent)
            .await
            .expect("add member_with_role");
        store
            .add_project_member(&pid, member_without_role, crate::types::MemberKind::Agent)
            .await
            .expect("add member_without_role");

        let v = call_tool(
            &router,
            Some(creator),
            103,
            "pm_get_project",
            json!({ "project_id": pid_str }),
        )
        .await;
        assert!(v["error"].is_null(), "get_project failed: {v}");
        let resp = tool_text(&v);
        let members = resp["members"].as_array().expect("members array");

        // 三个 member（creator auto-join + 2 个 add_member）
        assert_eq!(members.len(), 3, "members = {members:?}");

        let by_id = |id: &str| -> &serde_json::Value {
            members
                .iter()
                .find(|m| m["instance_id"] == id)
                .unwrap_or_else(|| panic!("missing instance {id}"))
        };

        // role 存在 → 透传
        let m = by_id(member_with_role);
        assert_eq!(m["agent_id"], "com.acowork.project-manager");
        assert_eq!(m["name"], "PM");
        assert_eq!(
            m["role"], "Project Manager",
            "role must be the manifest value, not null"
        );

        // role 缺失 → null,不省略字段
        let m = by_id(member_without_role);
        assert_eq!(m["agent_id"], "com.acowork.legacy");
        assert_eq!(m["name"], "Legacy");
        assert!(
            m["role"].is_null(),
            "missing role must serialize as JSON null (key still present), got {}",
            m["role"]
        );
        assert!(
            m.as_object().unwrap().contains_key("role"),
            "role key must be present even when value is null — schema stability for old callers"
        );

        // creator（SSE）role 默认 None → null
        let m = by_id(creator);
        assert!(
            m["role"].is_null(),
            "creator injected without role must serialize as null"
        );

        // ── 路径 3：NoopAgentDirectory（元信息全 null） ─────────────────
        let router_noop = test_router_with_dir(Arc::new(NoopAgentDirectory)).await;
        let v = call_tool(
            &router_noop,
            Some(creator),
            110,
            "pm_create_project",
            json!({ "title": "P-noop" }),
        )
        .await;
        let pid2 = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router_noop,
            Some(creator),
            111,
            "pm_get_project",
            json!({ "project_id": pid2 }),
        )
        .await;
        let resp = tool_text(&v);
        let members = resp["members"].as_array().unwrap();
        assert!(!members.is_empty(), "creator auto-joined");
        for m in members {
            assert!(m["agent_id"].is_null());
            assert!(m["name"].is_null());
            assert!(
                m["role"].is_null(),
                "NoopAgentDirectory must serialize role as null, got {}",
                m["role"]
            );
        }
    }

    /// PM task t-2ee347c3 — Agent 的 `role` 在 manifest 改了之后,下一次
    /// `HttpAgentDirectory::refresh` 必须把新 role 透出来。
    ///
    /// 这条守的是缓存失效路径 —— 之前 role 根本没进 `AgentInfo`,
    /// 现在新增字段后必须保证 Gateway 改了 manifest.role(重装 / 升级),
    /// PM 周期刷新窗口(默认 60s)收敛后 `pm_get_project` 立即看到新值。
    #[tokio::test]
    async fn e2e_role_updated_after_periodic_refresh() {
        use crate::mcp::agent_dir::HttpAgentDirectory;

        let instance = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        // 第一次 refresh:Gateway 返回 role = "Project Manager"
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // 用 Arc<Mutex<...>> 让同一 listener 在两次 refresh 之间切换响应
        let current_role: std::sync::Arc<std::sync::Mutex<String>> =
            std::sync::Arc::new(std::sync::Mutex::new("Project Manager".to_string()));
        let cr1 = current_role.clone();
        let app = Router::new()
            .route(
                "/api/agents",
                get(move || {
                    let cr = cr1.clone();
                    async move {
                        let role = cr.lock().unwrap().clone();
                        axum::Json(vec![serde_json::json!({
                            "instance_id": instance,
                            "agent_id": "com.acowork.dispatcher",
                            "name": "Dispatcher",
                            "role": role,
                        })])
                    }
                }),
            )
            .route(
                "/api/agents/{id}",
                get(move || async move {
                    axum::Json(serde_json::json!({
                        "instance_id": instance,
                        "agent_id": "com.acowork.dispatcher",
                        "name": "Dispatcher",
                        "role": "Project Manager",
                    }))
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let gw = format!("http://127.0.0.1:{port}");

        let dir = Arc::new(HttpAgentDirectory::new(
            gw,
            None,
            Duration::from_secs(3600),
        ));
        dir.refresh().await;

        // 第一次断言:role = "Project Manager"
        let info = dir.agent_info(instance).await.expect("cached meta");
        assert_eq!(info.role.as_deref(), Some("Project Manager"));

        // 模拟 manifest 升级:role 改成 "Senior Software Engineer"
        *current_role.lock().unwrap() = "Senior Software Engineer".to_string();
        dir.refresh().await;

        // 第二次断言:role 已收敛(否则 PM 永远拿不到 manifest 升级后的 role)
        let info = dir.agent_info(instance).await.expect("cached meta after refresh");
        assert_eq!(
            info.role.as_deref(),
            Some("Senior Software Engineer"),
            "refresh must converge role changes"
        );
    }

    /// e2e：所有任务/项目返回都附 `assignee_meta` / `created_by_meta`，
    /// 让 LLM 在 `pm_list_tasks` / `pm_list_projects` 等列表接口中能用
    /// `*_meta.name` 识别谁是谁（不需要反查 members）。
    ///
    /// 覆盖的接口：`pm_list_projects` / `pm_list_tasks` / `pm_list_my_tasks`
    /// / `pm_get_task` / `pm_check_task` / `pm_create_project`。
    #[tokio::test]
    async fn e2e_meta_fields_attach_to_tasks_and_projects() {
        let creator = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        let worker = "a91b07e4-c2d3-4f8b-a91b-07e4c2d34f8b";
        let (router, store) = test_router_store_with_dir(Arc::new(MapAgentDirectory::new(&[
            (creator, "com.acowork.senior-engineer", "Senior Engineer"),
            (worker, "com.acowork.architect", "Architect"),
        ])))
        .await;

        // 建项目 + 把 worker 加为成员（项目成员校验要求 assignee ∈ members）
        let v = call_tool(
            &router,
            Some(creator),
            100,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let proj = tool_text(&v);
        let pid_str = proj["id"].as_str().unwrap().to_string();
        let pid: ProjectId = pid_str.parse().unwrap();
        store.add_project_member(&pid, worker, crate::types::MemberKind::Agent).await.unwrap();

        // pm_create_project 返回值带 created_by_meta
        assert_eq!(proj["created_by"], creator);
        assert_eq!(proj["created_by_meta"]["instance_id"], creator);
        assert_eq!(proj["created_by_meta"]["agent_id"], "com.acowork.senior-engineer");
        assert_eq!(proj["created_by_meta"]["name"], "Senior Engineer");

        let v = call_tool(
            &router,
            Some(creator),
            101,
            "pm_create_task",
            json!({ "project_id": pid_str, "title": "T1", "assignee": worker }),
        )
        .await;
        let task = tool_text(&v);
        let tid = task["id"].as_str().unwrap().to_string();

        // pm_create_task 返回值带 assignee_meta + created_by_meta
        assert_eq!(task["assignee"], worker);
        assert_eq!(task["assignee_meta"]["instance_id"], worker);
        assert_eq!(task["assignee_meta"]["agent_id"], "com.acowork.architect");
        assert_eq!(task["assignee_meta"]["name"], "Architect");
        assert_eq!(task["created_by"], creator);
        assert_eq!(task["created_by_meta"]["name"], "Senior Engineer");

        // pm_get_task 同样带 _meta
        let v = call_tool(
            &router,
            Some(creator),
            102,
            "pm_get_task",
            json!({ "task_id": tid }),
        )
        .await;
        let t = tool_text(&v);
        assert_eq!(t["assignee_meta"]["name"], "Architect");
        assert_eq!(t["created_by_meta"]["name"], "Senior Engineer");

        // pm_list_tasks：每条都带 _meta
        let v = call_tool(
            &router,
            Some(creator),
            103,
            "pm_list_tasks",
            json!({ "project_id": pid_str }),
        )
        .await;
        let list_tasks_text = tool_text(&v);
        let tasks = list_tasks_text.as_array().expect("list_tasks returns array");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["assignee_meta"]["name"], "Architect");
        assert_eq!(tasks[0]["created_by_meta"]["name"], "Senior Engineer");
        // 同时原 String 字段保留不变（前端 join agentStore 用）
        assert_eq!(tasks[0]["assignee"], worker);
        assert_eq!(tasks[0]["created_by"], creator);

        // pm_list_projects：每个 project 带 created_by_meta
        let v = call_tool(
            &router,
            Some(creator),
            104,
            "pm_list_projects",
            json!({}),
        )
        .await;
        let list_projects_text = tool_text(&v);
        let projs = list_projects_text.as_array().unwrap();
        assert!(projs.iter().any(|p| p["created_by_meta"]["name"] == "Senior Engineer"));

        // pm_list_my_tasks（worker 自查）：assignee_meta 是自己，created_by_meta 是 creator
        let v = call_tool(
            &router,
            Some(worker),
            105,
            "pm_list_my_tasks",
            json!({}),
        )
        .await;
        let my_text = tool_text(&v);
        let my = my_text.as_array().unwrap();
        assert_eq!(my.len(), 1);
        assert_eq!(my[0]["assignee"], worker);
        assert_eq!(my[0]["assignee_meta"]["name"], "Architect");
        assert_eq!(my[0]["created_by_meta"]["name"], "Senior Engineer");

        // pm_check_task（creator 自查）：带 created_by_meta
        let v = call_tool(
            &router,
            Some(creator),
            106,
            "pm_check_task",
            json!({ "task_id": tid }),
        )
        .await;
        let ct = tool_text(&v);
        assert_eq!(ct["created_by"], creator);
        assert_eq!(ct["created_by_meta"]["name"], "Senior Engineer");
    }

    /// e2e：未在 agent_dir 里的 instance_id / `"human"` 创建者 → `_meta` 为 null，
    /// 不阻塞返回。LLM 拿到 null 时知道元信息暂不可用但仍能拿到 raw ID。
    ///
    /// 用 `MapAgentDirectory` 注入"存在但无元信息"的代理（`agent_exists`
    /// 返回 true，`agent_info` 返回 None），模拟 Agent 已注册但元信息缺失
    /// （宽松目录默认行为）。
    #[tokio::test]
    async fn e2e_meta_null_when_agent_unknown() {
        let ghost = "5d2e1100-7e4b-4d2a-b6f1-1a91b07e4c2d";
        // ExistsButNoMetaDirectory: 存在校验通过，元信息返回 None
        struct ExistsButNoMetaDirectory;
        #[async_trait::async_trait]
        impl AgentDirectory for ExistsButNoMetaDirectory {
            async fn agent_exists(&self, _instance_id: &str) -> bool {
                true
            }
            async fn agent_info(&self, _instance_id: &str) -> Option<AgentInfo> {
                None
            }
        }
        let (router, store) =
            test_router_store_with_dir(Arc::new(ExistsButNoMetaDirectory)).await;

        // 人类创建（actor = "human"）：created_by_meta 应该为 null
        let v = call_tool(
            &router,
            Some("human"),
            110,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let proj = tool_text(&v);
        let pid_str = proj["id"].as_str().unwrap().to_string();
        let pid: ProjectId = pid_str.parse().unwrap();
        assert!(proj["created_by_meta"].is_null(), "human creator → null");
        // 加 ghost 为成员（assignee 必须是成员）
        store
            .add_project_member(&pid, ghost, crate::types::MemberKind::Agent)
            .await
            .unwrap();

        // 已知存在但元信息为 null 的 instance → assignee_meta 也为 null
        let v = call_tool(
            &router,
            Some("human"),
            111,
            "pm_create_task",
            json!({ "project_id": pid_str, "title": "T", "assignee": ghost }),
        )
        .await;
        let task = tool_text(&v);
        assert_eq!(task["assignee"], ghost);
        assert!(task["assignee_meta"].is_null(), "no meta → null");
        // created_by_meta 仍是 null（"human" 不在 agent_dir）
        assert!(task["created_by_meta"].is_null());
    }

    /// e2e：`"human"` 保留创建者哨兵短路——`lookup_agent_meta` / 批量查询
    /// 对 `"human"` **不发起**任何 `agent_info` 查询（否则每次项目列表/详情
    /// 都会对 Gateway 白打一次 `/api/agents/human`）。
    ///
    /// 用 `RecordingAgentDirectory`（记录所有被查询的 instance_id）验证
    /// human 创建的项目/任务在各接口下都没有触发查询。
    #[tokio::test]
    async fn e2e_human_creator_shortcircuits_agent_lookup() {
        struct RecordingAgentDirectory {
            queried: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        }
        impl RecordingAgentDirectory {
            fn new() -> (Self, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
                let queried = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                (Self {
                    queried: queried.clone(),
                }, queried)
            }
        }
        #[async_trait::async_trait]
        impl AgentDirectory for RecordingAgentDirectory {
            async fn agent_exists(&self, _instance_id: &str) -> bool {
                true
            }
            async fn agent_info(&self, instance_id: &str) -> Option<AgentInfo> {
                self.queried.lock().unwrap().push(instance_id.to_string());
                None
            }
        }

        let (dir, queried) = RecordingAgentDirectory::new();
        let (router, store) = test_router_store_with_dir(Arc::new(dir)).await;

        // 人类创建项目 → created_by = "human" → 不应查询 agent_dir
        let v = call_tool(
            &router,
            Some("human"),
            120,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let proj = tool_text(&v);
        let pid_str = proj["id"].as_str().unwrap().to_string();
        assert!(proj["created_by_meta"].is_null());
        assert!(
            queried.lock().unwrap().is_empty(),
            "human create_project → no agent lookup"
        );

        // 人类创建任务（assignee 为真实 instance）→ created_by 为 "human"
        // 不查询，assignee 正常查询
        let ghost = "5d2e1100-7e4b-4d2a-b6f1-1a91b07e4c2d";
        store
            .add_project_member(&pid_str.parse().unwrap(), ghost, crate::types::MemberKind::Agent)
            .await
            .unwrap();
        let v = call_tool(
            &router,
            Some("human"),
            121,
            "pm_create_task",
            json!({ "project_id": pid_str, "title": "T", "assignee": ghost }),
        )
        .await;
        let task = tool_text(&v);
        assert!(task["created_by_meta"].is_null());
        {
            let q = queried.lock().unwrap();
            assert_eq!(
                &*q,
                &vec![ghost.to_string()],
                "only assignee (real instance) looked up, never 'human'"
            );
        }

        // 项目详情 / 列表 → 成员为 "human"（creator）+ ghost（真实 instance）：
        // "human" 不查询，ghost 正常查询（get_project 查一次成员元信息）
        let v = call_tool(
            &router,
            Some("human"),
            122,
            "pm_get_project",
            json!({ "project_id": pid_str }),
        )
        .await;
        assert!(v["error"].is_null(), "get_project failed: {v}");
        // 列表：created_by = "human" → 批量查询被过滤 → 无新增查询
        let v = call_tool(
            &router,
            Some("human"),
            123,
            "pm_list_projects",
            json!({}),
        )
        .await;
        assert!(v["error"].is_null(), "list_projects failed: {v}");
        {
            let q = queried.lock().unwrap();
            // create_task 查 1 次（assignee=ghost）+ get_project 查 1 次（成员 ghost）
            assert_eq!(
                &*q,
                &vec![ghost.to_string(), ghost.to_string()],
                "'human' creator/member must never reach agent_dir"
            );
            assert!(
                !q.iter().any(|s| s == "human"),
                "sentinel 'human' must be short-circuited everywhere"
            );
        }
    }

    /// e2e：依赖阻塞 —— depends_on(Blocks) 未完成时 claim 409，依赖完成
    /// 后（人类 review → Done）可 claim。
    #[tokio::test]
    async fn e2e_dependency_blocks_claim() {
        let (router, store) = test_router_and_store().await;
        let agent = "agent-dep";

        let v = call_tool(
            &router,
            Some(agent),
            66,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();

        // A：前置依赖；B：依赖 A（Blocks）
        let v = call_tool(
            &router,
            Some(agent),
            67,
            "pm_create_task",
            json!({ "project_id": pid, "title": "A", "assignee": agent }),
        )
        .await;
        let tid_a = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            68,
            "pm_create_task",
            json!({
                "project_id": pid, "title": "B", "assignee": agent,
                "depends_on": [{ "task_id": tid_a, "kind": "blocks" }],
            }),
        )
        .await;
        let tid_b = tool_text(&v)["id"].as_str().unwrap().to_string();

        // B 被 A 阻塞 → is_blocked=true
        let v = call_tool(&router, Some(agent), 69, "pm_get_task", json!({ "task_id": tid_b }))
            .await;
        let b = tool_text(&v);
        assert_eq!(b["is_blocked"], true, "B should be blocked: {b}");
        assert_eq!(b["blocked_by"][0], tid_a);

        // 人类批准 A / B 的创建（否则先撞审批闸门，看不到依赖错误）
        approve_creation(&store, &tid_a).await;
        approve_creation(&store, &tid_b).await;

        // claim B → 409 dependency_not_satisfied
        let v = call_tool(&router, Some(agent), 70, "pm_claim_task", json!({ "task_id": tid_b }))
            .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "dep error msg: {msg}");
        assert_eq!(prefix, "dependency_not_satisfied", "unexpected: {msg}");

        // 完成 A：claim → submit → 人类 review approve → Done
        let store = store.clone();
        let tid_a_typed = TaskId(tid_a.clone());
        store.claim_task(&tid_a_typed, agent).await.unwrap();
        store
            .submit_task(&tid_a_typed, "done by agent", vec![], agent)
            .await
            .unwrap();
        store.review_task(&tid_a_typed, true, "human", None).await.unwrap();
        let v = call_tool(&router, Some(agent), 71, "pm_get_task", json!({ "task_id": tid_a }))
            .await;
        assert_eq!(tool_text(&v)["status"], "done", "A should be done: {v}");

        // B 不再被阻塞 → 可 claim
        let v = call_tool(&router, Some(agent), 72, "pm_claim_task", json!({ "task_id": tid_b }))
            .await;
        assert!(v["error"].is_null(), "claim B after dep done failed: {v}");
        assert_eq!(tool_text(&v)["status"], "in_progress");
    }

    /// e2e：404 —— project / task 不存在返回对应错误。
    #[tokio::test]
    async fn e2e_not_found() {
        let router = test_router().await;
        let agent = "agent-404";

        // project 不存在
        let v = call_tool(
            &router,
            Some(agent),
            72,
            "pm_get_project",
            json!({ "project_id": "p-missing123" }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "404 project msg: {msg}");
        assert_eq!(prefix, "project_not_found", "unexpected: {msg}");

        // task 不存在
        let v = call_tool(
            &router,
            Some(agent),
            73,
            "pm_get_task",
            json!({ "task_id": "t-missing123" }),
        )
        .await;
        let (code, prefix, msg) = assert_rpc_error(&v);
        assert_eq!(code, INTERNAL_ERROR, "404 task msg: {msg}");
        assert_eq!(prefix, "task_not_found", "unexpected: {msg}");
    }

    /// e2e：pm_list_tasks 过滤（status / assignee / limit）+ include_archived。
    #[tokio::test]
    async fn e2e_list_filters() {
        let (router, store) = test_router_and_store().await;
        let agent = "agent-filter";
        let other = "agent-other";

        // 项目 1：归档（通过 store 直接归档，模拟人类操作）
        let v = call_tool(
            &router,
            Some(agent),
            74,
            "pm_create_project",
            json!({ "title": "archived-proj" }),
        )
        .await;
        let pid_arch = tool_text(&v)["id"].as_str().unwrap().to_string();
        store
            .update_project(
                &crate::types::ProjectId(pid_arch.clone()),
                UpdateProject {
                    title: None,
                    description: None,
                    status: Some(ProjectStatus::Archived),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        // 项目 2：正常，2 个任务（1 个 assignee=agent）
        let v = call_tool(
            &router,
            Some(agent),
            75,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        // 联动指派：agent-other 也要是成员才能被指派
        store
            .add_project_member(&pid.parse().unwrap(), other, crate::types::MemberKind::Agent)
            .await
            .unwrap();
        for (i, (title, a)) in [("T1", agent), ("T2", other)].into_iter().enumerate() {
            let v = call_tool(
                &router,
                Some(agent),
                76,
                "pm_create_task",
                json!({ "project_id": pid, "title": title, "assignee": a }),
            )
            .await;
            assert!(v["error"].is_null(), "create task {i} failed: {v}");
        }

        // include_archived=false（默认）：归档项目不出现
        let v = call_tool(&router, None, 77, "pm_list_projects", json!({})).await;
        let projs = tool_text(&v);
        let titles: Vec<&str> = projs
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["title"].as_str().unwrap())
            .collect();
        assert!(!titles.contains(&"archived-proj"), "archived shown by default: {projs}");

        // include_archived=true：归档项目出现
        let v = call_tool(
            &router,
            None,
            78,
            "pm_list_projects",
            json!({ "include_archived": true }),
        )
        .await;
        let projs = tool_text(&v);
        assert!(
            projs.as_array().unwrap().iter().any(|p| p["title"] == "archived-proj"),
            "archived missing with include_archived: {projs}"
        );

        // list_tasks 过滤：assignee=agent 只有 1 个
        let v = call_tool(
            &router,
            None,
            79,
            "pm_list_tasks",
            json!({ "project_id": pid, "assignee": agent }),
        )
        .await;
        assert_eq!(tool_text(&v).as_array().unwrap().len(), 1);

        // limit=1
        let v = call_tool(
            &router,
            None,
            80,
            "pm_list_tasks",
            json!({ "project_id": pid, "limit": 1 }),
        )
        .await;
        assert_eq!(tool_text(&v).as_array().unwrap().len(), 1);
    }

    /// e2e：pm_reparent_task 匿名拒绝（§9.3 匿名仅只读）。
    #[tokio::test]
    async fn e2e_reparent_anonymous_rejected() {
        let router = test_router().await;
        let agent = "agent-rp";

        let v = call_tool(
            &router,
            Some(agent),
            81,
            "pm_create_project",
            json!({ "title": "P" }),
        )
        .await;
        let pid = tool_text(&v)["id"].as_str().unwrap().to_string();
        let v = call_tool(
            &router,
            Some(agent),
            82,
            "pm_create_task",
            json!({ "project_id": pid, "title": "T", "assignee": agent }),
        )
        .await;
        let tid = tool_text(&v)["id"].as_str().unwrap().to_string();

        let v = call_tool(
            &router,
            None,
            83,
            "pm_reparent_task",
            json!({ "task_id": tid, "new_parent": null }),
        )
        .await;
        assert_eq!(v["error"]["code"], CODE_UNAUTHENTICATED, "anonymous reparent: {v}");
    }
}
