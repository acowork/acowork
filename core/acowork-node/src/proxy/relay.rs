//! ADR-089 D2: `/relay/*` — the node-side landing pad of the Gateway's
//! LSP relay reverse proxy.
//!
//! The relay binds loopback (D1), so the Gateway cannot reach it
//! directly: every Desktop request to
//! `{gateway}/api/nodes/{id}/lsp/{tail}` arrives here as
//! `/relay/{tail}` and is forwarded to `http://127.0.0.1:{relay_port}/{tail}`.
//!
//! Same auth boundary as the Runtime reverse proxy: the inbound request
//! MUST carry `X-ACowork-Node-Token` ([`super::authorize`]) — the
//! Gateway injects it, nothing else on the LAN can use this route.
//!
//! Two transports:
//! - **HTTP** (servers / status / install / project-root): buffered
//!   forward, mirroring `proxy_agent`. Uses a dedicated client with no
//!   total timeout — `POST /api/lsp/install/{lang}` runs a script whose
//!   wall time is minutes, and the 30 s `runtime_http_client` timeout
//!   would cut it off (the relay enforces its own 60 s idle timeout).
//! - **WebSocket** (`/lsp/{language}`, the Monaco editor channel):
//!   terminated here and bridged message-by-message to the relay with
//!   tokio-tungstenite. The relay's `ws` handler is unchanged; this hop
//!   only forwards frames.

use axum::body::Body;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::HeaderValue;
use tokio_tungstenite::tungstenite;

use crate::state::NodeHttpState;

use super::{authorize, is_hop_by_hop_header};

#[axum::debug_handler]
/// Forward `/relay/{*rest}` to the loopback LSP relay.
pub(crate) async fn proxy_relay(
    State(state): State<NodeHttpState>,
    Path(rest): Path<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    // axum 0.8: `Result<_, _>` is the fallible-extractor form (the old
    // `Option<T>` extractor was removed). Err = not an upgrade request.
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
    body: axum::body::Bytes,
) -> Response {
    // Same node-token boundary as every other node-exposed route.
    if let Some(denied) = authorize(&state, &headers, "lsp-relay").await {
        return denied;
    }

    let relay_port = state
        .node
        .read()
        .await
        .lsp_relay_process
        .as_ref()
        .map(|p| p.port);
    let Some(port) = relay_port else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(HeaderName::from_static("x-error-origin"), HeaderValue::from_static("node"))],
            axum::Json(serde_json::json!({
                "error": "lsp relay not running on this node",
                "code": "relay_not_ready",
            })),
        )
            .into_response();
    };

    // WebSocket channel: only a genuine upgrade request gets bridged;
    // the extractor rejects plain HTTP and the match falls through.
    if let Ok(ws) = ws
        && method == Method::GET
    {
        let url = relay_ws_url(port, &rest, query.as_deref());
        return ws.on_upgrade(move |client| bridge_relay_ws(client, url));
    }

    let target_url = match &query {
        Some(q) => format!("http://127.0.0.1:{port}/{rest}?{q}"),
        None => format!("http://127.0.0.1:{port}/{rest}"),
    };

    let client = relay_http_client();
    let mut request = client.request(method, &target_url);
    for (name, value) in &headers {
        if !is_hop_by_hop_header(name) {
            request = request.header(name, value);
        }
    }
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }

    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();
            let mut builder = Response::builder().status(status);
            *builder.headers_mut().unwrap() = resp_headers;
            builder.body(Body::from(body)).unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build relay proxy response")
                    .into_response()
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %target_url, "Failed to proxy to LSP relay");
            (
                StatusCode::BAD_GATEWAY,
                [(HeaderName::from_static("x-error-origin"), HeaderValue::from_static("node"))],
                axum::Json(serde_json::json!({
                    "error": "failed to connect to LSP relay",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// `ws://127.0.0.1:{port}/{rest}?{query}` — the relay's WS endpoint.
fn relay_ws_url(port: u16, rest: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() => format!("ws://127.0.0.1:{port}/{rest}?{q}"),
        _ => format!("ws://127.0.0.1:{port}/{rest}"),
    }
}

/// Buffered-forward client for the relay.
///
/// Deliberately NOT `runtime_http_client`: the install-run endpoint
/// streams a script's output and can legitimately run for minutes;
/// only a connect timeout is applied here (the relay's own idle
/// timeout bounds the request).
fn relay_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("Failed to build node relay-proxy HTTP client")
    })
}

/// Bridge one client WebSocket (the Gateway's hop) to the relay.
///
/// Frames are forwarded verbatim in both directions; either side
/// closing ends both. A failed upstream connect simply drops the
/// already-upgraded client socket (the editor surfaces it as a
/// connection error and retries).
async fn bridge_relay_ws(client: WebSocket, url: String) {
    let upstream = match tokio_tungstenite::connect_async(&url).await {
        Ok((stream, _resp)) => stream,
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "LSP relay WS bridge: upstream connect failed");
            return;
        }
    };
    tracing::debug!(url = %url, "LSP relay WS bridge established");

    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    // client -> relay
    let to_relay = async {
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
    // relay -> client
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
                // Internal handshake frames; never part of the LSP
                // protocol — skip rather than tear down the bridge.
                tungstenite::Message::Frame(_) => continue,
            };
            if client_tx.send(om).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = to_relay => {}
        _ = to_client => {}
    }
    let _ = client_tx.close().await;
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
    fn relay_ws_url_forwards_querystring() {
        assert_eq!(
            relay_ws_url(19878, "lsp/rust", Some("workspace_root=%2Ftmp")),
            "ws://127.0.0.1:19878/lsp/rust?workspace_root=%2Ftmp"
        );
        assert_eq!(
            relay_ws_url(19878, "lsp/rust", None),
            "ws://127.0.0.1:19878/lsp/rust"
        );
        assert_eq!(
            relay_ws_url(19878, "lsp/rust", Some("")),
            "ws://127.0.0.1:19878/lsp/rust"
        );
    }

    #[test]
    fn frame_mapping_round_trips_text_and_binary() {
        let t = map_to_tungstenite(AxumMessage::Text("hello".into()));
        assert!(matches!(&t, tungstenite::Message::Text(s) if s.as_str() == "hello"));

        let b = map_to_tungstenite(AxumMessage::Binary(vec![1u8, 2, 3].into()));
        assert!(matches!(&b, tungstenite::Message::Binary(v) if v.as_ref() == [1u8, 2, 3]));
    }

    #[test]
    fn close_frame_maps_with_code_and_reason() {
        let c = map_to_tungstenite(AxumMessage::Close(Some(axum::extract::ws::CloseFrame {
            code: 1000,
            reason: "bye".into(),
        })));
        match c {
            tungstenite::Message::Close(Some(f)) => {
                assert_eq!(u16::from(f.code), 1000);
                assert_eq!(f.reason.as_str(), "bye");
            }
            _ => panic!("expected close frame"),
        }
    }

    // ── End-to-end bridge check ──────────────────────────────────────
    //
    // The two-transport forward (buffered HTTP + WS frame bridge) is
    // non-trivial logic; this is its one runnable check: a fake relay
    // behind the real node proxy router, exercised over the wire.

    use crate::config::NodeConfig;
    use crate::identity::{EnrollmentState, NodeIdentity};
    use crate::state::{NodeHttpState, NodeState};
    use crate::sidecar::lsp_relay::LspRelayProcessState;
    use axum::extract::ws::WebSocketUpgrade;
    use axum::routing::get;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Fake relay: `/health` answers JSON, `/lsp/{lang}` echoes one frame.
    fn fake_relay() -> axum::Router {
        async fn health() -> axum::Json<serde_json::Value> {
            axum::Json(serde_json::json!({ "status": "ok" }))
        }
        async fn echo(ws: WebSocketUpgrade) -> Response {
            ws.on_upgrade(|mut socket| async move {
                if let Some(Ok(msg)) = socket.next().await {
                    let _ = socket.send(msg).await;
                }
            })
        }
        axum::Router::new()
            .route("/health", get(health))
            .route("/lsp/{lang}", get(echo))
    }

    fn state_with_relay(port: u16, token: &str) -> NodeHttpState {
        let mut node = NodeState::new(16);
        node.lsp_relay_process = Some(LspRelayProcessState {
            pid: 0,
            port,
            ready: true,
        });
        NodeHttpState {
            node: Arc::new(RwLock::new(node)),
            config: NodeConfig::default(),
            identity: Arc::new(RwLock::new(NodeIdentity {
                node_id: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
                node_name: "node-1".to_string(),
                node_token: Some(token.to_string()),
                gateway_addr: None,
                enrollment: EnrollmentState::Enrolled,
                created_at: chrono::Utc::now(),
                enrolled_at: None,
            })),
            // Not under test here: an empty registry and a relay handle that
            // does not point at a live relay.
            lsp: crate::lsp_registry::for_tests(std::path::Path::new("."), None),
            lsp_relay: crate::state::LspRelayControl::detached("127.0.0.1", port),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_bridge_forwards_http_and_ws() {
        // Fake relay on loopback.
        let relay_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_port = relay_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(relay_listener, fake_relay()).await.unwrap();
        });

        // Node proxy in front of it.
        let app = crate::proxy::router(state_with_relay(relay_port, "t0k3n"));
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(proxy_listener, app).await.unwrap() });

        let client = reqwest::Client::new();

        // HTTP forward: missing token is refused at the node boundary.
        let resp = client
            .get(format!("http://127.0.0.1:{proxy_port}/relay/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // HTTP forward: token reaches the fake relay verbatim.
        let resp = client
            .get(format!("http://127.0.0.1:{proxy_port}/relay/health"))
            .header("X-ACowork-Node-Token", "t0k3n")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(json["status"], "ok");

        // WS bridge: one frame out, the same frame back through two hops.
        // Full handshake headers: tungstenite treats a pre-built Request
        // as the finished handshake (see Gateway lsp_proxy::ws_handshake_request).
        let url = tungstenite::handshake::client::Request::builder()
            .uri(format!("ws://127.0.0.1:{proxy_port}/relay/lsp/rust"))
            .header("Host", format!("127.0.0.1:{proxy_port}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", tungstenite::handshake::client::generate_key())
            .header("X-ACowork-Node-Token", "t0k3n")
            .body(())
            .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        ws.send(tungstenite::Message::Text("lsp-frame".into()))
            .await
            .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("WS bridge echo timed out")
            .unwrap()
            .unwrap();
        assert!(
            matches!(&reply, tungstenite::Message::Text(t) if t.as_str() == "lsp-frame"),
            "unexpected echo frame: {reply:?}"
        );
    }
}
