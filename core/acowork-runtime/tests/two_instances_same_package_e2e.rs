//! ADR-073 regression E2E: two Runtime instances of one package coexist
//! on the same broker.
//!
//! ## What this proves
//!
//! Before ADR-073, the broker client_id was `agent:{agent_id}` and all
//! per-agent MQTT topics used the package id as path key. Two instances
//! of one package therefore (a) shared one broker client_id and kicked
//! each other off on the second CONNECT, and (b) published status /
//! fs-changed / control to the SAME topic — there was no way to address
//! a single instance.
//!
//! After ADR-073 the client_id and every `acowork/agents/{...}` topic
//! are keyed on `instance_id`. This test asserts the post-fix contract:
//!
//! 1. Two `RuntimeMqttClient`s with the same `agent_id` but distinct
//!    `instance_id`s both succeed in connecting — neither kicks the
//!    other off the broker.
//! 2. Each publishes `acowork/agents/{instance_id}/status = "online"`
//!    to its OWN topic (not the other's). A wildcard subscriber sees
//!    two distinct retained topics, each carrying the right id.
//! 3. A control message targeted at one instance reaches ONLY that
//!    instance's `control_rx` — proving the dispatch key is per-instance.
//! 4. Both clients can be cleanly disconnected.
//!
//! ## Topology
//!
//! ```text
//! ┌──────── broker (fresh port, isolated from production :19875) ────────┐
//! │                                                                     │
//! │  RuntimeMqttClient("com.test.agent", instance_id=A)  client_id=agent:A │
//! │      ├─ acowork/agents/A/status  = "online" (Retained)              │
//! │      └─ acowork/agents/A/control_rx ← chat                         │
//! │                                                                     │
//! │  RuntimeMqttClient("com.test.agent", instance_id=B)  client_id=agent:B │
//! │      ├─ acowork/agents/B/status  = "online" (Retained)              │
//! │      └─ acowork/agents/B/control_rx ← chat                         │
//! │                                                                     │
//! │  Wildcard subscriber on acowork/agents/+/status:  sees A + B       │
//! └──────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! No real Runtime/Gateway binary needed — uses module-level clients
//! exactly like `mqtt_e2e_full.rs`.

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use prost::Message as _;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use tokio::sync::mpsc;

use acowork_core::mqtt_proto::{
    ControlCommand, DataEnvelope, Intent, control_command::Command,
};
use acowork_gateway::mqtt::{GatewayMqttClient, start_broker};
use acowork_runtime::mqtt::{MqttConnectConfig, RuntimeMqttClient, new_shared_cache};

/// Reserve a unique broker port per `cargo test` worker. See the long
/// comment in `mqtt_e2e_full.rs::fresh_broker_port` for why this is
/// required (cargo runs `#[test]`s in parallel and rumqttd's bind error
/// is asynchronous — shared hard-coded ports silently cross-talk).
fn fresh_broker_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(20375);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ADR-073: instance identity is the ONLY thing that distinguishes the
// two clients below. Same `agent_id` (package), same `agent_name`, same
// `agent_version` — only `instance_id` differs. That is exactly the
// production scenario the bug report described.
const AGENT_ID: &str = "com.acowork.shared-package";
const INSTANCE_A: &str = "a1b2c3d4-1111-4111-8111-aaaaaaaaaaaa";
const INSTANCE_B: &str = "b2c3d4e5-2222-4222-8222-bbbbbbbbbbbb";

/// Subscribe to `acowork/agents/+/status` and collect every retained
/// `(topic, payload)` snapshot the broker replays + any later updates.
/// Used to assert two distinct status topics exist after both runtimes
/// come online.
async fn spawn_status_collector(port: u16) -> mpsc::UnboundedReceiver<(String, Vec<u8>)> {
    let mut opts = MqttOptions::new("e2e:two-instances:collector", "127.0.0.1", port);
    opts.set_clean_session(true);
    let (client, mut eventloop) = AsyncClient::new(opts, 32);

    // Subscribe BEFORE the runtimes publish so the retained bit replays.
    client
        .subscribe("acowork/agents/+/status", QoS::AtLeastOnce)
        .await
        .expect("subscribe");

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // Allow up to 2s of post-publish drain so the broker's retain
        // replay + any live update both land.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, eventloop.poll()).await {
                Ok(Ok(Event::Incoming(Incoming::Publish(p)))) => {
                    let _ = tx.send((p.topic.clone(), p.payload.to_vec()));
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => break, // deadline reached
            }
        }
    });
    rx
}

fn runtime_connect_cfg<'a>(
    port: u16,
    agent_id: &'a str,
    instance_id: &'a str,
    control_tx: tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
) -> MqttConnectConfig<'a> {
    MqttConnectConfig {
        host: "127.0.0.1",
        port,
        agent_id,
        instance_id,
        agent_name: "Shared Agent",
        agent_version: "1.0.0",
        config_json: "{}",
        available_cache: new_shared_cache(),
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
            .join(format!("acowork-two-instances-{}", uuid::Uuid::new_v4())),
        username: None,
        password: None,
    }
}

#[test]
fn two_runtime_instances_same_package_coexist_on_one_broker() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).expect("broker start");

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // ── 1) Start the wildcard status collector BEFORE either
        //    runtime publishes so retained replays are not lost. ──
        let mut status_rx = spawn_status_collector(port).await;

        // ── 2) Two runtimes, same agent_id, distinct instance_ids. ──
        //    Use unique control_tx / control_rx pairs so we can also
        //    assert per-instance dispatch in step 4 below.
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let cfg_a = runtime_connect_cfg(port, AGENT_ID, INSTANCE_A, tx_a);
        let cfg_b = runtime_connect_cfg(port, AGENT_ID, INSTANCE_B, tx_b);

        let runtime_a = RuntimeMqttClient::connect(cfg_a)
            .await
            .expect("runtime A connect (must not be kicked by B)");
        let runtime_b = RuntimeMqttClient::connect(cfg_b)
            .await
            .expect("runtime B connect (must not be kicked by A)");

        // ── 3) Each publishes its retained status to its own topic. ──
        runtime_a.publish_status(true).await.expect("A status");
        runtime_b.publish_status(true).await.expect("B status");

        // Give the broker a beat to persist the retained bits.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // ── 3a) Collect every status publish the wildcard subscriber
        //    received (de-duped by topic — broker may replay retained
        //    bits more than once in this short window). ──
        let mut by_topic: std::collections::HashMap<String, Vec<u8>> =
            std::collections::HashMap::new();
        // Channel drain over a fixed window. `Err(_)` here is the
        // per-iteration timeout, not a recv error — converting it into
        // the loop's break condition is what `while let` cannot express
        // without a flag, so we suppress the lint locally.
        #[allow(clippy::while_let_loop)]
        loop {
            match tokio::time::timeout(Duration::from_millis(500), status_rx.recv()).await {
                Ok(Some((topic, payload))) => {
                    // Last write wins per topic — the retained bits are
                    // idempotent and we only care about the latest value.
                    by_topic.insert(topic, payload);
                }
                Ok(None) | Err(_) => break, // channel closed or drain window elapsed
            }
        }

        let topic_a = format!("acowork/agents/{INSTANCE_A}/status");
        let topic_b = format!("acowork/agents/{INSTANCE_B}/status");

        // Each instance's topic MUST carry an `online=true`
        // DataEnvelope<AgentStatus>. The wire is protobuf since the
        // auto-sleep subsystem was retired (Sept 2026); we decode and
        // assert on the structured field rather than raw bytes.
        let payload_a = by_topic.get(&topic_a).unwrap_or_else(|| {
            panic!(
                "missing status for instance A on topic {topic_a}; observed topics: {:?}",
                by_topic.keys().collect::<Vec<_>>()
            )
        });
        let payload_b = by_topic.get(&topic_b).unwrap_or_else(|| {
            panic!(
                "missing status for instance B on topic {topic_b}; observed topics: {:?}",
                by_topic.keys().collect::<Vec<_>>()
            )
        });
        let status_a: acowork_core::mqtt_proto::AgentStatus = {
            let env: acowork_core::mqtt_proto::DataEnvelope =
                prost::Message::decode(payload_a.as_slice()).expect("A status must be a DataEnvelope");
            match env.payload {
                Some(acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(s)) => s,
                _ => panic!("A envelope did not carry an AgentStatus payload"),
            }
        };
        let status_b: acowork_core::mqtt_proto::AgentStatus = {
            let env: acowork_core::mqtt_proto::DataEnvelope =
                prost::Message::decode(payload_b.as_slice()).expect("B status must be a DataEnvelope");
            match env.payload {
                Some(acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(s)) => s,
                _ => panic!("B envelope did not carry an AgentStatus payload"),
            }
        };
        assert!(status_a.online, "A status must report online=true");
        assert!(status_b.online, "B status must report online=true");
        assert_eq!(status_a.instance_id, INSTANCE_A);
        assert_eq!(status_b.instance_id, INSTANCE_B);

        // The two topics MUST be distinct — i.e. neither instance's
        // status was misrouted onto the other's topic (the pre-ADR-073
        // bug: both would share a single `agent:{agent_id}` topic).
        assert_ne!(
            topic_a, topic_b,
            "instance identity must produce distinct topics, not collapse them"
        );

        // ── 4) Dispatch a control command to A only and verify ONLY
        //    A's control_rx receives it. Uses GatewayMqttClient
        //    (already covered by mqtt_e2e_full.rs) — here we only
        //    care about the per-instance routing. ──
        //
        // ADR-076 §决策 4: the payload is an `Intent` because that (plus the
        // presence heartbeat) is all the MQTT control plane still carries.
        // The routing property under test is unchanged: a message addressed
        // to A's instance topic must not surface on B's subscription.
        let gw = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("gateway publisher");
        let cmd = ControlCommand {
            instance_id: INSTANCE_A.to_string(),
            command: Some(Command::Intent(Intent {
                from: "cron:cron-agent".into(),
                action: "route_probe".into(),
                message_id: String::new(),
                conversation_id: String::new(),
                source_agent_session_id: String::new(),
                source_chat_session_id: String::new(),
                repo: String::new(),
                session_config_json: String::new(),
                kind: String::new(),
                in_reply_to: String::new(),
                status: String::new(),
                has_human_participant: None,
                chat_attachment_ids: vec![],
                for_execution_session: String::new(),
                params_json: r#"{"target":"A"}"#.into(),
            })),
        };
        gw.publish_control_command(INSTANCE_A, cmd)
            .await
            .expect("publish to A");

        // A must receive; B must NOT (within a tight window).
        let got_a = tokio::time::timeout(Duration::from_secs(2), rx_a.recv())
            .await
            .expect("A control_rx timeout — dispatch failed")
            .expect("A control_rx closed");

        // Decode and sanity-check the payload landed on A.
        let env = DataEnvelope::decode(got_a.1.as_slice()).expect("A decodes");
        if let Some(acowork_core::mqtt_proto::data_envelope::Payload::ControlCommand(ctrl)) =
            env.payload
        {
            match ctrl.command {
                Some(Command::Intent(msg)) => {
                    assert_eq!(msg.action, "route_probe");
                    assert_eq!(msg.params_json, r#"{"target":"A"}"#);
                }
                other => panic!("A received unexpected command variant: {:?}", other),
            }
        } else {
            panic!("A received non-ControlCommand payload: {:?}", env.payload);
        }

        // B's rx must remain empty during the same window. A short
        // try_recv with zero-wait is enough — broker fanout is sync
        // for QoS 1 within the same poll loop.
        assert!(
            rx_b.try_recv().is_err(),
            "B must NOT receive a control message addressed to A"
        );

        // ── 5) Clean shutdown — no panic, no lingering tasks. ──
        drop(runtime_a);
        drop(runtime_b);
        drop(gw);
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    drop(broker);
}
