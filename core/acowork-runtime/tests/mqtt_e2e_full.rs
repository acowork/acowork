//! Full E2E integration test for ADR-033 MQTT migration.
//!
//! Tests the complete MQTT flow with real broker + real clients:
//!   Gateway MQTT client → Broker → Runtime MQTT client (control_rx)
//!
//! No real Gateway/Runtime binaries needed — uses MQTT module-level clients.
//! No real LLM either — we only verify the MQTT control channel.

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use acowork_core::mqtt_proto::{
    AvailableMcps, AvailableProviders, ControlCommand, DataEnvelope, Intent, LlmProtocol, McpRef,
    McpTransport as ProtoMcpTransport, ProviderRef, control_command::Command, data_envelope::Payload,
};
use acowork_gateway::mqtt::{GatewayMqttClient, start_broker};
use acowork_runtime::mqtt::{MqttConnectConfig, RuntimeMqttClient, new_shared_cache};
use prost::Message as _;

/// Reserve a unique broker port for the current test.
///
/// **Why per-test ports?** Each test below starts its own embedded
/// `rumqttd` broker via [`start_broker`]. `cargo test` runs
/// `#[test]` functions in parallel by default — if every test bound the
/// same hard-coded port (the previous `BROKER_PORT = 19975`), only the
/// first broker to acquire the port would actually be listening while the
/// rest reported as "started" (rumqttd's `Broker::start` defers bind to a
/// background task and does not synchronously surface bind errors). The
/// surviving broker then received **all** clients' traffic, leading to
/// cross-test cross-talk: a Runtime's `control_rx` could be closed by a
/// sibling test's MQTT error, and the panic message was the misleading
/// `control_rx closed` / `Option::unwrap() on None`. Using a monotonically
/// increasing atomic counter guarantees each test gets its own port
/// (6 currently; the loop is `19975..=u16::MAX`) without serializing the
/// test suite.
fn fresh_broker_port() -> u16 {
    // Start at 19975 (matches the old hard-coded value so test logs stay
    // readable across the migration) and hand out one port per call.
    // `Relaxed` is sufficient — we only need uniqueness, no synchronisation.
    static NEXT: AtomicU16 = AtomicU16::new(19975);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ADR-073: every test runs exactly one Runtime per broker, so one shared
// instance identity is sufficient here. The Runtime MQTT client id and
// all `acowork/agents/{id}/...` topics are keyed on this instance id;
// the package `agent_id` fields above are kept only as display payload.
const TEST_INSTANCE_ID: &str = "6d3f0c2a-9b1e-4c7a-a5d8-2e4f6b8a0c1d";

// ═══════════════════════════════════════════════════════════════════════
// Test 1: Broker starts in separate thread
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_broker_starts() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).expect("broker should start in separate thread");
    assert_eq!(
        broker.listen_addr.to_string(),
        format!("127.0.0.1:{}", port)
    );
    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 2: Gateway + Runtime MQTT clients connect to broker
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_gateway_and_runtime_connect() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Gateway publisher connects
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("gateway connect");

        // Runtime client connects
        let cache = new_shared_cache();
        let (control_tx, mut _control_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.agent",
            instance_id: TEST_INSTANCE_ID,
            agent_name: "Test Agent",
            agent_version: "1.0.0",
            config_json: "{}",
            available_cache: cache,
            control_tx,
            identity_update_tx: None,
            mcp_notifier: None,
            provider_update_tx: None,
            embedding_update_tx: None,
            node_id: None,
            lsps_update_tx: None,
            node_proxy_update_tx: None,
            http_advertise_endpoint: None,
            http_port: None,
            work_dir: std::env::temp_dir().join(format!("acowork-test-{}", uuid::Uuid::new_v4())),
            username: None,
            password: None,
        })
        .await
        .expect("runtime connect");

        // Both connected — verify they're alive (can be dropped)
        drop(gw);
        drop(runtime);

        // Give broker time to process disconnect
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 3: Gateway publishes control message → Runtime receives via control_rx
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_control_message_flow() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Gateway publisher
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .unwrap();

        // Runtime with control_rx
        let cache = new_shared_cache();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let _runtime = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.agent",
            instance_id: TEST_INSTANCE_ID,
            agent_name: "Test",
            agent_version: "1.0.0",
            config_json: "{}",
            available_cache: cache,
            control_tx,
            identity_update_tx: None,
            mcp_notifier: None,
            provider_update_tx: None,
            embedding_update_tx: None,
            node_id: None,
            lsps_update_tx: None,
            node_proxy_update_tx: None,
            http_advertise_endpoint: None,
            http_port: None,
            work_dir: std::env::temp_dir().join(format!("acowork-test-{}", uuid::Uuid::new_v4())),
            username: None,
            password: None,
        })
        .await
        .unwrap();

        // Publish control message from Gateway
        let cmd = ControlCommand {
            instance_id: TEST_INSTANCE_ID.into(),
            command: Some(Command::Intent(Intent {
                from: "cron:cron-agent".into(),
                action: "e2e_trigger".into(),
                message_id: String::new(),
                conversation_id: String::new(),
                source_agent_session_id: String::new(),
                source_chat_session_id: String::new(),
                repo: String::new(),
                kind: String::new(),
                in_reply_to: String::new(),
                status: String::new(),
params_json: r#"{"hello":"e2e"}"#.into(),
            })),
        };
        gw.publish_control_command(TEST_INSTANCE_ID, cmd)
            .await
            .expect("publish");

        // Runtime receives via control_rx (with timeout)
        let received = tokio::time::timeout(Duration::from_secs(3), control_rx.recv())
            .await
            .expect("timeout")
            .expect("control_rx closed");

        let (_topic, payload_bytes) = received;

        // Verify it's valid ControlCommand protobuf
        let env = DataEnvelope::decode(payload_bytes.as_slice()).expect("decode DataEnvelope");

        match env.payload {
            Some(Payload::ControlCommand(ctrl)) => {
                assert_eq!(ctrl.instance_id, TEST_INSTANCE_ID);
                match ctrl.command {
                    Some(Command::Intent(msg)) => {
                        assert_eq!(msg.from, "cron:cron-agent");
                        assert_eq!(msg.action, "e2e_trigger");
                        assert_eq!(msg.params_json, r#"{"hello":"e2e"}"#);
                    }
                    other => panic!("Expected Intent, got {:?}", other),
                }
            }
            other => panic!("Expected ControlCommand, got {:?}", other),
        }

        drop(gw);
        drop(_runtime);
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 4: Gateway publishes a second intent → Runtime receives
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_control_intent_flow() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .unwrap();

        let cache = new_shared_cache();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let _rt = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.agent",
            instance_id: TEST_INSTANCE_ID,
            agent_name: "Test",
            agent_version: "1.0.0",
            config_json: "{}",
            available_cache: cache,
            control_tx,
            identity_update_tx: None,
            mcp_notifier: None,
            provider_update_tx: None,
            embedding_update_tx: None,
            node_id: None,
            lsps_update_tx: None,
            node_proxy_update_tx: None,
            http_advertise_endpoint: None,
            http_port: None,
            work_dir: std::env::temp_dir().join(format!("acowork-test-{}", uuid::Uuid::new_v4())),
            username: None,
            password: None,
        })
        .await
        .unwrap();

        let cmd = ControlCommand {
            instance_id: TEST_INSTANCE_ID.into(),
            command: Some(Command::Intent(Intent {
                from: "cron:cron-agent".into(),
                action: "retry_probe".into(),
                message_id: String::new(),
                conversation_id: String::new(),
                source_agent_session_id: String::new(),
                source_chat_session_id: String::new(),
                repo: String::new(),
                kind: String::new(),
                in_reply_to: String::new(),
                status: String::new(),
params_json: String::new(),
            })),
        };
        gw.publish_control_command(TEST_INSTANCE_ID, cmd)
            .await
            .unwrap();

        // 10s timeout (was 2s, raised after ADR-044 Phase 2 fixed the
        // cross-test port bind). Parallel `cargo test` runs 5 brokers
        // + 5 Runtimes concurrently; broker poll forwarding latency grows
        // nonlinearly under load — observed worst-case end-to-end delivery
        // (publish → runtime event loop → control_tx) hits ~5s on a busy
        // macOS dev box. Single retry below guards the long tail.
        let mut payload_opt = None;
        for attempt in 0..2 {
            match tokio::time::timeout(Duration::from_secs(10), control_rx.recv()).await {
                Ok(Some(p)) => {
                    payload_opt = Some(p);
                    break;
                }
                Ok(None) => panic!("control_rx closed unexpectedly on attempt {}", attempt),
                Err(_) if attempt == 0 => {
                    eprintln!("control_stop_flow: 10s timeout on attempt 0, retrying once");
                    // Re-publish in case the original message was lost in
                    // a parallel broker poll forward race.
                    let _ = gw
                        .publish_control_command(
                            TEST_INSTANCE_ID,
                            ControlCommand {
                                instance_id: TEST_INSTANCE_ID.into(),
                                command: Some(Command::Intent(Intent {
                                    from: "cron:cron-agent".into(),
                                    action: "retry_probe".into(),
                                    message_id: String::new(),
                                    conversation_id: String::new(),
                                    source_agent_session_id: String::new(),
                                    source_chat_session_id: String::new(),
                                    repo: String::new(),
                                    kind: String::new(),
                                    in_reply_to: String::new(),
                                    status: String::new(),
params_json: String::new(),
                                })),
                            },
                        )
                        .await;
                    continue;
                }
                Err(_) => panic!("control_rx recv timed out on retry"),
            }
        }
        let (_topic, payload) = payload_opt.expect("retry must populate payload_opt");

        let env = DataEnvelope::decode(payload.as_slice()).unwrap();
        assert!(matches!(
            env.payload,
            Some(Payload::ControlCommand(ControlCommand {
                command: Some(Command::Intent(_)),
                ..
            }))
        ));

        drop(gw);
        drop(_rt);
    });
    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 5: Multiple messages in sequence
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_multiple_messages() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .unwrap();
        let cache = new_shared_cache();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let _rt = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.agent",
            instance_id: TEST_INSTANCE_ID,
            agent_name: "Test",
            agent_version: "1.0.0",
            config_json: "{}",
            available_cache: cache,
            control_tx,
            identity_update_tx: None,
            mcp_notifier: None,
            provider_update_tx: None,
            embedding_update_tx: None,
            node_id: None,
            lsps_update_tx: None,
            node_proxy_update_tx: None,
            http_advertise_endpoint: None,
            http_port: None,
            work_dir: std::env::temp_dir().join(format!("acowork-test-{}", uuid::Uuid::new_v4())),
            username: None,
            password: None,
        })
        .await
        .unwrap();
        let messages = ["msg-1", "msg-2", "msg-3"];
        for (i, content) in messages.iter().enumerate() {
            let cmd = ControlCommand {
                instance_id: TEST_INSTANCE_ID.into(),
                command: Some(Command::Intent(Intent {
                    from: "cron:cron-agent".into(),
                    action: content.to_string(),
                    message_id: String::new(),
                    conversation_id: String::new(),
                    source_agent_session_id: String::new(),
                    source_chat_session_id: String::new(),
                    repo: String::new(),
                    kind: String::new(),
                    in_reply_to: String::new(),
                    status: String::new(),
params_json: format!(r#"{{"seq":{i}}}"#),
                })),
            };
            gw.publish_control_command(TEST_INSTANCE_ID, cmd)
                .await
                .unwrap();
        }

        let mut received = Vec::new();
        for _ in 0..3 {
            // 10s timeout per-receive: see comment on the Stop test for
            // the parallel-load rationale. The previous 2s window was
            // fine for serial runs but unstable under `cargo test`'s
            // default parallel scheduling once 5 brokers compete for the
            // same Tokio reactor.
            let (_, payload) = tokio::time::timeout(Duration::from_secs(10), control_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let env = DataEnvelope::decode(payload.as_slice()).unwrap();
            if let Some(Payload::ControlCommand(ctrl)) = env.payload
                && let Some(Command::Intent(msg)) = ctrl.command
            {
                received.push(msg.action);
            }
        }

        assert_eq!(received, ["msg-1", "msg-2", "msg-3"]);

        drop(gw);
        drop(_rt);
    });
    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 6: LWT — Runtime disconnect triggers offline status
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_lwt_offline_on_disconnect() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).unwrap();

    // Phase 1: connect inside a throwaway runtime, then drop the WHOLE
    // runtime. Dropping just `RuntimeMqttClient` does NOT tear down the
    // connection: the event-loop poll task owns the socket independently
    // (EventLoopGuard's JoinHandle is detached, not aborted), so the TCP
    // link stays alive and the broker never publishes the Will. Dropping
    // the runtime aborts the poll task and the OS closes the socket — the
    // exact equivalent of a Runtime process crash, which is the real LWT
    // trigger.
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cache = new_shared_cache();
            let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
            let runtime = RuntimeMqttClient::connect(MqttConnectConfig {
                host: "127.0.0.1",
                port,
                agent_id: "com.test.lwt",
                instance_id: TEST_INSTANCE_ID,
                agent_name: "LWT Agent",
                agent_version: "1.0.0",
                config_json: "{}",
                available_cache: cache,
                control_tx,
                identity_update_tx: None,
                mcp_notifier: None,
                provider_update_tx: None,
                embedding_update_tx: None,
                node_id: None,
                lsps_update_tx: None,
                node_proxy_update_tx: None,
                http_advertise_endpoint: None,
                http_port: None,
                work_dir: std::env::temp_dir()
                    .join(format!("acowork-test-{}", uuid::Uuid::new_v4())),
                username: None,
                password: None,
            })
            .await
            .unwrap();

            // connect() returns only after bootstrap published the retained
            // "online" status — the checker in Phase 2 sees a pre-existing
            // "online" unless the Will lands on top of it.
            drop(control_rx);
            drop(runtime);

            // Let the retained "online" fully propagate before teardown.
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
    } // rt dropped → poll task aborted → TCP FIN → broker publishes the Will

    // Phase 2: fresh runtime to observe the retained status.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Give the broker a moment to detect the closed socket and publish
        // the Will (retained "offline").
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Connect a fresh client to check the retained message
        let mut opts = rumqttc::MqttOptions::new("e2e:lwt:checker", "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut events) = rumqttc::AsyncClient::new(opts, 10);

        client
            .subscribe(
                &format!("acowork/agents/{}/status", TEST_INSTANCE_ID),
                rumqttc::QoS::AtLeastOnce,
            )
            .await
            .unwrap();

        let lwt_payload = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match events.poll().await {
                    Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                        if p.topic.contains(&format!("/{}/status", TEST_INSTANCE_ID)) {
                            return p.payload.to_vec();
                        }
                    }
                    _ => continue,
                }
            }
        })
        .await
        .expect("should receive retained LWT message");

        // The wire is uniformly `DataEnvelope<AgentStatus>` since the
        // auto-sleep subsystem was retired (Sept 2026) — decode and
        // assert `online=false`.
        let env: acowork_core::mqtt_proto::DataEnvelope =
            prost::Message::decode(lwt_payload.as_slice())
                .expect("LWT payload must be a DataEnvelope");
        let status = match env.payload {
            Some(acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(s)) => s,
            _ => panic!("LWT envelope did not carry an AgentStatus payload"),
        };
        assert!(!status.online, "LWT must publish AgentStatus with online=false");

        drop(client);
    });
    drop(broker);
}

// ═══════════════════════════════════════════════════════════════════════
// Test 6: `acowork/global/mcps` retained → Runtime persists catalog to
// `agent_mcp.json::catalog` so PUT /mcp-servers can resolve names
// (regression: ADR-040 follow-up — see commit message).
//
// Repro: before this fix, Gateway published the MCP catalog on
// `acowork/global/mcps` (retained), Runtime subscribed and updated the
// in-memory `available_cache.mcps`, but the on-disk
// `agent_mcp.json::catalog` stayed empty. The PUT /mcp-servers handler
// validated names against `merged()` (catalog + local), which was empty,
// so every "set active = ['context7']" call returned HTTP 400 with
// `UnknownServers(["context7"])`. The frontend then rolled back the
// optimistic toggle — user saw "checkbox won't tick".
//
// This test wires the full path: Gateway publish → broker retain →
// Runtime poll → save_agent_mcp_config_catalog → agent_mcp.json on disk
// contains catalog. After the publish, a direct `load_agent_mcp_config`
// from the same work_dir sees the catalog.
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_catalog_retained_persists_to_agent_mcp_json() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // ── 1. Spin up Runtime with a real work_dir on disk ─────────
        // The work_dir must exist so `save_agent_mcp_config_catalog`
        // can mkdir config/ and atomic-rename the file. Tests that
        // used `tempdir()` elsewhere went through `agent_config`
        // directly; this one needs the *Runtime MQTT poll loop* to
        // perform the write, which only happens after a retained
        // message arrives on `acowork/global/mcps`.
        let work_dir =
            std::env::temp_dir().join(format!("acowork-catalog-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&work_dir).expect("work_dir should be creatable");

        let cache = new_shared_cache();
        let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel();
        let _rt = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.catalog",
            instance_id: TEST_INSTANCE_ID,
            agent_name: "Catalog Test",
            agent_version: "1.0.0",
            config_json: "{}",
            available_cache: cache,
            control_tx,
            identity_update_tx: None,
            mcp_notifier: None,
            provider_update_tx: None,
            embedding_update_tx: None,
            node_id: None,
            lsps_update_tx: None,
            node_proxy_update_tx: None,
            http_advertise_endpoint: None,
            http_port: None,
            work_dir: work_dir.clone(),
            username: None,
            password: None,
        })
        .await
        .unwrap();

        // ── 2. Gateway publishes `acowork/global/mcps` (retained) ───
        // Mimics build_available_mcps in
        // acowork-gateway/src/mqtt/global_resources_publisher.rs.
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .unwrap();

        let payload = AvailableMcps {
            version: 1,
            servers: vec![
                McpRef {
                    id: "context7".into(),
                    name: "context7".into(),
                    transport: ProtoMcpTransport::Stdio.into(),
                    url: String::new(),
                    command: "npx".into(),
                    args: vec!["-y".into(), "@upstash/context7-mcp".into()],
                    env: Default::default(),
                    headers: Default::default(),
                    tool_timeout_secs: 0,
                    auth_token: "redacted-secret".into(), // wire-only; never persisted
                    install_spec_json: String::new(),
                },
                McpRef {
                    id: "openapi".into(),
                    name: "openapi".into(),
                    transport: ProtoMcpTransport::Http.into(),
                    url: "https://api.example.com/mcp".into(),
                    command: String::new(),
                    args: vec![],
                    env: Default::default(),
                    headers: Default::default(),
                    tool_timeout_secs: 30,
                    auth_token: String::new(),
                    install_spec_json: String::new(),
                },
            ],
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(Payload::AvailableMcps(payload)),
        };
        gw.publish_envelope(
            "acowork/global/mcps",
            &envelope,
            acowork_gateway::mqtt::MqttQoS::AtLeastOnce,
            true,
        )
        .await
        .expect("gateway publish should succeed");
        eprintln!("[test] gateway published available_mcps (retained)");

        // ── 3. Wait for Runtime poll loop to receive and persist ────
        // Polling cadence is driven by `MqttOptions.set_keep_alive` +
        // the embedded broker's forwarding latency. 1s is generous on a
        // busy dev box; observed worst-case end-to-end under parallel
        // `cargo test` is ~500ms.
        let agent_mcp_path = work_dir.join("config").join("agent_mcp.json");
        let mut written = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if agent_mcp_path.exists() {
                let raw = std::fs::read_to_string(&agent_mcp_path).unwrap();
                if raw.contains("context7") && raw.contains("openapi") {
                    written = true;
                    break;
                }
            }
        }
        assert!(
            written,
            "Runtime should have written agent_mcp.json containing catalog \
             entries (context7 + openapi) after receiving retained \
             acowork/global/mcps. file={} exists={}",
            agent_mcp_path.display(),
            agent_mcp_path.exists(),
        );

        // ── 4. Verify catalog landed on disk (no auth_token leak) ───
        let cfg = acowork_runtime::agent_config::load_agent_mcp_config(&work_dir)
            .expect("load should succeed")
            .expect("file should now exist");
        assert_eq!(cfg.catalog.len(), 2, "catalog should have both entries");
        assert_eq!(cfg.catalog[0].name, "context7");
        assert_eq!(cfg.catalog[1].name, "openapi");

        // auth_token MUST NOT be persisted (it's wire-only for the
        // Runtime to authenticate live MCP requests, but disk storage
        // would leak it via world-readable config files).
        let raw_json = std::fs::read_to_string(&agent_mcp_path).unwrap();
        assert!(
            !raw_json.contains("redacted-secret"),
            "auth_token leaked into agent_mcp.json (must be stripped before \
             write); raw={}",
            raw_json,
        );

        // ── 5. After sync, the merged catalog ∪ local resolves the
        // catalog names. This is what `put_mcp_servers` consults.
        // Before the fix, this returned `vec![]` and PUT /mcp-servers
        // 400'd with UnknownServers. After: the names resolve and the
        // user's active_names write succeeds.
        //
        // We use `active_merged()` (not the deprecated
        // `load_merged_mcp_configs`) but for this assertion that doesn't
        // matter — the persisted file has no active_names yet so both
        // return the same set.
        #[allow(deprecated)]
        let merged = acowork_runtime::agent_config::load_merged_mcp_configs(&work_dir);
        let merged_names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
        assert!(
            merged_names.contains(&"context7"),
            "merged() must include context7 — otherwise PUT /mcp-servers \
             would still 400 with UnknownServers. got={:?}",
            merged_names,
        );

        // ── 6. Cleanup ──────────────────────────────────────────────
        drop(_rt);
        drop(gw);
        std::fs::remove_dir_all(&work_dir).ok();
    });
    drop(broker);
}

// ════════════════════════════════════════════════════════════════════════════
// Provider list retained -> agent_provider.json persistence (C1/I3 regression)
// ════════════════════════════════════════════════════════════════════════════
//
// Mirrors `integration_catalog_retained_persists_to_agent_mcp_json` but for
// `acowork/global/providers`. Verifies:
//   1. Provider list is persisted to `agent_provider.json`
//   2. `protocol_type` is correctly mapped from protobuf `LlmProtocol`
//      (regression test for C1: was hardcoded to `ProtocolType::OpenAI`)
//   3. API keys are NOT persisted (wire-only, in-memory)

#[test]
fn integration_providers_retained_persists_to_agent_provider_json() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // ── 1. Spin up Runtime with a real work_dir ───────────────────
        let work_dir =
            std::env::temp_dir().join(format!("acowork-provider-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&work_dir).expect("work_dir should be creatable");

        let cache = new_shared_cache();
        let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel();
        let _rt = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.providers",
            instance_id: TEST_INSTANCE_ID,
            agent_name: "Provider Test",
            agent_version: "1.0.0",
            config_json: "{}",
            available_cache: cache,
            control_tx,
            identity_update_tx: None,
            mcp_notifier: None,
            provider_update_tx: None,
            embedding_update_tx: None,
            node_id: None,
            lsps_update_tx: None,
            node_proxy_update_tx: None,
            http_advertise_endpoint: None,
            http_port: None,
            work_dir: work_dir.clone(),
            username: None,
            password: None,
        })
        .await
        .unwrap();

        // ── 2. Gateway publishes `acowork/global/providers` (retained) ─
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .unwrap();

        let payload = AvailableProviders {
            version: 42,
            // ADR-056: no global default compact model in this fixture.
            #[allow(deprecated)]
            default_compact_model: None,
            default_compact_models: vec![],
            providers: vec![
                ProviderRef {
                    id: "anthropic".into(),
                    base_url: "https://api.anthropic.com".into(),
                    protocol_type: LlmProtocol::Anthropic as i32,
                    models: vec![],
                    compact_model: String::new(),
                    custom: false,
                    api_key: "sk-ant-secret-key".into(), // wire-only; must NOT persist
                    account_id: String::new(),
                },
                ProviderRef {
                    id: "openai".into(),
                    base_url: "https://api.openai.com/v1".into(),
                    protocol_type: LlmProtocol::Openai as i32,
                    models: vec![],
                    compact_model: String::new(),
                    custom: false,
                    api_key: String::new(),
                    account_id: String::new(),
                },
            ],
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(Payload::AvailableProviders(payload)),
        };
        gw.publish_envelope(
            "acowork/global/providers",
            &envelope,
            acowork_gateway::mqtt::MqttQoS::AtLeastOnce,
            true,
        )
        .await
        .expect("gateway publish should succeed");
        eprintln!("[test] gateway published available_providers (retained)");

        // ── 3. Wait for Runtime poll loop to persist ──────────────────
        let provider_path = work_dir.join("config").join("agent_provider.json");
        let mut written = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if provider_path.exists() {
                let raw = std::fs::read_to_string(&provider_path).unwrap();
                if raw.contains("anthropic") && raw.contains("openai") {
                    written = true;
                    break;
                }
            }
        }
        assert!(
            written,
            "Runtime should have written agent_provider.json containing both providers. \
             file={} exists={}",
            provider_path.display(),
            provider_path.exists(),
        );

        // ── 4. Verify provider list and protocol_type (C1 regression) ─
        let cfg = acowork_runtime::agent_config::load_agent_provider_config(&work_dir)
            .expect("load should succeed")
            .expect("file should now exist");
        assert_eq!(cfg.providers.len(), 2, "should have both providers");
        assert_eq!(cfg.version, 42, "version should match MQTT payload");

        // C1 regression: protocol_type must be mapped from LlmProtocol,
        // not hardcoded to OpenAI.
        let anthropic = cfg
            .providers
            .iter()
            .find(|p| p.id == "anthropic")
            .expect("anthropic provider should exist");
        assert_eq!(
            anthropic.protocol_type,
            acowork_core::protocol::ProtocolType::Anthropic,
            "C1 regression: anthropic provider protocol_type must be Anthropic, not OpenAI"
        );

        let openai = cfg
            .providers
            .iter()
            .find(|p| p.id == "openai")
            .expect("openai provider should exist");
        assert_eq!(
            openai.protocol_type,
            acowork_core::protocol::ProtocolType::OpenAI,
        );

        // ── 5. API key must NOT be persisted ──────────────────────────
        let raw_json = std::fs::read_to_string(&provider_path).unwrap();
        assert!(
            !raw_json.contains("sk-ant-secret-key"),
            "API key leaked into agent_provider.json (must be stripped before write); raw={}",
            raw_json,
        );

        // ── 6. Cleanup ────────────────────────────────────────────────
        drop(_rt);
        drop(gw);
        std::fs::remove_dir_all(&work_dir).ok();
    });
    drop(broker);
}

// ════════════════════════════════════════════════════════════════════════════
// ADR-073 回归：Gateway 下发 `{instance_id}` 模板 → Runtime 收到
// `acowork/global/mcps` 后把 headers/env 中的占位符替换为
// `self.bootstrap_data.instance_id`，然后持久化到 `agent_mcp.json`。
//
// 验证：
//   1. 模板 `{instance_id}` 出现在 Gateway publish 的 payload 里
//   2. Runtime 落盘后 header 必须是真实 UUID（**不是**字面量 `{instance_id}`）
//   3. 落盘的 header UUID == bootstrap_data.instance_id
//   4. 包 ID (`com.test.x`) 不出现在 header 中（旧行为回归捕获）
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn integration_mcp_template_instance_id_substituted_at_runtime() {
    use std::collections::HashMap;

    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let work_dir = std::env::temp_dir().join(format!(
            "acowork-template-e2e-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&work_dir).expect("work_dir should be creatable");

        let cache = new_shared_cache();
        let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel();
        let _rt = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.template", // 包 ID（不应出现在 X-MCP-Actor 中）
            instance_id: TEST_INSTANCE_ID,
            agent_name: "Template Test",
            agent_version: "1.0.0",
            config_json: "{}",
            available_cache: cache,
            control_tx,
            identity_update_tx: None,
            mcp_notifier: None,
            provider_update_tx: None,
            embedding_update_tx: None,
            node_id: None,
            lsps_update_tx: None,
            node_proxy_update_tx: None,
            http_advertise_endpoint: None,
            http_port: None,
            work_dir: work_dir.clone(),
            username: None,
            password: None,
        })
        .await
        .unwrap();

        // ── 1. Gateway publishes MCP catalog with `{instance_id}` template ─
        // 模拟 Gateway build_available_mcps（Diff K 后用 instance_id 模板）
        let mut headers = HashMap::new();
        headers.insert("X-MCP-Actor".to_string(), "{instance_id}".to_string());

        let mut env = HashMap::new();
        env.insert("AGENT_IDENTITY".to_string(), "{instance_id}".to_string());

        let payload = AvailableMcps {
            version: 1,
            servers: vec![McpRef {
                id: "pm".into(),
                name: "pm".into(),
                transport: ProtoMcpTransport::Http.into(),
                url: "http://gateway:19876/api/pm/mcp".into(),
                command: String::new(),
                args: vec![],
                env,
                headers,
                tool_timeout_secs: 60,
                auth_token: String::new(),
                install_spec_json: String::new(),
            }],
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(Payload::AvailableMcps(payload)),
        };

        let gw = GatewayMqttClient::new_publisher("127.0.0.1", port).await.unwrap();
        gw.publish_envelope(
            "acowork/global/mcps",
            &envelope,
            acowork_gateway::mqtt::MqttQoS::AtLeastOnce,
            true, // retained
        )
        .await
        .expect("gateway publish should succeed");

        // ── 2. Wait for Runtime poll loop to persist ─────────────────
        let agent_mcp_path = work_dir.join("config").join("agent_mcp.json");
        let mut written = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if agent_mcp_path.exists() {
                let raw = std::fs::read_to_string(&agent_mcp_path).unwrap();
                if raw.contains(TEST_INSTANCE_ID) {
                    written = true;
                    break;
                }
            }
        }
        assert!(
            written,
            "Runtime must substitute {{instance_id}} and persist the actual UUID to \
             agent_mcp.json. file={} exists={}",
            agent_mcp_path.display(),
            agent_mcp_path.exists(),
        );

        // ── 3. Verify header is the actual UUID, NOT the literal template ─
        let cfg = acowork_runtime::agent_config::load_agent_mcp_config(&work_dir)
            .expect("load should succeed")
            .expect("file should now exist");
        let pm = cfg
            .catalog
            .iter()
            .find(|s| s.name == "pm")
            .expect("pm should be in catalog");

        // ADR-073 §1.3 不变量 1：header 必须是 UUID，不是包 ID，也不是字面量模板
        let actor_header = pm
            .headers
            .get("X-MCP-Actor")
            .expect("X-MCP-Actor header must be present after substitution")
            .clone();
        assert_eq!(
            actor_header, TEST_INSTANCE_ID,
            "X-MCP-Actor must be the substituted instance_id UUID; got: {actor_header}"
        );
        assert!(
            !actor_header.contains("com.test.template"),
            "X-MCP-Actor must NOT contain package id (regression: old code substituted agent_id); got: {actor_header}"
        );
        assert_ne!(
            actor_header, "{instance_id}",
            "X-MCP-Actor must NOT be the literal template; got: {actor_header}"
        );

        // env 也应被替换
        let identity_env = pm
            .env
            .get("AGENT_IDENTITY")
            .expect("AGENT_IDENTITY env must be present")
            .clone();
        assert_eq!(
            identity_env, TEST_INSTANCE_ID,
            "env AGENT_IDENTITY must be the substituted instance_id"
        );

        // ── 4. Cleanup ───────────────────────────────────────────────
        drop(_rt);
        drop(gw);
        std::fs::remove_dir_all(&work_dir).ok();
    });
    drop(broker);
}
