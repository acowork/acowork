//! Runtime MQTT client (ADR-033 Phase 2).
//!
//! Connects to the Gateway's embedded broker with:
//! - `client_id: "agent:{agent_id}"`
//! - Last Will: `acowork/agents/{id}/status` = `DataEnvelope{AgentStatus{online=false}}` (Retained, QoS 1)
//!
//! On connect, publishes:
//! - `acowork/agents/{id}/status` = `DataEnvelope{AgentStatus{online=true}}` (Retained)
//! - `acowork/agents/{id}/meta` (Retained) — AgentMeta protobuf
//! - `acowork/agents/{id}/config` (Retained) — AgentConfig protobuf
//!
//! Subscribes to:
//! - `acowork/global/#` — global resource available state
//! - `acowork/agents/{id}/sessions/control/#` — control commands from Desktop
//!
//! See `docs/zh/protocols/mqtt.md` §5.1 (startup sequence) and §8.1 (Will Message).

use std::sync::Arc;

use prost::Message as _;
use rumqttc::{AsyncClient, LastWill, QoS};
use tokio::sync::{Mutex, oneshot};

use acowork_core::defaults;
use acowork_core::mqtt_proto::{
    AgentConfig, AgentLifecycleState, AgentMeta, AgentStatus, AskQuestionPayload, ChunkPayload,
    CompactionCancelledPayload, CompactingPayload,
    CompactionCancelReason as ProtoCompactionCancelReason, ContextUsagePayload, DataEnvelope,
    DonePayload, ErrorPayload, IterationLimitPausedPayload, LoopDetectedPausedPayload,
    McpTransport as ProtoMcpTransport, NewDataAvailablePayload, NodeInfo, RecordCompletePayload,
    SessionMessage, StoppedPayload, StreamDeltaPayload, StreamLine, TodoUpdatedPayload,
    ToolApprovalNeededPayload, data_envelope, session_message,
};
use acowork_mqtt_session::{
    ErrClass, MqttClient, MqttClientConfig, MqttClientHandler, SessionState, SessionStateRx,
};

use crate::mqtt::available_cache::SharedAvailableCache;
use acowork_core::protocol::McpTransportDef;

/// ADR-085 invariant 3: the lifecycle only moves forward (or re-stamps
/// the current value). `cur == 0` means "never stamped" (fresh process,
/// before the first connect) and accepts any target. FAILED is the
/// highest value, so the guard never blocks it — and once FAILED is
/// stamped, nothing below it can be published again.
pub(crate) fn lifecycle_regression_rejected(cur: u8, target: u8) -> bool {
    cur != 0 && target < cur
}

/// Encode an `AgentStatus` envelope into bytes for the
/// `acowork/agents/{id}/status` topic.
///
/// The Runtime publishes a fresh envelope on connect (online=true),
/// re-publishes every 5 s as a presence heartbeat, and uses the same
/// encoding for the Last Will (online=false). Callers that only need
/// the `online` flag and the `instance_id`/`node_id` location should
/// prefer this helper so the field set stays in sync across the three
/// publish sites.
///
/// ADR-085: `state` carries the capability progression (replacing the
/// retired plaintext `agents/{id}/ready` topic) and `detail` the
/// failure reason when `state == FAILED`. Invariant 6: senders passing
/// `online == false` must pass `AgentLifecycleState::Offline`.
pub(crate) fn encode_agent_status_payload(
    agent_id: &str,
    instance_id: &str,
    node_id: &str,
    online: bool,
    state: AgentLifecycleState,
    detail: &str,
) -> Vec<u8> {
    let status = AgentStatus {
        agent_id: agent_id.to_string(),
        online,
        instance_id: instance_id.to_string(),
        node_id: node_id.to_string(),
        state: state as i32,
        detail: detail.to_string(),
    };
    prost::Message::encode_to_vec(&DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::AgentStatus(status)),
    })
}

/// Convert a wire-format [`ProtoMcpTransport`] (from `McpRef` over MQTT)
/// to the on-disk [`McpTransportDef`] used by `agent_mcp.json::catalog`.
///
/// Inverse of `gateway::mqtt::global_resources_publisher::map_mcp_transport`.
/// The `Unspecified` variant is mapped to `Stdio` for backward
/// compatibility with hand-written catalog entries that omitted the
/// field; production data always carries an explicit transport.
fn mcp_transport_to_def(t: i32) -> McpTransportDef {
    match ProtoMcpTransport::try_from(t) {
        Ok(ProtoMcpTransport::Stdio) => McpTransportDef::Stdio,
        Ok(ProtoMcpTransport::Http) => McpTransportDef::Http,
        Ok(ProtoMcpTransport::Sse) => McpTransportDef::Sse,
        // Unspecified → Stdio fallback (matches the pre-proto era where
        // the only transport existed). Avoids panicking on a malformed
        // payload; a downgrade-to-stdio MCP will simply fail to spawn
        // when the runtime tries to connect to it.
        Ok(ProtoMcpTransport::Unspecified) | Err(_) => McpTransportDef::Stdio,
    }
}

/// Error type for Runtime MQTT client operations.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeMqttClientError {
    #[error("MQTT connection error: {0}")]
    Connection(String),
    #[error("MQTT publish error: {0}")]
    Publish(String),
    #[error("MQTT subscribe error: {0}")]
    Subscribe(String),
}

/// QoS level wrapper (mirrors the Gateway's MqttQoS).
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

/// Provider list update pushed from MQTT poll loop to SessionManager.
///
/// Carries both the provider metadata list (for `global_provider_list`)
/// and the decrypted API keys (for `provider_key_vault`).
#[derive(Debug, Clone)]
pub struct ProviderUpdate {
    pub provider_list: Vec<acowork_core::protocol::ProviderListItem>,
    pub provider_list_version: u64,
    pub provider_key_vault: Vec<acowork_core::protocol::ProviderKeyEntry>,
    /// ADR-056: Ordered global compact-model candidate list forwarded from
    /// `AvailableProviders.default_compact_models` (falls back to the
    /// deprecated single field when the list is absent). Empty = no global
    /// override.
    pub default_compact_models: Vec<acowork_core::protocol::CompactModelRef>,
}

/// Embedding model update pushed from the MQTT poll loop to SessionManager.
///
/// Carried by `acowork/global/embedding_models` (AvailableEmbeddingModels).
/// This is the MQTT-era replacement for the old gRPC `SidecarEndpointUpdate`
/// (ADR-033): it lets the Runtime rebuild its embedding provider in-place
/// when the Gateway (re)starts the embed sidecar or switches the active
/// model — including the boot-time case where the Runtime started before
/// the embed service was ready.
#[derive(Debug, Clone)]
pub struct EmbeddingUpdate {
    pub endpoint: String,
    pub model_id: String,
    pub dimension: usize,
    pub provider_id: Option<String>,
    pub api_key: Option<String>,
}

/// Node-local LSP relay state pushed from the MQTT poll loop to
/// SessionManager (ADR-055 §6.7, Phase 4).
///
/// `endpoint: Some(..)` → the node's LSP relay is ready; the
/// SessionManager registers the `codebase` tool. `None` → the relay is
/// unavailable (or the node cleared its retained state); the tool is
/// unregistered.
#[derive(Debug, Clone)]
pub struct LspRelayUpdate {
    pub endpoint: Option<String>,
}

/// Decode a retained `acowork/nodes/{node_id}/lsps` payload
/// (`DataEnvelope<AvailableLsps>`) into an [`LspRelayUpdate`].
///
/// An empty payload (node cleared the topic) and any decode failure
/// both collapse to `endpoint: None` — the unavailable state.
pub(crate) fn decode_lsps_payload(payload: &[u8]) -> LspRelayUpdate {
    // Empty retained payload = the node cleared its lsps state
    // (shutdown / leave) — the relay is no longer available.
    if payload.is_empty() {
        return LspRelayUpdate { endpoint: None };
    }
    let envelope = match DataEnvelope::decode(payload) {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!(error = %e, "node lsps payload is not a valid DataEnvelope");
            return LspRelayUpdate { endpoint: None };
        }
    };
    match envelope.payload {
        Some(data_envelope::Payload::AvailableLsps(lsps))
            if lsps.ready && !lsps.endpoint.is_empty() =>
        {
            LspRelayUpdate {
                endpoint: Some(lsps.endpoint),
            }
        }
        _ => LspRelayUpdate { endpoint: None },
    }
}

/// Decode the node's reverse-proxy base URL from a retained
/// `acowork/nodes/{node_id}/info` payload (`DataEnvelope<NodeInfo>`).
///
/// Returns `None` on any decode failure, a non-NodeInfo payload, or an
/// empty `http_endpoint` (the node cleared its state). The base is the
/// `http://{live_host}:{port}` part — the Runtime appends
/// `/agents/{agent_id}` when re-publishing its own `http_endpoint`.
pub(crate) fn decode_node_proxy_base(payload: &[u8]) -> Option<String> {
    if payload.is_empty() {
        return None;
    }
    let envelope = match DataEnvelope::decode(payload) {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!(error = %e, "node info payload is not a valid DataEnvelope");
            return None;
        }
    };
    match envelope.payload {
        Some(data_envelope::Payload::NodeInfo(NodeInfo { http_endpoint, .. }))
            if !http_endpoint.is_empty() =>
        {
            Some(http_endpoint)
        }
        _ => None,
    }
}

// ── MQTT protobuf -> domain mapping helpers ─────────────────────────────
//
// Extracted from the inline poll-loop closures so they are independently
// unit-testable and the poll loop stays readable. These are the Runtime-side


/// Map MQTT `ProviderRef` list to domain `ProviderListItem` list.
///
/// Preserves `protocol_type` from the protobuf `LlmProtocol` field
/// (previously hardcoded to `ProtocolType::OpenAI` - see C1 fix).
fn map_provider_refs_to_list_items(
    refs: &[acowork_core::mqtt_proto::ProviderRef],
) -> Vec<acowork_core::protocol::ProviderListItem> {
    refs.iter()
        .map(|pr| acowork_core::protocol::ProviderListItem {
            id: pr.id.clone(),
            base_url: pr.base_url.clone(),
            protocol_type: acowork_core::protocol::llm_protocol_to_protocol_type(pr.protocol_type),
            models: pr
                .models
                .iter()
                .map(|m| acowork_core::protocol::ProviderModelEntry {
                    id: m.id.clone(),
                    capabilities: acowork_core::protocol::ModelCapabilitiesInfo {
                        context_window: m
                            .capabilities
                            .as_ref()
                            .map(|c| c.context_window)
                            .unwrap_or(128_000),
                        max_output_tokens: m
                            .capabilities
                            .as_ref()
                            .map(|c| c.max_output_tokens)
                            .unwrap_or(16_384),
                        max_input_tokens: None,
                        supports_tool_calling: true,
                        supports_reasoning: m
                            .capabilities
                            .as_ref()
                            .and_then(|c| c.supports_reasoning),
                        supports_attachment: None,
                        supports_temperature: None,
                        cost: None,
                        modalities: Some(acowork_core::protocol::ModelModalities {
                            input: m
                                .capabilities
                                .as_ref()
                                .map(|c| c.input_modalities.clone())
                                .unwrap_or_default(),
                            output: m
                                .capabilities
                                .as_ref()
                                .map(|c| c.output_modalities.clone())
                                .unwrap_or_default(),
                        }),
                        name: None,
                        family: None,
                        knowledge_cutoff: None,
                        default_reasoning_effort: m
                            .capabilities
                            .as_ref()
                            .and_then(|c| c.default_reasoning_effort.clone()),
                        thinking_mode: None,
                    },
                    max_output_tokens_limit: m.max_output_tokens_limit,
                    // ADR-103 D2: model-level effective protocol / base_url
                    // override as decided by the Gateway. Absent = inherit
                    // the provider-level entry. Probe verdicts are not
                    // delivered (§3 red line 2), so `protocol_probe` stays
                    // `None` on the Runtime side.
                    protocol_type: acowork_core::protocol::model_protocol_from_proto(
                        m.protocol_type,
                    ),
                    base_url: m
                        .base_url
                        .clone()
                        .filter(|u| !u.is_empty()),
                    protocol_probe: None,
                })
                .collect(),
            compact_model: if pr.compact_model.is_empty() {
                None
            } else {
                Some(pr.compact_model.clone())
            },
            custom: pr.custom,
        })
        .collect()
}

/// Extract non-empty API keys from MQTT `ProviderRef` list.
fn extract_provider_keys(
    refs: &[acowork_core::mqtt_proto::ProviderRef],
) -> Vec<acowork_core::protocol::ProviderKeyEntry> {
    refs.iter()
        .filter(|pr| !pr.api_key.is_empty())
        .map(|pr| acowork_core::protocol::ProviderKeyEntry {
            provider_id: pr.id.clone(),
            account_id: pr.account_id.clone(),
            api_key: pr.api_key.clone(),
        })
        .collect()
}

/// Configuration for `RuntimeMqttClient::connect`.
///
/// ADR-034 Phase 8: replaces 11 individual parameters.
pub struct MqttConnectConfig<'a> {
    pub host: &'a str,
    pub port: u16,
    pub agent_id: &'a str,
    /// ADR-073: instance identity (UUID v4) used for every
    /// `acowork/agents/{instance_id}/...` topic and the broker
    /// client id. Required — no fallback to `agent_id`.
    pub instance_id: &'a str,
    pub agent_name: &'a str,
    pub agent_version: &'a str,
    pub config_json: &'a str,
    pub available_cache: SharedAvailableCache,
    pub control_tx: tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
    /// ADR-042: Sink for user-profile updates. The MQTT event loop sends
    /// the latest `acowork_core::protocol::UserProfile` here whenever
    /// `acowork/global/user_profile` retained is received (initial snapshot
    /// or hot-push). The receiver (held by `agent_init.rs` → `gateway_loop`)
    /// forwards to `SessionManager::update_user_identity` so all active
    /// sessions pick up the new identity_context.
    ///
    /// Optional: when None, the MQTT event loop still updates
    /// `available_cache` but does not notify SessionManager (suitable for
    /// tests and Standalone mode where there is no SessionManager).
    #[cfg_attr(not(test), allow(dead_code))]
    pub identity_update_tx:
        Option<tokio::sync::mpsc::UnboundedSender<acowork_core::protocol::UserProfile>>,
    /// ADR-040 follow-up: shared MCP config notifier. The MQTT event
    /// loop fires it after persisting a new catalog from
    /// `acowork/global/mcps` so `gateway_loop` reconnects MCP servers
    /// instead of waiting for the next `PUT /mcp-servers` or restart.
    /// Optional: when None, the catalog is still persisted (startup
    /// reads it from disk; hot reload simply doesn't trigger).
    pub mcp_notifier: crate::mcp_notify::McpNotifyRef,
    /// Sink for provider list updates. The MQTT event loop sends
    /// `ProviderUpdate` here whenever `acowork/global/providers` retained
    /// is received. The receiver (held by `agent_init.rs` → `gateway_loop`)
    /// forwards to `SessionManager::update_global_provider_list` so all
    /// sessions pick up the new provider list and API keys.
    ///
    /// Optional: when None, the MQTT event loop still updates
    /// `available_cache` but does not notify SessionManager.
    #[cfg_attr(not(test), allow(dead_code))]
    pub provider_update_tx: Option<tokio::sync::mpsc::UnboundedSender<ProviderUpdate>>,
    /// Sink for embedding-model updates. The MQTT event loop sends
    /// [`EmbeddingUpdate`] here whenever `acowork/global/embedding_models`
    /// retained is received (initial snapshot or hot-push after the embed
    /// sidecar becomes ready / the active model switches). The receiver
    /// (held by `agent_init.rs` → `gateway_loop`) forwards to
    /// `SessionManager::handle_embedding_config_update` so all sessions
    /// rebuild their embedding provider in-place (ADR-033 MQTT replacement
    /// for the removed gRPC `SidecarEndpointUpdate`).
    ///
    /// Optional: when None, the MQTT event loop still updates
    /// `available_cache` but does not notify SessionManager.
    #[cfg_attr(not(test), allow(dead_code))]
    pub embedding_update_tx: Option<tokio::sync::mpsc::UnboundedSender<EmbeddingUpdate>>,
    /// Node id this Runtime belongs to (ADR-055 §6.7, Phase 4). When
    /// set, bootstrap subscribes to `acowork/nodes/{node_id}/lsps` and
    /// the poll loop forwards relay state changes to `lsps_update_tx`.
    /// `None` (standalone / Gateway-spawned) keeps the legacy behavior:
    /// no subscription, no `codebase` tool.
    pub node_id: Option<&'a str>,
    /// Sink for node LSP relay state changes. The MQTT poll loop sends
    /// an [`LspRelayUpdate`] here whenever `acowork/nodes/{id}/lsps`
    /// retained is received. The receiver (held by `agent_init.rs` →
    /// `gateway_loop`) forwards to SessionManager so all sessions pick
    /// up or drop the `codebase` tool.
    ///
    /// Optional: when None, the MQTT event loop drops the update
    /// (suitable for tests and Standalone mode).
    #[cfg_attr(not(test), allow(dead_code))]
    pub lsps_update_tx: Option<tokio::sync::mpsc::UnboundedSender<LspRelayUpdate>>,
    /// Per-agent workspace directory (`work_dir`). Used by the MQTT poll
    /// task to persist `acowork/global/mcps` into `agent_mcp.json::catalog`
    /// so the Tools-panel `PUT /agents/{id}/mcp-servers` validation can
    /// match catalog names against disk (the legacy gRPC path did this in
    /// `handle_agent_hello`; that path was removed in ADR-040, leaving the
    /// catalog sync as MQTT-only — see startup/subsystems.rs §MCP for the
    /// rationale and the on-disk contract).
    pub work_dir: std::path::PathBuf,
    /// ADR-055 Phase 5a: optional broker CONNECT credentials
    /// (`node:{id}` / `agent:{id}` username + node_token). When None,
    /// the client connects anonymously (auth disabled broker).
    pub username: Option<&'a str>,
    pub password: Option<&'a str>,
    /// §6.3.3: sink for the node's reverse-proxy base URL on CHANGE.
    /// The event loop decodes the node's retained `NodeInfo`; when its
    /// `http_endpoint` base differs from the last one, the new base is
    /// pushed here. The owner (agent_init) re-publishes this Runtime's
    /// retained `http_endpoint` so the Gateway keeps routing through
    /// the node after a LAN IP change — no node / Runtime restart.
    ///
    /// Optional: None disables live endpoint re-publication (standalone
    /// Runtimes have no node).
    pub node_proxy_update_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// §6.3.3 / ADR-055 D3: the node reverse-proxy base URL injected
    /// at spawn time (`--http-advertise-endpoint`). Cached in
    /// `BootstrapData` so `run_bootstrap` can re-publish the retained
    /// `http_endpoint` on every (re)connect — a Gateway restart drops
    /// all retained state (in-memory broker), and without this the
    /// Gateway can never route to a Runtime that did not itself
    /// restart. `None` standalone / tests.
    pub http_advertise_endpoint: Option<&'a str>,
    /// Loopback port of this Runtime's HTTP server (`--http-port`).
    /// Used to build the standalone `http://127.0.0.1:{port}`
    /// fallback endpoint on (re)connect when no node base is set.
    pub http_port: Option<u16>,
}

/// Event payload for `publish_tool_approval_needed`.
///
/// ADR-034 Phase 8: replaces 8 individual arguments.
pub struct ToolApprovalNeededEvent<'a> {
    pub session_id: &'a str,
    pub request_id: &'a str,
    pub tool_name: &'a str,
    pub action: &'a str,
    pub risk_level: &'a str,
    pub reason: &'a str,
    pub tool_call_id: &'a str,
    pub approval_timeout_secs: u64,
}

/// The Runtime's MQTT client.
///
/// Wraps `rumqttc::AsyncClient` with:
/// - Last Will for automatic offline detection
/// - Agent lifecycle publishing (status/meta/config)
/// - Global resource subscription → `AvailableResourceCache`
/// - Control command subscription → caller-provided channel
pub struct RuntimeMqttClient {
    /// The unified MQTT client (ADR-065 Step 4). Owns the poll task,
    /// session state, force-restart signal and the entity handler.
    inner: MqttClient<RuntimeHandler>,
    /// Shared client handle, swappable during soft-restart.
    ///
    /// Points at the same `Arc<Mutex<AsyncClient>>` as
    /// `inner.shared_handle()` — the poll task swaps in a fresh
    /// `AsyncClient` when it recreates the `EventLoop`, and all publish
    /// methods obtain a clone via `self.client().await`, ensuring they
    /// always use the current handle.
    shared_client: Arc<Mutex<AsyncClient>>,
    /// The agent_id this client represents (PACKAGE identity).
    agent_id: String,
    /// ADR-073: instance identity (UUID v4) used for topic construction.
    /// Falls back to `agent_id` for standalone / test mode.
    instance_id: String,
    /// Cached inputs needed to re-run `run_bootstrap` on every
    /// (re)connect. See `docs/adr/zh/ADR-039-mqtt-client-lifecycle.md`.
    bootstrap_data: Arc<BootstrapData>,
}

// `Clone` lets the same underlying MQTT client be shared between
// `AgentBootContext::mqtt_client` (consumed by Phase C/D's chunk relay
// and gateway loop) and the HTTP server's late-bind slot (consumed by
// `PUT /agents/{id}/config`). All fields are cheap-to-clone handles —
// the AsyncClient is internally reference-counted by rumqttc and the
// unified client holds a JoinHandle, not owned state.
impl Clone for RuntimeMqttClient {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared_client: Arc::clone(&self.shared_client),
            agent_id: self.agent_id.clone(),
            instance_id: self.instance_id.clone(),
            bootstrap_data: Arc::clone(&self.bootstrap_data),
        }
    }
}

/// Cached inputs needed to re-run bootstrap on every (re)connect.
///
/// Built once in `RuntimeMqttClient::connect` and shared (via
/// `Arc<BootstrapData>`) between the initial bootstrap call inside
/// `connect` and the event-loop task that re-runs bootstrap on every
/// subsequent `ConnAck`. See ADR-039.
struct BootstrapData {
    agent_id: String,
    /// ADR-073: instance identity (UUID v4) for topic construction.
    instance_id: String,
    /// ADR-073: current location (node hosting this Runtime). Empty for
    /// standalone / Gateway-spawned runtimes. Populated from `--node-id`.
    node_id: Option<String>,
    agent_name: String,
    agent_version: String,
    config_json: String,
    status_topic: String,
    meta_topic: String,
    config_topic: String,
    /// Filter passed to `AsyncClient::subscribe` (ends with `#`).
    control_filter: String,
    /// Prefix used to route incoming `control/#` publishes to
    /// `control_tx` (ends with `/`, **no** `#`).
    control_filter_prefix: String,
    /// ADR-055 §6.7 (Phase 4): the node's LSP relay topic
    /// (`acowork/nodes/{node_id}/lsps`). `None` when the Runtime runs
    /// without `--node-id` (standalone / Gateway-spawned).
    node_lsps_topic: Option<String>,
    /// §6.3.3: the node's retained info topic
    /// (`acowork/nodes/{node_id}/info`). `None` standalone. Subscribed
    /// so the Runtime can watch the node's reverse-proxy base URL and
    /// re-publish its own `http_endpoint` when the node changes
    /// network (self-healing without a restart).
    node_info_topic: Option<String>,
    /// ADR-085 D3b: the most recent lifecycle state this process has
    /// published (an `AgentLifecycleState` discriminant; `0` = not yet
    /// initialised). Replaces the retired `ready_topic` + `ready_ever`
    /// pair. `run_bootstrap` re-stamps this value into the retained
    /// `AgentStatus` on every (re)connect — a reconnect is not a
    /// process restart, so the capability state must never regress to
    /// STARTING. Also the input to `publish_lifecycle`'s monotonicity
    /// guard. Atomic — shared through `Arc<BootstrapData>` between the
    /// handler task (bootstrap) and the `publish_lifecycle` caller.
    current_lifecycle: std::sync::atomic::AtomicU8,
    /// ADR-085: the `detail` string that accompanies `current_lifecycle`
    /// (non-empty only for FAILED). Kept so (re)connect re-stamps do
    /// not silently drop the failure reason from the retained envelope.
    lifecycle_detail: std::sync::Mutex<String>,
    /// §6.3.3 / ADR-055 D3: the node reverse-proxy base URL injected
    /// at spawn time (`--http-advertise-endpoint`), and the Runtime's
    /// own loopback HTTP port. `run_bootstrap` re-publishes the
    /// retained `http_endpoint` from these on every (re)connect so a
    /// Gateway that restarts (its embedded broker is in-memory and
    /// loses all retained state) can re-discover this Runtime without
    /// a process restart. `None` for standalone / tests.
    http_advertise_endpoint: Option<String>,
    /// Loopback port of this Runtime's HTTP server (`--http-port`).
    http_port: Option<u16>,
}

/// Runtime entity handler for the shared [`MqttClient`] (ADR-065 Step 4).
///
/// Implements the Runtime's per-process differences: global-resource
/// cache updates + persistence + SessionManager fan-out on incoming
/// publishes, and the ADR-039 bootstrap on every ConnAck. The shared
/// client owns the poll loop, error classification, backoff and
/// soft-restart.
struct RuntimeHandler {
    /// Cached inputs needed to re-run `run_bootstrap` on every
    /// (re)connect.
    bootstrap_data: Arc<BootstrapData>,
    /// Shared global-resource cache (updated on `acowork/global/#`).
    available_cache: SharedAvailableCache,
    /// Sink for control commands (`acowork/agents/{id}/sessions/control/#`).
    control_tx: tokio::sync::mpsc::UnboundedSender<(String, Vec<u8>)>,
    /// Sink for user-profile updates (ADR-042).
    identity_update_tx:
        Option<tokio::sync::mpsc::UnboundedSender<acowork_core::protocol::UserProfile>>,
    /// Shared MCP config notifier — fired after `acowork/global/mcps`
    /// persists a new catalog so gateway_loop hot-reloads MCP servers.
    mcp_notifier: crate::mcp_notify::McpNotifyRef,
    /// Sink for provider list updates.
    provider_update_tx: Option<tokio::sync::mpsc::UnboundedSender<ProviderUpdate>>,
    /// Sink for embedding-model updates.
    embedding_update_tx: Option<tokio::sync::mpsc::UnboundedSender<EmbeddingUpdate>>,
    /// Sink for node LSP relay state changes (ADR-055 §6.7).
    lsps_update_tx: Option<tokio::sync::mpsc::UnboundedSender<LspRelayUpdate>>,
    /// Per-agent workspace directory (persist global catalogs).
    work_dir: std::path::PathBuf,
    /// The agent_id this client represents.
    agent_id: String,
    /// The node's LSP relay topic (ADR-055 §6.7). `None` standalone.
    node_lsps_topic: Option<String>,
    /// The node's retained info topic (§6.3.3). `None` standalone.
    node_info_topic: Option<String>,
    /// §6.3.3: sink for the node's reverse-proxy base URL whenever it
    /// CHANGES (extracted from a fresh NodeInfo). The owner
    /// (`agent_init.rs`) re-publishes this Runtime's retained
    /// `http_endpoint` so the Gateway keeps routing through the node.
    /// `None` standalone / tests.
    node_proxy_update_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Last known node reverse-proxy base URL — dedupes the node's
    /// 60 s heartbeat, which re-delivers an unchanged NodeInfo.
    last_node_proxy_base: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Signals `connect()` when the first ConnAck bootstrap completes.
    first_conn_tx: tokio::sync::Mutex<Option<oneshot::Sender<Result<(), String>>>>,
}

#[async_trait::async_trait]
impl MqttClientHandler for RuntimeHandler {
    async fn on_publish(&self, topic: &str, payload: &[u8]) {
        if topic.starts_with("acowork/global/") {
            let mut cache_write = self.available_cache.write().await;
            cache_write.update_from_mqtt(topic, payload);

            if topic == "acowork/global/bootstrap" {
                // ADR-059 Phase 5.3: the cache layer above already
                // rejected stale retained re-delivery by `instance_id` +
                // `version` and cleared old-generation resource snapshots
                // on a generation switch (cold start, hot restart and
                // reconnect share this one path). Log the currently
                // authoritative snapshot for reconnect / restart
                // diagnostics.
                let snapshot = cache_write.bootstrap_snapshot().cloned();
                drop(cache_write);
                if let Some(bs) = snapshot {
                    tracing::info!(
                        instance_id = %bs.instance_id,
                        version = bs.version,
                        phase = ?acowork_core::mqtt_proto::BootstrapPhase::try_from(bs.phase).ok(),
                        "acowork/global/bootstrap retained received (authoritative Gateway generation)"
                    );
                }
            } else if topic == "acowork/global/user_profile" {
                let profile = cache_write.active_user_profile();
                tracing::debug!(
                    has_profile = profile.is_some(),
                    "acowork/global/user_profile retained received"
                );
                drop(cache_write);
                if let (Some(tx), Some(profile)) = (self.identity_update_tx.as_ref(), profile) {
                    let _ = tx.send(profile);
                }
            } else if topic == "acowork/global/mcps" {
                // ADR-040 follow-up: persist catalog to disk so
                // PUT /agents/{id}/mcp-servers validation can resolve
                // names against merged(). See the long-form comment near
                // `work_dir` for the full rationale.
                let servers = cache_write
                    .mcps
                    .as_ref()
                    .map(|m| m.servers.clone())
                    .unwrap_or_default();
                drop(cache_write);
                // T3-4 + ADR-073: 替换 MCP header 模板占位符。
                //
                // Gateway 注入的 pm/doc MCP 通过
                // `X-MCP-Actor: {instance_id}` 识别调用者
                // （设计 §9.2；ADR-073 §1.3 不变量 1：身份 key 必须是
                // agent_instance_id，不是 agent_id）。Runtime 把 `{instance_id}`
                // 替换为 `self.bootstrap_data.instance_id`，连接路径
                // （startup / config change）读到的是已解析的 header，无需
                // 感知模板。`{agent_id}` 模板为旧兼容保留，env 同样替换以
                // 支持扩展。
                let bootstrap_instance_id = self.bootstrap_data.instance_id.clone();
                let resolve_instance_id =
                    |value: &str| value.replace("{instance_id}", &bootstrap_instance_id);
                let resolve_agent_id = |value: &str| value.replace("{agent_id}", &self.agent_id);
                let defs: Vec<acowork_core::protocol::McpServerConfigDef> = servers
                    .into_iter()
                    .map(|s| acowork_core::protocol::McpServerConfigDef {
                        name: s.name.clone(),
                        display_name: None,
                        transport: mcp_transport_to_def(s.transport),
                        url: if s.url.is_empty() {
                            None
                        } else {
                            Some(s.url.clone())
                        },
                        command: s.command.clone(),
                        args: s.args.clone(),
                        env: s
                            .env
                            .into_iter()
                            .map(|(k, v)| {
                                // ADR-073: `{instance_id}` 是身份 key；保留
                                // `{agent_id}` 兼容老 Gateway 模板（env 非身份字段）。
                                let v = resolve_instance_id(&v);
                                let v = resolve_agent_id(&v);
                                (k, v)
                            })
                            .collect(),
                        headers: s
                            .headers
                            .into_iter()
                            .map(|(k, v)| (k, resolve_instance_id(&v)))
                            .collect(),
                        install: if s.install_spec_json.is_empty() {
                            None
                        } else {
                            match serde_json::from_str::<
                                acowork_core::protocol::McpInstallSpec,
                            >(&s.install_spec_json)
                            {
                                Ok(spec) => Some(spec),
                                Err(e) => {
                                    // ADR-090 D5: without the spec the Agent
                                    // cannot self-install on first enable —
                                    // keep the entry (spawn config may still
                                    // work if the binary is present) but make
                                    // the drop visible.
                                    tracing::warn!(
                                        server = %s.name,
                                        error = %e,
                                        "MCP install_spec_json failed to parse — first-enable install disabled for this server"
                                    );
                                    None
                                }
                            }
                        },
                        required_secrets: vec![],
                        tool_timeout_secs: if s.tool_timeout_secs == 0 {
                            None
                        } else {
                            Some(s.tool_timeout_secs)
                        },
                    })
                    .collect();
                // Hot-reload only when the catalog actually changed:
                // retained `acowork/global/mcps` is redelivered on every
                // MQTT reconnect, and an unconditional notify() would
                // reconnect every MCP server (respawning stdio children)
                // on each broker flap.
                let prev_catalog = crate::agent_config::load_agent_mcp_config(&self.work_dir)
                    .ok()
                    .flatten()
                    .map(|c| c.catalog)
                    .unwrap_or_default();
                let catalog_changed = serde_json::to_string(&prev_catalog)
                    .ok()
                    .and_then(|p| serde_json::to_string(&defs).ok().map(|n| p != n))
                    .unwrap_or(true);
                if let Err(e) =
                    crate::agent_config::save_agent_mcp_config_catalog(&self.work_dir, &defs)
                {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        error = %e,
                        "Failed to persist acowork/global/mcps catalog to agent_mcp.json (PUT /mcp-servers will reject catalog names until retry)"
                    );
                } else {
                    tracing::info!(
                        agent_id = %self.agent_id,
                        catalog_count = defs.len(),
                        "Synced MCP catalog from acowork/global/mcps into agent_mcp.json::catalog"
                    );
                    if catalog_changed
                        && let Some(notifier) = &self.mcp_notifier
                    {
                        notifier.notify();
                    }
                }
            } else if topic == "acowork/global/providers" {
                // Persist provider list to agent_provider.json so the
                // Runtime can load it on restart. API keys are NOT
                // persisted - they stay in available_cache (in-memory
                // only).
                let (provider_list, version, key_vault, default_compact_models) =
                    match cache_write.providers.as_ref() {
                        Some(p) => {
                            let list = map_provider_refs_to_list_items(&p.providers);
                            let keys = extract_provider_keys(&p.providers);
                            // ADR-056: forward the global compact-model
                            // candidate list. Prefer the list field; fall
                            // back to the deprecated single field so an old
                            // Gateway keeps working.
                            let dcm: Vec<acowork_core::protocol::CompactModelRef> = if !p
                                .default_compact_models
                                .is_empty()
                            {
                                p.default_compact_models
                                    .iter()
                                    .map(|r| acowork_core::protocol::CompactModelRef {
                                        provider_id: r.provider_id.clone(),
                                        model_id: r.model_id.clone(),
                                    })
                                    .collect()
                            } else {
                                #[allow(deprecated)]
                                p.default_compact_model
                                    .iter()
                                    .map(|r| acowork_core::protocol::CompactModelRef {
                                        provider_id: r.provider_id.clone(),
                                        model_id: r.model_id.clone(),
                                    })
                                    .collect()
                            };
                            (list, p.version, keys, dcm)
                        }
                        None => (vec![], 0, vec![], vec![]),
                    };
                drop(cache_write);

                // Persist to agent_provider.json (no API keys)
                if let Err(e) = crate::agent_config::save_agent_provider_config_from_available(
                    &self.work_dir,
                    &provider_list,
                    version,
                    &default_compact_models,
                ) {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        error = %e,
                        "Failed to persist acowork/global/providers to agent_provider.json"
                    );
                } else {
                    tracing::info!(
                        agent_id = %self.agent_id,
                        provider_count = provider_list.len(),
                        default_compact_count = default_compact_models.len(),
                        "Synced provider list from acowork/global/providers into agent_provider.json"
                    );
                }

                // Forward to SessionManager via channel
                if let Some(ref tx) = self.provider_update_tx {
                    let update = ProviderUpdate {
                        provider_list,
                        provider_list_version: version,
                        provider_key_vault: key_vault,
                        default_compact_models,
                    };
                    if let Err(e) = tx.send(update) {
                        tracing::warn!(
                            agent_id = %self.agent_id,
                            error = %e,
                            "Failed to send provider update to SessionManager"
                        );
                    }
                }
            } else if topic == "acowork/global/embedding_models" {
                // ADR-033 MQTT replacement for the removed gRPC
                // SidecarEndpointUpdate: forward the active embedding
                // endpoint/model/dim to SessionManager so every session
                // rebuilds its embedding provider in-place. This covers
                // both the boot-time race (the Runtime starts before the
                // embed sidecar is ready) and hot model switches.
                let update = {
                    let models = cache_write.embedding_models.as_ref();
                    match models {
                        Some(m) if !m.endpoint.is_empty() && !m.active_model_id.is_empty() => {
                            Some(EmbeddingUpdate {
                                endpoint: m.endpoint.clone(),
                                model_id: m.active_model_id.clone(),
                                dimension: m.active_dimension as usize,
                                provider_id: if m.active_provider_id.is_empty() {
                                    None
                                } else {
                                    Some(m.active_provider_id.clone())
                                },
                                api_key: if m.active_api_key.is_empty() {
                                    None
                                } else {
                                    Some(m.active_api_key.clone())
                                },
                            })
                        }
                        _ => None,
                    }
                };
                drop(cache_write);
                if let Some(ref tx) = self.embedding_update_tx
                    && let Some(update) = update
                    && let Err(e) = tx.send(update)
                {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        error = %e,
                        "Failed to send embedding update to SessionManager"
                    );
                }
            }
        }

        if topic.starts_with(&self.bootstrap_data.control_filter_prefix) {
            let _ = self.control_tx.send((topic.to_string(), payload.to_vec()));
        } else if let Some(lsps_topic) = self.node_lsps_topic.as_deref()
            && topic == lsps_topic
        {
            // ADR-055 §6.7: the node's LSP relay state changed. Forward
            // to SessionManager so it can register / unregister the
            // codebase tool (retained snapshot or hot-push).
            let update = decode_lsps_payload(payload);
            if let Some(ref tx) = self.lsps_update_tx {
                let _ = tx.send(update);
            }
        } else if let Some(info_topic) = self.node_info_topic.as_deref()
            && topic == info_topic
        {
            // §6.3.3: the node re-published its NodeInfo — initial
            // connect, 60 s heartbeat, or a LAN IP change after a Wi-Fi
            // hotspot switch. Extract the node's CURRENT reverse-proxy
            // base; when it differs from the last known one, forward the
            // new base so agent_init re-publishes our retained
            // `http_endpoint` (`{base}/agents/{id}`). The dedupe guard
            // keeps the node's heartbeat from churning retained
            // publishes on every 60 s tick.
            if let Some(base) = decode_node_proxy_base(payload) {
                let mut last = self
                    .last_node_proxy_base
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if last.as_deref() != Some(base.as_str()) {
                    *last = Some(base.clone());
                    if let Some(ref tx) = self.node_proxy_update_tx
                        && tx.send(base).is_err()
                    {
                        tracing::debug!(
                            agent_id = %self.agent_id,
                            "node proxy update receiver dropped (endpoint re-publish sink closed)"
                        );
                    }
                }
            }
        }
    }

    async fn on_connack(&self, client: &AsyncClient) -> Result<(), String> {
        tracing::info!(
            agent_id = %self.agent_id,
            "Runtime MQTT broker confirmed (re)connection - re-running bootstrap"
        );
        let result = RuntimeMqttClient::run_bootstrap(client, &self.bootstrap_data).await;
        if let Err(ref e) = result {
            let _ = client
                .publish(
                    &self.bootstrap_data.status_topic,
                    QoS::AtLeastOnce,
                    true,
                    "degraded",
                )
                .await;
            tracing::error!(
                agent_id = %self.agent_id,
                error = %e,
                "Runtime MQTT bootstrap after (re)connect failed - agent is degraded"
            );
        }
        if let Some(tx) = self.first_conn_tx.lock().await.take() {
            let _ = tx.send(result.as_ref().map_err(|e| e.to_string()).map(|_| ()));
        }
        result.map_err(|e| e.to_string())
    }

    async fn on_error(&self, _client: &AsyncClient, class: ErrClass, error: &str) {
        tracing::warn!(
            agent_id = %self.agent_id,
            error,
            err_class = class.label(),
            "Runtime MQTT event loop error (backoff handled by shared client)"
        );
    }
}

impl RuntimeMqttClient {
    /// Obtain a clone of the current `AsyncClient`.
    ///
    /// The lock is held only for the duration of the clone (an `Arc`
    /// bump inside rumqttc), so this is safe to call from async code.
    async fn client(&self) -> AsyncClient {
        self.shared_client.lock().await.clone()
    }

    /// Connect to the MQTT broker and perform the Phase 2 startup sequence.
    ///
    /// ADR-034 Phase 8: takes a single `MqttConnectConfig` struct.
    pub async fn connect(cfg: MqttConnectConfig<'_>) -> Result<Self, RuntimeMqttClientError> {
        // ADR-073: the broker client id identifies THIS RUNTIME INSTANCE.
        // Two instances of the same package must never share a client id
        // (the broker would disconnect one); the package `agent_id` is
        // therefore never used here.
        let instance_id = cfg.instance_id;
        let client_id = format!("agent:{}", instance_id);

        // ADR-039: cache every input needed to re-run bootstrap on every
        // (re)connect (status/meta/config publish + persistent subscriptions).
        let bootstrap_data = Arc::new(BootstrapData {
            agent_id: cfg.agent_id.to_string(),
            instance_id: instance_id.to_string(),
            node_id: cfg.node_id.map(|s| s.to_string()),
            agent_name: cfg.agent_name.to_string(),
            agent_version: cfg.agent_version.to_string(),
            config_json: cfg.config_json.to_string(),
            status_topic: format!("acowork/agents/{}/status", instance_id),
            meta_topic: format!("acowork/agents/{}/meta", instance_id),
            config_topic: format!("acowork/agents/{}/config", instance_id),
            control_filter: format!("acowork/agents/{}/sessions/control/#", instance_id),
            control_filter_prefix: format!("acowork/agents/{}/sessions/control/", instance_id),
            node_lsps_topic: cfg.node_id.map(acowork_core::node::node_lsps_topic),
            node_info_topic: cfg.node_id.map(acowork_core::node::node_info_topic),
            current_lifecycle: std::sync::atomic::AtomicU8::new(0),
            lifecycle_detail: std::sync::Mutex::new(String::new()),
            http_advertise_endpoint: cfg.http_advertise_endpoint.map(|s| s.to_string()),
            http_port: cfg.http_port,
        });

        // ADR-065 Step 4: build the entity-only config for the shared
        // `MqttClient`. Timing (keepalive 5 s / watchdog 5 s / backoff)
        // is owned by `acowork-mqtt-session` — the Runtime no longer
        // overrides it (previously 30 s keepalive + 60 s watchdog, see
        // `desktop-onboarding-bugfix_154b7ff7.md` §Fix 3; ADR-065 §2.3
        // unifies on 5 s).
        let config = MqttClientConfig {
            client_id: client_id.clone(),
            host: cfg.host.to_string(),
            port: cfg.port,
            transport: acowork_mqtt_session::MqttTransport::Tcp,
            // ADR-055 Phase 5a: authenticate when the broker requires it
            // (username `agent:{id}` + the spawning node's token).
            credentials: cfg
                .username
                .zip(cfg.password)
                .map(|(u, p)| (u.to_string(), p.to_string())),
            // Last Will: if Runtime crashes/disconnects, broker
            // publishes a retained `AgentStatus{online=false,
            // state=OFFLINE}` envelope (ADR-085 invariant 6).
            last_will: Some(LastWill::new(
                &bootstrap_data.status_topic,
                encode_agent_status_payload(
                    &bootstrap_data.agent_id,
                    &bootstrap_data.instance_id,
                    bootstrap_data.node_id.as_deref().unwrap_or(""),
                    false,
                    AgentLifecycleState::Offline,
                    "",
                ),
                QoS::AtLeastOnce,
                true,
            )),
            // ADR-039: align outgoing packet size with the broker's
            // `max_payload_size` (`GATEWAY_MQTT_MAX_PACKET_SIZE`).
            max_packet_size: defaults::GATEWAY_MQTT_MAX_PACKET_SIZE,
            queue_capacity: 100,
        };

        // ADR-039 §5.2.1: use a oneshot channel to synchronise
        // connect() with the first ConnAck instead of the old
        // wait_for_connection() anti-pattern (subscribe to a dummy
        // topic as a readiness probe). The sender is consumed on
        // the first ConnAck only; subsequent reconnects just
        // re-run bootstrap and log.
        let (first_conn_tx, first_conn_rx) = oneshot::channel::<Result<(), String>>();

        let handler = RuntimeHandler {
            bootstrap_data: bootstrap_data.clone(),
            available_cache: cfg.available_cache.clone(),
            control_tx: cfg.control_tx.clone(),
            identity_update_tx: cfg.identity_update_tx.clone(),
            mcp_notifier: cfg.mcp_notifier.clone(),
            provider_update_tx: cfg.provider_update_tx.clone(),
            embedding_update_tx: cfg.embedding_update_tx.clone(),
            lsps_update_tx: cfg.lsps_update_tx.clone(),
            work_dir: cfg.work_dir.clone(),
            agent_id: cfg.agent_id.to_string(),
            node_lsps_topic: bootstrap_data.node_lsps_topic.clone(),
            node_info_topic: bootstrap_data.node_info_topic.clone(),
            node_proxy_update_tx: cfg.node_proxy_update_tx.clone(),
            last_node_proxy_base: std::sync::Arc::new(std::sync::Mutex::new(None)),
            first_conn_tx: tokio::sync::Mutex::new(Some(first_conn_tx)),
            // The lifecycle stamp (current_lifecycle / lifecycle_detail)
            // is not cloned into the handler struct — it lives on
            // `bootstrap_data` (an `Arc<BootstrapData>` clone is held by
            // the handler via `self.bootstrap_data` below) and
            // `run_bootstrap` consumes only `bootstrap_data`.
        };

        let inner = MqttClient::connect(config, handler, None)
            .await
            .map_err(|e| RuntimeMqttClientError::Connection(e.to_string()))?;

        // ADR-039 §5.2.1: wait for the first ConnAck bootstrap to
        // complete via a oneshot channel instead of the old
        // wait_for_connection() anti-pattern. The event loop's
        // ConnAck handler runs run_bootstrap() and signals us here -
        // no double bootstrap, no dummy subscribe probe.
        let bootstrap_result = first_conn_rx.await.map_err(|_| {
            RuntimeMqttClientError::Connection(
                "bootstrap signal channel closed (event loop dropped)".into(),
            )
        })?;
        bootstrap_result.map_err(RuntimeMqttClientError::Connection)?;

        tracing::info!(
            host = %cfg.host,
            port = %cfg.port,
            client_id = %client_id,
            agent_id = %cfg.agent_id,
            "Runtime MQTT client connected and bootstrapped \
             (status/meta/config + global/control)"
        );

        let shared_client = inner.shared_handle();
        let mqtt_client = Self {
            inner,
            shared_client,
            agent_id: cfg.agent_id.to_string(),
            instance_id: instance_id.to_string(),
            bootstrap_data,
        };

        // Presence self-heal (2026-09-17 sleep/wake + Wi-Fi switch
        // incident): on reconnect the retained "online" publish can be
        // clobbered by the *previous* connection's Last Will ("offline",
        // retained), which the broker may deliver *after* our online
        // publish. The retained status then sticks at "offline" while the
        // session is alive, so the Gateway/UI shows the agent as stopped
        // and rejects start with 400 "already running" (the process is
        // genuinely up). Re-publish retained "online" every 5 s while
        // `Connected` so any stale LWT is overwritten within one tick.
        // A real drop still reads as offline: the LWT fires and we skip
        // publishing while the session is not Connected.
        //
        // `ponytail:` the task lives for the process lifetime (one Runtime
        // hosts one agent), so the detached clone is intentional; it dies
        // with the tokio runtime on shutdown.
        let hb = mqtt_client.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if hb.session_state() != SessionState::Connected {
                    continue;
                }
                if let Err(e) = hb.publish_status(true).await {
                    tracing::debug!(
                        agent_id = %hb.agent_id,
                        error = %e,
                        "status heartbeat publish failed"
                    );
                }
            }
        });

        Ok(mqtt_client)
    }

    /// Publish agent status (`online`, retained), agent meta, agent
    /// config (both retained), and subscribe to the global resource
    /// tree plus the per-agent control tree. Idempotent; safe to
    /// invoke on every (re)connect to restore both retained state
    /// and persistent subscriptions.
    ///
    /// Implements the "Bootstrap seven-step contract" of ADR-039
    /// (ADR-085: the post-2026-09-07 Step 7 `ready` re-publish is
    /// retired — the lifecycle re-stamp it provided now rides Step 1's
    /// `AgentStatus` envelope):
    /// 1. PUBLISH `status = online` (Retained) - overrides the Last
    ///    Will payload (`offline`) set during `connect()`. Also the
    ///    ADR-085 D3b lifecycle re-stamp site: the first connect of
    ///    the process stamps STARTING, every later (re)connect
    ///    re-stamps the current value (a reconnect is not a process
    ///    restart and must not regress the state).
    /// 2. PUBLISH `meta` (Retained) - agent capability descriptor.
    /// 3. PUBLISH `config` (Retained) - agent runtime configuration.
    /// 4. SUBSCRIBE `acowork/global/#` - global resources.
    /// 5. SUBSCRIBE `acowork/agents/{id}/sessions/control/#` - Desktop
    ///    control commands. Without this step, a `clean_session =
    ///    true` broker silently drops the subscription on the next
    ///    (re)connect - the symptom that prompted ADR-039.
    /// 6. SUBSCRIBE `acowork/nodes/{node_id}/lsps` - node LSP relay
    ///    state (ADR-055 §6.7, only when `--node-id` is set).
    /// 7. SUBSCRIBE `acowork/nodes/{node_id}/info` - node reverse-proxy
    ///    base URL watch (§6.3.3, only when `--node-id` is set).
    async fn run_bootstrap(
        client: &AsyncClient,
        data: &BootstrapData,
    ) -> Result<(), RuntimeMqttClientError> {
        // Step 1: PUBLISH status (Retained) — `DataEnvelope<AgentStatus>`.
        // ADR-085 D3/D3b: first connect stamps STARTING; reconnects
        // re-stamp the current lifecycle (plus FAILED detail, if any).
        let (lifecycle, detail) = {
            let cur = data.current_lifecycle.load(std::sync::atomic::Ordering::Acquire);
            if cur == 0 {
                data.current_lifecycle.store(
                    AgentLifecycleState::Starting as u8,
                    std::sync::atomic::Ordering::Release,
                );
                (AgentLifecycleState::Starting, String::new())
            } else {
                (
                    AgentLifecycleState::try_from(cur as i32)
                        .unwrap_or(AgentLifecycleState::Unspecified),
                    data.lifecycle_detail.lock().unwrap().clone(),
                )
            }
        };
        client
            .publish(
                &data.status_topic,
                QoS::AtLeastOnce,
                true,
                encode_agent_status_payload(
                    &data.agent_id,
                    &data.instance_id,
                    data.node_id.as_deref().unwrap_or(""),
                    true,
                    lifecycle,
                    &detail,
                ),
            )
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("status: {}", e)))?;

        // Step 2: PUBLISH meta (Retained).
        let meta = AgentMeta {
            agent_id: data.agent_id.clone(),
            name: data.agent_name.clone(),
            version: data.agent_version.clone(),
            // ADR-009 §V-Q: the avatar / display name are resolved
            // server-side (Gateway `list_agents` merges the Runtime's
            // per-instance overrides). The Runtime reports none.
            avatar: String::new(),
            builtin_avatar: String::new(),
            // ADR-073: instance + location metadata.
            instance_id: data.instance_id.clone(),
            node_id: data.node_id.clone().unwrap_or_default(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentMeta(meta)),
        };
        let payload = prost::Message::encode_to_vec(&envelope);
        client
            .publish(&data.meta_topic, QoS::AtLeastOnce, true, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("meta: {}", e)))?;

        // Step 3: PUBLISH config (Retained).
        let config = AgentConfig {
            agent_id: data.agent_id.clone(),
            config_json: data.config_json.clone(),
            // ADR-073: instance + location metadata.
            instance_id: data.instance_id.clone(),
            node_id: data.node_id.clone().unwrap_or_default(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentConfig(config)),
        };
        let payload = prost::Message::encode_to_vec(&envelope);
        client
            .publish(&data.config_topic, QoS::AtLeastOnce, true, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("config: {}", e)))?;

        // Step 4: SUBSCRIBE acowork/global/#.
        client
            .subscribe("acowork/global/#", QoS::AtLeastOnce)
            .await
            .map_err(|e| RuntimeMqttClientError::Subscribe(format!("global: {}", e)))?;

        // Step 5: SUBSCRIBE agents/{id}/sessions/control/#.
        client
            .subscribe(&data.control_filter, QoS::AtLeastOnce)
            .await
            .map_err(|e| RuntimeMqttClientError::Subscribe(format!("control: {}", e)))?;

        // Step 6: SUBSCRIBE the node's LSP relay topic (ADR-055 §6.7,
        // Phase 4). Only when the Runtime was spawned with `--node-id`;
        // standalone Runtimes have no node and skip this step (the
        // codebase tool stays unregistered — the pre-Phase-4 behavior).
        if let Some(lsps_topic) = data.node_lsps_topic.as_deref() {
            client
                .subscribe(lsps_topic, QoS::AtLeastOnce)
                .await
                .map_err(|e| RuntimeMqttClientError::Subscribe(format!("node lsps: {}", e)))?;
        }

        // Step 7: SUBSCRIBE the node's retained info topic (§6.3.3,
        // only when `--node-id` is set). Lets the Runtime watch the
        // node's reverse-proxy base URL and re-publish its own
        // `http_endpoint` when the node changes network — self-healing
        // without a node or Runtime restart. On a fresh broker the
        // retained NodeInfo is re-delivered here right after ConnAck,
        // which also repairs the endpoint if the node re-published it
        // while we were disconnected.
        if let Some(info_topic) = data.node_info_topic.as_deref() {
            client
                .subscribe(info_topic, QoS::AtLeastOnce)
                .await
                .map_err(|e| RuntimeMqttClientError::Subscribe(format!("node info: {}", e)))?;
        }

        // Step 9: PUBLISH `http_endpoint` (Retained) — §6.3.3 /
        // ADR-055 D3 re-discovery after a Gateway restart.
        //
        // The Gateway embeds an in-memory rumqttd broker; restarting
        // the Gateway drops every retained message, including this
        // Runtime's `acowork/agents/{id}/http_endpoint`. The startup
        // path (`agent_init`) publishes it exactly once per process
        // lifetime, so a Runtime that merely reconnects after a
        // Gateway restart is never re-discovered and every
        // reverse-proxy request 503s ("loading" forever in the
        // Desktop). Re-stamp the retained bit here on EVERY ConnAck,
        // same as status/meta/config/ready (ADR-039). The §6.3.3
        // node-proxy-change re-publish still owns the "node changed
        // its base URL" case; this step re-publishes the base
        // captured at spawn time, which is correct unless the node
        // actually moved networks (then §6.3.3 corrects it).
        let endpoint = match &data.http_advertise_endpoint {
            Some(base) => Some(format!(
                "{}/agents/{}",
                base.trim_end_matches('/'),
                data.instance_id
            )),
            None => data
                .http_port
                .map(|port| format!("http://127.0.0.1:{}", port)),
        };
        if let Some(endpoint) = endpoint {
            let topic = format!("acowork/agents/{}/http_endpoint", data.instance_id);
            if let Err(e) = client
                .publish(&topic, QoS::AtLeastOnce, true, endpoint.as_str())
                .await
            {
                tracing::warn!(
                    agent_id = %data.agent_id,
                    error = %e,
                    "Step 9 (http_endpoint) republish failed; \
                     Gateway will 503 until the next reconnect"
                );
            } else {
                tracing::info!(
                    agent_id = %data.agent_id,
                    %endpoint,
                    "Re-published retained http_endpoint after (re)connect (§6.3.3 reconnect self-heal)"
                );
            }
        }

        Ok(())
    }

    /// ADR-065 §2.5/§5.4: force a soft-restart of the MQTT event loop.
    ///
    /// Used by the shared `run_power_probe_loop` wake recovery: in
    /// never-sleep / standalone mode the Runtime is a resident process
    /// with no parent to restart it, so after an OS sleep/wake it must
    /// recover its stale MQTT connection by itself. The unified client
    /// owns the `ForceRestart` (AtomicBool + Notify) signal; this is a
    /// thin passthrough.
    pub fn force_reconnect(&self) {
        self.inner.force_reconnect();
    }

    /// ADR-039 Phase 2: returns the current MQTT session state.
    ///
    /// External consumers (health checks, DevMode) can poll this to
    /// see if the agent is connected, reconnecting, or permanently
    /// disconnected.
    pub fn session_state(&self) -> SessionState {
        self.inner.current_state()
    }

    /// ADR-039 Phase 2: subscribe to session state changes.
    pub fn session_state_rx(&self) -> SessionStateRx {
        // The unified client owns the session-state watch channel
        // (ADR-065 Step 4); subscribe to it directly.
        self.inner.state_rx()
    }

    /// Publish a `DataEnvelope` payload to a topic.
    pub async fn publish_envelope(
        &self,
        topic: &str,
        envelope: &DataEnvelope,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), RuntimeMqttClientError> {
        let payload = prost::Message::encode_to_vec(envelope);
        self.client()
            .await
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Publish agent status as a Retained `DataEnvelope<AgentStatus>`
    /// message. `online=true` is the presence heartbeat; `online=false`
    /// is published by [`Self::shutdown`] before disconnect.
    ///
    /// ADR-085: the heartbeat re-stamps the current lifecycle state so
    /// a Gateway that restarts (its embedded broker is in-memory and
    /// loses retained state) self-heals within one heartbeat, and
    /// `online=false` always carries `state=OFFLINE` (invariant 6).
    pub async fn publish_status(&self, online: bool) -> Result<(), RuntimeMqttClientError> {
        let topic = format!("acowork/agents/{}/status", self.instance_id);
        let (state, detail) = if online {
            let cur = self
                .bootstrap_data
                .current_lifecycle
                .load(std::sync::atomic::Ordering::Acquire);
            (
                AgentLifecycleState::try_from(cur.max(AgentLifecycleState::Starting as u8) as i32)
                    .unwrap_or(AgentLifecycleState::Unspecified),
                self.bootstrap_data.lifecycle_detail.lock().unwrap().clone(),
            )
        } else {
            (AgentLifecycleState::Offline, String::new())
        };
        let payload = encode_agent_status_payload(
            &self.agent_id,
            &self.instance_id,
            self.bootstrap_data.node_id.as_deref().unwrap_or(""),
            online,
            state,
            &detail,
        );
        self.client()
            .await
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("status: {}", e)))?;
        Ok(())
    }

    /// Publish a lifecycle state transition (ADR-085 D3).
    ///
    /// Replaces the retired plaintext `publish_ready`: the state rides
    /// the retained `AgentStatus` envelope (`online=true` + `state` +
    /// `detail`). Capability states advance monotonically — the
    /// built-in guard rejects a target numerically below the current
    /// stamp with a WARN instead of publishing a regression
    /// (invariant 3; reconnect re-stamps go through `run_bootstrap`,
    /// not here). FAILED is the highest value, so the guard never
    /// blocks it; once FAILED is stamped nothing below it can be
    /// published again.
    ///
    /// The stamp is stored BEFORE publishing: a failed publish means
    /// the link is about to retry anyway, and Step 1 of
    /// `run_bootstrap` re-stamps the retained envelope on the next
    /// (re)connect — the canonical "recoverable error → automatic
    /// retry, user-invisible" pattern from the 2026-09-07 incident
    /// review.
    pub async fn publish_lifecycle(
        &self,
        state: AgentLifecycleState,
        detail: &str,
    ) -> Result<(), RuntimeMqttClientError> {
        let cur = self
            .bootstrap_data
            .current_lifecycle
            .load(std::sync::atomic::Ordering::Acquire);
        let target = state as u8;
        if lifecycle_regression_rejected(cur, target) {
            tracing::warn!(
                agent_id = %self.agent_id,
                current = cur,
                rejected = target,
                "lifecycle regression rejected (ADR-085 invariant 3)"
            );
            return Ok(());
        }
        self.bootstrap_data
            .current_lifecycle
            .store(target, std::sync::atomic::Ordering::Release);
        let stored_detail = if matches!(state, AgentLifecycleState::Failed) {
            detail.to_string()
        } else {
            String::new()
        };
        *self.bootstrap_data.lifecycle_detail.lock().unwrap() = stored_detail;
        let topic = format!("acowork/agents/{}/status", self.instance_id);
        let payload = encode_agent_status_payload(
            &self.agent_id,
            &self.instance_id,
            self.bootstrap_data.node_id.as_deref().unwrap_or(""),
            true,
            state,
            detail,
        );
        self.client()
            .await
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("lifecycle: {}", e)))?;
        Ok(())
    }

    /// ADR-085: the lifecycle discriminant most recently stamped by
    /// this process (`0` = never stamped). Read by the HTTP session
    /// layer to keep `404 no_session` honest: the SESSIONS_READY stamp
    /// is set only after the startup scan seeded `latest_session`, so
    /// an empty cache before the stamp means "not known yet" (→ 503),
    /// never "zero sessions" (D4 table, "扫描未完成" row).
    pub fn current_lifecycle(&self) -> u8 {
        self.bootstrap_data
            .current_lifecycle
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Gracefully disconnect from the MQTT broker.
    ///
    /// Sends a DISCONNECT packet and tears down the client. The caller
    /// does **not** need to wait for the broker to acknowledge — the
    /// reconnect path (`pub async fn connect`) handles reconnect on the
    /// next `connect()` call.
    ///
    /// NOTE on the Last Will: per MQTT spec a **clean** DISCONNECT must
    /// NOT trigger the Will — rumqttd honours this (its `Packet::Disconnect`
    /// handler deletes the stored last will), so no `AgentStatus{online=false}`
    /// envelope is published and the retained status stays at whatever the
    /// last [`Self::publish_status`] / [`Self::shutdown`] call left it as.
    /// Crashes and dropped connections still trigger the Will.
    pub async fn disconnect(&self) -> Result<(), RuntimeMqttClientError> {
        let client = self.client().await;
        client
            .disconnect()
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("disconnect: {}", e)))?;
        Ok(())
    }

    /// Publish a raw payload to a topic (non-protobuf).
    pub async fn publish_raw(
        &self,
        topic: &str,
        payload: &[u8],
        qos: MqttQoS,
        retain: bool,
    ) -> Result<(), RuntimeMqttClientError> {
        self.client()
            .await
            .publish(topic, qos.into(), retain, payload)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("'{}': {}", topic, e)))?;
        Ok(())
    }

    /// Subscribe to a topic filter.
    pub async fn subscribe(
        &self,
        filter: &str,
        qos: MqttQoS,
    ) -> Result<(), RuntimeMqttClientError> {
        self.client()
            .await
            .subscribe(filter, qos.into())
            .await
            .map_err(|e| RuntimeMqttClientError::Subscribe(format!("'{}': {}", filter, e)))?;
        Ok(())
    }

    /// Publish a session event (chunk, done, error, etc.) to the messages topic.
    #[allow(dead_code)]
    pub async fn publish_session_event(
        &self,
        session_id: &str,
        event_type: &str,
        envelope: &DataEnvelope,
    ) -> Result<(), RuntimeMqttClientError> {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/messages/{}",
            self.instance_id, session_id, event_type
        );
        // Session messages are QoS 0 (fire-and-forget for streaming events)
        self.publish_envelope(&topic, envelope, MqttQoS::AtMostOnce, false)
            .await
    }

    /// Publish a session lifecycle event (created/deleted).
    #[allow(dead_code)]
    pub async fn publish_session_lifecycle(
        &self,
        event_type: &str,
        envelope: &DataEnvelope,
    ) -> Result<(), RuntimeMqttClientError> {
        let topic = format!(
            "acowork/agents/{}/sessions/{}",
            self.instance_id, event_type
        );
        self.publish_envelope(&topic, envelope, MqttQoS::AtLeastOnce, false)
            .await
    }

    /// Get the agent_id this client represents.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// ADR-073: get the instance identity used for topic construction.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Get a clone of the inner AsyncClient.
    pub async fn inner(&self) -> AsyncClient {
        self.client().await
    }

    /// Borrow a clone of the shared MQTT client handle.
    ///
    /// Companion to [`RuntimeMqttClient::inner`]: returns the
    /// underlying `Arc<Mutex<AsyncClient>>` so companion publishers
    /// (e.g. [`crate::mqtt::agent_config_publisher::MqttAgentConfigPublisher`])
    /// can be constructed in their own modules without coupling to
    /// `RuntimeMqttClient`'s private layout. `pub(crate)` because the
    /// handle must not leak past the `acowork-runtime` crate — every
    /// outside publish goes through a typed publisher.
    pub(crate) fn shared_handle(&self) -> Arc<Mutex<AsyncClient>> {
        Arc::clone(&self.shared_client)
    }
}

impl RuntimeMqttClient {
    /// Graceful shutdown: publish `AgentStatus{online=false}` (retained)
    /// and disconnect.
    ///
    /// There is deliberately NO `Drop` impl that publishes: this type is
    /// `Clone` and cheap copies are dropped all the time (for example the
    /// `SessionContext` clone that lives on the control-loop task), so a
    /// `Drop`-triggered publish turns every one of those into a spurious
    /// retained "offline" message — which makes the Gateway `remove_running()`
    /// right after auto-start, dropping the ready signal that follows
    /// (Phase 5a startup report). A crash is still covered by the Last
    /// Will; callers that need a clean offline transition must call
    /// [`Self::shutdown`] explicitly.
    pub async fn shutdown(&self) {
        let client = self.client().await;
        let status_topic = format!("acowork/agents/{}/status", self.instance_id);
        let payload = encode_agent_status_payload(
            &self.agent_id,
            &self.instance_id,
            self.bootstrap_data.node_id.as_deref().unwrap_or(""),
            false,
            AgentLifecycleState::Offline,
            "",
        );
        let _ = client
            .publish(status_topic, QoS::AtLeastOnce, true, payload)
            .await;
        let _ = client.disconnect().await;
    }
}

/// Shared, thread-safe RuntimeMqttClient.
pub type SharedRuntimeMqttClient = Arc<Mutex<RuntimeMqttClient>>;

// ── MQTT Chunk Publisher (ADR-033 Phase 4: dual-channel session events) ──

/// Lightweight, clonable publisher for MQTT session events.
///
/// Created from a `RuntimeMqttClient` and passed to `SessionCore` so that
/// chunk/tool_call/done events can be published via MQTT alongside the
/// existing gRPC channel. All payloads use `DataEnvelope` protobuf encoding
/// per `docs/zh/protocols/mqtt.md` §4.
///
/// NOTE: Wired into the session loop via MQTT chunk relay task
/// in `subsystems.rs`. See P0-2 in `docs/review/zh/28-adr-033-mqtt-refactor-code-review.md`.
#[derive(Clone)]
pub struct MqttChunkPublisher {
    agent_id: String,
    /// ADR-073: instance identity for topic construction.
    instance_id: String,
    shared_client: Arc<Mutex<AsyncClient>>,
}

impl MqttChunkPublisher {
    /// Create from a RuntimeMqttClient.
    pub fn from_runtime_client(client: &RuntimeMqttClient) -> Self {
        Self {
            agent_id: client.agent_id().to_string(),
            instance_id: client.instance_id().to_string(),
            shared_client: Arc::clone(&client.shared_client),
        }
    }

    /// Obtain a clone of the current AsyncClient.
    async fn client(&self) -> AsyncClient {
        self.shared_client.lock().await.clone()
    }

    /// Return the agent_id this publisher is bound to.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// ADR-073: return the instance identity used for topic construction.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Clear a retained `messages/{event_type}` event for a session.
    ///
    /// Publishes a zero-byte payload with `retain = true` to the
    /// `acowork/agents/{instance_id}/sessions/{sid}/messages/{event_type}`
    /// topic.
    /// Per MQTT spec this deletes the previously stored retained message,
    /// preventing a reconnecting Desktop from receiving stale blocking
    /// events (tool approval / ask question) from a previous turn.
    pub async fn clear_retained_event(&self, session_id: &str, event_type: &str) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/messages/{}",
            self.instance_id, session_id, event_type
        );
        if let Err(e) = self
            .client()
            .await
            .publish(topic, QoS::AtLeastOnce, true, &[])
            .await
        {
            tracing::warn!(
                agent_id = %self.agent_id,
                session_id = %session_id,
                event_type = %event_type,
                error = %e,
                "Failed to clear retained MQTT event"
            );
        }
    }

    /// Publish a session lifecycle envelope (created/deleted) to the
    /// `acowork/agents/{id}/sessions/{event_type}` topic with QoS 1.
    pub async fn publish_lifecycle(
        &self,
        event_type: &str,
        envelope: &DataEnvelope,
    ) -> Result<(), RuntimeMqttClientError> {
        let topic = format!(
            "acowork/agents/{}/sessions/{}",
            self.instance_id, event_type
        );
        let bytes = prost::Message::encode_to_vec(envelope);
        self.client()
            .await
            .publish(topic, QoS::AtLeastOnce, false, bytes)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("lifecycle: {}", e)))
    }

    /// ADR-038: Publish a `SessionOpened` ack to
    /// `acowork/agents/{id}/sessions/{sid}/opened` (QoS 1, non-retained).
    pub async fn publish_session_opened(
        &self,
        session_id: &str,
        status: &str,
        model: Option<String>,
        provider: Option<String>,
        last_active_at: Option<String>,
    ) -> Result<(), RuntimeMqttClientError> {
        let payload = acowork_core::mqtt_proto::SessionOpened {
            session_id: session_id.to_string(),
            status: status.to_string(),
            model: model.unwrap_or_default(),
            provider: provider.unwrap_or_default(),
            last_active_at: last_active_at.unwrap_or_default(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::SessionOpened(payload)),
        };
        let topic = format!(
            "acowork/agents/{}/sessions/{}/opened",
            self.instance_id, session_id
        );
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.client()
            .await
            .publish(topic, QoS::AtLeastOnce, false, bytes)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("session_opened: {}", e)))
    }

    /// ADR-038: Publish a `SessionNotOpened` error to
    /// `acowork/agents/{id}/sessions/{sid}/not_opened` (QoS 0).
    pub async fn publish_session_not_opened(
        &self,
        session_id: &str,
        attempted_command: &str,
        reason: &str,
    ) -> Result<(), RuntimeMqttClientError> {
        let payload = acowork_core::mqtt_proto::SessionNotOpened {
            session_id: session_id.to_string(),
            attempted_command: attempted_command.to_string(),
            reason: reason.to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::SessionNotOpened(payload),
            ),
        };
        let topic = format!(
            "acowork/agents/{}/sessions/{}/not_opened",
            self.instance_id, session_id
        );
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.client()
            .await
            .publish(topic, QoS::AtMostOnce, false, bytes)
            .await
            .map_err(|e| RuntimeMqttClientError::Publish(format!("session_not_opened: {}", e)))
    }

    /// Publish per-session persisted metadata to
    /// `acowork/agents/{id}/sessions/{sid}/meta` with **Retained=true,
    /// QoS=1**.
    ///
    /// Invariant: payload is the *latest complete value*, never a diff.
    /// The broker retains the last value per session, so a Desktop that
    /// (re)connects mid-conversation immediately receives the current
    /// title/model/provider/etc. without an HTTP fetch.
    ///
    /// Distinction from `publish_session_state_changed`:
    ///   - `session_meta`    → persisted per-session config (title, model,
    ///     provider, reasoning_effort, temperature, workspace_id,
    ///     message_count, cumulative tokens)
    ///   - `session_state_changed` → runtime state (status, current
    ///     context_usage, embedding_provider)
    ///
    /// Retained semantics: the broker overwrites the previous retained
    /// message, so hot-field flooding (tokens / message_count changing on
    /// every LLM round-trip) cannot leak stale snapshots — the Desktop
    /// always sees the latest one on (re)connect.
    /// Publish a SessionConfig snapshot to the retained `sessions/{sid}/config`
    /// topic (ADR-043).
    ///
    /// Config fields (title, model, provider, workspace_id, reasoning_effort,
    /// temperature) are low-frequency user actions - published immediately
    /// with no throttle.
    pub async fn publish_session_config(
        &self,
        session_id: &str,
        config: &acowork_core::mqtt_proto::SessionConfig,
    ) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/config",
            self.instance_id, session_id
        );
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::SessionConfig(config.clone()),
            ),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        if let Err(e) = self
            .client()
            .await
            .publish(topic, QoS::AtLeastOnce, /* retain */ true, bytes)
            .await
        {
            tracing::warn!(
                agent_id = %self.agent_id,
                session_id = %session_id,
                error = %e,
                "Failed to publish session_config"
            );
        }
    }

    /// Publish a SessionState snapshot to the retained `sessions/{sid}/state`
    /// topic (ADR-043).
    ///
    /// Runtime telemetry (status, message_count, tokens, ratio,
    /// context_usage) is high-frequency - the state relay coalesces behind
    /// a 3 s cooldown before calling this method.
    pub async fn publish_session_state(
        &self,
        session_id: &str,
        state: &acowork_core::mqtt_proto::SessionState,
    ) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/state",
            self.instance_id, session_id
        );
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::SessionState(state.clone()),
            ),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        if let Err(e) = self
            .client()
            .await
            .publish(topic, QoS::AtLeastOnce, /* retain */ true, bytes)
            .await
        {
            tracing::warn!(
                agent_id = %self.agent_id,
                session_id = %session_id,
                error = %e,
                "Failed to publish session_state"
            );
        }
    }

    /// Publish a session event envelope to the broker at QoS 0 (default for
    /// `messages/*` streaming events per ADR-035 D1 / `mqtt.md` §8.3).
    async fn publish(&self, session_id: &str, event_type: &str, payload: &[u8]) {
        self.publish_with_qos(session_id, event_type, payload, QoS::AtMostOnce, false)
            .await;
    }

    /// Publish a session event envelope at the given QoS.
    ///
    /// ADR-035 O2/D1: `record_complete` is published at QoS 1 (AtLeastOnce)
    /// because it is the authoritative terminal event — losing it leaves
    /// the message stuck in the streaming state with no fallback.
    /// `tool_approval_needed` and `session_state_changed` are also QoS 1
    /// (AtLeastOnce) because losing them breaks the approval flow and
    /// leaves the frontend with a stale session state.
    /// All other `messages/*` events stay at QoS 0.
    ///
    /// `session_state_changed` is published with `retain = true` so that
    /// a Desktop client reconnecting to the broker immediately receives
    /// the latest session status (Idle / Streaming) without needing an
    /// HTTP round-trip. This is the primary recovery path for missed
    /// state transitions during an MQTT outage.
    async fn publish_with_qos(
        &self,
        session_id: &str,
        event_type: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) {
        let topic = format!(
            "acowork/agents/{}/sessions/{}/messages/{}",
            self.instance_id, session_id, event_type
        );
        if let Err(e) = self
            .client()
            .await
            .publish(topic, qos, retain, payload)
            .await
        {
            tracing::warn!(error = %e, session_id, event_type, "Failed to publish MQTT session event");
        }
    }

    /// Publish a chunk event via MQTT (QoS 0, protobuf DataEnvelope).
    #[allow(dead_code)]
    pub(crate) async fn publish_chunk(&self, session_id: &str, message_id: &str, delta: &str) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let d = delta.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::Chunk(ChunkPayload {
                message_id: mid,
                delta: d,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "chunk", &bytes).await;
    }

    /// Publish a done event via MQTT (QoS 0, protobuf DataEnvelope).
    pub(crate) async fn publish_done(&self, session_id: &str, message_id: &str) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::Done(DonePayload {
                message_id: mid,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "done", &bytes).await;
    }

    // ADR-035 C1: publish_tool_call / publish_tool_result removed — tool_call
    // and tool_result records are now delivered via the unified
    // `record_complete` event (see publish_record_complete below), which
    // carries role / message_id / content and applies D9.2 truncation for
    // tool_result at the publish layer.

    /// Publish an error event via MQTT (QoS 1, protobuf DataEnvelope).
    pub(crate) async fn publish_error(&self, session_id: &str, message_id: &str, error_msg: &str) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let err = error_msg.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::Error(ErrorPayload {
                message_id: mid,
                error: err,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "error", &bytes).await;
    }

    /// Publish a stopped event via MQTT (QoS 1, protobuf DataEnvelope).
    pub(crate) async fn publish_stopped(&self, session_id: &str, message_id: &str) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::Stopped(StoppedPayload {
                message_id: mid,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "stopped", &bytes).await;
    }

    /// ADR-045: Publish a tool progress heartbeat (QoS 0).
    pub(crate) async fn publish_tool_progress(
        &self,
        session_id: &str,
        tool_call_id: &str,
        elapsed_ms: u64,
        timeout_ms: u64,
    ) {
        let sid = session_id.to_string();
        let tid = tool_call_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::ToolProgress(
                acowork_core::mqtt_proto::ToolProgressPayload {
                    session_id: sid.clone(),
                    tool_call_id: tid,
                    elapsed_ms,
                    timeout_ms,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "tool_progress", &bytes).await;
    }

    /// Publish a context_usage event via MQTT (QoS 0).
    ///
    /// Carries the full [`acowork_core::protocol::ContextUsageInfo`] serialised
    /// as JSON in `context_usage`. The Desktop Rust subscriber expands it
    /// into the same shape it emits for `SessionStateChanged`, so the frontend
    /// can render the StatusBar from either source without special-casing.
    pub(crate) async fn publish_context_usage(
        &self,
        session_id: &str,
        ctx_info: &acowork_core::protocol::ContextUsageInfo,
    ) {
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        // Backwards-compat: keep the legacy individual token fields populated
        // for any in-flight Desktop subscriber that hasn't switched to
        // `context_usage` yet.
        let input_tokens = ctx_info.input_tokens;
        let output_tokens = ctx_info.output_tokens;
        let total_input_tokens = ctx_info.total_input_tokens.unwrap_or(0);
        let total_output_tokens = ctx_info.total_output_tokens.unwrap_or(0);
        let cu_json = serde_json::to_string(ctx_info).unwrap_or_default();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::ContextUsage(ContextUsagePayload {
                session_id: sid.clone(),
                input_tokens,
                output_tokens,
                total_input_tokens,
                total_output_tokens,
                context_usage: cu_json,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "context_usage", &bytes).await;
    }

    /// Publish a compacting_started/compacting_ended event via MQTT (QoS 1).
    pub(crate) async fn publish_compacting(&self, session_id: &str, started: bool) {
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let event_type = if started {
            "compacting_started"
        } else {
            "compacting_ended"
        };
        let payload = CompactingPayload {
            session_id: sid.clone(),
        };
        let event = if started {
            session_message::Event::CompactingStarted(payload)
        } else {
            session_message::Event::CompactingEnded(payload)
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(event),
            })),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, event_type, &bytes).await;
    }

    /// Publish a `compaction_cancelled` event (ADR-083, QoS 1).
    ///
    /// Emitted on every non-success end of a compaction — user cancel,
    /// deadline expiry, or all tiers failing — so the frontend can reset its
    /// dual-state button and show a reason-specific toast. The success path
    /// keeps [`Self::publish_compacting`]`(started = false)`.
    pub(crate) async fn publish_compaction_cancelled(
        &self,
        session_id: &str,
        reason: crate::agent::loop_::CompactionCancelReason,
    ) {
        use crate::agent::loop_::CompactionCancelReason as R;
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let proto_reason = match reason {
            R::UserCancelled => ProtoCompactionCancelReason::User,
            R::Timeout => ProtoCompactionCancelReason::Timeout,
            R::Failed => ProtoCompactionCancelReason::Failed,
        } as i32;
        let payload = CompactionCancelledPayload {
            session_id: sid.clone(),
            reason: proto_reason,
        };
        let event = session_message::Event::CompactionCancelled(payload);
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(SessionMessage {
                agent_id,
                session_id: sid.clone(),
                event: Some(event),
            })),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "compaction_cancelled", &bytes).await;
    }

    /// Publish an ask_question event via MQTT (QoS 1, retained).
    ///
    /// Retained so that a Desktop reconnecting during a pending question
    /// immediately receives the question content without needing an HTTP
    /// round-trip. The retained message is cleared (zero-byte publish)
    /// when the user answers – see `ClearRetainedEvent`.
    pub(crate) async fn publish_ask_question(
        &self,
        session_id: &str,
        message_id: &str,
        question_json: &str,
    ) {
        let sid = session_id.to_string();
        let mid = message_id.to_string();
        let qj = question_json.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::AskQuestion(AskQuestionPayload {
                message_id: mid,
                question_json: qj,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish_with_qos(&sid, "ask_question", &bytes, QoS::AtLeastOnce, true)
            .await;
    }

    /// Publish a todo_updated event via MQTT (QoS 0).
    pub(crate) async fn publish_todo_updated(&self, session_id: &str, todos_json: &str) {
        let sid = session_id.to_string();
        let tj = todos_json.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::TodoUpdated(TodoUpdatedPayload {
                todos_json: tj,
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "todo_updated", &bytes).await;
    }

    /// Publish an iteration_limit_paused event via MQTT (QoS 1).
    pub(crate) async fn publish_iteration_limit_paused(
        &self,
        session_id: &str,
        iteration: u32,
        max_iterations: u32,
        message: String,
    ) {
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::IterationLimitPaused(
                IterationLimitPausedPayload {
                    session_id: sid.clone(),
                    iteration,
                    max_iterations,
                    message,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "iteration_limit_paused", &bytes).await;
    }

    /// Publish a loop_detected_paused event via MQTT (QoS 1).
    pub(crate) async fn publish_loop_detected_paused(&self, session_id: &str, message: &str) {
        let sid = session_id.to_string();
        let msg = message.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::LoopDetectedPaused(
                LoopDetectedPausedPayload {
                    session_id: sid.clone(),
                    message: msg,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "loop_detected_paused", &bytes).await;
    }

    /// Publish a tool_approval_needed event via MQTT (QoS 1).
    ///
    /// ADR-034 Phase 8: takes a single `ToolApprovalNeededEvent` struct.
    pub(crate) async fn publish_tool_approval_needed(&self, ev: ToolApprovalNeededEvent<'_>) {
        let sid = ev.session_id.to_string();
        let rid = ev.request_id.to_string();
        let tn = ev.tool_name.to_string();
        let act = ev.action.to_string();
        let rl = ev.risk_level.to_string();
        let rsn = ev.reason.to_string();
        let tcid = ev.tool_call_id.to_string();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::ToolApprovalNeeded(
                ToolApprovalNeededPayload {
                    session_id: sid.clone(),
                    request_id: rid,
                    tool_name: tn,
                    action: act,
                    risk_level: rl,
                    reason: rsn,
                    tool_call_id: tcid,
                    approval_timeout_secs: ev.approval_timeout_secs,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish_with_qos(&sid, "tool_approval_needed", &bytes, QoS::AtLeastOnce, true)
            .await;
    }

    /// Publish a new_data_available event via MQTT (QoS 0).
    pub(crate) async fn publish_new_data_available(
        &self,
        session_id: &str,
        interval_ms: u32,
        title: Option<&str>,
    ) {
        let sid = session_id.to_string();
        let t = title.map(|s| s.to_string()).unwrap_or_default();
        let agent_id = self.agent_id.clone();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::NewDataAvailable(
                NewDataAvailablePayload {
                    session_id: sid.clone(),
                    interval_ms,
                    title: t,
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        self.publish(&sid, "new_data_available", &bytes).await;
    }

    /// Publish a `stream_delta` event via MQTT (QoS 1, ADR-035 + reorder fix).
    ///
    /// Carries the new COMPLETE streaming lines since the last push. Each
    /// `StreamLine.content` is a whole line — never a partial line or token.
    ///
    /// `seq` is the per-session monotonic counter assigned at the emit site
    /// (`SessionCore::next_seq`); the chunk_relay is single-threaded so the
    /// value matches the order in which the relay enqueued this frame. The
    /// Desktop uses it to place this frame at the correct position in
    /// `messages[]` even if the broker delivers frames in a different order.
    /// See `mqtt_payload.proto::StreamDeltaPayload.seq` for the receiving
    /// contract.
    pub(crate) async fn publish_stream_delta(
        &self,
        session_id: &str,
        lines: &[StreamLine],
        seq: u64,
    ) {
        if lines.is_empty() {
            return;
        }
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let lines = lines.to_vec();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::StreamDelta(StreamDeltaPayload {
                session_id: sid.clone(),
                lines,
                seq: Some(seq),
            })),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        // QoS 1 — `stream_delta` was QoS 0 in ADR-035 initial cut, but
        // a fire-and-forget channel cannot guarantee order against
        // QoS 1 `record_complete` events arriving in parallel. With QoS
        // 1 on both endpoints the broker preserves relative order
        // end-to-end; combined with the `seq` payload the Desktop is
        // also robust to any rare reorder.
        self.publish_with_qos(&sid, "stream_delta", &bytes, QoS::AtLeastOnce, false)
            .await;
    }

    /// Publish a `record_complete` event via MQTT (QoS 1, ADR-035 C1/O2).
    ///
    /// Carries the COMPLETE finalized record. The frontend freezes the
    /// active stream buffer into `messages[]` on receipt and clears
    /// `activeStream`. Published at QoS 1 (AtLeastOnce) because this is
    /// the authoritative terminal event — losing it leaves the message
    /// stuck in the streaming state (ADR-035 O2).
    ///
    /// `tool_name` / `tool_call_id` / `is_error` are populated for
    /// `tool_call` and `tool_result` roles; otherwise empty / false. They
    /// mirror the JSONL metadata so the frontend can pair tool_call with
    /// tool_result and render correct labels without an extra HTTP fetch.
    ///
    /// ADR-035 D9.2: `tool_result` content is truncated to the first 5
    /// lines before publishing. The full content stays in JSONL for LLM
    /// context. No exception — the frontend never receives full tool_result.
    //
    // The 8 arguments (self + 7 fields) are intentional: the struct
    // form (`RecordCompletePayload`) is the protobuf DTO and we want
    // this publisher wrapper to look the same. A dedicated builder
    // would just shuffle the same fields around without reducing the
    // surface area, so we keep it inline and silence the lint.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn publish_record_complete(
        &self,
        session_id: &str,
        role: &str,
        message_id: &str,
        content: &str,
        tool_name: &str,
        tool_call_id: &str,
        is_error: bool,
        seq: u64,
    ) {
        // ADR-035 D9.2: truncate tool_result to first 5 lines for display.
        // Full content stays in JSONL for LLM context. No exception.
        let final_content = if role == "tool_result" {
            truncate_tool_result_lines(content)
        } else {
            content.to_string()
        };
        let sid = session_id.to_string();
        let agent_id = self.agent_id.clone();
        let role = role.to_string();
        let mid = message_id.to_string();
        let tool_name = tool_name.to_string();
        let tool_call_id = tool_call_id.to_string();
        let event = SessionMessage {
            agent_id,
            session_id: sid.clone(),
            event: Some(session_message::Event::RecordComplete(
                RecordCompletePayload {
                    session_id: sid.clone(),
                    role,
                    message_id: mid,
                    content: final_content,
                    tool_name,
                    tool_call_id,
                    is_error,
                    seq: Some(seq),
                },
            )),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::SessionMessage(event)),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        // ADR-035 O2: QoS 1 — record_complete is the authoritative
        // terminal event; losing it leaves the message stuck. The
        // per-session `seq` makes the frame position self-healing on
        // the Desktop (see `insertBySeq`).
        self.publish_with_qos(&sid, "record_complete", &bytes, QoS::AtLeastOnce, false)
            .await;
    }
}

/// ADR-035 D9.2: truncate tool_result to first 5 lines for frontend display.
/// Full content stays in JSONL. No exception.
fn truncate_tool_result_lines(result_json: &str) -> String {
    let lines: Vec<&str> = result_json.lines().collect();
    if lines.len() <= 5 {
        return result_json.to_string();
    }
    let mut truncated = lines.into_iter().take(5).collect::<Vec<_>>().join("\n");
    truncated.push_str("\n...(truncated)");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumqttc::Event;
    use std::time::Duration;

    // ── ADR-085 lifecycle guard + status encoding ──────────────────────

    #[test]
    fn lifecycle_guard_allows_forward_and_restamps_only() {
        // Never stamped (fresh process): anything is accepted.
        assert!(!lifecycle_regression_rejected(0, AgentLifecycleState::Starting as u8));
        assert!(!lifecycle_regression_rejected(0, AgentLifecycleState::SessionsReady as u8));
        // Forward transitions accepted.
        assert!(!lifecycle_regression_rejected(
            AgentLifecycleState::Starting as u8,
            AgentLifecycleState::HttpReady as u8
        ));
        // Re-stamp (equal) accepted — reconnect path re-publishes the current value.
        assert!(!lifecycle_regression_rejected(
            AgentLifecycleState::HttpReady as u8,
            AgentLifecycleState::HttpReady as u8
        ));
        // Regressions rejected (invariant 3).
        assert!(lifecycle_regression_rejected(
            AgentLifecycleState::SessionsReady as u8,
            AgentLifecycleState::HttpReady as u8
        ));
        // FAILED never blocked (highest value), and it locks out everything below.
        assert!(!lifecycle_regression_rejected(
            AgentLifecycleState::HttpReady as u8,
            AgentLifecycleState::Failed as u8
        ));
        assert!(lifecycle_regression_rejected(
            AgentLifecycleState::Failed as u8,
            AgentLifecycleState::SessionsReady as u8
        ));
    }

    #[test]
    fn status_envelope_roundtrips_state_and_detail() {
        use prost::Message as _;
        let bytes = encode_agent_status_payload(
            "com.test.agent",
            "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c",
            "node-1",
            true,
            AgentLifecycleState::Failed,
            "session init: sqlite open failed",
        );
        let envelope = acowork_core::mqtt_proto::DataEnvelope::decode(bytes.as_slice()).unwrap();
        let acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(status) =
            envelope.payload.unwrap()
        else {
            panic!("envelope must carry AgentStatus");
        };
        assert!(status.online);
        assert_eq!(status.state, AgentLifecycleState::Failed as i32);
        assert_eq!(status.detail, "session init: sqlite open failed");
    }

    // ── decode_lsps_payload (ADR-055 §6.7) ────────────────────────────

    fn encode_available_lsps(endpoint: &str, ready: bool) -> Vec<u8> {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AvailableLsps(
                acowork_core::mqtt_proto::AvailableLsps {
                    version: 1,
                    endpoint: endpoint.to_string(),
                    ready,
                },
            )),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn decode_lsps_ready_endpoint() {
        let payload = encode_available_lsps("http://192.168.1.10:19878", true);
        let update = decode_lsps_payload(&payload);
        assert_eq!(
            update.endpoint.as_deref(),
            Some("http://192.168.1.10:19878")
        );
    }

    #[test]
    fn decode_lsps_not_ready_drops_endpoint() {
        let payload = encode_available_lsps("http://192.168.1.10:19878", false);
        let update = decode_lsps_payload(&payload);
        assert!(update.endpoint.is_none());
    }

    #[test]
    fn decode_lsps_empty_payload_is_unavailable() {
        let update = decode_lsps_payload(&[]);
        assert!(update.endpoint.is_none());
    }

    #[test]
    fn decode_lsps_garbage_payload_is_unavailable() {
        let update = decode_lsps_payload(b"not-a-protobuf");
        assert!(update.endpoint.is_none());
    }

    // ── decode_node_proxy_base (§6.3.3) ──────────────────────────────

    fn encode_node_info(http_endpoint: &str) -> Vec<u8> {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeInfo(NodeInfo {
                node_id: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
                node_name: "node-test".to_string(),
                gateway_managed: true,
                hostname: "host".to_string(),
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                node_version: "1.0.0".to_string(),
                protocol_version: 1,
                capabilities: vec!["control_plane".to_string()],
                max_agents: 10,
                agent_count: 0,
                http_endpoint: http_endpoint.to_string(),
            })),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn decode_node_proxy_base_extracts_endpoint() {
        let payload = encode_node_info("http://192.168.1.20:19900");
        assert_eq!(
            decode_node_proxy_base(&payload).as_deref(),
            Some("http://192.168.1.20:19900")
        );
    }

    #[test]
    fn decode_node_proxy_base_empty_endpoint_is_none() {
        let payload = encode_node_info("");
        assert!(decode_node_proxy_base(&payload).is_none());
    }

    #[test]
    fn decode_node_proxy_base_empty_payload_is_none() {
        assert!(decode_node_proxy_base(&[]).is_none());
    }

    #[test]
    fn decode_node_proxy_base_garbage_is_none() {
        assert!(decode_node_proxy_base(b"not-a-protobuf").is_none());
    }

    #[tokio::test]
    async fn test_runtime_mqtt_client_connects_and_publishes() {
        // Start a broker (using the Gateway's broker module). Threaded
        // mode: `start_broker` blocks forever on rumqttd's
        // `Broker::start()` (it joins the server threads), so the
        // Gateway exposes only `start_broker`.
        let port = 18980;
        let broker =
            acowork_gateway::mqtt::start_broker("127.0.0.1", port).expect("broker should start");

        let cache = crate::mqtt::available_cache::new_shared_cache();
        let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();

        // Throwaway work_dir — the test only verifies bootstrap + publishes,
        // not the MCP catalog poll task. A temp path keeps the poll task's
        // filesystem writes (if any) from polluting the project tree.
        let work_dir = std::env::temp_dir().join("acowork-test-mqtt-client-18980");

        let client = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.agent",
            // ADR-073: broker client id + topics are keyed on the
            // instance id (two instances of one package may coexist).
            instance_id: "aa11bb22-cc33-4dd4-8e5e-6f7f8a9b0c1d",
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
            work_dir,
            username: None,
            password: None,
            http_advertise_endpoint: None,
            http_port: None,
        })
        .await
        .expect("Runtime MQTT client should connect");

        // Verify status was published as retained by subscribing and receiving it
        use rumqttc::{AsyncClient as SubClient, MqttOptions as SubOpts};
        let mut sub_opts = SubOpts::new("test:subscriber", "127.0.0.1", port);
        // Mirror production keep-alive so the test broker does not
        // disconnect the subscriber while we are still collecting
        // published topics.
        sub_opts.set_keep_alive(acowork_mqtt_session::KEEPALIVE_INTERVAL);
        let (sub_client, mut sub_eventloop) = SubClient::new(sub_opts, 10);
        sub_client
            .subscribe(
                "acowork/agents/aa11bb22-cc33-4dd4-8e5e-6f7f8a9b0c1d/#",
                QoS::AtLeastOnce,
            )
            .await
            .unwrap();

        let mut received_topics = Vec::new();
        for _ in 0..100 {
            match sub_eventloop.poll().await {
                Ok(Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                    received_topics.push(p.topic);
                    if received_topics.len() >= 3 {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }

        assert!(
            received_topics.contains(
                &"acowork/agents/aa11bb22-cc33-4dd4-8e5e-6f7f8a9b0c1d/status".to_string()
            ),
            "should receive status: {:?}",
            received_topics
        );
        assert!(
            received_topics
                .contains(&"acowork/agents/aa11bb22-cc33-4dd4-8e5e-6f7f8a9b0c1d/meta".to_string()),
            "should receive meta: {:?}",
            received_topics
        );
        assert!(
            received_topics.contains(
                &"acowork/agents/aa11bb22-cc33-4dd4-8e5e-6f7f8a9b0c1d/config".to_string()
            ),
            "should receive config: {:?}",
            received_topics
        );

        drop(sub_client);
        drop(client);
        drop(broker);
    }

    /// Presence self-heal (2026-09-17 sleep/wake incident): the broker
    /// can deliver the previous connection's Last Will (`AgentStatus{online=false}`,
    /// retained) AFTER the reconnect's retained `online=true`, clobbering
    /// the status. The 5 s heartbeat must re-publish retained `online=true`
    /// so a stale offline envelope is overwritten within one tick.
    #[tokio::test]
    async fn test_status_heartbeat_reclaims_retained_online() {
        let port = 18982;
        let broker =
            acowork_gateway::mqtt::start_broker("127.0.0.1", port).expect("broker should start");

        let cache = crate::mqtt::available_cache::new_shared_cache();
        let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();
        let work_dir = std::env::temp_dir().join("acowork-test-mqtt-heartbeat-18982");
        let instance_id = "bb22cc33-4dd4-4e5e-8f6f-7a8b9c0d1e2f";

        let client = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.agent",
            instance_id,
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
            work_dir,
            username: None,
            password: None,
            http_advertise_endpoint: None,
            http_port: None,
        })
        .await
        .expect("Runtime MQTT client should connect");

        use rumqttc::{AsyncClient as SubClient, MqttOptions as SubOpts};
        let mut sub_opts = SubOpts::new("test:heartbeat:sub", "127.0.0.1", port);
        sub_opts.set_keep_alive(acowork_mqtt_session::KEEPALIVE_INTERVAL);
        let (sub_client, mut sub_eventloop) = SubClient::new(sub_opts, 10);
        let status_topic = format!("acowork/agents/{}/status", instance_id);
        sub_client
            .subscribe(&status_topic, QoS::AtLeastOnce)
            .await
            .unwrap();

        let online_payload = encode_agent_status_payload(
            "com.test.agent",
            instance_id,
            "",
            true,
            AgentLifecycleState::Starting,
            "",
        );
        let offline_payload = encode_agent_status_payload(
            "com.test.agent",
            instance_id,
            "",
            false,
            AgentLifecycleState::Offline,
            "",
        );

        // 1) Wait for the initial retained `online=true` from bootstrap.
        //    (rumqttd forwards retained publishes to live subscribers
        //    with retain=false, so match on payload, not the flag.)
        let mut got_online = false;
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_millis(500), sub_eventloop.poll()).await {
                Ok(Ok(Event::Incoming(rumqttc::Incoming::Publish(p)))) => {
                    if p.topic == status_topic && p.payload.as_ref() == online_payload.as_slice() {
                        got_online = true;
                        break;
                    }
                }
                _ => continue,
            }
        }
        assert!(got_online, "should receive the initial retained online envelope");

        // 2) Simulate the stale Last Will clobbering the retained status.
        sub_client
            .publish(&status_topic, QoS::AtLeastOnce, true, offline_payload.clone())
            .await
            .unwrap();

        // 3) The 5 s heartbeat must re-publish retained online within
        //    one tick (allow 20 s of slack for tick alignment).
        let mut reclaimed = false;
        for _ in 0..40 {
            match tokio::time::timeout(Duration::from_millis(500), sub_eventloop.poll()).await {
                Ok(Ok(Event::Incoming(rumqttc::Incoming::Publish(p)))) => {
                    if p.topic == status_topic && p.payload.as_ref() == online_payload.as_slice() {
                        reclaimed = true;
                        break;
                    }
                }
                _ => continue,
            }
        }
        assert!(reclaimed, "heartbeat should re-publish retained online within one tick");

        drop(sub_client);
        drop(client);
        drop(broker);
    }

    /// ADR-039 Phase 2: verify that `run_bootstrap` is idempotent —
    /// calling it multiple times does not error, and retained messages
    /// are still present (overwritten, not duplicated).
    #[tokio::test]
    async fn test_bootstrap_idempotency() {
        let port = 18981;
        let broker =
            acowork_gateway::mqtt::start_broker("127.0.0.1", port).expect("broker should start");

        let cache = crate::mqtt::available_cache::new_shared_cache();
        let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();

        // Throwaway work_dir (see test_runtime_mqtt_client_connects_and_publishes
        // for rationale — bootstrap idempotency test never inspects the poll task).
        let work_dir = std::env::temp_dir().join("acowork-test-mqtt-bootstrap-18981");

        let client = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.bootstrap",
            // ADR-073: instance-scoped identity (see first test).
            instance_id: "0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d",
            agent_name: "Bootstrap Test",
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
            work_dir,
            username: None,
            password: None,
            http_advertise_endpoint: None,
            http_port: None,
        })
        .await
        .expect("Runtime MQTT client should connect");

        // Get the bootstrap data from the client and call run_bootstrap
        // a second and third time to verify idempotency.
        let data = &client.bootstrap_data;

        // Second bootstrap (first was done by connect()).
        // `client()` is `async fn` returning `AsyncClient` by clone —
        // the borrow into a new local keeps the future owned across `.await`.
        let second_handle = client.client().await;
        RuntimeMqttClient::run_bootstrap(&second_handle, data)
            .await
            .expect("second bootstrap should succeed (idempotent)");

        // Third bootstrap (refetched; the handle was moved into run_bootstrap).
        let third_handle = client.client().await;
        RuntimeMqttClient::run_bootstrap(&third_handle, data)
            .await
            .expect("third bootstrap should succeed (idempotent)");

        // Verify retained messages are still present.
        use rumqttc::{AsyncClient as SubClient, MqttOptions as SubOpts};
        let mut sub_opts = SubOpts::new("test:bootstrap:sub", "127.0.0.1", port);
        // Mirror production keep-alive so the test broker does not
        // disconnect the subscriber while we are still collecting
        // published topics.
        sub_opts.set_keep_alive(acowork_mqtt_session::KEEPALIVE_INTERVAL);
        let (sub_client, mut sub_loop) = SubClient::new(sub_opts, 10);
        sub_client
            .subscribe(
                "acowork/agents/0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d/#",
                QoS::AtLeastOnce,
            )
            .await
            .unwrap();

        let mut received = Vec::new();
        for _ in 0..100 {
            match sub_loop.poll().await {
                Ok(Event::Incoming(rumqttc::Incoming::Publish(p))) => {
                    received.push(p.topic);
                    if received.len() >= 3 {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }

        assert!(
            received.contains(
                &"acowork/agents/0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d/status".to_string()
            ),
            "should receive status after multiple bootstraps"
        );
        assert!(
            received
                .contains(&"acowork/agents/0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d/meta".to_string()),
            "should receive meta after multiple bootstraps"
        );
        assert!(
            received.contains(
                &"acowork/agents/0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d/config".to_string()
            ),
            "should receive config after multiple bootstraps"
        );

        drop(sub_client);
        drop(client);
        drop(broker);
    }
}

// ── ADR-103 S1: model-level wire mapping tests ────────────────────────

#[cfg(test)]
mod adr103_wire_tests {
    use super::map_provider_refs_to_list_items;
    use acowork_core::mqtt_proto::{
        LlmProtocol, ProviderModelRef, ProviderRef,
    };
    use acowork_core::protocol::ProtocolType;

    fn model(id: &str, proto: Option<i32>, base: Option<&str>) -> ProviderModelRef {
        ProviderModelRef {
            id: id.to_string(),
            capabilities: None,
            max_output_tokens_limit: 0,
            protocol_type: proto,
            base_url: base.map(|s| s.to_string()),
        }
    }

    fn provider(protocol: LlmProtocol, models: Vec<ProviderModelRef>) -> ProviderRef {
        ProviderRef {
            id: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            protocol_type: protocol as i32,
            models,
            ..Default::default()
        }
    }

    #[test]
    fn absent_model_protocol_inherits_provider_level_not_openai() {
        // The §3 red line: an Anthropic provider's models must NOT be
        // re-classified to OpenAI just because the model-level field is absent.
        let list = map_provider_refs_to_list_items(&[provider(
            LlmProtocol::Anthropic,
            vec![model("claude-x", None, None)],
        )]);
        let m = &list[0].models[0];
        assert_eq!(m.protocol_type, None, "absent = inherit, not a concrete value");
        assert_eq!(m.base_url, None);
        assert_eq!(m.protocol_probe, None, "probe verdicts are never delivered");
        assert_eq!(list[0].protocol_type, ProtocolType::Anthropic);
    }

    #[test]
    fn present_model_protocol_maps_through_the_safe_helper() {
        let list = map_provider_refs_to_list_items(&[provider(
            LlmProtocol::Anthropic,
            vec![
                model("a", Some(LlmProtocol::Responses as i32), Some("https://x/v1")),
                model("b", Some(LlmProtocol::Unspecified as i32), None),
                model("c", Some(99), Some("")),
            ],
        )]);
        // Some(RESPONSES) -> effective Responses override.
        assert_eq!(list[0].models[0].protocol_type, Some(ProtocolType::Responses));
        assert_eq!(list[0].models[0].base_url.as_deref(), Some("https://x/v1"));
        // Some(UNSPECIFIED) / Some(garbage) -> inherit (None), never OpenAI.
        assert_eq!(list[0].models[1].protocol_type, None);
        assert_eq!(list[0].models[2].protocol_type, None);
        // Empty base_url string normalizes to None.
        assert_eq!(list[0].models[2].base_url, None);
    }
}
