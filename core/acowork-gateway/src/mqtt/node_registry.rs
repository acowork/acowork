//! Node Registry (ADR-055 Phase 2a).
//!
//! Tracks Node Agents based on MQTT `acowork/nodes/{id}/status`
//! (plain text, Retained + LWT) and `acowork/nodes/{id}/info`
//! (protobuf `DataEnvelope<NodeInfo>`, Retained) messages. Mirrors
//! the `AgentRegistry` pattern (`agent_registry.rs`): LWT-driven
//! online/offline state + retained metadata snapshot.
//!
//! The registry is the Gateway-side source of truth for "which nodes
//! exist / are online" — used by the local-node supervisor (§6.11)
//! and, from Phase 2b on, by the lifecycle/package control plane
//! (`installed_agents.node_id` routing).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use prost::Message as _;
use tokio::sync::RwLock;

use acowork_core::mqtt_proto::{data_envelope, DataEnvelope, NodeInfo};

/// Gateway-side view of one Node Agent.
#[derive(Debug, Clone)]
pub struct NodeInfoState {
    pub node_id: String,
    /// Whether the node is currently online (status topic; LWT flips
    /// this to false when the node dies ungracefully).
    pub online: bool,
    /// Last `NodeInfo` retained snapshot (None until the first info
    /// message arrives).
    pub info: Option<NodeInfo>,
    /// Node-local LSP relay endpoint (ADR-055 §6.7, Phase 4): the
    /// retained `acowork/nodes/{node_id}/lsps` AvailableLsps payload.
    /// `Some(endpoint)` while the node's relay is ready, `None` while
    /// unavailable or before the first message.
    pub lsp_endpoint: Option<String>,
    /// Display name from the info snapshot (ADR-075 D2) — user-facing
    /// only, never a routing key.
    pub node_name: Option<String>,
    /// Long-lived per-node credential issued at enroll time (ADR-055
    /// §6.12 / Phase 5a). Persisted in NodeTokenStore; this slot is
    /// the in-memory mirror so HTTP handlers (X-ACowork-Node-Token
    /// checks) and /api/nodes can read it without the store.
    pub node_token: Option<String>,
    /// When the registry last observed any message from this node.
    pub last_updated: Instant,
    /// UTC timestamp of the last observed online transition (None
    /// while the node has never been online).
    pub online_since: Option<DateTime<Utc>>,
}

/// In-memory registry of Node Agents.
#[derive(Debug, Default)]
pub struct NodeRegistry {
    nodes: HashMap<String, NodeInfoState>,
    /// Exact identity anchor for the Gateway's own-machine node.
    /// ADR-113: this is launcher-local bookkeeping (learned from a
    /// spawn cycle and persisted to `{data_dir}/local_node.json`), NOT
    /// protocol state. It is only used by the Gateway to decide whether
    /// it needs to spawn a child and to pick a loopback download host.
    /// Install/ensure requests never consult it — they carry an
    /// explicit node_id.
    local_node_anchor: Option<String>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin the exact node_id of the Gateway's own-machine node. The
    /// anchor is authoritative in [`local_node_id`]: once set, it wins
    /// over any hostname-based scan.
    pub fn set_local_node_anchor(&mut self, node_id: &str) {
        self.local_node_anchor = Some(node_id.to_string());
    }

    /// Update a node's status from a plain-text MQTT message.
    ///
    /// `topic` must match `acowork/nodes/{node_id}/status`; payload is
    /// "online" / "offline" (UTF-8 text, mirrors the agent status
    /// topic contract). An empty payload (retained clear, sent by the
    /// `nodes remove` CLI) is a removal signal: the node's record is
    /// dropped for good.
    pub fn update_status_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() != 4
            || parts[0] != "acowork"
            || parts[1] != "nodes"
            || parts[3] != "status"
        {
            tracing::warn!(topic, "Invalid node status topic format");
            return;
        }
        let node_id = parts[2];
        if node_id.is_empty() {
            return;
        }

        let payload_str = match std::str::from_utf8(payload) {
            Ok(s) => s.trim(),
            Err(e) => {
                tracing::warn!(
                    topic,
                    node_id,
                    error = %e,
                    "node status payload is not valid UTF-8 — treating as offline"
                );
                "offline"
            }
        };
        // Empty payload = the retained status was cleared, i.e. the
        // node left the fleet for good (ADR-055 §6.13.3, `nodes
        // remove` CLI). Drop the record: the clear is a removal
        // signal and must never resurrect a ghost entry.
        if payload_str.is_empty() {
            let removed = self.nodes.remove(node_id).is_some();
            tracing::info!(
                node_id,
                removed,
                "Node status retained cleared — registry record dropped"
            );
            return;
        }

        let online = payload_str == "online";

        // An `offline` for an unknown node carries no usable state
        // (e.g. a stale LWT replay): do not materialise a record for
        // it — only `online` / `info` / `lsps` messages may create
        // entries.
        if !online && !self.nodes.contains_key(node_id) {
            tracing::debug!(node_id, "Offline status for unknown node — ignored");
            return;
        }

        let now = Instant::now();
        let entry = self.nodes.entry(node_id.to_string()).or_insert_with(|| {
            tracing::info!(node_id, "Node discovered via status topic");
            NodeInfoState {
                node_id: node_id.to_string(),
                online: false,
                info: None,
                lsp_endpoint: None,
                node_name: None,
                node_token: None,
                last_updated: now,
                online_since: None,
            }
        });
        if online && !entry.online {
            entry.online_since = Some(Utc::now());
        } else if !online {
            entry.online_since = None;
        }
        entry.online = online;
        entry.last_updated = now;

        tracing::debug!(node_id, online, "Node registry status updated");
    }

    /// Update a node's metadata from a protobuf `DataEnvelope<NodeInfo>`
    /// MQTT message on `acowork/nodes/{node_id}/info`.
    pub fn update_info_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() != 4
            || parts[0] != "acowork"
            || parts[1] != "nodes"
            || parts[3] != "info"
        {
            tracing::warn!(topic, "Invalid node info topic format");
            return;
        }
        let node_id = parts[2].to_string();

        // Empty retained payload = the info topic was cleared (node
        // removed / left). Clear the snapshot on an existing record
        // but never create one from a cleared topic.
        if payload.is_empty() {
            if let Some(entry) = self.nodes.get_mut(&node_id) {
                entry.info = None;
                entry.node_name = None;
                entry.last_updated = Instant::now();
                tracing::debug!(node_id = %node_id, "Node info cleared (retained empty)");
            }
            return;
        }

        let envelope = match DataEnvelope::decode(payload) {
            Ok(env) => env,
            Err(e) => {
                tracing::warn!(topic, error = %e, "node info payload is not a valid DataEnvelope");
                return;
            }
        };
        let info = match envelope.payload {
            Some(data_envelope::Payload::NodeInfo(info)) => info,
            _ => {
                tracing::warn!(
                    topic,
                    "node info topic carried a non-NodeInfo envelope"
                );
                return;
            }
        };

        if info.node_id != node_id {
            tracing::warn!(
                topic,
                envelope_node_id = %info.node_id,
                "node info payload node_id does not match topic — ignoring"
            );
            return;
        }

        let now = Instant::now();
        let node_name = (!info.node_name.is_empty()).then(|| info.node_name.clone());
        let entry = self.nodes.entry(node_id.clone()).or_insert_with(|| {
            tracing::info!(node_id = %node_id, "Node discovered via info topic");
            NodeInfoState {
                node_id: node_id.clone(),
                online: false,
                info: None,
                lsp_endpoint: None,
                node_name: None,
                node_token: None,
                last_updated: now,
                online_since: None,
            }
        });
        entry.info = Some(info);
        entry.node_name = node_name;
        entry.last_updated = now;
    }

    /// Update a node's LSP relay endpoint from the retained
    /// `acowork/nodes/{node_id}/lsps` topic (ADR-055 §6.7, Phase 4):
    /// protobuf `DataEnvelope<AvailableLsps>`. Ready → `Some(endpoint)`;
    /// unavailable or an empty retained payload (node cleared it) → None.
    pub fn update_lsps_from_mqtt(&mut self, topic: &str, payload: &[u8]) {
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() != 4
            || parts[0] != "acowork"
            || parts[1] != "nodes"
            || parts[3] != "lsps"
        {
            tracing::warn!(topic, "Invalid node lsps topic format");
            return;
        }
        let node_id = parts[2].to_string();

        // Empty retained payload = the node cleared its lsps state
        // (shutdown / leave) — the relay is no longer available.
        // Clear the endpoint on an existing record; never create a
        // record from a cleared topic.
        if payload.is_empty() {
            if let Some(entry) = self.nodes.get_mut(&node_id) {
                entry.lsp_endpoint = None;
                entry.last_updated = Instant::now();
                tracing::debug!(node_id = %node_id, "Node lsps cleared (retained empty)");
            }
            return;
        }

        let envelope = match DataEnvelope::decode(payload) {
            Ok(env) => env,
            Err(e) => {
                tracing::warn!(topic, error = %e, "node lsps payload is not a valid DataEnvelope");
                return;
            }
        };
        let lsps = match envelope.payload {
            Some(data_envelope::Payload::AvailableLsps(lsps)) => lsps,
            _ => {
                tracing::warn!(topic, "node lsps topic carried a non-AvailableLsps envelope");
                return;
            }
        };

        let endpoint = if lsps.ready && !lsps.endpoint.is_empty() {
            Some(lsps.endpoint)
        } else {
            None
        };

        let now = Instant::now();
        let entry = self.nodes.entry(node_id.clone()).or_insert_with(|| {
            tracing::info!(node_id = %node_id, "Node discovered via lsps topic");
            NodeInfoState {
                node_id: node_id.clone(),
                online: false,
                info: None,
                lsp_endpoint: None,
                node_name: None,
                node_token: None,
                last_updated: now,
                online_since: None,
            }
        });
        entry.lsp_endpoint = endpoint;
        entry.last_updated = now;
        tracing::debug!(
            node_id = %node_id,
            endpoint = ?entry.lsp_endpoint,
            "Node registry lsps updated"
        );
    }

    /// Whether a node is currently online.
    pub fn is_online(&self, node_id: &str) -> bool {
        self.nodes.get(node_id).map(|n| n.online).unwrap_or(false)
    }

    /// Get the info snapshot for a node.
    pub fn get(&self, node_id: &str) -> Option<&NodeInfoState> {
        self.nodes.get(node_id)
    }

    /// All known nodes (online + offline).
    pub fn list_nodes(&self) -> Vec<NodeInfoState> {
        let mut nodes: Vec<NodeInfoState> = self.nodes.values().cloned().collect();
        nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        nodes
    }

    /// Number of online nodes.
    pub fn online_count(&self) -> usize {
        self.nodes.values().filter(|n| n.online).count()
    }

    /// Remove a node record (e.g. `nodes remove` CLI, Phase 2c, or
    /// the permanent-offline cleanup in `dispatch`).
    pub fn remove(&mut self, node_id: &str) {
        self.nodes.remove(node_id);
    }

    /// Record the long-lived token minted at enroll time (ADR-055
    /// Phase 5a). Creates the entry when the node was never seen via
    /// status/info topics (enroll may arrive first).
    pub fn set_node_token(&mut self, node_id: &str, token: String) {
        let now = Instant::now();
        let entry = self.nodes.entry(node_id.to_string()).or_insert_with(|| {
            tracing::info!(node_id, "Node registered via enrollment");
            NodeInfoState {
                node_id: node_id.to_string(),
                online: false,
                info: None,
                lsp_endpoint: None,
                node_name: None,
                node_token: None,
                last_updated: now,
                online_since: None,
            }
        });
        entry.node_token = Some(token);
        entry.last_updated = now;
    }
}

/// Thread-safe shared NodeRegistry.
pub type SharedNodeRegistry = Arc<RwLock<NodeRegistry>>;

/// Create a new shared NodeRegistry.
pub fn new_shared_registry() -> SharedNodeRegistry {
    Arc::new(RwLock::new(NodeRegistry::new()))
}

/// The node_id of the Gateway's own-machine node.
///
/// ADR-113: this is a **Gateway-local deployment query**, not protocol
/// identity. It has no knowledge of who launched a node, and install /
/// ensure routing never calls it — see `read_agent_upload` for the
/// explicit-`node_id` contract.
///
/// Resolution order:
///
/// 1. **Anchor** — the id this Gateway recorded during an earlier spawn /
///    supervision cycle, persisted to `{data_dir}/local_node.json` and
///    re-injected via [`NodeRegistry::set_local_node_anchor`], still
///    online. This is launcher-local bookkeeping, not a wire fact: it
///    deliberately does not re-check the hostname, so it also works in
///    the window before the retained `info` snapshot arrives (otherwise
///    the Gateway would spawn a duplicate child on every restart).
/// 2. **Same-hostname scan** — an online node whose reported
///    `NodeInfo.hostname` equals this machine's. Covers the first boot
///    after an upgrade (no anchor file yet) and a deleted anchor file.
///    Multiple hits → newest `last_updated` wins (WARN).
///
/// None → the launcher may decide to spawn a child; no caller may turn
/// `None` into an install target.
pub async fn local_node_id(registry: &SharedNodeRegistry) -> Option<String> {
    // Fast path: the persisted/learned anchor, still online.
    {
        let reg = registry.read().await;
        if let Some(anchor) = reg.local_node_anchor.as_deref()
            && reg.nodes.get(anchor).is_some_and(|n| n.online)
        {
            return Some(anchor.to_string());
        }
    }

    // Fallback: an ONLINE node reporting THIS machine's hostname.
    // Missing info / empty hostname never matches — fail safe (spawn
    // our own node) over guessing.
    let local_hostname = acowork_core::node::system_hostname();
    let mut reg = registry.write().await;
    let mut candidates: Vec<&NodeInfoState> = reg
        .nodes
        .values()
        .filter(|n| n.online && node_is_on_local_machine(n, &local_hostname))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by_key(|n| n.last_updated);
    let best = candidates.pop().expect("non-empty");
    let resolved = best.node_id.clone();
    if !candidates.is_empty() {
        let others: Vec<&str> = candidates.iter().map(|n| n.node_id.as_str()).collect();
        tracing::warn!(
            best = %best.node_id,
            others = ?others,
            "Multiple online nodes report this machine's hostname — using the most recently updated"
        );
    }
    // Memoize so later calls skip the scan (online-ness is re-checked
    // above on every call).
    reg.local_node_anchor = Some(resolved.clone());
    Some(resolved)
}

/// Whether a registry entry reports this machine's hostname.
fn node_is_on_local_machine(node: &NodeInfoState, local_hostname: &str) -> bool {
    node.info.as_ref().is_some_and(|info| {
        !info.hostname.is_empty() && info.hostname.eq_ignore_ascii_case(local_hostname)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Info fixture for a node reporting THIS machine's hostname —
    /// what `acowork-node` always does (`system_hostname()`), and what
    /// the local-node resolution requires.
    fn info_envelope(node_id: &str, node_name: &str) -> Vec<u8> {
        info_envelope_on_host(node_id, node_name, &acowork_core::node::system_hostname())
    }

    /// Info fixture reporting an arbitrary hostname (cross-machine
    /// scenarios).
    fn info_envelope_on_host(
        node_id: &str,
        node_name: &str,
        hostname: &str,
    ) -> Vec<u8> {
        let info = NodeInfo {
            node_id: node_id.to_string(),
            node_name: node_name.to_string(),
            hostname: hostname.to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec![],
            max_agents: 16,
            agent_count: 0,
            http_endpoint: "http://127.0.0.1:19900".to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeInfo(info)),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn status_topic_drives_online_state() {
        let mut registry = NodeRegistry::new();
        registry.update_status_from_mqtt("acowork/nodes/local/status", b"online");
        assert!(registry.is_online("local"));
        assert_eq!(registry.online_count(), 1);
        assert!(registry.get("local").unwrap().online_since.is_some());

        // LWT flips it offline.
        registry.update_status_from_mqtt("acowork/nodes/local/status", b"offline");
        assert!(!registry.is_online("local"));
        assert!(registry.get("local").unwrap().online_since.is_none());
    }

    #[test]
    fn empty_status_payload_drops_record() {
        let mut registry = NodeRegistry::new();
        registry.update_status_from_mqtt("acowork/nodes/local/status", b"online");
        assert!(registry.get("local").is_some());

        // Retained clear (nodes remove) = removal signal.
        registry.update_status_from_mqtt("acowork/nodes/local/status", b"");
        assert!(registry.get("local").is_none());
        assert!(registry.list_nodes().is_empty());

        // Clearing an unknown node is a no-op.
        registry.update_status_from_mqtt("acowork/nodes/ghost/status", b"");
        assert!(registry.list_nodes().is_empty());
    }

    #[test]
    fn offline_status_for_unknown_node_creates_no_record() {
        let mut registry = NodeRegistry::new();
        registry.update_status_from_mqtt("acowork/nodes/ghost/status", b"offline");
        assert!(registry.get("ghost").is_none());
        assert!(registry.list_nodes().is_empty());
    }

    #[test]
    fn info_topic_populates_metadata() {
        let mut registry = NodeRegistry::new();
        registry.update_info_from_mqtt(
            "acowork/nodes/gpu-1/info",
            &info_envelope("gpu-1", "gpu-box"),
        );
        let node = registry.get("gpu-1").unwrap();
        assert_eq!(node.node_name.as_deref(), Some("gpu-box"));
        assert_eq!(node.info.as_ref().unwrap().protocol_version, 1);
        // Info alone does not imply online.
        assert!(!registry.is_online("gpu-1"));
    }

    #[test]
    fn empty_info_payload_clears_without_creating_record() {
        let mut registry = NodeRegistry::new();
        registry.update_info_from_mqtt(
            "acowork/nodes/gpu-1/info",
            &info_envelope("gpu-1", "gpu-box"),
        );
        registry.update_info_from_mqtt("acowork/nodes/gpu-1/info", &[]);
        let node = registry.get("gpu-1").unwrap();
        assert!(node.info.is_none());
        assert!(node.node_name.is_none());

        // Unknown node stays unknown.
        registry.update_info_from_mqtt("acowork/nodes/ghost/info", &[]);
        assert!(registry.get("ghost").is_none());
    }

    #[test]
    fn node_id_mismatch_between_topic_and_payload_is_rejected() {
        let mut registry = NodeRegistry::new();
        registry.update_info_from_mqtt(
            "acowork/nodes/other/info",
            &info_envelope("gpu-1", "gpu-box"),
        );
        // The info is untrustworthy (topic says "other", payload says
        // "gpu-1") — no entry is created at all; the status topic will
        // create "other" separately when the node publishes liveness.
        assert!(registry.get("gpu-1").is_none());
        assert!(registry.get("other").is_none());
    }

    #[test]
    fn invalid_topics_are_ignored() {
        let mut registry = NodeRegistry::new();
        registry.update_status_from_mqtt("acowork/agents/x/status", b"online");
        registry.update_status_from_mqtt("acowork/nodes/a/b/status", b"online");
        registry.update_info_from_mqtt("acowork/nodes/local/status", b"garbage");
        assert!(registry.list_nodes().is_empty());
    }

    #[test]
    fn list_nodes_is_sorted_by_node_id() {
        let mut registry = NodeRegistry::new();
        registry.update_status_from_mqtt("acowork/nodes/zeta/status", b"online");
        registry.update_status_from_mqtt("acowork/nodes/alpha/status", b"online");
        let ids: Vec<String> = registry.list_nodes().into_iter().map(|n| n.node_id).collect();
        assert_eq!(ids, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    fn lsps_envelope(endpoint: &str, ready: bool) -> Vec<u8> {
        let lsps = acowork_core::mqtt_proto::AvailableLsps {
            version: 1,
            endpoint: endpoint.to_string(),
            ready,
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AvailableLsps(lsps)),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn lsps_topic_sets_endpoint_when_ready() {
        let mut registry = NodeRegistry::new();
        registry.update_lsps_from_mqtt(
            "acowork/nodes/gpu-1/lsps",
            &lsps_envelope("http://192.168.1.10:19878", true),
        );
        assert_eq!(
            registry.get("gpu-1").unwrap().lsp_endpoint.as_deref(),
            Some("http://192.168.1.10:19878")
        );
    }

    #[test]
    fn lsps_topic_clears_endpoint_when_not_ready() {
        let mut registry = NodeRegistry::new();
        registry.update_lsps_from_mqtt(
            "acowork/nodes/gpu-1/lsps",
            &lsps_envelope("http://192.168.1.10:19878", true),
        );
        registry.update_lsps_from_mqtt("acowork/nodes/gpu-1/lsps", &lsps_envelope("", false));
        assert!(registry.get("gpu-1").unwrap().lsp_endpoint.is_none());
    }

    #[test]
    fn lsps_topic_empty_retained_payload_clears_endpoint() {
        let mut registry = NodeRegistry::new();
        registry.update_lsps_from_mqtt(
            "acowork/nodes/gpu-1/lsps",
            &lsps_envelope("http://192.168.1.10:19878", true),
        );
        registry.update_lsps_from_mqtt("acowork/nodes/gpu-1/lsps", &[]);
        assert!(registry.get("gpu-1").unwrap().lsp_endpoint.is_none());
    }

    #[test]
    fn empty_lsps_payload_for_unknown_node_creates_no_record() {
        let mut registry = NodeRegistry::new();
        registry.update_lsps_from_mqtt("acowork/nodes/ghost/lsps", &[]);
        assert!(registry.get("ghost").is_none());
        assert!(registry.list_nodes().is_empty());
    }

    #[test]
    fn invalid_lsps_topics_are_ignored() {
        let mut registry = NodeRegistry::new();
        registry.update_lsps_from_mqtt("acowork/global/lsps", &lsps_envelope("x", true));
        registry.update_lsps_from_mqtt("acowork/nodes/a/b/lsps", &lsps_envelope("x", true));
        assert!(registry.list_nodes().is_empty());
    }

    /// ADR-113 architecture guard: resolving the own-machine node uses
    /// ONLY protocol-visible facts (online + reported hostname). There
    /// is no "spawned by the Gateway" bit left for it to read, so a node
    /// started by the Desktop, a CLI, or a remote launcher resolves
    /// identically.
    #[tokio::test]
    async fn local_node_id_depends_only_on_online_and_hostname() {
        let registry = new_shared_registry();
        assert_eq!(local_node_id(&registry).await, None, "no node → None");

        // Known and online, but no `info` snapshot yet → cannot prove
        // it is on this machine. Fail safe.
        registry
            .write()
            .await
            .update_status_from_mqtt("acowork/nodes/node-1/status", b"online");
        assert_eq!(local_node_id(&registry).await, None);

        // Info reporting this machine's hostname → resolved, with no
        // spawn-marker involved anywhere in the chain.
        registry
            .write()
            .await
            .update_info_from_mqtt(
                "acowork/nodes/node-1/info",
                &info_envelope("node-1", "box-1"),
            );
        assert_eq!(local_node_id(&registry).await.as_deref(), Some("node-1"));

        // Offline → None again.
        registry
            .write()
            .await
            .update_status_from_mqtt("acowork/nodes/node-1/status", b"offline");
        assert_eq!(local_node_id(&registry).await, None);
    }

    #[tokio::test]
    async fn local_node_id_prefers_most_recently_updated_on_multiple_hits() {
        let registry = new_shared_registry();
        for id in ["local-1", "local-2"] {
            registry
                .write()
                .await
                .update_status_from_mqtt(&format!("acowork/nodes/{id}/status"), b"online");
            registry
                .write()
                .await
                .update_info_from_mqtt(
                    &format!("acowork/nodes/{id}/info"),
                    &info_envelope(id, &format!("{id}-box")),
                );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // local-2 was updated last → it wins.
        assert_eq!(local_node_id(&registry).await.as_deref(), Some("local-2"));
    }

    #[tokio::test]
    async fn local_node_id_rejects_a_node_on_another_machine() {
        // 2026-09-20 incident regression: a node on ANOTHER machine
        // must never resolve as this machine's own, regardless of who
        // launched it. Hostname is the protocol-visible discriminator.
        let registry = new_shared_registry();
        registry
            .write()
            .await
            .update_status_from_mqtt("acowork/nodes/remote-managed/status", b"online");
        registry
            .write()
            .await
            .update_info_from_mqtt(
                "acowork/nodes/remote-managed/info",
                &info_envelope_on_host("remote-managed", "nytb", "OTHER-PC"),
            );
        assert_eq!(local_node_id(&registry).await, None);

        // The real local node (reports this machine's hostname) still
        // resolves alongside it.
        registry
            .write()
            .await
            .update_status_from_mqtt("acowork/nodes/local-1/status", b"online");
        registry
            .write()
            .await
            .update_info_from_mqtt(
                "acowork/nodes/local-1/info",
                &info_envelope("local-1", "local-box"),
            );
        assert_eq!(local_node_id(&registry).await.as_deref(), Some("local-1"));
    }

    #[tokio::test]
    async fn local_node_id_requires_a_non_empty_hostname() {
        // No reported hostname cannot be proven local — fail safe
        // (spawn our own) over guessing.
        let registry = new_shared_registry();
        registry
            .write()
            .await
            .update_status_from_mqtt("acowork/nodes/unknown-host/status", b"online");
        registry
            .write()
            .await
            .update_info_from_mqtt(
                "acowork/nodes/unknown-host/info",
                &info_envelope_on_host("unknown-host", "m1", ""),
            );
        assert_eq!(local_node_id(&registry).await, None);
    }

    #[tokio::test]
    async fn local_node_id_prefers_exact_anchor_over_candidates() {
        let registry = new_shared_registry();
        // Anchor recorded by a previous spawn/supervision cycle.
        registry.write().await.set_local_node_anchor("local-1");
        for id in ["local-1", "local-2"] {
            registry
                .write()
                .await
                .update_status_from_mqtt(&format!("acowork/nodes/{id}/status"), b"online");
            registry
                .write()
                .await
                .update_info_from_mqtt(
                    &format!("acowork/nodes/{id}/info"),
                    &info_envelope(id, &format!("{id}-box")),
                );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // local-2 is newer, but the exact anchor wins.
        assert_eq!(local_node_id(&registry).await.as_deref(), Some("local-1"));

        // Anchor goes offline → fall back to the same-hostname scan.
        registry
            .write()
            .await
            .update_status_from_mqtt("acowork/nodes/local-1/status", b"offline");
        assert_eq!(local_node_id(&registry).await.as_deref(), Some("local-2"));
    }

    #[tokio::test]
    async fn local_node_id_stale_anchor_falls_back_to_hostname_scan() {
        let registry = new_shared_registry();
        // Anchor points at a node that is gone (identity re-minted).
        registry.write().await.set_local_node_anchor("ghost");
        registry
            .write()
            .await
            .update_status_from_mqtt("acowork/nodes/local-1/status", b"online");
        registry
            .write()
            .await
            .update_info_from_mqtt(
                "acowork/nodes/local-1/info",
                &info_envelope("local-1", "local-box"),
            );
        assert_eq!(local_node_id(&registry).await.as_deref(), Some("local-1"));
    }
}
