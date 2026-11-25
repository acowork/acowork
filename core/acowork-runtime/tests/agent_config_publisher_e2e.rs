//! Integration + E2E tests for `MqttAgentConfigPublisher`.
//!
//! Scope:
//! - The wire contract of `acowork/agents/{id}/config` retained snapshot:
//!   DataEnvelope::AgentConfig round-trip on a real broker.
//! - The retain semantics that make the Desktop Tools panel "late
//!   refresh" possible: a subscriber that connects AFTER the publish
//!   must still receive the snapshot.
//! - Best-effort degradation: publish against a missing broker does
//!   not panic, log floods, or block the caller.
//!
//! Why this file (and not the unit tests in `agent_config_publisher.rs`):
//! - Unit tests cover construction + payload shape.
//! - These cover the on-the-wire guarantee that makes the bug fix work
//!   end-to-end. The Desktop subscriber (`chatStore.ts::case
//!   "agent_config"` + `ToolsTab.acowork:refresh-agent-config` handler)
//!   depends on every property below.

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use acowork_core::mqtt_proto::{AgentConfig, DataEnvelope, data_envelope::Payload};
use acowork_gateway::mqtt::start_broker;
use acowork_runtime::mqtt::{
    MqttAgentConfigPublisher, MqttConnectConfig, RuntimeMqttClient, new_shared_cache,
};
use prost::Message as _;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};

/// Per-test unique broker port (parallel cargo test would otherwise cross-talk).
fn fresh_broker_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(29975);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Poll eventloop until we see a Publish on target_topic; decode and return AgentConfig.
async fn wait_for_retained_publish(
    eventloop: &mut rumqttc::EventLoop,
    target_topic: &str,
    budget: Duration,
) -> AgentConfig {
    let start = Instant::now();
    while start.elapsed() < budget {
        let remaining = budget.saturating_sub(start.elapsed());
        match tokio::time::timeout(remaining.min(Duration::from_millis(100)), eventloop.poll())
            .await
        {
            Ok(Ok(Event::Incoming(Incoming::Publish(p)))) => {
                if p.topic != target_topic {
                    continue;
                }
                let env = DataEnvelope::decode(p.payload.as_ref()).expect("DataEnvelope decode");
                match env.payload {
                    Some(Payload::AgentConfig(ac)) => return ac,
                    other => panic!("expected AgentConfig payload, got {:?}", other),
                }
            }
            Ok(_) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(_) => {}
        }
    }
    panic!(
        "did not receive retained publish on '{}' within {:?}",
        target_topic, budget
    );
}

/// Connect a test Runtime and return it together with the ADR-073
/// instance id it bound (the caller keys its subscriber topic on that id).
async fn connect_runtime(port: u16, agent_id: &str) -> (RuntimeMqttClient, String) {
    let cache = new_shared_cache();
    let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel();
    let instance_id = uuid::Uuid::new_v4().to_string();
    let client = RuntimeMqttClient::spawn(MqttConnectConfig {
        host: "127.0.0.1",
        port,
        agent_id,
        // ADR-073: the broker client id and the `.../config` snapshot
        // topic are keyed on the instance id — the package `agent_id` is
        // display/payload only.
        instance_id: &instance_id,
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
    .expect("RuntimeMqttClient connect");
        assert!(client.wait_bootstrapped(std::time::Duration::from_secs(15)).await > 0, "bootstrap must complete");
    (client, instance_id)
}

// Test 1 — Publish round-trips over a real broker
#[tokio::test(flavor = "multi_thread")]
async fn e2e_publish_delivers_retained_agent_config() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).expect("broker start");

    let (runtime, instance_id) = connect_runtime(port, "com.test.e2e.agent").await;
    let publisher = MqttAgentConfigPublisher::from_runtime_client(&runtime);
    assert_eq!(publisher.agent_id(), "com.test.e2e.agent");

    let mut opts = MqttOptions::new("test:subscriber", "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (sub_client, mut eventloop) = AsyncClient::new(opts, 10);
    let target = format!("acowork/agents/{}/config", instance_id);
    sub_client
        .subscribe(&target, QoS::AtLeastOnce)
        .await
        .expect("subscribe");

    tokio::time::sleep(Duration::from_millis(150)).await;

    let payload_json = r#"{"temperature": 0.7, "active_mcp_servers": ["pm", "playwright"]}"#;
    publisher.publish(payload_json.to_string()).await;

    let ac = wait_for_retained_publish(&mut eventloop, &target, Duration::from_secs(5)).await;
    assert_eq!(ac.agent_id, "com.test.e2e.agent");
    assert!(ac.config_json.contains("active_mcp_servers"));
    assert!(ac.config_json.contains("pm"));

    drop(sub_client);
    drop(runtime);
    drop(broker);
}

// Test 2 — Retained snapshot survives late subscription
#[tokio::test(flavor = "multi_thread")]
async fn e2e_retained_snapshot_arrives_to_late_subscriber() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).expect("broker start");

    let (runtime, instance_id) = connect_runtime(port, "com.test.retained").await;
    let publisher = MqttAgentConfigPublisher::from_runtime_client(&runtime);

    publisher
        .publish(r#"{"active_mcp_servers":["pm"]}"#.to_string())
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut opts = MqttOptions::new("test:late", "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (_sub_client, mut eventloop) = AsyncClient::new(opts, 10);
    let target = format!("acowork/agents/{}/config", instance_id);
    _sub_client
        .subscribe(&target, QoS::AtLeastOnce)
        .await
        .expect("subscribe");

    let ac = wait_for_retained_publish(&mut eventloop, &target, Duration::from_secs(5)).await;
    assert_eq!(ac.agent_id, "com.test.retained");
    assert!(ac.config_json.contains("active_mcp_servers"));

    drop(runtime);
    drop(broker);
}

// Test 3 — Publish is best-effort and never deadlocks the caller.
//
// Simulates the runtime-bootstrap window where the broker has just been
// taken down (or never came up): the publisher's AsyncClient has no live
// connection, but the publisher call must still resolve within budget
// rather than block forever or panic.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_publish_without_broker_is_graceful() {
    let port = fresh_broker_port();
    let broker = start_broker("127.0.0.1", port).expect("broker start");

    // Construct runtime and publisher against a healthy broker first.
    let (runtime, _instance_id) = connect_runtime(port, "com.test.no-broker").await;
    let publisher = MqttAgentConfigPublisher::from_runtime_client(&runtime);

    // Now drop the broker — the runtime's EventLoop sees a disconnect
    // but `AsyncClient::publish` returns Ok as soon as the request is
    // queued. The publisher's contract is best-effort: the call must
    // resolve, not deadlock.
    drop(broker);

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        publisher.publish(r#"{"x":1}"#.to_string()),
    )
    .await;
    assert!(
        result.is_ok(),
        "publish() must not deadlock when broker is unreachable"
    );

    drop(runtime);
}
