//! Node control-plane end-to-end test (ADR-055 Phase 2a §7.1 + Phase 5a).
//!
//! Spawns a real `acowork-node` binary against an in-process broker and
//! verifies the full MQTT contract:
//!
//!   1. `acowork/nodes/{id}/status` → retained "online" on connect;
//!   2. `acowork/nodes/{id}/info` → retained NodeInfo envelope;
//!   3. `acowork/nodes/{id}/control/ping` → NodeEvent "pong" on
//!      `acowork/nodes/{id}/events`;
//!   4. request_id dedup: re-sending the same ping yields a second
//!      pong (cached reply, no re-execution observable at this layer);
//!   5. LWT: SIGKILL-ing the node flips status to retained "offline";
//!   6. **Phase 5a auth**: with an auth-enabled broker, a node booting
//!      with an enrollment token enrolls (enroll → enroll_result),
//!      persists the minted node_token, and reconnects with it after a
//!      restart; anonymous / wrong-token node connections are rejected
//!      (CONNACK error).
//!
//! The node binary is a sibling of the test harness (built by the
//! workspace). When it is missing (e.g. `cargo test -p acowork-gateway`
//! in isolation), the test is SKIPPED rather than failed — the
//! dependency on the compiled binary is a build-ordering concern, not
//! a code-correctness one.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prost::Message as _;
use rumqttc::{AsyncClient, ConnectReturnCode, Event, Incoming, MqttOptions, QoS};
use tokio::sync::mpsc;

use acowork_core::mqtt_proto::{
    data_envelope, node_control_command, DataEnvelope, NodeControlCommand, NodeEnroll,
    NodeEnrollResult, NodeEvent, NodeInfo,
};
use acowork_core::node::{node_enroll_result_topic, node_info_topic, node_status_topic};
use acowork_gateway::mqtt::enrollment::{
    EnrollmentTokenStore, NodeTokenStore, SharedEnrollmentTokenStore, SharedNodeTokenStore,
    TokenValidation,
};
use acowork_gateway::mqtt::node_registry::{local_node_id, new_shared_registry};
use acowork_gateway::mqtt::{start_broker_with_auth, BrokerAuth};

const NODE_ID: &str = "verify-node";
const TEST_PORT: u16 = 18990;
/// Phase 5a auth scenarios run on isolated ports so the anonymous
/// control-plane test and the auth tests can run in parallel.
const AUTH_TEST_PORT: u16 = 18991;
const AUTH_REJECT_PORT: u16 = 18992;
/// Reverse-proxy port for the auth happy-path node — distinct from the
/// default 19900 so it does not collide with the control-plane test's
/// node when both tests run in parallel.
const AUTH_PROXY_PORT: u16 = 19901;
const AUTH_NODE_ID: &str = "auth-node";
const PUBLISHER_TOKEN: &str = "e2e-publisher-token";
/// Invalid-instance-id gate test uses an isolated port so its parallel
/// run does not collide with the other three tests' brokers.
const GATE_TEST_PORT: u16 = 18993;
const GATE_NODE_ID: &str = "gate-node";
const GATE_NODE_PROXY_PORT: u16 = 19902;
/// ADR-075 D4: rename runs on an isolated port so it can run in
/// parallel with the other tests.
const RENAME_TEST_PORT: u16 = 18994;
const RENAME_NODE_ID: &str = "rename-node";
/// ADR-075 D5: the gateway-managed boot marker runs on an isolated port.
const GW_MANAGED_TEST_PORT: u16 = 18995;
const GW_MANAGED_NODE_ID: &str = "gw-managed-node";

/// Locate the compiled `acowork-node` binary (workspace target dir).
fn node_binary() -> Option<std::path::PathBuf> {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        // isolated test target dir (CARGO_TARGET_DIR=target_test)
        manifest.join("../../target_test/debug/acowork-node.exe"),
        manifest.join("../../target_test/debug/acowork-node"),
        // workspace target-dir layout (core/acowork-gateway → ../../target)
        manifest.join("../../target/debug/acowork-node"),
        manifest.join("../../target/debug/acowork-node.exe"),
        // standalone crate layout (core/acowork-node/target/debug)
        manifest.join("../acowork-node/target/debug/acowork-node"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

struct Collector {
    client: AsyncClient,
    rx: mpsc::Receiver<(String, Vec<u8>)>,
}

impl Collector {
    /// Connect and subscribe to `acowork/nodes/#`.
    ///
    /// `client_id` / `credentials` are for Phase 5a auth-enabled
    /// brokers — the collector impersonates the Gateway's internal
    /// publisher (`gateway:publisher` + its token) so it can observe
    /// node traffic; the anonymous `test:node-e2e` client is used for
    /// the auth-disabled control-plane test.
    fn connect(port: u16, client_id: &str, credentials: Option<(&str, &str)>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        opts.set_clean_session(true);
        if let Some((user, pass)) = credentials {
            opts.set_credentials(user.to_string(), pass.to_string());
        }
        let (client, mut eventloop) = AsyncClient::new(opts, 64);
        let poll_client = client.clone();

        tokio::spawn(async move {
            poll_client
                .subscribe("acowork/nodes/#", QoS::AtLeastOnce)
                .await
                .ok();
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        let _ = tx.send((p.topic, p.payload.to_vec())).await;
                    }
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {}
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        Self { client, rx }
    }

    /// Publish through the collector's own connection (Phase 5a: the
    /// collector impersonates the Gateway publisher, so it is the only
    /// client that can answer the node's enroll request without a
    /// second `gateway:publisher` connection bumping it off the broker).
    async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<(), String> {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await
            .map_err(|e| e.to_string())
    }

    async fn wait_for_topic(&mut self, suffix: &str, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, self.rx.recv()).await {
                Ok(Some((topic, payload))) => {
                    if topic.ends_with(suffix) {
                        return Some(payload);
                    }
                }
                _ => return None,
            }
        }
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_binary_speaks_the_control_plane_contract() {
    let Some(bin) = node_binary() else {
        eprintln!("SKIP: acowork-node binary not found — run `cargo build --workspace` first");
        return;
    };

    // Broker on an isolated port (the real Gateway owns 19875).
    let broker_handle =
        acowork_gateway::mqtt::start_broker("127.0.0.1", TEST_PORT).expect("broker starts");

    // Fresh node home so identity is minted for this run.
    let home = tempfile::tempdir().unwrap();
    let node_home = home.path().join("node-home");

    let mut child = std::process::Command::new(&bin)
        .args([
            "start",
            "--gateway",
            &format!("127.0.0.1:{TEST_PORT}"),
            "--name",
            NODE_ID,
            "--home",
        ])
        .arg(&node_home)
        // ADR-087: on a fresh home the node asks for account credentials on
        // stdin when stdin is a TTY. `cargo test` from an interactive terminal
        // inherits the console, so the child would block on the "Username:"
        // prompt and never write identity.json — close stdin to keep the
        // service-style (credential-less) startup the assertions expect.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn node");

    let mut collector = Collector::connect(TEST_PORT, "test:node-e2e", None);

    // ADR-075 D1: the node_id is a UUID v4 minted at first start —
    // read it back from identity.json (the `--name` value is now the
    // display-only node_name).
    let node_id = wait_for_identity_node_id(&node_home.join("identity.json"), Duration::from_secs(10))
        .await
        .expect("node_id persisted to identity.json");
    assert_ne!(node_id, NODE_ID, "node_id is a UUID, not the --name slug");

    // 1) status = online (retained).
    let status = collector
        .wait_for_topic(&format!("/nodes/{node_id}/status"), Duration::from_secs(10))
        .await
        .expect("status topic");
    assert_eq!(std::str::from_utf8(&status).unwrap().trim(), "online");

    // 2) info = NodeInfo envelope (retained).
    let info_bytes = collector
        .wait_for_topic(&format!("/nodes/{node_id}/info"), Duration::from_secs(10))
        .await
        .expect("info topic");
    let info = DataEnvelope::decode(&info_bytes[..])
        .expect("info decodes")
        .payload
        .expect("info payload");
    let data_envelope::Payload::NodeInfo(info) = info else {
        panic!("expected NodeInfo");
    };
    assert_eq!(info.node_id, node_id);
    assert_eq!(info.node_name, NODE_ID, "--name becomes node_name (ADR-075 D2)");
    assert_eq!(info.protocol_version, acowork_core::node::NODE_PROTOCOL_VERSION);

    // 3) ping → pong.
    send_ping(&node_id, "req-e2e-1").await;
    let pong = collector
        .wait_for_topic(&format!("/nodes/{node_id}/events"), Duration::from_secs(5))
        .await
        .expect("pong event");
    let event = decode_event(&pong);
    assert_eq!(event.request_id, "req-e2e-1");
    assert_eq!(event.status, "ok");
    assert_eq!(event.message, "pong");

    // 4) request_id dedup: same id → a second (cached) reply.
    send_ping(&node_id, "req-e2e-1").await;
    let dup_pong = collector
        .wait_for_topic(&format!("/nodes/{node_id}/events"), Duration::from_secs(5))
        .await
        .expect("duplicate pong");
    assert_eq!(decode_event(&dup_pong).request_id, "req-e2e-1");

    // 5) LWT: hard-kill → broker publishes retained "offline".
    let _ = child.kill();
    let offline = collector
        .wait_for_topic(&format!("/nodes/{node_id}/status"), Duration::from_secs(10))
        .await
        .expect("offline status");
    assert_eq!(std::str::from_utf8(&offline).unwrap().trim(), "offline");

    // Cleanup.
    let _ = child.wait();
    drop(broker_handle);
}

async fn send_ping(node_id: &str, request_id: &str) {
    let cmd = NodeControlCommand {
        node_id: node_id.to_string(),
        request_id: request_id.to_string(),
        command: Some(node_control_command::Command::Ping(Default::default())),
    };
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeControlCommand(cmd)),
    };
    let mut opts = MqttOptions::new("test:node-e2e-pinger", "127.0.0.1", TEST_PORT);
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = AsyncClient::new(opts, 16);
    client
        .publish(
            format!("acowork/nodes/{node_id}/control/ping"),
            QoS::AtLeastOnce,
            false,
            envelope.encode_to_vec(),
        )
        .await
        .expect("publish ping");
    // rumqttc queues requests; the event loop must run to flush the
    // CONNECT + PUBLISH out the socket. Poll for a short window.
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(100), eventloop.poll()).await;
    }
    drop(eventloop);
}

fn decode_event(bytes: &[u8]) -> NodeEvent {
    let envelope = DataEnvelope::decode(bytes).expect("event decodes");
    match envelope.payload.expect("event payload") {
        data_envelope::Payload::NodeEvent(event) => event,
        _ => panic!("expected NodeEvent"),
    }
}

/// Decode a `DataEnvelope<NodeEnroll>` payload.
fn decode_enroll(bytes: &[u8]) -> NodeEnroll {
    let envelope = DataEnvelope::decode(bytes).expect("enroll decodes");
    match envelope.payload.expect("enroll payload") {
        data_envelope::Payload::NodeEnroll(enroll) => enroll,
        _ => panic!("expected NodeEnroll"),
    }
}

/// Poll `identity.json` until the Gateway-issued `node_token` appears
/// (written by the node's enroll_result handler).
async fn wait_for_identity_token(path: &std::path::Path, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(path)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(token) = json.get("node_token").and_then(|v| v.as_str())
        {
            return Some(token.to_string());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// Poll `identity.json` until the `node_id` appears (written on first
/// start — ADR-075 D1: node_id is a UUID v4, NOT derivable from
/// `--name`, so tests must read it back to build the node topics).
async fn wait_for_identity_node_id(path: &std::path::Path, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(content) = std::fs::read_to_string(path)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(id) = json.get("node_id").and_then(|v| v.as_str())
        {
            return Some(id.to_string());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// Whether the broker rejects a CONNECT with the given client_id /
/// password (CONNACK error or immediate disconnect). `false` when the
/// connection is accepted.
async fn expect_connect_rejected(port: u16, client_id: &str, password: Option<&str>) -> bool {
    let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    if let Some(p) = password {
        opts.set_credentials(client_id.to_string(), p.to_string());
    }
    let (_client, mut eventloop) = AsyncClient::new(opts, 16);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, eventloop.poll()).await {
            // Poll error = broker refused / dropped the connection.
            Ok(Err(_)) => return true,
            Ok(Ok(Event::Incoming(Incoming::ConnAck(ack)))) => {
                return ack.code != ConnectReturnCode::Success;
            }
            Ok(Ok(_)) => continue,
            Err(_) => return false, // timeout without error — still alive
        }
    }
    false
}

/// Phase 5a happy path: enroll → node_token → credential reconnect.
///
/// The test plays the Gateway's enrollment half (validate → consume →
/// mint → enroll_result) against the real node binary; the broker runs
/// with `auth_enabled`, so every CONNECT must present a valid token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_enrolls_and_reconnects_with_node_token_under_auth() {
    let Some(bin) = node_binary() else {
        eprintln!("SKIP: acowork-node binary not found — run `cargo build --workspace` first");
        return;
    };

    // ── Auth-enabled broker + fresh token stores ───────────────────────
    let gw_home = tempfile::tempdir().unwrap();
    let mut enrollment_store = EnrollmentTokenStore::load(gw_home.path());
    let enrollment_token = enrollment_store.create_token(Duration::from_secs(3600), None);
    let enrollment_tokens: SharedEnrollmentTokenStore = Arc::new(Mutex::new(enrollment_store));
    let node_tokens: SharedNodeTokenStore = Arc::new(Mutex::new(NodeTokenStore::load(gw_home.path())));

    let auth = BrokerAuth {
        auth_enabled: true,
        enrollment_tokens: enrollment_tokens.clone(),
        node_tokens: node_tokens.clone(),
        publisher_token: PUBLISHER_TOKEN.to_string(),
        http_token: Some("e2e-http-token".to_string()),
    };
    let broker_handle = start_broker_with_auth("127.0.0.1", AUTH_TEST_PORT, Some(auth), None)
        .expect("auth broker starts");

    // The collector impersonates the Gateway publisher — the only
    // credential an observer can use on an auth-enabled broker.
    let mut collector = Collector::connect(
        AUTH_TEST_PORT,
        "gateway:publisher",
        Some(("gateway:publisher", PUBLISHER_TOKEN)),
    );

    // ── 1) Node boots with the enrollment token ────────────────────────
    let home = tempfile::tempdir().unwrap();
    let node_home = home.path().join("node-home");
    let mut child = std::process::Command::new(&bin)
        .args([
            "start",
            "--gateway",
            &format!("127.0.0.1:{AUTH_TEST_PORT}"),
            "--name",
            AUTH_NODE_ID,
            "--proxy-port",
            &AUTH_PROXY_PORT.to_string(),
            "--token",
            &enrollment_token,
            "--home",
        ])
        .arg(&node_home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn node");

    // ── 2) Enroll request arrives; act as the Gateway ──────────────────
    // ADR-075 D1: the node_id is a UUID v4 minted at first start —
    // read it back from identity.json and key the whole handshake on it
    // (the --name value is the display-only node_name).
    let identity_path = node_home.join("identity.json");
    let node_id = wait_for_identity_node_id(&identity_path, Duration::from_secs(10))
        .await
        .expect("node_id persisted to identity.json");
    assert_ne!(node_id, AUTH_NODE_ID, "node_id is a UUID, not the --name slug");

    let enroll_bytes = collector
        .wait_for_topic(&format!("/nodes/{node_id}/enroll"), Duration::from_secs(10))
        .await
        .expect("enroll request");
    let enroll = decode_enroll(&enroll_bytes);
    assert_eq!(enroll.node_id, node_id);
    assert_eq!(
        enroll.enrollment_token, enrollment_token,
        "node presents the --token in the enroll payload"
    );

    // Gateway-side handshake: validate → consume → mint node token.
    {
        let mut store = enrollment_tokens.lock().unwrap();
        assert_eq!(
            store.validate_token(&enrollment_token),
            TokenValidation::Valid,
            "one-time enrollment token is still valid"
        );
        assert!(store.consume_token(&enrollment_token, &node_id));
    }
    let node_token = node_tokens.lock().unwrap().upsert(&node_id);
    assert!(!node_token.is_empty());

    // Answer with `enroll_result` (QoS 1, non-retained).
    let result = NodeEnrollResult {
        node_id: node_id.clone(),
        node_token: node_token.clone(),
        status: "ok".to_string(),
        message: String::new(),
    };
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeEnrollResult(result)),
    };
    collector
        .publish(
            &node_enroll_result_topic(&node_id),
            envelope.encode_to_vec(),
        )
        .await
        .expect("publish enroll_result");
    // ── 3) Node persists the token and reconnects with it ──────────────
    let persisted = wait_for_identity_token(&identity_path, Duration::from_secs(10))
        .await
        .expect("node_token persisted to identity.json");
    assert_eq!(persisted, node_token, "identity.json carries the minted token");

    // Hard-kill, then restart WITHOUT the enrollment token — the node
    // must reconnect using the persisted node_token (broker-side
    // `node:{id}` rule). Success is proven by the retained status=online
    // replay, which only happens after a successful ConnAck.
    let _ = child.kill();
    let _ = child.wait();
    let mut child2 = std::process::Command::new(&bin)
        .args([
            "start",
            "--gateway",
            &format!("127.0.0.1:{AUTH_TEST_PORT}"),
            "--name",
            AUTH_NODE_ID,
            "--proxy-port",
            &AUTH_PROXY_PORT.to_string(),
            "--home",
        ])
        .arg(&node_home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("re-spawn node");

    // The hard-kill disconnects the first process, so the broker first
    // replays its retained LWT "offline"; keep reading until the
    // restarted node (reconnected with the persisted node_token)
    // publishes its own retained "online".
    let status = loop {
        let msg = collector
            .wait_for_topic(&format!("/nodes/{node_id}/status"), Duration::from_secs(10))
            .await
            .expect("status after node_token reconnect");
        if std::str::from_utf8(&msg).unwrap_or("").trim() == "online" {
            break msg;
        }
    };
    assert_eq!(std::str::from_utf8(&status).unwrap().trim(), "online");

    // Cleanup.
    let _ = child2.kill();
    let _ = child2.wait();
    drop(broker_handle);
}

// ═══════════════════════════════════════════════════════════════════════
// ADR-073 P2-2: the control-plane entry gate rejects malformed
// `instance_id` payloads BEFORE any fs/process side effect.
//
// Scenario:
// 1. Spin up a real node binary against an isolated broker.
// 2. Send a `NodeStart` with `instance_id = "not-a-uuid"`.
// 3. Assert the node replies on
//    `acowork/nodes/{id}/agents/{instance_id}/events` with
//    `status="error"` mentioning UUID.
// 4. Assert NO retained `acowork/agents/{instance_id}/status` appears —
//    the invalid id never reached spawn() so no Runtime was started.
// 5. Positive control: send a second `NodeStart` with a valid UUID
//    for a real agent package — the gate lets it through (no error
//    reply with the "invalid instance_id" message shape).
//
// This is the e2e counterpart of the three unit tests in
// `acowork-node/src/control/mod.rs` (`handle_command` gate) — it
// proves the gate runs BEFORE any spawn / fs call by checking for the
// absence of a retained Runtime status topic.
// ═══════════════════════════════════════════════════════════════════════

/// Send a `NodeStart` carrying a specific `instance_id` to the
/// per-agent control topic. Uses an INDEPENDENT AsyncClient + a
/// short flush window (same pattern as the existing `send_ping` in
/// this file). The Collector-based path is unsuitable here because
/// Collector relies on a background eventloop task that silently
/// exits on error — if it dies, the queued publish never flushes.
async fn send_start_command(
    port: u16,
    node_id: String,
    instance_id: &str,
    request_id: &str,
) {
    let cmd = NodeControlCommand {
        node_id: node_id.to_string(),
        request_id: request_id.to_string(),
        command: Some(node_control_command::Command::Start(
            acowork_core::mqtt_proto::NodeStart {
                agent_id: "com.test.gate-agent".to_string(),
                dev_mode: false,
                instance_id: instance_id.to_string(),
            },
        )),
    };
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeControlCommand(cmd)),
    };
    let mut opts = MqttOptions::new(
        format!("e2e:gate-publisher:{request_id}"),
        "127.0.0.1",
        port,
    );
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = AsyncClient::new(opts, 16);
    client
        .publish(
            acowork_core::node::node_agent_control_topic(&node_id, instance_id, "start"),
            QoS::AtLeastOnce,
            false,
            envelope.encode_to_vec(),
        )
        .await
        .expect("publish start");
    // Flush window — same as send_ping.
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(100), eventloop.poll()).await;
    }
    drop(eventloop);
}

/// Poll `acowork/nodes/{id}/agents/{instance_id}/events` until the
/// matching event lands (or timeout). The existing
/// `Collector::wait_for_topic` matches only by suffix, so this test
/// uses a purpose-built subscriber scoped to the per-agent events
/// topic — a malformed id never produces anything, and a valid id
/// produces a single NodeEvent we need to inspect.
async fn wait_for_agent_event(
    port: u16,
    node_id: String,
    instance_id: &str,
    request_id: &str,
    timeout: Duration,
) -> Option<NodeEvent> {
    let mut opts = MqttOptions::new(
        format!("e2e:gate-listener:{request_id}"),
        "127.0.0.1",
        port,
    );
    opts.set_clean_session(true);
    let (client, mut eventloop) = AsyncClient::new(opts, 16);

    let topic = acowork_core::node::node_agent_events_topic(&node_id, instance_id);
    client
        .subscribe(&topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe agent events");

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, eventloop.poll()).await {
            Ok(Ok(Event::Incoming(Incoming::Publish(p)))) => {
                let Ok(env) = DataEnvelope::decode(p.payload.as_ref()) else {
                    continue;
                };
                if let Some(data_envelope::Payload::NodeEvent(ev)) = env.payload
                    && ev.request_id == request_id
                {
                    return Some(ev);
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return None,
            Err(_) => return None,
        }
    }
    None
}

/// Did the broker ever publish a retained status for `instance_id`?
/// A "yes" means the gate let the start command through and a Runtime
/// process came up; a "no" means the gate blocked before spawn.
async fn retained_status_exists(
    port: u16,
    instance_id: &str,
    timeout: Duration,
) -> bool {
    let mut opts = MqttOptions::new(
        format!("e2e:gate-retained-check:{instance_id}"),
        "127.0.0.1",
        port,
    );
    opts.set_clean_session(true);
    let (client, mut eventloop) = AsyncClient::new(opts, 16);

    client
        .subscribe(
            format!("acowork/agents/{instance_id}/status"),
            QoS::AtLeastOnce,
        )
        .await
        .expect("subscribe status");

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, eventloop.poll()).await {
            Ok(Ok(Event::Incoming(Incoming::Publish(p)))) => {
                // Any retained payload means a Runtime is alive.
                if !p.payload.is_empty() {
                    return true;
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return false,
            Err(_) => return false,
        }
    }
    false
}

/// RAII guard that guarantees the spawned `acowork-node` process is
/// killed and reaped on every exit path — normal completion, panic,
/// timeout, or early return. Without this, a hung test leaves a node
/// process bound to its test port and `--name` slot, and the next
/// test run collides with the orphan (and accumulates 6+ zombies
/// during a session — see the cleanup note in `docs/plan/...`).
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        kill_tree(&mut self.0);
    }
}

/// Kill the child **and every process it spawned**.
///
/// `Child::kill()` is TerminateProcess on Windows: it reaps only the direct
/// child, so the services that child hosted (embed / node / doc / pm / user)
/// survive until their ADR-018 watchdog notices the Gateway is gone (5 min),
/// holding their ports and — on Windows — a lock on `acowork-*.exe` that
/// fails the next `cargo build`. `taskkill /T` reaps the tree with it.
///
/// Only ever our own child — never a pattern match on process names. Unix
/// needs no equivalent here: the watchdog is the designed backstop and a
/// running binary does not lock its own path there.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .output();
    let _ = child.kill();
    let _ = child.wait();
}


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_control_rejects_invalid_instance_id_before_spawn() {
    let Some(bin) = node_binary() else {
        eprintln!("SKIP: acowork-node binary not found — run `cargo build --workspace` first");
        return;
    };

    let broker_handle =
        acowork_gateway::mqtt::start_broker("127.0.0.1", GATE_TEST_PORT).expect("broker starts");

    let home = tempfile::tempdir().unwrap();
    let node_home = home.path().join("node-home");

    let child = std::process::Command::new(&bin)
        .args([
            "start",
            "--gateway",
            &format!("127.0.0.1:{GATE_TEST_PORT}"),
            "--name",
            GATE_NODE_ID,
            "--proxy-port",
            &GATE_NODE_PROXY_PORT.to_string(),
            "--home",
        ])
        .arg(&node_home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn node");
    let child_guard = ChildGuard(child);

    // ADR-075 D1: resolve the UUID node_id from identity.json (the
    // `--name` value is the display-only node_name).
    let node_id = wait_for_identity_node_id(&node_home.join("identity.json"), Duration::from_secs(10))
        .await
        .expect("node_id persisted to identity.json");

    // Hard timeout around the whole body so a hang cannot leak the
    // node past the test harness. Drop guard still fires on unwind.
    let result: Result<(), String> = async {
        let mut collector = Collector::connect(GATE_TEST_PORT, "test:gate-publisher", None);

        // Wait for the node to come online and for its control
        // subscription (`acowork/nodes/{id}/agents/+/control/#`) to be
        // ready. The status=online retained publish happens AFTER the
        // control subscriptions land (control/mod.rs:969-979), so
        // seeing status implies the agent control filter is live.
        let status = collector
            .wait_for_topic(&format!("/nodes/{node_id}/status"), Duration::from_secs(10))
            .await
            .expect("node online");
        assert_eq!(std::str::from_utf8(&status).unwrap().trim(), "online");

        // ── 1) Negative path: malformed instance_id must be rejected. ──
        //
        // CRITICAL: subscribe BEFORE publishing. The Node reply is a
        // QoS-1 non-retained message — if our subscriber connects
        // after the reply is published, the broker drops it
        // silently. The Collector pattern used by the rest of this
        // file relies on the same race-free ordering (Collector::
        // connect spawns its eventloop with the SUBSCRIBE already in
        // the queue). See the same race in test 1 below.
        let bad_id = "not-a-uuid";
        // ADR-073: the node's reply topic is keyed by the URL segment
        // after `agents/` — i.e. exactly the value we are testing for
        // rejection. Subscribing to that topic lets the handler's
        // "invalid instance_id" error reply reach us. (The handler
        // returns the reply before any side effect, so the segment
        // echo is intentional — the malformed id never touches FS
        // or queue.)
        let bad_listener = tokio::spawn(wait_for_agent_event(
            GATE_TEST_PORT,
            node_id.clone(),
            bad_id,
            "req-bad-1",
            Duration::from_secs(8),
        ));
        // Give the SUBSCRIBE packet time to flush before we publish.
        tokio::time::sleep(Duration::from_millis(500)).await;
        send_start_command(GATE_TEST_PORT, node_id.clone(), bad_id, "req-bad-1").await;

        let reply = bad_listener
            .await
            .expect("subscriber task panicked")
            .expect("node must reply even for an invalid id");

        assert_eq!(
            reply.status, "error",
            "gate must reply status=error for invalid instance_id, got: {:?}",
            reply
        );
        assert!(
            reply.message.contains("invalid instance_id")
                && reply.message.contains("UUID"),
            "error message must name the gate reason; got: {:?}",
            reply.message
        );

        // ── 2) The gate MUST have blocked before spawn — no Runtime
        //    retained status appears for the bad id. ──
        assert!(
            !retained_status_exists(GATE_TEST_PORT, bad_id, Duration::from_secs(2)).await,
            "no Runtime process must have been spawned for the invalid id"
        );

        // ── 3) Empty-string instance_id must also be rejected —
        //    one of TWO defence layers will catch it:
        //    (a) `parse_control_topic` returns None when the
        //        `agents/.../control/{cmd}` path part is empty (the
        //        segment between the slashes is empty), so no
        //        `handle_command` ever runs and the node never
        //        publishes a reply;
        //    (b) the UUID gate inside `handle_command` returns an
        //        `error` NodeEvent.
        //    Either outcome is correct — the test passes if the
        //    empty string does NOT spawn a Runtime. ──
        let empty_listener = tokio::spawn(wait_for_agent_event(
            GATE_TEST_PORT,
            node_id.clone(),
            // Empty segment ⇒ `parse_control_topic` returns `None` and
            // the command is silently dropped before any handler runs.
            // Subscribe to the per-agent events topic that *would* be
            // used if a handler ever fired; the contract under test is
            // that the listener times out without seeing a reply.
            "empty-listener",
            "req-bad-2",
            Duration::from_secs(5),
        ));
        tokio::time::sleep(Duration::from_millis(500)).await;
        send_start_command(GATE_TEST_PORT, node_id.clone(), "", "req-bad-2").await;
        let empty_result = empty_listener
            .await
            .expect("subscriber task panicked");
        if let Some(reply) = empty_result {
            // Layer (b): gate rejected — must be the gate error, not
            // anything else (e.g. an "Agent not found" from a
            // maliciously crafted id bypassing the gate).
            assert_eq!(
                reply.status, "error",
                "empty-string id must surface as error from the gate, got: {:?}",
                reply
            );
            assert!(
                reply.message.contains("invalid instance_id"),
                "empty-string id must hit the same gate, got: {:?}",
                reply.message
            );
        }
        // else: layer (a) — parse_control_topic dropped the message
        // silently. Either outcome is acceptable; what matters is the
        // final assertion below.

        // ── 4) No Runtime was spawned for ANY malformed id. ──
        assert!(
            !retained_status_exists(GATE_TEST_PORT, "", Duration::from_secs(2)).await,
            "no Runtime process must have been spawned for empty-string id"
        );

        Ok(())
    }
    .await;

    // Drop child BEFORE broker so the LWT fires against a still-live
    // broker (LWT is only delivered to live connections).
    drop(child_guard);
    drop(broker_handle);

    if let Err(msg) = result {
        panic!("{msg}");
    }
}

/// Phase 5a negative path: the auth-enabled broker rejects node
/// CONNECTs without a credential, with a wrong token, and for
/// unclassified client_ids — and accepts a valid enrollment token
/// (positive control).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_broker_rejects_uncredentialed_node_connects() {
    let gw_home = tempfile::tempdir().unwrap();
    let mut enrollment_store = EnrollmentTokenStore::load(gw_home.path());
    let enrollment_token = enrollment_store.create_token(Duration::from_secs(3600), None);
    let enrollment_tokens: SharedEnrollmentTokenStore = Arc::new(Mutex::new(enrollment_store));
    let node_tokens: SharedNodeTokenStore = Arc::new(Mutex::new(NodeTokenStore::load(gw_home.path())));

    let auth = BrokerAuth {
        auth_enabled: true,
        enrollment_tokens: enrollment_tokens.clone(),
        node_tokens: node_tokens.clone(),
        publisher_token: PUBLISHER_TOKEN.to_string(),
        http_token: Some("e2e-http-token".to_string()),
    };
    let broker_handle = start_broker_with_auth("127.0.0.1", AUTH_REJECT_PORT, Some(auth), None)
        .expect("auth broker starts");

    // Missing credential.
    assert!(
        expect_connect_rejected(AUTH_REJECT_PORT, "node:evil-node", None).await,
        "anonymous node CONNECT must be rejected"
    );
    // Wrong token.
    assert!(
        expect_connect_rejected(AUTH_REJECT_PORT, "node:evil-node", Some("wrong-token")).await,
        "wrong-token node CONNECT must be rejected"
    );
    // Unclassified client_id (no matching CONNECT rule).
    assert!(
        expect_connect_rejected(AUTH_REJECT_PORT, "random-client", None).await,
        "unclassified client_id must be rejected"
    );
    // Positive control: a valid, unconsumed enrollment token passes.
    assert!(
        !expect_connect_rejected(
            AUTH_REJECT_PORT,
            "node:good-node",
            Some(&enrollment_token),
        )
        .await,
        "valid enrollment token must be accepted"
    );

    drop(broker_handle);
}

// ═══════════════════════════════════════════════════════════════════════
// ADR-075 D4: rename updates ONLY the display name — the UUID routing
// key never changes, so there is no retained migration, no installed
// inventory rebuild, and no daemon-stop precondition. The daemon keeps
// running under `node:{uuid}` while a temporary `node:{uuid}:rename`
// connection performs the rename.
//
// Scenario:
// 1. Real node binary boots against an isolated broker.
// 2. Assert identity.json node_id is a UUID and the info topic carries
//    the boot-time node_name.
// 3. Run `acowork-node rename <new>` against the live broker.
// 4. Assert: exit 0; identity.json node_id UNCHANGED, node_name updated;
//    a fresh info retained snapshot with the new node_name arrives;
//    status stays "online" (the rename's temp connection has no LWT, so
//    it never flips the daemon's status).
// ═══════════════════════════════════════════════════════════════════════
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_rename_updates_display_name_without_touching_uuid() {
    let Some(bin) = node_binary() else {
        eprintln!("SKIP: acowork-node binary not found — run `cargo build --workspace` first");
        return;
    };

    let broker_handle =
        acowork_gateway::mqtt::start_broker("127.0.0.1", RENAME_TEST_PORT).expect("broker starts");

    let home = tempfile::tempdir().unwrap();
    let node_home = home.path().join("node-home");

    let mut child = std::process::Command::new(&bin)
        .args([
            "start",
            "--gateway",
            &format!("127.0.0.1:{RENAME_TEST_PORT}"),
            "--name",
            RENAME_NODE_ID,
            "--home",
        ])
        .arg(&node_home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn node");

    let mut collector = Collector::connect(RENAME_TEST_PORT, "test:rename-e2e", None);

    // ADR-075 D1: node_id is a UUID, read back from identity.json.
    let identity_path = node_home.join("identity.json");
    let node_id = wait_for_identity_node_id(&identity_path, Duration::from_secs(10))
        .await
        .expect("node_id persisted to identity.json");
    assert_ne!(node_id, RENAME_NODE_ID, "node_id is a UUID, not the --name slug");

    // Boot state: online + info with the original node_name.
    let status = collector
        .wait_for_topic(&format!("/nodes/{node_id}/status"), Duration::from_secs(10))
        .await
        .expect("status topic");
    assert_eq!(std::str::from_utf8(&status).unwrap().trim(), "online");

    let boot_info = collector
        .wait_for_topic(&format!("/nodes/{node_id}/info"), Duration::from_secs(10))
        .await
        .expect("boot info topic");
    let boot_info = decode_info(&boot_info);
    assert_eq!(boot_info.node_id, node_id);
    assert_eq!(boot_info.node_name, RENAME_NODE_ID, "boot name is the --name slug");

    // Run the rename against the live broker (separate process, like the
    // real CLI). auth_enabled=false → no credential needed.
    let new_name = "renamed-box";
    let out = std::process::Command::new(&bin)
        .args([
            "rename",
            new_name,
            "--gateway",
            &format!("127.0.0.1:{RENAME_TEST_PORT}"),
            "--home",
        ])
        .arg(&node_home)
        .output()
        .expect("rename runs");
    assert!(
        out.status.success(),
        "rename must exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // identity.json: node_id UNCHANGED, node_name updated (ADR-075 D4).
    let identity = std::fs::read_to_string(&identity_path).expect("identity re-read");
    let identity: serde_json::Value =
        serde_json::from_str(&identity).expect("identity parses");
    assert_eq!(
        identity["node_id"].as_str().unwrap(),
        node_id,
        "rename must NOT change node_id (the UUID routing key)"
    );
    assert_eq!(
        identity["node_name"].as_str().unwrap(),
        new_name,
        "rename must update node_name"
    );

    // The rename republishes the info retained snapshot with the new
    // node_name; read until it shows up (the collector may still have the
    // boot snapshot in flight).
    let renamed_info = loop {
        let info = collector
            .wait_for_topic(&format!("/nodes/{node_id}/info"), Duration::from_secs(10))
            .await
            .expect("info topic after rename");
        let info = decode_info(&info);
        if info.node_name == new_name {
            break info;
        }
    };
    assert_eq!(renamed_info.node_id, node_id, "info node_id unchanged after rename");

    // Status must still be online — the rename used a temp connection with
    // no LWT, so the daemon's retained status is untouched. The rename does
    // NOT republish status (only info), so open a fresh subscription: MQTT
    // replays the retained snapshot on subscribe, which is exactly what a
    // Gateway would observe after the rename.
    let retained = expect_retained_status(RENAME_TEST_PORT, &node_id, Duration::from_secs(10)).await;
    assert_eq!(
        retained.as_deref(),
        Some("online"),
        "daemon's retained status must still be online after rename (rename has no LWT)"
    );

    // Cleanup.
    let _ = child.kill();
    let _ = child.wait();
    drop(broker_handle);
}

// ═══════════════════════════════════════════════════════════════════════
// ADR-075 D5: the Gateway recognizes its own-machine node by the
// `gateway_managed` marker — persisted into identity.json at spawn
// (`--gateway-managed`), carried on every NodeInfo, and queried via the
// NodeRegistry `local_node_id()` (gateway_managed == true && online).
//
// Scenario:
// 1. Real node binary boots with `--gateway-managed`.
// 2. Assert the marker is persisted in identity.json AND the info topic
//    carries gateway_managed = true.
// 3. Feed a real NodeRegistry the same way MQTT dispatch does (status +
//    info retained topics) and assert `local_node_id()` resolves to this
//    node's UUID.
// ═══════════════════════════════════════════════════════════════════════
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_booted_with_gateway_managed_marker_is_visible_to_registry() {
    let Some(bin) = node_binary() else {
        eprintln!("SKIP: acowork-node binary not found — run `cargo build --workspace` first");
        return;
    };

    let broker_handle = acowork_gateway::mqtt::start_broker("127.0.0.1", GW_MANAGED_TEST_PORT)
        .expect("broker starts");

    let home = tempfile::tempdir().unwrap();
    let node_home = home.path().join("node-home");

    let mut child = std::process::Command::new(&bin)
        .args([
            "start",
            "--gateway",
            &format!("127.0.0.1:{GW_MANAGED_TEST_PORT}"),
            "--name",
            GW_MANAGED_NODE_ID,
            "--gateway-managed",
            "--home",
        ])
        .arg(&node_home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn node");

    let mut collector = Collector::connect(GW_MANAGED_TEST_PORT, "test:gw-managed-e2e", None);

    let identity_path = node_home.join("identity.json");
    let node_id = wait_for_identity_node_id(&identity_path, Duration::from_secs(10))
        .await
        .expect("node_id persisted to identity.json");

    // The spawn marker is persisted (ADR-075 D5) — a service/container
    // restart that drops the flag still leaves a recognized node.
    let identity = std::fs::read_to_string(&identity_path).expect("identity read");
    let identity: serde_json::Value = serde_json::from_str(&identity).expect("identity parses");
    assert_eq!(
        identity["gateway_managed"].as_bool(),
        Some(true),
        "gateway_managed must be persisted into identity.json at spawn"
    );

    // NodeInfo carries the marker on the wire.
    let status = collector
        .wait_for_topic(&format!("/nodes/{node_id}/status"), Duration::from_secs(10))
        .await
        .expect("status topic");
    assert_eq!(std::str::from_utf8(&status).unwrap().trim(), "online");

    let info_bytes = collector
        .wait_for_topic(&format!("/nodes/{node_id}/info"), Duration::from_secs(10))
        .await
        .expect("info topic");
    let info = decode_info(&info_bytes);
    assert_eq!(info.node_id, node_id);
    assert_eq!(info.node_name, GW_MANAGED_NODE_ID);
    assert!(
        info.gateway_managed,
        "NodeInfo must carry gateway_managed=true for a Gateway-spawned node"
    );

    // Feed the registry exactly like MQTT dispatch does (status + info
    // retained), then ask local_node_id() — ADR-075 D5 query semantics.
    let registry = new_shared_registry();
    registry
        .write()
        .await
        .update_status_from_mqtt(&node_status_topic(&node_id), b"online");
    registry
        .write()
        .await
        .update_info_from_mqtt(&node_info_topic(&node_id), &info_bytes);

    let resolved = local_node_id(&registry).await;
    assert_eq!(
        resolved.as_deref(),
        Some(node_id.as_str()),
        "local_node_id() must resolve the gateway-managed online node to its UUID"
    );

    // Cleanup.
    let _ = child.kill();
    let _ = child.wait();
    drop(broker_handle);
}

/// Decode a `DataEnvelope<NodeInfo>` payload.
fn decode_info(bytes: &[u8]) -> NodeInfo {
    let envelope = DataEnvelope::decode(bytes).expect("info decodes");
    match envelope.payload.expect("info payload") {
        data_envelope::Payload::NodeInfo(info) => info,
        _ => panic!("expected NodeInfo"),
    }
}

/// Subscribe to a node's status topic and return the retained snapshot
/// (MQTT replays retained messages on subscribe). `None` on timeout.
async fn expect_retained_status(port: u16, node_id: &str, timeout: Duration) -> Option<String> {
    let mut opts = MqttOptions::new("test:retained-probe", "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = AsyncClient::new(opts, 16);
    let topic = node_status_topic(node_id);
    client
        .subscribe(&topic, QoS::AtLeastOnce)
        .await
        .ok()?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, eventloop.poll()).await {
            Ok(Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p))))
                if p.topic == topic =>
            {
                return Some(String::from_utf8_lossy(&p.payload).trim().to_string());
            }
            Ok(Ok(_)) | Ok(Err(_)) => {}
            Err(_) => return None,
        }
    }
    None
}
