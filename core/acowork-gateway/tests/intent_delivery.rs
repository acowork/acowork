//! Intent delivery end-to-end tests (intent-delivery-tech-debt-plan
//! PR-A/PR-B/PR-C acceptance): wake-then-deliver against a real embedded
//! broker, D4 sender attribution, D8 permission decisions, D3 mailbox
//! persistence, and the cron → delivery reuse path.
//!
//! The fake Node answers the control plane exactly like `acowork-node`
//! does (NodeEvent reply + retained AgentStatus flip), so the D2 race
//! window is exercised against the real registry signal rather than a
//! mock of it.

use std::sync::Arc;
use std::time::Duration;

use acowork_core::mqtt_proto::{
    data_envelope, AgentStatus, ControlCommand, DataEnvelope, NodeEvent,
};
use acowork_core::AgentManifest;
use prost::Message as _;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use tokio::sync::mpsc::UnboundedReceiver;

use acowork_gateway::gateway::state::{AgentInfo, GatewayState};
use acowork_gateway::handlers::server::SharedState;
use acowork_gateway::http::intent_api::{attribute_sender, permission_denial};
use acowork_gateway::intent::delivery::{self, DeliveryContext, DeliveryRequest};
use acowork_gateway::intent::mailbox::{IntentMailbox, MailboxEntry};
use acowork_gateway::mqtt::agent_registry::{new_shared_registry, AgentOnlineState};
use acowork_gateway::mqtt::client::{GatewayMqttClient, MqttQoS};
use acowork_gateway::mqtt::node_control::NodeControlClient;
use acowork_gateway::mqtt::start_broker;

const SENDER: &str = "11111111-1111-4111-8111-111111111111";
const TARGET: &str = "22222222-2222-4222-8222-222222222222";
const NODE: &str = "node-test-1";

fn agent_info(instance: &str, package: &str, node: &str, permissions: &str) -> AgentInfo {
    let manifest = AgentManifest::from_toml(&format!(
        r#"
        agent_id = "{package}"
        version = "1.0.0"
        name = "t"
        description = "t"
        author = "t"
        runtime_version = "0.1.0"
        {permissions}
        "#
    ))
    .expect("valid manifest");
    AgentInfo {
        instance_id: instance.into(),
        agent_id: package.into(),
        version: "1.0.0".into(),
        name: "t".into(),
        install_path: String::new(),
        manifest,
        node_id: node.into(),
    }
}

fn req(target: &str) -> DeliveryRequest {
    DeliveryRequest {
        message_id: uuid::Uuid::new_v4().to_string(),
        conversation_id: String::new(),
        from_instance: SENDER.into(),
        from_session_id: String::new(),
        target_instance: target.into(),
        action: "do".into(),
        params_json: r#"{"k":1}"#.into(),
        source_chat_session_id: String::new(),
        repo: String::new(),
        kind: "request".into(),
        in_reply_to: String::new(),
        status: String::new(),
    }
}

fn entry(
    message_id: &str,
    from: &str,
    to: &str,
    created_at: &str,
    delivered_at: &str,
) -> MailboxEntry {
    MailboxEntry {
        message_id: message_id.into(),
        conversation_id: String::new(),
        from_instance: from.into(),
        from_session_id: String::new(),
        to_instance: to.into(),
        body: "{}".into(),
        created_at: created_at.into(),
        delivered_at: delivered_at.into(),
        result_ref: String::new(),
        kind: "request".into(),
        status: String::new(),
        source_chat_session_id: String::new(),
    }
}

/// Subscribe to topics; the returned receiver yields raw publishes.
async fn subscribe(
    port: u16,
    client_id: &str,
    topics: &[&str],
) -> (AsyncClient, UnboundedReceiver<(String, Vec<u8>)>) {
    let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(2));
    let (client, mut eventloop) = AsyncClient::new(opts, 10);
    for t in topics {
        client.subscribe(*t, QoS::AtLeastOnce).await.unwrap();
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    let _ = tx.send((p.topic.clone(), p.payload.to_vec()));
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
    (client, rx)
}

/// Wait until the broker TCP port actually accepts publishes.
async fn wait_publish_ready(client: &AsyncClient) {
    for _ in 0..200 {
        if client
            .publish("acowork/test/ready", QoS::AtLeastOnce, false, "ping")
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("broker never accepted a publish");
}

/// A publish-only client whose eventloop is kept polling in the
/// background (dropping the EventLoop would silently swallow every
/// publish — rumqttc accepts into a dead channel).
async fn publisher(port: u16, client_id: &str) -> AsyncClient {
    let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(2));
    let (client, mut el) = AsyncClient::new(opts, 10);
    tokio::spawn(async move {
        loop {
            match el.poll().await {
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
    wait_publish_ready(&client).await;
    client
}

fn decode_envelope(bytes: &[u8]) -> DataEnvelope {
    DataEnvelope::decode(bytes).expect("DataEnvelope")
}

fn status_bytes(instance: &str, node: &str) -> Vec<u8> {
    DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::AgentStatus(AgentStatus {
            instance_id: instance.into(),
            agent_id: "com.test.target".into(),
            node_id: node.into(),
            online: true,
            ..Default::default()
        })),
    }
    .encode_to_vec()
}

/// D1 + D2 + D3 end-to-end: offline target, fake node wakes it, the
/// retained AgentStatus flip gates the publish, the mailbox records
/// `delivered`, and the target topic really receives the Intent with
/// the Gateway-stamped `from` and the wire-carried `message_id`.
#[tokio::test]
async fn wake_then_deliver_end_to_end() {
    let port = 29911;
    start_broker("127.0.0.1", port).expect("broker");

    // Target-side subscriber: sees the delivered Intent.
    let (probe, mut trx) = subscribe(
        port,
        "t1-target",
        &[&format!("acowork/agents/{TARGET}/sessions/control/intent")],
    )
    .await;
    wait_publish_ready(&probe).await;

    // Fake node: answers every control command with `ok` and flips the
    // retained AgentStatus online — the contract acowork-node implements
    // (ADR-055 §6.4, ADR-085).
    let (_nc, mut nrx) = subscribe(
        port,
        "t1-node",
        &[&format!("acowork/nodes/{NODE}/agents/{TARGET}/control/#")],
    )
    .await;
    let node_client = publisher(port, "t1-node-pub").await;
    tokio::spawn(async move {
        while let Some((_topic, bytes)) = nrx.recv().await {
            let env = decode_envelope(&bytes);
            let cmd = match env.payload {
                Some(data_envelope::Payload::NodeControlCommand(c)) => c,
                _ => continue,
            };
            let reply = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::NodeEvent(NodeEvent {
                    node_id: NODE.into(),
                    request_id: cmd.request_id.clone(),
                    status: "ok".into(),
                    message: "spawned".into(),
                    result_json: None,
                })),
            };
            let _ = node_client
                .publish(
                    &format!("acowork/nodes/{NODE}/agents/{TARGET}/events"),
                    QoS::AtLeastOnce,
                    false,
                    reply.encode_to_vec(),
                )
                .await;
            let _ = node_client
                .publish(
                    &format!("acowork/agents/{TARGET}/status"),
                    QoS::AtLeastOnce,
                    true,
                    status_bytes(TARGET, NODE),
                )
                .await;
        }
    });

    // Gateway-side objects. The publisher also receives (like
    // production's dispatch client): the callback routes NodeEvent
    // replies into the control client's pending table and feeds the
    // registry from the status topic — both via the broker's own
    // delivery, no second subscriber racing for the same message.
    let registry = new_shared_registry();
    // The callback is sync (runs on the eventloop task) — hand the raw
    // frames to a processing task instead of blocking the loop.
    let (frame_tx, mut frame_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();
    let cb: acowork_gateway::mqtt::client::MqttMessageCallback =
        std::sync::Arc::new(move |topic: String, payload: Vec<u8>| {
            let _ = frame_tx.send((topic, payload));
        });
    let mqtt = GatewayMqttClient::new_publisher_with_callback(
        "127.0.0.1",
        port,
        cb,
    )
    .await
    .expect("gateway publisher");
    mqtt.subscribe(
        &format!("acowork/nodes/{NODE}/agents/{TARGET}/events"),
        MqttQoS::AtLeastOnce,
    )
        .await
        .expect("subscribe events");
    mqtt.subscribe("acowork/agents/+/status", MqttQoS::AtLeastOnce)
        .await
        .expect("subscribe status");
    let node_control = NodeControlClient::new(Arc::new(mqtt.clone()));
    {
        let nc = node_control.clone();
        let reg = registry.clone();
        tokio::spawn(async move {
            while let Some((topic, payload)) = frame_rx.recv().await {
                if let Ok(env) = DataEnvelope::decode(payload.as_slice()) {
                    match env.payload {
                        Some(data_envelope::Payload::NodeEvent(ev)) => {
                            nc.handle_event(ev).await;
                        }
                        Some(data_envelope::Payload::AgentStatus(_)) => {
                            reg.write().await.update_from_mqtt(&topic, &payload);
                        }
                        _ => {}
                    }
                }
            }
        });
    }



    let mut gw = GatewayState::new("/tmp/acowork-intent-e2e");
    gw.add_installed(agent_info(
        SENDER,
        "com.test.sender",
        NODE,
        r#"[[permissions]]
type = "IntentSend"
value = "com.test.target"
"#,
    ));
    gw.add_installed(agent_info(
        TARGET,
        "com.test.target",
        NODE,
        r#"[[permissions]]
type = "IntentReceive"
"#,
    ));
    let state: SharedState = Arc::new(tokio::sync::RwLock::new(gw));
    let mailbox = IntentMailbox::in_memory();

    let ctx = DeliveryContext {
        state: &state,
        registry: &registry,
        node_control: Some(&node_control),
        mqtt: Some(&mqtt),
        mailbox: Some(&mailbox),
    };

    let r = req(TARGET);
    delivery::wake_and_deliver(&ctx, &r, Duration::from_secs(10))
        .await
        .expect("wake-then-deliver must succeed");

    // D3: the mailbox holds the delivered fact.
    let e = mailbox.get(&r.message_id).expect("mailbox entry");
    assert!(e.delivered(), "entry must be marked delivered");

    // The target really received the Intent, with the stamped `from`.
    let (topic, bytes) = tokio::time::timeout(Duration::from_secs(5), trx.recv())
        .await
        .expect("intent publish must arrive")
        .expect("channel open");
    assert!(topic.ends_with("/sessions/control/intent"));
    let env = decode_envelope(&bytes);
    let command = match env.payload {
        Some(data_envelope::Payload::ControlCommand(c)) => c,
        other => panic!("expected ControlCommand, got {other:?}"),
    };
    assert_eq!(command.instance_id, TARGET);
    let intent = match command.command {
        Some(acowork_core::mqtt_proto::control_command::Command::Intent(i)) => i,
        other => panic!("expected Intent, got {other:?}"),
    };
    assert_eq!(intent.from, SENDER, "from is the stamped sender identity");
    assert_eq!(intent.action, "do");
    assert_eq!(
        intent.message_id, r.message_id,
        "message_id rides the wire envelope"
    );
}

/// D2: a node that answers `ok` but never flips the AgentStatus must
/// NOT get a publish — the wake timeout is the delivery refusal, and
/// the refusal is still an observable mailbox fact.
#[tokio::test]
async fn spawn_ok_without_status_flip_refuses_delivery() {
    let port = 29912;
    start_broker("127.0.0.1", port).expect("broker");
    let (probe, mut trx) = subscribe(
        port,
        "t2-target",
        &[&format!("acowork/agents/{TARGET}/sessions/control/intent")],
    )
    .await;
    wait_publish_ready(&probe).await;

    let (_nc, mut nrx) = subscribe(
        port,
        "t2-node",
        &[&format!("acowork/nodes/{NODE}/agents/{TARGET}/control/#")],
    )
    .await;
    let node_client = publisher(port, "t2-node-pub").await;
    tokio::spawn(async move {
        while let Some((_topic, bytes)) = nrx.recv().await {
            let env = decode_envelope(&bytes);
            let cmd = match env.payload {
                Some(data_envelope::Payload::NodeControlCommand(c)) => c,
                _ => continue,
            };
            // Reply ok, but NO AgentStatus flip — the D2 race.
            let reply = DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::NodeEvent(NodeEvent {
                    node_id: NODE.into(),
                    request_id: cmd.request_id.clone(),
                    status: "ok".into(),
                    message: "spawned".into(),
                    result_json: None,
                })),
            };
            let _ = node_client
                .publish(
                    &format!("acowork/nodes/{NODE}/agents/{TARGET}/events"),
                    QoS::AtLeastOnce,
                    false,
                    reply.encode_to_vec(),
                )
                .await;
        }
    });

    let mqtt = GatewayMqttClient::new_publisher("127.0.0.1", port)
        .await
        .unwrap();
    let node_control = NodeControlClient::new(Arc::new(mqtt.clone()));
    let registry = new_shared_registry();
    let mut gw = GatewayState::new("/tmp/acowork-intent-e2e-2");
    gw.add_installed(agent_info(TARGET, "com.test.target", NODE, ""));
    let state: SharedState = Arc::new(tokio::sync::RwLock::new(gw));
    let mailbox = IntentMailbox::in_memory();

    let ctx = DeliveryContext {
        state: &state,
        registry: &registry,
        node_control: Some(&node_control),
        mqtt: Some(&mqtt),
        mailbox: Some(&mailbox),
    };
    let r = req(TARGET);
    let err = delivery::wake_and_deliver(&ctx, &r, Duration::from_millis(400))
        .await
        .expect_err("no status flip must refuse delivery");
    assert_eq!(err.code(), "delivery_unavailable");
    let e = mailbox.get(&r.message_id).unwrap();
    assert!(!e.delivered(), "refusal is an observable undelivered fact");
    // And nothing was published to the target.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), trx.recv())
            .await
            .is_err(),
        "no Intent publish may reach an offline target"
    );
}

/// D3 persistence: delivered facts survive reopening the mailbox file.
#[test]
fn mailbox_persistence_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("intent_mailbox.json");
    {
        let mb = IntentMailbox::open(&path);
        mb.record(entry(
            "m-p",
            SENDER,
            TARGET,
            &chrono::Utc::now().to_rfc3339(),
            "",
        ))
        .unwrap();
        mb.mark_delivered("m-p").unwrap();
    }
    let mb2 = IntentMailbox::open(&path);
    let e = mb2.get("m-p").expect("entry survives reopen");
    assert!(e.delivered());
}

/// D8 pure logic: the manifest permission matrix.
#[test]
fn permission_denial_matrix() {
    let sender = |perms: &str| agent_info(SENDER, "com.test.sender", NODE, perms);
    let target = |perms: &str| agent_info(TARGET, "com.test.target", NODE, perms);

    // bare send + bare receive → allowed
    assert!(permission_denial(
        &sender(r#"[[permissions]]
type = "IntentSend"
"#),
        &target(r#"[[permissions]]
type = "IntentReceive"
"#)
    )
    .is_none());

    // scoped send to the target package → allowed
    assert!(permission_denial(
        &sender(r#"[[permissions]]
type = "IntentSend"
value = "com.test.target"
"#),
        &target(r#"[[permissions]]
type = "IntentReceive"
"#)
    )
    .is_none());

    // scoped send to a different package → denied on the sender side
    assert!(permission_denial(
        &sender(r#"[[permissions]]
type = "IntentSend"
value = "com.other"
"#),
        &target(r#"[[permissions]]
type = "IntentReceive"
"#)
    )
    .is_some());

    // sender declares nothing → denied
    assert!(permission_denial(
        &sender(""),
        &target(r#"[[permissions]]
type = "IntentReceive"
"#)
    )
    .is_some());

    // receiver declares nothing → denied on the receiver side
    assert!(
        permission_denial(
            &sender(r#"[[permissions]]
type = "IntentSend"
"#),
            &target("")
        )
        .is_some()
    );

    // receive scoped to the sender package → allowed
    assert!(permission_denial(
        &sender(r#"[[permissions]]
type = "IntentSend"
"#),
        &target(r#"[[permissions]]
type = "IntentReceive"
value = "com.test.sender"
"#)
    )
    .is_none());
}

/// D4 pure logic: sender attribution — hosted-node match vs mismatch.
#[test]
fn sender_attribution_matrix() {
    let state = |node: &str| AgentOnlineState {
        online: true,
        last_updated: std::time::Instant::now(),
        instance_id: TARGET.into(),
        agent_id: "com.test.target".into(),
        node_id: node.into(),
    };

    // Node-token caller, sender hosted there → ok.
    assert!(attribute_sender(Some(NODE.into()), Some(&state(NODE))).is_ok());
    // Node-token caller, sender hosted elsewhere → rejected.
    assert!(attribute_sender(Some(NODE.into()), Some(&state("node-other"))).is_err());
    // Node-token caller, sender never seen online → rejected.
    assert!(attribute_sender(Some(NODE.into()), None).is_err());
    // Local mode (no token), sender online → accepted.
    assert!(attribute_sender(None, Some(&state(NODE))).is_ok());
    // Local mode, sender unknown → rejected.
    assert!(attribute_sender(None, None).is_err());
}

/// Cron reuse (D1/D2/D3): the scheduler path shares the delivery entry.
/// An offline "local"-sentinel target records an undelivered fact
/// instead of the old silent skip; an online target delivers.
#[tokio::test]
async fn cron_trigger_uses_the_same_delivery_entry() {
    let port = 29913;
    start_broker("127.0.0.1", port).expect("broker");
    let (probe, mut trx) = subscribe(
        port,
        "t3-target",
        &[&format!("acowork/agents/{TARGET}/sessions/control/intent")],
    )
    .await;
    wait_publish_ready(&probe).await;

    let mqtt = GatewayMqttClient::new_publisher("127.0.0.1", port)
        .await
        .unwrap();
    let registry = new_shared_registry();

    let mut gw = GatewayState::new("/tmp/acowork-intent-cron");
    let ghost = "33333333-3333-4333-8333-333333333333";
    gw.add_installed(agent_info(TARGET, "com.test.target", NODE, ""));
    gw.add_installed(agent_info(ghost, "com.test.ghost", "local", ""));
    let state: SharedState = Arc::new(tokio::sync::RwLock::new(gw));
    let mailbox = IntentMailbox::in_memory();

    let ctx = DeliveryContext {
        state: &state,
        registry: &registry,
        node_control: None,
        mqtt: Some(&mqtt),
        mailbox: Some(&mailbox),
    };

    // Case 1: offline + "local" sentinel → NoHostingNode, recorded
    // undelivered (the observable failure the plan demands).
    let r1 = req(ghost);
    let err = delivery::wake_and_deliver(&ctx, &r1, Duration::from_millis(200))
        .await
        .expect_err("offline local-sentinel target must fail");
    assert!(matches!(err, delivery::DeliveryError::NoHostingNode(_)));
    assert!(!mailbox.get(&r1.message_id).unwrap().delivered());

    // Case 2: online target (retained status arrives exactly like a
    // Runtime's) → delivered via the same entry.
    registry
        .write()
        .await
        .update_from_mqtt(&format!("acowork/agents/{TARGET}/status"), &status_bytes(TARGET, NODE));
    let r2 = req(TARGET);
    delivery::wake_and_deliver(&ctx, &r2, Duration::from_secs(5))
        .await
        .expect("online target delivers");
    assert!(mailbox.get(&r2.message_id).unwrap().delivered());
    let (_t, _b) = tokio::time::timeout(Duration::from_secs(5), trx.recv())
        .await
        .expect("intent arrives")
        .expect("channel open");
}

/// ControlCommand sanity: the Runtime-side parse of the delivered
/// envelope (message_id / repo fields survive the wire).
#[test]
fn intent_wire_fields_roundtrip() {
    let intent = acowork_core::mqtt_proto::Intent {
        from: SENDER.into(),
        action: "a".into(),
        params_json: "{}".into(),
        message_id: "m-7".into(),
        conversation_id: "c-1".into(),
        source_agent_session_id: "s-1".into(),
        source_chat_session_id: "ch-1".into(),
        repo: "https://github.com/o/p".into(),
        kind: "request".into(),
        in_reply_to: String::new(),
        status: String::new(),
    };
    let cmd = ControlCommand {
        instance_id: TARGET.into(),
        command: Some(acowork_core::mqtt_proto::control_command::Command::Intent(
            intent,
        )),
    };
    let env = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::ControlCommand(cmd)),
    };
    let decoded = decode_envelope(&env.encode_to_vec());
    let control = match decoded.payload {
        Some(data_envelope::Payload::ControlCommand(c)) => c,
        other => panic!("expected ControlCommand, got {other:?}"),
    };
    let acowork_core::mqtt_proto::control_command::Command::Intent(back) =
        control.command.expect("intent command")
    ;
    assert_eq!(back.message_id, "m-7");
    assert_eq!(back.conversation_id, "c-1");
    assert_eq!(back.source_agent_session_id, "s-1");
    assert_eq!(back.source_chat_session_id, "ch-1");
    assert_eq!(back.repo, "https://github.com/o/p");
}
