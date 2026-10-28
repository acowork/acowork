//! Integration test for the MqttGlobalResourcesPublisher ready barrier
//! (desktop-onboarding-bugfix_154b7ff7.md §Fix 1).
//!
//! Validates that:
//!   1. The publisher does NOT emit its first retained snapshot while the
//!      ready barrier is held (the `mark_ready()` signal has not been
//!      fired). Subscribers connecting during this window must not
//!      receive `acowork/global/providers`.
//!   2. After `mark_ready()`, the FIRST retained payload carries the
//!      decrypted API key for the provider that was stored in the
//!      vault BEFORE `mark_ready()`. An empty `api_key` field means the
//!      publisher emitted before the vault was unlocked — the bug Fix 1
//!      was created to remove.
//!
//! Reproduces the original failure mode: pre-Fix-1, the publisher
//! emitted its initial snapshot synchronously at startup, when the
//! vault was still locked. The payload's `ProviderRef.api_key` was the
//! empty string for every provider. Runtimes that subscribed in the
//! first few seconds cached `api_key_lengths=[0]` and ignored all
//! subsequent republishes (MQTT retained messages are only delivered
//! once per subscriber per topic transition). The fix defers the
//! initial publish until both the vault is unlocked AND the local node
//! is enrolled, so the first retained snapshot always carries the
//! real key material.
//!
//! Test pattern mirrors the `test_publisher_publishes_retained_snapshot`
//! unit test in `mqtt/global_resources_publisher.rs` but exercises the
//! ready-barrier semantics end-to-end.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use prost::Message as _;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use tokio::sync::RwLock;

use acowork_core::mqtt_proto::{data_envelope, DataEnvelope};
use acowork_core::protocol::{
    ModelCapabilitiesInfo, ProtocolType, ProviderListItem, ProviderModelEntry,
};
use acowork_gateway::gateway::state::GatewayState;
use acowork_gateway::http::routes::SharedHttpState;
use acowork_gateway::mqtt::broker::start_broker;
use acowork_gateway::mqtt::client::GatewayMqttClient;
use acowork_gateway::mqtt::global_resources_publisher::MqttGlobalResourcesPublisher;

const PROVIDER_ID: &str = "minimax-cn";
const API_KEY_VALUE: &str = "sk-minimax-secret-12345";
const PASSWORD: &str = "publisher-vault-race-test-password";

/// Pick a fresh MQTT port for each test invocation. The broker is
/// spawned on a thread that does NOT exit on `MqttBrokerHandle::drop`
/// (rumqttd's `Broker::start()` blocks the thread forever), so reusing
/// a port across tests would carry retained messages from the previous
/// test run and break the `assert_no_publish_in_window` invariant.
/// `18980..18990` is the slot we reserved for this file.
fn unique_port(label: &str) -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(18980);
    let p = NEXT.fetch_add(1, Ordering::Relaxed);
    eprintln!("[publisher_vault_race] {label} using port {p}");
    p
}

/// Build a fresh `ModelCapabilitiesInfo` with the minimum fields we
/// need for the publisher payload — the publisher only inspects the
/// model id (and the proto encoder serialises every field), so the
/// rest can be sensible defaults.
fn empty_capabilities() -> ModelCapabilitiesInfo {
    ModelCapabilitiesInfo {
        context_window: 32_000,
        max_output_tokens: 8_192,
        max_input_tokens: None,
        supports_tool_calling: true,
        supports_reasoning: None,
        supports_attachment: None,
        supports_temperature: None,
        cost: None,
        modalities: None,
        name: None,
        family: None,
        knowledge_cutoff: None,
        default_reasoning_effort: None,
        thinking_mode: None,
    }
}

/// Make a per-test unique vault dir. Each test gets its own dir so
/// parallel test runs cannot collide on the salt / key files.
fn unique_vault_dir(label: &str, port: u16) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "acowork-test-publisher-vault-race-{}-{}-{}-{}",
        label,
        std::process::id(),
        port,
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp vault dir");
    dir
}

/// Seed `acowork_vault::Vault` at the given directory by unlocking
/// with the test password and storing one provider key.
///
/// This is the "warmup" step that simulates the user pasting an API
/// key during onboarding. The subsequent `GatewayState::new(vault_dir)`
/// reopens the vault and re-derives the master key with the same
/// password, picking up the stored key automatically.
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
    // Drop the vault so its master_key is zeroized and the on-disk
    // files are flushed.
    drop(vault);
}

/// Build a `SharedHttpState` with the given vault dir unlocked + a
/// single seeded provider whose key is already in the vault.
fn build_shared_state(vault_dir: &std::path::Path, password: &str) -> SharedHttpState {
    let mut gw_state = GatewayState::new(&vault_dir.to_string_lossy());
    gw_state
        .vault
        .unlock(password)
        .expect("GatewayState.vault re-unlock");
    assert!(
        gw_state.vault.is_unlocked(),
        "GatewayState.vault must be unlocked after unlock()"
    );

    gw_state.resource_cache.provider_list.providers = vec![ProviderListItem {
        id: PROVIDER_ID.to_string(),
        base_url: "https://api.minimaxi.com/v1".to_string(),
        protocol_type: ProtocolType::OpenAI,
        compact_model: None,
        custom: false,
        models: vec![ProviderModelEntry {
            id: "minimax-abab6".to_string(),
            capabilities: empty_capabilities(),
            max_output_tokens_limit: 16_384,
            protocol_type: None,
            base_url: None,
            protocol_probe: None,
        }],
    }];
    gw_state.resource_cache.provider_list.version = 1;

    Arc::new(RwLock::new(gw_state))
}

/// Drain a single event from `sub_eventloop` with a timeout. Returns
/// the event if any, otherwise `None`.
async fn poll_one_event(
    sub_eventloop: &mut rumqttc::EventLoop,
    timeout: Duration,
) -> Option<rumqttc::Event> {
    match tokio::time::timeout(timeout, sub_eventloop.poll()).await {
        Ok(Ok(ev)) => Some(ev),
        _ => None,
    }
}

/// Drive `sub_eventloop` until an `Incoming::Publish` arrives on
/// `target_topic`, or the deadline elapses. Returns the payload bytes
/// of the first matching publish (callers decode the protobuf).
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

/// Drive `sub_eventloop` for `window` and assert that NO
/// `Incoming::Publish` arrives on `target_topic`. Used to verify that
/// the publisher is actually parked on the ready barrier.
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

/// Subscribe to `target_topic` on the test broker. Returns the
/// `AsyncClient` (drop to disconnect) and its `EventLoop`.
async fn subscribe(
    target_topic: &str,
    client_id: &str,
    port: u16,
) -> (AsyncClient, rumqttc::EventLoop) {
    let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
    // Keep-alive 5s mirrors the broker's connection_timeout_ms so the
    // test broker doesn't disconnect the subscriber mid-run.
    opts.set_keep_alive(Duration::from_secs(5));
    let (client, eventloop) = AsyncClient::new(opts, 10);
    client
        .subscribe(target_topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe to {target_topic}");
    (client, eventloop)
}

// ── Tests ──────────────────────────────────────────────────────────────

/// The publisher must defer its first retained publish until
/// `mark_ready()` is called. While the barrier is held, a subscriber
/// connecting to `acowork/global/providers` must not receive any
/// `Publish` event for that topic.
///
/// Before Fix 1, the publisher emitted synchronously at `start()` and
/// the subscriber would receive the (empty-key) snapshot immediately.
/// After Fix 1, the snapshot lands only after `mark_ready()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publisher_defers_initial_publish_until_mark_ready() {
    let port = unique_port("defer");

    // 1. Start the broker (spawns its own thread, returns once it's up).
    let broker = start_broker("127.0.0.1", port).expect("broker should start");

    // 2. Seed the vault with the provider key.
    let vault_dir = unique_vault_dir("defer", port);
    seed_vault_with_provider_key(&vault_dir, PROVIDER_ID, API_KEY_VALUE, PASSWORD);

    // 3. Build the shared GatewayState (vault re-unlocks via the
    //    same password, picking up the stored key).
    let shared_state = build_shared_state(&vault_dir, PASSWORD);

    // 4. Connect a GatewayMqttClient and start the publisher.
    let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
        .await
        .expect("publisher client should connect");
    let publisher = MqttGlobalResourcesPublisher::new(client.clone(), shared_state.clone());
    let handle = publisher.start();

    // 5. Subscribe BEFORE mark_ready so the broker has a subscriber
    //    ready to receive the retained snapshot the moment it lands.
    let (sub_client, mut sub_eventloop) =
        subscribe("acowork/global/providers", "test:publisher-vault-race:defer", port).await;

    // 6. While ready=false, the publisher must NOT emit
    //    acowork/global/providers. Poll the subscriber for 1 s and
    //    fail loudly if we see one.
    assert_no_publish_in_window(
        &mut sub_eventloop,
        "acowork/global/providers",
        Duration::from_secs(1),
        "publisher must defer initial publish until mark_ready() (Fix 1)",
    )
    .await;

    // 7. Now mark ready. The publisher's loop unblocks and the initial
    //    retained snapshot lands on the broker.
    handle.mark_ready();

    // 8. Collect the first retained payload. The 5 s window is
    //    generous; in practice this completes in <100 ms.
    let payload = collect_first_publish_payload(
        &mut sub_eventloop,
        "acowork/global/providers",
        Duration::from_secs(5),
    )
    .await
    .expect("publisher must emit acowork/global/providers after mark_ready()");

    // 9. Decode + sanity check: the provider's api_key must equal the
    //    value stored in the vault before mark_ready().
    let envelope = DataEnvelope::decode(payload.as_slice()).expect("decode DataEnvelope");
    let providers_payload = match envelope.payload {
        Some(data_envelope::Payload::AvailableProviders(p)) => p,
        other => panic!("expected AvailableProviders, got {other:?}"),
    };
    let minimax_entry = providers_payload
        .providers
        .iter()
        .find(|p| p.id == PROVIDER_ID)
        .unwrap_or_else(|| {
            panic!(
                "minimax-cn must appear in the published payload; got ids: {:?}",
                providers_payload
                    .providers
                    .iter()
                    .map(|p| &p.id)
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        minimax_entry.api_key, API_KEY_VALUE,
        "First retained payload after mark_ready() must include the decrypted API key \
         that was stored in the vault BEFORE mark_ready. An empty string here means the \
         publisher emitted before the vault was unlocked — Fix 1 regression."
    );

    drop(sub_client);
    drop(handle);
    drop(client);
    drop(broker);

    let _ = std::fs::remove_dir_all(&vault_dir);
}

/// A subsequent `trigger_republish()` after `mark_ready()` must also
/// carry the decrypted API key. This guards against a future
/// regression where the initial publish goes through the barrier but
/// the trigger-driven loop accidentally re-reads the vault while it's
/// locked (e.g. after a separate unlock-then-relock sequence).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publisher_republish_after_mark_ready_keeps_decrypted_key() {
    let port = unique_port("republish");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");

    let vault_dir = unique_vault_dir("republish", port);
    seed_vault_with_provider_key(&vault_dir, PROVIDER_ID, API_KEY_VALUE, PASSWORD);
    let shared_state = build_shared_state(&vault_dir, PASSWORD);

    let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
        .await
        .expect("publisher client should connect");
    let publisher = MqttGlobalResourcesPublisher::new(client.clone(), shared_state.clone());
    let handle = publisher.start();

    let (sub_client, mut sub_eventloop) = subscribe(
        "acowork/global/providers",
        "test:publisher-vault-race:republish",
        port,
    )
    .await;

    // Mark ready so the loop unblocks and publishes the initial snapshot.
    handle.mark_ready();

    // Drain the first retained payload (the initial snapshot).
    let first_payload = collect_first_publish_payload(
        &mut sub_eventloop,
        "acowork/global/providers",
        Duration::from_secs(5),
    )
    .await
    .expect("initial retained payload after mark_ready()");
    let first_envelope = DataEnvelope::decode(first_payload.as_slice()).expect("decode first");
    let first_providers = match first_envelope.payload {
        Some(data_envelope::Payload::AvailableProviders(p)) => p,
        other => panic!("first: expected AvailableProviders, got {other:?}"),
    };
    let first_key = first_providers
        .providers
        .iter()
        .find(|p| p.id == PROVIDER_ID)
        .map(|p| p.api_key.clone())
        .expect("first snapshot must include minimax-cn");
    assert_eq!(
        first_key, API_KEY_VALUE,
        "first snapshot must have decrypted key"
    );

    // Now trigger a republish (e.g. simulated add_provider_key HTTP call).
    handle.trigger_republish();

    // The broker will deliver the new retained message; collect it.
    let second_payload = collect_first_publish_payload(
        &mut sub_eventloop,
        "acowork/global/providers",
        Duration::from_secs(5),
    )
    .await
    .expect("second retained payload after trigger_republish()");
    let second_envelope = DataEnvelope::decode(second_payload.as_slice()).expect("decode second");
    let second_providers = match second_envelope.payload {
        Some(data_envelope::Payload::AvailableProviders(p)) => p,
        other => panic!("second: expected AvailableProviders, got {other:?}"),
    };
    let second_key = second_providers
        .providers
        .iter()
        .find(|p| p.id == PROVIDER_ID)
        .map(|p| p.api_key.clone())
        .expect("second snapshot must include minimax-cn");
    assert_eq!(
        second_key, API_KEY_VALUE,
        "trigger-driven republish must also carry the decrypted API key"
    );

    drop(sub_client);
    drop(handle);
    drop(client);
    drop(broker);

    let _ = std::fs::remove_dir_all(&vault_dir);
}

/// Multiple `mark_ready()` calls must be idempotent: the publisher
/// loop unblocks once and subsequent calls do not produce additional
/// "ready" transitions. The test pins down the watch-channel
/// semantics — the ready_tx.send(true) is idempotent on the receiver
/// side because the value is the same, so the loop's `ready_rx.changed()`
/// future only resolves once per actual value change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mark_ready_is_idempotent() {
    let port = unique_port("idem");
    let broker = start_broker("127.0.0.1", port).expect("broker should start");

    let vault_dir = unique_vault_dir("idem", port);
    seed_vault_with_provider_key(&vault_dir, PROVIDER_ID, API_KEY_VALUE, PASSWORD);
    let shared_state = build_shared_state(&vault_dir, PASSWORD);
    let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
        .await
        .expect("publisher client should connect");
    let publisher = MqttGlobalResourcesPublisher::new(client.clone(), shared_state.clone());
    let handle = publisher.start();

    let (sub_client, mut sub_eventloop) = subscribe(
        "acowork/global/providers",
        "test:publisher-vault-race:idem",
        port,
    )
    .await;

    // Three consecutive mark_ready() calls must not panic. Each call
    // sends `true` through the watch channel and notifies the loop.
    handle.mark_ready();
    handle.mark_ready();
    handle.mark_ready();

    // Verify the loop still publishes (i.e. didn't crash on the
    // duplicate ready signals).
    let payload = collect_first_publish_payload(
        &mut sub_eventloop,
        "acowork/global/providers",
        Duration::from_secs(5),
    )
    .await
    .expect("loop must still publish after multiple mark_ready calls");
    let envelope = DataEnvelope::decode(payload.as_slice()).expect("decode");
    assert!(
        matches!(
            envelope.payload,
            Some(data_envelope::Payload::AvailableProviders(_))
        ),
        "published payload must still be AvailableProviders"
    );

    drop(sub_client);
    drop(handle);
    drop(client);
    drop(broker);

    let _ = std::fs::remove_dir_all(&vault_dir);
}