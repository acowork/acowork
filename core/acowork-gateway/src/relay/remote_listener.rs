//! Remote-origin loopback listener (design doc 24 §7.2, M3).
//!
//! The second Gateway HTTP server, bound to `127.0.0.1:{relay.
//! remote_http_port}`. It is the ONLY thing the relay tunnel client
//! forwards inbound tunnel streams to, which makes "traffic arrived on
//! this listener" the trust anchor for "this request came from the
//! public internet through the relay" — the origin is carried by the
//! listener identity itself, not by any proxy header an attacker could
//! forge.
//!
//! What is different from the main `:19876` listener:
//!
//! - [`remote_origin_guard`] layers an allow/deny policy on top of the
//!   otherwise-identical router: debug endpoints, Gateway config
//!   writes and fs browsing answer **404** (their existence is not
//!   revealed), and a request bearing `X-ACowork-Node-Token` is
//!   rejected outright — internal machine identities may not be
//!   asserted over the remote channel.
//! - `/mqtt` is a WebSocket↔TCP bridge to the strict MQTT listener
//!   (`:19874`), where remote Desktop/Mobile clients CONNECT with user
//!   access tokens (§7.2). The route is added AFTER the main router's
//!   layers, so the HTTP bearer gate does not apply — MQTT carries its
//!   own CONNECT authentication, enforced by the strict broker handler.
//!   The same bridge is also mounted on the MAIN listener for LAN
//!   clients ([`lan_mqtt_bridge`], which additionally re-checks the
//!   peer-IP allowlist the outer layer would otherwise have applied).
//!
//! Everything else — including the auth middleware semantics — is
//! byte-for-byte the main router, so a route added later is remotely
//! reachable by default and must consciously be added to the guard's
//! deny list instead (fail-open at the HTTP surface, fail-closed at
//! authentication).

use std::net::SocketAddr;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::error::GatewayError;

/// Header carrying the Node machine identity (ADR-055 Phase 5a). Never
/// legitimate on the remote surface.
const NODE_TOKEN_HEADER: &str = "x-acowork-node-token";

// ── Origin policy ───────────────────────────────────────────────────

/// Decision of the remote-origin guard for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDecision {
    /// No remote-specific objection — proceed into the normal router
    /// (auth middleware included).
    Allow,
    /// The path/method combination does not exist as far as the remote
    /// surface is concerned. A plain 404 — no detail, no hint that the
    /// route exists on the local listener.
    NotFound,
    /// The request tried to assert an identity that may not travel the
    /// remote channel at all.
    Forbidden,
}

/// Pure policy table for the remote listener (§7.2). Kept separate
/// from the middleware so the deny list is unit-testable in one place.
///
/// Deny list:
/// - `/api/debug/*` — broker debug/restart control (any method).
/// - `/api/fs/browse` — node-local filesystem listing (F5).
/// - `/api/relay/enable|disable` — Gateway config writes (they persist
///   `[relay]` to gateway.toml; remote users manage nothing).
/// - `PUT /api/config` — Gateway config write. `GET` stays available
///   (read-only view, same information a remote Desktop needs locally).
/// - `DELETE /api/logs` — destructive log maintenance.
///
/// Anything else flows into the SAME auth middleware as the local
/// listener — remote requests are authenticated users, not trusted
/// operators.
pub fn remote_origin_policy(path: &str, method: &str, has_node_token: bool) -> RemoteDecision {
    if has_node_token {
        return RemoteDecision::Forbidden;
    }
    if path.starts_with("/api/debug/") {
        return RemoteDecision::NotFound;
    }
    if path == "/api/fs/browse" {
        return RemoteDecision::NotFound;
    }
    if matches!(path, "/api/relay/enable" | "/api/relay/disable") && method == "POST" {
        return RemoteDecision::NotFound;
    }
    if path == "/api/config" && method != "GET" {
        return RemoteDecision::NotFound;
    }
    if path == "/api/logs" && method == "DELETE" {
        return RemoteDecision::NotFound;
    }
    RemoteDecision::Allow
}

/// axum middleware form of [`remote_origin_policy`].
pub async fn remote_origin_guard(req: Request, next: Next) -> Response {
    let has_node_token = req.headers().contains_key(NODE_TOKEN_HEADER);
    let decision = remote_origin_policy(
        req.uri().path(),
        req.method().as_str(),
        has_node_token,
    );
    match decision {
        RemoteDecision::Allow => next.run(req).await,
        // Deliberately bodiless/plain: the remote surface must not
        // confirm that these routes exist anywhere.
        RemoteDecision::NotFound => StatusCode::NOT_FOUND.into_response(),
        RemoteDecision::Forbidden => (
            StatusCode::FORBIDDEN,
            "internal machine identity cannot be used over the remote channel",
        )
            .into_response(),
    }
}

// ── MQTT-over-WebSocket bridge ──────────────────────────────────────

/// Bridge one WebSocket connection to the strict MQTT listener's TCP
/// port (§7.2): binary WS frames ↔ raw MQTT bytes. MQTT CONNECT
/// authentication (user access token) happens inside the tunnelled
/// protocol, enforced by the strict broker handler — this bridge is a
/// byte pipe by design, exactly like the relay itself.
async fn bridge_mqtt(ws: WebSocket, target: SocketAddr) {
    let tcp = match tokio::net::TcpStream::connect(target).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, target = %target, "mqtt bridge: strict listener unreachable");
            return;
        }
    };
    tracing::debug!(target = %target, "mqtt bridge: connection established");
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();
    let mut buf = [0u8; 16 * 1024];

    loop {
        tokio::select! {
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    if tcp_tx.write_all(&data).await.is_err() {
                        break;
                    }
                }
                // WS-layer keepalive is transparent noise; tungstenite
                // answers pings itself on the next send.
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                // Text frames are a protocol violation on this bridge
                // (MQTT-over-WS is binary-only).
                Some(Ok(Message::Text(_))) => break,
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
            },
            read = tcp_rx.read(&mut buf) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_tx
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            },
        }
    }
    let _ = ws_tx.close().await;
    tracing::debug!(target = %target, "mqtt bridge: connection closed");
}

// ── LAN MQTT-over-WebSocket bridge ──────────────────────────────────

/// Peer-IP gate for the LAN bridge: an empty allowlist allows every peer
/// (same rule as [`crate::security::IpAllowlist::allows`]), otherwise the
/// peer must match one entry.
///
/// Kept out of the handler so the deny path is unit-testable — the
/// allowlist admits loopback unconditionally, so a loopback test
/// connection can never exercise it.
fn lan_bridge_allowed(allowlist: &crate::security::IpAllowlist, peer: SocketAddr) -> bool {
    allowlist.is_empty() || allowlist.allows_socket(peer)
}

/// `GET /mqtt` mounted on the MAIN (LAN) listener.
///
/// The same byte pipe to the same strict listener, reachable from a device on
/// the local network: a mobile WebView cannot open a raw TCP socket and the
/// embedded broker binds loopback, so without this bridge a LAN client has no
/// MQTT path at all and must poll.
///
/// Authentication is unchanged — the CONNECT packet inside the tunnel carries
/// `user:{name}:mobile:{id}` plus a user access token and is verified by the
/// strict broker handler (`mqtt::check_remote_connect_auth`), which FAILS
/// CLOSED while the user service has not published its verifier. A Gateway
/// running without the account system therefore simply refuses the bridge and
/// the client stays on HTTP polling.
///
/// Like the relay route this is added AFTER `build_router`, so it does not
/// pass the HTTP bearer gate (MQTT authenticates itself). That also puts it
/// outside the peer-IP allowlist layer, which is therefore enforced here.
pub(crate) async fn lan_mqtt_bridge(
    ws: WebSocketUpgrade,
    peer: SocketAddr,
    target_port: u16,
    allowlist: crate::security::IpAllowlist,
) -> Response {
    if !lan_bridge_allowed(&allowlist, peer) {
        tracing::warn!(
            peer = %peer.ip(),
            "blocked by security.allowed_node_ips (LAN MQTT bridge)"
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    let target = SocketAddr::from(([127, 0, 0, 1], target_port));
    ws.protocols(["mqtt"])
        .on_upgrade(move |socket| bridge_mqtt(socket, target))
        .into_response()
}

// ── Listener lifecycle ──────────────────────────────────────────────

/// Start the remote-origin HTTP listener on `127.0.0.1:http_port`.
///
/// The router is the main router (same routes, same auth middleware)
/// plus the `/mqtt` bridge route, wrapped in [`remote_origin_guard`].
/// Runs until the process exits; the returned task is intentionally
/// detached (the listener is loopback-only infrastructure, not a
/// service the caller supervises).
///
/// Binds loopback only — the relay tunnel client (in this process) is
/// the only intended consumer. A bind failure is logged and returned
/// as an error but MUST NOT abort the main Gateway startup: remote
/// access is an additive path (design doc 24 §9 "rollback design").
pub async fn start_remote_http_listener(
    state: crate::http::routes::AppState,
    http_port: u16,
    mqtt_port: u16,
) -> Result<(), GatewayError> {
    let addr = SocketAddr::from(([127, 0, 0, 1], http_port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| GatewayError::Config(format!("remote listener bind {addr}: {e}")))?;

    let mqtt_target = SocketAddr::from(([127, 0, 0, 1], mqtt_port));
    // The `/mqtt` route is added AFTER `build_router` returns, so the
    // main router's layer stack (auth middleware included) does NOT
    // wrap it — MQTT authenticates at CONNECT, inside the bridged
    // protocol. The remote-origin guard is layered last and therefore
    // sees every request, including `/mqtt`.
    let app = crate::http::routes::build_router(state)
        .route(
            "/mqtt",
            get(move |ws: WebSocketUpgrade| {
                let target = mqtt_target;
                async move {
                    // Echo the `mqtt` WebSocket subprotocol. rumqttc's
                    // Ws/Wss transports send `Sec-WebSocket-Protocol:
                    // mqtt` and REJECT the handshake unless the server
                    // echoes it back (rumqttc `validate_response_headers`
                    // → SubprotocolHeaderMissing). Axum only selects a
                    // subprotocol when `.protocols()` is called — without
                    // this, every rumqttc-based client (the M4 Desktop
                    // relay path) fails the upgrade. Caught by the M6
                    // full-path e2e; the M3 e2e used a bare tungstenite
                    // client without a subprotocol, so it passed either way.
                    ws.protocols(["mqtt"])
                        .on_upgrade(move |socket| bridge_mqtt(socket, target))
                }
            }),
        )
        .layer(axum::middleware::from_fn(remote_origin_guard));

    tracing::info!(
        http = %addr,
        mqtt_bridge = %mqtt_target,
        "remote-origin HTTP listener started (design doc 24 §7.2)"
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .map_err(|e| GatewayError::Config(format!("remote listener serve: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_blocks_debug_fs_and_config_writes() {
        // Debug endpoints — any method.
        assert_eq!(
            remote_origin_policy("/api/debug/mqtt/shutdown", "POST", false),
            RemoteDecision::NotFound
        );
        assert_eq!(
            remote_origin_policy("/api/debug/mqtt/start", "POST", false),
            RemoteDecision::NotFound
        );
        // fs browsing (F5).
        assert_eq!(
            remote_origin_policy("/api/fs/browse", "GET", false),
            RemoteDecision::NotFound
        );
        // Relay management writes (persist gateway.toml).
        assert_eq!(
            remote_origin_policy("/api/relay/enable", "POST", false),
            RemoteDecision::NotFound
        );
        assert_eq!(
            remote_origin_policy("/api/relay/disable", "POST", false),
            RemoteDecision::NotFound
        );
        // Gateway config write; read stays available.
        assert_eq!(
            remote_origin_policy("/api/config", "PUT", false),
            RemoteDecision::NotFound
        );
        assert_eq!(
            remote_origin_policy("/api/config", "GET", false),
            RemoteDecision::Allow
        );
        // Log maintenance.
        assert_eq!(
            remote_origin_policy("/api/logs", "DELETE", false),
            RemoteDecision::NotFound
        );
    }

    #[test]
    fn policy_allows_normal_remote_surface() {
        // The remote Desktop's daily surface must all be reachable —
        // it authenticates through the same middleware as localhost.
        for (path, method) in [
            ("/api/status", "GET"),
            ("/api/agents", "GET"),
            ("/api/agents", "POST"),
            ("/health", "GET"),
            ("/api/relay/status", "GET"),
            ("/api/config", "GET"),
            ("/api/settings/default-compact-model", "PUT"),
            ("/api/user/profile", "GET"),
            ("/mqtt", "GET"),
            ("/api/some/future/route", "POST"),
        ] {
            assert_eq!(
                remote_origin_policy(path, method, false),
                RemoteDecision::Allow,
                "{method} {path} should be allowed remotely"
            );
        }
    }

    #[test]
    fn node_token_header_is_rejected_everywhere() {
        for (path, method) in [
            ("/api/agents", "GET"),
            ("/health", "GET"),
            ("/api/debug/mqtt/start", "POST"),
        ] {
            assert_eq!(
                remote_origin_policy(path, method, true),
                RemoteDecision::Forbidden,
                "{method} {path} with a node token must be forbidden"
            );
        }
    }

    #[test]
    fn lan_bridge_gate_denies_only_peers_outside_a_non_empty_allowlist() {
        use crate::security::{AllowlistEntry, IpAllowlist};
        let outside: std::net::Ipv4Addr = "203.0.113.9".parse().unwrap();
        let inside: std::net::Ipv4Addr = "198.51.100.7".parse().unwrap();
        let allowlist = IpAllowlist::new(
            AllowlistEntry::parse("198.51.100.0/24")
                .into_iter()
                .collect(),
        );
        // No policy configured (empty) — every peer reaches the bridge.
        assert!(lan_bridge_allowed(&IpAllowlist::default(), SocketAddr::from((outside, 1))));
        // A configured list gates the bridge like every other route.
        assert!(lan_bridge_allowed(&allowlist, SocketAddr::from((inside, 1))));
        assert!(!lan_bridge_allowed(&allowlist, SocketAddr::from((outside, 1))));
    }
}

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use std::sync::Arc;

    /// Fixed test ports, deliberately far from the production defaults
    /// (19874/19875/19876/19877) so a concurrently running Gateway is
    /// never disturbed.
    const TEST_MAIN_MQTT: u16 = 27975;
    const TEST_REMOTE_MQTT: u16 = 27974;
    const TEST_REMOTE_HTTP: u16 = 27977;

    /// Minimal MQTT 3.1.1 CONNECT packet (protocol level 4, clean
    /// session, username + password, keep-alive 60s).
    fn mqtt_connect(client_id: &str, username: &str, password: &str) -> Vec<u8> {
        fn lstr(s: &str) -> Vec<u8> {
            let mut v = (s.len() as u16).to_be_bytes().to_vec();
            v.extend_from_slice(s.as_bytes());
            v
        }
        let mut vh = lstr("MQTT");
        vh.push(4); // protocol level
        vh.push(0xC2); // clean session | username | password
        vh.extend_from_slice(&60u16.to_be_bytes());
        let mut payload = lstr(client_id);
        payload.extend(lstr(username));
        payload.extend(lstr(password));
        let body = [vh, payload].concat();
        // MQTT remaining length: base-128 varint, 7 bits per byte, MSB
        // = continuation (tokens are long enough to need 2+ bytes).
        let mut len = body.len();
        let mut pkt = vec![0x10u8];
        loop {
            let mut byte = (len % 128) as u8;
            len /= 128;
            if len > 0 {
                byte |= 0x80;
            }
            pkt.push(byte);
            if len == 0 {
                break;
            }
        }
        assert!(body.len() < 128 * 128 * 128, "test packet size sanity");
        pkt.extend_from_slice(&body);
        pkt
    }

    /// MQTT CONNACK return code, or None if the frame is not a CONNACK.
    fn connack_return_code(frame: &[u8]) -> Option<u8> {
        if frame.len() >= 4 && frame[0] == 0x20 && frame[1] == 2 {
            Some(frame[3])
        } else {
            None
        }
    }

    /// Minimal MQTT 3.1.1 SUBSCRIBE packet: one filter, QoS 1.
    fn mqtt_subscribe(topic: &str, packet_id: u16) -> Vec<u8> {
        let mut body = packet_id.to_be_bytes().to_vec();
        body.extend_from_slice(&(topic.len() as u16).to_be_bytes());
        body.extend_from_slice(topic.as_bytes());
        body.push(1);
        let mut len = body.len();
        let mut pkt = vec![0x82u8]; // SUBSCRIBE requires the 0b0010 flags
        loop {
            let mut byte = (len % 128) as u8;
            len /= 128;
            if len > 0 {
                byte |= 0x80;
            }
            pkt.push(byte);
            if len == 0 {
                break;
            }
        }
        pkt.extend_from_slice(&body);
        pkt
    }

    /// MQTT SUBACK return codes, or None if the frame is not a SUBACK.
    /// Assumes a one-byte remaining length (true for these tiny packets):
    /// `[type, remlen, pid_hi, pid_lo, code…]`.
    fn suback_codes(frame: &[u8]) -> Option<Vec<u8>> {
        (frame.len() >= 5 && frame[0] == 0x90).then(|| frame[4..].to_vec())
    }

    /// Wait until a TCP port accepts connections (bounded).
    async fn wait_listening(port: u16) {
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("port {port} never came up");
    }

    /// Full remote-listener e2e (§7.2): strict MQTT listener + guard +
    /// `/mqtt` WS bridge, verified end to end:
    ///
    /// 1. HTTP guard — 404 for debug/fs/config-write paths, 403 for a
    ///    forged node token, 200 for /health.
    /// 2. MQTT-over-WS through the bridge into the strict listener —
    ///    a valid user access token CONNECTs (CONNACK rc=0), a wrong
    ///    password is refused (rc=5 "not authorized").
    #[tokio::test]
    async fn remote_listener_guard_and_mqtt_bridge_e2e() {
        let dir = tempfile::tempdir().unwrap();
        let mut gw =
            crate::gateway::state::GatewayState::new(&dir.path().to_string_lossy());
        gw.user_verifier = Some(crate::http::test_support::verifier());
        let gateway_state = Arc::new(tokio::sync::RwLock::new(gw));

        // Broker with the strict remote listener (main server unused
        // here but part of the same broker by design).
        let broker = crate::mqtt::start_broker_with_auth(
            "127.0.0.1",
            TEST_MAIN_MQTT,
            None,
            Some(crate::mqtt::RemoteMqttListener {
                host: "127.0.0.1".to_string(),
                port: TEST_REMOTE_MQTT,
                auth: crate::mqtt::RemoteMqttAuth {
                    gateway_state: gateway_state.clone(),
                },
            }),
        )
        .expect("broker with strict listener starts");
        assert_eq!(
            broker.remote_listen_addr.expect("remote addr").port(),
            TEST_REMOTE_MQTT
        );

        // Remote-origin HTTP listener.
        let state = crate::http::routes::AppState::new(
            gateway_state.clone(),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        let server = tokio::spawn(start_remote_http_listener(
            state,
            TEST_REMOTE_HTTP,
            TEST_REMOTE_MQTT,
        ));
        wait_listening(TEST_REMOTE_HTTP).await;
        // The broker's remote server binds asynchronously inside its
        // OS thread — the bridge would otherwise race it and close.
        wait_listening(TEST_REMOTE_MQTT).await;

        // ── 1. HTTP guard ───────────────────────────────────────────
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{TEST_REMOTE_HTTP}");

        let health = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(health.status(), 200);

        let debug = client
            .post(format!("{base}/api/debug/mqtt/start"))
            .send()
            .await
            .unwrap();
        assert_eq!(debug.status(), 404, "debug endpoints are not visible");

        let fs = client.get(format!("{base}/api/fs/browse")).send().await.unwrap();
        assert_eq!(fs.status(), 404, "fs browse is not visible");

        let config_put = client.put(format!("{base}/api/config")).send().await.unwrap();
        assert_eq!(config_put.status(), 404, "config writes are not visible");

        let node_token = client
            .get(format!("{base}/api/agents"))
            .header("X-ACowork-Node-Token", "forged")
            .send()
            .await
            .unwrap();
        assert_eq!(
            node_token.status(),
            403,
            "machine identity cannot be asserted remotely"
        );

        // ── 2. MQTT over the /mqtt WS bridge ────────────────────────
        use futures_util::{SinkExt as _, StreamExt as _};
        let token = crate::http::test_support::access_token("alice", "user");

        // Valid credential: CONNECT → CONNACK rc=0.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{TEST_REMOTE_HTTP}/mqtt"
        ))
        .await
        .expect("WS upgrade on /mqtt");
        ws.send(tokio_tungstenite::tungstenite::Message::Binary(
            mqtt_connect("user:alice:desktop:mac-1", "alice", &token).into(),
        ))
        .await
        .unwrap();
        let connack = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("CONNACK within 5s")
            .expect("stream open")
            .expect("ws read ok");
        match connack {
            tokio_tungstenite::tungstenite::Message::Binary(data) => {
                assert_eq!(
                    connack_return_code(&data),
                    Some(0),
                    "valid access token must CONNECT (got {data:?})"
                );
            }
            other => panic!("expected binary CONNACK, got {other:?}"),
        }

        // Wrong password: rumqttd's strict handler fails the CONNECT
        // and drops the connection (Error::InvalidAuth — no CONNACK).
        // Whatever surfaces on the WS (Close / end / error), it must
        // NOT be a successful CONNACK.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{TEST_REMOTE_HTTP}/mqtt"
        ))
        .await
        .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Binary(
            mqtt_connect("user:alice:desktop:mac-1", "alice", "wrong-password").into(),
        ))
        .await
        .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("reply within 5s");
        // Close / stream end / error — all acceptable: the broker
        // refused the CONNECT. Only a *successful* CONNACK would
        // be a violation.
        if let Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) = reply {
            assert_ne!(
                connack_return_code(&data),
                Some(0),
                "bad credentials must not CONNECT (got {data:?})"
            );
        }

        server.abort();
    }

    /// The strict listener refuses internal identities outright (§7.2):
    /// a `node:*` client with a perfectly valid user token is still
    /// rejected — remote traffic may only arrive as a Desktop/Mobile
    /// user. Direct TCP (no WS bridge) keeps this test independent of
    /// the bridge internals.
    #[tokio::test]
    async fn strict_listener_rejects_internal_identities() {
        let dir = tempfile::tempdir().unwrap();
        let mut gw =
            crate::gateway::state::GatewayState::new(&dir.path().to_string_lossy());
        gw.user_verifier = Some(crate::http::test_support::verifier());
        let gateway_state = Arc::new(tokio::sync::RwLock::new(gw));

        let _broker = crate::mqtt::start_broker_with_auth(
            "127.0.0.1",
            TEST_MAIN_MQTT + 10,
            None,
            Some(crate::mqtt::RemoteMqttListener {
                host: "127.0.0.1".to_string(),
                port: TEST_REMOTE_MQTT + 10,
                auth: crate::mqtt::RemoteMqttAuth {
                    gateway_state: gateway_state.clone(),
                },
            }),
        )
        .expect("broker starts");
        wait_listening(TEST_REMOTE_MQTT + 10).await;

        use rumqttc::{AsyncClient, MqttOptions};
        let token = crate::http::test_support::access_token("alice", "user");

        let mut opts = MqttOptions::new("node:rogue", "127.0.0.1", TEST_REMOTE_MQTT + 10);
        opts.set_keep_alive(std::time::Duration::from_secs(5));
        opts.set_credentials("rogue", &token);
        let (_client, mut eventloop) = AsyncClient::new(opts, 4);
        // Poll: the only acceptable outcome is an error (CONNECT
        // refused) — a ConnAck would mean an internal identity was
        // admitted remotely.
        let mut saw_error = false;
        for _ in 0..20 {
            match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Incoming::ConnAck(_))) => {
                    panic!("node:* identity must not CONNECT via the strict listener");
                }
                Ok(_) => continue,
                Err(_) => {
                    saw_error = true;
                    break;
                }
            }
        }
        assert!(saw_error, "broker must refuse the rogue node CONNECT");
    }

    /// The MAIN-listener `/mqtt` bridge (mobile presence over LAN). Mounted
    /// exactly as `http::server::start_http_server` mounts it — after the
    /// router's layers, served with `ConnectInfo` for the peer gate — and
    /// verified end to end: a user access token CONNECTs, and the wildcard
    /// presence filter the mobile list subscribes to is GRANTED (a `0x80`
    /// SUBACK code would mean the whole phase-2 design fails closed on ACLs).
    #[tokio::test]
    async fn lan_mqtt_bridge_connects_and_grants_presence_subscribe() {
        const TEST_LAN_HTTP: u16 = 27980;
        let strict_port = TEST_REMOTE_MQTT + 20;

        let dir = tempfile::tempdir().unwrap();
        let mut gw = crate::gateway::state::GatewayState::new(&dir.path().to_string_lossy());
        gw.user_verifier = Some(crate::http::test_support::verifier());
        let gateway_state = Arc::new(tokio::sync::RwLock::new(gw));

        let _broker = crate::mqtt::start_broker_with_auth(
            "127.0.0.1",
            TEST_MAIN_MQTT + 20,
            None,
            Some(crate::mqtt::RemoteMqttListener {
                host: "127.0.0.1".to_string(),
                port: strict_port,
                auth: crate::mqtt::RemoteMqttAuth {
                    gateway_state: gateway_state.clone(),
                },
            }),
        )
        .expect("broker with strict listener starts");
        wait_listening(strict_port).await;

        let allowlist = crate::security::IpAllowlist::default();
        let app = axum::Router::new().route(
            "/mqtt",
            get(
                move |ws: WebSocketUpgrade, peer: axum::extract::ConnectInfo<SocketAddr>| {
                    lan_mqtt_bridge(ws, peer.0, strict_port, allowlist.clone())
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", TEST_LAN_HTTP))
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        wait_listening(TEST_LAN_HTTP).await;

        use futures_util::StreamExt as _;
        use tokio_tungstenite::tungstenite::Message;
        let token = crate::http::test_support::access_token("alice", "user");
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{TEST_LAN_HTTP}/mqtt"
        ))
        .await
        .expect("WS upgrade on the LAN /mqtt route");

        ws.send(Message::Binary(
            mqtt_connect("user:alice:mobile:phone-1", "alice", &token).into(),
        ))
        .await
        .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("CONNACK within 5s")
            .expect("stream open")
            .expect("ws read ok");
        let Message::Binary(connack) = frame else {
            panic!("expected binary CONNACK, got {frame:?}");
        };
        assert_eq!(
            connack_return_code(&connack),
            Some(0),
            "a valid access token must CONNECT over the LAN bridge (got {connack:?})"
        );

        ws.send(Message::Binary(
            mqtt_subscribe("acowork/agents/+/status", 7).into(),
        ))
        .await
        .unwrap();
        let mut codes: Option<Vec<u8>> = None;
        for _ in 0..5 {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
                .await
                .expect("SUBACK within 5s")
                .expect("stream open")
                .expect("ws read ok");
            let parsed = match frame {
                Message::Binary(data) => suback_codes(&data),
                _ => None,
            };
            if let Some(c) = parsed {
                codes = Some(c);
                break;
            }
        }
        let codes = codes.expect("a SUBACK for the presence filter");
        assert_eq!(
            codes.first().copied(),
            Some(1),
            "presence subscription must be granted at QoS 1, not refused (0x80)"
        );

        server.abort();
    }
}
