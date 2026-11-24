//! Embedded rumqttd MQTT broker (ADR-033 Phase 1).
//!
//! The Gateway embeds a rumqttd broker in-process, listening on
//! `127.0.0.1:19875` (MQTT 3.1.1 / TCP). All clients (Runtime, Desktop,
//! and the Gateway's own publisher) connect to this broker.
//!
//! See `docs/zh/protocols/mqtt.md` §1–§2 for the protocol conventions
//! and architecture overview.
//!
//! ADR-055 Phase 5a adds CONNECT-layer authentication via rumqttd's
//! `ConnectionSettings::set_auth_handler`. The policy is a pure
//! function ([`check_connect_auth`]) so it can be unit-tested without
//! a running broker; the broker thread only adapts it to rumqttd's
//! async handler shape.

use std::net::SocketAddr;
use std::sync::Arc;

use rumqttd::{Broker, Config};

use acowork_core::defaults;
use acowork_core::node::NODE_CLIENT_ID_PREFIX;

use super::enrollment::{
    constant_time_eq, EnrollmentTokenStore, NodeTokenStore, SharedEnrollmentTokenStore,
    SharedNodeTokenStore, TokenValidation,
};

/// Error type for MQTT broker operations.
#[derive(Debug, thiserror::Error)]
pub enum MqttBrokerError {
    #[error("MQTT broker failed to start: {0}")]
    Start(String),
    #[error("MQTT broker config error: {0}")]
    Config(String),
}

/// ADR-055 Phase 5a: broker CONNECT authentication inputs.
///
/// Cloned into the broker thread; every credential check is delegated
/// to the pure decision function [`check_connect_auth`]. The stores
/// are shared (std Mutex) with the MQTT dispatch and the HTTP
/// handlers, so tokens issued at runtime are immediately honored.
#[derive(Clone)]
pub struct BrokerAuth {
    /// Master switch — `mqtt.auth_enabled` config.
    pub auth_enabled: bool,
    /// One-time enrollment tokens (first node connect).
    pub enrollment_tokens: SharedEnrollmentTokenStore,
    /// Long-lived per-node tokens (node reconnect + `agent:{id}`).
    pub node_tokens: SharedNodeTokenStore,
    /// Internal publisher credential (generated at Gateway startup).
    pub publisher_token: String,
    /// HTTP bearer token (HttpAuth) — Desktop MQTT credential.
    pub http_token: Option<String>,
}

/// Reference snapshot of the auth decision inputs — lets
/// [`check_connect_auth`] stay a pure function over locked store
/// guards.
pub struct ConnectAuthContext<'a> {
    pub auth_enabled: bool,
    pub enrollment_tokens: &'a EnrollmentTokenStore,
    pub node_tokens: &'a NodeTokenStore,
    pub publisher_token: Option<&'a str>,
    pub http_token: Option<&'a str>,
}

// ── Strict remote MQTT listener (design doc 24 §7.2) ────────────────

/// Inputs for the strict listener (the second rumqttd v4 server).
///
/// The verifier is NOT captured at start time: `GatewayState.user_verifier`
/// is loaded by the user supervisor once the user service publishes its
/// Ed25519 public key, and unloaded again when that service stops — the
/// handler re-reads it on every CONNECT so the remote listener follows the
/// same lifecycle as HTTP authentication (no credential source divergence).
#[derive(Clone)]
pub struct RemoteMqttAuth {
    /// Shared Gateway state — read on every CONNECT for the current
    /// `user_verifier`.
    pub gateway_state: std::sync::Arc<tokio::sync::RwLock<crate::gateway::state::GatewayState>>,
}

/// Admissible remote client-id shape (§7.2): `user:{name}:desktop:{id}`
/// or `user:{name}:mobile:{id}`, both segments non-empty.
///
/// Returns the `{name}` segment on match (it is cross-checked against the
/// token subject — a client may not wear another user's identity).
fn remote_client_shape(client_id: &str) -> Option<&str> {
    let rest = client_id.strip_prefix("user:")?;
    for sep in [":desktop:", ":mobile:"] {
        if let Some(idx) = rest.find(sep) {
            let (name, tail) = rest.split_at(idx);
            let device = &tail[sep.len()..];
            if !name.is_empty() && !device.is_empty() && !name.contains(':') && !device.contains(':')
            {
                return Some(name);
            }
            return None;
        }
    }
    None
}

/// Strict CONNECT decision for the remote listener (§7.2).
///
/// Allow: `user:{name}:desktop:{id}` / `user:{name}:mobile:{id}` with a
/// valid access token whose subject IS `{name}`. Everything else —
/// `node:*`, `agent:*`, internal publishers, unknown shapes — is rejected:
/// there is no remote scenario where a Node/Runtime/machine identity
/// should present itself (they live on the LAN and use `:19875`).
pub fn check_remote_connect_auth(
    client_id: &str,
    password: &str,
    verifier: &acowork_core::auth::TokenVerifier,
    now: i64,
) -> bool {
    let Some(name) = remote_client_shape(client_id) else {
        return false;
    };
    match verifier.verify_kind(password, acowork_core::auth::TokenKind::Access, now) {
        Ok(claims) => claims.sub == name,
        Err(_) => false,
    }
}


/// Pure CONNECT authentication decision (ADR-055 §6.8, Phase 5a).
///
/// client_id conventions (protocol docs §8.5):
/// - `node:{node_id}` — Node Agent. Password = the node's long-lived
///   token, or a valid unconsumed enrollment token (first connect).
/// - `agent:{agent_id}` — Runtime. Password = ANY registered node
///   token (Phase 5a simplification: agent→node ownership is NOT
///   verified — the Node only injects its own token when spawning
///   Runtimes; strict per-agent ACLs are deferred to Phase 5b).
/// - `gateway:publisher`, `user:service`, `doc:service` — internal
///   publishers (Gateway / user service / doc service), password =
///   the startup-generated publisher token. The user/doc supervisors
///   forward the token to the services they spawn (ADR-084 §决策 4b).
/// - `user:{name}:desktop:{id}` — Desktop, password = the HTTP bearer
///   token (HttpAuth; available when `http.auth_enabled` is on).
///
/// The `username` field is informational at this tier (identity is
/// keyed by client_id); it is not yet cross-checked against the
/// client_id. Everything else is rejected when auth is enabled; when
/// `auth_enabled` is false every connection passes (default).
pub fn check_connect_auth(
    client_id: &str,
    _username: &str,
    password: &str,
    ctx: &ConnectAuthContext<'_>,
) -> bool {
    if !ctx.auth_enabled {
        return true;
    }
    if let Some(node_id) = client_id.strip_prefix(NODE_CLIENT_ID_PREFIX) {
        // ADR-075 D4: `node:{id}:rename` is the temporary client_id the
        // node's `rename` command connects with — same credential as
        // the node itself (no LWT, so it never flips the status).
        if let Some(real_id) = node_id.strip_suffix(":rename") {
            return ctx.node_tokens.node_token_matches(real_id, password);
        }
        if ctx.node_tokens.node_token_matches(node_id, password) {
            return true;
        }
        // First-connect path: a valid, unconsumed enrollment token
        // (auth_enabled implies the enrollment store was loaded).
        return ctx.enrollment_tokens.validate_token(password) == TokenValidation::Valid;
    }
    if client_id.starts_with("agent:") {
        return ctx.node_tokens.any_token_matches(password);
    }
    // Internal publishers share the startup-generated token: the
    // Gateway's own publisher plus the user/doc service publishers
    // (the supervisors hand them the token at spawn time, ADR-084
    // §决策 4b).
    if client_id == "gateway:publisher"
        // ADR-084 §决策 4b: structural — `user:service` / `doc:service`,
        // optionally followed by a `:pid` suffix the supervisor-spawned
        // publisher appends to dodge `Duplicate client_id` on internal
        // reconnect. A bare `user:serviceevil` / `doc:serviceevil`
        // (no colon after `service`) must NOT match — it would otherwise
        // reach the publisher token check, which still requires the
        // correct token to pass, but the structural form keeps the
        // prefix-match contract tight and consistent with the
        // `user:*:desktop:*` branch below.
        || client_id == "user:service"
        || client_id.starts_with("user:service:")
        || client_id == "doc:service"
        || client_id.starts_with("doc:service:")
    {
        return match ctx.publisher_token {
            Some(expected) => constant_time_eq(expected.as_bytes(), password.as_bytes()),
            None => false,
        };
    }
    if let Some(rest) = client_id.strip_prefix("user:")
        && rest.contains(":desktop:")
    {
        return match ctx.http_token {
            Some(expected) => constant_time_eq(expected.as_bytes(), password.as_bytes()),
            None => false,
        };
    }
    false
}

/// Handle to the running MQTT broker.
///
/// The broker lives on a dedicated OS thread (see [`start_broker`]);
/// this handle carries the shutdown channel, the listen addresses and
/// the listener's state stream.
///
/// Production callers never need to shut the broker down — it lives
/// for the lifetime of the Gateway process. The `shutdown_tx` exists
/// only so the debug HTTP endpoints (`POST /api/debug/mqtt/*`) can
/// request a graceful exit without restarting the whole process.
pub struct MqttBrokerHandle {
    /// Channel to signal the broker thread to exit.
    shutdown_tx: Option<std::sync::mpsc::Sender<()>>,
    /// The address the broker is listening on.
    pub listen_addr: SocketAddr,
    /// The address of the strict remote listener, when it was requested.
    pub remote_listen_addr: Option<SocketAddr>,
    /// Liveness of the listener, updated by the supervisor thread.
    state: tokio::sync::watch::Receiver<BrokerState>,
}

/// Lifecycle of the embedded broker's listener.
///
/// The broker is a supervised resource, not a one-shot construction: a
/// bind failure (port taken by a process that is still shutting down,
/// for instance) is retried on the same curve the MQTT sessions use.
/// Every non-terminal state has a bounded exit, so a consumer that
/// renders this state can never be stuck on a stale value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerState {
    /// Supervisor is bringing the listener up; no evidence yet.
    Starting,
    /// A TCP connection to the listen address succeeded — the listener
    /// is accepting. This is measured, not assumed.
    Listening,
    /// The listener died and will be rebuilt after `retry_in`.
    Retrying {
        reason: String,
        attempt: u32,
        retry_in: std::time::Duration,
    },
    /// The supervisor stopped for good.
    Closed,
}

impl BrokerState {
    /// Whether the listener is currently accepting connections.
    pub fn is_listening(&self) -> bool {
        matches!(self, BrokerState::Listening)
    }

    /// Whether the supervisor has stopped for good.
    pub fn is_terminal(&self) -> bool {
        matches!(self, BrokerState::Closed)
    }
}

/// The strict second v4 listener a broker may host (§7.2).
pub struct RemoteMqttListener {
    /// Bind host for the strict listener (always loopback — the only
    /// path to it is the `/mqtt` WebSocket bridge on the remote HTTP
    /// listener, itself loopback-only and fed by the relay tunnel).
    pub host: String,
    pub port: u16,
    pub auth: RemoteMqttAuth,
}

/// Build the rumqttd `Config` from a TOML template (the library's intended API).
///
/// Programmatic struct construction is fragile — rumqttd expects config via
/// deserialization and fields like `ConsoleSettings` have non-trivial defaults.
///
/// `remote` adds the strict second v4 server (design doc 24 §7.2): same
/// router/session state as the main server (remote Desktop clients and
/// local Runtime publishers must see each other's traffic), but with its
/// own listener address and auth handler.
pub fn build_broker_config(host: &str, port: u16, remote: Option<(&str, u16)>) -> Config {
    let remote_toml = remote
        .map(|(h, p)| {
            format!(
                r#"
[v4.acowork-remote]
name = "acowork-remote"
listen = "{h}:{p}"
next_connection_delay_ms = 1

[v4.acowork-remote.connections]
connection_timeout_ms = 5000
max_payload_size = {max_pkt}
max_inflight_count = 100
max_inflight_size = 1048576
throttle_delay_ms = 0
dynamic_filters = false
"#,
                max_pkt = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE,
            )
        })
        .unwrap_or_default();
    let config_toml = format!(
        r#"
id = 0

[router]
max_connections = {max_conn}
max_segment_size = {max_pkt}
max_segment_count = 10
max_read_len = 1048576
max_outgoing_packet_count = 1000
instant_ack = true

[v4.acowork]
name = "acowork"
listen = "{host}:{port}"
next_connection_delay_ms = 1

[v4.acowork.connections]
connection_timeout_ms = 5000
max_payload_size = {max_pkt}
max_inflight_count = 100
max_inflight_size = 1048576
throttle_delay_ms = 0
dynamic_filters = false
{remote_toml}
[console]
listen = "127.0.0.1:0"
"#,
        host = host,
        port = port,
        max_conn = defaults::GATEWAY_MQTT_MAX_CONNECTIONS,
        max_pkt = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE,
    );

    toml::from_str(&config_toml)
        .unwrap_or_else(|e| panic!("BUG: invalid MQTT broker config template: {}", e))
}

/// Start the embedded MQTT broker (non-blocking, single entry point).
///
/// The broker runs on a dedicated OS thread named `mqtt-broker`.
///
/// # Why a background thread?
///
/// rumqttd 0.20's `Broker::start()` blocks while the broker is alive —
/// it joins the server threads, whose accept loops run forever. Calling
/// it directly would block the caller, so the broker lives on a
/// dedicated OS thread and this function returns as soon as that
/// supervisor thread exists.
///
/// Construction is NOT readiness. `Ok` means "a supervisor owns the
/// listener", nothing more: a bind conflict is retried on the session
/// backoff curve, and liveness is published as [`BrokerState`] on the
/// handle's watch channel. Consumers that need the listener wait on
/// [`MqttBrokerHandle::wait_listening`] or consume the state stream;
/// nothing here blocks the Gateway's boot on it.
///
/// This is the ONLY public entry point for starting the broker; there
/// is intentionally no "direct" (blocking) variant.
pub fn start_broker(host: &str, port: u16) -> Result<MqttBrokerHandle, MqttBrokerError> {
    start_broker_with_auth(host, port, None, None)
}

/// Start the embedded MQTT broker with an optional CONNECT auth
/// handler (ADR-055 Phase 5a). `Some(auth)` wires
/// [`check_connect_auth`] into rumqttd's `set_auth_handler`;
/// `None` keeps the historical permissive behavior (every connection
/// passes).
///
/// `remote` (design doc 24 §7.2) additionally hosts a second v4 server
/// with the STRICT handler ([`check_remote_connect_auth`]) on its own
/// loopback listener. The two servers share one router, so remote
/// Desktop clients and local Runtime publishers exchange traffic
/// normally. A port conflict on the remote listener does NOT fail the
/// broker — the main listener must stay up regardless; the strict
/// listener is skipped with a warning.
#[allow(clippy::too_many_arguments)]
pub fn start_broker_with_auth(
    host: &str,
    port: u16,
    auth: Option<BrokerAuth>,
    remote: Option<RemoteMqttListener>,
) -> Result<MqttBrokerHandle, MqttBrokerError> {
    let listen_addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| MqttBrokerError::Config(format!(
            "Invalid listen address '{}:{}': {}",
            host, port, e
        )))?;

    // The remote listener is configured unconditionally. rumqttd 0.20
    // runs every server on its own thread and only *logs* a server's
    // start error (`error!(.., "Server error - V4")`), so a remote bind
    // failure ends that thread alone — the main listener keeps serving.
    // The former bind-then-drop "pre-probe" was therefore unnecessary,
    // and it opened a window in which another process could steal the
    // port between the probe and rumqttd's own bind.
    let remote_bind: Option<(String, u16)> = remote.as_ref().map(|r| (r.host.clone(), r.port));
    let remote_listen_addr: Option<SocketAddr> = remote_bind
        .as_ref()
        .and_then(|(h, p)| format!("{}:{}", h, p).parse().ok());

    let (state_tx, state_rx) = tokio::sync::watch::channel(BrokerState::Starting);
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    let h = host.to_string();

    std::thread::Builder::new()
        .name("mqtt-broker".into())
        .spawn(move || {
            let mut config =
                build_broker_config(&h, port, remote_bind.as_ref().map(|(h, p)| (h.as_str(), *p)));
            // The liveness probe authenticates as an internal publisher,
            // so it needs the token before `auth` moves into the handler.
            let probe_token = auth.as_ref().map(|a| a.publisher_token.clone());
            if let Some(auth) = auth {
                let v4 = config.v4.as_mut().expect("v4 servers configured");
                let server = v4.get_mut("acowork").expect("server 'acowork' configured");
                tracing::info!(
                    auth_enabled = auth.auth_enabled,
                    "MQTT broker CONNECT auth handler installed (ADR-055 Phase 5a)"
                );
                server.connections.set_auth_handler(
                    move |client_id, username, password| {
                        // std::sync::Mutex poisoning is unrecoverable
                        // here — any poisoned lock means a panicked
                        // holder, so fall back to the inner guard and
                        // log via the normal decision path.
                        std::future::ready({
                            let enrollment = auth
                                .enrollment_tokens
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let node_tokens = auth
                                .node_tokens
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let ctx = ConnectAuthContext {
                                auth_enabled: auth.auth_enabled,
                                enrollment_tokens: &enrollment,
                                node_tokens: &node_tokens,
                                publisher_token: Some(auth.publisher_token.as_str()),
                                http_token: auth.http_token.as_deref(),
                            };
                            check_connect_auth(&client_id, &username, &password, &ctx)
                        })
                    },
                );
            }
            // Strict remote listener handler (§7.2): every CONNECT
            // re-reads the CURRENT verifier from GatewayState, so the
            // remote MQTT surface follows the user service lifecycle
            // exactly like the HTTP surface does (fail closed while
            // the verifier is not loaded). Guard = the server actually
            // being present in the built config.
            if let Some(remote) = remote.as_ref()
                && let Some(server) = config
                    .v4
                    .as_mut()
                    .and_then(|v4| v4.get_mut("acowork-remote"))
            {
                let state = remote.auth.gateway_state.clone();
                tracing::info!(
                    "strict remote MQTT CONNECT auth handler installed (design doc 24 §7.2)"
                );
                server.connections.set_auth_handler(
                    move |client_id, _username, password| {
                        let state = state.clone();
                        async move {
                            let gw = state.read().await;
                            match &gw.user_verifier {
                                Some(verifier) => check_remote_connect_auth(
                                    &client_id,
                                    &password,
                                    verifier,
                                    acowork_core::auth::now_unix(),
                                ),
                                None => false,
                            }
                        }
                    },
                );
            }
            // Supervised lifetime. The decisive fact about rumqttd
            // 0.20: `Broker::start()` joins the server threads, so it
            // **blocks while the broker is alive and returns when every
            // server thread has exited**. A bind failure is logged
            // inside the server thread, not returned — measured: with
            // the port taken, `start()` returns Ok(()) in ~3 ms.
            //
            // The previous wiring read that return as a *success*
            // confirmation and the 500 ms timeout as "still starting",
            // which is exactly inverted: the only signal that could
            // arrive meant "the broker is dead". Hence a real probe for
            // liveness, and a retry loop for recovery.
            // No owner (handle dropped) counts as shutdown: a broker
            // nobody can observe must not keep retrying forever.
            let stopped = |rx: &std::sync::mpsc::Receiver<()>| {
                !matches!(
                    rx.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                )
            };
            let policy = acowork_mqtt_session::ReconnectPolicy::default();
            let mut attempt: u32 = 0;
            loop {
                if stopped(&shutdown_rx) {
                    break;
                }
                let _ = state_tx.send(BrokerState::Starting);
                let mut broker = Broker::new(config.clone());
                tracing::info!(
                    addr = %listen_addr,
                    port,
                    attempt,
                    max_connections = defaults::GATEWAY_MQTT_MAX_CONNECTIONS,
                    max_packet_size = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE,
                    "Starting embedded MQTT broker (rumqttd)"
                );

                let probe =
                    spawn_listen_probe(listen_addr, probe_token.clone(), state_tx.clone());
                let outcome = broker.start();
                probe.stop();
                drop(broker);

                if stopped(&shutdown_rx) {
                    break;
                }
                let reason = match outcome {
                    Ok(()) => "every broker server thread exited (listen address unavailable?)"
                        .to_string(),
                    Err(e) => format!("rumqttd broker start failed: {e}"),
                };
                let retry_in = policy.delay(attempt);
                attempt = attempt.saturating_add(1);
                tracing::error!(
                    addr = %listen_addr, %reason, ?retry_in, attempt,
                    "MQTT broker listener is down; retrying"
                );
                let _ = state_tx.send(BrokerState::Retrying {
                    reason,
                    attempt,
                    retry_in,
                });

                // Sleep in slices so a shutdown request stays responsive
                // without exposing the parked thread handle.
                let deadline = std::time::Instant::now() + retry_in;
                while std::time::Instant::now() < deadline {
                    if stopped(&shutdown_rx) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
            let _ = state_tx.send(BrokerState::Closed);
            tracing::info!("MQTT broker supervisor exiting (broker dropped)");
        })
        .map_err(|e| MqttBrokerError::Start(format!("spawn thread: {e}")))?;

    Ok(MqttBrokerHandle {
        shutdown_tx: Some(shutdown_tx),
        listen_addr,
        remote_listen_addr,
        state: state_rx,
    })
}

/// Background probe that turns "the broker answers CONNECT" into an
/// event.
///
/// rumqttd exposes no bind result, so liveness has to be measured from
/// the outside. A bare TCP connect is **not** sufficient evidence: a
/// socket that merely holds the port (a process still shutting down, a
/// test blocker) completes the handshake from its accept backlog
/// without ever serving MQTT — measured, and it produced a false
/// `Listening`. So the probe performs a real MQTT CONNECT and requires
/// an accepted CONNACK.
///
/// The probe has no deadline of its own: it ends on success, or when the
/// supervisor stops it because `Broker::start()` returned (the death
/// event). Those are the two bounded exits of `Starting`.
struct ListenProbe {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ListenProbe {
    fn stop(self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.handle {
            let _ = handle.join();
        }
    }
}

fn spawn_listen_probe(
    listen_addr: SocketAddr,
    password: Option<String>,
    state: tokio::sync::watch::Sender<BrokerState>,
) -> ListenProbe {
    // A wildcard bind address is not a valid connect target (Windows
    // rejects connects to 0.0.0.0), so probe the loopback equivalent.
    let probe_addr = if listen_addr.ip().is_unspecified() {
        let loopback: std::net::IpAddr = if listen_addr.is_ipv4() {
            "127.0.0.1".parse().expect("loopback v4")
        } else {
            "::1".parse().expect("loopback v6")
        };
        SocketAddr::new(loopback, listen_addr.port())
    } else {
        listen_addr
    };
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("mqtt-broker-probe".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let mut warned = false;
            // Each attempt is a full CONNECT, so the interval backs off:
            // a fixed 50 ms would drive 20 CONNECT/s against our own
            // broker for as long as it stays unreachable. Fast at first
            // (the normal case is a listener that is already up), then
            // settling at 1 s — the probe is a liveness check, not a
            // readiness race.
            let mut interval = std::time::Duration::from_millis(50);
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                if mqtt_connack_probe(&probe_addr, password.as_deref().unwrap_or("")) {
                    tracing::info!(addr = %listen_addr, "MQTT broker confirmed serving (CONNECT accepted)");
                    let _ = state.send(BrokerState::Listening);
                    return;
                }
                if !warned && started.elapsed() > std::time::Duration::from_secs(5) {
                    warned = true;
                    tracing::warn!(
                        addr = %listen_addr,
                        "MQTT broker still not accepting connections after 5s"
                    );
                }
                // Slice the sleep so `stop()` stays responsive.
                let until = std::time::Instant::now() + interval;
                while std::time::Instant::now() < until
                    && !flag.load(std::sync::atomic::Ordering::SeqCst)
                {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                interval = (interval * 2).min(std::time::Duration::from_secs(1));
            }
        })
        .ok();
    ListenProbe { stop, handle }
}

/// Client id the liveness probe CONNECTs with.
///
/// `user:service:{suffix}` is the internal-publisher shape, so the
/// startup publisher token authorizes it, and the suffix keeps it clear
/// of the real `user:service` publisher (a duplicate client id would
/// kick that session off the broker).
const PROBE_CLIENT_ID: &str = "user:service:broker-probe";

/// One MQTT 3.1.1 CONNECT → CONNACK round trip, hand-rolled.
///
/// Deliberately dependency-free and synchronous: it runs on the probe
/// thread, must not need a tokio runtime (rumqttd owns its own), and
/// only needs the four packets' worth of bytes below. Returns true only
/// for CONNACK return code 0 (accepted).
fn mqtt_connack_probe(addr: &SocketAddr, password: &str) -> bool {
    use std::io::{Read, Write};

    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(addr, std::time::Duration::from_millis(200))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(500)));
    let _ = stream.set_nodelay(true);

    // Variable header: protocol name/level, flags, keep alive.
    let mut body: Vec<u8> = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04];
    let mut flags = 0x02u8; // clean session
    if !password.is_empty() {
        flags |= 0x40; // password present (no username)
    }
    body.push(flags);
    body.extend_from_slice(&60u16.to_be_bytes()); // keep alive: rumqttd rejects 0
    let id = PROBE_CLIENT_ID.as_bytes();
    body.extend_from_slice(&(id.len() as u16).to_be_bytes());
    body.extend_from_slice(id);
    if !password.is_empty() {
        let pw = password.as_bytes();
        body.extend_from_slice(&(pw.len() as u16).to_be_bytes());
        body.extend_from_slice(pw);
    }

    let mut packet = vec![0x10u8];
    // Remaining length, MQTT variable-byte encoding (body is < 16 KiB).
    let mut len = body.len();
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        packet.push(byte);
        if len == 0 {
            break;
        }
    }
    packet.extend_from_slice(&body);
    if stream.write_all(&packet).is_err() {
        return false;
    }

    let mut head = [0u8; 4];
    if stream.read_exact(&mut head).is_err() {
        return false;
    }
    // CONNACK: type 2, remaining length 2, [session present, return code].
    let accepted = head[0] == 0x20 && head[1] == 0x02 && head[3] == 0x00;
    // Leave cleanly so no session or LWT is left behind either way.
    let _ = stream.write_all(&[0xE0, 0x00]);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    accepted
}

/// Gracefully shut down the broker.
///
/// Sends a signal to the broker thread (if running in-thread mode) to
/// unpark and exit, which drops the `Broker` and closes all TCP
/// connections. After shutdown, the broker is permanently stopped —
/// restarting requires creating a new handle via `start_broker`.
impl MqttBrokerHandle {
    /// Current listener state.
    pub fn state(&self) -> BrokerState {
        self.state.borrow().clone()
    }

    /// Watch receiver for every listener state change.
    pub fn state_rx(&self) -> tokio::sync::watch::Receiver<BrokerState> {
        self.state.clone()
    }

    /// Wait until the listener accepts connections, or `budget` elapses.
    ///
    /// This is a *consumer-side* budget, not the broker's own deadline:
    /// the supervisor guarantees a state change on its own (Listening,
    /// or Retrying once `Broker::start()` returns). `false` means "not
    /// yet", never "failed forever" — consume [`Self::state_rx`] for the
    /// authoritative picture.
    pub async fn wait_listening(&self, budget: std::time::Duration) -> bool {
        let mut rx = self.state.clone();
        if rx.borrow_and_update().is_listening() {
            return true;
        }
        let deadline = tokio::time::sleep(budget);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                changed = rx.changed() => {
                    match changed {
                        Ok(()) => {
                            if rx.borrow_and_update().is_listening() {
                                return true;
                            }
                        }
                        Err(_) => return false,
                    }
                }
                () = &mut deadline => return false,
            }
        }
    }

    /// Signal the broker supervisor to exit, then return immediately.
    ///
    /// This **does not wait** for the broker thread to actually exit.
    /// rumqttd's `Broker::start()` joins the server threads, so the TCP
    /// listener stays open until process exit; the signal only ends the
    /// supervisor loop between attempts. Production callers should treat
    /// the broker as process-lifetime state and never shut it down.
    pub fn signal_shutdown(&mut self) -> Result<(), String> {
        let tx = self
            .shutdown_tx
            .take()
            .ok_or_else(|| "broker already shut down".to_string())?;
        tx.send(()).map_err(|_| "broker thread already exited".to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_broker_config_defaults() {
        let config = build_broker_config(
            defaults::GATEWAY_MQTT_HOST,
            defaults::GATEWAY_MQTT_PORT,
            None,
        );

        assert_eq!(config.router.max_connections, 100);
        assert_eq!(
            config.router.max_segment_size,
            10 * 1024 * 1024,
            "segment size should cover 10 MB packets"
        );

        let v4 = config.v4.as_ref().expect("v4 servers must be configured");
        let server = v4.get("acowork").expect("server 'acowork' must exist");
        assert_eq!(server.listen.port(), defaults::GATEWAY_MQTT_PORT);
        assert_eq!(
            server.connections.max_payload_size,
            10 * 1024 * 1024
        );
        assert!(server.tls.is_none(), "TLS should be disabled for localhost");
        assert!(
            !v4.contains_key("acowork-remote"),
            "no remote server without the strict listener"
        );
    }

    #[test]
    fn test_build_broker_config_custom_host_port() {
        let config = build_broker_config("127.0.0.1", 32100, None);
        let v4 = config.v4.as_ref().expect("v4 servers must be configured");
        let server = v4.get("acowork").unwrap();
        assert_eq!(server.listen.port(), 32100);
    }

    #[test]
    fn test_build_broker_config_with_remote() {
        let config = build_broker_config("127.0.0.1", 32100, Some(("127.0.0.1", 19874)));
        let v4 = config.v4.as_ref().expect("v4 servers must be configured");
        let remote = v4
            .get("acowork-remote")
            .expect("strict remote server must exist");
        assert_eq!(remote.listen.port(), 19874);
        // Same payload limits as the main server.
        assert_eq!(
            remote.connections.max_payload_size,
            v4.get("acowork").unwrap().connections.max_payload_size
        );
    }

    #[test]
    fn remote_client_shape_accepts_desktop_and_mobile() {
        assert_eq!(remote_client_shape("user:alice:desktop:mac-1"), Some("alice"));
        assert_eq!(remote_client_shape("user:alice:mobile:phone-1"), Some("alice"));
        assert_eq!(remote_client_shape("user:大鱼:desktop:mac-1"), Some("大鱼"));
        // Mobile opens a SECOND connection for agent presence (design doc 25
        // §8.1): the device segment gets a `-presence` suffix, so this exact
        // id must keep CONNECTing. A `:presence` suffix would NOT — the shape
        // allows exactly three colon-separated identity segments, which is why
        // the suffix is a hyphen. Coupled to
        // `apps/acowork-mobile/src/main.tsx`.
        assert_eq!(
            remote_client_shape(
                "user:alice:mobile:3f9a2b1c-6d4e-4f70-8a1b-2c5d9e0f1a2b-presence"
            ),
            Some("alice")
        );
    }

    #[test]
    fn remote_client_shape_rejects_everything_else() {
        // Internal identities never allowed remotely (§7.2).
        assert_eq!(remote_client_shape("node:local"), None);
        assert_eq!(remote_client_shape("agent:com.example"), None);
        assert_eq!(remote_client_shape("gateway:publisher"), None);
        assert_eq!(remote_client_shape("user:service"), None);
        // Malformed shapes.
        assert_eq!(remote_client_shape("user:alice:web:mac-1"), None);
        assert_eq!(remote_client_shape("user:alice:desktop:"), None);
        assert_eq!(remote_client_shape("user::desktop:mac-1"), None);
        assert_eq!(remote_client_shape("user:al:ice:desktop:x"), None);
        assert_eq!(remote_client_shape("user:alice:desktop:a:b"), None);
        assert_eq!(remote_client_shape("alice:desktop:mac-1"), None);
        assert_eq!(remote_client_shape("random"), None);
        assert_eq!(remote_client_shape(""), None);
    }

    #[test]
    fn remote_connect_requires_matching_access_token() {
        let verifier = crate::http::test_support::verifier();
        let now = acowork_core::auth::now_unix();
        let alice = crate::http::test_support::access_token("alice", "user");

        // Matching subject + valid token → allowed.
        assert!(check_remote_connect_auth(
            "user:alice:desktop:mac-1",
            &alice,
            &verifier,
            now
        ));
        // Valid token worn under ANOTHER user's client id → rejected.
        assert!(!check_remote_connect_auth(
            "user:bob:desktop:mac-1",
            &alice,
            &verifier,
            now
        ));
        // Wrong password / refresh-kind token → rejected.
        assert!(!check_remote_connect_auth(
            "user:alice:desktop:mac-1",
            "wrong-token",
            &verifier,
            now
        ));
        let issuer = acowork_core::auth::TokenIssuer::new(ed25519_dalek::SigningKey::from_bytes(
            &{
                // Same seed as test_support — a refresh token from the
                // SAME issuer verifies as a signature but fails the
                // kind check.
                const S: [u8; 32] = [
                    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
                    0xdd, 0xee, 0xff, 0x00, 0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78,
                    0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1, 0xf0,
                ];
                S
            },
        ));
        let refresh = issuer.sign_refresh("alice", "alice.family-1", now);
        assert!(!check_remote_connect_auth(
            "user:alice:desktop:mac-1",
            &refresh,
            &verifier,
            now
        ));
    }

    use std::path::PathBuf;

    /// Build an auth context with in-memory (unpersisted) stores.
    fn test_ctx<'a>(
        auth_enabled: bool,
        enrollment: &'a EnrollmentTokenStore,
        node_tokens: &'a NodeTokenStore,
        publisher_token: Option<&'a str>,
        http_token: Option<&'a str>,
    ) -> ConnectAuthContext<'a> {
        ConnectAuthContext {
            auth_enabled,
            enrollment_tokens: enrollment,
            node_tokens,
            publisher_token,
            http_token,
        }
    }

    fn empty_enrollment() -> EnrollmentTokenStore {
        EnrollmentTokenStore::load(&PathBuf::from("/nonexistent"))
    }

    fn empty_node_tokens() -> NodeTokenStore {
        NodeTokenStore::load(&PathBuf::from("/nonexistent"))
    }

    #[test]
    fn auth_disabled_allows_everything() {
        let enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let ctx = test_ctx(false, &enrollment, &node_tokens, Some("p"), Some("h"));
        // Unknown client ids, empty passwords — all pass when disabled.
        assert!(check_connect_auth("node:local", "", "", &ctx));
        assert!(check_connect_auth("anything:else", "", "", &ctx));
        assert!(check_connect_auth("", "", "", &ctx));
    }

    #[test]
    fn node_accepts_node_token() {
        let enrollment = empty_enrollment();
        let mut node_tokens = empty_node_tokens();
        let token = node_tokens.upsert("gpu-1");
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        assert!(check_connect_auth("node:gpu-1", "", &token, &ctx));
        assert!(!check_connect_auth("node:gpu-1", "", "wrong", &ctx));
        // Unenrolled node — no node token, no enrollment token.
        assert!(!check_connect_auth("node:other", "", "wrong", &ctx));
    }

    #[test]
    fn node_rename_client_accepts_node_token() {
        // ADR-075 D4: `node:{id}:rename` (temporary rename client) uses
        // the node's own credential — and must never be accepted with
        // an enrollment token (rename is only for enrolled nodes).
        let enrollment = empty_enrollment();
        let mut node_tokens = empty_node_tokens();
        let token = node_tokens.upsert("gpu-1");
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        assert!(check_connect_auth("node:gpu-1:rename", "", &token, &ctx));
        assert!(!check_connect_auth("node:gpu-1:rename", "", "wrong", &ctx));
        assert!(!check_connect_auth("node:other:rename", "", &token, &ctx));
    }

    #[test]
    fn node_first_connect_accepts_enrollment_token() {
        let mut enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let tok = enrollment.create_token(std::time::Duration::from_secs(3600), None);
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        assert!(check_connect_auth("node:gpu-1", "", &tok, &ctx));

        // Consumed token is rejected on a later connect.
        assert!(enrollment.consume_token(&tok, "gpu-1"));
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        assert!(!check_connect_auth("node:gpu-1", "", &tok, &ctx));
    }

    #[test]
    fn agent_accepts_any_registered_node_token() {
        let enrollment = empty_enrollment();
        let mut node_tokens = empty_node_tokens();
        let token = node_tokens.upsert("gpu-1");
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        // Phase 5a simplification: ownership is not verified.
        assert!(check_connect_auth("agent:com.example", "", &token, &ctx));
        assert!(!check_connect_auth("agent:com.example", "", "wrong", &ctx));
    }

    #[test]
    fn publisher_accepts_internal_token() {
        let enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("pub-tok"), Some("h"));
        assert!(check_connect_auth("gateway:publisher", "", "pub-tok", &ctx));
        assert!(!check_connect_auth("gateway:publisher", "", "wrong", &ctx));
        // No publisher token configured → reject.
        let ctx = test_ctx(true, &enrollment, &node_tokens, None, Some("h"));
        assert!(!check_connect_auth("gateway:publisher", "", "pub-tok", &ctx));
    }

    #[test]
    fn internal_service_publishers_accept_publisher_token() {
        // ADR-084 §决策 4b: `user:service` / `doc:service` reuse the
        // startup-generated publisher token (the supervisor forwards it
        // when `mqtt.auth_enabled` is on) — without this the broker
        // would refuse their CONNECT and profile/tree signals stop.
        let enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("pub-tok"), Some("h"));
        assert!(check_connect_auth("user:service", "", "pub-tok", &ctx));
        assert!(check_connect_auth("doc:service", "", "pub-tok", &ctx));
        assert!(!check_connect_auth("user:service", "", "wrong", &ctx));
        assert!(!check_connect_auth("doc:service", "", "wrong", &ctx));
        // Per-process suffixes (user:service:{pid}, doc:service:{pid})
        // must still authenticate — the supervisor-spawned publisher
        // appends a process-unique suffix to avoid `Duplicate client_id`
        // on internal reconnect (see acowork-user / acowork-doc
        // `mqtt_publisher::client_id`); the broker keeps matching on
        // prefix and the same publisher token is shared.
        assert!(
            check_connect_auth("user:service:1234", "", "pub-tok", &ctx),
            "user:service:<pid> must still pass prefix match"
        );
        assert!(
            check_connect_auth("doc:service:1234", "", "pub-tok", &ctx),
            "doc:service:<pid> must still pass prefix match"
        );
        assert!(!check_connect_auth("user:service:1234", "", "wrong", &ctx));
        assert!(!check_connect_auth("doc:service:1234", "", "wrong", &ctx));
        // Prefix collisions: a `user:` desktop id must not be admitted
        // via the publisher prefix path — desktop ids fall through to
        // the `user:...:desktop:...` branch below.
        assert!(!check_connect_auth("user:evil", "", "pub-tok", &ctx));
        assert!(!check_connect_auth("doc:evil", "", "pub-tok", &ctx));
        // Structural prefix: a bare `user:serviceevil` / `doc:serviceevil`
        // (no colon after `service`) must NOT match the internal-publisher
        // branch — the broker requires a `:pid` suffix (or exact match)
        // so a malicious or buggy client cannot slip a near-collision
        // through. The publisher token check would still gate the
        // outcome, but the structural form here rejects it earlier and
        // consistently with the `user:*:desktop:*` shape below.
        assert!(
            !check_connect_auth("user:serviceevil", "", "pub-tok", &ctx),
            "user:serviceevil must not match the internal-publisher prefix"
        );
        assert!(
            !check_connect_auth("doc:serviceevil", "", "pub-tok", &ctx),
            "doc:serviceevil must not match the internal-publisher prefix"
        );
        // No publisher token configured → reject.
        let ctx = test_ctx(true, &enrollment, &node_tokens, None, Some("h"));
        assert!(!check_connect_auth("user:service", "", "pub-tok", &ctx));
        assert!(!check_connect_auth("doc:service", "", "pub-tok", &ctx));
    }

    #[test]
    fn desktop_accepts_http_token() {
        let enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("http-tok"));
        assert!(check_connect_auth("user:nicholas:desktop:mac-1", "", "http-tok", &ctx));
        assert!(!check_connect_auth("user:nicholas:desktop:mac-1", "", "wrong", &ctx));
        // No http token → reject desktop client ids.
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), None);
        assert!(!check_connect_auth("user:nicholas:desktop:mac-1", "", "http-tok", &ctx));
    }

    #[test]
    fn unknown_client_ids_are_rejected() {
        let enrollment = empty_enrollment();
        let node_tokens = empty_node_tokens();
        let ctx = test_ctx(true, &enrollment, &node_tokens, Some("p"), Some("h"));
        assert!(!check_connect_auth("user:nicholas:web:mac-1", "", "h", &ctx));
        assert!(!check_connect_auth("random", "", "", &ctx));
        assert!(!check_connect_auth("node:", "", "", &ctx), "empty node id");
    }

    #[tokio::test]
    async fn test_broker_starts_and_accepts_connections() {
        // Use a non-default port to avoid conflicts with a running Gateway.
        let port = 18975; // different from default 19875
        let host = "127.0.0.1";

        // Threaded mode: rumqttd's `Broker::start()` blocks while the
        // broker lives (it joins the server threads, whose accept loops
        // never exit), so calling it from the test thread would hang
        // whenever the port is free. `start_broker` supervises it on a
        // background OS thread and returns immediately.
        let handle = start_broker(host, port).expect("broker should start");
        assert_eq!(handle.listen_addr.port(), port);

        // Verify the broker is listening by connecting a rumqttc client.
        use rumqttc::{AsyncClient, MqttOptions, QoS};
        use std::time::Duration;

        let mut mqttoptions = MqttOptions::new("test:broker_smoke", host, port);
        mqttoptions.set_keep_alive(Duration::from_secs(5));

        let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

        // Poll the event loop a few times to establish the connection.
        let mut connected = false;
        for _ in 0..20 {
            match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Incoming::ConnAck(_))) => {
                    connected = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        assert!(connected, "rumqttc client should connect to the broker");

        // Publish and subscribe smoke test
        client
            .subscribe("acowork/test/#", QoS::AtLeastOnce)
            .await
            .expect("subscribe should succeed");

        // Give the broker a moment to process the subscription
        tokio::time::sleep(Duration::from_millis(50)).await;

        client
            .publish(
                "acowork/test/hello",
                QoS::AtLeastOnce,
                false,
                b"smoke test",
            )
            .await
            .expect("publish should succeed");

        // Dropping the handle releases the shutdown channel; the broker
        // thread parks until process exit, when the OS reclaims the
        // listener. The test never joins rumqttd's threads, so it cannot
        // hang on shutdown.
        drop(handle);
        drop(client);
    }

    /// Bind a loopback port and hold it, so the broker cannot take it.
    fn hold_port() -> (std::net::TcpListener, u16) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind blocker");
        let port = l.local_addr().expect("blocker addr").port();
        (l, port)
    }

    /// Await a state predicate, failing the test if it never arrives.
    async fn await_state(
        rx: &mut tokio::sync::watch::Receiver<BrokerState>,
        budget: std::time::Duration,
        what: &str,
        pred: impl Fn(&BrokerState) -> bool,
    ) -> BrokerState {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let current = rx.borrow_and_update().clone();
            if pred(&current) {
                return current;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "never reached {what}; last state {current:?}");
            tokio::time::timeout(remaining, rx.changed())
                .await
                .unwrap_or_else(|_| panic!("never reached {what}; last state {current:?}"))
                .expect("broker supervisor watch closed");
        }
    }

    /// The core M3 regression: a taken port must NOT read as "started".
    ///
    /// rumqttd logs the bind failure inside its server thread and
    /// `Broker::start()` returns Ok(()) — the old code treated that as
    /// success and reported a live broker on a dead port. The supervisor
    /// must instead leave `Starting` on its own, publish `Retrying`, and
    /// recover once the port is released.
    #[tokio::test]
    async fn bind_conflict_reports_retrying_then_recovers() {
        let (blocker, port) = hold_port();
        let handle = start_broker("127.0.0.1", port).expect("supervisor should spawn");
        let mut rx = handle.state_rx();

        // Bounded exit from Starting without any consumer deadline.
        let state = await_state(&mut rx, std::time::Duration::from_secs(5), "Retrying", |s| {
            matches!(s, BrokerState::Retrying { .. })
        })
        .await;
        match state {
            BrokerState::Retrying { attempt, retry_in, .. } => {
                assert!(attempt >= 1, "attempt counter must advance");
                assert!(!retry_in.is_zero(), "retry must be scheduled");
            }
            other => panic!("expected Retrying, got {other:?}"),
        }
        assert!(!handle.state().is_listening());

        // Release the port: the supervisor rebuilds the broker and the
        // probe confirms it. No process restart involved.
        drop(blocker);
        await_state(&mut rx, std::time::Duration::from_secs(10), "Listening", |s| {
            s.is_listening()
        })
        .await;
        assert!(handle.wait_listening(std::time::Duration::from_millis(1)).await);
    }

    /// A consumer budget expires without killing anything: `false` means
    /// "not yet", and the supervisor keeps working afterwards.
    #[tokio::test]
    async fn wait_listening_budget_is_not_destructive() {
        let (blocker, port) = hold_port();
        let handle = start_broker("127.0.0.1", port).expect("supervisor should spawn");
        assert!(
            !handle
                .wait_listening(std::time::Duration::from_millis(200))
                .await,
            "a held port cannot be listening"
        );
        drop(blocker);
        assert!(
            handle.wait_listening(std::time::Duration::from_secs(10)).await,
            "the same handle must still recover after an expired budget"
        );
    }

    /// The probe must authenticate: with `auth_enabled` the broker
    /// refuses an uncredentialed CONNECT, and a probe that could not get
    /// CONNACK would report a healthy broker as never-listening.
    #[tokio::test]
    async fn probe_authenticates_when_auth_is_enabled() {
        let port = crate::test_support::free_ports(1)[0];
        let auth = BrokerAuth {
            auth_enabled: true,
            enrollment_tokens: Arc::new(std::sync::Mutex::new(empty_enrollment())),
            node_tokens: Arc::new(std::sync::Mutex::new(empty_node_tokens())),
            publisher_token: "probe-token".to_string(),
            http_token: None,
        };
        let handle = start_broker_with_auth("127.0.0.1", port, Some(auth), None)
            .expect("supervisor should spawn");
        assert!(
            handle
                .wait_listening(std::time::Duration::from_secs(5))
                .await,
            "probe must CONNECT successfully with the publisher token"
        );
    }

    /// A socket that holds the port without speaking MQTT must not read
    /// as a live broker — the false positive a bare TCP connect gives.
    #[test]
    fn connack_probe_rejects_a_silent_port_holder() {
        let (blocker, port) = hold_port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        assert!(
            !mqtt_connack_probe(&addr, ""),
            "a backlog-only socket answers TCP but never CONNACK"
        );
        drop(blocker);
    }

    /// Removing the bind-then-drop pre-probe must not regress isolation:
    /// rumqttd runs each server on its own thread, so a remote bind
    /// conflict leaves the main listener serving.
    #[tokio::test]
    async fn remote_port_conflict_keeps_main_listener_alive() {
        let main_port = crate::test_support::free_ports(1)[0];
        let (_remote_blocker, remote_port) = hold_port();
        let dir = tempfile::tempdir().expect("tempdir");
        let gw = crate::gateway::state::GatewayState::new(&dir.path().to_string_lossy());
        let remote = RemoteMqttListener {
            host: "127.0.0.1".to_string(),
            port: remote_port,
            auth: RemoteMqttAuth {
                gateway_state: Arc::new(tokio::sync::RwLock::new(gw)),
            },
        };
        let handle = start_broker_with_auth("127.0.0.1", main_port, None, Some(remote))
            .expect("supervisor should spawn");
        assert!(
            handle
                .wait_listening(std::time::Duration::from_secs(5))
                .await,
            "main listener must serve despite the remote port conflict"
        );
    }
}
