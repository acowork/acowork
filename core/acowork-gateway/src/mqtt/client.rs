//! Gateway MQTT client (ADR-033 Phase 1).
//!
//! The Gateway uses a `rumqttc::AsyncClient` to connect to its own
//! embedded broker. This client (`client_id: gateway:publisher`) is
//! the sole publisher of `acowork/global/{kind}` Retained topics.
//!
//! In Phase 2+, the Gateway also subscribes to agent status topics
//! for the Agent Registry. Phase 1 uses this client only for publishing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, QoS};
use tokio::sync::Mutex;

use acowork_core::defaults;
use acowork_core::mqtt_proto::DataEnvelope;
use acowork_core::mqtt_proto::USER_PROFILES_CHANGED_TOPIC;
use acowork_mqtt_session::{
    MqttClient, MqttClientConfig, MqttClientError, MqttClientHandler,
};

/// Topic filters that must be re-subscribed on every ConnAck.
///
/// With `clean_session = true` the broker drops all subscriptions on
/// disconnect, so **every** active subscription must be listed here.
/// Without re-subscribing, the Gateway silently loses agent http_port,
/// status, and ready updates after any MQTT reconnect.
const PERSISTENT_SUBSCRIPTIONS: &[(&str, QoS)] = &[
    // ADR-055 D3 / Phase 1.4: the Runtime registers its full endpoint
    // URL ("http://127.0.0.1:{port}") on this topic. NOTE: this was
    // originally `http_port`; the Phase 1.4 topic rename updated
    // dispatch.rs but missed this subscription list — fixed together
    // with the ADR-055 Phase 2a node subscriptions.
    ("acowork/agents/+/http_endpoint", QoS::AtLeastOnce),
    ("acowork/agents/+/status", QoS::AtLeastOnce),
    // ADR-055 §6.2: node control plane — LWT-driven node online/offline
    // (plain text) + node metadata (protobuf NodeInfo envelope), both
    // retained so a fresh Gateway startup recovers the node view from
    // the broker without polling.
    ("acowork/nodes/+/status", QoS::AtLeastOnce),
    ("acowork/nodes/+/info", QoS::AtLeastOnce),
    // ADR-055 §6.7 (Phase 4): node-local LSP relay endpoint (retained
    // AvailableLsps envelope, replaces the deprecated
    // `acowork/global/lsps`). Feeds NodeRegistry::lsp_endpoint, served
    // via `GET /api/agents/{id}/lsp-endpoint`.
    ("acowork/nodes/+/lsps", QoS::AtLeastOnce),
    // ADR-055 §6.2: per-agent NodeEvent results (protobuf envelope,
    // QoS 1) — the Gateway correlates these against in-flight control
    // commands by request_id (NodeControlClient).
    ("acowork/nodes/+/agents/+/events", QoS::AtLeastOnce),
    // ADR-055 §6.5: per-agent installed-package inventory (Retained).
    // The Gateway aggregates these into its installed_agents view,
    // replacing the pre-hard-cut on-disk packages scan (L2-9). An empty
    // retained payload clears the entry on uninstall.
    ("acowork/nodes/+/agents/+/installed", QoS::AtLeastOnce),
    // ADR-055 Phase 5a: node enrollment handshake (QoS 1, non-retained
    // — the Node publishes once on bootstrap).
    ("acowork/nodes/+/enroll", QoS::AtLeastOnce),
    // ADR-059 §7.2: Node control-plane readiness (DataEnvelope<NodeReady>,
    // QoS 1 retained). The Node publishes after CONNECT + control
    // subscriptions; the Gateway re-marks `node.{id}` ready in the
    // SubsystemReadinessRegistry on receipt, and demotes it on an empty
    // retained payload (Node shutdown / LWT / disconnect clear).
    ("acowork/nodes/+/ready", QoS::AtLeastOnce),
    // ADR-084 §决策 4b: the user service publishes this after every account /
    // profile mutation; the Gateway re-pulls `GET /internal/user-profiles` and
    // republishes global resources (`last_user_profile`, ADR-042). Non-retained
    // on purpose — it is a nudge, not a value, so a signal missed while the
    // Gateway was down costs nothing: the startup pull below covers it.
    (USER_PROFILES_CHANGED_TOPIC, QoS::AtLeastOnce),
];

/// Callback type for receiving non-global MQTT messages (e.g. agent http_port).
pub type MqttMessageCallback = Arc<dyn Fn(String, Vec<u8>) + Send + Sync>;

/// Error type for Gateway MQTT client operations.
#[derive(Debug, thiserror::Error)]
pub enum GatewayMqttClientError {
    #[error("MQTT connection error: {0}")]
    Connection(String),
    #[error("MQTT publish error: {0}")]
    Publish(String),
    #[error("MQTT subscribe error: {0}")]
    Subscribe(String),
}

impl From<MqttClientError> for GatewayMqttClientError {
    fn from(e: MqttClientError) -> Self {
        match e {
            MqttClientError::Connection(s) => GatewayMqttClientError::Connection(s),
            MqttClientError::Publish(s) => GatewayMqttClientError::Publish(s),
            MqttClientError::Subscribe(s) => GatewayMqttClientError::Subscribe(s),
        }
    }
}

/// QoS level for MQTT messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttQoS {
    /// At most once (fire-and-forget). For streaming events.
    AtMostOnce,
    /// At least once. For state changes and control commands.
    AtLeastOnce,
}

impl From<MqttQoS> for QoS {
    fn from(qos: MqttQoS) -> Self {
        match qos {
            MqttQoS::AtMostOnce => QoS::AtMostOnce,
            MqttQoS::AtLeastOnce => QoS::AtLeastOnce,
        }
    }
}

/// The Gateway's MQTT client for publishing to the embedded broker.
///
/// Wraps the shared [`MqttClient`] (ADR-065): the entire poll loop —
/// error classification, backoff, soft-restart, watchdog and wake
/// recovery — lives in `acowork-mqtt-session`. This struct only adds
/// the Gateway's entity differences via [`GatewayHandler`].
#[derive(Clone)]
pub struct GatewayMqttClient {
    inner: MqttClient<GatewayHandler>,
    /// ADR-059 §7.2 replay guard (optional) — the handler stamps a
    /// gateway (re)connection on every ConnAck so the dispatch layer
    /// can suppress stale retained replays of node offline signals.
    replay_guard: Arc<std::sync::Mutex<Option<Arc<crate::mqtt::dispatch::NodeReplayGuard>>>>,
}

/// Gateway entity handler for the shared [`MqttClient`] (ADR-065 Step 4).
///
/// Implements the Gateway publisher's per-process differences: message
/// routing, persistent-topic re-subscription on every ConnAck, and the
/// ADR-059 §7.2 replay-window stamping (only reconnects open the
/// window — the first ConnAck's retained replay reflects the nodes'
/// real current state).
struct GatewayHandler {
    /// ADR-059 §7.2: replay guard slot (attached after connect via
    /// [`GatewayMqttClient::set_replay_guard`]).
    replay_guard: Arc<std::sync::Mutex<Option<Arc<crate::mqtt::dispatch::NodeReplayGuard>>>>,
    /// Whether the first ConnAck has been observed. Only later
    /// (re)connects open the replay window.
    ever_connected: Arc<AtomicBool>,
    /// Callback for incoming non-global MQTT messages.
    message_callback: Option<MqttMessageCallback>,
}

#[async_trait::async_trait]
impl MqttClientHandler for GatewayHandler {
    async fn on_publish(&self, topic: &str, payload: &[u8]) {
        if let Some(ref cb) = self.message_callback {
            cb(topic.to_string(), payload.to_vec());
        }
    }

    async fn on_connack(&self, client: &AsyncClient) -> Result<(), String> {
        tracing::info!(
            "Gateway MQTT broker confirmed (re)connection - re-subscribing persistent topics"
        );
        // ADR-059 §7.2: open the replay window — stale retained replays
        // only exist after a (re)subscribe. The FIRST ConnAck is the
        // initial connection whose retained replay reflects the nodes'
        // real state (no window). Every later ConnAck is a reconnect
        // (same eventloop or soft-restarted): stamp the guard.
        if self.ever_connected.swap(true, Ordering::SeqCst)
            && let Some(guard) = self
                .replay_guard
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        {
            guard.mark_gateway_reconnect();
        }
        for (filter, qos) in PERSISTENT_SUBSCRIPTIONS {
            if let Err(e) = client.subscribe(*filter, *qos).await {
                tracing::warn!(filter, error = %e, "Gateway MQTT resubscribe failed");
            }
        }
        Ok(())
    }
}

impl GatewayMqttClient {
    /// Obtain a clone of the current `AsyncClient`.
    async fn client(&self) -> AsyncClient {
        self.inner.shared_handle().lock().await.clone()
    }

    /// Attach the ADR-059 §7.2 replay guard so every gateway (re)connect
    /// (ConnAck) opens the replay window for node offline signals.
    pub fn set_replay_guard(&self, guard: Arc<crate::mqtt::dispatch::NodeReplayGuard>) {
        *self
            .replay_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(guard);
    }
}

impl GatewayMqttClient {
    /// Create and connect a new Gateway MQTT client.
    ///
    /// Spawns a background task that polls the event loop to maintain
    /// the connection and handle reconnections automatically.
    pub async fn connect(
        host: &str,
        port: u16,
        client_id: &str,
        credentials: Option<(&str, &str)>,
        message_callback: Option<MqttMessageCallback>,
    ) -> Result<Self, GatewayMqttClientError> {
        // The caller usually passes the *configured* host, which doubles
        // as the broker bind address (`--mqtt-addr 0.0.0.0:19875` in
        // remote mode). A wildcard bind address is NOT a valid TCP
        // connect target (Windows rejects connects to `0.0.0.0`), so
        // normalize it to loopback before dialing — otherwise the
        // client never reaches the embedded broker and every
        // subscription (node enroll, status, …) is silently lost.
        let host = acowork_core::addr::connect_host(host);
        let config = MqttClientConfig {
            client_id: client_id.to_string(),
            host: host.to_string(),
            port,
            transport: acowork_mqtt_session::MqttTransport::Tcp,
            credentials: credentials.map(|(u, p)| (u.to_string(), p.to_string())),
            last_will: None,
            max_packet_size: defaults::GATEWAY_MQTT_MAX_PACKET_SIZE,
            queue_capacity: 50,
        };

        let replay_guard: Arc<
            std::sync::Mutex<Option<Arc<crate::mqtt::dispatch::NodeReplayGuard>>>,
        > = Arc::new(std::sync::Mutex::new(None));
        let handler = GatewayHandler {
            replay_guard: Arc::clone(&replay_guard),
            ever_connected: Arc::new(AtomicBool::new(false)),
            message_callback,
        };

        let inner = MqttClient::connect(config, handler, None).await?;

        // Wait for the connection to be established (ADR-065: the
        // shared client's state machine reaches `Connected` on the
        // first ConnAck).
        if !inner.wait_for_connected(Duration::from_secs(2)).await {
            return Err(GatewayMqttClientError::Connection(format!(
                "Failed to connect to MQTT broker at {}:{} within timeout",
                host, port
            )));
        }

        tracing::info!(
            host,
            port,
            client_id,
            "Gateway MQTT client connected to broker"
        );

        Ok(Self { inner, replay_guard })
    }

    pub async fn new_publisher(
        host: &str,
        port: u16,
    ) -> Result<Self, GatewayMqttClientError> {
        Self::connect(host, port, defaults::GATEWAY_MQTT_PUBLISHER_CLIENT_ID, None, None)
            .await
    }

    /// Create a publisher with the internal credentials (ADR-055 Phase
    /// 5a: required when `mqtt.auth_enabled` is on).
    pub async fn new_publisher_with_credentials(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
    ) -> Result<Self, GatewayMqttClientError> {
        Self::connect(
            host,
            port,
            defaults::GATEWAY_MQTT_PUBLISHER_CLIENT_ID,
            Some((username, password)),
            None,
        )
        .await
    }

    /// Create a publisher with a message callback for incoming subscriptions.
    pub async fn new_publisher_with_callback(
        host: &str,
        port: u16,
        callback: MqttMessageCallback,
    ) -> Result<Self, GatewayMqttClientError> {
        Self::connect(
            host,
            port,
            defaults::GATEWAY_MQTT_PUBLISHER_CLIENT_ID,
            None,
            Some(callback),
        )
        .await
    }

    /// Create a publisher with a message callback + internal
    /// credentials (ADR-055 Phase 5a).
    pub async fn new_publisher_with_callback_and_credentials(
        host: &str,
        port: u16,
        callback: MqttMessageCallback,
        username: &str,
        password: &str,
    ) -> Result<Self, GatewayMqttClientError> {
        Self::connect(
            host,
            port,
            defaults::GATEWAY_MQTT_PUBLISHER_CLIENT_ID,
            Some((username, password)),
            Some(callback),
        )
        .await
    }

    /// Create the Gateway publisher client with default localhost settings.
    pub async fn new_default_publisher() -> Result<Self, GatewayMqttClientError> {
        Self::new_publisher(
            defaults::GATEWAY_MQTT_HOST,
            defaults::GATEWAY_MQTT_PORT,
        )
        .await
    }

    /// Publish a `DataEnvelope` payload to a topic.
    ///
    /// The envelope is protobuf-encoded and published as binary.
    pub async fn publish_envelope(
        &self,
        topic: &str,
        envelope: &DataEnvelope,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), GatewayMqttClientError> {
        let payload = prost::Message::encode_to_vec(envelope);
        self.client().await
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| GatewayMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        tracing::trace!(topic, qos = ?qos, retain, "MQTT published envelope");
        Ok(())
    }

    /// Publish a control command to an agent's control topic.
    ///
    /// Desktop sends commands to Runtime via MQTT (mqtt.md §5.2).
    /// QoS: AtLeastOnce — control commands must not be lost.
    pub async fn publish_control_command(
        &self,
        instance_id: &str,
        command: acowork_core::mqtt_proto::ControlCommand,
    ) -> Result<(), GatewayMqttClientError> {
        let topic = format!("acowork/agents/{}/sessions/control/{}",
            instance_id,
            match &command.command {
                // ADR-076 §决策 4: this is the *only* user-invisible control
                // command left. Every user-initiated session action (lifecycle,
                // config, chat, stop, approve, …) is HTTP-only now, because a
                // control message carries no identity for the Runtime to
                // authorize. Their proto variants are deleted, so they cannot
                // be matched (or published) at all.
                Some(acowork_core::mqtt_proto::control_command::Command::Intent(_)) => "intent",
                // `ActiveHeartbeat` is Desktop → Runtime and is never
                // published from here.
                _ => "unknown",
            }
        );
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::ControlCommand(command)),
        };
        self.publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false).await
    }

    /// Publish a raw binary payload to a topic.
    pub async fn publish_raw(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), GatewayMqttClientError> {
        self.client().await
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| GatewayMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Publish a text payload to a topic (e.g. agent status "online"/"offline").
    pub async fn publish_text(
        &self,
        topic: &str,
        payload: &str,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), GatewayMqttClientError> {
        self.client().await
            .publish(topic, qos.into(), retain, payload.as_bytes())
            .await
            .map_err(|e| GatewayMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Subscribe to a topic filter.
    #[allow(dead_code)]
    pub async fn subscribe(
        &self,
        filter: &str,
        qos: MqttQoS,
    ) -> Result<(), GatewayMqttClientError> {
        self.client().await
            .subscribe(filter, qos.into())
            .await
            .map_err(|e| GatewayMqttClientError::Subscribe(format!("'{}': {}", filter, e)))?;
        Ok(())
    }

    /// Get a clone of the inner AsyncClient for advanced use cases.
    ///
    /// The returned client shares the same event loop, so publishes
    /// through it will be handled by the same connection.
    pub async fn inner(&self) -> AsyncClient {
        self.client().await
    }
}

/// A shared, thread-safe Gateway MQTT client.
pub type SharedGatewayMqttClient = Arc<Mutex<GatewayMqttClient>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_gateway_client_connects_to_broker() {
        // Start a broker on a test port. Threaded mode: `start_broker`
        // blocks forever on rumqttd's `Broker::start()` (it joins the
        // server threads), which would hang this test whenever the port
        // is free. `start_broker` parks the broker on a
        // background OS thread instead.
        let port = crate::test_support::free_port();
        let broker_handle = crate::mqtt::broker::start_broker("127.0.0.1", port)
            .expect("broker should start");

        // Connect a publisher client
        let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("client should connect");

        // Publish a test message
        client
            .publish_text(
                "acowork/test/status",
                "online",
                MqttQoS::AtLeastOnce,
                true,
            )
            .await
            .expect("publish should succeed");

        // Clean up
        drop(client);
        drop(broker_handle);
    }

    #[tokio::test]
    async fn test_publish_envelope() {
        let port = crate::test_support::free_port();
        // Threaded mode — see `test_gateway_client_connects_to_broker`
        // for why `start_broker` must not be called on the test thread.
        let broker_handle = crate::mqtt::broker::start_broker("127.0.0.1", port)
            .expect("broker should start");

        let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("client should connect");

        // Build and publish a DataEnvelope
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(
                acowork_core::mqtt_proto::AvailableProviders {
                    version: 1,
                    providers: vec![],
                    // ADR-056: forward the global compact-model list (empty here).
                    #[allow(deprecated)]
                    default_compact_model: None,
                    default_compact_models: vec![],
                },
            )),
        };

        client
            .publish_envelope(
                "acowork/global/providers",
                &envelope,
                MqttQoS::AtLeastOnce,
                true,
            )
            .await
            .expect("envelope publish should succeed");

        drop(client);
        drop(broker_handle);
    }
}
