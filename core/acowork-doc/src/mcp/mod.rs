//! MCP (Model Context Protocol) HTTP Server — JSON-RPC over HTTP.
//!
//! Mirrors the acowork-pm MCP server (same protocol subset + error
//! mapping) so Agents get a uniform `doc_*` tool experience.
//!
//! ## Protocol
//!
//! Single `POST /mcp` endpoint, JSON-RPC 2.0:
//!
//! | JSON-RPC method | 说明 |
//! |-----------------|------|
//! | `initialize` | 握手：返回协议版本 + `capabilities.tools` + `serverInfo` |
//! | `notifications/initialized` | 客户端初始化完成通知（不回响应，202） |
//! | `tools/list` | 列出工具（见 [`manifest::DOC_TOOL_MANIFEST`]） |
//! | `tools/call` | 调用工具（见 [`tools::dispatch`]） |
//!
//! The server is **stateless** — every request authenticates on its own
//! via the `X-MCP-Actor` header (populated by the Gateway catalog with the
//! `{instance_id}` template, see Gateway `build_available_mcps`). The
//! header value is the runtime instance identity — a UUID (ADR-073),
//! not the package id (reverse-DNS).
//!
//! ## Layering (ADR-040 style)
//!
//! `tools.rs` dispatches to the **service traits** only (`DocumentService`,
//! `DirectoryService`, `RequestService`, `SearchService` via [`crate::state::DocState`]) —
//! never to `store`/`types` directly, same rule as the REST layer. The MCP
//! layer additionally maps human-readable `path` arguments (`项目A/纪要.md`)
//! onto the library's `dir_id` + filename addressing, because Agents do not
//! see internal `dir-*` ids.
//!
//! ## Security (design §9)
//!
//! - Anonymous (`X-MCP-Actor` absent): read-only tools only
//!   (`doc_list` / `doc_read` / `doc_stat` / `doc_search` /
//!   `doc_check_request`) — mutation tools return 403 `forbidden`.
//! - Writes carry the authenticated **runtime instance identity** (UUID,
//!   ADR-073) and record it as `submitted_by` /
//!   `ImportSource.instance_id`. The Gateway doc_proxy validates the
//!   header value against `installed_agents` (which is keyed by
//!   instance_id), so an attacker cannot forge it from the client side.
//! - **Governance is orthogonal to the verb** (`service::policy`): every
//!   content write goes through `doc_write`, and `gate_write` decides
//!   whether it applies now or becomes a `pending` review request.
//!   Structural operations (`doc_move` / `doc_remove`) are ownership-gated.
//! - Version concurrency (`if_version`) and the PR review flow are enforced
//!   by the service layer (design §5.4).
//!
//! ## Mounting
//!
//! `mcp_router(state)` returns `Router<()>` merged by
//! [`crate::server::DocService::router`] alongside the REST router. Public
//! endpoint: `http://{gw}/api/doc/mcp` (Gateway reverse proxy strips
//! `/api/doc` and forwards `X-MCP-Actor` only when the agent is installed).
//!
//! Design ref: `docs/design/zh/20-doc-online-document.md` §6 / §9.

pub mod manifest;
pub mod tools;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::api::ApiError;
use crate::error::DocError;
use crate::limit::transport_ceiling;
use crate::state::DocState;

/// MCP 协议版本（与 acowork-mcp 客户端握手版本一致）。
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// JSON-RPC 2.0 版本。
pub const JSONRPC_VERSION: &str = "2.0";

// JSON-RPC 标准错误码
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
// 自定义服务错误码（-32000.. 服务端错误区段）——客户端按 [permission] 分类
const CODE_FORBIDDEN: i32 = -32002;

impl DocError {
    /// MCP JSON-RPC error code（协议级之外的业务错误映射）。
    ///
    /// - `Forbidden` → -32002（客户端分类为 `[permission]`，不重试）
    /// - 其余 `DocError` → -32603（内部错误）
    pub fn mcp_error_code(&self) -> i32 {
        match self {
            DocError::Forbidden(_) => CODE_FORBIDDEN,
            _ => INTERNAL_ERROR,
        }
    }
}

/// 构建 MCP HTTP Server 路由（JSON-RPC，`POST /mcp`）。
pub fn mcp_router(state: DocState) -> Router {
    Router::new()
        .route("/mcp", post(jsonrpc_endpoint))
        .with_state(state)
}

/// 从 header 读取可信调用方 **runtime instance id**（UUID, ADR-073）。
///
/// 由 Gateway catalog 通过 `{instance_id}` 模板注入（仅在 agent 已安装且
/// `is_installed(instance_id)` 通过时才会被 doc_proxy 转发）；缺失或
/// 未通过校验 = 匿名（只读工具）。
///
/// 返回的字符串语义上是 *runtime instance*，不是 package id — 它必须能在
/// `state.actors` 等缓存里被当作 HashMap key 直接查表。
fn extract_actor(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-mcp-actor")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// 入站 JSON-RPC 请求（MCP 客户端 → 服务端）。
#[derive(Debug, serde::Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

/// `POST /mcp` —— JSON-RPC 单一入口。
///
/// 请求体：`{"jsonrpc":"2.0","id":1,"method":"...","params":{...}}`
/// 身份：`X-MCP-Actor` header（可选；缺失视为匿名只读）。
///
/// 请求体由本函数自己按 [`transport_ceiling`] 限长（而不是交给
/// `DefaultBodyLimit` 层）：工具参数里可能带着整篇文档内容，超限时必须
/// 回与 REST 相同的 `payload_too_large` JSON 信封，而不是 axum 的纯文本
/// 拒绝——否则调用方分不清「太大了」和「JSON 坏了」。
#[tracing::instrument(skip(state, req))]
pub async fn jsonrpc_endpoint(
    State(state): State<DocState>,
    headers: HeaderMap,
    req: Request,
) -> Response {
    let actor = extract_actor(&headers);

    let ceiling = transport_ceiling(state.config.max_doc_size_bytes);
    let body = match axum::body::to_bytes(req.into_body(), ceiling).await {
        Ok(b) => b,
        Err(_) => {
            return ApiError::from(DocError::PayloadTooLarge {
                size: None,
                max: state.config.max_doc_size_bytes,
            })
            .into_response();
        }
    };

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
    let request_id = parsed.get("id").cloned();

    // 批处理不支持（acowork 客户端单发）
    let req: JsonRpcRequest = match serde_json::from_value(parsed) {
        Ok(r) => r,
        Err(e) => {
            return rpc_error(request_id, INVALID_REQUEST, format!("invalid request: {e}"));
        }
    };

    if req.jsonrpc != JSONRPC_VERSION {
        return rpc_error(req.id, INVALID_REQUEST, "jsonrpc version must be 2.0".into());
    }

    handle_request(state, actor, req).await
}

/// 分发 JSON-RPC 方法到协议处理。
async fn handle_request(state: DocState, actor: Option<String>, req: JsonRpcRequest) -> Response {
    let id = req.id.clone();

    match req.method.as_str() {
        "initialize" => {
            let result = json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "acowork-doc",
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
                    // 业务错误 → JSON-RPC error，message 带错误码前缀使
                    // 客户端 [permission]/[permanent] 分类生效
                    let msg = format!("{}: {}", e.code(), e);
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

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    async fn raw(body: &str, actor: Option<&str>) -> (StatusCode, Value) {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = crate::config::DocConfig {
            data_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let state = DocState::new(cfg).await.unwrap();
        let router = mcp_router(state);
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");
        if let Some(a) = actor {
            builder = builder.header("x-mcp-actor", a);
        }
        let resp = router
            .oneshot(
                builder
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn call(name: &str, args: Value, actor: Option<&str>) -> Value {
        let payload = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        let (_status, value) = raw(&payload.to_string(), actor).await;
        value
    }

    /// 解析成功调用响应中的 `result.content[0].text`（JSON 文本）。
    fn content(value: &Value) -> Value {
        let text = value["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let (status, value) = raw(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["result"]["serverInfo"]["name"], "acowork-doc");
        assert_eq!(value["result"]["protocolVersion"], "2024-11-05");
    }

    #[tokio::test]
    async fn tools_list_returns_manifest() {
        let (status, value) = raw(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let tools = value["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 9);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "doc_list",
            "doc_read",
            "doc_stat",
            "doc_write",
            "doc_move",
            "doc_remove",
            "doc_check_request",
            "doc_mkdir",
            "doc_search",
        ] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let (status, value) = raw(
            r#"{"jsonrpc":"2.0","id":3,"method":"bogus","params":{}}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK); // JSON-RPC errors ride in 200 bodies
        assert_eq!(value["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn anonymous_write_tool_is_forbidden() {
        let value = call("doc_mkdir", json!({ "path": "新目录" }), None).await;
        assert_eq!(value["error"]["code"], CODE_FORBIDDEN, "{value}");
        assert!(
            value["error"]["message"].as_str().unwrap().contains("forbidden"),
            "{value}"
        );
    }

    #[tokio::test]
    async fn anonymous_read_tool_is_allowed() {
        let value = call("doc_list", json!({}), None).await;
        assert!(value.get("error").is_none(), "{value}");
        let payload = content(&value);
        let items = payload["items"].as_array().unwrap();
        assert!(items.is_empty());
    }
}

