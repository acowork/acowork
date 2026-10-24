//! MCP HTTP endpoint for the memory service (ADR-100) — JSON-RPC over HTTP.
//!
//! Same stateless protocol subset as acowork-doc / acowork-pm: one `POST /mcp`,
//! four methods, identity carried per request on `X-MCP-Actor`.
//!
//! ## Protocol
//!
//! | JSON-RPC method | purpose |
//! |-----------------|---------|
//! | `initialize` | handshake: protocol version + `capabilities.tools` + `serverInfo` |
//! | `notifications/initialized` | client done notifying — no response body (202) |
//! | `tools/list` | the tool manifest ([`manifest::MEMORY_TOOL_MANIFEST`]) |
//! | `tools/call` | `memory_store` / `memory_recall` ([`tools::dispatch`]) |
//!
//! ## Security
//!
//! Memory has **no anonymous tier**: the actor header is the instance
//! namespace key, so without it there is no store to read from or write to.
//! A missing actor is `Forbidden` (JSON-RPC `-32002`, classified
//! `[permission]` by the client, never retried); a header that cannot be used
//! as a directory name is rejected as invalid params (`-32602`). Forging is
//! prevented upstream — the Gateway `memory_proxy` only forwards the header
//! for an instance present in `installed_agents`.
//!
//! ## Mounting
//!
//! `mcp_router(state)` returns a `Router` merged into
//! [`crate::service::server::MemoryService::router`]. Public endpoint:
//! `http://{gw}/api/memory/mcp`.

pub mod manifest;
pub mod tools;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::service::error::{INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR};
use crate::service::state::MemoryState;

/// MCP protocol version (matches the acowork-mcp client handshake).
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// JSON-RPC version.
pub const JSONRPC_VERSION: &str = "2.0";

/// Build the MCP router (`POST /mcp`, JSON-RPC 2.0).
pub fn mcp_router(state: MemoryState) -> Router {
    Router::new()
        .route("/mcp", post(jsonrpc_endpoint))
        .with_state(state)
}

/// Read the trusted caller identity — the **runtime instance id** (UUID,
/// ADR-073) injected by the Gateway catalog's `{instance_id}` template.
/// Absent or empty means anonymous, which for memory means "no namespace".
fn extract_actor(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-mcp-actor")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Inbound JSON-RPC request (MCP client → server).
#[derive(Debug, serde::Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

/// `POST /mcp` — the single JSON-RPC entry point.
#[tracing::instrument(skip(state, body))]
pub async fn jsonrpc_endpoint(
    State(state): State<MemoryState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let actor = extract_actor(&headers);

    let text = match std::str::from_utf8(&body) {
        Ok(t) => t,
        Err(e) => return rpc_error(None, PARSE_ERROR, format!("request body is not UTF-8: {e}")),
    };
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return rpc_error(None, PARSE_ERROR, format!("parse error: {e}")),
    };
    let request_id = parsed.get("id").cloned();

    // Batching is not supported (the acowork client sends one request).
    let req: JsonRpcRequest = match serde_json::from_value(parsed) {
        Ok(r) => r,
        Err(e) => {
            return rpc_error(request_id, INVALID_REQUEST, format!("invalid request: {e}"));
        }
    };

    if req.jsonrpc != JSONRPC_VERSION {
        return rpc_error(
            req.id,
            INVALID_REQUEST,
            "jsonrpc version must be 2.0".into(),
        );
    }

    handle_request(state, actor, req).await
}

/// Route a JSON-RPC method to its handler.
async fn handle_request(
    state: MemoryState,
    actor: Option<String>,
    req: JsonRpcRequest,
) -> Response {
    let id = req.id.clone();

    match req.method.as_str() {
        "initialize" => rpc_result(
            id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "acowork-memory",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),

        // Client finished its initialization notification — no response (202).
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),

        "tools/list" => rpc_result(id, json!({ "tools": manifest::manifest_tools() })),

        "tools/call" => {
            let params = req.params.clone().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string);
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);

            let Some(name) = name else {
                return rpc_error(
                    id,
                    INVALID_PARAMS,
                    "tools/call requires a `name` field".into(),
                );
            };

            match tools::dispatch(&state, actor.as_deref(), &name, args).await {
                Ok(text) => {
                    // The tool's own text is the content block — an Agent
                    // reads exactly what it read from the builtin tool.
                    let envelope = json!({
                        "content": [ { "type": "text", "text": text } ],
                        "isError": false
                    });
                    rpc_result(id, envelope)
                }
                Err(e) => rpc_error(id, e.mcp_error_code(), e.mcp_message()),
            }
        }

        // Unknown method: notifications (no id) are ignored, requests -32601.
        other => {
            if id.is_none() {
                StatusCode::ACCEPTED.into_response()
            } else {
                rpc_error(id, METHOD_NOT_FOUND, format!("method not found: {other}"))
            }
        }
    }
}

/// Success: `{"jsonrpc":"2.0","id":..,"result":..}`.
fn rpc_result(id: Option<Value>, result: Value) -> Response {
    let body = json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// Error: `{"jsonrpc":"2.0","id":..,"error":{code,message}}`.
///
/// JSON-RPC errors ride in a `200` body — the MCP client reads the `error`
/// object, not the status line.
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
    use crate::service::error::CODE_FORBIDDEN;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    const INSTANCE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    async fn raw(body: &str, actor: Option<&str>) -> (StatusCode, Value) {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = crate::service::config::MemoryServiceConfig {
            data_dir: tmp.path().to_path_buf(),
            // Keep the protocol tests off the network: with a backend installed
            // the store path embeds its statement, which would depend on whether
            // something is listening on the default sidecar port.
            embedding: crate::service::config::EmbeddingSection {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let state = MemoryState::new(config).unwrap();
        let router = mcp_router(state);
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");
        if let Some(a) = actor {
            builder = builder.header("x-mcp-actor", a);
        }
        let resp = router
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
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

    /// Unwrap a successful call's content text.
    fn content(value: &Value) -> &str {
        value["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let (status, value) = raw(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["result"]["serverInfo"]["name"], "acowork-memory");
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
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"memory_store"));
        assert!(names.contains(&"memory_recall"));
    }

    #[tokio::test]
    async fn initialized_notification_is_accepted_without_body() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = crate::service::config::MemoryServiceConfig {
            data_dir: tmp.path().to_path_buf(),
            embedding: crate::service::config::EmbeddingSection {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let router = mcp_router(MemoryState::new(config).unwrap());
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error() {
        let (status, value) = raw(r#"{"jsonrpc":"2.0",,"#, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["error"]["code"], PARSE_ERROR);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let (status, value) = raw(
            r#"{"jsonrpc":"2.0","id":3,"method":"bogus","params":{}}"#,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn anonymous_tool_call_is_forbidden() {
        let value = call(
            "memory_store",
            json!({ "content": "x", "category": "fact" }),
            None,
        )
        .await;
        assert_eq!(value["error"]["code"], CODE_FORBIDDEN, "{value}");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not_authorized"),
            "{value}"
        );
    }

    #[tokio::test]
    async fn traversal_actor_is_invalid_params() {
        let value = call("memory_recall", json!({ "query": "x" }), Some("../escape")).await;
        assert_eq!(value["error"]["code"], INVALID_PARAMS, "{value}");
    }

    #[tokio::test]
    async fn tool_call_returns_the_builtin_text() {
        let value = call(
            "memory_store",
            json!({ "content": "User prefers metric units", "category": "preference" }),
            Some(INSTANCE),
        )
        .await;
        assert!(value.get("error").is_none(), "{value}");
        assert_eq!(
            content(&value),
            "Stored episode: \"User prefers metric units\" (confidence: 0.70, subtype: Preference)"
        );
        assert_eq!(value["result"]["isError"], false);
    }

    #[tokio::test]
    async fn validation_failure_message_keeps_the_code_prefix() {
        let value = call("memory_recall", json!({}), Some(INSTANCE)).await;
        assert_eq!(value["error"]["code"], INVALID_PARAMS);
        assert_eq!(
            value["error"]["message"].as_str().unwrap(),
            "bad_request: Provide at least 'query' (keywords) or time range ('since'/'until')"
        );
    }
}
