//! Agent Registry (ADR-033 Phase 1 scaffolding).
//!
//! Tracks agent-instance online status based on MQTT
//! `acowork/agents/{instance_id}/status` Retained messages. ADR-073: the
//! topic variable is the INSTANCE identity (UUID), so the registry key
//! is an instance id — two instances of the same package never collide.
//! In Phase 2+, it replaces the gRPC `()` as the source of truth for
//! which agents are online.
//!
//! The status payload is a protobuf `DataEnvelope<AgentStatus>` published
//! by the Runtime (see `docs/zh/protocols/mqtt.md` §8.1) — the Runtime
//! owns the wire format end-to-end, the Gateway is a pure consumer.
//!
//! See `docs/zh/protocols/mqtt.md` §3.2 and §8.1 (Will Message).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use acowork_core::mqtt_proto::{data_envelope, AgentStatus, DataEnvelope};
use prost::Message as _;
use tokio::sync::RwLock;

/// Lifecycle state of an agent, derived from MQTT retained status messages.
///
/// `online=true` means the Runtime's MQTT session is alive (the process
/// is connected to the broker), which is what the reconcile loop /
/// Desktop `running` gate care about.
///
/// A manual stop or a crash leaves `online=false` (the broker fired
/// the LWT `AgentStatus{online=false}` envelope).
#[derive(Debug, Clone)]
pub struct AgentOnlineState {
    /// Whether the agent is currently reachable.
    pub online: bool,
    /// Wall-clock instant when the registry last observed a status update.
    pub last_updated: Instant,
    /// ADR-073: the INSTANCE identity this entry tracks — the registry
    /// key, taken from the envelope `instance_id` field. Never the
    /// package id.
    pub instance_id: String,
    /// Package identity (`AgentStatus.agent_id`), present only when the
    /// status arrived as a DataEnvelope carrying it — display &
    /// diagnostics only, NEVER a lookup key (ADR-073).
    pub agent_id: String,
    /// ADR-073: current location (`AgentStatus.node_id`) from the
    /// envelope. Positional metadata only.
    pub node_id: String,
}

/// In-memory registry of agent online status.
///
/// Updated by subscribing to `acowork/agents/+/status` and parsing
/// the payload ("online" / "offline"). The Gateway uses this to
/// answer `GET /api/agents?status=active` without polling each Runtime.
#[derive(Debug, Default)]
pub struct AgentRegistry {
    agents: HashMap<String, AgentOnlineState>,
}

impl AgentRegistry {
    /// Snapshot of every entry the registry currently holds
    /// (online or offline — anything in the map). Used by the
    /// reconciliation loop in `mqtt::dispatch::reconcile_running_agents`
    /// to walk the authoritative broker view without locking the
    /// registry for the entire iteration. The returned vector is a
    /// pure copy — safe to iterate without holding the read lock.
    pub fn snapshot(&self) -> Vec<(String, AgentOnlineState)> {
        self.agents
            .iter()
            .map(|(id, s)| (id.clone(), s.clone()))
            .collect()
    }

    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update an agent's status from an MQTT message.
    ///
    /// `topic` should match `acowork/agents/{instance_id}/status`
    /// (ADR-073: the path variable is the INSTANCE identity).
    /// `payload` is a protobuf `DataEnvelope<AgentStatus>` published by the
    /// Runtime (Sept 2026 — the plain-text contract was retired when the
    /// auto-sleep subsystem was removed; see `mqtt_payload.proto`).
    ///
    /// The previous vocabulary-discrimination logic ("is it 'online'/
    /// 'sleeping'/'offline'/'degraded' or is it a protobuf envelope?")
    /// was the source of the 2026-09-25 WARN-spam incident: the
    /// ADR-076 field-number compaction made `DataEnvelope<AgentStatus>`
    /// bytes parse as valid UTF-8, so the plaintext discriminator
    /// misread envelopes and flipped freshly-online agents to offline.
    /// With the wire now uniformly protobuf, the discriminator is gone.
    pub fn update_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        // Parse the instance id from the topic: acowork/agents/{instance_id}/status
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() != 4 || parts[0] != "acowork" || parts[1] != "agents" || parts[3] != "status" {
            tracing::warn!(topic, "Invalid agent status topic format");
            return;
        }

        let envelope = match DataEnvelope::decode(payload) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    topic,
                    instance_id = %parts[2],
                    error = %e,
                    "agent status payload is not a DataEnvelope — ignoring"
                );
                return;
            }
        };
        let status: AgentStatus = match envelope.payload {
            Some(data_envelope::Payload::AgentStatus(s)) => s,
            _ => {
                tracing::warn!(
                    topic,
                    instance_id = %parts[2],
                    "agent status envelope carries no AgentStatus payload"
                );
                return;
            }
        };
        // ADR-073: the envelope carries the instance identity in
        // `instance_id` (UUIDv4). Use the envelope field exclusively;
        // the legacy plaintext-empty-fallback has been removed.
        let instance_id = status.instance_id.clone();
        if acowork_core::AgentInstanceId::from_string(instance_id.clone()).is_err() {
            tracing::warn!(
                topic,
                instance_id = %status.instance_id,
                "agent status envelope carries non-UUID instance_id — ignoring"
            );
            return;
        }
        self.agents.insert(
            instance_id.clone(),
            AgentOnlineState {
                online: status.online,
                last_updated: Instant::now(),
                instance_id,
                agent_id: status.agent_id,
                node_id: status.node_id,
            },
        );

        tracing::debug!(
            instance_id = %parts[2],
            online = status.online,
            "Agent registry updated from MQTT"
        );
    }

    /// Check if an agent instance is online (keyed by instance identity).
    ///
    /// This is the authoritative distributed liveness signal: the
    /// Runtime's MQTT session is reachable at the broker level. It is
    /// topology-independent — the same answer for local, remote and
    /// node-hosted Runtimes — and must be preferred over any
    /// process-level probe.
    ///
    /// Auto-sleep was retired in Sept 2026; the only on/off transition
    /// is now stop / start.
    pub fn is_online(&self, instance_id: &str) -> bool {
        self.agents
            .get(instance_id)
            .map(|s| s.online)
            .unwrap_or(false)
    }

    /// Full online-state entry for one instance (D4 sender attribution:
    /// the retained AgentStatus carries the host node the Runtime itself
    /// asserted). `None` = never seen on the status topic.
    pub fn instance(&self, instance_id: &str) -> Option<&AgentOnlineState> {
        self.agents.get(instance_id)
    }

    /// Get all online agent IDs.
    pub fn online_agents(&self) -> Vec<String> {
        self.agents
            .iter()
            .filter(|(_, s)| s.online)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Get the total number of tracked agents (online + offline).
    #[allow(dead_code)]
    pub fn total_tracked(&self) -> usize {
        self.agents.len()
    }

    /// Get the number of online agents.
    pub fn online_count(&self) -> usize {
        self.agents.values().filter(|s| s.online).count()
    }

    /// Remove an agent instance from the registry (e.g. on uninstall,
    /// or when its hosting node is removed from the fleet).
    pub fn remove(&mut self, instance_id: &str) {
        self.agents.remove(instance_id);
    }
}

/// Thread-safe shared AgentRegistry.
pub type SharedAgentRegistry = Arc<RwLock<AgentRegistry>>;

/// Create a new shared AgentRegistry.
pub fn new_shared_registry() -> SharedAgentRegistry {
    Arc::new(RwLock::new(AgentRegistry::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode an `AgentStatus` envelope for tests. The Runtime encodes the
    /// same shape (`encode_agent_status_payload` in
    /// `acowork-runtime/src/mqtt/client.rs`); the helper exists here so
    /// `agent_registry` tests do not depend on the runtime crate.
    fn status_bytes(instance_id: &str, online: bool, node_id: &str) -> Vec<u8> {
        use acowork_core::mqtt_proto::{data_envelope, AgentStatus as AgentStatusProto, DataEnvelope};
        use prost::Message as _;
        DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AgentStatus(AgentStatusProto {
                agent_id: String::new(),
                online,
                instance_id: instance_id.to_string(),
                node_id: node_id.to_string(),
                state: 0,
                detail: String::new(),
            })),
        }
        .encode_to_vec()
    }

    #[test]
    fn test_update_from_mqtt_online() {
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt(
            "acowork/agents/3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c/status",
            &status_bytes("3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c", true, "node-a"),
        );
        assert!(registry.is_online("3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c"));
        assert_eq!(registry.online_count(), 1);
    }

    #[test]
    fn test_update_from_mqtt_offline() {
        let uuid = "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c";
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt(
            &format!("acowork/agents/{uuid}/status"),
            &status_bytes(uuid, true, "node-a"),
        );
        assert!(registry.is_online(uuid));

        registry.update_from_mqtt(
            &format!("acowork/agents/{uuid}/status"),
            &status_bytes(uuid, false, "node-a"),
        );
        assert!(!registry.is_online(uuid));
        assert_eq!(registry.online_count(), 0);
    }

    #[test]
    fn test_update_from_mqtt_invalid_topic() {
        let mut registry = AgentRegistry::new();
        // The topic doesn't match the `acowork/agents/{id}/status`
        // shape; the payload is irrelevant — it should be rejected
        // before any decode attempt.
        registry.update_from_mqtt(
            "invalid/topic",
            &status_bytes("3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c", true, "node-a"),
        );
        assert_eq!(registry.total_tracked(), 0);
    }

    #[test]
    fn test_update_from_mqtt_envelope_is_online() {
        // Sept 2026: the wire is uniformly `DataEnvelope<AgentStatus>`.
        // Round-trip an online envelope and verify the registry accepts it.
        let uuid = "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c";
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt(
            &format!("acowork/agents/{uuid}/status"),
            &status_bytes(uuid, true, "node-a"),
        );
        assert!(registry.is_online(uuid));
        assert_eq!(registry.online_count(), 1);
    }

    #[test]
    fn test_envelope_keys_by_instance_not_agent_id() {
        // ADR-073: the Runtime publishes the instance identity in the
        // envelope's `instance_id` field (UUIDv4). The registry keys on
        // it — keying on the package id would collapse two instances of
        // the same package into one entry.
        let uuid = "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c";
        let mut registry = AgentRegistry::new();
        registry.update_from_mqtt(
            &format!("acowork/agents/{uuid}/status"),
            &status_bytes(uuid, true, "node-a"),
        );

        assert!(registry.is_online(uuid), "instance must be online");
        assert!(
            !registry.is_online(""),
            "the loopback must NOT create an empty-key entry"
        );
        assert_eq!(registry.online_count(), 1);
        let state = registry.agents.get(uuid).expect("entry keyed by instance id");
        assert_eq!(state.instance_id, uuid);
        assert_eq!(state.node_id, "node-a");
    }

    #[test]
    fn test_two_instances_same_package_are_independent() {
        // ADR-073: the same package installed twice publishes on two
        // instance-scoped topics. The online registry must keep the
        // entries independent — offline for one must never flip the other.
        let uuid_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let uuid_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let mut registry = AgentRegistry::new();
        for (uuid, node) in [(uuid_a, "node-a"), (uuid_b, "node-b")] {
            registry.update_from_mqtt(
                &format!("acowork/agents/{uuid}/status"),
                &status_bytes(uuid, true, node),
            );
        }
        assert!(registry.is_online(uuid_a) && registry.is_online(uuid_b));
        assert_eq!(registry.online_count(), 2);

        // B goes offline (crash / LWT) — A must stay online.
        registry.update_from_mqtt(
            &format!("acowork/agents/{uuid_b}/status"),
            &status_bytes(uuid_b, false, "node-b"),
        );
        assert!(!registry.is_online(uuid_b), "B must be offline");
        assert!(registry.is_online(uuid_a), "A must be unaffected");
        assert_eq!(registry.online_count(), 1);
    }

    #[test]
    fn test_online_agents() {
        let mut registry = AgentRegistry::new();
        let uuid_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let uuid_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let uuid_c = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        registry.update_from_mqtt(
            &format!("acowork/agents/{uuid_a}/status"),
            &status_bytes(uuid_a, true, "node-a"),
        );
        registry.update_from_mqtt(
            &format!("acowork/agents/{uuid_b}/status"),
            &status_bytes(uuid_b, true, "node-b"),
        );
        registry.update_from_mqtt(
            &format!("acowork/agents/{uuid_c}/status"),
            &status_bytes(uuid_c, false, "node-c"),
        );

        let online = registry.online_agents();
        assert_eq!(online.len(), 2);
        assert!(online.contains(&uuid_a.to_string()));
        assert!(online.contains(&uuid_b.to_string()));
    }
}
