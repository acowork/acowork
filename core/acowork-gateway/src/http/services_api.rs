//! Full-stack service diagnostics API (plan: desktop-unified-diagnostics §3.3 P2).
//!
//! `GET /api/services/diagnose` — a **Gateway-perspective** snapshot of
//! every service the Desktop's diagnostic panel renders (gateway, mqtt,
//! embed, pm, doc, memory, nodes). This is the "server-side aggregate" half
//! of P2: instead of the Desktop issuing 3+ separate probes (each with its
//! own timeout budget) it can fetch this one endpoint and learn the
//! Gateway's ground truth about its own subsystems.
//!
//! Why the Gateway (and not the Node) answers this:
//! - The Gateway *supervises* embed / pm / doc / memory (`embed_process`,
//!   `pm_process`, `doc_process`, `memory_process`) — it holds their
//!   pid / port / ready state. No other process has this view.
//! - The Gateway's `NodeRegistry` is the LWT-driven source of truth for
//!   node online state.
//! - LSP relay is a **node-local** sidecar (ADR-055 §6.7) — the Gateway
//!   has no direct view of it, so this endpoint reports the node-level
//!   capability flag instead and the Desktop keeps probing it
//!   per-node when needed.
//!
//! **Remote topology (P2-1)**: a Desktop pointed at a remote Gateway
//! fetches this endpoint over the network — every subsystem row it
//! describes lives on the Gateway host, so nothing needs to cross the
//! loopback boundary. This is what makes the panel behave identically
//! in local / remote deployments.
//!
//! **Scope discipline (plan §3.3 P2-2)**: this handler reads only
//! in-memory state — no active network probes, no locks held across
//! awaits. Subsystem "online" means "the supervisor last reported
//! ready", which is exactly the signal the Desktop's retry button is
//! meant to re-verify.

use axum::{extract::State, routing::get, Json, Router};

use serde::Serialize;

use crate::http::routes::AppState;

/// One supervised-subsystem row (embed / pm / doc share this shape).
#[derive(Debug, Serialize)]
pub struct SubsystemSnapshot {
    /// Whether the supervisor holds a live process entry.
    pub running: bool,
    /// Whether the process completed startup + health check.
    pub ready: bool,
    /// Port the subsystem listens on (0 when not running).
    pub port: u16,
    /// PID of the subsystem process (0 when not running; also 0 for an
    /// externally-started embed service).
    pub pid: u32,
    /// Embed-only: loaded model id; other subsystems omit this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_model_id: Option<String>,
}

/// Gateway self-description row.
#[derive(Debug, Serialize)]
pub struct GatewaySnapshot {
    pub version: String,
    /// ADR-059: per-process instance id (fresh on every start).
    pub instance_id: String,
    pub http_port: u16,
    pub mqtt_port: u16,
    pub agents_running: usize,
    pub agents_installed: usize,
}

/// MQTT broker row.
#[derive(Debug, Serialize)]
pub struct MqttSnapshot {
    /// Whether the embedded broker handle is live.
    pub broker_running: bool,
    pub port: u16,
    /// ADR-055 Phase 5a: whether CONNECT auth is enforced.
    pub auth_enabled: bool,
}

/// One node row (subset of `/api/nodes` — the fields a diagnostic panel
/// renders, kept small so the payload stays one screen of JSON).
#[derive(Debug, Serialize)]
pub struct NodeDiagnosticRow {
    pub node_id: String,
    pub online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_version: Option<String>,
    /// Node-level LSP relay capability (ADR-055 §6.7). The relay itself
    /// is node-local; this flag is how its existence is surfaced here.
    #[serde(default)]
    pub has_lsp_relay: bool,
}

/// Node topology row.
#[derive(Debug, Serialize)]
pub struct NodesSnapshot {
    pub total: usize,
    pub online: usize,
    pub items: Vec<NodeDiagnosticRow>,
}

/// `GET /api/services/diagnose` response.
#[derive(Debug, Serialize)]
pub struct ServicesDiagnoseResponse {
    pub gateway: GatewaySnapshot,
    pub mqtt: MqttSnapshot,
    pub embed: SubsystemSnapshot,
    pub pm: SubsystemSnapshot,
    pub doc: SubsystemSnapshot,
    /// ADR-100: the standalone memory service (Gateway-spawned supervisor).
    pub memory: SubsystemSnapshot,
    pub nodes: NodesSnapshot,
    /// RFC 3339 UTC timestamp of this snapshot.
    pub diagnosed_at: String,
}

/// `GET /api/services/diagnose` — Gateway-perspective service snapshot.
pub async fn diagnose_services(State(state): State<AppState>) -> Json<ServicesDiagnoseResponse> {
    // Lock discipline: the only other lock taken while `gw` is held is
    // `mqtt_broker_control` (a short-lived tokio::sync::Mutex), matching
    // the global Gateway lock order (gateway_state → mqtt_broker_control).
    // The agent / node registries use independent RwLocks.
    let gw = state.gateway_state.read().await;

    let http_port = gw
        .config
        .as_ref()
        .map(|c| c.http.port)
        .unwrap_or_else(crate::config::default_http_port);
    let mqtt_port = gw
        .config
        .as_ref()
        .map(|c| c.mqtt.port)
        .unwrap_or_else(crate::config::default_mqtt_port);

    // MQTT broker liveness: the control handle is only populated while
    // the embedded broker thread is running (None when mqtt disabled).
    let broker_running = gw.mqtt_broker_control.lock().await.is_some();
    let auth_enabled = gw
        .mqtt_broker_auth
        .as_ref()
        .map(|a| a.auth_enabled)
        .unwrap_or(false);

    let embed = match gw.embed_process.as_ref() {
        Some(e) => SubsystemSnapshot {
            running: true,
            ready: e.ready,
            port: e.port,
            pid: e.pid,
            active_model_id: e.active_model_id.clone(),
        },
        None => not_running(),
    };
    let pm = match gw.pm_process.as_ref() {
        Some(p) => SubsystemSnapshot {
            running: true,
            ready: p.ready,
            port: p.port,
            pid: p.pid,
            active_model_id: None,
        },
        None => not_running(),
    };
    let doc = match gw.doc_process.as_ref() {
        Some(p) => SubsystemSnapshot {
            running: true,
            ready: p.ready,
            port: p.port,
            pid: p.pid,
            active_model_id: None,
        },
        None => not_running(),
    };
    // ADR-100: the memory service is supervised exactly like doc, so its row
    // reuses the shape — a `running: false` row is what tells the panel the
    // `/api/memory/*` surface is currently 503.
    let memory = match gw.memory_process.as_ref() {
        Some(p) => SubsystemSnapshot {
            running: true,
            ready: p.ready,
            port: p.port,
            pid: p.pid,
            active_model_id: None,
        },
        None => not_running(),
    };

    // Agent online count — same source as `/api/status` (ADR-033).
    let agents_running = if let Some(ref reg) = state.agent_registry {
        reg.read().await.online_count()
    } else {
        gw.running_agents.len()
    };

    // Node topology — read from the registry AFTER dropping the gateway
    // state guard would be ideal, but `list_nodes()` only touches the
    // registry; the gateway guard here is read-only and the registry
    // call is a plain in-memory read, so holding both briefly is fine.
    let nodes = match state.node_registry.as_ref() {
        Some(registry) => {
            let reg = registry.read().await;
            let all = reg.list_nodes();
            let online = all.iter().filter(|n| n.online).count();
            let items: Vec<NodeDiagnosticRow> = all
                .into_iter()
                .map(|n| {
                    let info = n.info.as_ref();
                    NodeDiagnosticRow {
                        node_id: n.node_id,
                        online: n.online,
                        node_name: n.node_name.clone(),
                        hostname: info.map(|i| i.hostname.clone()),
                        node_version: info.map(|i| i.node_version.clone()),
                        // H-3: the relay availability lives in the retained
                        // `acowork/nodes/{id}/lsps` topic (`lsp_endpoint`),
                        // NOT in `NodeInfo.capabilities` — the Node never
                        // advertises an "lsp-relay" capability string, so
                        // the old capability check was always false.
                        has_lsp_relay: n.lsp_endpoint.is_some(),
                    }
                })
                .collect();
            NodesSnapshot {
                total: items.len(),
                online,
                items,
            }
        }
        // No node registry (MQTT disabled) — empty topology, not an error.
        None => NodesSnapshot {
            total: 0,
            online: 0,
            items: Vec::new(),
        },
    };

    Json(ServicesDiagnoseResponse {
        gateway: GatewaySnapshot {
            version: env!("CARGO_PKG_VERSION").to_string(),
            instance_id: gw.instance_id.clone(),
            http_port,
            mqtt_port,
            agents_running,
            agents_installed: gw.installed_agents.len(),
        },
        mqtt: MqttSnapshot {
            broker_running,
            port: mqtt_port,
            auth_enabled,
        },
        embed,
        pm,
        doc,
        memory,
        nodes,
        diagnosed_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// The all-zero row for a subsystem whose supervisor holds no process.
fn not_running() -> SubsystemSnapshot {
    SubsystemSnapshot {
        running: false,
        ready: false,
        port: 0,
        pid: 0,
        active_model_id: None,
    }
}

/// Route definitions for the service diagnostics API.
pub fn services_routes() -> Router<AppState> {
    Router::new().route("/api/services/diagnose", get(diagnose_services))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::routes::AppState;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-services-api-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        )
    }

    /// Empty state: every subsystem reports not-running, node topology is
    /// empty — but the handler still returns a complete, well-formed row
    /// set (the Desktop panel must never see missing keys).
    #[tokio::test]
    async fn diagnose_returns_complete_snapshot_on_empty_state() {
        let state = test_app_state();
        let Json(resp) = diagnose_services(State(state)).await;

        assert!(!resp.gateway.version.is_empty());
        assert!(resp.gateway.http_port > 0);
        assert!(resp.gateway.mqtt_port > 0);

        assert!(!resp.embed.running);
        assert!(!resp.embed.ready);
        assert_eq!(resp.embed.port, 0);
        assert!(!resp.pm.running);
        assert!(!resp.doc.running);

        assert_eq!(resp.nodes.total, 0);
        assert_eq!(resp.nodes.online, 0);
        assert!(resp.nodes.items.is_empty());

        assert!(!resp.diagnosed_at.is_empty());
    }

    /// Serialization contract: field names are snake_case (the Desktop's
    /// TypeScript types depend on it), and `active_model_id` is omitted
    /// for pm/doc (skip_serializing_if).
    #[tokio::test]
    async fn diagnose_serializes_with_snake_case_contract() {
        let state = test_app_state();
        let Json(resp) = diagnose_services(State(state)).await;
        let json = serde_json::to_value(&resp).expect("serializes");

        assert!(json["gateway"]["http_port"].is_u64());
        assert!(json["gateway"]["agents_running"].is_u64());
        assert!(json["mqtt"]["broker_running"].is_boolean());
        assert!(json["mqtt"]["auth_enabled"].is_boolean());
        assert!(json["nodes"]["total"].is_u64());
        // pm/doc rows must NOT carry the embed-only field.
        assert!(json["pm"].get("active_model_id").is_none());
        assert!(json["doc"].get("active_model_id").is_none());
        assert!(json["memory"].get("active_model_id").is_none());
    }

    /// When a supervisor process state is present, its fields flow
    /// through verbatim (this is the "embed is up" path the panel
    /// renders as a green row).
    #[tokio::test]
    async fn diagnose_reports_running_embed_process() {
        let state = test_app_state();
        {
            let mut gw = state.gateway_state.write().await;
            gw.embed_process = Some(crate::lifecycle::embed::EmbedProcessState {
                pid: 4242,
                port: 19901,
                active_model_id: Some("bge-small-zh-v1.5".to_string()),
                active_dimension: Some(512),
                ready: true,
            });
        }
        let Json(resp) = diagnose_services(State(state)).await;
        assert!(resp.embed.running);
        assert!(resp.embed.ready);
        assert_eq!(resp.embed.pid, 4242);
        assert_eq!(resp.embed.port, 19901);
        assert_eq!(
            resp.embed.active_model_id.as_deref(),
            Some("bge-small-zh-v1.5")
        );
    }

    /// Build a retained `AvailableLsps` envelope exactly like the Node
    /// publishes it on `acowork/nodes/{id}/lsps`.
    fn lsps_envelope(endpoint: &str, ready: bool) -> Vec<u8> {
        use acowork_core::mqtt_proto::{data_envelope, AvailableLsps, DataEnvelope};
        use prost::Message as _;
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::AvailableLsps(AvailableLsps {
                version: 1,
                endpoint: endpoint.to_string(),
                ready,
            })),
        };
        envelope.encode_to_vec()
    }

    /// H-3 regression: `has_lsp_relay` is powered by the retained
    /// `acowork/nodes/{id}/lsps` endpoint (`NodeRegistry::lsp_endpoint`),
    /// not by a non-existent `NodeInfo.capabilities` string — the old
    /// capability check made every node report `false` in production
    /// while the fixture-faked unit tests stayed green.
    #[tokio::test]
    async fn diagnose_reports_lsp_relay_from_node_registry_endpoint() {
        let mut state = test_app_state();
        let registry = crate::mqtt::new_shared_node_registry();
        {
            let mut reg = registry.write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
            reg.update_lsps_from_mqtt(
                "acowork/nodes/gpu-1/lsps",
                &lsps_envelope("http://10.0.0.7:19878", true),
            );
        }
        state.node_registry = Some(registry);

        let Json(resp) = diagnose_services(State(state)).await;
        assert_eq!(resp.nodes.total, 1);
        assert_eq!(resp.nodes.online, 1);
        assert!(
            resp.nodes.items[0].has_lsp_relay,
            "a ready lsps endpoint must flip has_lsp_relay on"
        );
    }

    /// The flag stays off while the relay is unavailable (ready=false) or
    /// before the node ever publishes a `lsps` snapshot.
    #[tokio::test]
    async fn diagnose_reports_lsp_relay_off_without_ready_endpoint() {
        let mut state = test_app_state();
        let registry = crate::mqtt::new_shared_node_registry();
        {
            let mut reg = registry.write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
            reg.update_lsps_from_mqtt("acowork/nodes/gpu-1/lsps", &lsps_envelope("", false));
        }
        state.node_registry = Some(registry);

        let Json(resp) = diagnose_services(State(state)).await;
        assert_eq!(resp.nodes.online, 1);
        assert!(!resp.nodes.items[0].has_lsp_relay);
    }
}
