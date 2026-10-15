//! Intent delivery — the Gateway-side wake-then-deliver entry point.
//!
//! Tech-debt plan D1/D2 (`docs/plan/zh/intent-delivery-tech-debt-plan.md`):
//! the Gateway is the **only** Intent delivery entry. A raw MQTT publish
//! from a sender Runtime is silent-loss-prone (`clean_session=true` +
//! non-retained = dropped when nobody subscribes), so delivery is
//! centralised here:
//!
//! 1. resolve the target INSTANCE (ADR-073) against the install table;
//! 2. liveness = retained `AgentStatus` (`AgentRegistry::is_online`,
//!    ADR-085) — NOT the Node process table and NOT the spawn reply,
//!    because a spawn reply only proves the process exists, not that the
//!    Runtime has CONNECTed and subscribed the control filter (D2);
//! 3. offline → ask the hosting Node to start it, then wait (bounded by
//!    a deadline) for the status flip;
//! 4. publish the `Intent` ControlCommand to the target's control topic.
//!
//! Cron reuses the exact same function (D2: one cold-start path, one
//! test surface), so the pre-existing "spawn OK but not yet subscribed"
//! race is closed for scheduled triggers too.

use std::time::{Duration, Instant};

use acowork_core::mqtt_proto::{self, control_command::Command};

use crate::gateway::state::GatewayState;
use crate::handlers::server::SharedState;
use crate::intent::mailbox::{IntentMailbox, MailboxEntry};
use crate::mqtt::agent_registry::SharedAgentRegistry;
use crate::mqtt::GatewayMqttClient;
use crate::mqtt::node_control::NodeControlClient;

/// Poll interval while waiting for the target's retained `AgentStatus`
/// to flip online after a spawn. 200 ms is well below the human-
/// perceptible cold-start window and cheap against a broker round-trip.
const ONLINE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Delivery failures with the error-code vocabulary the sender sees
/// (design doc 27 §10; formal codes are frozen at the next protocol
/// revision).
#[derive(Debug, Clone)]
pub enum DeliveryError {
    /// The target instance id is not in the install table at all.
    TargetNotInstalled(String),
    /// The install record exists but its host node is unknown
    /// (`node_id == "local"` sentinel, ADR-077 §3.4) and the instance
    /// is offline — nothing to wake.
    NoHostingNode(String),
    /// A wake was required but the node control plane is unavailable.
    NoControlPlane,
    /// The hosting Node rejected the start command.
    SpawnFailed(String),
    /// The Runtime process was started but its retained AgentStatus did
    /// not flip online within the deadline — delivering now would be a
    /// silent drop (D2).
    WakeTimeout { instance_id: String, timeout: Duration },
    /// The MQTT publish itself failed.
    PublishFailed(String),
    /// The Gateway MQTT publisher is not connected — nothing to
    /// publish through (same shape as cron's pre-existing guard).
    NoPublisher,
    /// The mailbox write failed — with persistence configured this means
    /// the delivery fact could not be recorded, so the send is refused
    /// rather than silently unrecorded (D3).
    MailboxFailed(String),
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeliveryError::TargetNotInstalled(t) => write!(f, "target_not_found: no install record for instance '{t}'"),
            DeliveryError::NoHostingNode(t) => write!(f, "delivery_unavailable: instance '{t}' is offline and its hosting node is unknown"),
            DeliveryError::NoControlPlane => write!(f, "delivery_unavailable: node control plane not connected"),
            DeliveryError::SpawnFailed(e) => write!(f, "delivery_unavailable: target start failed: {e}"),
            DeliveryError::WakeTimeout { instance_id, timeout } => write!(f, "delivery_unavailable: instance '{instance_id}' did not come online within {}s", timeout.as_secs()),
            DeliveryError::PublishFailed(e) => write!(f, "delivery_unavailable: MQTT publish failed: {e}"),
            DeliveryError::NoPublisher => write!(f, "delivery_unavailable: Gateway MQTT publisher not connected"),
            DeliveryError::MailboxFailed(e) => write!(f, "delivery_unavailable: mailbox write failed: {e}"),
        }
    }
}

impl DeliveryError {
    /// Stable machine-readable code (design doc 27 §10 vocabulary).
    pub fn code(&self) -> &'static str {
        match self {
            DeliveryError::TargetNotInstalled(_) => "target_not_found",
            DeliveryError::NoHostingNode(_)
            | DeliveryError::NoControlPlane
            | DeliveryError::SpawnFailed(_)
            | DeliveryError::WakeTimeout { .. }
            | DeliveryError::PublishFailed(_)
            | DeliveryError::NoPublisher
            | DeliveryError::MailboxFailed(_) => "delivery_unavailable",
        }
    }
}

/// Dependencies of one delivery attempt, borrowed from whichever caller
/// owns them (cron loop, HTTP handler).
pub struct DeliveryContext<'a> {
    pub state: &'a SharedState,
    pub registry: &'a SharedAgentRegistry,
    pub node_control: Option<&'a NodeControlClient>,
    /// `None` in tests that only exercise resolution / wake paths —
    /// publish then returns `NoPublisher`.
    pub mqtt: Option<&'a GatewayMqttClient>,
    /// Delivery buffer (D3). `None` skips mailbox bookkeeping — only
    /// tests use that; every production path passes the Gateway store.
    pub mailbox: Option<&'a IntentMailbox>,
}

/// One Intent to deliver. `from_instance` must already be the trusted
/// sender identity (the Gateway stamps it — D4); client-claimed `from`
/// values are never copied here.
#[derive(Debug, Clone)]
pub struct DeliveryRequest {
    pub message_id: String,
    pub conversation_id: String,
    pub from_instance: String,
    pub from_session_id: String,
    pub target_instance: String,
    pub action: String,
    /// JSON-serialised params (wire `Intent.params_json`).
    pub params_json: String,
    /// Source chat session id (design 27 §3.2; empty until Phase 2).
    pub source_chat_session_id: String,
    /// Source-side authorized repo (design 27 §3.2; validated at the
    /// HTTP entry before this request is built — PR-D).
    pub repo: String,
    /// "request" (default) | "response" (design 27 §6.5 R2). A response
    /// is system-generated: the Gateway builds it at the responder's
    /// turn end and routes it to the initiator — `intent_send` never
    /// produces one.
    pub kind: String,
    /// For a response: the request `message_id` this answers (§4).
    pub in_reply_to: String,
    /// For a response: §7.1 outcome. Empty = completed.
    pub status: String,
}

/// Wake-then-deliver one Intent, recording the delivery fact in the
/// mailbox (D1 + D2 + D3).
///
/// Ok(()) means the publish succeeded while the registry reported the
/// target online. The mailbox entry transitions
/// recorded → delivered; on failure the entry stays recorded-undelivered
/// so `GET /api/intents/{message_id}` can report the truth and the
/// caller can map the error to a delivery state.
pub async fn wake_and_deliver(
    ctx: &DeliveryContext<'_>,
    req: &DeliveryRequest,
    wake_deadline: Duration,
) -> Result<(), DeliveryError> {
    // 0. Record the intent before touching the network: a delivery that
    //    fails must still be observable (D3).
    if let Some(mb) = ctx.mailbox {
        mb.record(MailboxEntry {
            message_id: req.message_id.clone(),
            conversation_id: req.conversation_id.clone(),
            from_instance: req.from_instance.clone(),
            from_session_id: req.from_session_id.clone(),
            to_instance: req.target_instance.clone(),
            body: req.params_json.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            delivered_at: String::new(),
            result_ref: req.in_reply_to.clone(),
            kind: req.kind.clone(),
            status: req.status.clone(),
            source_chat_session_id: req.source_chat_session_id.clone(),
            answered_at: String::new(),
        })
        .map_err(|e| DeliveryError::MailboxFailed(e.to_string()))?;
    }

    let result = deliver_inner(ctx, req, wake_deadline).await;
    // A mailbox write failure after a successful publish must not
    // flip the delivery verdict — the message IS delivered.
    if result.is_ok()
        && let Some(mb) = ctx.mailbox
        && let Err(e) = mb.mark_delivered(&req.message_id)
    {
        tracing::warn!(error = %e, message_id = %req.message_id, "mailbox mark_delivered failed");
    }
    result
}

async fn deliver_inner(
    ctx: &DeliveryContext<'_>,
    req: &DeliveryRequest,
    wake_deadline: Duration,
) -> Result<(), DeliveryError> {
    // 1. Resolve the instance against the install table (ADR-073: the
    //    package agent_id never addresses a runtime on the wire).
    let (instance_id, package_id, node_id) = {
        let gw: tokio::sync::RwLockReadGuard<'_, GatewayState> = ctx.state.read().await;
        let inst = gw
            .resolve_installed_key(&req.target_instance)
            .ok_or_else(|| DeliveryError::TargetNotInstalled(req.target_instance.clone()))?;
        let info = gw
            .installed(&inst)
            .ok_or_else(|| DeliveryError::TargetNotInstalled(inst.clone()))?;
        (inst, info.agent_id.clone(), info.node_id.clone())
    };

    // 2. Liveness = retained AgentStatus (ADR-085). The registry key is
    //    the canonical instance id; `resolve_installed_key` may map an
    //    alias, so re-check online under the canonical id.
    let online = {
        let reg = ctx.registry.read().await;
        reg.is_online(&instance_id)
    };

    // 3. Offline → start on the hosting node, then wait for the flip.
    if !online {
        if node_id == "local" {
            // ADR-075 D6 / ADR-077 §3.4: "local" is the bookkeeping
            // sentinel for "host node not yet known" — there is no node
            // to ask for a start.
            return Err(DeliveryError::NoHostingNode(instance_id));
        }
        let node_control = ctx.node_control.ok_or(DeliveryError::NoControlPlane)?;
        let event = node_control
            .start_agent(&node_id, &instance_id, &package_id, false)
            .await
            .map_err(|e| DeliveryError::SpawnFailed(e.to_string()))?;
        NodeControlClient::check_reply(&instance_id, &event)
            .map_err(|e| DeliveryError::SpawnFailed(e.to_string()))?;

        wait_online(ctx.registry, &instance_id, wake_deadline).await?;
    }

    // 4. Publish. The control topic and the ControlCommand both carry
    //    the INSTANCE identity.
    let intent = mqtt_proto::Intent {
        from: req.from_instance.clone(),
        action: req.action.clone(),
        params_json: req.params_json.clone(),
        message_id: req.message_id.clone(),
        conversation_id: req.conversation_id.clone(),
        source_agent_session_id: req.from_session_id.clone(),
        source_chat_session_id: req.source_chat_session_id.clone(),
        repo: req.repo.clone(),
        kind: req.kind.clone(),
        in_reply_to: req.in_reply_to.clone(),
        status: req.status.clone(),
    };
    let command = mqtt_proto::ControlCommand {
        instance_id: instance_id.clone(),
        command: Some(Command::Intent(intent)),
    };
    let mqtt = ctx.mqtt.ok_or(DeliveryError::NoPublisher)?;
    mqtt.publish_control_command(&instance_id, command)
        .await
        .map_err(|e| DeliveryError::PublishFailed(e.to_string()))?;

    tracing::info!(
        instance = %instance_id,
        action = %req.action,
        message_id = %req.message_id,
        "Intent delivered via wake-then-deliver"
    );
    Ok(())
}

/// Wait until the retained `AgentStatus` for `instance_id` flips online,
/// up to `deadline`. Exposed for tests: this is the D2 race window.
async fn wait_online(
    registry: &SharedAgentRegistry,
    instance_id: &str,
    deadline: Duration,
) -> Result<(), DeliveryError> {
    let start = Instant::now();
    loop {
        {
            let reg = registry.read().await;
            if reg.is_online(instance_id) {
                return Ok(());
            }
        }
        if start.elapsed() >= deadline {
            return Err(DeliveryError::WakeTimeout {
                instance_id: instance_id.to_string(),
                timeout: deadline,
            });
        }
        tokio::time::sleep(ONLINE_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state::{AgentInfo, GatewayState};
    use std::sync::Arc;
    use crate::mqtt::agent_registry::AgentRegistry;
    use acowork_core::mqtt_proto::{data_envelope, AgentStatus, DataEnvelope};
    use prost::Message as _;

    const INST: &str = "22222222-2222-4222-8222-222222222222";

    fn agent_info(instance: &str, node: &str) -> AgentInfo {
        let manifest = acowork_core::AgentManifest::from_toml(
            "agent_id = \"com.test.target\"\nversion = \"1\"\nname = \"t\"\ndescription = \"t\"\nauthor = \"t\"\nruntime_version = \"0.1.0\"",
        )
        .expect("minimal test manifest");
        AgentInfo {
            instance_id: instance.into(),
            agent_id: "com.test.target".into(),
            version: "1".into(),
            name: "t".into(),
            install_path: String::new(),
            manifest,
            node_id: node.into(),
        }
    }

    fn status_envelope(instance: &str, online: bool) -> Vec<u8> {
        let env = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentStatus(AgentStatus {
                instance_id: instance.into(),
                agent_id: "com.test.target".into(),
                node_id: "node-1".into(),
                online,
                ..Default::default()
            })),
        };
        env.encode_to_vec()
    }

    fn req(target: &str) -> DeliveryRequest {
        DeliveryRequest {
            message_id: "m-1".into(),
            conversation_id: String::new(),
            from_instance: "cron:test".into(),
            from_session_id: String::new(),
            target_instance: target.into(),
            action: "a".into(),
            params_json: "{}".into(),
            repo: String::new(),
            source_chat_session_id: String::new(),
            kind: "request".into(),
            in_reply_to: String::new(),
            status: String::new(),
        }
    }

    #[tokio::test]
    async fn unknown_target_is_rejected_not_dropped() {
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(GatewayState::new("/tmp")));
        let registry = crate::mqtt::agent_registry::new_shared_registry();
        // Resolution fails before the publisher is touched, so `mqtt:
        // None` is enough to exercise the error path.
        let mb = IntentMailbox::in_memory();
        let ctx = DeliveryContext {
            state: &state,
            registry: &registry,
            node_control: None,
            mqtt: None,
            mailbox: Some(&mb),
        };
        let err = wake_and_deliver(&ctx, &req("no-such-instance"), Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "target_not_found");
        // D3: the failed attempt is still an observable fact.
        assert!(mb.get("m-1").is_some());
        assert!(!mb.get("m-1").unwrap().delivered());
    }

    #[tokio::test]
    async fn offline_local_sentinel_target_reports_no_hosting_node() {
        let mut gw = GatewayState::new("/tmp");
        gw.add_installed(agent_info(INST, "local"));
        let state: SharedState = Arc::new(tokio::sync::RwLock::new(gw));
        let registry = crate::mqtt::agent_registry::new_shared_registry();
        let ctx = DeliveryContext {
            state: &state,
            registry: &registry,
            node_control: None,
            mqtt: None,
            mailbox: None,
        };
        let err = wake_and_deliver(&ctx, &req(INST), Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "delivery_unavailable");
        assert!(matches!(err, DeliveryError::NoHostingNode(_)));
    }

    #[tokio::test]
    async fn wait_online_flips_when_status_arrives() {
        // D2 core: spawn reply alone is NOT the online fact — the wait
        // must key on the retained AgentStatus flip.
        let registry = crate::mqtt::agent_registry::new_shared_registry();
        let reg2 = registry.clone();
        let topic = format!("acowork/agents/{INST}/status");
        let online = status_envelope(INST, true);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            reg2.write().await.update_from_mqtt(&topic, &online);
        });
        wait_online(&registry, INST, Duration::from_secs(2))
            .await
            .expect("status flip must satisfy the wait");
    }

    #[tokio::test]
    async fn wait_online_times_out_without_flip() {
        let registry: SharedAgentRegistry =
            Arc::new(tokio::sync::RwLock::new(AgentRegistry::new()));
        let err = wait_online(&registry, INST, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(err, DeliveryError::WakeTimeout { .. }));
    }
}


