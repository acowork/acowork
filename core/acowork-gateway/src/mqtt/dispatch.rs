//! MQTT message dispatch (ADR-033).
//!
//! Handles incoming MQTT messages on the Gateway's broker connection.
//! Most topics are matched by topic pattern and either decoded as
//! `DataEnvelope` protobuf or treated as plain text where the payload is
//! genuinely a small textual signal (e.g. `http_endpoint`, agent `ready`).
//!
//! See `docs/zh/protocols/mqtt.md` §8 (Topic patterns).

use std::sync::Arc;

use acowork_core::mqtt_proto::{
    data_envelope, DataEnvelope, NodeEnrollResult,
};
use acowork_core::operation::{OperationId, OperationState};
use acowork_core::{StructuredErrorBody, StructuredErrorCode};
use prost::Message as _;

use crate::handlers::server::SharedState;
use crate::bootstrap::{SharedSubsystemReadinessRegistry, SubsystemId};
use crate::http::proxy::SharedRuntimeHttpRegistry;
use crate::mqtt::agent_registry::SharedAgentRegistry;
use crate::mqtt::client::{GatewayMqttClient, MqttQoS};
use crate::mqtt::enrollment::{
    EnrollmentTokenStore, NodeTokenStore, SharedEnrollmentTokenStore, SharedNodeTokenStore,
    TokenValidation,
};
use crate::mqtt::node_control::NodeControlClient;
use crate::mqtt::node_registry::SharedNodeRegistry;
use crate::operation_store::SharedOperationStore;

/// Window within which an offline signal (LWT `offline` status or an
/// empty retained `NodeReady`) is treated as a stale retained replay
/// rather than a live event (ADR-059 §7.2 replay race).
///
/// When the Gateway MQTT client (re)connects, the broker replays its
/// retained snapshot asynchronously; the replayed queue can deliver a
/// pre-reconnect offline snapshot AFTER the node's live re-announcement
/// (`status=online` + `NodeReady`) already marked it ready. Without the
/// guard such a stale snapshot demotes a healthy node back to BOOTING
/// with no recovery path (its control gate keeps rejecting the node's
/// work; the Runtime's LLM availability can get stuck in LOADING).
/// Retained replay of the small node
/// topic set completes in well under this bound, so any offline signal
/// arriving within it of a gateway (re)connect + node re-announcement
/// is a replay.
const REPLAY_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// Grace period a node's records survive after its last offline signal
/// (ADR-055 §6.13.3). A node killed hard (no graceful shutdown) stops
/// publishing forever; without removal its records — the `node.{id}`
/// bootstrap subsystem above all — linger and leave the Desktop (and
/// every `/api/nodes` consumer) showing a ghost entry. Since the
/// `node.{id}` entry is Optional this no longer pins the aggregated
/// phase in BOOTING; the removal keeps the fleet view truthful.
/// The timer arms on offline and fires only if the node is still
/// offline at expiry; a reconnect inside the window re-announces every
/// retained record, so nothing is lost by waiting.
pub const NODE_OFFLINE_REMOVAL_GRACE: std::time::Duration = std::time::Duration::from_secs(120);

/// How often the reconcile loop re-aligns `running_agents` to the
/// authoritative `agent_registry` view. Cheap (a couple of `HashMap`
/// walks) so a 5-second cadence is fine. The loop's first tick fires
/// immediately (retained online snapshots from Runtimes may already
/// be sitting in `agent_registry`); afterwards it is a pure
/// interval — Gateway (re)connects converge through the dispatch
/// event path (the broker replays retained statuses) with this loop
/// as the backstop.
pub const RECONCILE_INTERVAL_SECS: u64 = 5;

/// Age beyond which a `running_agents` entry with no broker `online`
/// backing is treated as stale and dropped. Entries younger than
/// this are considered in-flight: the start path installs the entry
/// as soon as the node acknowledges, but the Runtime still needs
/// time to boot, connect and publish its retained `online` (cold
/// boot after an OS wake can exceed one tick). Dropping such an
/// entry would silently undo a start that is still progressing.
pub const RECONCILE_STALE_GRACE_SECS: u64 = 2 * RECONCILE_INTERVAL_SECS;

/// Per-node last-live-online-signal bookkeeping for the replay guard.
///
/// An offline signal is only treated as a stale replay when BOTH hold:
/// - the Gateway MQTT client (re)connected within [`REPLAY_WINDOW`]
///   (retained replays only exist right after a (re)subscribe), and
/// - the node announced a live online signal AFTER that reconnect
///   (the node re-announced, so an offline snapshot predating the
///   reconnect is stale).
///
/// Synchronous (called from the dispatch path) — the timestamps must
/// be visible to the demote checks in the same message-processing order
/// the broker delivered the live signals, which is what makes the
/// suppression deterministic.
#[derive(Default)]
pub struct NodeReplayGuard {
    last_online_signal: std::sync::Mutex<
        std::collections::HashMap<String, std::time::Instant>,
    >,
    last_gateway_reconnect: std::sync::Mutex<Option<std::time::Instant>>,
}

impl NodeReplayGuard {
    /// Record a live online signal for `node_id` (`status=online` or a
    /// non-empty `NodeReady`).
    pub fn mark_online(&self, node_id: &str) {
        self.last_online_signal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node_id.to_string(), std::time::Instant::now());
    }

    /// Record a Gateway MQTT client (re)connection (ConnAck).
    ///
    /// Called by the Gateway's own MQTT poll task; retained replays
    /// only exist in the window following a (re)subscribe.
    pub fn mark_gateway_reconnect(&self) {
        *self
            .last_gateway_reconnect
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    }

    /// Whether an offline signal for `node_id` is a stale replay.
    ///
    /// True only when the Gateway recently (re)connected AND the node
    /// announced a live online signal after that reconnect — i.e. the
    /// node already re-announced, so a queued offline snapshot
    /// predating the reconnect must be ignored. A genuine offline (no
    /// recent gateway reconnect, or a node that never re-announced) is
    /// never suppressed.
    pub fn is_replay(&self, node_id: &str) -> bool {
        let reconnect = self
            .last_gateway_reconnect
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(gw_reconnect) = *reconnect else {
            return false;
        };
        if gw_reconnect.elapsed() >= REPLAY_WINDOW {
            return false;
        }
        let signals = self
            .last_online_signal
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        matches!(signals.get(node_id), Some(t) if *t > gw_reconnect)
    }

    /// Drop the per-node bookkeeping (the node left the fleet for
    /// good — ADR-055 §6.13.3). Keeps the map from growing with dead
    /// node ids and clears any stale suppression state.
    pub fn forget(&self, node_id: &str) {
        self.last_online_signal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(node_id);
    }
}

/// Bundled dependencies for the MQTT dispatch layer.
///
/// Groups every optional dependency the dispatch handlers need so the
/// entry points stay narrow (`topic`, `payload`, `ctx`) and new
/// subsystems do not keep growing the argument list. All fields are
/// cheaply clonable (`Arc` / `Option<Arc>`), so a single context can
/// be shared across every message without ownership fights.
///
/// Constructed once by `Gateway::run` (see `gateway/mod.rs`) and cloned
/// per message into each dispatch task; tests build it via `Default`
/// and override only the fields they exercise.
#[derive(Clone)]
pub struct DispatchContext {
    /// Runtime HTTP port registry for the reverse proxy (ADR-033).
    pub runtime_http_registry: SharedRuntimeHttpRegistry,
    /// Agent online/offline registry (ADR-033).
    pub agent_registry: SharedAgentRegistry,
    /// Node registry — LWT-driven online state + retained info
    /// snapshots (ADR-055 §6.2).
    pub node_registry: SharedNodeRegistry,
    /// Gateway MQTT client for node-control plane replies (enrollment
    /// handshake, etc.). `None` in tests without a broker.
    pub mqtt_client: Option<Arc<GatewayMqttClient>>,
    /// Shared Gateway state.
    pub state: SharedState,
    /// Node control-plane client for correlating NodeEvent replies
    /// (ADR-055 §6.2).
    pub node_control: Option<NodeControlClient>,
    /// Enrollment token store (ADR-055 Phase 5a).
    pub enrollment_tokens: Option<SharedEnrollmentTokenStore>,
    /// Node token store (ADR-055 Phase 5a).
    pub node_tokens: Option<SharedNodeTokenStore>,
    /// ADR-087: node ownership table — the enroll accept path binds the
    /// enrollment token's creator as the node owner.
    pub node_owners: Option<crate::gateway::ownership::SharedOwnershipStore>,
    /// ADR-087: agent ownership table — the retained-inventory clear
    /// path prunes rows for instances that no longer exist anywhere.
    pub agent_owners: Option<crate::gateway::ownership::SharedOwnershipStore>,
    /// ADR-087 D7 / review M4: pending installs — committed to
    /// `agent_owners` when the instance's retained inventory lands,
    /// dropped when the Node reports the operation failed.
    pub pending_installs: Option<crate::gateway::ownership::SharedPendingInstalls>,
    /// Whether enrollment-token validation is enforced (ADR-055 Phase 5a).
    pub auth_enabled: bool,
    /// ADR-087: default owner for resources with no interactive owner —
    /// the earliest-created admin (server scenario: whoever can start the
    /// Gateway on that machine is the admin). `None` in local mode (no
    /// account system; ownerless = "the machine is the owner") and while
    /// no admin account exists yet. The two convergence points (enroll
    /// accept, first inventory landing) fall back to this so bypass
    /// paths (CLI-issued tokens, CLI installs) stop minting ownerless
    /// rows.
    pub default_owner: Option<String>,
    /// Subsystem readiness registry — receives `node.{id}` readiness
    /// transitions from `acowork/nodes/+/ready` (ADR-059 §7.2).
    pub bootstrap_registry: Option<SharedSubsystemReadinessRegistry>,
    /// Operation store — correlates NodeEvent replies to tracked
    /// mutation operations (ADR-059 §6).
    pub operation_store: Option<SharedOperationStore>,
    /// Replay guard — suppresses stale retained replays of offline
    /// signals from demoting freshly-reconnected nodes (ADR-059 §7.2).
    pub node_replay_guard: Arc<NodeReplayGuard>,
    /// Triggers an `acowork/desktop/inventory` publish whenever the
    /// aggregated `installed_agents` table mutates. `None` in tests
    /// that do not start the notifier; dispatcher code paths must
    /// `if let Some(t) = ctx.inventory_trigger { t.notify(); }` so the
    /// absence is harmless.
    pub inventory_trigger: Option<crate::mqtt::InventoryNotifierTrigger>,
}

impl Default for DispatchContext {
    fn default() -> Self {
        Self {
            runtime_http_registry: crate::http::proxy::new_shared_registry(),
            agent_registry: crate::mqtt::agent_registry::new_shared_registry(),
            node_registry: crate::mqtt::node_registry::new_shared_registry(),
            mqtt_client: None,
            state: Arc::new(tokio::sync::RwLock::new(
                crate::gateway::state::GatewayState::new("/tmp/acowork-dispatch-default"),
            )),
            node_control: None,
            enrollment_tokens: None,
            node_tokens: None,
            node_owners: None,
            agent_owners: None,
            pending_installs: None,
            auth_enabled: false,
            default_owner: None,
            bootstrap_registry: None,
            inventory_trigger: None,
            operation_store: None,
            node_replay_guard: Arc::new(NodeReplayGuard::default()),
        }
    }
}

/// Unified MQTT message handler — called from the Gateway's MQTT callback.
///
/// `ctx` bundles every dependency the dispatch layer needs (see
/// [`DispatchContext`]); subsystems that a test does not wire are
/// simply `None` and their topics are logged and skipped.
pub fn handle_message(topic: &str, payload: &[u8], ctx: &DispatchContext) {
    handle_plaintext_message(topic, payload, ctx);
}

/// Topic pattern matcher.
///
/// Supports MQTT wildcard matching:
/// - `+` matches a single level
/// - `#` matches all remaining levels (must be the last character)
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let filter_parts: Vec<&str> = filter.split('/').collect();
    let topic_parts: Vec<&str> = topic.split('/').collect();

    for (i, fp) in filter_parts.iter().enumerate() {
        if *fp == "#" {
            return true; // matches everything remaining
        }
        if i >= topic_parts.len() {
            return false;
        }
        if *fp != "+" && *fp != topic_parts[i] {
            return false;
        }
    }

    filter_parts.len() == topic_parts.len()
}

/// Handle a plain-text MQTT message (non-DataEnvelope payload).
///
/// Topics with simple text payloads rather than protobuf envelopes:
/// - `acowork/agents/+/http_endpoint` → registers Runtime HTTP port for reverse proxy
/// - `acowork/nodes/+/status` → updates NodeRegistry online/offline
///   (ADR-055 §6.2; plain text + LWT, same shape as agent status).
/// - `acowork/nodes/+/info` → protobuf `DataEnvelope<NodeInfo>`
///   metadata snapshot into the NodeRegistry (the payload IS an
///   envelope; routed here because the node topic family shares the
///   same dispatch surface).
/// - `acowork/nodes/+/enroll` → ADR-055 Phase 5a enrollment handshake
///   (protobuf `DataEnvelope<NodeEnroll>`); validated, then answered
///   with an `enroll_result` reply on the per-node result topic.
///
/// Agent status (`acowork/agents/+/status`) is decoded as a protobuf
/// `DataEnvelope<AgentStatus>` — see the dedicated arm below. The plain
/// text was retired when the auto-sleep subsystem was removed (Sept
/// 2026): the only on/off transition is now stop / start, and both
/// sides of that wire are protobuf.
///
/// This replaces the inline callback previously in `gateway/mod.rs`.
pub fn handle_plaintext_message(topic: &str, payload: &[u8], ctx: &DispatchContext) {
    if topic_matches("acowork/agents/+/http_endpoint", topic) {
        let agent_id = topic
            .strip_prefix("acowork/agents/")
            .and_then(|s| s.strip_suffix("/http_endpoint"))
            .unwrap_or("");
        if agent_id.is_empty() {
            tracing::warn!(topic, "http_endpoint topic matched but agent_id extraction failed");
            return;
        }
        // ADR-055 D3: payload is now the full endpoint URL (e.g.
        // "http://127.0.0.1:54321") rather than a bare port. Surface
        // malformed payloads as warnings — without these, a bad payload
        // would cause the Gateway to 503 with no diagnostic trail.
        let endpoint = match std::str::from_utf8(payload) {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                tracing::warn!(
                    topic,
                    agent_id,
                    error = %e,
                    "http_endpoint payload is not valid UTF-8 — ignoring"
                );
                return;
            }
        };
        if endpoint.is_empty() {
            tracing::warn!(
                topic,
                agent_id,
                "http_endpoint payload is empty — Gateway will 503 every reverse-proxy request for this agent"
            );
            return;
        }
        let reg = ctx.runtime_http_registry.clone();
        let aid = agent_id.to_string();
        let aid_for_log = aid.clone();
        let endpoint_for_log = endpoint.clone();
        tokio::spawn(async move {
            reg.write().await.register(&aid, &endpoint);
        });
        tracing::info!(agent_id = %aid_for_log, endpoint = %endpoint_for_log, "Registered Runtime HTTP endpoint via MQTT");
    } else if topic_matches("acowork/agents/+/status", topic) {
        // Decode the protobuf DataEnvelope<AgentStatus> on a detached
        // task — the handler does an async registry write, but
        // `handle_plaintext_message` is called from a sync MQTT callback
        // and must return promptly to avoid backing up the broker
        // dispatch loop.
        let instance_id = match extract_agent_id_from_status_topic(topic) {
            Some(id) => id,
            None => {
                tracing::warn!(
                    topic,
                    "agent status topic matched but instance_id extraction failed — ignoring"
                );
                return;
            }
        };
        let agent_registry = ctx.agent_registry.clone();
        let state = ctx.state.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        let id_for_task = instance_id.clone();
        tokio::spawn(async move {
            let envelope = match DataEnvelope::decode(payload_owned.as_slice()) {
                Ok(env) => env,
                Err(e) => {
                    tracing::warn!(
                        topic = %topic_owned,
                        instance_id = %id_for_task,
                        error = %e,
                        "agent status payload is not a DataEnvelope protobuf — ignoring"
                    );
                    return;
                }
            };
            let status = match envelope.payload {
                Some(data_envelope::Payload::AgentStatus(s)) => s,
                _ => {
                    tracing::warn!(
                        topic = %topic_owned,
                        instance_id = %id_for_task,
                        "DataEnvelope did not carry an AgentStatus payload — ignoring"
                    );
                    return;
                }
            };
            // Update the registry (authoritative online/offline source).
            agent_registry
                .write()
                .await
                .update_from_mqtt(&topic_owned, &payload_owned);
            // ADR-085: the envelope now carries the capability state
            // (`state`/`detail`) that the retired plaintext
            // `agents/{id}/ready` topic used to pin. Unknown enum values
            // (encoder from a newer version) decode to UNSPECIFIED, which
            // every consumer gates like OFFLINE.
            let lifecycle =
                acowork_core::mqtt_proto::AgentLifecycleState::try_from(status.state)
                    .unwrap_or(acowork_core::mqtt_proto::AgentLifecycleState::Unspecified);
            // Drive running_agents to match the broker view.
            if status.online {
                track_running_agent_for_status(&state, &id_for_task).await;
                let mut gw = state.write().await;
                gw.set_agent_lifecycle(
                    &id_for_task,
                    lifecycle,
                    &status.detail,
                );
                if lifecycle == acowork_core::mqtt_proto::AgentLifecycleState::Failed {
                    // D3: startup failure is user-visible — log it at
                    // info (not just the UI) so server-side triage has
                    // the reason on record.
                    tracing::info!(
                        agent_id = %id_for_task,
                        detail = %status.detail,
                        "Runtime reported FAILED lifecycle state via MQTT"
                    );
                }
            } else {
                let mut gw = state.write().await;
                if gw.running_agents.contains_key(&id_for_task) {
                    tracing::info!(
                        agent_id = %id_for_task,
                        "agent status=offline — removing running_agents entry"
                    );
                    gw.running_agents.remove(&id_for_task);
                }
            }
        });
    } else if topic_matches("acowork/nodes/+/status", topic) {
        // ADR-055 §6.2: node online/offline (plain text + LWT,
        // same shape as the agent status topic). Feeds the
        // NodeRegistry — the local-node supervisor and (Phase 2b)
        // the lifecycle control plane read it.
        let reg = ctx.node_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            reg.write().await.update_status_from_mqtt(&topic_owned, &payload_owned);
        });
        // ADR-059 §7.2: an offline status (LWT or graceful shutdown)
        // means the node's control channel is gone — demote its
        // readiness so dependent work stops hitting the dead channel.
        // The node re-announces `NodeReady` on its next (re)connect,
        // which re-marks it ready; `mark_booting` (not `mark_failed`)
        // keeps that path open. `node.{id}` is Optional — the demotion
        // must NOT drag the aggregated phase out of READY (a killed
        // remote node used to block every starting Desktop for the
        // whole offline-removal grace); the snapshot still re-publishes
        // with a fresh `version` so clients can refresh their node
        // topology in real time.
        if let Some(node_id) = extract_node_id_from_status_topic(topic) {
            let payload_text = String::from_utf8_lossy(payload);
            let trimmed = payload_text.trim();
            // ADR-055 §6.13.3: an empty retained status is a removal
            // signal (the `nodes remove` CLI, or this dispatcher's own
            // cleanup pass). Run the full record cleanup — it is
            // convergent, so the echo of the cleanup's own publish
            // finds no traces and stops.
            if trimmed.is_empty() {
                tracing::info!(
                    node_id,
                    "Node status retained cleared — running node record cleanup"
                );
                let ctx_cleanup = ctx.clone();
                tokio::spawn(async move {
                    remove_node_records(&ctx_cleanup, &node_id).await;
                });
                return;
            }
            let is_offline = trimmed == "offline";
            // Live online signals reset the replay guard so a stale
            // replayed `offline` (queued at subscribe time) cannot
            // demote a node that already reconnected and re-announced
            // (ADR-059 §7.2 replay race — see `NodeReplayGuard`).
            if !is_offline {
                ctx.node_replay_guard.mark_online(&node_id);
            }
            if is_offline && !ctx.node_replay_guard.is_replay(&node_id) {
                // ADR-059 §7.2: demote the node's readiness so
                // dependent work stops hitting the dead channel. The
                // node re-announces `NodeReady` on its next
                // (re)connect, which re-marks it ready;
                // `mark_booting` (not `mark_failed`) keeps that path
                // open. The registration below uses Optional — it
                // must match the first-registration kind in the
                // NodeReady handler (re-registering keeps the
                // recorded kind anyway; this is self-documentation).
                if let Some(registry) = ctx.bootstrap_registry.as_ref() {
                    let subsystem = SubsystemId(format!("node.{}", node_id));
                    // Only demote when currently ready — a node that
                    // never announced NodeReady stays booting already.
                    if registry.state(&subsystem) == Some(crate::bootstrap::SubsystemState::Ready) {
                        let handle = registry.register(subsystem.clone(), crate::bootstrap::ReadinessKind::Optional);
                        handle.mark_booting(Some(format!("node '{node_id}' reported offline")));
                        tracing::info!(
                            node_id,
                            "Node offline — demoted node.{node_id} to booting (ADR-059 §7.2)"
                        );
                    }
                }
                // ADR-055 §6.13.3: arm the permanent-offline cleanup
                // timer — a node still offline when the grace period
                // elapses is removed from the fleet records entirely.
                schedule_node_offline_cleanup(ctx, &node_id);
            }
        }
    } else if topic_matches("acowork/nodes/+/ready", topic) {
        // ADR-059 §7.2: Node control-plane readiness. The Node
        // publishes a retained DataEnvelope<NodeReady> after CONNECT +
        // control subscriptions; an empty retained payload means the
        // snapshot was cleared (Node shutdown / disconnect).
        let Some(node_id) = extract_node_id_from_ready_topic(topic) else {
            tracing::warn!(topic, "ready topic matched but node_id extraction failed");
            return;
        };
        let Some(registry) = ctx.bootstrap_registry.as_ref() else {
            tracing::debug!(topic, "NodeReady received but no bootstrap registry wired — dropping");
            return;
        };
        let subsystem = SubsystemId(format!("node.{}", node_id));
        if payload.is_empty() {
            // Retained snapshot cleared (graceful shutdown or poll-task
            // disconnect clear). Demote to booting; NOT terminal — the
            // Node re-announces NodeReady on reconnect.
            //
            // Never register here: an empty retained payload must not
            // resurrect a node subsystem that the removal cleanup
            // (ADR-055 §6.13.3) just deregistered — the cleanup's own
            // empty-ready publish would otherwise re-create the ghost
            // it just removed and pin the aggregated phase in BOOTING.
            if registry.state(&subsystem).is_none() {
                tracing::debug!(
                    node_id,
                    "Empty NodeReady for unknown subsystem — ignored (no resurrection)"
                );
                return;
            }
            // Replay guard: the empty snapshot is retained, so the
            // broker replays it to a reconnecting Gateway. When it
            // arrives after the node's live re-announcement (status
            // online + NodeReady) the guard recognizes it as a stale
            // replay and skips the demotion — otherwise the node stays
            // demoted (its control gate keeps rejecting work) until
            // its next reconnect. Demote only a node that is currently
            // ready (a never-ready node is already BOOTING; a Failed
            // node must not be resurrected by a replay).
            if !ctx.node_replay_guard.is_replay(&node_id)
                && registry.state(&subsystem) == Some(crate::bootstrap::SubsystemState::Ready)
            {
                let handle = registry.register(subsystem.clone(), crate::bootstrap::ReadinessKind::Optional);
                handle.mark_booting(Some(format!("node '{node_id}' cleared NodeReady (offline)")));
                tracing::info!(
                    node_id,
                    "NodeReady retained cleared — demoted node.{node_id} to booting (ADR-059 §7.2)"
                );
            }
            return;
        }
        // A non-empty NodeReady is authoritative (live re-announcement
        // or a valid retained replay) — record it as an online signal
        // so subsequent stale replays are suppressed by the guard.
        ctx.node_replay_guard.mark_online(&node_id);
        // ADR-059 §7.2 (revised): node control planes register as
        // OPTIONAL subsystems. A node — local or remote — going
        // offline must NOT pin the aggregated phase in BOOTING: that
        // used to block every starting Desktop on "N/M required
        // ready" until the 120s offline-removal grace deregistered
        // the node. Node availability is surfaced through
        // `/api/nodes` and the per-node `is_ready` control gate, both
        // of which are kind-independent. Offline nodes still publish
        // a fresh bootstrap snapshot `version`, so clients refresh in
        // real time.
        let handle = registry.register(subsystem.clone(), crate::bootstrap::ReadinessKind::Optional);
        let payload_owned = payload.to_vec();
        let node_id_owned = node_id.clone();
        tokio::spawn(async move {
            let envelope = match DataEnvelope::decode(payload_owned.as_slice()) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        node_id = %node_id_owned,
                        error = %e,
                        "Bad NodeReady envelope on node ready topic"
                    );
                    return;
                }
            };
            match envelope.payload {
                Some(data_envelope::Payload::NodeReady(ready)) => {
                    // Only the announcing node may mark ITS subsystem.
                    if ready.node_id != node_id_owned {
                        tracing::warn!(
                            announced = %ready.node_id,
                            topic_node = %node_id_owned,
                            "NodeReady node_id mismatch — ignoring"
                        );
                        return;
                    }
                    handle.mark_ready(Some(format!(
                        "node '{node_id_owned}' NodeReady (control plane up)"
                    )));
                    tracing::info!(
                        node_id = %node_id_owned,
                        protocol_version = ready.protocol_version,
                        "Node control plane ready (ADR-059 §7.2)"
                    );
                }
                _ => {
                    tracing::warn!(
                        node_id = %node_id_owned,
                        "Non-NodeReady envelope on node ready topic — ignoring"
                    );
                }
            }
        });
    } else if topic_matches("acowork/nodes/+/info", topic) {
        // ADR-055 §6.2: node metadata snapshot (protobuf
        // DataEnvelope<NodeInfo>, retained). Feeds the NodeRegistry.
        let reg = ctx.node_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            reg.write().await.update_info_from_mqtt(&topic_owned, &payload_owned);
        });
    } else if topic_matches("acowork/nodes/+/lsps", topic) {
        // ADR-055 §6.7 (Phase 4): node-local LSP relay endpoint
        // (protobuf DataEnvelope<AvailableLsps>, retained — replaces
        // the deprecated `acowork/global/lsps`). Feeds the
        // NodeRegistry's `lsp_endpoint` slot, served via
        // `GET /api/agents/{id}/lsp-endpoint`.
        let reg = ctx.node_registry.clone();
        let topic_owned = topic.to_string();
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            reg.write().await.update_lsps_from_mqtt(&topic_owned, &payload_owned);
        });
    } else if topic_matches("acowork/nodes/+/enroll", topic) {
        // ADR-055 Phase 5a: node enrollment handshake. The Node
        // publishes a DataEnvelope<NodeEnroll> with its identity and
        // (when auth is enabled) a one-time enrollment token; the
        // Gateway validates, mints/reuses the long-lived node token,
        // records it, and replies on `enroll_result` (QoS 1).
        let Some(node_id) = extract_enroll_topic_node_id(topic) else {
            tracing::warn!(topic, "enroll topic matched but node_id extraction failed");
            return;
        };
        let reg = ctx.node_registry.clone();
        let mqtt_for_reply = ctx.mqtt_client.clone();
        let enroll_store = ctx.enrollment_tokens.clone();
        let node_store = ctx.node_tokens.clone();
        let node_owners_store = ctx.node_owners.clone();
        let default_owner = ctx.default_owner.clone();
        // `auth_enabled` is a Copy bool — hoist it out of the context
        // before the `'static` spawn so the task does not borrow `ctx`.
        let auth_enabled = ctx.auth_enabled;
        let payload_owned = payload.to_vec();
        tokio::spawn(async move {
            process_enroll_message(
                payload_owned,
                node_id,
                reg,
                mqtt_for_reply,
                enroll_store,
                node_store,
                node_owners_store,
                auth_enabled,
                default_owner,
            )
            .await;
        });
    } else if topic_matches("acowork/nodes/+/agents/+/events", topic) {
        // ADR-055 §6.2: per-agent NodeEvent result (protobuf envelope,
        // QoS 1). Correlate against in-flight node commands.
        let Some(control) = ctx.node_control.clone() else {
            tracing::debug!(topic, "Node agent event received but no NodeControlClient wired — dropping");
            return;
        };
        let payload_owned = payload.to_vec();
        let topic_owned = topic.to_string();
        let op_store = ctx.operation_store.clone();
        // ADR-087 D7 / review M4: the pending-install ledger is keyed by
        // the instance id carried in this very topic — "ok" commits the
        // staged owner row, a terminal failure drops it (no orphan row).
        let pending_installs = ctx.pending_installs.clone();
        let agent_owners_for_events = ctx.agent_owners.clone();
        let instance_id = extract_agent_events_instance_id(topic);
        tokio::spawn(async move {
            match DataEnvelope::decode(payload_owned.as_slice()) {
                Ok(envelope) => {
                    if let Some(data_envelope::Payload::NodeEvent(event)) = envelope.payload {
                        // ADR-059 §6: correlate the reply back to the
                        // tracked mutation operation — install dispatch
                        // carries `operation_id` as the command's
                        // `request_id`. "ok" → Completed; "error" →
                        // Failed (uncertain: the client must re-check by
                        // operation id, never blindly retry);
                        // "in_progress" → Running.
                        if let Some(store) = op_store.as_ref() {
                            let op_id = OperationId::from(event.request_id.clone());
                            match event.status.as_str() {
                                "ok" => {
                                    store.transition(&op_id, OperationState::Completed, None, None);
                                    // ADR-087: install confirmed — commit
                                    // the staged owner row (no-op unless a
                                    // pending install exists for this id).
                                    if let (Some(pending), Some(owners), Some(iid)) =
                                        (pending_installs.as_ref(), agent_owners_for_events.as_ref(), instance_id.as_ref())
                                    {
                                        crate::gateway::ownership::commit_pending(pending, owners, iid);
                                    }
                                }
                                "error" => {
                                    // ADR-087 D7 / review M4: a failed
                                    // install must not leave its owner row
                                    // behind — drop the staged pending.
                                    if let (Some(pending), Some(iid)) =
                                        (pending_installs.as_ref(), instance_id.as_ref())
                                    {
                                        crate::gateway::ownership::drop_pending(pending, iid);
                                    }
                                    // Log the failure at WARN so a
                                    // failing install/upgrade is visible
                                    // in the Gateway log without needing
                                    // to correlate operation store state.
                                    tracing::warn!(
                                        node_id = %event.node_id,
                                        operation_id = %op_id,
                                        message = %event.message,
                                        "Node operation failed"
                                    );
                                    store.fail(
                                        &op_id,
                                        StructuredErrorBody {
                                            code: StructuredErrorCode::OperationUncertain,
                                            current_phase: None,
                                            phase_detail: if event.message.is_empty() {
                                                None
                                            } else {
                                                Some(event.message.clone())
                                            },
                                            retry_hint: None,
                                            operation_id: Some(op_id.clone()),
                                            last_known_phase: None,
                                            current_version: None,
                                            client_expected_version: None,
                                            lease_deadline_ms: None,
                                            endpoint: None,
                                            deadline_ms: None,
                                        },
                                    );
                                }
                                "in_progress" => {
                                    store.transition(&op_id, OperationState::Running, None, None);
                                }
                                _ => {}
                            }
                        }
                        control.handle_event(event).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(topic = %topic_owned, error = %e, "Bad NodeEvent envelope on agent events topic");
                }
            }
        });
    } else if topic_matches("acowork/nodes/+/agents/+/installed", topic) {
        // ADR-055 §6.5: per-agent installed-package inventory (retained).
        // Aggregates into GatewayState::installed_agents, replacing the
        // pre-hard-cut on-disk packages scan (L2-9). An empty retained
        // payload clears the entry (uninstall).
        let Some((node_id, agent_id)) = extract_installed_topic_ids(topic) else {
            tracing::warn!(topic, "installed topic matched but id extraction failed");
            return;
        };
        let state_for_installed = ctx.state.clone();
        let payload_owned = payload.to_vec();
        let node_id_owned = node_id.clone();
        // ADR-087 D7: the retained-inventory clear is the authoritative
        // "this instance no longer exists" signal — prune its ownership
        // row there (the uninstall handler prunes eagerly; this is the
        // backstop for uninstalls the Gateway did not dispatch).
        let agent_owners = ctx.agent_owners.clone();
        // ADR-087 backstop: if the install's "ok" NodeEvent was lost
        // (QoS-1 drop, Gateway restart between dispatch and event), the
        // first inventory sighting still commits the staged owner row.
        let pending_installs_installed = ctx.pending_installs.clone();
        let agent_owners_installed = ctx.agent_owners.clone();
        // ADR-087 convergence point: instances that land with no staged
        // row (CLI install, direct MQTT dispatch — paths that bypass the
        // HTTP staging) are attributed to the default owner instead of
        // staying ownerless.
        let default_owner_installed = ctx.default_owner.clone();
        // Inventory-change signal: every mutation to installed_agents
        // (install complete / retained replay / empty-payload clear)
        // must wake the publisher so subscribers (Desktop) refresh.
        let inventory_trigger = ctx.inventory_trigger.clone();
        tokio::spawn(async move {
            let mut gw = state_for_installed.write().await;
            if payload_owned.is_empty() {
                gw.remove_installed(&agent_id);
                if let Some(store) = agent_owners.as_ref() {
                    store
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&agent_id);
                }
                if let Some(t) = inventory_trigger.as_ref() {
                    t.notify();
                }
                tracing::info!(node_id = %node_id_owned, agent_id, "Removed installed agent (node retained cleared)");
                return;
            }
            match DataEnvelope::decode(payload_owned.as_slice()) {
                Ok(envelope) => {
                    if let Some(data_envelope::Payload::InstalledAgentInfo(info)) = envelope.payload
                    {
                        // ADR-055 §3.2: an install completed (or a retained
                        // re-publish on node reconnect). Record whether this
                        // instance is NEW so cron triggers register exactly
                        // once (the async-install completion hook).
                        // ADR-073: the table is keyed by INSTANCE identity,
                        // and cron entries are instance-scoped too — a
                        // trigger registered under a package id would never
                        // resolve on the fire path.
                        let is_new = !gw.installed_agents.contains_key(&info.instance_id);
                        let Some(aid) = gw.upsert_installed_from_node(&node_id_owned, &info) else {
                            return;
                        };
                        if is_new
                            && let (Some(pending), Some(owners)) = (
                                pending_installs_installed.as_ref(),
                                agent_owners_installed.as_ref(),
                            )
                        {
                            crate::gateway::ownership::commit_pending(
                                pending,
                                owners,
                                &info.instance_id,
                            );
                        }

                        // ADR-087: no staged row was committed (CLI /
                        // bypass install) and no owner row exists — mint
                        // one for the default owner so the instance is
                        // never ownerless in a deployment that has an
                        // admin. Mirrors `build_owner_record`: hosting
                        // node denormalized, shared-by-default package
                        // stays shared.
                        if is_new
                            && let (Some(owner), Some(owners)) = (
                                default_owner_installed.as_ref(),
                                agent_owners_installed.as_ref(),
                            )
                        {
                            let mut rec = crate::gateway::ownership::OwnerRecord::new(Some(
                                owner.clone(),
                            ))
                            .with_node(Some(node_id_owned.clone()));
                            if info.agent_id == crate::http::permission::DEFAULT_SHARED_AGENT_PACKAGE
                            {
                                rec.visibility = crate::gateway::ownership::Visibility::Shared;
                            }
                            let mut store = owners.lock().unwrap_or_else(|e| e.into_inner());
                            if store.get(&info.instance_id).is_none() {
                                store.put_if_absent(&info.instance_id, rec);
                            }
                        }

                        // ADR-055 §3.2 / S3.3: register the manifest-declared
                        // cron triggers on first install. The node no longer
                        // owns cron; the Gateway registers them once the
                        // install-completed inventory arrives. ADR-073: the
                        // trigger key is the INSTANCE identity — the same
                        // key the fire path resolves.
                        if is_new
                            && let Ok(manifest) =
                                acowork_core::AgentManifest::from_toml(&info.manifest_toml)
                        {
                            crate::cron::register_agent_cron_triggers(
                                &mut gw,
                                &info.instance_id,
                                &manifest,
                            );
                        }

                        tracing::info!(node_id = %node_id_owned, agent_id = %aid, "Aggregated installed agent from node");

                        // Wake the inventory-change notifier — the
                        // Desktop listens on `acowork/desktop/inventory`
                        // and refetches `GET /api/agents` on each
                        // signal. Idempotent if the same Node
                        // re-publishes its retained inventory on
                        // reconnect.
                        if let Some(t) = inventory_trigger.as_ref() {
                            t.notify();
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(node_id = %node_id_owned, agent_id, error = %e, "Bad InstalledAgentInfo envelope on installed topic");
                }
            }
        });
    } else if topic == acowork_core::mqtt_proto::USER_PROFILES_CHANGED_TOPIC {
        // ADR-084 §决策 4b: the user service owns `user_profiles.json`; this
        // tells us our cached copy (and the `last_user_profile` the Runtimes
        // hold, ADR-042) is behind. Pull + republish in the background: this
        // handler runs on the MQTT poll loop and must not await HTTP.
        let state = ctx.state.clone();
        let version = String::from_utf8_lossy(payload).trim().to_string();
        tokio::spawn(async move {
            tracing::debug!(version, "user profile change signal received");
            crate::lifecycle::user_profile_sync::refresh(&state).await;
        });
    }
}

/// Arm (or re-arm) the permanent-offline cleanup timer for `node_id`
/// (ADR-055 §6.13.3).
///
/// Called on every live offline signal. The timer fires after
/// [`NODE_OFFLINE_REMOVAL_GRACE`] and only acts when the node is still
/// offline at expiry — a node that reconnects inside the window
/// re-announces all of its retained records, so waiting is safe.
/// Re-arms are idempotent in effect: a duplicated timer finds its
/// traces already gone and no-ops.
fn schedule_node_offline_cleanup(ctx: &DispatchContext, node_id: &str) {
    let ctx_cleanup = ctx.clone();
    let node_id_owned = node_id.to_string();
    let registry = ctx.node_registry.clone();
    tokio::spawn(async move {
        tokio::time::sleep(NODE_OFFLINE_REMOVAL_GRACE).await;
        if registry.read().await.is_online(&node_id_owned) {
            tracing::debug!(
                node_id = %node_id_owned,
                "Node reconnected within grace period — cleanup skipped"
            );
            return;
        }
        tracing::warn!(
            node_id = %node_id_owned,
            grace_secs = NODE_OFFLINE_REMOVAL_GRACE.as_secs(),
            "Node offline beyond grace period — removing its records (ADR-055 §6.13.3)"
        );
        remove_node_records(&ctx_cleanup, &node_id_owned).await;
    });
}

/// Full record cleanup for a node that left the fleet for good
/// (ADR-055 §6.13.3).
///
/// Removes the `node.{id}` readiness subsystem (the orchestrator
/// recomputes the aggregated phase on the deregister broadcast), the
/// NodeRegistry record, every instance trace hosted on the node
/// (install table, running table, agent registry, in-memory cron
/// scheduler, persisted cron store), and clears the node's retained
/// broker topics (status / info / ready / lsps / per-instance
/// installed) so no replay or `nodes discover` scan can resurrect the
/// ghost.
///
/// Convergent: every step first checks for real traces, and when none
/// remain the call is a pure no-op. The retained-clear publications
/// echo back through the dispatcher and re-enter this function; the
/// trace check makes that echo a no-op, so cleanup cannot loop.
pub async fn remove_node_records(ctx: &DispatchContext, node_id: &str) {
    let subsystem = SubsystemId(format!("node.{}", node_id));

    // Gather traces first (one read pass) so the no-op decision and
    // the work list are consistent.
    let (instances, running_instances, had_subsystem) = {
        let gw = ctx.state.read().await;
        let instances: Vec<String> = gw
            .installed_agents
            .values()
            .filter(|i| i.node_id == node_id)
            .map(|i| i.instance_id.clone())
            .collect();
        let running: Vec<String> = gw
            .running_agents
            .values()
            .filter(|r| r.node_id == node_id)
            .map(|r| r.instance_id.clone())
            .collect();
        let had_subsystem = ctx
            .bootstrap_registry
            .as_ref()
            .map(|reg| reg.state(&subsystem).is_some())
            .unwrap_or(false);
        (instances, running, had_subsystem)
    };
    let had_registry_record = ctx.node_registry.read().await.get(node_id).is_some();

    if instances.is_empty()
        && running_instances.is_empty()
        && !had_subsystem
        && !had_registry_record
    {
        tracing::debug!(node_id, "Node cleanup found no traces — no-op");
        return;
    }

    tracing::info!(
        node_id,
        instances = instances.len(),
        running = running_instances.len(),
        had_subsystem,
        had_registry_record,
        "Removing node records (ADR-055 §6.13.3)"
    );

    // 1) Readiness subsystem. `deregister` broadcasts a Deregistered
    //    update, so the orchestrator recomputes the phase immediately
    //    without the dead `node.{id}` required subsystem.
    if let Some(registry) = ctx.bootstrap_registry.as_ref() {
        registry.deregister(&subsystem);
    }

    // 2) NodeRegistry record + replay-guard bookkeeping.
    ctx.node_registry.write().await.remove(node_id);
    ctx.node_replay_guard.forget(node_id);

    // 3) Per-instance tables (ADR-073 instance keys). Union install +
    //    running so an entry surviving its install record is covered.
    let mut all_instances = instances.clone();
    for id in &running_instances {
        if !all_instances.contains(id) {
            all_instances.push(id.clone());
        }
    }
    {
        let mut gw = ctx.state.write().await;
        for id in &all_instances {
            gw.remove_installed(id);
            gw.remove_running(id);
            gw.cron_scheduler.unregister_agent(id);
        }
    }
    // ADR-087 D7: instances that vanish with their node have no
    // ownership left to govern — prune their rows (node rows stay; the
    // machine's owner is an audit fact, and re-enrolling the same
    // node_id keeps it).
    if let Some(store) = ctx.agent_owners.as_ref() {
        let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
        for id in &all_instances {
            guard.remove(id);
        }
    }
    // Bulk uninstalls from a permanently-offline node also need to wake
    // the inventory notifier (Desktop subscribes to one signal per
    // change; coalescing handles the burst on the publish side).
    if let Some(t) = ctx.inventory_trigger.as_ref() {
        t.notify();
    }
    {
        let mut agents = ctx.agent_registry.write().await;
        for id in &all_instances {
            agents.remove(id);
        }
    }
    // Persisted cron entries (blocking file I/O off the async runtime,
    // same pattern as the HTTP uninstall path).
    let cron_store = ctx.state.read().await.cron_store.clone();
    if let Some(store) = cron_store {
        for id in &all_instances {
            let store = store.clone();
            let instance_id = id.clone();
            match tokio::task::spawn_blocking(move || store.delete_by_agent(&instance_id)).await {
                Ok(Ok(removed)) => {
                    if removed > 0 {
                        tracing::info!(
                            node_id,
                            instance_id = %id,
                            removed,
                            "Removed persisted cron entries for removed node"
                        );
                    }
                }
                Ok(Err(e)) => tracing::warn!(
                    node_id,
                    instance_id = %id,
                    error = %e,
                    "Failed to remove persisted cron entries"
                ),
                Err(e) => tracing::warn!(
                    node_id,
                    instance_id = %id,
                    error = %e,
                    "Cron store cleanup task failed"
                ),
            }
        }
    }

    // 4) Clear the retained node topics. The empty payloads echo back
    //    into the dispatcher and re-enter this function; the trace
    //    check above turns that echo into a no-op.
    if let Some(client) = ctx.mqtt_client.as_ref() {
        let node_topics = [
            acowork_core::node::node_status_topic(node_id),
            acowork_core::node::node_info_topic(node_id),
            acowork_core::node::node_ready_topic(node_id),
            acowork_core::node::node_lsps_topic(node_id),
        ];
        for topic in node_topics {
            if let Err(e) = client
                .publish_raw(&topic, Vec::new(), MqttQoS::AtLeastOnce, true)
                .await
            {
                tracing::warn!(node_id, topic, error = %e, "Failed to clear retained node topic");
            }
        }
        for id in &all_instances {
            let topic = acowork_core::node::node_agent_installed_topic(node_id, id);
            if let Err(e) = client
                .publish_raw(&topic, Vec::new(), MqttQoS::AtLeastOnce, true)
                .await
            {
                tracing::warn!(
                    node_id,
                    topic,
                    error = %e,
                    "Failed to clear retained installed topic"
                );
            }
        }
    }

    tracing::info!(node_id, "Node records removed");
}

/// Extract `(node_id, agent_id)` from
/// `acowork/nodes/{node_id}/agents/{agent_id}/installed`.
fn extract_installed_topic_ids(topic: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 6
        && parts[0] == "acowork"
        && parts[1] == "nodes"
        && parts[3] == "agents"
        && parts[5] == "installed"
    {
        let node_id = parts[2];
        let agent_id = parts[4];
        if !node_id.is_empty() && !agent_id.is_empty() {
            return Some((node_id.to_string(), agent_id.to_string()));
        }
    }
    None
}

/// Extract the instance id from
/// `acowork/nodes/{node_id}/agents/{instance_id}/events` — the reply
/// channel of a per-agent Node command (ADR-055 §6.2). Used by the
/// ADR-087 pending-install rollback: a failed install must drop its
/// staged owner row, and only the topic carries the instance identity
/// (the NodeEvent itself is keyed by `request_id`).
fn extract_agent_events_instance_id(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 6
        && parts[0] == "acowork"
        && parts[1] == "nodes"
        && parts[3] == "agents"
        && parts[5] == "events"
        && !parts[4].is_empty()
    {
        return Some(parts[4].to_string());
    }
    None
}

/// Extract `node_id` from `acowork/nodes/{node_id}/enroll`.
fn extract_enroll_topic_node_id(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 4 && parts[0] == "acowork" && parts[1] == "nodes" && parts[3] == "enroll" {
        let node_id = parts[2];
        if !node_id.is_empty() {
            return Some(node_id.to_string());
        }
    }
    None
}

/// Extract `node_id` from `acowork/nodes/{node_id}/status`.
fn extract_node_id_from_status_topic(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 4
        && parts[0] == "acowork"
        && parts[1] == "nodes"
        && parts[3] == "status"
    {
        let node_id = parts[2];
        if !node_id.is_empty() {
            return Some(node_id.to_string());
        }
    }
    None
}

/// Extract `node_id` from `acowork/nodes/{node_id}/ready` (ADR-059 §7.2).
fn extract_node_id_from_ready_topic(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 4
        && parts[0] == "acowork"
        && parts[1] == "nodes"
        && parts[3] == "ready"
    {
        let node_id = parts[2];
        if !node_id.is_empty() {
            return Some(node_id.to_string());
        }
    }
    None
}

/// Outcome of the enrollment decision (ADR-055 Phase 5a).
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnrollDecision {
    /// Accepted; `reuse` is true when the node re-enrolls (its existing
    /// node token is reused). `consume_enrollment_token` is true when
    /// the presented token is a one-time enrollment token that must be
    /// consumed afterwards (false for a pre-issued node token — local-
    /// node path).
    Accept {
        reuse: bool,
        consume_enrollment_token: bool,
    },
    /// Rejected, with the reason surfaced in the `enroll_result`.
    Reject { reason: String },
}

/// Pure read-only enrollment decision.
///
/// 1. When `auth_enabled`, the presented token must be either a valid
///    (known, unexpired, unconsumed) one-time enrollment token — the
///    standard path — OR the node's own long-lived token (pre-issued
///    local-node credential; mirrors the broker CONNECT check).
/// 2. ADR-075 D7: `node_id` is a globally-unique UUID v4, so there is
///    no conflict detection — a node_id that already has a record
///    reuses its token (idempotent re-enrollment), a fresh node_id is
///    accepted and minted on first enrollment.
///
/// All side effects (token consumption, minting, registry write, reply)
/// are performed by the caller after this returns.
fn decide_enroll(
    node_id: &str,
    enrollment_token: Option<&str>,
    auth_enabled: bool,
    enrollment_store: Option<&EnrollmentTokenStore>,
    node_store: Option<&NodeTokenStore>,
) -> EnrollDecision {
    let mut consume_enrollment_token = false;
    if auth_enabled {
        let Some(token) = enrollment_token else {
            return EnrollDecision::Reject {
                reason: "enrollment token required (mqtt.auth_enabled)".to_string(),
            };
        };
        let valid_enrollment = enrollment_store.map(|s| s.validate_token(token))
            == Some(TokenValidation::Valid);
        let valid_node_token = node_store
            .map(|s| s.node_token_matches(node_id, token))
            .unwrap_or(false);
        if valid_enrollment {
            consume_enrollment_token = true;
        } else if !valid_node_token {
            let reason = match enrollment_store.map(|s| s.validate_token(token)) {
                Some(TokenValidation::Expired) => "enrollment token expired",
                Some(TokenValidation::Consumed) => "enrollment token already used",
                Some(TokenValidation::Unknown) | None => "unknown enrollment token",
                Some(TokenValidation::Valid) => unreachable!(),
            };
            return EnrollDecision::Reject {
                reason: reason.to_string(),
            };
        }
    }
    // ADR-075 D7: UUID node_ids are globally unique — no conflict
    // detection. A node_id that already has a record reuses its token;
    // a fresh node_id is accepted and minted on first enrollment.
    EnrollDecision::Accept {
        reuse: node_store
            .map(|s| s.get(node_id).is_some())
            .unwrap_or(false),
        consume_enrollment_token,
    }
}

/// Parse a `DataEnvelope<NodeEnroll>` payload and enforce
/// topic/payload consistency: the payload's node_id must equal the
/// topic's node_id (otherwise a node could register a peer's identity).
/// Returns the optional one-time enrollment token.
fn parse_enroll_payload(payload: &[u8], topic_node_id: &str) -> Result<Option<String>, String> {
    let envelope = DataEnvelope::decode(payload).map_err(|e| format!("bad DataEnvelope: {e}"))?;
    let enroll = match envelope.payload {
        Some(data_envelope::Payload::NodeEnroll(enroll)) => enroll,
        _ => return Err("payload is not a NodeEnroll envelope".to_string()),
    };
    if enroll.node_id != topic_node_id {
        return Err(format!(
            "payload node_id '{}' does not match topic node_id '{}'",
            enroll.node_id, topic_node_id
        ));
    }
    let token = if enroll.enrollment_token.is_empty() {
        None
    } else {
        Some(enroll.enrollment_token)
    };
    Ok(token)
}

/// Process an enrollment request end-to-end: validate → mint/reuse
/// node token → record in NodeRegistry → reply `enroll_result`.
///
/// Runs on the tokio runtime; the std mutex guards (non-Send) are
/// scoped so they never live across an await.
#[allow(clippy::too_many_arguments)]
async fn process_enroll_message(
    payload: Vec<u8>,
    node_id: String,
    node_registry: SharedNodeRegistry,
    mqtt_client: Option<Arc<GatewayMqttClient>>,
    enrollment_tokens: Option<SharedEnrollmentTokenStore>,
    node_tokens: Option<SharedNodeTokenStore>,
    node_owners: Option<crate::gateway::ownership::SharedOwnershipStore>,
    auth_enabled: bool,
    default_owner: Option<String>,
) {
    let enrollment_token = match parse_enroll_payload(&payload, &node_id) {
        Ok(ok) => ok,
        Err(error) => {
            tracing::warn!(node_id = %node_id, error = %error, "dropping malformed enroll message");
            return;
        }
    };

    // Read-only decision; both guards drop before any await below
    // (std::sync::MutexGuard is not Send, so it cannot cross a
    // tokio::spawn boundary or an await point).
    let decision = {
        let enroll_guard = enrollment_tokens
            .as_ref()
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()));
        let node_guard = node_tokens
            .as_ref()
            .map(|s| s.lock().unwrap_or_else(|e| e.into_inner()));
        decide_enroll(
            &node_id,
            enrollment_token.as_deref(),
            auth_enabled,
            enroll_guard.as_deref(),
            node_guard.as_deref(),
        )
    };

    match decision {
        EnrollDecision::Reject { reason } => {
            tracing::warn!(node_id = %node_id, reason = %reason, "Enrollment rejected");
            publish_enroll_result(
                &mqtt_client,
                NodeEnrollResult {
                    node_id,
                    node_token: String::new(),
                    status: "rejected".to_string(),
                    message: reason,
                },
            )
            .await;
        }
        EnrollDecision::Accept {
            reuse,
            consume_enrollment_token,
        } => {
            // Consume the one-time enrollment token when the decision
            // says so (a pre-issued node token is long-lived and is
            // left untouched). Validation ran under the same mutex
            // scope above, so a failure here means concurrent use.
            if auth_enabled && consume_enrollment_token {
                let consumed = match (enrollment_token.as_deref(), enrollment_tokens.as_ref()) {
                    (Some(token), Some(store)) => store
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .consume_token(token, &node_id),
                    _ => false,
                };
                if !consumed {
                    tracing::warn!(node_id = %node_id, "enrollment token consumption failed");
                    publish_enroll_result(
                        &mqtt_client,
                        NodeEnrollResult {
                            node_id,
                            node_token: String::new(),
                            status: "rejected".to_string(),
                            message: "enrollment token already used".to_string(),
                        },
                    )
                    .await;
                    return;
                }
            }

            // ADR-087 D2: bind the enrollment token's creator as the
            // node owner (first enrollment wins — `put_if_absent` keeps
            // a re-enroll from changing the owner). Tokens with no
            // creator (CLI-issued, Gateway-spawned local node before an
            // admin existed) fall back to the default owner, so no
            // enroll path mints an ownerless node when an admin exists.
            if consume_enrollment_token
                && let (Some(token), Some(owners)) =
                    (enrollment_token.as_deref(), node_owners.as_ref())
            {
                let owner = enrollment_tokens
                    .as_ref()
                    .and_then(|store| {
                        store
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .token_owner(token)
                    })
                    .or_else(|| default_owner.clone());
                if let Some(owner) = owner {
                    owners
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .put_if_absent(
                            &node_id,
                            crate::gateway::ownership::OwnerRecord::new(Some(owner)),
                        );
                }
            }

            let node_token = match node_tokens.as_ref() {
                Some(store) => store
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .upsert(&node_id),
                // No store wired (unit tests): mint a one-off token so
                // the handshake still completes with a usable credential.
                None => crate::mqtt::enrollment::generate_token(),
            };

            // Registry write is a tokio RwLock — safe to await here;
            // the std mutex guards above are already dropped.
            node_registry
                .write()
                .await
                .set_node_token(&node_id, node_token.clone());

            tracing::info!(node_id = %node_id, reuse, "Node enrolled");
            publish_enroll_result(
                &mqtt_client,
                NodeEnrollResult {
                    node_id,
                    node_token,
                    status: "ok".to_string(),
                    message: String::new(),
                },
            )
            .await;
        }
    }
}

/// Publish an `enroll_result` reply on `acowork/nodes/{id}/enroll_result`
/// (QoS 1, not retained — the reply is per-request). No-op without a
/// client (tests / broker-less dispatch).
async fn publish_enroll_result(
    mqtt_client: &Option<Arc<GatewayMqttClient>>,
    result: NodeEnrollResult,
) {
    let Some(client) = mqtt_client else { return };
    let node_id = result.node_id.clone();
    let topic = acowork_core::node::node_enroll_result_topic(&node_id);
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeEnrollResult(result)),
    };
    if let Err(e) = client
        .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
        .await
    {
        tracing::warn!(node_id = %node_id, error = %e, "failed to publish enroll result");
    }
}

/// Extract `agent_id` from `acowork/agents/{agent_id}/status`.
///
/// Unlike `extract_agent_id_from_topic` (which is permissive), this
/// version requires the exact 4-segment shape of the status topic.
fn extract_agent_id_from_status_topic(topic: &str) -> Option<String> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() == 4 && parts[0] == "acowork" && parts[1] == "agents" && parts[3] == "status" {
        let agent_id = parts[2];
        if !agent_id.is_empty() {
            return Some(agent_id.to_string());
        }
    }
    None
}

/// Re-install (or keep) a `running_agents` entry on an online status
/// transition. Symmetric counterpart of the `offline` branch in the
/// status dispatch path.
///
/// **Why this exists**: the original code only removed the
/// `running_agents` entry on `status=offline`. After an OS sleep/wake
/// the broker fires LWT `offline` (which correctly removes the entry),
/// then 100-300 ms later the Runtime reconnects and publishes
/// `online` — but no path re-installed the entry, so the Gateway's
/// view of "running" diverged from the broker's view of "online". The
/// Desktop showed `休眠`, POST `/start` short-circuited via the
/// ADR-055 idempotent fast-path (because `is_online` is true), and
/// the user could not recover.
///
/// Idempotent: preserves the existing entry's `lifecycle`, `dev_mode`,
/// `debug_state` and `started_at` fields if the entry already
/// exists. The status dispatch handler refines `lifecycle` separately,
/// so this helper only needs to install the bare-minimum liveness shape.
///
/// Auto-sleep was removed in Sept 2026 — this helper now only handles
/// the online transition; `sleeping` no longer exists as a state.
async fn track_running_agent_for_status(state: &SharedState, agent_id: &str) {
    let mut gw = state.write().await;
    if gw.running_agents.contains_key(agent_id) {
        // Already tracked — keep the existing rich fields (ready,
        // dev_mode, debug_state, started_at) and just return. Liveness
        // is read from the AgentRegistry (MQTT), never re-derived here.
        tracing::debug!(
            agent_id,
            "running_agents: entry already tracked; keeping rich fields"
        );
        return;
    }
    // The status topic (`acowork/agents/{id}/status`) carries no node
    // identity, so the install record is the only place to learn which
    // node hosts this Runtime. ADR-075 D6: only a MISSING record falls
    // back to the `"local"` bookkeeping anchor — hardcoding it here
    // mis-attributed remote-node Runtimes whenever the Gateway had to
    // re-install the entry (restart / post-wake), which the Desktop's
    // per-node grouping then surfaced as a bogus `local` node.
    let (workspace, resolved_agent_id, node_id) = match gw.installed(agent_id) {
        Some(i) => (
            std::path::PathBuf::from(&i.install_path)
                .join("workspace")
                .to_string_lossy()
                .to_string(),
            i.agent_id.clone(),
            i.node_id.clone(),
        ),
        None => (
            String::new(),
            agent_id.to_string(),
            acowork_core::node::LOCAL_NODE_ID.to_string(),
        ),
    };
    gw.add_running(crate::gateway::state::RunningAgentInfo {
        instance_id: agent_id.to_string(),
        agent_id: resolved_agent_id,
        // Node-hosted Runtimes are tracked with `pid = 0`; liveness
        // is guaranteed by the MQTT broker's LWT registry, not by a
        // local process probe.
        pid: 0,
        started_at: chrono::Utc::now(),
        workspace,
        // Hosting node from the install record, `"local"` only when the
        // record is missing (ADR-075 D6).
        node_id,
        // ADR-085: the lifecycle starts at OFFLINE and is upgraded by
        // the status envelope's `state` field the moment the next
        // (retained) publish lands — Desktop capability gates read the
        // state directly, no separate ready signal exists anymore.
        lifecycle: acowork_core::mqtt_proto::AgentLifecycleState::Offline,
        lifecycle_detail: String::new(),
        // Status is online-class; the Runtime's session has been resumed.
        // dev_mode / debug_state default to off until refined by the
        // start path / debug flow.
        dev_mode: false,
        debug_state: crate::gateway::state::DebugState::Disabled,
        debug_port: None,
        workspace_config_json: None,
        current_embed_dim: None,
        migration: None,
    });
    tracing::info!(
        agent_id,
        "Auto-tracked node-hosted Runtime from status=online (pid=0, post-wake recovery)"
    );
}

/// Best-effort full reconciliation pass.
///
/// Walks every installed agent, asks the AgentRegistry for the
/// authoritative online/offline state, and re-aligns `running_agents`
/// to match. Designed as the final safety net for the post-wake
/// recovery flow:
///
/// 1. The Gateway MQTT client subscribes to `acowork/agents/+/status`.
///    On every (re)connect the broker replays retained messages.
/// 2. If the Gateway itself (or the broker) restarted while a Runtime
///    was alive, the retained `online` snapshot re-installs the entry
///    here in addition to the live event-driven path.
/// 3. The `reconcile_loop` task in `gateway/mod.rs` calls this every
///    `RECONCILE_INTERVAL` (its first tick fires immediately after
///    the loop starts), so any divergence between `running_agents`
///    (process view) and `agent_registry` (broker view) self-heals
///    without user intervention. A Gateway (re)connect converges
///    through the dispatch event path first — the broker replays
///    retained statuses — with this loop as the interval backstop.
///
/// **Not a substitute** for the dispatch event handlers — those are
/// the low-latency path. This is the consistency / idempotency
/// backstop that catches everything the event path missed (e.g. a
/// crash between the LWT `offline` and the live `online` re-publish).
pub async fn reconcile_running_agents(state: &SharedState, agent_registry: &SharedAgentRegistry) {
    let reg = agent_registry.read().await;
    let online_set: std::collections::HashSet<String> = reg
        .snapshot()
        .into_iter()
        .filter(|(_, s)| s.online)
        .map(|(id, _)| id)
        .collect();
    drop(reg);

    let mut gw = state.write().await;
    // Install missing entries for every agent the broker reports as online.
    let installed_ids: Vec<String> = gw.installed_agents.keys().cloned().collect();
    for agent_id in &installed_ids {
        if online_set.contains(agent_id) && !gw.running_agents.contains_key(agent_id) {
            let workspace = gw
                .installed(agent_id)
                .map(|i| {
                    std::path::PathBuf::from(&i.install_path)
                        .join("workspace")
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_default();
            let resolved_agent_id = gw
                .installed(agent_id)
                .map(|i| i.agent_id.clone())
                .unwrap_or_else(|| agent_id.clone());
            gw.add_running(crate::gateway::state::RunningAgentInfo {
                instance_id: agent_id.clone(),
                agent_id: resolved_agent_id,
                pid: 0,
                started_at: chrono::Utc::now(),
                workspace,
                // ADR-075 D6: fallback anchor for the local node.
                node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                lifecycle: acowork_core::mqtt_proto::AgentLifecycleState::Offline,
                lifecycle_detail: String::new(),
                dev_mode: false,
                debug_state: crate::gateway::state::DebugState::Disabled,
                debug_port: None,
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
            tracing::info!(
                agent_id = %agent_id,
                "reconcile_running_agents: installed missing entry from broker online view"
            );
        }
    }
    // Drop stale entries that the broker no longer reports as online.
    //
    // Grace window: an entry younger than RECONCILE_STALE_GRACE_SECS
    // is treated as in-flight — a just-acknowledged start whose
    // Runtime has not published its retained `online` yet (cold boot
    // after an OS wake can take longer than one tick). Dropping it
    // would silently undo the start and flick the UI back to
    // "stopped"; the next tick can drop it once it truly ages out.
    let stale: Vec<String> = gw
        .running_agents
        .iter()
        .filter(|(id, info)| {
            if online_set.contains(id.as_str()) {
                return false;
            }
            let age = chrono::Utc::now().signed_duration_since(info.started_at);
            age.num_seconds() >= RECONCILE_STALE_GRACE_SECS as i64
        })
        .map(|(id, _)| id.clone())
        .collect();
    for agent_id in stale {
        gw.remove_running(&agent_id);
        tracing::info!(
            agent_id = %agent_id,
            grace_secs = RECONCILE_STALE_GRACE_SECS,
            "reconcile_running_agents: dropped stale entry not present in broker online view"
        );
    }
}

/// Periodic reconcile loop (background task). Spawned by
/// `Gateway::run` after the MQTT client connects.
///
/// Cadence: [`RECONCILE_INTERVAL_SECS`] (5 s by default). Each tick
/// re-aligns `running_agents` with the authoritative
/// `agent_registry` view. This is the consistency backstop for the
/// dispatch event handlers: if a transition is missed (broker
/// retained replay ordering, race between LWT `offline` and live
/// `online`, etc.) the next tick self-heals without user
/// intervention.
///
/// The helper body is a couple of `HashMap` walks and `GatewayState`
/// helper calls — no I/O, no allocation in the hot path. We don't
/// wrap it in a `catch_unwind`: a panic here means the gateway is
/// in an undefined state and the right thing to do is let the
/// task die and let the supervisor (or operator) restart the
/// process. Tokio logs a loud warning on task death, which is the
/// right observability surface.
pub async fn run_reconcile_loop(state: SharedState, agent_registry: SharedAgentRegistry) {
    let interval = std::time::Duration::from_secs(RECONCILE_INTERVAL_SECS);
    let mut ticker = tokio::time::interval(interval);
    // First tick fires immediately (the Gateway just started —
    // any retained online snapshots from Runtimes that connected
    // before the Gateway did are now sitting in `agent_registry`).
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(
        interval_secs = RECONCILE_INTERVAL_SECS,
        "reconcile_loop: first tick (immediate)"
    );
    loop {
        ticker.tick().await;
        reconcile_running_agents(&state, &agent_registry).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state::GatewayState;
    use std::sync::Arc;

    /// Test-only instance identities (ADR-073: must be UUIDv4).
    const INSTANCE_ARCHITECT: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    const INSTANCE_STALE: &str = "8c9d0e1f-2a3b-4c4c-8d5d-be6f7a8b9c0d";
    const INSTANCE_BOOTING: &str = "9d0e1f2a-3b4c-4d5d-8e6e-cf7a8b9c0d1e";
    use tokio::sync::RwLock;

    fn test_state() -> SharedState {
        Arc::new(RwLock::new(GatewayState::new(
            "/tmp/acowork-dispatch-test-vault",
        )))
    }

    /// Shared dispatch context wired with the given bootstrap registry
    /// (the replay guard must be shared across messages, so build one
    /// context per test rather than per call).
    fn ctx_with_registry(
        registry: crate::bootstrap::SharedSubsystemReadinessRegistry,
    ) -> DispatchContext {
        DispatchContext {
            runtime_http_registry: crate::http::proxy::new_shared_registry(),
            agent_registry: crate::mqtt::agent_registry::new_shared_registry(),
            node_registry: crate::mqtt::node_registry::new_shared_registry(),
            state: test_state(),
            bootstrap_registry: Some(registry),
            ..Default::default()
        }
    }

    fn node_ready_payload(node_id: &str) -> Vec<u8> {
        let ready = acowork_core::mqtt_proto::NodeReady {
            node_id: node_id.to_string(),
            protocol_version: 1,
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeReady(ready)),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    /// ADR-085: build an `AgentStatus` envelope for status-topic dispatch tests.
    fn agent_status_payload(
        agent_id: &str,
        online: bool,
        state: acowork_core::mqtt_proto::AgentLifecycleState,
    ) -> Vec<u8> {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentStatus(
                acowork_core::mqtt_proto::AgentStatus {
                    agent_id: agent_id.to_string(),
                    online,
                    instance_id: agent_id.to_string(),
                    node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                    state: state as i32,
                    detail: String::new(),
                },
            )),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[tokio::test]
    async fn node_ready_replay_does_not_demote_reconnected_node() {
        // Regression for the ADR-059 §7.2 replay race: when the
        // Gateway MQTT client reconnects, the broker replays the
        // retained snapshot asynchronously. A stale EMPTY NodeReady
        // (the node cleared it on disconnect BEFORE the Gateway
        // subscribed) or a stale LWT offline can be delivered AFTER
        // the node's live re-announcement marked it ready. They must
        // not demote the node back to BOOTING — that would stick
        // bootstrap in BOOTING and the Runtime's LLM availability in
        // LOADING until the node's next reconnect.
        //
        // The fixture node id is `nytb` to mirror the unified topology
        // where every node is identified by its machine slug.
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let ctx = ctx_with_registry(registry.clone());
        // The Gateway MQTT client (re)connected — this is what makes
        // later offline signals candidates for stale retained replays.
        ctx.node_replay_guard.mark_gateway_reconnect();

        // Live re-announcement: retained non-empty NodeReady.
        handle_plaintext_message(
            "acowork/nodes/nytb/ready",
            &node_ready_payload("nytb"),
            &ctx,
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.state(&SubsystemId("node.nytb".to_string())),
            Some(crate::bootstrap::SubsystemState::Ready)
        );

        // Stale replay: empty retained snapshot delivered late.
        handle_plaintext_message("acowork/nodes/nytb/ready", &[], &ctx);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.state(&SubsystemId("node.nytb".to_string())),
            Some(crate::bootstrap::SubsystemState::Ready),
            "stale empty NodeReady replay must not demote a reconnected node"
        );

        // Stale replay: LWT offline delivered late.
        handle_plaintext_message("acowork/nodes/nytb/status", b"offline", &ctx);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.state(&SubsystemId("node.nytb".to_string())),
            Some(crate::bootstrap::SubsystemState::Ready),
            "stale offline replay must not demote a reconnected node"
        );
    }

    #[tokio::test]
    async fn node_offline_demotes_when_not_a_replay() {
        // A genuine offline signal (no recent live online signal) must
        // still demote a ready node — the replay guard only suppresses
        // the reconnect window, not real LWT offlines (ADR-059 §7.2).
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let ctx = ctx_with_registry(registry.clone());
        let handle = registry.register("node.nytb", crate::bootstrap::ReadinessKind::Optional);
        handle.mark_ready(None);

        handle_plaintext_message("acowork/nodes/nytb/status", b"offline", &ctx);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.state(&SubsystemId("node.nytb".to_string())),
            Some(crate::bootstrap::SubsystemState::Booting),
            "genuine offline must demote a ready node"
        );
    }

    #[tokio::test]
    async fn node_offline_demotes_even_after_gateway_reconnect_when_node_stayed_offline() {
        // The Gateway reconnected (replays exist) but the node never
        // re-announced — its offline signal is genuine and must still
        // demote (the guard requires a node online signal AFTER the
        // gateway reconnect).
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let ctx = ctx_with_registry(registry.clone());
        ctx.node_replay_guard.mark_gateway_reconnect();
        let handle = registry.register("node.nytb", crate::bootstrap::ReadinessKind::Optional);
        handle.mark_ready(None);

        handle_plaintext_message("acowork/nodes/nytb/status", b"offline", &ctx);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.state(&SubsystemId("node.nytb".to_string())),
            Some(crate::bootstrap::SubsystemState::Booting),
            "offline must demote when the node never re-announced after the gateway reconnect"
        );
    }

    #[tokio::test]
    async fn node_offline_keeps_aggregate_phase_ready() {
        // Regression: a killed remote node used to demote its
        // `node.{id}` subsystem from Required+Ready to Booting, which
        // pinned the aggregated phase in BOOTING — every starting
        // Desktop waited on "N/M required ready" until the 120s
        // offline-removal grace deregistered the node. Node subsystems
        // are Optional now: the demotion must NOT move the aggregate
        // out of READY, while the snapshot keeps re-publishing (fresh
        // version) so clients refresh their node topology.
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let orchestrator =
            crate::bootstrap::BootstrapOrchestrator::new("instance-A".into(), registry.clone());
        registry
            .register("vault", crate::bootstrap::ReadinessKind::Required)
            .mark_ready(None);
        registry
            .register("publisher", crate::bootstrap::ReadinessKind::Required)
            .mark_ready(None);
        let ctx = ctx_with_registry(registry.clone());

        // Node comes online through the real topic path — registered
        // as Optional and marked ready.
        handle_plaintext_message(
            "acowork/nodes/nytb/ready",
            &node_ready_payload("nytb"),
            &ctx,
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        while orchestrator.snapshot().phase != crate::bootstrap::orchestrator::BootstrapPhase::Ready
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            orchestrator.snapshot().phase,
            crate::bootstrap::orchestrator::BootstrapPhase::Ready
        );

        // Node is killed: the genuine offline demotes the node
        // subsystem itself…
        handle_plaintext_message("acowork/nodes/nytb/status", b"offline", &ctx);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.state(&SubsystemId("node.nytb".to_string())),
            Some(crate::bootstrap::SubsystemState::Booting),
            "offline must still demote the node subsystem itself"
        );
        // …but the aggregated phase must stay READY.
        assert_eq!(
            orchestrator.snapshot().phase,
            crate::bootstrap::orchestrator::BootstrapPhase::Ready,
            "an offline node must not drag the aggregate out of READY"
        );
    }

    #[tokio::test]
    async fn empty_status_payload_removes_node_records() {
        // ADR-055 §6.13.3: an empty retained status is a removal
        // signal — the subsystem is deregistered (the phase recomputes
        // without the dead required subsystem) and the NodeRegistry
        // record is dropped.
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let mut ctx = ctx_with_registry(registry.clone());
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        node_reg
            .write()
            .await
            .update_status_from_mqtt("acowork/nodes/nytb/status", b"online");
        ctx.node_registry = node_reg.clone();

        let handle = registry.register("node.nytb", crate::bootstrap::ReadinessKind::Optional);
        handle.mark_ready(None);

        handle_plaintext_message("acowork/nodes/nytb/status", &[], &ctx);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(
            registry.state(&SubsystemId("node.nytb".to_string())),
            None,
            "empty status must deregister the node subsystem"
        );
        assert!(node_reg.read().await.get("nytb").is_none());
    }

    #[tokio::test]
    async fn empty_ready_does_not_resurrect_removed_subsystem() {
        // The cleanup's own empty-ready publish must not re-create the
        // subsystem it just removed (no resurrection): an empty ready
        // for an unknown subsystem is a no-op.
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let ctx = ctx_with_registry(registry.clone());

        handle_plaintext_message("acowork/nodes/ghost/ready", &[], &ctx);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            registry.state(&SubsystemId("node.ghost".to_string())),
            None,
            "empty ready must not register a subsystem"
        );
    }

    #[tokio::test]
    async fn node_re_announcement_after_removal_rebuilds_subsystem() {
        // Self-heal guarantee: a node that reconnects after its records
        // were removed re-announces NodeReady and is registered again
        // through the normal path.
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let ctx = ctx_with_registry(registry.clone());
        let handle = registry.register("node.nytb", crate::bootstrap::ReadinessKind::Optional);
        handle.mark_ready(None);

        handle_plaintext_message("acowork/nodes/nytb/status", &[], &ctx);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(registry.state(&SubsystemId("node.nytb".to_string())), None);

        handle_plaintext_message(
            "acowork/nodes/nytb/ready",
            &node_ready_payload("nytb"),
            &ctx,
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            registry.state(&SubsystemId("node.nytb".to_string())),
            Some(crate::bootstrap::SubsystemState::Ready)
        );
    }

    #[tokio::test]
    async fn test_node_status_topic_updates_node_registry() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        handle_plaintext_message(
            "acowork/nodes/local/status",
            b"online",
            &DispatchContext {
                runtime_http_registry: http_reg.clone(),
                agent_registry: agent_reg.clone(),
                node_registry: node_reg.clone(),
                state,
                ..Default::default()
            },
        );
        // handle_plaintext_message spawns a tokio task; wait for it.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(node_reg.read().await.is_online("local"));
    }

    /// ADR-085: a node-hosted Runtime that announces an online status
    /// envelope without having been started through POST /start must be
    /// auto-tracked as pid=0 (so GET /api/agents surfaces it as running),
    /// and the envelope's `state` mirrors into `RunningAgentInfo.lifecycle`.
    #[tokio::test]
    async fn test_status_topic_auto_tracks_node_hosted_runtime() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        let payload = agent_status_payload(
            "com.acowork.senior-engineer",
            true,
            acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady,
        );
        handle_plaintext_message(
            "acowork/agents/com.acowork.senior-engineer/status",
            &payload,
            &DispatchContext {
                runtime_http_registry: http_reg.clone(),
                agent_registry: agent_reg.clone(),
                node_registry: node_reg.clone(),
                state: state.clone(),
                ..Default::default()
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let gw = state.read().await;
        let entry = gw
            .running_agents
            .get("com.acowork.senior-engineer")
            .expect("node-hosted Runtime must be auto-tracked from its online status envelope");
        assert_eq!(entry.pid, 0, "node-hosted Runtime is tracked with pid=0");
        assert_eq!(
            entry.lifecycle,
            acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady,
            "status.state must mirror into RunningAgentInfo.lifecycle"
        );
    }

    /// ADR-085 D3 / §8.1: a status envelope from a newer encoder with an
    /// unknown `state` discriminant must degrade to `UNSPECIFIED` (never
    /// an error, never a capability state) — every consumer gates
    /// UNSPECIFIED like OFFLINE, so an unknown value cannot be misread
    /// as "ready". The `detail` field must still ride through.
    #[tokio::test]
    async fn test_status_unknown_state_degrades_to_unspecified() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        // `state = 99` — a discriminant outside AgentLifecycleState
        // (simulates an encoder from a newer protocol version).
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentStatus(
                acowork_core::mqtt_proto::AgentStatus {
                    agent_id: "com.acowork.senior-engineer".to_string(),
                    online: true,
                    instance_id: "com.acowork.senior-engineer".to_string(),
                    node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                    state: 99,
                    detail: "from a newer runtime".to_string(),
                },
            )),
        };
        handle_plaintext_message(
            "acowork/agents/com.acowork.senior-engineer/status",
            &prost::Message::encode_to_vec(&envelope),
            &DispatchContext {
                runtime_http_registry: http_reg.clone(),
                agent_registry: agent_reg.clone(),
                node_registry: node_reg.clone(),
                state: state.clone(),
                ..Default::default()
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let gw = state.read().await;
        let entry = gw
            .running_agents
            .get("com.acowork.senior-engineer")
            .expect("unknown state must not reject the envelope — the Runtime stays tracked");
        assert_eq!(
            entry.lifecycle,
            acowork_core::mqtt_proto::AgentLifecycleState::Unspecified,
            "unknown discriminant must decode to UNSPECIFIED, never a capability state"
        );
    }

    /// ADR-085 invariant 6: an `online=false` status envelope (the LWT path
    /// — crash or clean exit) removes the tracked entry.
    #[tokio::test]
    async fn test_status_offline_envelope_removes_tracked_runtime() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        let ctx = DispatchContext {
            runtime_http_registry: http_reg.clone(),
            agent_registry: agent_reg.clone(),
            node_registry: node_reg.clone(),
            state: state.clone(),
            ..Default::default()
        };
        handle_plaintext_message(
            "acowork/agents/com.acowork.senior-engineer/status",
            &agent_status_payload(
                "com.acowork.senior-engineer",
                true,
                acowork_core::mqtt_proto::AgentLifecycleState::HttpReady,
            ),
            &ctx,
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle_plaintext_message(
            "acowork/agents/com.acowork.senior-engineer/status",
            &agent_status_payload(
                "com.acowork.senior-engineer",
                false,
                acowork_core::mqtt_proto::AgentLifecycleState::Offline,
            ),
            &ctx,
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let gw = state.read().await;
        assert!(
            !gw.running_agents.contains_key("com.acowork.senior-engineer"),
            "online=false must remove the tracked Runtime (invariant 6)"
        );
    }

    #[tokio::test]
    async fn test_node_info_topic_updates_node_registry() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        let info = acowork_core::mqtt_proto::NodeInfo {
            node_id: "gpu-1".to_string(),
            hostname: "h".to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec![],
            max_agents: 16,
            agent_count: 0,
            http_endpoint: "http://127.0.0.1:19900".to_string(),
            node_name: "gpu-1".to_string(),
            gateway_managed: true,
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeInfo(info)),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        handle_plaintext_message(
            "acowork/nodes/gpu-1/info",
            &payload,
            &DispatchContext {
                runtime_http_registry: http_reg.clone(),
                agent_registry: agent_reg.clone(),
                node_registry: node_reg.clone(),
                state,
                ..Default::default()
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reg = node_reg.read().await;
        assert_eq!(
            reg.get("gpu-1").and_then(|n| n.node_name.clone()),
            Some("gpu-1".to_string())
        );
    }

    #[tokio::test]
    async fn test_node_lsps_topic_updates_node_registry() {
        let http_reg = crate::http::proxy::new_shared_registry();
        let agent_reg = crate::mqtt::agent_registry::new_shared_registry();
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();

        let lsps = acowork_core::mqtt_proto::AvailableLsps {
            version: 1,
            endpoint: "http://192.168.1.10:19878".to_string(),
            ready: true,
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AvailableLsps(lsps)),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        handle_plaintext_message(
            "acowork/nodes/gpu-1/lsps",
            &payload,
            &DispatchContext {
                runtime_http_registry: http_reg.clone(),
                agent_registry: agent_reg.clone(),
                node_registry: node_reg.clone(),
                state,
                ..Default::default()
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reg = node_reg.read().await;
        assert_eq!(
            reg.get("gpu-1").and_then(|n| n.lsp_endpoint.clone()),
            Some("http://192.168.1.10:19878".to_string())
        );
    }

    fn temp_test_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("acowork-dispatch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn enroll_payload(node_id: &str, token: Option<&str>) -> Vec<u8> {
        let enroll = acowork_core::mqtt_proto::NodeEnroll {
            node_id: node_id.to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec![],
            enrollment_token: token.unwrap_or("").to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeEnroll(enroll)),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn extract_enroll_topic_node_id_ok() {
        assert_eq!(
            extract_enroll_topic_node_id("acowork/nodes/gpu-1/enroll"),
            Some("gpu-1".to_string())
        );
        assert_eq!(extract_enroll_topic_node_id("acowork/nodes//enroll"), None);
        assert_eq!(extract_enroll_topic_node_id("acowork/nodes/gpu-1/info"), None);
        assert_eq!(extract_enroll_topic_node_id("acowork/nodes/gpu-1/enroll/x"), None);
    }

    #[test]
    fn parse_enroll_payload_enforces_topic_consistency() {
        let payload = enroll_payload("gpu-1", Some("tok-1"));
        let token = parse_enroll_payload(&payload, "gpu-1").expect("valid enroll parses");
        assert_eq!(token.as_deref(), Some("tok-1"));

        // Topic/payload node_id mismatch must be rejected (a node
        // cannot enroll under a peer's node_id via the topic).
        assert!(parse_enroll_payload(&payload, "other-node").is_err());

        // Empty enrollment token is normalized to None.
        let no_token = enroll_payload("gpu-1", None);
        let token = parse_enroll_payload(&no_token, "gpu-1").expect("token-less enroll parses");
        assert!(token.is_none());

        // Non-NodeEnroll envelope payloads are rejected.
        let info = acowork_core::mqtt_proto::NodeInfo {
            node_id: "gpu-1".to_string(),
            hostname: "h".to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec![],
            max_agents: 16,
            agent_count: 0,
            http_endpoint: "http://127.0.0.1:19900".to_string(),
            node_name: "gpu-1".to_string(),
            gateway_managed: false,
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeInfo(info)),
        };
        assert!(
            parse_enroll_payload(&prost::Message::encode_to_vec(&envelope), "gpu-1").is_err()
        );
    }

    #[test]
    fn decide_enroll_accepts_fresh_node_without_token_when_auth_disabled() {
        let dir = temp_test_dir("decide-fresh");
        let enrollment = EnrollmentTokenStore::load(&dir);
        let node_store = NodeTokenStore::load(&dir);
        let decision = decide_enroll("gpu-1", None, false, Some(&enrollment), Some(&node_store));
        assert_eq!(
            decision,
            EnrollDecision::Accept {
                reuse: false,
                consume_enrollment_token: false,
            }
        );
    }

    #[test]
    fn decide_enroll_accepts_preissued_node_token_without_consuming() {
        // A node presenting its own long-lived node token (rather than a
        // one-time enrollment token) must be accepted without consuming
        // it. ADR-075 D7 removed the Gateway-side pre-issuing placeholder,
        // but the branch it exercises is still reachable (a re-enrolling
        // node that already holds its credential).
        let dir = temp_test_dir("decide-preissued");
        let enrollment = EnrollmentTokenStore::load(&dir);
        let mut node_store = NodeTokenStore::load(&dir);
        let preissued = node_store.upsert("local");

        let decision = decide_enroll(
            "local",
            Some(&preissued),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert_eq!(
            decision,
            EnrollDecision::Accept {
                reuse: true,
                consume_enrollment_token: false,
            }
        );
    }

    #[test]
    fn decide_enroll_requires_token_when_auth_enabled() {
        let dir = temp_test_dir("decide-token-required");
        let enrollment = EnrollmentTokenStore::load(&dir);
        let node_store = NodeTokenStore::load(&dir);
        let decision = decide_enroll("gpu-1", None, true, Some(&enrollment), Some(&node_store));
        assert!(matches!(
            decision,
            EnrollDecision::Reject { reason } if reason.contains("required")
        ));
    }

    #[test]
    fn decide_enroll_valid_unknown_consumed_expired_tokens() {
        let dir = temp_test_dir("decide-token-states");
        let mut enrollment = EnrollmentTokenStore::load(&dir);
        let node_store = NodeTokenStore::load(&dir);

        // Valid token → accept.
        let valid = enrollment.create_token(std::time::Duration::from_secs(3600), None);
        let decision = decide_enroll(
            "gpu-1",
            Some(&valid),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert_eq!(
            decision,
            EnrollDecision::Accept {
                reuse: false,
                consume_enrollment_token: true,
            }
        );

        // Unknown token → reject.
        let decision = decide_enroll(
            "gpu-1",
            Some("not-a-real-token"),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert!(matches!(
            decision,
            EnrollDecision::Reject { reason } if reason.contains("unknown")
        ));

        // Consumed token → reject.
        assert!(enrollment.consume_token(&valid, "gpu-1"));
        let decision = decide_enroll(
            "gpu-1",
            Some(&valid),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert!(matches!(
            decision,
            EnrollDecision::Reject { reason } if reason.contains("already used")
        ));

        // Expired token (ttl = 0 → expires_at == now) → reject.
        let expired = enrollment.create_token(std::time::Duration::ZERO, None);
        let decision = decide_enroll(
            "gpu-1",
            Some(&expired),
            true,
            Some(&enrollment),
            Some(&node_store),
        );
        assert!(matches!(
            decision,
            EnrollDecision::Reject { reason } if reason.contains("expired")
        ));
    }

    #[test]
    fn decide_enroll_reuses_token_for_re_enrolled_node() {
        // ADR-075 D7: no conflict detection — a node_id that already
        // has a record reuses its token (idempotent re-enrollment,
        // e.g. after a Gateway restart).
        let dir = temp_test_dir("decide-uid");
        let enrollment = EnrollmentTokenStore::load(&dir);
        let mut node_store = NodeTokenStore::load(&dir);
        node_store.upsert("gpu-1");

        // Re-enrollment → reuse (idempotent).
        let decision = decide_enroll("gpu-1", None, false, Some(&enrollment), Some(&node_store));
        assert_eq!(
            decision,
            EnrollDecision::Accept {
                reuse: true,
                consume_enrollment_token: false,
            }
        );
    }

    #[tokio::test]
    async fn enroll_handshake_end_to_end() {
        // Real broker + auth: enroll → node token minted → registry
        // record → enroll_result reply observed by the node client.
        let dir = temp_test_dir("e2e");
        let enrollment_store = crate::mqtt::new_shared_enrollment_store(&dir);
        let node_store = crate::mqtt::new_shared_node_token_store(&dir);
        let token = enrollment_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .create_token(std::time::Duration::from_secs(3600), None);

        let port = 18979; // distinct from the broker smoke-test ports (18976/18977 in mqtt/client.rs, 18978 in global_resources_publisher.rs)
        let host = "127.0.0.1";
        let auth = crate::mqtt::broker::BrokerAuth {
            auth_enabled: true,
            enrollment_tokens: enrollment_store.clone(),
            node_tokens: node_store.clone(),
            publisher_token: "publisher-tok".to_string(),
            http_token: None,
        };
        let _handle = crate::mqtt::start_broker_with_auth(host, port, Some(auth), None)
            .expect("broker should start");

        // Gateway-side client: subscribes the persistent topics and
        // routes messages through handle_plaintext_message. The client
        // is created AFTER the callback (same set-after-connect pattern
        // as gateway/mod.rs), so the callback borrows it via a slot.
        let node_reg = crate::mqtt::node_registry::new_shared_registry();
        let state = test_state();
        let reg_for_cb = node_reg.clone();
        let state_for_cb = state.clone();
        let enroll_store_for_cb = enrollment_store.clone();
        let node_store_for_cb = node_store.clone();
        let dispatch_slot: std::sync::Arc<
            tokio::sync::Mutex<Option<Arc<crate::mqtt::GatewayMqttClient>>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let slot_for_cb = dispatch_slot.clone();
        let callback: crate::mqtt::MqttMessageCallback = Arc::new(move |topic, payload| {
            let slot = slot_for_cb.clone();
            let reg = reg_for_cb.clone();
            let state = state_for_cb.clone();
            let es = enroll_store_for_cb.clone();
            let ns = node_store_for_cb.clone();
            tokio::spawn(async move {
                let client = slot.lock().await.clone();
                crate::mqtt::dispatch::handle_plaintext_message(
                    &topic,
                    &payload,
                    &DispatchContext {
                        runtime_http_registry: crate::http::proxy::new_shared_registry(),
                        agent_registry: crate::mqtt::agent_registry::new_shared_registry(),
                        node_registry: reg.clone(),
                        mqtt_client: client,
                        state,
                        enrollment_tokens: Some(es),
                        node_tokens: Some(ns),
                        auth_enabled: true,
                        ..Default::default()
                    },
                );
            });
        });
        let gw_client = crate::mqtt::GatewayMqttClient::new_publisher_with_callback_and_credentials(
            host,
            port,
            callback,
            "gateway:publisher",
            "publisher-tok",
        )
        .await
        .expect("gateway client should connect");
        *dispatch_slot.lock().await = Some(Arc::new(gw_client.clone()));
        let _gw_client = gw_client;
        // Give the ConnAck subscription round a moment to settle.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Node-side client: node:{id} username + enrollment token as
        // the CONNECT password; subscribes the result topic.
        use rumqttc::{AsyncClient, Incoming, MqttOptions, QoS};
        let mut mqttoptions = MqttOptions::new("node:gpu-1", host, port);
        mqttoptions.set_keep_alive(std::time::Duration::from_secs(5));
        mqttoptions.set_credentials("node:gpu-1".to_string(), token.clone());
        let (node_client, mut node_eventloop) = AsyncClient::new(mqttoptions, 10);
        let result_topic = acowork_core::node::node_enroll_result_topic("gpu-1");
        node_client
            .subscribe(&result_topic, QoS::AtLeastOnce)
            .await
            .expect("subscribe result topic");

        // Poll the node event loop in the background (this also flushes
        // the subscribe + pending publishes).
        let enroll_topic = acowork_core::node::node_enroll_topic("gpu-1");
        let enroll_msg = enroll_payload("gpu-1", Some(&token));
        let node_poll = tokio::spawn(async move {
            let mut saw_result = false;
            for _ in 0..100 {
                match node_eventloop.poll().await {
                    Ok(rumqttc::Event::Incoming(Incoming::Publish(p))) => {
                        if p.topic == result_topic {
                            saw_result = DataEnvelope::decode(p.payload.as_ref())
                                .ok()
                                .and_then(|e| e.payload)
                                .is_some_and(|p| {
                                    matches!(p, data_envelope::Payload::NodeEnrollResult(_))
                                });
                            if saw_result {
                                break;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            saw_result
        });
        // Wait until the node connection is established before publishing.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        node_client
            .publish(&enroll_topic, QoS::AtLeastOnce, false, enroll_msg.as_slice())
            .await
            .expect("publish enroll");

        let saw_result = tokio::time::timeout(std::time::Duration::from_secs(10), node_poll)
            .await
            .expect("enroll reply must arrive")
            .expect("node poll task must not fail");
        assert!(saw_result, "node client should observe the enroll_result reply");

        // Side effects: node token minted + persisted, registry record,
        // one-time enrollment token consumed.
        let node_token = node_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_token("gpu-1")
            .expect("node token minted")
            .to_string();
        assert_eq!(node_token.len(), 64);
        assert_eq!(
            node_reg
                .read()
                .await
                .get("gpu-1")
                .and_then(|n| n.node_token.clone())
                .as_deref(),
            Some(node_token.as_str())
        );
        assert_eq!(
            enrollment_store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .validate_token(&token),
            TokenValidation::Consumed
        );

        // Re-enrollment reuses the token and still completes
        // (idempotent), even without a fresh token. A fresh short-lived
        // client carries the re-enroll publish (the first node client's
        // eventloop task has already finished).
        let reused = enrollment_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .create_token(std::time::Duration::from_secs(3600), None);
        let mut re_mqttoptions = MqttOptions::new("node:gpu-1", host, port);
        re_mqttoptions.set_keep_alive(std::time::Duration::from_secs(5));
        // Reconnect with the minted node token — the realistic
        // re-enroll path after a Gateway restart.
        re_mqttoptions.set_credentials("node:gpu-1".to_string(), node_token.clone());
        let (re_client, mut re_eventloop) = AsyncClient::new(re_mqttoptions, 10);
        let second_payload = enroll_payload("gpu-1", Some(&reused));
        re_client
            .publish(&enroll_topic, QoS::AtLeastOnce, false, second_payload.as_slice())
            .await
            .expect("publish re-enroll");
        tokio::spawn(async move {
            for _ in 0..20 {
                if re_eventloop.poll().await.is_err() {
                    break;
                }
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            node_store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_token("gpu-1")
                .expect("token preserved on re-enroll"),
            node_token.as_str(),
            "re-enroll must reuse the node token"
        );
    }

    #[test]
    fn test_topic_matches_exact() {
        assert!(topic_matches("acowork/global/providers", "acowork/global/providers"));
        assert!(!topic_matches("acowork/global/providers", "acowork/global/mcps"));
    }

    #[test]
    fn test_topic_matches_single_wildcard() {
        assert!(topic_matches("acowork/agents/+/status", "acowork/agents/com.example/status"));
        assert!(topic_matches("acowork/agents/+/status", "acowork/agents/foo/status"));
        assert!(!topic_matches("acowork/agents/+/status", "acowork/agents/foo/meta"));
        assert!(!topic_matches("acowork/agents/+/status", "acowork/agents/foo/sessions/s1/config"));
    }

    #[test]
    fn test_topic_matches_multi_wildcard() {
        assert!(topic_matches("acowork/agents/+/sessions/+/messages/#", "acowork/agents/foo/sessions/s1/messages/chunk"));
        assert!(topic_matches("acowork/global/#", "acowork/global/providers"));
        assert!(topic_matches("acowork/global/#", "acowork/global/anything/deep"));
        assert!(!topic_matches("acowork/global/#", "acowork/agents/foo/status"));
    }

    #[test]
    fn test_topic_matches_edge_cases() {
        // # must be last
        assert!(topic_matches("#", "anything/at/all"));
        // + matches exactly one level
        assert!(!topic_matches("a/+", "a/b/c"));
        assert!(topic_matches("a/+/c", "a/b/c"));
    }

    #[test]
    fn extract_agent_id_from_status_topic_ok() {
        assert_eq!(
            extract_agent_id_from_status_topic("acowork/agents/com.example/status"),
            Some("com.example".to_string())
        );
    }

    #[test]
    fn extract_agent_id_from_status_topic_rejects_extra_segments() {
        // The permissive `extract_agent_id_from_topic` would accept
        // this; the status variant must NOT (re-publish logic relies
        // on the exact 4-segment shape).
        assert_eq!(
            extract_agent_id_from_status_topic(
                "acowork/agents/com.example/sessions/s-1/messages/chunk",
            ),
            None,
        );
    }

    #[test]
    fn extract_agent_id_from_status_topic_rejects_wrong_topic() {
        assert_eq!(extract_agent_id_from_status_topic("acowork/global/providers"), None);
        assert_eq!(extract_agent_id_from_status_topic("acowork/agents//status"), None);
    }

    // ── 2026-09-07 incident follow-up: post-wake reconcile invariants ─────

    /// `track_running_agent_for_status` is the symmetric counterpart of
    /// the `offline` branch. The pre-fix bug only removed the
    /// `running_agents` entry on `offline`, so an OS sleep/wake cycle
    /// (LWT `offline` → Runtime reconnects → publishes `online`) left
    /// the entry empty. The fast-path then 200'd but the UI still
    /// showed 休眠. Pin the fix at the helper level so a future
    /// refactor of the status handler cannot regress to the asymmetric
    /// shape.
    #[tokio::test]
    async fn track_running_agent_for_status_installs_entry_on_online() {
        let state = test_state();
        // Seed an installed agent (the helper derives the workspace
        // path from `installed_agents[agent_id].install_path`).
        {
            let mut gw = state.write().await;
            gw.installed_agents.insert(
                INSTANCE_ARCHITECT.to_string(),
                crate::gateway::state::AgentInfo {
                    instance_id: INSTANCE_ARCHITECT.to_string(),
                    agent_id: "com.acowork.architect".to_string(),
                    version: "1.0.0".to_string(),
                    name: "Architect".to_string(),
                    install_path: "/tmp/pkg/architect".to_string(),
                    manifest: acowork_core::AgentManifest {
                        config: Default::default(),
                        labels: Vec::new(),
                        mcp_servers: Vec::new(),
                        agent_id: "com.acowork.architect".to_string(),
                        version: "1.0.0".to_string(),
                        name: "Architect".to_string(),
                        display_name: None,
                        role: None,
                        avatar: None,
                        builtin_avatar: None,
                        description: "test".to_string(),
                        author: "test".to_string(),
                        runtime_version: "0.1.0".to_string(),
                        permissions: vec![],
                        triggers: vec![],
                        llm: Default::default(),
                        memory: Default::default(),
                        identity_deps: vec![],
                        tools: vec![],
                        capabilities: Default::default(),
                        resources: Default::default(),
                        sandbox: Default::default(),
                        dev: false,
                        skills: Default::default(),
                    },
                    node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                },
            );
        }

        // No entry pre-condition.
        assert!(
            !state.read().await.is_running(INSTANCE_ARCHITECT),
            "preflight: entry must NOT exist before the status transition"
        );

        // Dispatch the online-class status.
        track_running_agent_for_status(&state, INSTANCE_ARCHITECT).await;

        // Post-condition: entry installed, pid=0 (node-hosted).
        let entry = state
            .read()
            .await
            .running_agents
            .get(INSTANCE_ARCHITECT)
            .cloned()
            .expect("entry must be installed after status=online");
        assert_eq!(entry.pid, 0, "node-hosted Runtime is tracked with pid=0");
    }

    /// Minimal install record for `node_id` attribution tests.
    fn installed_agent_info(node_id: &str) -> crate::gateway::state::AgentInfo {
        crate::gateway::state::AgentInfo {
            instance_id: INSTANCE_ARCHITECT.to_string(),
            agent_id: "com.acowork.architect".to_string(),
            version: "1.0.0".to_string(),
            name: "Architect".to_string(),
            install_path: "/tmp/pkg/architect".to_string(),
            manifest: acowork_core::AgentManifest {
                config: Default::default(),
                labels: Vec::new(),
                mcp_servers: Vec::new(),
                agent_id: "com.acowork.architect".to_string(),
                version: "1.0.0".to_string(),
                name: "Architect".to_string(),
                display_name: None,
                role: None,
                avatar: None,
                builtin_avatar: None,
                description: "test".to_string(),
                author: "test".to_string(),
                runtime_version: "0.1.0".to_string(),
                permissions: vec![],
                triggers: vec![],
                llm: Default::default(),
                memory: Default::default(),
                identity_deps: vec![],
                tools: vec![],
                capabilities: Default::default(),
                resources: Default::default(),
                sandbox: Default::default(),
                dev: false,
                skills: Default::default(),
            },
            node_id: node_id.to_string(),
        }
    }

    /// ADR-075: the hosting node must be taken from the install record.
    /// The status topic (`acowork/agents/{id}/status`) carries no node
    /// identity, so hardcoding the `"local"` bookkeeping anchor made
    /// every re-track (Gateway restart / post-wake) mis-attribute a
    /// remote-node Runtime, which the Desktop's per-node grouping then
    /// rendered as a bogus `local` node. `"local"` is only correct when
    /// the record is genuinely absent.
    #[tokio::test]
    async fn track_running_agent_for_status_takes_node_id_from_install_record() {
        const REMOTE: &str = "9f1c4e2a-1111-4bbb-8ccc-0123456789ab";
        const UNKNOWN: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

        let state = test_state();
        {
            let mut gw = state.write().await;
            gw.installed_agents
                .insert(INSTANCE_ARCHITECT.to_string(), installed_agent_info(REMOTE));
        }

        track_running_agent_for_status(&state, INSTANCE_ARCHITECT).await;
        let entry = state
            .read()
            .await
            .running_agents
            .get(INSTANCE_ARCHITECT)
            .cloned()
            .expect("entry must be installed");
        assert_eq!(
            entry.node_id, REMOTE,
            "node_id must come from the install record, not the `local` anchor"
        );

        // No install record (inventory not aggregated yet) → the
        // documented `local` fallback is the only correct answer.
        track_running_agent_for_status(&state, UNKNOWN).await;
        let entry = state
            .read()
            .await
            .running_agents
            .get(UNKNOWN)
            .cloned()
            .expect("entry must be installed");
        assert_eq!(
            entry.node_id,
            acowork_core::node::LOCAL_NODE_ID,
            "missing record must still fall back to the `local` anchor"
        );
    }

    /// Repeated `online` signals on the same agent must be idempotent
    /// — the existing entry's rich fields (started_at, dev_mode, etc.)
    /// must be preserved across the second transition. Otherwise a
    /// re-connection storm would reset `started_at` and the Desktop's
    /// uptime displays would reset on every wake.
    #[tokio::test]
    async fn track_running_agent_for_status_is_idempotent_on_repeated_online() {
        let state = test_state();
        track_running_agent_for_status(&state, INSTANCE_ARCHITECT).await;
        let started_at_first = state
            .read()
            .await
            .running_agents
            .get(INSTANCE_ARCHITECT)
            .map(|e| e.started_at)
            .expect("first online installs entry");

        // Sleep so the timestamp would observably change if the helper
        // naively re-inserted the entry.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        track_running_agent_for_status(&state, INSTANCE_ARCHITECT).await;

        let started_at_second = state
            .read()
            .await
            .running_agents
            .get(INSTANCE_ARCHITECT)
            .map(|e| e.started_at)
            .expect("entry must still exist after repeated online signal");
        assert_eq!(
            started_at_first, started_at_second,
            "repeated online signals must NOT reset started_at"
        );
    }

    /// `reconcile_running_agents` is the safety-net backstop for the
    /// dispatch event handlers — it must re-install missing entries
    /// from the broker's authoritative online view even when the
    /// event-driven `track_running_agent_for_status` did not fire
    /// (e.g. Gateway restart after a wake, retained replay ordering
    /// race, etc.).
    #[tokio::test]
    async fn reconcile_running_agents_installs_missing_entry_from_broker_view() {
        let state = test_state();
        // Seed an installed agent.
        {
            let mut gw = state.write().await;
            gw.installed_agents.insert(
                INSTANCE_ARCHITECT.to_string(),
                crate::gateway::state::AgentInfo {
                    instance_id: INSTANCE_ARCHITECT.to_string(),
                    agent_id: "com.acowork.architect".to_string(),
                    version: "1.0.0".to_string(),
                    name: "Architect".to_string(),
                    install_path: "/tmp/pkg/architect".to_string(),
                    manifest: acowork_core::AgentManifest {
                        config: Default::default(),
                        labels: Vec::new(),
                        mcp_servers: Vec::new(),
                        agent_id: "com.acowork.architect".to_string(),
                        version: "1.0.0".to_string(),
                        name: "Architect".to_string(),
                        display_name: None,
                        role: None,
                        avatar: None,
                        builtin_avatar: None,
                        description: "test".to_string(),
                        author: "test".to_string(),
                        runtime_version: "0.1.0".to_string(),
                        permissions: vec![],
                        triggers: vec![],
                        llm: Default::default(),
                        memory: Default::default(),
                        identity_deps: vec![],
                        tools: vec![],
                        capabilities: Default::default(),
                        resources: Default::default(),
                        sandbox: Default::default(),
                        dev: false,
                        skills: Default::default(),
                    },
                    node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                },
            );
        }

        // Simulate: the broker says `online` (e.g. a retained snapshot
        // arrived while the Gateway's event handler was on a backoff),
        // but `running_agents` has no entry — this is the 2026-09-07
        // desync the user observed.
        let reg = crate::mqtt::agent_registry::new_shared_registry();
        let envelope = acowork_core::mqtt_proto::DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(
                    acowork_core::mqtt_proto::AgentStatus {
                        agent_id: "com.acowork.architect".to_string(),
                        online: true,
                        instance_id: INSTANCE_ARCHITECT.to_string(),
                        node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                        state: 4,
                        detail: String::new(),
                    },
                ),
            ),
        };
        let payload = prost::Message::encode_to_vec(&envelope);
        reg.write().await.update_from_mqtt(
            &format!("acowork/agents/{}/status", INSTANCE_ARCHITECT),
            &payload,
        );
        assert!(
            reg.read().await.is_online(INSTANCE_ARCHITECT),
            "preflight: broker says online"
        );
        assert!(
            !state.read().await.is_running(INSTANCE_ARCHITECT),
            "preflight: gateway says NOT running (DESYNC)"
        );

        reconcile_running_agents(&state, &reg).await;

        let entry = state
            .read()
            .await
            .running_agents
            .get(INSTANCE_ARCHITECT)
            .cloned()
            .expect("reconcile must install the missing entry from broker online view");
        assert_eq!(entry.pid, 0, "reconciled entry is node-hosted");
    }

    /// Stale `running_agents` entries (process view thinks X is
    /// running, but the broker reports X offline) must be dropped —
    /// otherwise the UI shows `running=true` for an agent whose MQTT
    /// session is gone, and `POST /start` would short-circuit on a
    /// stale entry.
    #[tokio::test]
    async fn reconcile_running_agents_drops_stale_entries_no_longer_in_broker_view() {
        let state = test_state();
        // Plant a stale entry: the agent is in `running_agents` but the
        // broker has never seen it (fresh registry), and the entry is
        // OLDER than the reconcile grace window — only then may
        // reconcile drop it.
        {
            let mut gw = state.write().await;
            gw.add_running(crate::gateway::state::RunningAgentInfo {
                instance_id: INSTANCE_STALE.to_string(),
                agent_id: "com.acowork.stale".to_string(),
                pid: 0,
                started_at: chrono::Utc::now() - chrono::Duration::seconds(60),
                workspace: String::new(),
                // ADR-075 D6: fallback anchor for the local node.
                node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                lifecycle: acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady,
                lifecycle_detail: String::new(),
                dev_mode: false,
                debug_state: crate::gateway::state::DebugState::Disabled,
                debug_port: None,
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
        }
        // Empty broker registry — no agent is reported online.
        let reg = crate::mqtt::agent_registry::new_shared_registry();

        reconcile_running_agents(&state, &reg).await;

        assert!(
            !state.read().await.is_running(INSTANCE_STALE),
            "stale entry must be dropped by reconcile (broker says offline)"
        );
    }

    /// An entry YOUNGER than the grace window must survive reconcile
    /// even when the broker has no `online` backing yet — this is the
    /// in-flight window between a node acknowledging `start` and the
    /// Runtime publishing its retained `online` (cold boot after an
    /// OS wake can take longer than one reconcile tick). Dropping it
    /// would silently undo a start that is still progressing.
    #[tokio::test]
    async fn reconcile_running_agents_preserves_fresh_in_flight_entries() {
        let state = test_state();
        // Plant a just-installed entry (started_at = now): the broker
        // has no record of this agent yet, but the entry is younger
        // than RECONCILE_STALE_GRACE_SECS.
        {
            let mut gw = state.write().await;
            gw.add_running(crate::gateway::state::RunningAgentInfo {
                instance_id: INSTANCE_BOOTING.to_string(),
                agent_id: "com.acowork.booting".to_string(),
                pid: 0,
                started_at: chrono::Utc::now(),
                workspace: String::new(),
                // ADR-075 D6: fallback anchor for the local node.
                node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                lifecycle: acowork_core::mqtt_proto::AgentLifecycleState::Offline,
                lifecycle_detail: String::new(),
                dev_mode: false,
                debug_state: crate::gateway::state::DebugState::Disabled,
                debug_port: None,
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
        }
        // Empty broker registry — no agent is reported online yet.
        let reg = crate::mqtt::agent_registry::new_shared_registry();

        reconcile_running_agents(&state, &reg).await;

        assert!(
            state.read().await.is_running(INSTANCE_BOOTING),
            "in-flight entry younger than RECONCILE_STALE_GRACE_SECS must survive reconcile"
        );
    }

    /// Reconciling against an empty broker view must NOT touch entries
    /// that ARE in the broker's online view — the helper is
    /// idempotent on the broker-authoritative set.
    #[tokio::test]
    async fn reconcile_running_agents_preserves_entries_consistent_with_broker_view() {
        let state = test_state();
        let reg = crate::mqtt::agent_registry::new_shared_registry();
        let envelope = acowork_core::mqtt_proto::DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(
                    acowork_core::mqtt_proto::AgentStatus {
                        agent_id: "com.acowork.architect".to_string(),
                        online: true,
                        instance_id: INSTANCE_ARCHITECT.to_string(),
                        node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                        state: 4,
                        detail: String::new(),
                    },
                ),
            ),
        };
        let payload = prost::Message::encode_to_vec(&envelope);
        reg.write().await.update_from_mqtt(
            &format!("acowork/agents/{}/status", INSTANCE_ARCHITECT),
            &payload,
        );

        track_running_agent_for_status(&state, INSTANCE_ARCHITECT).await;
        let started_at = state
            .read()
            .await
            .running_agents
            .get(INSTANCE_ARCHITECT)
            .map(|e| e.started_at)
            .expect("preflight: entry exists");

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        reconcile_running_agents(&state, &reg).await;

        let entry = state
            .read()
            .await
            .running_agents
            .get(INSTANCE_ARCHITECT)
            .cloned()
            .expect("entry must still exist after idempotent reconcile");
        assert_eq!(
            entry.started_at, started_at,
            "consistent entries must NOT be re-installed (started_at preserved)"
        );
    }

    /// Regression: a Node's `acowork/nodes/+/agents/+/installed`
    /// retained message must wake the inventory-change notifier so
    /// the Desktop sidebar refetches without a tab-switch remount
    /// (the bug that motivated this signal — see PR description).
    ///
    /// We exercise the dispatcher path by feeding a synthetic
    /// retained payload and then polling the trigger's Notify
    /// registry directly: every `Notify::notify_one()` increments an
    /// internal counter that the standard `Notify` does not expose,
    /// so we use a thin wrapper here. The trigger struct itself
    /// is an `Arc<Notify>`; `Notify::notified()` is what we drive.
    #[tokio::test]
    async fn installed_inventory_aggregation_wakes_inventory_trigger() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::sync::Notify;

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_cb = counter.clone();
        let inventory_trigger = crate::mqtt::InventoryNotifierTrigger::for_test(Notify::new());
        // Poll the test-side counter via a one-shot task: subscribe
        // via `notified()` and bump a sibling atomic on wake. This is
        // racey if the publish has already happened, so we register
        // the listener BEFORE feeding the dispatcher.
        let counter_for_task = counter_for_cb.clone();
        let inventory_trigger_for_task = inventory_trigger.clone();
        let listener = tokio::spawn(async move {
            // Wait in a loop so a single burst still registers.
            loop {
                inventory_trigger_for_task.notify.notified().await;
                counter_for_task.fetch_add(1, Ordering::SeqCst);
            }
        });

        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let mut ctx = ctx_with_registry(registry);
        // Inject the trigger AFTER ctx_with_registry (which sets `..Default::default()`,
        // so inventory_trigger is None).
        ctx.inventory_trigger = Some(inventory_trigger.clone());

        // Build a valid InstalledAgentInfo envelope.
        let manifest_toml = "agent_id = \"com.acowork.test\"\n\
            version = \"1.0.0\"\n\
            name = \"Test\"\n\
            description = \"\"\n\
            author = \"\"\n\
            runtime_version = \"0.1.0\"";
        let info = acowork_core::mqtt_proto::InstalledAgentInfo {
            agent_id: "com.acowork.test".to_string(),
            version: "1.0.0".to_string(),
            name: "Test".to_string(),
            install_path: "/tmp/pkg/abc".to_string(),
            manifest_toml: manifest_toml.to_string(),
            instance_id: "1a1a1a1a-0000-4000-8000-000000000099".to_string(),
            overrides_json: String::new(),
        };
        let envelope = acowork_core::mqtt_proto::DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::InstalledAgentInfo(info),
            ),
        };
        let payload = prost::Message::encode_to_vec(&envelope);

        // Feed the dispatcher. The installed-topic matcher spawns an
        // async task; wait for the installed_agents table to populate
        // (then the notify() has already fired).
        let topic = acowork_core::node::node_agent_installed_topic(
            "node-test",
            "1a1a1a1a-0000-4000-8000-000000000099",
        );
        handle_message(&topic, &payload, &ctx);

        // Poll for either: (a) the trigger woke (counter > 0) or
        // (b) the installed_agents table populated. Either proves
        // the dispatcher path fired.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if counter_for_cb.load(Ordering::SeqCst) >= 1 {
                break;
            }
            let gw = ctx.state.read().await;
            if gw.installed_agents.contains_key("1a1a1a1a-0000-4000-8000-000000000099") {
                // Aggregation completed; the notify() that runs AFTER
                // upsert_installed_from_node must have been called.
                // Give the listener a brief moment to wake.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if counter_for_cb.load(Ordering::SeqCst) >= 1 {
                    break;
                }
                panic!(
                    "installed_agents was populated but inventory_trigger.notify() \
                     was never called — the dispatcher forgot to wake the notifier"
                );
            }
            drop(gw);
            if std::time::Instant::now() >= deadline {
                panic!(
                    "neither the inventory trigger nor the installed_agents table \
                     updated within 2s — dispatcher did not process the message"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        listener.abort();
    }

    /// Regression: empty-payload `installed` retained message (the
    /// node clearing its entry on uninstall) must also wake the
    /// trigger — covers the "uninstall from remote node" case where
    /// the Desktop sidebar must drop the entry in real time.
    #[tokio::test]
    async fn installed_inventory_empty_payload_wakes_inventory_trigger() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_cb = counter.clone();
        let inventory_trigger = crate::mqtt::InventoryNotifierTrigger::for_test(
            tokio::sync::Notify::new(),
        );
        let inventory_trigger_for_task = inventory_trigger.clone();
        let counter_for_task = counter.clone();
        let listener = tokio::spawn(async move {
            loop {
                inventory_trigger_for_task.notify.notified().await;
                counter_for_task.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Seed an installed entry so `remove_installed` has something to drop.
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let mut ctx = ctx_with_registry(registry);
        ctx.inventory_trigger = Some(inventory_trigger.clone());
        {
            let mut gw = ctx.state.write().await;
            gw.add_installed(crate::gateway::state::AgentInfo {
                instance_id: "1a1a1a1a-0000-4000-8000-000000000077".to_string(),
                agent_id: "com.acowork.to-remove".to_string(),
                version: "1.0.0".to_string(),
                name: "To Remove".to_string(),
                install_path: "/tmp/pkg/rm".to_string(),
                manifest: acowork_core::AgentManifest {
                    config: Default::default(),
                    labels: Vec::new(),
                    mcp_servers: Vec::new(),
                    agent_id: "com.acowork.to-remove".to_string(),
                    version: "1.0.0".to_string(),
                    name: "To Remove".to_string(),
                    display_name: None,
                    role: None,
                    avatar: None,
                    builtin_avatar: None,
                    description: String::new(),
                    author: String::new(),
                    runtime_version: "0.1.0".to_string(),
                    permissions: vec![],
                    triggers: vec![],
                    llm: Default::default(),
                    memory: Default::default(),
                    identity_deps: vec![],
                    tools: vec![],
                    capabilities: Default::default(),
                    resources: Default::default(),
                    sandbox: Default::default(),
                    dev: false,
                    skills: Default::default(),
                },
                node_id: "node-test".to_string(),
            });
        }

        // Feed an empty retained payload (the node confirming uninstall).
        let topic = acowork_core::node::node_agent_installed_topic(
            "node-test",
            "1a1a1a1a-0000-4000-8000-000000000077",
        );
        handle_message(&topic, &[], &ctx);

        // Wait until either the table loses the entry or the trigger fires.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if counter_for_cb.load(Ordering::SeqCst) >= 1 {
                break;
            }
            let gw = ctx.state.read().await;
            if !gw.installed_agents.contains_key("1a1a1a1a-0000-4000-8000-000000000077") {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if counter_for_cb.load(Ordering::SeqCst) >= 1 {
                    break;
                }
                panic!(
                    "installed_agents lost the entry but inventory_trigger.notify() \
                     was never called — dispatcher forgot to wake the notifier"
                );
            }
            drop(gw);
            if std::time::Instant::now() >= deadline {
                panic!("dispatcher did not process the empty-payload clear within 2s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        listener.abort();
    }

    // ── ADR-087 (revised): default-owner convergence points ───────────

    #[tokio::test]
    async fn enroll_with_ownerless_token_falls_back_to_default_owner() {
        // A CLI-issued (or pre-admin) enrollment token carries no owner.
        // The enroll accept path must attribute the node to the default
        // owner instead of leaving it ownerless.
        let dir = temp_test_dir("enroll-default-owner");
        let tokens = crate::mqtt::new_shared_enrollment_store(&dir);
        let node_tokens = crate::mqtt::new_shared_node_token_store(&dir);
        let owners = crate::gateway::ownership::new_shared_node_owners(&dir);
        let token = tokens
            .lock()
            .unwrap()
            .create_token(std::time::Duration::from_secs(60), None);
        process_enroll_message(
            enroll_payload("gpu-9", Some(&token)),
            "gpu-9".to_string(),
            crate::mqtt::node_registry::new_shared_registry(),
            None,
            Some(tokens),
            Some(node_tokens),
            Some(owners.clone()),
            true,
            Some("admin-1".to_string()),
        )
        .await;
        let rec = owners.lock().unwrap().get("gpu-9").cloned();
        assert_eq!(
            rec.and_then(|r| r.owner_user_id).as_deref(),
            Some("admin-1"),
            "ownerless token + default owner ⇒ bound node row"
        );
    }

    #[tokio::test]
    async fn installed_landing_without_staged_row_gets_default_owner() {
        // A CLI install bypasses the HTTP staging (no pending row). The
        // first inventory sighting must still mint an owner row for the
        // default owner, with the hosting node denormalized.
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let mut ctx = ctx_with_registry(registry);
        let dir = temp_test_dir("landing-default-owner");
        ctx.agent_owners = Some(crate::gateway::ownership::new_shared_agent_owners(&dir));
        ctx.default_owner = Some("admin-1".to_string());
        let info = acowork_core::mqtt_proto::InstalledAgentInfo {
            agent_id: "com.acowork.test".to_string(),
            version: "1.0.0".to_string(),
            name: "Test".to_string(),
            install_path: "/tmp/pkg/abc".to_string(),
            manifest_toml: "agent_id = \"com.acowork.test\"\nversion = \"1.0.0\"\n\
                name = \"Test\"\ndescription = \"\"\nauthor = \"\"\nruntime_version = \"0.1.0\""
                .to_string(),
            instance_id: "1a1a1a1a-0000-4000-8000-0000000000ab".to_string(),
            overrides_json: String::new(),
        };
        let envelope = acowork_core::mqtt_proto::DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::InstalledAgentInfo(info),
            ),
        };
        let payload = prost::Message::encode_to_vec(&envelope);
        let topic = acowork_core::node::node_agent_installed_topic(
            "node-landing",
            "1a1a1a1a-0000-4000-8000-0000000000ab",
        );
        handle_message(&topic, &payload, &ctx);
        let store = ctx.agent_owners.clone().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            {
                let guard = store.lock().unwrap();
                if let Some(rec) = guard.get("1a1a1a1a-0000-4000-8000-0000000000ab") {
                    assert_eq!(rec.owner_user_id.as_deref(), Some("admin-1"));
                    assert_eq!(rec.node_id.as_deref(), Some("node-landing"));
                    return;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("landing backstop did not mint an owner row within 2s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn installed_landing_without_default_owner_stays_rowless() {
        // Local mode (default_owner None): no row is minted — ownerless
        // remains the correct "machine is the owner" semantics.
        let registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        let ctx = ctx_with_registry(registry);
        let dir = temp_test_dir("landing-no-default-owner");
        let owners = crate::gateway::ownership::new_shared_agent_owners(&dir);
        // ctx is not mutated after construction in this harness; wire
        // the store by rebuilding the context fields directly.
        let mut ctx = ctx;
        ctx.agent_owners = Some(owners.clone());
        let info = acowork_core::mqtt_proto::InstalledAgentInfo {
            agent_id: "com.acowork.test".to_string(),
            version: "1.0.0".to_string(),
            name: "Test".to_string(),
            install_path: "/tmp/pkg/abc".to_string(),
            manifest_toml: "agent_id = \"com.acowork.test\"\nversion = \"1.0.0\"\n\
                name = \"Test\"\ndescription = \"\"\nauthor = \"\"\nruntime_version = \"0.1.0\""
                .to_string(),
            instance_id: "1a1a1a1a-0000-4000-8000-0000000000cd".to_string(),
            overrides_json: String::new(),
        };
        let envelope = acowork_core::mqtt_proto::DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::InstalledAgentInfo(info),
            ),
        };
        let payload = prost::Message::encode_to_vec(&envelope);
        let topic = acowork_core::node::node_agent_installed_topic(
            "node-rowless",
            "1a1a1a1a-0000-4000-8000-0000000000cd",
        );
        handle_message(&topic, &payload, &ctx);
        // Give the spawned task time to run, then assert no row appeared.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            owners.lock().unwrap().get("1a1a1a1a-0000-4000-8000-0000000000cd").is_none(),
            "no default owner ⇒ no minted row"
        );
    }
}
