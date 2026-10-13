//! Desktop MQTT client (ADR-033 Phase 3 / ADR-065 Step 4).
//!
//! The Tauri Rust backend connects to the Gateway's embedded MQTT broker
//! as a `rumqttc` client. It subscribes to agent lifecycle topics
//! (status, meta, config) and session events, forwarding them to the
//! React frontend via Tauri events. Control commands from the frontend
//! (send message, stop, create session) are published via MQTT.
//!
//! ADR-065 Step 4: this file now wraps the shared
//! [`acowork_mqtt_session::MqttClient<B>`] with a [`DesktopHandler`]
//! implementing [`acowork_mqtt_session::MqttClientHandler`]. The
//! poll loop, error classification, exponential backoff, soft-restart,
//! watchdog and wake recovery all live in the shared crate. The Desktop
//! wrapper only carries the entity differences: client_id format,
//! `MqttStatus` → Tauri event bridge, persistent topic filter list, and
//! `ControlCommand` protobuf encoding for the control topic.
//!
//! See `docs/zh/protocols/mqtt.md` §5.1.6, §5.2, §9.1.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rumqttc::{AsyncClient, QoS};
use tokio::sync::Mutex;

use acowork_core::defaults;
use acowork_mqtt_session::{
    ErrClass, MqttClient, MqttClientConfig, MqttClientHandler, MqttTransport, SessionState,
};

/// Where + how the Desktop MQTT client connects.
///
/// The endpoint is the full transport-level address of the broker —
/// `connect_mqtt` derives it from the deployment mode (design doc 24
/// §8.0): Local/Remote modes speak plain TCP to the broker host derived
/// from the Gateway base URL (port from `/api/status`); Relay mode
/// speaks WSS through the cloud relay's TLS byte pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MqttEndpoint {
    /// Plain TCP broker at `host:port` (local / LAN-remote modes).
    Tcp { host: String, port: u16 },
    /// Secure WebSocket through the cloud relay (relay mode). `url` is
    /// the FULL broker URL — `wss://<base-url-authority>/mqtt` — where
    /// the authority is the relay's device domain for this Gateway.
    Wss { url: String },
}

impl MqttEndpoint {
    /// Human-readable endpoint for logs and error messages.
    pub fn display(&self) -> String {
        match self {
            MqttEndpoint::Tcp { host, port } => format!("{host}:{port}"),
            MqttEndpoint::Wss { url } => url.clone(),
        }
    }
}

/// Fresh-credential source for MQTT soft-restarts: returns the current
/// `(username, password)` pair, or `None` to keep the previous options.
pub type MqttCredentialRefresher = Arc<dyn Fn() -> Option<(String, String)> + Send + Sync>;

/// MQTT CONNECT credentials with an optional renewal source.
///
/// Relay mode MUST supply the [`MqttCredentials::refresher`]: the
/// password is the logged-in account's access token, which expires
/// after 15 minutes. MQTT 3.1.1 never re-authenticates a live
/// connection, but every (re)connect re-sends the password — and the
/// strict remote listener drops a bad-credential connection WITHOUT
/// CONNACK (rumqttd `InvalidAuth` behavior) — so a stale token would
/// turn every post-expiry reconnect into an endless silent retry loop.
/// The refresher feeds the shared client's soft-restart path
/// (`MqttClientHandler::on_soft_restart`), which is the only place a
/// fresh `MqttOptions` password is ever applied.
#[derive(Clone)]
pub struct MqttCredentials {
    /// Username for the initial CONNECT (relay mode: the account name,
    /// i.e. the token's `sub`; the strict listener ignores the username
    /// field but it keeps broker logs readable).
    pub username: String,
    /// Password for the initial CONNECT (relay mode: the access token).
    pub password: String,
    /// Fresh `(username, password)` for soft-restarts. `None` keeps the
    /// initial pair verbatim (correct for local/LAN mode, where the
    /// broker credential — when present at all — is a long-lived
    /// node token from `/api/status`).
    pub refresher: Option<MqttCredentialRefresher>,
}

/// MQTT QoS level (mirrors the Gateway's).
#[derive(Debug, Clone, Copy)]
pub enum MqttQoS {
    AtMostOnce,
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

/// Topic filter an MQTT message arrived on, plus the raw payload bytes.
#[derive(Debug, Clone)]
pub struct MqttMessage {
    pub topic: String,
    pub payload: Vec<u8>,
}

/// MQTT connection status observed from the broker eventloop.
///
/// Per ADR-036 the Rust side is the source-of-truth for connection state:
/// the Desktop `chatStore` only consumes these transitions via the
/// `mqtt-status` Tauri event and never attempts to mutate the underlying
/// `rumqttc::AsyncClient` itself. Reconnection is owned by the shared
/// [`MqttClient`]'s built-in retry; this enum only reports the
/// externally-observable state changes.
#[derive(Debug, Clone)]
pub enum MqttStatus {
    /// The client is attempting to establish a connection (initial
    /// connect or after a soft-restart / force-reconnect). The frontend
    /// uses this to avoid flashing the disconnected banner while the
    /// client is actively trying to connect.
    Connecting,
    /// Broker confirmed the connection (CONNACK received). Also fired
    /// after a successful automatic reconnect following a disconnect.
    Connected,
    /// Connection was lost and the client is retrying with backoff.
    /// `reason` explains why the connection was lost.
    Reconnecting { reason: String },
}

/// All topic filters that must be re-subscribed on every ConnAck
/// (initial connect + automatic reconnects). With
/// `clean_session = true` the broker drops all subscriptions on
/// disconnect, so **every** active subscription must be listed here.
///
/// ADR-039 P2: mirrors the Runtime's `run_bootstrap` subscribe steps.
///
/// CRITICAL: `messages/#` MUST be in this list. Without it, any
/// reconnect causes the Desktop to silently lose all session message
/// events (stream_delta, record_complete, session_state_changed,
/// done, stopped, context_usage, todo_updated) — the agent keeps
/// running but the frontend appears frozen / "connecting".
pub const ALL_TOPIC_FILTERS: &[(&str, MqttQoS)] = &[
    // ── Lifecycle topics ──
    ("acowork/agents/+/status", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/meta", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/config", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/sessions/created", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/sessions/deleted", MqttQoS::AtLeastOnce),
    // ── ADR-059: Gateway bootstrap snapshot ──
    // Retained `BootstrapState` proto (QoS 1). Re-delivered on every
    // (re)connect so the Desktop always converges on the current
    // instance_id / version / phase even after a Gateway restart.
    ("acowork/global/bootstrap", MqttQoS::AtLeastOnce),
    // ── Inventory-change signal ──
    // Gateway emits a non-retained timestamp message here whenever its
    // aggregated `installed_agents` table mutates (a Node finishes an
    // install/uninstall, or a Node replays its retained inventory on
    // reconnect). Subscribers (AgentList sidebar) treat it as "refresh
    // now" and refetch `GET /api/agents`, which combines
    // `installed_agents` with the MQTT AgentRegistry's liveness verdict —
    // the signal carries the change hint, not the data.
    //
    // The topic lives outside `acowork/global/` on purpose: every Runtime
    // decodes that prefix as a protobuf `DataEnvelope`, so a non-envelope
    // payload there would log a decode failure in each Runtime. Not
    // retained either: the signal only reaches a subscriber that is
    // already connected, and changes missed while disconnected are caught
    // by the sidebar's refetch on each MQTT connect edge.
    ("acowork/desktop/inventory", MqttQoS::AtLeastOnce),
    // ── User-profile change signal (ADR-084 §决策 4b) ──
    // The user service publishes a non-retained version-stamped message
    // here after **every** account/profile mutation — including every
    // avatar write (`PUT /api/user/avatar-config`, avatar delete, and the
    // `sync_profiles` tail on every `PUT /api/users/{id}`). The Gateway
    // subscribes to it to re-pull `GET /internal/user-profiles`; the
    // Desktop subscribes for the sidebar, so a user who changes their
    // avatar gets it repainted without a refetch-on-every-render or a
    // polling interval.
    //
    // Same shape as the inventory signal above, same reasons: outside
    // `acowork/global/` (every Runtime decodes that prefix as a
    // `DataEnvelope`, so a bare integer payload there would log a decode
    // failure per Runtime), and non-retained because it is a change
    // *event* — the catch-up for anything missed while disconnected is
    // the subscriber's refetch on each MQTT connect edge.
    ("acowork/user/profiles/changed", MqttQoS::AtLeastOnce),
    // ADR-043: Retained per-session config + state. Runtime publishes
    // config (title/model/provider/workspace/reasoning_effort/temperature)
    // and state (status/message_count/tokens/ratio/context_usage) on
    // separate retained topics. Broker stores the last value, so
    // (re)connect immediately receives the current state.
    ("acowork/agents/+/sessions/+/config", MqttQoS::AtLeastOnce),
    ("acowork/agents/+/sessions/+/state", MqttQoS::AtLeastOnce),
    ("acowork/sidecar/+/status", MqttQoS::AtLeastOnce),
    // ── Session message events ──
    // ADR-033: Subscribe to all session message topics (streaming chunks,
    // context_usage, session_state_changed, etc.) so the frontend receives
    // real-time session events across all sessions.
    // QoS 1 is mandatory: the Runtime publishes stream_delta and
    // record_complete at QoS 1; subscribing at QoS 0 would force the
    // broker to downgrade delivery, losing end-to-end ordering.
    ("acowork/agents/+/sessions/+/messages/#", MqttQoS::AtLeastOnce),
    // ── Debug protocol events (ADR-048 D6) ──
    // Runtime publishes DevMode debug events (onStep / onContextBuilt /
    // onStateChange) on `acowork/agents/{id}/debug/events/{type}`.
    // Zero traffic outside DevMode, so subscribing unconditionally is
    // free. QoS 0 matches the publisher: events are fire-and-forget and
    // the DevMode panel re-syncs via `GET /api/debug/state` after a
    // reconnect.
    ("acowork/agents/+/debug/events/#", MqttQoS::AtMostOnce),
    // ── Workspace FS change events (ADR-058) ──
    // Runtime publishes aggregated workspace file changes on
    // `acowork/agents/{id}/workspaces/{wid}/fs-changed`. QoS 1 is
    // mandatory (same reason as messages/#: a lost event desyncs the
    // Desktop FileTree until the reconnect full-sync fallback fires).
    ("acowork/agents/+/workspaces/+/fs-changed", MqttQoS::AtLeastOnce),
    // ── Git state changed (ADR-078 follow-up) ──
    // Runtime publishes a bare "re-read git status" nudge on
    // `acowork/agents/{id}/workspaces/{wid}/git-changed` after a shell
    // tool call that may have mutated index/HEAD. This signal is NOT
    // derivable from fs-changed: a `git commit` writes only inside the
    // gitdir, which the workspace PollWatcher never observes (and in a
    // worktree checkout that directory is outside the workspace
    // entirely). QoS 1 for the same reason as fs-changed — a lost nudge
    // leaves a stale Git Status Bar, with panel-expand as the fallback.
    ("acowork/agents/+/workspaces/+/git-changed", MqttQoS::AtLeastOnce),
    // ── Doc library tree changes ──
    // acowork-doc publishes after every structural mutation on
    // `acowork/doc/tree/changed` (QoS 1, non-retained); the DocTreeStore
    // force-refreshes its expanded layers on receipt. A lost event is
    // covered by the 30s poll + reconnect full-sync fallback.
    ("acowork/doc/tree/changed", MqttQoS::AtLeastOnce),
    // ── User chat wake-up signal (design doc 25 §8, ADR-076 §决策 8) ──
    // `acowork-user` publishes an EMPTY, non-retained doorbell on
    // `acowork/users/{user_id}/chat/changed` whenever a thread changes — a
    // message landed, or a read cleared a badge. Keyed by account alone: the
    // embedded broker has no subscription ACL, so the payload carries nothing
    // that could name a peer, and the thread never appears in the topic either
    // (`chat_id` is literally the two account ids joined).
    //
    // A wildcard rather than the signed-in account's own topic: this table is
    // compile-time and re-subscribed on every ConnAck, while the account can
    // change without a reconnect, so the filter stays static and the webview
    // drops signals that belong to someone else (it is the side that knows
    // `account.user_id`). That leaks nothing new: a Desktop is an
    // authenticated client that may already list every account id through
    // `GET /api/users/directory`.
    ("acowork/users/+/chat/changed", MqttQoS::AtLeastOnce),
];

/// Desktop entity handler for the shared [`MqttClient`] (ADR-065 Step 4).
///
/// Implements the Desktop subscriber's per-process differences: incoming
/// message routing, persistent-topic re-subscription on every ConnAck,
/// and `MqttStatus` surfacing for the `mqtt-status` Tauri event.
///
/// ADR-065 §5.6: this is the **only** error-classification surface for
/// the Desktop. The shared `From<&ConnectionError>` (lives in
/// `acowork-mqtt-session::err_class`) handles the MqttState/Io unwrap;
/// this handler consumes the classified [`ErrClass`] only to forward a
/// human-readable reason to the frontend. No private error adapter is
/// allowed (ADR-065 §7 #1 CI red line).
struct DesktopHandler {
    on_message: Arc<dyn Fn(MqttMessage) + Send + Sync>,
    on_status: Arc<dyn Fn(MqttStatus) + Send + Sync>,
    /// Fresh CONNECT credentials for soft-restarts (relay mode's
    /// expiring access token — see [`MqttCredentials`]).
    credentials_refresher: Option<MqttCredentialRefresher>,
}

#[async_trait]
impl MqttClientHandler for DesktopHandler {
    async fn on_publish(&self, topic: &str, payload: &[u8]) {
        (self.on_message)(MqttMessage {
            topic: topic.to_string(),
            payload: payload.to_vec(),
        });
    }

    async fn on_connack(&self, client: &AsyncClient) -> Result<(), String> {
        // Bridge to the `mqtt-status` Tauri event BEFORE re-subscribing
        // so the frontend learns of the new connection as soon as the
        // broker confirms it.
        (self.on_status)(MqttStatus::Connected);

        // ADR-039 P2: re-subscribe ALL topics on every (re)connect.
        // With clean_session = true the broker drops all subscriptions on
        // disconnect, so without this the Desktop would silently lose
        // agent status/meta/config AND session message events
        // (stream_delta, record_complete, etc.) after a reconnect.
        // CRITICAL: `messages/#` must be in ALL_TOPIC_FILTERS — see the
        // doc comment on the const.
        for (filter, qos) in ALL_TOPIC_FILTERS {
            if let Err(e) = client.subscribe(*filter, (*qos).into()).await {
                tracing::warn!(filter, error = %e, "Desktop MQTT resubscribe failed");
            }
        }
        tracing::info!(
            "Desktop MQTT topics re-subscribed ({} filters)",
            ALL_TOPIC_FILTERS.len()
        );
        Ok(())
    }

    async fn on_disconnect(&self, _client: &AsyncClient) {
        tracing::warn!(
            "DesktopHandler::on_disconnect fired — about to emit mqtt-status \
             Reconnecting event with hardcoded reason \"broker sent DISCONNECT\". \
             Frontend chatStore will set effectiveConnection = reconnecting."
        );
        (self.on_status)(MqttStatus::Reconnecting {
            reason: "broker sent DISCONNECT".into(),
        });
    }

    async fn on_error(&self, _client: &AsyncClient, _class: ErrClass, error: &str) {
        tracing::warn!(
            error = %error,
            err_class = ?_class,
            "DesktopHandler::on_error fired — about to emit mqtt-status Reconnecting event."
        );
        (self.on_status)(MqttStatus::Reconnecting {
            reason: format!("eventloop error: {error}"),
        });
    }

    async fn on_soft_restart(&self) -> Option<(String, String)> {
        tracing::warn!(
            "DesktopHandler::on_soft_restart fired — about to emit mqtt-status \
             Connecting event. This is the soft-restart trigger."
        );
        // Surfacing `Connecting` here mirrors the Desktop's pre-Step-4
        // behaviour, where the inline poll task fired
        // `on_status(MqttStatus::Connecting)` immediately before
        // recreating the EventLoop + AsyncClient.
        (self.on_status)(MqttStatus::Connecting);
        // Relay mode: swap in the current access token. Returning None
        // (local/LAN mode, or no session) keeps the original options.
        self.credentials_refresher.as_ref().and_then(|f| f())
    }
}

/// The Desktop's MQTT client.
///
/// ADR-065 Step 4: this is a thin wrapper around
/// [`acowork_mqtt_session::MqttClient<DesktopHandler>`]. The poll loop,
/// error classification, exponential backoff, soft-restart, watchdog
/// and wake recovery all live in the shared crate. The wrapper only
/// carries the entity differences: client_id format, `MqttStatus` →
/// Tauri event bridge, persistent topic filter list (in `on_connack`),
/// and `ControlCommand` protobuf encoding for the control topic.
///
/// Thread-safe: cheap `Clone` (all fields are `Arc`-shared with the
/// poll task). Held inside a `tokio::sync::Mutex` by the Tauri state
/// so command callers serialise their access to the inner handle.
#[derive(Clone)]
pub struct DesktopMqttClient {
    inner: MqttClient<DesktopHandler>,
}

/// Thread-safe shared DesktopMqttClient.
pub type SharedDesktopMqttClient = Arc<Mutex<DesktopMqttClient>>;

impl DesktopMqttClient {
    /// Connect to the MQTT broker and start the event loop.
    ///
    /// Returns IMMEDIATELY after spawning the polling task — does NOT
    /// wait for the initial CONNACK. Connection status (Connected /
    /// Connecting / Reconnecting) is delivered through the `on_status`
    /// callback asynchronously as the shared `MqttClient` observes
    /// CONNACK / DISCONNECT / network errors.
    ///
    /// This is the correct shape for an event-driven MQTT client: the
    /// caller doesn't block, doesn't need to manage a "still
    /// connecting" intermediate state, and the consumer (Tauri layer)
    /// is expected to expose a synchronous `get_mqtt_status` query
    /// alongside the `mqtt-status` event so the frontend can fetch the
    /// current state on startup without racing the listener
    /// registration.
    ///
    /// `on_message` — called for every received MQTT Publish.
    /// `on_status`  — ADR-036: called on every state transition
    ///                 (CONNACK → Connected; broker DISCONNECT /
    ///                 eventloop error → Reconnecting; force-restart /
    ///                 soft-restart → Connecting). May fire multiple
    ///                 times over the lifetime of the client (initial
    ///                 connect, reconnects, outages); the consumer
    ///                 treats it as idempotent.
    ///
    /// `endpoint`   — transport + address of the broker (TCP for
    ///                 local/LAN, WSS for relay — see [`MqttEndpoint`]).
    /// `user_id`    — account/user name encoded into the client_id
    ///                 (`user:{user_id}:desktop:{pid}`). Relay mode
    ///                 derives it from the access token's `sub` so the
    ///                 strict remote listener's cross-check passes.
    /// `credentials`— optional CONNECT credentials; relay mode passes
    ///                 the access token WITH a refresher so post-expiry
    ///                 reconnects re-authenticate (see [`MqttCredentials`]).
    pub async fn connect<F, G>(
        endpoint: MqttEndpoint,
        user_id: &str,
        credentials: Option<MqttCredentials>,
        on_message: F,
        on_status: G,
    ) -> Result<Self, String>
    where
        F: Fn(MqttMessage) + Send + Sync + 'static,
        G: Fn(MqttStatus) + Send + Sync + 'static,
    {
        let pid = std::process::id();
        let client_id = format!("user:{}:desktop:{}", user_id, pid);

        // ADR-039: align outgoing packet size with the broker's
        // `max_payload_size` (`GATEWAY_MQTT_MAX_PACKET_SIZE`). Without
        // this, large Desktop publish packets (e.g. protobuf-wrapped
        // ControlCommand payloads with embedded `config_json`) would
        // hit rumqttc's default 10 KB outgoing limit and trigger
        // `OutgoingPacketTooLarge`, which the broker translates into
        // `connection closed by peer`.
        let pkt_size = defaults::GATEWAY_MQTT_MAX_PACKET_SIZE;

        // Map the endpoint onto the shared config. For WSS the URL
        // rides inside the transport (rumqttc expects it in the
        // MqttOptions "host" slot — `acowork-mqtt-session` centralizes
        // that mapping in `build_mqtt_options`), so host/port are inert
        // placeholders for that transport.
        let (host, port, transport) = match &endpoint {
            MqttEndpoint::Tcp { host, port } => (host.clone(), *port, MqttTransport::Tcp),
            MqttEndpoint::Wss { url } => {
                (String::new(), 0, MqttTransport::Wss { url: url.clone() })
            }
        };

        let credentials_refresher = credentials.as_ref().and_then(|c| c.refresher.clone());
        let config = MqttClientConfig {
            client_id: client_id.clone(),
            host,
            port,
            transport,
            credentials: credentials.map(|c| (c.username, c.password)),
            last_will: None,
            max_packet_size: pkt_size,
            queue_capacity: 100,
        };

        let handler = DesktopHandler {
            on_message: Arc::new(on_message),
            on_status: Arc::new(on_status),
            credentials_refresher,
        };

        tracing::info!(
            endpoint = %endpoint.display(),
            client_id = %client_id,
            "Desktop MQTT client creating via shared MqttClient (ADR-065 Step 4)"
        );

        // ADR-065 Step 4: the shared MqttClient owns the entire poll
        // loop. We pass `None` for `on_state_change` because the
        // Desktop handler surfaces status via the explicit
        // `MqttClientHandler` callbacks (on_connack, on_disconnect,
        // on_error, on_soft_restart) — those are the *only* sources of
        // `MqttStatus` transitions, matching the pre-Step-4 inline
        // poll-task behaviour exactly.
        let inner = MqttClient::connect(config, handler, None)
            .await
            .map_err(|e| e.to_string())?;

        Ok(Self { inner })
    }

    /// Returns the current [`SessionState`] from the shared watch
    /// channel. The frontend's `get_mqtt_status` Tauri command reads
    /// this synchronously.
    pub fn session_state(&self) -> SessionState {
        self.inner.current_state()
    }

    /// Subscribe to an additional topic filter (not in `ALL_TOPIC_FILTERS`).
    ///
    /// ADR-065: persistent subscriptions belong in `ALL_TOPIC_FILTERS`
    /// and are re-applied on every ConnAck by `DesktopHandler::on_connack`.
    /// This escape hatch exists for per-session dynamic subscriptions;
    /// with the current Desktop design (subscribe to everything on
    /// connect) it is unused by production code paths but kept for
    /// parity with the pre-Step-4 `subscribe` helper.
    #[allow(dead_code)]
    pub async fn subscribe(&self, filter: &str, qos: MqttQoS) -> Result<(), String> {
        self.inner
            .shared_handle()
            .lock()
            .await
            .subscribe(filter, qos.into())
            .await
            .map_err(|e| format!("subscribe '{filter}': {e}"))
    }

    /// Force a soft-restart of the MQTT event loop (deterministic
    /// recovery after a system wake).
    ///
    /// Synchronously resets the session state to `Connecting` — so
    /// `wait_for_connected` can never read the stale pre-sleep
    /// `Connected` value — and requests a fresh `AsyncClient` +
    /// `EventLoop` pair. The OS tears down TCP sockets during sleep,
    /// so the old EventLoop is unusable by definition: rebuild
    /// immediately instead of waiting for rumqttc to classify the
    /// failure.
    pub fn recover_after_wake(&self) {
        self.inner.reset_to_connecting();
    }

    /// Signals the background poll task to drop the current `EventLoop`
    /// and create a fresh `AsyncClient` + `EventLoop` pair. This is
    /// the same recovery path as the automatic 3-fatal-error
    /// soft-restart, but triggered externally by the user via the
    /// `force_reconnect_mqtt` Tauri command.
    ///
    /// Use this when the MQTT connection appears stuck (e.g. status
    /// shows "Reconnecting" for an extended period, or messages stop
    /// arriving despite the broker being healthy).
    pub fn force_reconnect(&self) {
        self.recover_after_wake();
    }

    /// Wait for the MQTT client to reach `Connected` state.
    ///
    /// Returns `true` if connected within the timeout, `false`
    /// otherwise. Call this after `force_reconnect()` to ensure the
    /// frontend loads with a live connection rather than racing
    /// against the reconnection.
    pub async fn wait_for_connected(&self, timeout: Duration) -> bool {
        self.inner.wait_for_connected(timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_desktop_mqtt_client_connects() {
        // This test requires the Gateway broker to be running.
        // In CI, skip if the broker is not available.
        let endpoint = MqttEndpoint::Tcp {
            host: "127.0.0.1".to_string(),
            port: 19875,
        };
        let client = DesktopMqttClient::connect(
            endpoint,
            "test-user",
            None,
            |_msg| {
                // no-op callback
            },
            |_status| {
                // no-op status callback (ADR-036)
            },
        )
        .await;

        match client {
            Ok(client) => {
                // Subscriptions are now automatic via on_connack. Wait
                // briefly for the first ConnAck so the test exercises
                // the same path as a real connect.
                let _ = client.wait_for_connected(Duration::from_secs(2)).await;
                drop(client);
            }
            Err(e) => {
                eprintln!("Skipping test: broker not available ({})", e);
                // Don't fail — the test environment may not have a broker running
            }
        }
    }
}
