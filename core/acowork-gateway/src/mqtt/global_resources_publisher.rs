//! Global Resources Publisher (ADR-033 Phase 1).
//!
//! The Gateway's MQTT publisher for `acowork/global/{kind}` Retained topics.
//! Reads from `GatewayState.resource_cache` + sidecar process state, builds
//! `Available*` protobuf payloads, and publishes them as Retained messages
//! on the MQTT broker.
//!
//! In Phase 1, this runs alongside the existing gRPC `()`.
//! When a resource changes (provider added, MCP installed, embedding model
//! loaded, etc.), both pushers fire: gRPC pushes to connected Runtimes,
//! MQTT publishes the Retained snapshot for future subscribers.
//!
//! Retained messages ensure new Runtime subscribers receive the latest
//! global resource state immediately on subscription, without waiting for
//! the next periodic publish cycle.
//!
//! See `docs/zh/protocols/mqtt.md` §3.1.1 and §5.4.

use std::sync::Arc;

use tokio::sync::{watch, Notify};

use acowork_core::mqtt_proto::{
    self, AvailableEmbeddingModels, AvailableMcps, AvailableProviders, AvailableUsers, DataEnvelope,
};

use crate::http::routes::SharedHttpState;
use crate::mqtt::client::{GatewayMqttClient, MqttQoS};
use crate::mqtt::global_resources_builders::{
    build_available_embedding_models, build_available_mcps, build_available_providers,
    build_available_users,
};

/// MQTT topic constants for global resources (§3.1.1).
mod topics {
    pub const PROVIDERS: &str = "acowork/global/providers";
    pub const MCPS: &str = "acowork/global/mcps";
    pub const EMBEDDING_MODELS: &str = "acowork/global/embedding_models";
    /// ADR-042: active user profile snapshot. Runtime uses this to populate
    /// the identity_context for the compact model's language hint.
    pub const USER_PROFILE: &str = "acowork/global/user_profile";
}

/// The Gateway's MQTT global resources publisher.
///
/// Holds a `GatewayMqttClient`, a `Notify` trigger for republishes, and a
/// `watch::Sender<bool>` for the "ready to publish" barrier.
///
/// The publish loop defers its first Retained publish until [`MqttPublisherHandle::set_ready`]
/// is called with `true`. This guarantees that any Runtime subscribing
/// to `acowork/global/providers` receives a snapshot whose `ProviderRef.api_key`
/// fields are populated, never the all-empty snapshot emitted when the vault
/// is still locked at boot.
///
/// After the first publish, the loop runs purely on Notify triggers (no
/// polling). See `desktop-onboarding-bugfix_154b7ff7.md` §Fix 1.
pub struct MqttGlobalResourcesPublisher {
    /// The MQTT client used to publish.
    client: GatewayMqttClient,
    /// Shared Gateway state (read-only access for building payloads).
    gateway_state: SharedHttpState,
    /// Notification trigger for republish.
    notify: Arc<Notify>,
    /// Ready barrier sender. The publish loop defers its initial publish
    /// until the sender transmits `true`.
    ready_tx: watch::Sender<bool>,
}

/// Handle returned by `start()`. Dropping it stops the publisher loop.
pub struct MqttPublisherHandle {
    _task: tokio::task::JoinHandle<()>,
    notify: Arc<Notify>,
    ready_tx: watch::Sender<bool>,
}

impl MqttPublisherHandle {
    /// Trigger an immediate republish of all global resource topics.
    pub fn trigger_republish(&self) {
        self.notify.notify_one();
    }

    /// Get a clonable trigger that HTTP handlers can store in AppState.
    pub fn create_trigger(&self) -> MqttPublisherTrigger {
        MqttPublisherTrigger {
            notify: self.notify.clone(),
        }
    }

    /// Ready-barrier control.
    ///
    /// The publish loop blocks on `set_ready(true)` before performing its
    /// first Retained publish. Callers that need the first snapshot to be
    /// coherent — specifically, the vault must be unlocked (so the
    /// ProviderRef.api_key fields decrypt) AND the local node must be
    /// enrolled (so other subscribers don't race against onboarding) —
    /// use this as a coordination point.
    ///
    /// Setting `ready = true` additionally nudges the publish loop so a
    /// fast-path (already past the barrier and parked on `notified()`) is
    /// not skipped.
    pub fn set_ready(&self, ready: bool) {
        let _ = self.ready_tx.send(ready);
        if ready {
            self.notify.notify_one();
        }
    }

    /// Convenience: `set_ready(true)`. See [`Self::set_ready`] for details.
    pub fn mark_ready(&self) {
        self.set_ready(true);
    }
}

/// Clonable trigger for the MQTT publisher.
///
/// Stored in `AppState` so HTTP handlers can signal the publisher
/// to republish after a resource change, without holding a full
/// `MqttPublisherHandle` (which is not clonable).
#[derive(Clone)]
pub struct MqttPublisherTrigger {
    notify: Arc<Notify>,
}

impl MqttPublisherTrigger {
    /// Trigger an immediate republish of all global resource topics.
    pub fn trigger(&self) {
        self.notify.notify_one();
    }
}

impl MqttGlobalResourcesPublisher {
    /// Create a new publisher with the given MQTT client and Gateway state.
    pub fn new(client: GatewayMqttClient, gateway_state: SharedHttpState) -> Self {
        let (ready_tx, _) = watch::channel(false);
        Self {
            client,
            gateway_state,
            notify: Arc::new(Notify::new()),
            ready_tx,
        }
    }

    /// Start the publisher background loop.
    ///
    /// The loop:
    /// 1. **Ready barrier** — blocks on `set_ready(true)` before the first
    ///    publish, so the first retained snapshot only fires once the vault
    ///    is unlocked AND the local node has enrolled. See
    ///    `desktop-onboarding-bugfix_154b7ff7.md` §Fix 1.
    /// 2. Publishes all `acowork/global/*` topics once (initial Retained snapshot).
    /// 3. Waits for `trigger_republish()` calls from HTTP handlers.
    /// 4. On each trigger, re-reads state and republishes all topics as
    ///    Retained messages.
    ///
    /// No periodic polling — Retained messages ensure new subscribers get
    /// the latest snapshot immediately, and every resource-mutating HTTP
    /// handler calls `MqttPublisherTrigger::trigger()` to drive republish on
    /// change.
    pub fn start(&self) -> MqttPublisherHandle {
        let notify_for_loop = self.notify.clone();
        let mut ready_rx = self.ready_tx.subscribe();
        let handle_ready_tx = self.ready_tx.clone();
        let handle_notify = self.notify.clone();

        let helper = LoopHelper {
            client: self.client.clone(),
            gateway_state: self.gateway_state.clone(),
        };

        let task = tokio::spawn(async move {
            tracing::info!("MQTT Global Resources Publisher loop started");

            // ── Ready barrier ───────────────────────────────────────
            // The publisher MUST NOT emit its first retained snapshot
            // before both (a) the vault is unlocked (so ProviderRef.api_key
            // fields are decryptable) and (b) the local node has enrolled
            // (so other subscribers don't race against onboarding).
            // Otherwise a Runtime subscribing in the first few seconds
            // caches `api_key_lengths=[0]` and ignores subsequent
            // republishes (Retained messages are only delivered to
            // subscribers that have not already seen the prior retained
            // value).
            if !*ready_rx.borrow() {
                tracing::info!(
                    "Publisher: deferring initial publish until vault unlocked AND node local online"
                );
            }
            loop {
                if *ready_rx.borrow() {
                    break;
                }
                match ready_rx.changed().await {
                    Ok(()) => {}
                    Err(_) => {
                        tracing::warn!(
                            "MQTT publisher: ready signal sender dropped; aborting publisher loop"
                        );
                        return;
                    }
                }
            }
            tracing::info!("Publisher: ready signal received, performing initial publish");

            helper.publish_all().await;

            // ── Trigger-driven republish loop — no periodic polling ─────
            // Every resource-mutating HTTP handler (add/remove/update provider,
            // MCP catalog entry, embedding model, global config)
            // calls `trigger()` which wakes this loop via `Notify`.
            loop {
                notify_for_loop.notified().await;
                tracing::debug!("MQTT publisher: triggered republish");
                helper.publish_all().await;
            }
        });

        MqttPublisherHandle {
            _task: task,
            notify: handle_notify,
            ready_tx: handle_ready_tx,
        }
    }
}

// ── Loop helper ───────────────────────────────────────────────────────
//
// `LoopHelper` is an internal struct that owns the publish-state needed by
// the loop body. It is created from `MqttGlobalResourcesPublisher::start()`
// and moved outright into the spawned task, so the loop has no borrow
// conflicts with the `ready_rx` receiver parked on the ready barrier.

struct LoopHelper {
    client: GatewayMqttClient,
    gateway_state: SharedHttpState,
}

impl LoopHelper {
    /// Publish all `acowork/global/*` Retained topics.
    ///
    /// Reads the current GatewayState snapshot and publishes:
    /// - `acowork/global/providers` — AvailableProviders
    /// - `acowork/global/mcps` — AvailableMcps
    /// - `acowork/global/embedding_models` — AvailableEmbeddingModels
    /// - `acowork/global/user_profile` — AvailableUsers (ADR-042)
    async fn publish_all(&self) {
        let gw = self.gateway_state.read().await;

        // Build all payloads from the snapshot.
        let providers_payload = build_available_providers(&gw);
        let mcps_payload = build_available_mcps(&gw);
        let embedding_payload = build_available_embedding_models(&gw);
        let user_profile_payload = build_available_users(&gw);

        tracing::debug!(
            provider_count = providers_payload.providers.len(),
            api_key_lengths = ?providers_payload
                .providers
                .iter()
                .map(|p| (p.id.clone(), p.api_key.len()))
                .collect::<Vec<_>>(),
            "MQTT publisher: built AvailableProviders payload (debug)"
        );

        // Drop the read lock before publishing (don't hold it across network I/O).
        drop(gw);

        // Publish each topic. Errors are logged but don't abort the batch.
        self.publish_providers(providers_payload).await;
        self.publish_mcps(mcps_payload).await;
        self.publish_embedding_models(embedding_payload).await;
        self.publish_user_profiles(user_profile_payload).await;
    }

    async fn publish_providers(&self, payload: AvailableProviders) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableProviders(payload)),
        };
        self.publish_envelope_raw(topics::PROVIDERS, &envelope).await;
    }

    async fn publish_mcps(&self, payload: AvailableMcps) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableMcps(payload)),
        };
        self.publish_envelope_raw(topics::MCPS, &envelope).await;
    }

    async fn publish_embedding_models(&self, payload: AvailableEmbeddingModels) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableEmbeddingModels(payload)),
        };
        self.publish_envelope_raw(topics::EMBEDDING_MODELS, &envelope).await;
    }

    /// ADR-042: publish the active user profile snapshot.
    ///
    /// Empty `active_user` (no user created yet) is still published as a
    /// retained message with `version` bumped — this signals to Runtime
    /// "no identity, fall back to detection-based heuristics".
    async fn publish_user_profiles(&self, payload: AvailableUsers) {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(mqtt_proto::data_envelope::Payload::AvailableUsers(payload)),
        };
        self.publish_envelope_raw(topics::USER_PROFILE, &envelope).await;
    }

    /// Helper: publish a DataEnvelope with Retained=true.
    async fn publish_envelope_raw(&self, topic: &str, envelope: &DataEnvelope) {
        if let Err(e) = self
            .client
            .publish_envelope(topic, envelope, MqttQoS::AtLeastOnce, true)
            .await
        {
            tracing::warn!(topic, error = %e, "Failed to publish MQTT global resource topic");
        } else {
            tracing::debug!(topic, "Published MQTT global resource (Retained)");
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────
//
// Payload builders (`build_available_*`) and enum mappers live in
// `mqtt::global_resources_builders` and are tested there. Tests below
// focus on the publisher's own wiring (broker, retained delivery, ready
// barrier).

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn test_publisher_publishes_retained_snapshot() {
        use crate::gateway::state::GatewayState;
        use crate::http::routes::SharedHttpState;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let port = crate::test_support::free_port();
        // Threaded mode: `start_broker` blocks forever on rumqttd's
        // `Broker::start()` (it joins the server threads), so calling it
        // from the test thread would hang whenever the port is free.
        let broker = crate::mqtt::broker::start_broker("127.0.0.1", port)
            .expect("broker should start");

        let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("client should connect");

        let gw_state: SharedHttpState = Arc::new(RwLock::new(GatewayState::new("/tmp/test-vault")));
        let publisher = MqttGlobalResourcesPublisher::new(client, gw_state);
        let handle = publisher.start();

        // Fix-1: the publisher loop blocks on `set_ready(true)` before
        // its first retained publish. `mark_ready()` sends the ready
        // signal AND nudges the loop, so the initial snapshot lands
        // immediately. (Pre-Fix-1 this test used `trigger_republish()`,
        // which only notifies and would hang the test forever after
        // the ready barrier was added.)
        handle.mark_ready();

        // Give the publisher a moment to publish
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Subscribe and verify we receive the retained messages.
        use rumqttc::{AsyncClient, MqttOptions, QoS};
        let mut opts = MqttOptions::new("test:subscriber", "127.0.0.1", port);
        opts.set_keep_alive(std::time::Duration::from_secs(5));
        let (sub_client, mut eventloop) = AsyncClient::new(opts, 10);
        sub_client
            .subscribe("acowork/global/#", QoS::AtLeastOnce)
            .await
            .unwrap();

        // Poll for retained messages. We use `tokio::time::timeout`
        // around each poll so we never spin-busy when the eventloop
        // keeps returning non-Publish events (e.g. ConnAck, SubAck);
        // without the sleep the for-loop would burn CPU until the
        // broker eventually delivers the retained publish.
        let mut received_topics = Vec::new();
        let poll_budget = std::time::Duration::from_secs(5);
        let poll_start = std::time::Instant::now();
        while poll_start.elapsed() < poll_budget && received_topics.len() < 6 {
            let remaining = poll_budget.saturating_sub(poll_start.elapsed());
            match tokio::time::timeout(
                remaining.min(std::time::Duration::from_millis(100)),
                eventloop.poll(),
            )
            .await
            {
                Ok(Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p)))) => {
                    received_topics.push(p.topic.clone());
                }
                Ok(_) => {
                    // Non-Publish event (ConnAck/SubAck/PingResp/etc) or
                    // transient transport error. Yield so we don't pin a
                    // CPU core while waiting for the broker's retained
                    // delivery.
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(_) => {
                    // poll() deadline elapsed without an event. Loop
                    // and try again until the outer budget is reached.
                }
            }
        }

        assert!(
            received_topics.contains(&"acowork/global/providers".to_string()),
            "should receive providers retained within {:?}: {:?}",
            poll_budget,
            received_topics
        );
        assert!(
            received_topics.contains(&"acowork/global/mcps".to_string()),
            "should receive mcps retained within {:?}: {:?}",
            poll_budget,
            received_topics
        );
        // ADR-042: verify the new user_profile retained topic is published.
        assert!(
            received_topics.contains(&"acowork/global/user_profile".to_string()),
            "should receive user_profile retained (ADR-042) within {:?}: {:?}",
            poll_budget,
            received_topics
        );

        drop(sub_client);
        drop(handle);
        drop(broker);
    }
}
