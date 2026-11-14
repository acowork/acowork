//! ADR-089 D2: Gateway-side reverse proxy for the node-local LSP relay.
//!
//! `/api/nodes/{node_id}/lsp/{*tail}` → Node reverse proxy
//! `http://{node http_endpoint}/relay/{tail}` → node-loopback relay
//! `http://127.0.0.1:{relay_port}/{tail}`.
//!
//! Why a proxy instead of handing the relay's own address to the
//! Desktop: the relay binds loopback (D1 — reachability ≠ exposure), so
//! no client outside this machine can reach it directly; the Gateway is
//! the only principal that already holds a route to every node
//! (`node_http_endpoint`) *and* the node's token (`node_token_for`).
//!
//! Two transports, mirroring the node-side `proxy/relay.rs`:
//! - **HTTP** (servers / status / install / project-root): buffered
//!   forward. A dedicated client with no total timeout —
//!   `POST /api/lsp/install/{lang}` runs a script for minutes; the
//!   30 s `runtime_http_client` would cut it off.
//! - **WebSocket** (`/lsp/{language}`, the Monaco editor channel):
//!   terminated here, bridged frame-by-frame to the node's `/relay/*`
//!   with tokio-tungstenite. `proxy_to_node` cannot carry this: its
//!   reqwest client strips `upgrade`/`connection` as hop-by-hop.
//!
//! Auth: the route sits behind the Gateway's bearer gate like every
//! `/api/*` route (WS clients authenticate with `?access_token=`,
//! handled by `auth_middleware`), and additionally enforces ADR-087
//! node visibility (`can_view`) — a private node's relay must not be
//! reachable by a stranger merely guessing the node id. The
//! `X-ACowork-Node-Token` is injected here and is the node's only
//! auth boundary.

use axum::body::Body;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::HeaderValue;
use tokio_tungstenite::tungstenite;

use crate::gateway::ownership;
use crate::http::auth_middleware::AuthContext;
use crate::http::nodes_api::node_owner_rec;
use crate::http::proxy::{is_hop_by_hop_header, node_token_for};
use crate::http::routes::{ApiError, AppState};

pub fn lsp_proxy_routes() -> Router<AppState> {
    Router::new().route("/api/nodes/{id}/lsp/{*tail}", any(proxy_lsp_relay))
}

/// Gate result: everything the two transports need, resolved once.
struct RelayTarget {
    /// Node reverse-proxy base, e.g. `http://192.168.17.113:19900`.
    base: String,
    /// The node's HTTP auth token (fail-closed: absent → error).
    node_token: String,
}

/// ADR-089 D4 reason-coded failure for the proxy path — small enum
/// (clippy: `Response` in a `Result` Err is oversized), rendered as the
/// same JSON shape the endpoint probe documents (`{error, code}`).
#[derive(Debug)]
enum RelayFailure {
    /// Unknown node, or a private node outside the caller's circle —
    /// indistinguishable by design (no existence leak).
    NotFound,
    Offline,
    RelayNotReady,
    NotEnrolled,
}

fn relay_unavailable(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": message, "code": code })),
    )
        .into_response()
}

impl From<RelayFailure> for Response {
    fn from(f: RelayFailure) -> Response {
        match f {
            RelayFailure::NotFound => {
                ApiError::not_found("node not found").into_response()
            }
            RelayFailure::Offline => relay_unavailable(
                StatusCode::SERVICE_UNAVAILABLE,
                "node is offline",
                "node_offline",
            ),
            RelayFailure::RelayNotReady => relay_unavailable(
                StatusCode::SERVICE_UNAVAILABLE,
                "the LSP relay on this node is not ready",
                "relay_not_ready",
            ),
            RelayFailure::NotEnrolled => relay_unavailable(
                StatusCode::SERVICE_UNAVAILABLE,
                "node is not enrolled; cannot authenticate to its proxy",
                "node_not_enrolled",
            ),
        }
    }
}

#[allow(clippy::too_many_arguments)] // axum handler: extractor list, not an API
pub async fn proxy_lsp_relay(
    State(state): State<AppState>,
    Path((node_id, tail)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    auth: Option<Extension<AuthContext>>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
    body: axum::body::Bytes,
) -> Response {
    let target = match resolve_relay_target(&state, &node_id, auth.as_ref()).await {
        Ok(t) => t,
        Err(e) => return e.into(),
    };

    // WS only on a genuine GET upgrade; everything else is HTTP.
    if let Ok(ws) = ws
        && method == Method::GET
    {
        let url = relay_ws_url(&target.base, &tail, query.as_deref());
        return ws.on_upgrade(move |client| {
            bridge_relay_ws(client, url, target.node_token, node_id)
        });
    }

    let path = format!("/relay/{tail}");
    let target_url = match &query {
        Some(q) if !q.is_empty() => format!("{}{}?{}", target.base, path, q),
        _ => format!("{}{}", target.base, path),
    };

    let client = relay_proxy_client();
    let mut request = client.request(method, &target_url);
    for (name, value) in &headers {
        if !is_hop_by_hop_header(name) {
            request = request.header(name, value);
        }
    }
    request = request.header("X-ACowork-Node-Token", target.node_token.as_str());
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }

    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = response.headers().clone();
            let resp_body = response.bytes().await.unwrap_or_default();
            let mut builder = Response::builder().status(status);
            *builder.headers_mut().unwrap() = resp_headers;
            builder.body(Body::from(resp_body)).unwrap_or_else(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to build LSP relay proxy response",
                )
                    .into_response()
            })
        }
        Err(e) => {
            tracing::warn!(node_id = %node_id, url = %target_url, error = %e, "Failed to proxy to node LSP relay");
            (
                StatusCode::BAD_GATEWAY,
                [(
                    HeaderName::from_static("x-error-origin"),
                    HeaderValue::from_static("gateway"),
                )],
                Json(serde_json::json!({
                    "error": "failed to connect to node LSP relay",
                    "code": "relay_unreachable",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Validate caller + node state and resolve the forwarding target.
///
/// Failure codes (ADR-089 D4, same vocabulary the endpoint probe
/// returns): `404` unknown or invisible node, `503 node_offline`,
/// `503 relay_not_ready`, `503 node_not_enrolled`.
async fn resolve_relay_target(
    state: &AppState,
    node_id: &str,
    auth: Option<&Extension<AuthContext>>,
) -> Result<RelayTarget, RelayFailure> {
    let node = {
        let registry = match state.node_registry.as_ref() {
            Some(r) => r.clone(),
            None => return Err(RelayFailure::RelayNotReady),
        };
        let reg = registry.read().await;
        reg.get(node_id).cloned()
    };
    let Some(node) = node else {
        return Err(RelayFailure::NotFound);
    };

    // ADR-087: a private node is invisible to callers outside its
    // sharing circle — same predicate as `list_nodes`, checked here
    // because this route bypasses the list entirely. `auth == None`
    // is Local mode / machine actor: no visibility layer.
    if let Some(ctx) = auth {
        let rec = node_owner_rec(state, node_id);
        if !ownership::can_view(rec.as_ref(), &ctx.0.user_id, ctx.0.is_admin()) {
            return Err(RelayFailure::NotFound);
        }
    }

    if !node.online {
        return Err(RelayFailure::Offline);
    }
    if node.lsp_endpoint.is_none() {
        return Err(RelayFailure::RelayNotReady);
    }
    let base = node
        .info
        .as_ref()
        .map(|i| i.http_endpoint.clone())
        .filter(|ep| !ep.is_empty())
        .ok_or(RelayFailure::RelayNotReady)?;

    let node_token = node_token_for(state, node_id)
        .await
        .ok_or(RelayFailure::NotEnrolled)?;

    Ok(RelayTarget { base, node_token })
}

/// `ws://{host:port}/relay/{tail}?{query}` from the node's http base.
fn relay_ws_url(base: &str, tail: &str, query: Option<&str>) -> String {
    let ws_base = base
        .strip_prefix("http://")
        .map(|s| format!("ws://{s}"))
        .or_else(|| base.strip_prefix("https://").map(|s| format!("wss://{s}")))
        .unwrap_or_else(|| base.to_string());
    let ws_base = ws_base.trim_end_matches('/');
    match query {
        Some(q) if !q.is_empty() => format!("{ws_base}/relay/{tail}?{q}"),
        _ => format!("{ws_base}/relay/{tail}"),
    }
}

/// Long-lived forward client (install runs are minutes; connect-only
/// timeout, the relay's own idle timeout bounds the request).
///
/// `pub(crate)` because the ADR-091 registry proxy forwards the same
/// kind of call (`POST /lsp/servers/{name}/install` runs a script on the
/// node) and must not grow a second total-timeout client that would cut
/// a long install off mid-script.
pub(crate) fn relay_proxy_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            // Gateway -> Node hop. An ambient HTTP_PROXY (Clash, mitm, a
            // corporate proxy) must not carry it: these requests attach
            // internal credentials, which would leak to the proxy, and the
            // answer it gives describes the proxy, not the node.
            .no_proxy()
            .build()
            .expect("Failed to build LSP relay proxy HTTP client")
    })
}

/// Bridge the Desktop's WebSocket to the node's `/relay/*` endpoint.
async fn bridge_relay_ws(client: WebSocket, url: String, node_token: String, node_id: String) {
    let request = match ws_handshake_request(&url, &node_token) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(node_id = %node_id, error = %e, "LSP relay WS: bad upstream request line");
            return;
        }
    };
    let upstream = match tokio_tungstenite::connect_async(request).await {
        Ok((stream, _resp)) => stream,
        Err(e) => {
            tracing::warn!(node_id = %node_id, url = %url, error = %e, "LSP relay WS bridge: upstream connect failed");
            return;
        }
    };
    tracing::debug!(node_id = %node_id, url = %url, "LSP relay WS bridge established");

    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    let to_node = async {
        while let Some(msg) = client_rx.next().await {
            match msg {
                Ok(m) => {
                    if up_tx.send(map_to_tungstenite(m)).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };
    let to_client = async {
        while let Some(msg) = up_rx.next().await {
            let m = match msg {
                Ok(m) => m,
                Err(_) => break,
            };
            let om = match m {
                tungstenite::Message::Text(t) => AxumMessage::Text(t.to_string().into()),
                tungstenite::Message::Binary(b) => AxumMessage::Binary(b),
                tungstenite::Message::Ping(p) => AxumMessage::Ping(p),
                tungstenite::Message::Pong(p) => AxumMessage::Pong(p),
                tungstenite::Message::Close(c) => {
                    AxumMessage::Close(c.map(|c| axum::extract::ws::CloseFrame {
                        code: c.code.into(),
                        reason: c.reason.to_string().into(),
                    }))
                }
                tungstenite::Message::Frame(_) => continue,
            };
            if client_tx.send(om).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = to_node => {}
        _ = to_client => {}
    }
    let _ = client_tx.close().await;
}

/// Build the WS handshake request for the node hop.
///
/// tungstenite's `connect_async` treats a pre-built `Request` as the
/// *finished* handshake: the five mandatory headers (Host / Connection /
/// Upgrade / Sec-WebSocket-Version / Sec-WebSocket-Key) must be present
/// or `generate_request` fails with `InvalidHeader` — the URL-string
/// form that auto-generates them is not usable here because the node
/// token has to ride along.
fn ws_handshake_request(
    url: &str,
    node_token: &str,
) -> Result<tungstenite::handshake::client::Request, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: axum::http::Uri = url.parse()?;
    let host = parsed
        .authority()
        .map(|a| a.as_str().to_string())
        .ok_or("websocket relay URL has no authority")?;
    tungstenite::handshake::client::Request::builder()
        .uri(url)
        .header("Host", host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
        .header("X-ACowork-Node-Token", node_token)
        .body(())
        .map_err(Into::into)
}

/// axum frame -> tungstenite frame.
fn map_to_tungstenite(m: AxumMessage) -> tungstenite::Message {
    match m {
        AxumMessage::Text(s) => tungstenite::Message::Text(s.as_str().to_string().into()),
        AxumMessage::Binary(b) => tungstenite::Message::Binary(b),
        AxumMessage::Ping(p) => tungstenite::Message::Ping(p),
        AxumMessage::Pong(p) => tungstenite::Message::Pong(p),
        AxumMessage::Close(c) => tungstenite::Message::Close(c.map(|c| {
            tungstenite::protocol::CloseFrame {
                code: c.code.into(),
                reason: c.reason.to_string().into(),
            }
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_ws_url_converts_scheme_and_keeps_query() {
        assert_eq!(
            relay_ws_url("http://192.168.17.113:19900", "lsp/rust", Some("workspace_root=%2Ftmp")),
            "ws://192.168.17.113:19900/relay/lsp/rust?workspace_root=%2Ftmp"
        );
        assert_eq!(
            relay_ws_url("http://127.0.0.1:19900/", "api/lsp/servers", None),
            "ws://127.0.0.1:19900/relay/api/lsp/servers"
        );
    }

    /// The five mandatory handshake headers must be on the pre-built
    /// request — tungstenite refuses to generate them for the `Request`
    /// form (`InvalidHeader` at connect), which is exactly how the
    /// node hop failed in the first end-to-end run.
    #[test]
    fn ws_handshake_request_carries_mandatory_headers() {
        let req = ws_handshake_request("ws://192.168.1.5:19900/relay/lsp/rust", "tok").unwrap();
        for h in [
            "host",
            "connection",
            "upgrade",
            "sec-websocket-version",
            "sec-websocket-key",
        ] {
            assert!(req.headers().get(h).is_some(), "{h} missing");
        }
        assert_eq!(req.headers()["host"], "192.168.1.5:19900");
        assert_eq!(req.headers()["X-ACowork-Node-Token"], "tok");
        assert_eq!(req.uri().path(), "/relay/lsp/rust");
    }

    #[test]
    fn frame_mapping_round_trips_text_and_binary() {
        let t = map_to_tungstenite(AxumMessage::Text("hello".into()));
        assert!(matches!(&t, tungstenite::Message::Text(s) if s.as_str() == "hello"));
        let b = map_to_tungstenite(AxumMessage::Binary(vec![1u8, 2, 3].into()));
        assert!(matches!(&b, tungstenite::Message::Binary(v) if v.as_ref() == [1u8, 2, 3]));
    }
}
