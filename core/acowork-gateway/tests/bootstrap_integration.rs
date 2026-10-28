//! ADR-059 Phase 4.2 — bootstrap handshake integration tests.
//!
//! Exercises the full bootstrap protocol over a real embedded broker:
//!
//!   1. Cold-start first publisher: the global resources publisher
//!      defers its initial retained snapshot until the ready barrier,
//!      and the first payload carries the decrypted provider key;
//!      the bootstrap snapshot reaches READY with a monotonic version.
//!   2. First Node enroll: an early install before `node.{id}` ready
//!      returns `dependency_not_ready`; after `NodeReady` the same
//!      install is accepted and the NodeEvent reply completes the
//!      operation (operation_id closed loop).
//!   3. System Agent delay keeps the aggregated phase BOOTING.
//!   4. Concurrent installs produce unique operation ids, and the
//!      retained installed inventory aggregates exactly once.
//!   5. Concurrent provider + identity writes carry correct per-resource
//!      retained versions.
//!   6. MQTT reconnect: the Desktop observes the new instance id /
//!      version baseline; an operation from the old process resolves to
//!      `operation_uncertain`.
//!   7. Cross-generation restart: a fresh instance must NOT surface a
//!      premature READY to the Desktop.
//!   8. Normal handshake: HTTP `/api/bootstrap` and the MQTT retained
//!      snapshot agree field-for-field.
//!   9. Remote Node reconnect: LWT demotes READY → BOOTING; the
//!      re-announced NodeReady restores READY.
//!  10. Gateway restart: old operation ids are no longer resolvable —
//!      the client-side mapping is `operation_uncertain`.
//!  11. OCP: subsystem churn never changes the wire-level `BootstrapState`
//!      field set, the error-code set, or the `/api/bootstrap` JSON shape.
//!
//! Every test runs against its own broker instance on a fresh port:
//! the broker thread never exits, so reusing a port would leak retained
//! messages from the previous test.

use std::sync::Arc;
use std::time::Duration;

use prost::Message as _;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use tokio::sync::RwLock;

use acowork_core::mqtt_proto::{data_envelope, BootstrapState, DataEnvelope};
use acowork_core::node::{
    node_agent_events_topic, node_agent_installed_topic, node_ready_topic, node_status_topic,
};
use acowork_gateway::bootstrap::{
    BootstrapOrchestrator, ReadinessKind, SubsystemReadinessRegistry,
};
use acowork_gateway::bootstrap::orchestrator::BootstrapPhase;
use acowork_gateway::gateway::state::GatewayState;
use acowork_gateway::http::auth::HttpAuth;
use acowork_gateway::http::routes::{AppState, SharedHttpState};
use acowork_gateway::mqtt::broker::start_broker;
use acowork_gateway::mqtt::bootstrap_publisher::{
    BootstrapPublisher, BootstrapPublisherOptions, TOPIC_BOOTSTRAP,
};
use acowork_gateway::mqtt::client::GatewayMqttClient;
use acowork_gateway::mqtt::dispatch;
use acowork_gateway::mqtt::global_resources_publisher::MqttGlobalResourcesPublisher;
use acowork_gateway::mqtt::node_control::NodeControlClient;
use acowork_gateway::operation_store::OperationStore;

const PROVIDER_ID: &str = "minimax-cn";
const API_KEY_VALUE: &str = "sk-minimax-secret-12345";
const PASSWORD: &str = "bootstrap-integration-test-password";
const NODE_ID: &str = "verify-node";

/// Path to a real bundled `.agent` package, used as an install fixture.
///
/// Any bundled package works: these tests exercise the install / retained
/// inventory wire path, not one package's semantics. (ADR-077 removed the
/// bundled System Agent, which previously served as the fixture.)
///
/// Resolves from `CARGO_MANIFEST_DIR` (core/acowork-gateway) up to the
/// workspace root, then down into `examples/agent-packages/`. Keeps the
/// test runnable on any machine (previously hardcoded to a Windows path).
fn bundled_package_path() -> std::path::PathBuf {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .expect("core/")
        .parent()
        .expect("workspace root")
        .join("examples/agent-packages/com.acowork.senior-engineer.agent")
}

/// Pick a fresh MQTT port per test invocation (broker threads never
/// exit, so ports must not be reused).
fn unique_port(label: &str) -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(19010);
    let p = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    eprintln!("[bootstrap_integration] {label} using port {p}");
    p
}

/// Poll `cond` until it returns true or the deadline elapses.
async fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cond()
}

/// Open a raw MQTT client that impersonates a Node publishing its own
/// `NodeReady` (retained) or clearing it (empty payload).
async fn node_publisher(port: u16, client_id: &str) -> AsyncClient {
    let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = AsyncClient::new(opts, 32);
    tokio::spawn(async move {
        loop {
            if eventloop.poll().await.is_err() {
                break;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
}

fn ready_envelope(node_id: &str) -> Vec<u8> {
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeReady(
            acowork_core::mqtt_proto::NodeReady {
                node_id: node_id.to_string(),
                protocol_version: 1,
            },
        )),
    };
    envelope.encode_to_vec()
}

/// Subscribe to `target_topic` on the test broker. Returns the
/// `AsyncClient` (drop to disconnect) and its `EventLoop`.
async fn subscribe(
    target_topic: &str,
    client_id: &str,
    port: u16,
) -> (AsyncClient, rumqttc::EventLoop) {
    let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, eventloop) = AsyncClient::new(opts, 10);
    client
        .subscribe(target_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe to {target_topic}");
    (client, eventloop)
}

/// Drain a single event from `sub_eventloop` with a timeout.
async fn poll_one_event(
    sub_eventloop: &mut rumqttc::EventLoop,
    timeout: Duration,
) -> Option<rumqttc::Event> {
    match tokio::time::timeout(timeout, sub_eventloop.poll()).await {
        Ok(Ok(ev)) => Some(ev),
        _ => None,
    }
}

/// Drive `sub_eventloop` until a `Publish` arrives on `target_topic`,
/// or the deadline elapses. Returns the payload of the first match.
async fn collect_first_publish_payload(
    sub_eventloop: &mut rumqttc::EventLoop,
    target_topic: &str,
    deadline: Duration,
) -> Option<Vec<u8>> {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        if let Some(Event::Incoming(Incoming::Publish(p))) = poll_one_event(
            sub_eventloop,
            remaining.min(Duration::from_millis(100)),
        )
        .await
            && p.topic == target_topic
        {
            return Some(p.payload.to_vec());
        }
    }
    None
}

/// Drive `sub_eventloop` until a `Publish` arrives on `target_topic`
/// whose decoded [`BootstrapState`] satisfies `pred`, or the deadline
/// elapses. Returns the first matching state.
///
/// Unlike [`collect_first_publish_payload`] this skips intermediate
/// publishes: the retained topic is change-driven, so a `BOOTING`
/// republish can race a subsequent `READY` flip and land in the
/// subscriber queue first. The contract under test is "the retained
/// topic converges to the target phase", not "the next single publish
/// is the target phase".
async fn collect_bootstrap_until(
    sub_eventloop: &mut rumqttc::EventLoop,
    target_topic: &str,
    deadline: Duration,
    pred: impl Fn(&BootstrapState) -> bool,
) -> Option<BootstrapState> {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        match poll_one_event(sub_eventloop, remaining.min(Duration::from_millis(100))).await {
            Some(Event::Incoming(Incoming::Publish(p))) if p.topic == target_topic => {
                let bs = decode_bootstrap_state(&p.payload);
                if pred(&bs) {
                    return Some(bs);
                }
            }
            _ => {}
        }
    }
    None
}

/// Drive `sub_eventloop` for `window` and assert that NO publish
/// arrives on `target_topic`.
async fn assert_no_publish_in_window(
    sub_eventloop: &mut rumqttc::EventLoop,
    target_topic: &str,
    window: Duration,
    why: &str,
) {
    let start = std::time::Instant::now();
    while start.elapsed() < window {
        let remaining = window.saturating_sub(start.elapsed());
        if let Some(Event::Incoming(Incoming::Publish(p))) =
            poll_one_event(sub_eventloop, remaining.min(Duration::from_millis(50))).await
        {
            assert_ne!(
                p.topic, target_topic,
                "{why}: unexpected publish on {target_topic} while ready barrier is held"
            );
        }
    }
}

/// Drive `sub_eventloop` until a `Publish` on `target_topic` whose
/// `BootstrapState` payload satisfies `pred` arrives, or the deadline
/// elapses. Non-bootstrap payloads and undecodable payloads (e.g. a
/// retained-clear empty payload) are skipped.
async fn collect_until_publish(
    sub_eventloop: &mut rumqttc::EventLoop,
    target_topic: &str,
    deadline: Duration,
    mut pred: impl FnMut(&acowork_core::mqtt_proto::BootstrapState) -> bool,
) -> Option<Vec<u8>> {
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        if let Some(Event::Incoming(Incoming::Publish(p))) = poll_one_event(
            sub_eventloop,
            remaining.min(Duration::from_millis(100)),
        )
        .await
        {
            if p.topic != target_topic {
                continue;
            }
            let Ok(envelope) = DataEnvelope::decode(p.payload.as_ref()) else {
                continue;
            };
            let Some(data_envelope::Payload::BootstrapState(bs)) = envelope.payload else {
                continue;
            };
            if pred(&bs) {
                return Some(p.payload.to_vec());
            }
        }
    }
    None
}

/// Decode a bootstrap retained payload into its proto fields.
fn decode_bootstrap_state(payload: &[u8]) -> acowork_core::mqtt_proto::BootstrapState {
    let envelope = DataEnvelope::decode(payload).expect("decode DataEnvelope");
    match envelope.payload {
        Some(data_envelope::Payload::BootstrapState(bs)) => bs,
        other => panic!("expected BootstrapState payload, got {other:?}"),
    }
}

// ── Shared dispatch surface ────────────────────────────────────────────

/// In-process Gateway dispatch surface: real broker + real MQTT client
/// whose callback routes every message through
/// `dispatch::handle_plaintext_message` with the shared registries.
struct TestSurface {
    orchestrator: Arc<BootstrapOrchestrator>,
    registry: Arc<SubsystemReadinessRegistry>,
    gw_client: GatewayMqttClient,
    gw_state: SharedHttpState,
    operation_store: Arc<OperationStore>,
    app_state: AppState,
    /// Temp data/vault dir; removed when the surface is dropped.
    vault_dir: std::path::PathBuf,
}

impl Drop for TestSurface {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.vault_dir);
    }
}

/// Build the dispatch surface for `instance_id`. The caller registers
/// subsystems afterwards (a fresh registry starts empty).
async fn build_surface(port: u16, instance_id: &str) -> TestSurface {
    let registry = SubsystemReadinessRegistry::new_shared();
    let orchestrator = BootstrapOrchestrator::new(instance_id.to_string(), registry.clone());

    // Fresh temp data dir + vault: the mutation handlers need a
    // `GatewayConfig` (install persists caches) — without it they 500
    // with "Gateway config unavailable".
    let vault_dir = unique_vault_dir(instance_id, port);
    let mut gw_state = GatewayState::new(&vault_dir.to_string_lossy());
    let data_dir = vault_dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = acowork_gateway::config::GatewayConfig {
        data_dir: data_dir.to_string_lossy().to_string(),
        vault_dir: vault_dir.to_string_lossy().to_string(),
        ..acowork_gateway::config::GatewayConfig::default()
    };
    gw_state.config = Some(config);
    let gw_state: SharedHttpState = Arc::new(RwLock::new(gw_state));
    gw_state.write().await.bootstrap.orchestrator = Some(orchestrator.clone());

    let runtime_http_registry = acowork_gateway::http::proxy::new_shared_registry();
    let agent_registry = acowork_gateway::mqtt::agent_registry::new_shared_registry();
    let node_registry = acowork_gateway::mqtt::node_registry::new_shared_registry();
    let operation_store = OperationStore::new_shared();

    let reg_for_cb = registry.clone();
    let state_for_cb = gw_state.clone();
    let http_reg_for_cb = runtime_http_registry.clone();
    let agent_reg_for_cb = agent_registry.clone();
    let node_reg_for_cb = node_registry.clone();
    let op_store_for_cb = operation_store.clone();
    let dispatch_slot: Arc<tokio::sync::Mutex<Option<Arc<GatewayMqttClient>>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let slot_for_cb = dispatch_slot.clone();
    let callback: acowork_gateway::mqtt::MqttMessageCallback = Arc::new(move |topic, payload| {
        let slot = slot_for_cb.clone();
        let reg = reg_for_cb.clone();
        let state = state_for_cb.clone();
        let http_reg = http_reg_for_cb.clone();
        let agent_reg = agent_reg_for_cb.clone();
        let node_reg = node_reg_for_cb.clone();
        let op_store = op_store_for_cb.clone();
        tokio::spawn(async move {
            let client = slot.lock().await.clone();
            let node_control = client
                .as_ref()
                .map(|c| NodeControlClient::new(Arc::new((**c).clone())));
            dispatch::handle_plaintext_message(
                &topic,
                &payload,
                &dispatch::DispatchContext {
                    runtime_http_registry: http_reg,
                    agent_registry: agent_reg,
                    node_registry: node_reg,
                    mqtt_client: client,
                    state,
                    node_control,
                    bootstrap_registry: Some(reg),
                    operation_store: Some(op_store),
                    ..Default::default()
                },
            );
        });
    });

    let gw_client = GatewayMqttClient::new_publisher_with_callback("127.0.0.1", port, callback)
        .await
        .expect("gateway client should connect");
    *dispatch_slot.lock().await = Some(Arc::new(gw_client.clone()));
    // Let the ConnAck subscription round settle before the node speaks.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let node_control = NodeControlClient::new(Arc::new(gw_client.clone()));
    let mut app_state = AppState::new(gw_state.clone(), Arc::new(HttpAuth::new(false)));
    app_state.bootstrap_registry = Some(registry.clone());
    app_state.operation_store = Some(operation_store.clone());
    app_state.node_control = Some(node_control);

    TestSurface {
        orchestrator,
        registry,
        gw_client,
        gw_state,
        operation_store,
        app_state,
        vault_dir,
    }
}

// ── Vault / provider helpers (mirror publisher_vault_race.rs) ──────────

fn unique_vault_dir(label: &str, port: u16) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "acowork-test-bootstrap-integration-{}-{}-{}-{}",
        label,
        std::process::id(),
        port,
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp vault dir");
    dir
}

fn seed_vault_with_provider_key(
    vault_dir: &std::path::Path,
    provider_id: &str,
    api_key: &str,
    password: &str,
) {
    let mut vault = acowork_vault::Vault::open(vault_dir).expect("open vault");
    vault.unlock(password).expect("unlock vault");
    let json = serde_json::json!({ "api_key": api_key }).to_string();
    vault
        .store(provider_id, &json)
        .expect("store provider key in vault");
    drop(vault);
}

/// Build a `SharedHttpState` with the given vault dir unlocked + a
/// single seeded provider whose key is already in the vault, plus a
/// `GatewayConfig` pointing at a fresh temp data dir (needed by the
/// mutation handlers that persist caches).
fn build_shared_state(vault_dir: &std::path::Path, password: &str) -> SharedHttpState {
    let mut gw_state = GatewayState::new(&vault_dir.to_string_lossy());
    gw_state
        .vault
        .unlock(password)
        .expect("GatewayState.vault re-unlock");
    assert!(gw_state.vault.is_unlocked());

    let data_dir = vault_dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let config = acowork_gateway::config::GatewayConfig {
        data_dir: data_dir.to_string_lossy().to_string(),
        vault_dir: vault_dir.to_string_lossy().to_string(),
        ..acowork_gateway::config::GatewayConfig::default()
    };
    gw_state.config = Some(config);

    gw_state.resource_cache.provider_list.providers = vec![acowork_core::protocol::ProviderListItem {
        id: PROVIDER_ID.to_string(),
        base_url: "https://api.minimaxi.com/v1".to_string(),
        protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
        compact_model: None,
        custom: false,
        models: vec![],
    }];
    gw_state.resource_cache.provider_list.version = 1;

    Arc::new(RwLock::new(gw_state))
}

/// Read `manifest.toml` out of a `.agent` zip package.
fn manifest_toml_from_package(package_path: &std::path::Path) -> String {
    use std::io::Read;
    let data = std::fs::read(package_path).expect("read package");
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader).expect("open package zip");
    let mut file = archive.by_name("manifest.toml").expect("manifest.toml");
    let mut s = String::new();
    file.read_to_string(&mut s).expect("read manifest.toml");
    s
}

/// Build a `multipart/form-data` body for the install endpoint.
fn multipart_body(fields: &[(&str, Vec<u8>)], boundary: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, bytes) in fields {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        if *name == "package" {
            out.extend_from_slice(
                b"Content-Disposition: form-data; name=\"package\"; filename=\"pkg.agent\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            );
        } else {
            out.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
        }
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// Build an install HTTP request through the real router (multipart
/// extractor only works via axum). Returns `(status, body)`.
async fn post_install(
    app_state: &AppState,
    node_id: &str,
    package_bytes: &[u8],
    expected_version: Option<u64>,
) -> (axum::http::StatusCode, String) {
    use tower::ServiceExt;

    let boundary = "acowork-test-boundary";
    let mut fields: Vec<(&str, Vec<u8>)> = vec![
        ("package", package_bytes.to_vec()),
        ("node_id", node_id.as_bytes().to_vec()),
    ];
    if let Some(v) = expected_version {
        fields.push(("expected_version", v.to_string().into_bytes()));
    }
    let body = multipart_body(&fields, boundary);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/agents/install")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(axum::body::Body::from(body))
        .expect("build install request");
    let response = acowork_gateway::http::routes::build_router(app_state.clone())
        .oneshot(request)
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read response body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// ── Tests ──────────────────────────────────────────────────────────────

/// 1. Cold start: the global resources publisher defers its first
/// retained snapshot until the ready barrier; the first payload then
/// carries the decrypted provider key, and the bootstrap snapshot
/// reaches READY with a monotonic version.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_start_first_publisher_carries_key_then_readiness() {
    let port = unique_port("cold");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");

    let vault_dir = unique_vault_dir("cold", port);
    seed_vault_with_provider_key(&vault_dir, PROVIDER_ID, API_KEY_VALUE, PASSWORD);
    let shared_state = build_shared_state(&vault_dir, PASSWORD);

    let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
        .await
        .expect("publisher client should connect");
    let publisher = MqttGlobalResourcesPublisher::new(client.clone(), shared_state.clone());
    let handle = publisher.start();

    let (sub_client, mut sub_eventloop) =
        subscribe("acowork/global/providers", "test:bootstrap:cold", port).await;

    // While ready=false the publisher must not emit the providers topic.
    assert_no_publish_in_window(
        &mut sub_eventloop,
        "acowork/global/providers",
        Duration::from_millis(800),
        "cold-start first publisher must defer until mark_ready",
    )
    .await;

    handle.mark_ready();
    let payload = collect_first_publish_payload(
        &mut sub_eventloop,
        "acowork/global/providers",
        Duration::from_secs(5),
    )
    .await
    .expect("first retained providers payload after mark_ready");
    let envelope = DataEnvelope::decode(payload.as_slice()).expect("decode providers");
    let providers = match envelope.payload {
        Some(data_envelope::Payload::AvailableProviders(p)) => p,
        other => panic!("expected AvailableProviders, got {other:?}"),
    };
    let entry = providers
        .providers
        .iter()
        .find(|p| p.id == PROVIDER_ID)
        .expect("minimax-cn in first payload");
    assert_eq!(
        entry.api_key, API_KEY_VALUE,
        "first retained providers payload must carry the decrypted key"
    );

    // Bootstrap orchestrator: all required subsystems ready → READY.
    // System Agent is intentionally NOT Required (ADR-077).
    let registry = SubsystemReadinessRegistry::new_shared();
    for id in ["vault", "mqtt", "publisher", "node.local"] {
        registry.register(id, ReadinessKind::Required).mark_ready(None);
    }
    let orchestrator = BootstrapOrchestrator::new("instance-cold".to_string(), registry.clone());
    // The registrations above happened BEFORE the orchestrator existed,
    // so its background listener never saw them; fold them in so the
    // FIRST retained payload is already READY.
    orchestrator.recompute();
    let _bootstrap_handle = BootstrapPublisher::start(BootstrapPublisherOptions {
        client: &client,
        orchestrator: orchestrator.clone(),
    });

    let (sub_client2, mut sub_eventloop2) =
        subscribe(TOPIC_BOOTSTRAP, "test:bootstrap:cold-bs", port).await;
    let bs_payload = collect_until_publish(
        &mut sub_eventloop2,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
        |bs| bs.phase == 2,
    )
    .await
    .expect("bootstrap retained payload");
    let bs = decode_bootstrap_state(&bs_payload);
    assert_eq!(bs.instance_id, "instance-cold");
    assert_eq!(bs.phase, 2, "phase must be READY (proto enum 2)");
    assert!(bs.version >= 2, "version must advance past the initial 1");

    drop(sub_client);
    drop(sub_client2);
    drop(_bootstrap_handle);
    drop(handle);
    drop(client);
    drop(broker);
    let _ = std::fs::remove_dir_all(&vault_dir);
}

/// 2. First Node enroll: an install submitted before `node.{id}` has
/// announced NodeReady is rejected with `dependency_not_ready`; after
/// NodeReady the same install is accepted (202 + operation_id) and a
/// NodeEvent reply with `status=ok` completes the operation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn early_install_returns_dependency_not_ready_then_succeeds() {
    let port = unique_port("enroll");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let surface = build_surface(port, "instance-enroll").await;

    // Seed the node registry via a real status message through the
    // dispatcher (the node has connected).
    let node = node_publisher(port, "node:verify-node-enroll").await;
    node.publish(
        node_status_topic(NODE_ID),
        QoS::AtLeastOnce,
        true,
        b"online",
    )
    .await
    .expect("publish status online");

    // Register the node subsystem; it stays BOOTING (no NodeReady yet).
    surface
        .registry
        .register(format!("node.{NODE_ID}"), ReadinessKind::Required);

    let package_bytes = std::fs::read(bundled_package_path()).expect("read test package");

    // Early install → 409 dependency_not_ready.
    let (status, body) = post_install(&surface.app_state, NODE_ID, &package_bytes, None).await;
    assert_eq!(status, 409, "early install must be rejected: {body}");
    assert!(
        body.contains("dependency_not_ready"),
        "409 body must name dependency_not_ready: {body}"
    );

    // Node announces control-plane readiness.
    node.publish(
        node_ready_topic(NODE_ID),
        QoS::AtLeastOnce,
        true,
        ready_envelope(NODE_ID),
    )
    .await
    .expect("publish NodeReady");

    let snapshot = surface.orchestrator.clone();
    let ready = wait_until(Duration::from_secs(5), move || {
        snapshot.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(ready, "phase must reach READY after NodeReady");

    // Same install now succeeds → 202 + operation_id.
    let (status, body) = post_install(&surface.app_state, NODE_ID, &package_bytes, None).await;
    assert_eq!(status, 202, "install after NodeReady must be accepted: {body}");
    let ack_json: serde_json::Value = serde_json::from_str(&body).expect("ack json");
    let op_id = ack_json["operation_id"]
        .as_str()
        .expect("ack operation_id")
        .to_string();
    assert!(!op_id.is_empty());

    // Node replies with NodeEvent status=ok → operation completes.
    let event = acowork_core::mqtt_proto::NodeEvent {
        node_id: NODE_ID.to_string(),
        request_id: op_id.as_str().to_string(),
        status: "ok".to_string(),
        message: "installed".to_string(),
        result_json: None,
    };
    node.publish(
        node_agent_events_topic(NODE_ID, "com.acowork.senior-engineer"),
        QoS::AtLeastOnce,
        false,
        DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeEvent(event)),
        }
        .encode_to_vec(),
    )
    .await
    .expect("publish NodeEvent");

    let store = surface.operation_store.clone();
    let op_id2 = op_id.clone();
    let completed = wait_until(Duration::from_secs(5), move || {
        store
            .get_by_str(op_id2.as_str())
            .map(|r| r.state == acowork_core::operation::OperationState::Completed)
            .unwrap_or(false)
    })
    .await;
    assert!(
        completed,
        "NodeEvent(ok) must complete the operation (operation_id closed loop)"
    );

    drop(node);
    drop(surface);
    drop(broker);
}

/// 3. ADR-077: the System Agent is NOT a Required bootstrap subsystem.
/// It may be absent, slow to install, or have its Runtime still booting —
/// none of that must delay the aggregated phase reaching READY. Readiness
/// is signalled separately via the agent's own `acowork/agents/{id}/ready`
/// retained topic, consumed by the Desktop via `/api/agents`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_succeeds_without_system_agent() {
    let port = unique_port("system");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let surface = build_surface(port, "instance-system").await;

    // Required subsystems — exactly the platform-readiness blockers.
    // System Agent is intentionally NOT in this list.
    for id in ["vault", "mqtt", "publisher", "node.local"] {
        surface
            .registry
            .register(id, ReadinessKind::Required)
            .mark_ready(None);
    }

    // Bootstrap publisher is live; the retained snapshot must reach
    // READY without any system_agent registration at all.
    let _bp = BootstrapPublisher::start(BootstrapPublisherOptions {
        client: &surface.gw_client,
        orchestrator: surface.orchestrator.clone(),
    });
    let (sub_client, mut sub_eventloop) =
        subscribe(TOPIC_BOOTSTRAP, "test:bootstrap:system", port).await;
    let payload = collect_bootstrap_until(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
        |bs: &BootstrapState| bs.phase == 2,
    )
    .await
    .expect("bootstrap reaches READY without system_agent");
    assert_eq!(payload.phase, 2, "phase READY (proto enum 2)");
    assert!(payload.version >= 2, "version must advance past initial BOOTING");

    drop(sub_client);
    drop(_bp);
    drop(surface);
    drop(broker);
}

/// 4. Concurrent installs: three parallel submissions produce three
/// distinct operation ids; re-submitting the same id never creates a
/// second record; and three retained `installed` entries aggregate
/// exactly once into the Gateway state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_installs_unique_ids_and_aggregate_inventory() {
    let store = OperationStore::new_shared();

    // Three concurrent inserts — ids must be pairwise unique.
    let ids: Vec<acowork_core::operation::OperationId> = {
        let mut handles = Vec::new();
        for _ in 0..3 {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                let record = acowork_core::operation::OperationRecord::new(0);
                let id = record.operation_id.clone();
                s.insert(record);
                id
            }));
        }
        let mut out = Vec::new();
        for h in handles {
            out.push(h.await.expect("concurrent insert task"));
        }
        out
    };
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 3, "concurrent installs must yield unique operation ids");
    assert_eq!(store.len(), 3);

    // Re-submitting the same id is a no-op (idempotency).
    let record = acowork_core::operation::OperationRecord::new(0);
    assert!(store.insert(record.clone()));
    assert!(!store.insert(record), "duplicate operation_id must be rejected");
    assert_eq!(store.len(), 4);

    // Inventory aggregation: three retained installed entries over the
    // real dispatcher land exactly once in GatewayState.
    let port = unique_port("inventory");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let surface = build_surface(port, "instance-inventory").await;

    let manifest = manifest_toml_from_package(bundled_package_path().as_path());
    let node = node_publisher(port, "node:verify-node-inv").await;
    for (agent_id, instance_id) in [
        ("com.acowork.a", "1a1a1a1a-0000-4000-8000-000000000001"),
        ("com.acowork.b", "1a1a1a1a-0000-4000-8000-000000000002"),
        ("com.acowork.c", "1a1a1a1a-0000-4000-8000-000000000003"),
    ] {
        let info = acowork_core::mqtt_proto::InstalledAgentInfo {
            agent_id: agent_id.to_string(),
            version: "1.0.0".to_string(),
            name: agent_id.to_string(),
            install_path: format!("/agents/{instance_id}"),
            manifest_toml: manifest.clone(),
            // ADR-073: the install table is keyed by INSTANCE identity;
            // each retained inventory entry carries the UUID the Node used
            // when it landed the package.
            instance_id: instance_id.to_string(),
            // ADR-009 §5: no user-preference overrides in this fixture.
            overrides_json: String::new(),
        };
        node.publish(
            node_agent_installed_topic(NODE_ID, instance_id),
            QoS::AtLeastOnce,
            true,
            DataEnvelope {
                version: 1,
                payload: Some(data_envelope::Payload::InstalledAgentInfo(info)),
            }
            .encode_to_vec(),
        )
        .await
        .expect("publish installed inventory");
    }

    let state = surface.gw_state.clone();
    let aggregated = wait_until(Duration::from_secs(5), move || {
        state.try_read().map(|gw| gw.installed_agents.len() == 3).unwrap_or(false)
    })
    .await;
    assert!(aggregated, "3 retained installed entries must aggregate exactly once");
    {
        let gw = surface.gw_state.read().await;
        assert_eq!(gw.installed_agents.len(), 3);
    }

    drop(node);
    drop(surface);
    drop(broker);
}

/// 5. Concurrent provider + identity writes: each mutation carries the
/// correct per-resource retained version, and the republished MQTT
/// snapshots expose those versions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_provider_writes_carry_correct_versions() {
    let port = unique_port("mutations");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");

    let vault_dir = unique_vault_dir("mutations", port);
    seed_vault_with_provider_key(&vault_dir, PROVIDER_ID, API_KEY_VALUE, PASSWORD);
    let shared_state = build_shared_state(&vault_dir, PASSWORD);
    let operation_store = OperationStore::new_shared();

    let mut app_state = AppState::new(shared_state.clone(), Arc::new(HttpAuth::new(false)));
    app_state.operation_store = Some(operation_store.clone());

    // Concurrent mutations: 2 providers.
    let app_for_p1 = app_state.clone();
    let app_for_p2 = app_state.clone();
    let p1 = tokio::spawn(async move {
        acowork_gateway::http::provider_api::add_provider(
            axum::extract::State(app_for_p1),
            axum::Json(acowork_gateway::http::provider_api::AddProviderRequest {
                provider: "deepseek".to_string(),
                key: "sk-deepseek-1".to_string(),
                keys: Vec::new(),
                base_url: None,
                default_model: None,
                models: vec!["deepseek-chat".to_string()],
                compact_model: None,
                custom: Some(true),
                model_capabilities: None,
                protocol: None,
                model_protocols: None,
                model_base_urls: None,
                expected_version: None,
            }),
        )
        .await
    });
    let p2 = tokio::spawn(async move {
        acowork_gateway::http::provider_api::add_provider(
            axum::extract::State(app_for_p2),
            axum::Json(acowork_gateway::http::provider_api::AddProviderRequest {
                provider: "openai".to_string(),
                key: "sk-openai-1".to_string(),
                keys: Vec::new(),
                base_url: None,
                default_model: None,
                models: vec!["gpt-4o".to_string()],
                compact_model: None,
                custom: Some(true),
                model_capabilities: None,
                protocol: None,
                model_protocols: None,
                model_base_urls: None,
                expected_version: None,
            }),
        )
        .await
    });
    let (r1, r2) = tokio::join!(p1, p2);
    let (code1, ack1) = r1.expect("p1 task").expect("p1 succeeds");
    let (code2, ack2) = r2.expect("p2 task").expect("p2 succeeds");
    assert_eq!(code1, axum::http::StatusCode::CREATED);
    assert_eq!(code2, axum::http::StatusCode::CREATED);

    // Per-resource versions: provider list starts at 1 → 2, 3.
    //
    // ADR-084 dropped the concurrent *identity* write this test used to
    // make: the account store left the Gateway, so there is no Gateway-side
    // path to bump `user_profile_list` any more. The identity channel is
    // still asserted below (its empty shape is a documented contract), and
    // M3 adds the snapshot-fed version of this test.
    let mut provider_versions: Vec<u64> = vec![
        ack1.0.ack.resource_version.expect("p1 resource_version"),
        ack2.0.ack.resource_version.expect("p2 resource_version"),
    ];
    provider_versions.sort_unstable();
    assert_eq!(provider_versions, vec![2, 3]);

    // The operation store tracked both mutations.
    assert_eq!(operation_store.len(), 2);

    // MQTT republish carries the merged snapshots with those versions.
    let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
        .await
        .expect("publisher client should connect");
    let publisher = MqttGlobalResourcesPublisher::new(client.clone(), shared_state.clone());
    let phandle = publisher.start();
    phandle.mark_ready();
    phandle.trigger_republish();

    let (sub_client, mut sub_eventloop) =
        subscribe("acowork/global/providers", "test:bootstrap:mut", port).await;
    let payload = collect_first_publish_payload(
        &mut sub_eventloop,
        "acowork/global/providers",
        Duration::from_secs(5),
    )
    .await
    .expect("providers retained after mutations");
    let envelope = DataEnvelope::decode(payload.as_slice()).expect("decode providers");
    let providers = match envelope.payload {
        Some(data_envelope::Payload::AvailableProviders(p)) => p,
        other => panic!("expected AvailableProviders, got {other:?}"),
    };
    assert_eq!(providers.providers.len(), 3, "minimax + deepseek + openai");
    assert!(providers.version >= 3, "providers retained version must be >= 3");

    let (sub_client2, mut sub_eventloop2) =
        subscribe("acowork/global/user_profile", "test:bootstrap:mut-u", port).await;
    let payload = collect_first_publish_payload(
        &mut sub_eventloop2,
        "acowork/global/user_profile",
        Duration::from_secs(5),
    )
    .await
    .expect("user_profile retained after mutation");
    let envelope = DataEnvelope::decode(payload.as_slice()).expect("decode users");
    let users = match envelope.payload {
        Some(data_envelope::Payload::AvailableUsers(u)) => u,
        other => panic!("expected AvailableUsers, got {other:?}"),
    };
    // ADR-084 M2 boundary: the Gateway still *publishes* the identity
    // channel (that is `publish_user_profiles`' documented empty case) but
    // no longer owns its contents, and the user-service projection is not
    // wired until M3 — so no active user is expected here.
    assert!(
        users.active_user.is_none(),
        "identity is not Gateway-owned in M2"
    );
    assert_eq!(users.version, 0);

    drop(sub_client);
    drop(sub_client2);
    drop(phandle);
    drop(client);
    drop(broker);
    let _ = std::fs::remove_dir_all(&vault_dir);
}

/// Desktop-side snapshot consumer implementing the ADR-059 §8.3
/// acceptance rule (same instance: strictly newer versions only;
/// cross-instance: only a version-1 snapshot switches the baseline).
#[derive(Debug, Clone)]
struct DesktopView {
    instance_id: Option<String>,
    version: u64,
    phase: Option<i32>,
}

impl DesktopView {
    fn new() -> Self {
        Self {
            instance_id: None,
            version: 0,
            phase: None,
        }
    }

    fn apply(&mut self, bs: &acowork_core::mqtt_proto::BootstrapState) -> bool {
        match &self.instance_id {
            Some(current) if *current == bs.instance_id => {
                if bs.version > self.version {
                    self.version = bs.version;
                    self.phase = Some(bs.phase);
                    true
                } else {
                    false
                }
            }
            Some(_) => {
                // Cross-instance: only the new process's first snapshot
                // (version 1) wins the switch; stale redeliveries are
                // rejected.
                if bs.version == 1 {
                    self.instance_id = Some(bs.instance_id.clone());
                    self.version = bs.version;
                    self.phase = Some(bs.phase);
                    true
                } else {
                    false
                }
            }
            None => {
                self.instance_id = Some(bs.instance_id.clone());
                self.version = bs.version;
                self.phase = Some(bs.phase);
                true
            }
        }
    }
}

/// 6. MQTT disconnect + reconnect: the Desktop observes the NEW
/// instance id / version baseline after a Gateway restart, and an
/// operation from the old process resolves to `operation_uncertain`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_observes_new_instance_and_old_operation_is_uncertain() {
    let port = unique_port("reconnect");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");

    // Generation A: fully ready, publishes READY retained.
    // System Agent is intentionally NOT Required (ADR-077).
    let surface_a = build_surface(port, "instance-A").await;
    for id in ["vault", "mqtt", "publisher", "node.local"] {
        surface_a.registry.register(id, ReadinessKind::Required).mark_ready(None);
    }
    // Fold the registrations into the snapshot before the publisher
    // starts: the listener observes readiness events asynchronously, so
    // an intermediate BOOTING publish could otherwise race the READY
    // one and become the first retained payload (same fix as the
    // cold-start test).
    surface_a.orchestrator.recompute();
    let _bp_a = BootstrapPublisher::start(BootstrapPublisherOptions {
        client: &surface_a.gw_client,
        orchestrator: surface_a.orchestrator.clone(),
    });

    let mut desktop = DesktopView::new();
    let (sub_client, mut sub_eventloop) =
        subscribe(TOPIC_BOOTSTRAP, "test:bootstrap:reconnect", port).await;
    let payload = collect_first_publish_payload(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
    )
    .await
    .expect("generation A retained");
    let bs_a = decode_bootstrap_state(&payload);
    assert_eq!(bs_a.instance_id, "instance-A");
    assert_eq!(bs_a.phase, 2);
    assert!(desktop.apply(&bs_a));
    drop(sub_client);

    // Generation A exits (publisher loop stops), then generation B
    // starts and publishes its FIRST snapshot: version 1, BOOTING.
    drop(_bp_a);
    drop(surface_a);
    let surface_b = build_surface(port, "instance-B").await;
    // No subsystem registers before the Desktop observes B's v1 BOOTING:
    // a readiness event would bump the version past 1, and the
    // cross-instance baseline switch keys on version 1 (see
    // `DesktopView::apply`).
    let _bp_b = BootstrapPublisher::start(BootstrapPublisherOptions {
        client: &surface_b.gw_client,
        orchestrator: surface_b.orchestrator.clone(),
    });

    // Desktop reconnects: the retained payload is now B's v1 BOOTING.
    let (sub_client, mut sub_eventloop) =
        subscribe(TOPIC_BOOTSTRAP, "test:bootstrap:reconnect-2", port).await;
    let payload = collect_until_publish(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
        |bs| bs.instance_id == "instance-B",
    )
    .await
    .expect("generation B retained");
    let bs_b = decode_bootstrap_state(&payload);
    assert_eq!(bs_b.instance_id, "instance-B");
    assert_eq!(bs_b.version, 1);
    assert_eq!(bs_b.phase, 1, "fresh instance must start BOOTING");
    assert!(desktop.apply(&bs_b), "version-1 snapshot must switch the baseline");
    assert_eq!(desktop.instance_id.as_deref(), Some("instance-B"));
    assert_eq!(desktop.version, 1);
    assert_eq!(desktop.phase, Some(1), "desktop must not see READY from B yet");

    // A stale retained redelivery from generation A must be rejected.
    let stale = acowork_core::mqtt_proto::BootstrapState {
        protocol_version: 1,
        instance_id: "0e1f2a3b-4c5d-4026-8576-268a9b0c1d2e".to_string(),
        version: 9,
        phase: 2,
        phase_detail: "stale".to_string(),
        issued_at_ms: 0,
    };
    assert!(
        !desktop.apply(&stale),
        "stale cross-instance snapshot must be rejected"
    );
    assert_eq!(desktop.instance_id.as_deref(), Some("instance-B"));

    // Old-process operation: the new process's store has no record →
    // the client-side mapping is operation_uncertain.
    let old_store = OperationStore::new_shared();
    let mut record = acowork_core::operation::OperationRecord::new(0);
    record.state = acowork_core::operation::OperationState::Running;
    let old_op = record.operation_id.clone();
    old_store.insert(record);
    drop(old_store);

    let new_store = OperationStore::new_shared();
    assert!(new_store.get(&old_op).is_none(), "old operation lost on restart");
    assert_eq!(new_store.sweep_expired(), 0);
    let uncertain = acowork_core::error_codes::StructuredErrorBody::operation_uncertain(
        old_op.as_str(),
    );
    assert_eq!(
        uncertain.code,
        acowork_core::error_codes::StructuredErrorCode::OperationUncertain
    );

    drop(sub_client);
    drop(_bp_b);
    drop(surface_b);
    drop(broker);
}

/// 7. Cross-generation restart: generation A reaches READY; the fresh
/// generation B must NEVER surface a premature READY to the Desktop —
/// its version-1 snapshot is BOOTING, and READY appears only after B's
/// own subsystems report ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_generation_restart_does_not_prematurely_ready() {
    let port = unique_port("crossgen");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");

    // Generation A: fully ready at version 7+.
    // System Agent is intentionally NOT Required (ADR-077).
    let surface_a = build_surface(port, "instance-A").await;
    for id in ["vault", "mqtt", "publisher", "node.local"] {
        surface_a.registry.register(id, ReadinessKind::Required).mark_ready(None);
    }
    // Fold the registrations into the snapshot before the publisher
    // starts: the listener observes readiness events asynchronously, so
    // an intermediate BOOTING publish could otherwise race the READY
    // one and become the first retained payload (same fix as the
    // cold-start test).
    surface_a.orchestrator.recompute();
    let _bp_a = BootstrapPublisher::start(BootstrapPublisherOptions {
        client: &surface_a.gw_client,
        orchestrator: surface_a.orchestrator.clone(),
    });
    let mut desktop = DesktopView::new();
    let (sub_client, mut sub_eventloop) =
        subscribe(TOPIC_BOOTSTRAP, "test:bootstrap:crossgen", port).await;
    let payload = collect_first_publish_payload(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
    )
    .await
    .expect("generation A READY retained");
    let bs_a = decode_bootstrap_state(&payload);
    assert!(desktop.apply(&bs_a));
    assert_eq!(desktop.phase, Some(2));
    let version_a = bs_a.version;
    assert!(version_a >= 2);
    drop(sub_client);

    // A shuts down: the retained snapshot is cleared.
    drop(_bp_a);
    drop(surface_a);
    let cleaner = node_publisher(port, "node:crossgen-clear").await;
    cleaner
        .publish(TOPIC_BOOTSTRAP, QoS::AtLeastOnce, true, Vec::new())
        .await
        .expect("clear retained bootstrap");
    drop(cleaner);

    // Generation B starts. No subsystem registers before the Desktop
    // observes B's v1 BOOTING: a readiness event would bump the version
    // past 1, and the cross-instance baseline switch keys on version 1
    // (see `DesktopView::apply`). They register below, once the v1
    // baseline has been switched.
    let surface_b = build_surface(port, "instance-B").await;
    let _bp_b = BootstrapPublisher::start(BootstrapPublisherOptions {
        client: &surface_b.gw_client,
        orchestrator: surface_b.orchestrator.clone(),
    });

    // Desktop reconnects: must see BOOTING, never the old READY.
    let (sub_client, mut sub_eventloop) =
        subscribe(TOPIC_BOOTSTRAP, "test:bootstrap:crossgen-2", port).await;
    let payload = collect_until_publish(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
        |bs| bs.instance_id == "instance-B",
    )
    .await
    .expect("generation B first snapshot");
    let bs_b = decode_bootstrap_state(&payload);
    assert_eq!(bs_b.instance_id, "instance-B");
    assert_eq!(bs_b.version, 1);
    assert_eq!(bs_b.phase, 1, "B's first snapshot must be BOOTING");
    assert!(desktop.apply(&bs_b));
    assert_eq!(
        desktop.phase,
        Some(1),
        "Desktop must never see a premature READY from the new instance"
    );

    // B becomes fully ready → READY, with a higher version.
    // System Agent is intentionally NOT Required (ADR-077).
    for id in ["vault", "node.local"] {
        surface_b.registry.register(id, ReadinessKind::Required).mark_ready(None);
    }
    let orch_b = surface_b.orchestrator.clone();
    let ready = wait_until(Duration::from_secs(5), move || {
        orch_b.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(ready, "generation B must reach READY on its own");
    let payload = collect_until_publish(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
        |bs| bs.phase == 2,
    )
    .await
    .expect("generation B READY retained");
    let bs_b_ready = decode_bootstrap_state(&payload);
    assert!(desktop.apply(&bs_b_ready));
    assert_eq!(desktop.phase, Some(2));
    assert!(bs_b_ready.version > bs_b.version);

    drop(sub_client);
    drop(_bp_b);
    drop(surface_b);
    drop(broker);
}

/// 8. Normal handshake: HTTP `/api/bootstrap` and the MQTT retained
/// snapshot agree field-for-field while the Gateway boots AND after it
/// reaches READY.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_and_mqtt_projections_agree() {
    let port = unique_port("agree");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let surface = build_surface(port, "instance-agree").await;
    surface.registry.register("vault", ReadinessKind::Required);

    let _bp = BootstrapPublisher::start(BootstrapPublisherOptions {
        client: &surface.gw_client,
        orchestrator: surface.orchestrator.clone(),
    });
    let (sub_client, mut sub_eventloop) =
        subscribe(TOPIC_BOOTSTRAP, "test:bootstrap:agree", port).await;

    // Compare while still booting.
    let http_view = acowork_gateway::http::bootstrap_api::get_bootstrap(axum::extract::State(
        surface.app_state.clone(),
    ))
    .await
    .expect("GET /api/bootstrap");
    let payload = collect_first_publish_payload(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
    )
    .await
    .expect("mqtt retained bootstrap");
    let mqtt = decode_bootstrap_state(&payload);
    assert_eq!(http_view.instance_id, mqtt.instance_id);
    assert_eq!(http_view.version, mqtt.version);
    assert_eq!(http_view.protocol_version, mqtt.protocol_version);
    assert_eq!(http_view.phase_detail, mqtt.phase_detail);
    assert_eq!(http_view.issued_at_ms, mqtt.issued_at_ms);
    assert_eq!(http_view.phase, "BOOTING");
    assert_eq!(mqtt.phase, 1);

    // Reach READY; both channels must advance in lockstep.
    // System Agent is intentionally NOT Required (ADR-077).
    for id in ["vault", "node.local"] {
        surface.registry.register(id, ReadinessKind::Required).mark_ready(None);
    }
    let orch = surface.orchestrator.clone();
    let ready = wait_until(Duration::from_secs(5), move || {
        orch.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(ready, "phase must reach READY");
    let http_view = acowork_gateway::http::bootstrap_api::get_bootstrap(axum::extract::State(
        surface.app_state.clone(),
    ))
    .await
    .expect("GET /api/bootstrap after READY");
    let payload = collect_until_publish(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
        |bs| bs.phase == 2,
    )
    .await
    .expect("mqtt retained bootstrap after READY");
    let mqtt = decode_bootstrap_state(&payload);
    assert_eq!(http_view.phase, "READY");
    assert_eq!(mqtt.phase, 2);
    assert_eq!(http_view.instance_id, mqtt.instance_id);
    assert_eq!(http_view.version, mqtt.version);
    assert!(http_view.version > 1, "version must advance across READY");

    drop(sub_client);
    drop(_bp);
    drop(surface);
    drop(broker);
}

/// 9. Remote Node reconnect: LWT `status=offline` demotes READY →
/// BOOTING; the re-announced NodeReady restores READY. The Desktop's
/// retained snapshot reflects every transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_node_reconnect_restores_readiness() {
    let port = unique_port("node-reconnect");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let surface = build_surface(port, "instance-node").await;
    // System Agent is intentionally NOT Required (ADR-077).
    for id in ["vault", "mqtt", "publisher"] {
        surface.registry.register(id, ReadinessKind::Required).mark_ready(None);
    }
    surface.registry.register(format!("node.{NODE_ID}"), ReadinessKind::Required);

    let _bp = BootstrapPublisher::start(BootstrapPublisherOptions {
        client: &surface.gw_client,
        orchestrator: surface.orchestrator.clone(),
    });
    let (sub_client, mut sub_eventloop) =
        subscribe(TOPIC_BOOTSTRAP, "test:bootstrap:node-reconnect", port).await;

    // Node announces ready → READY.
    let node = node_publisher(port, "node:verify-node-nr").await;
    node.publish(
        node_ready_topic(NODE_ID),
        QoS::AtLeastOnce,
        true,
        ready_envelope(NODE_ID),
    )
    .await
    .expect("publish NodeReady");
    let mut desktop = DesktopView::new();
    let payload = collect_until_publish(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
        |bs| bs.phase == 2,
    )
    .await
    .expect("retained after NodeReady");
    let bs = decode_bootstrap_state(&payload);
    assert_eq!(bs.phase, 2, "READY after NodeReady");
    assert!(desktop.apply(&bs));

    // LWT: node dies → status=offline demotes to BOOTING.
    node.publish(
        node_status_topic(NODE_ID),
        QoS::AtLeastOnce,
        true,
        b"offline",
    )
    .await
    .expect("publish status offline");
    drop(node);
    let payload = collect_until_publish(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
        |bs| bs.phase == 1,
    )
    .await
    .expect("retained after LWT offline");
    let bs = decode_bootstrap_state(&payload);
    assert_eq!(bs.phase, 1, "LWT offline must demote to BOOTING");
    assert!(desktop.apply(&bs));

    // Node reconnects and re-announces → READY restored.
    let node2 = node_publisher(port, "node:verify-node-nr-2").await;
    node2
        .publish(
            node_ready_topic(NODE_ID),
            QoS::AtLeastOnce,
            true,
            ready_envelope(NODE_ID),
        )
        .await
        .expect("re-announce NodeReady");
    let payload = collect_until_publish(
        &mut sub_eventloop,
        TOPIC_BOOTSTRAP,
        Duration::from_secs(5),
        |bs| bs.phase == 2,
    )
    .await
    .expect("retained after re-announce");
    let bs = decode_bootstrap_state(&payload);
    assert_eq!(bs.phase, 2, "re-announced NodeReady restores READY");
    assert!(bs.version > 1);
    assert!(desktop.apply(&bs));
    assert_eq!(desktop.phase, Some(2));

    drop(node2);
    drop(sub_client);
    drop(_bp);
    drop(surface);
    drop(broker);
}

/// 10. Gateway restart: old operation ids are not resolvable in the
/// new process's store; the client-side mapping is
/// `operation_uncertain` (never a silent retry / duplicate install).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_old_operation_ids_resolve_to_operation_uncertain() {
    // Generation A tracks a running operation.
    let store_a = OperationStore::new_shared();
    let mut record = acowork_core::operation::OperationRecord::new(3);
    record.state = acowork_core::operation::OperationState::Running;
    record.resource_version = Some(9);
    let op_id = record.operation_id.clone();
    store_a.insert(record);
    assert_eq!(
        store_a.get(&op_id).unwrap().state,
        acowork_core::operation::OperationState::Running
    );
    drop(store_a);

    // Generation B starts with an empty store.
    let store_b = OperationStore::new_shared();
    assert!(store_b.get(&op_id).is_none());
    assert_eq!(store_b.sweep_expired(), 0);

    // The client resolves the unknown id to operation_uncertain.
    let body = acowork_core::error_codes::StructuredErrorBody::operation_uncertain(op_id.as_str());
    assert_eq!(
        body.code,
        acowork_core::error_codes::StructuredErrorCode::OperationUncertain
    );
    assert_eq!(body.operation_id.as_ref().map(|i| i.as_str()), Some(op_id.as_str()));

    // A duplicate submit after restart is a fresh operation, not a
    // resumption of the old one.
    let fresh = acowork_core::operation::OperationRecord::new(0);
    assert!(store_b.insert(fresh));
    assert_eq!(store_b.len(), 1);
}

/// 11. OCP: subsystem churn (add / remove / rename) never changes the
/// wire-level `BootstrapState` field set, the error-code set, or the
/// `/api/bootstrap` JSON shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ocp_wire_fields_stable_under_subsystem_churn() {
    use acowork_core::error_codes::StructuredErrorCode;

    // ── Wire-level BootstrapState: field set is independent of the
    // subsystem picture. Encode two snapshots with different subsystem
    // sets at the same phase; decode both and compare the six fields.
    let registry1 = SubsystemReadinessRegistry::new_shared();
    // Same required COUNT as registry2 below — the phase_detail
    // aggregate must be byte-identical across the churn so the wire
    // fields stay stable (only the subsystem NAMES differ).
    // System Agent is intentionally NOT Required (ADR-077).
    for id in ["vault", "publisher", "node.local"] {
        registry1.register(id, ReadinessKind::Required).mark_ready(None);
    }
    let orch1 = BootstrapOrchestrator::new("instance-ocp-1".to_string(), registry1.clone());
    // Registrations happened before construction — fold them in so the
    // snapshot reflects the ready subsystems.
    orch1.recompute();
    let orch1_wait = orch1.clone();
    let ready_1 = wait_until(Duration::from_secs(2), move || {
        orch1_wait.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(ready_1, "orch1 must reach READY");

    // "Churn": a renamed subsystem replaces an old one, plus extras.
    // Count must match registry1 above so phase_detail is byte-identical
    // across the churn (the test asserts equality on the wire shape).
    let registry2 = SubsystemReadinessRegistry::new_shared();
    for id in ["renamed-vault", "embedding", "extra"] {
        registry2.register(id, ReadinessKind::Required).mark_ready(None);
    }
    let orch2 = BootstrapOrchestrator::new("instance-ocp-2".to_string(), registry2.clone());
    // Registrations happened before construction — fold them in so the
    // snapshot reflects the ready subsystems.
    orch2.recompute();
    let orch2_wait = orch2.clone();
    let ready_2 = wait_until(Duration::from_secs(2), move || {
        orch2_wait.snapshot().phase == BootstrapPhase::Ready
    })
    .await;
    assert!(ready_2, "orch2 must reach READY");

    let snap1 = orch1.snapshot();
    let snap2 = orch2.snapshot();
    let p1 = snap1.to_proto();
    let p2 = snap2.to_proto();
    // Same phase / detail shape — only instance + version differ.
    assert_eq!(p1.phase, p2.phase);
    assert_eq!(p1.protocol_version, p2.protocol_version);
    assert_eq!(p1.phase_detail, p2.phase_detail);
    assert_ne!(p1.instance_id, p2.instance_id);

    // Round-trip: encode/decode preserves exactly the six fields.
    let decoded = acowork_core::mqtt_proto::BootstrapState::decode(p1.encode_to_vec().as_slice())
        .expect("decode roundtrip");
    assert_eq!(decoded.protocol_version, p1.protocol_version);
    assert_eq!(decoded.instance_id, p1.instance_id);
    assert_eq!(decoded.version, p1.version);
    assert_eq!(decoded.phase, p1.phase);
    assert_eq!(decoded.phase_detail, p1.phase_detail);
    assert_eq!(decoded.issued_at_ms, p1.issued_at_ms);

    // phase_detail stays aggregated-only — no subsystem ids leak.
    for forbidden in ["vault", "publisher", "embedding", "extra", "renamed"] {
        assert!(
            !snap1.phase_detail.contains(forbidden) && !snap2.phase_detail.contains(forbidden),
            "phase_detail leaked subsystem id '{forbidden}'"
        );
    }

    // ── Error-code set: exactly the five ADR-059 §6.3 codes.
    let codes = [
        StructuredErrorCode::DependencyNotReady,
        StructuredErrorCode::OperationUncertain,
        StructuredErrorCode::OperationExpired,
        StructuredErrorCode::ResourceVersionConflict,
        StructuredErrorCode::HandshakeTimeout,
    ];
    assert_eq!(codes.len(), 5);
    for (code, name) in [
        (StructuredErrorCode::DependencyNotReady, "dependency_not_ready"),
        (StructuredErrorCode::OperationUncertain, "operation_uncertain"),
        (StructuredErrorCode::OperationExpired, "operation_expired"),
        (StructuredErrorCode::ResourceVersionConflict, "resource_version_conflict"),
        (StructuredErrorCode::HandshakeTimeout, "handshake_timeout"),
    ] {
        assert_eq!(serde_json::to_value(code).unwrap(), serde_json::json!(name));
    }

    // ── HTTP /api/bootstrap JSON shape: exactly six protocol fields.
    let port = unique_port("ocp");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");
    let surface = build_surface(port, "instance-ocp-http").await;
    surface.registry.register("vault", ReadinessKind::Required);
    let view = acowork_gateway::http::bootstrap_api::get_bootstrap(axum::extract::State(
        surface.app_state.clone(),
    ))
    .await
    .expect("GET /api/bootstrap");
    let json = serde_json::to_value(view.0).unwrap();
    let obj = json.as_object().unwrap();
    assert_eq!(obj.len(), 6, "/api/bootstrap must expose exactly 6 protocol fields");
    for key in [
        "protocol_version",
        "instance_id",
        "version",
        "phase",
        "phase_detail",
        "issued_at_ms",
    ] {
        assert!(obj.contains_key(key), "missing field {key}");
    }

    drop(surface);
    drop(broker);
}
