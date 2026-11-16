//! ADR-059 Phase 1.3 — HTTP projection of the bootstrap snapshot.
//!
//! `GET /api/bootstrap` exposes the same aggregated `BootstrapState`
//! that the retained MQTT topic `acowork/global/bootstrap` carries
//! (both read the same orchestrator snapshot), so HTTP and MQTT
//! consumers always agree on `instance_id` / `version` / `phase`.
//!
//! Liveness vs readiness split (ADR-059 §5.1):
//! - `GET /health` (routes.rs) is liveness-only — answers when the
//!   process is up; says nothing about subsystem readiness.
//! - `GET /api/bootstrap` is the readiness source of truth — the
//!   phase transitions BOOTING → READY / DEGRADED / FAILED and the
//!   client waits for READY before submitting dependent work.

use axum::{extract::State, routing::get, Json, Router};
#[cfg(test)]
use axum::http::StatusCode;
use serde::Serialize;

use crate::bootstrap::orchestrator::BootstrapPhase;
use crate::http::routes::{ApiError, AppState};

/// Proto-name of a phase, e.g. `BOOTING`, `SHUTTING_DOWN`.
///
/// Explicit mapping (not `Debug` + uppercase): `ShuttingDown` would
/// uppercase to `SHUTTINGDOWN` and drift from the proto enum name.
fn phase_name(phase: BootstrapPhase) -> &'static str {
    match phase {
        BootstrapPhase::Unspecified => "UNSPECIFIED",
        BootstrapPhase::Booting => "BOOTING",
        BootstrapPhase::Ready => "READY",
        BootstrapPhase::Degraded => "DEGRADED",
        BootstrapPhase::Failed => "FAILED",
        BootstrapPhase::ShuttingDown => "SHUTTING_DOWN",
    }
}

/// JSON projection of the latest bootstrap snapshot.
///
/// Field-for-field the wire-level `BootstrapState` proto — protocol
/// fields only, no subsystem internals (ADR-059 §5.4.4). `phase` is
/// the SCREAMING_SNAKE_CASE name of the proto enum (`BOOTING`,
/// `READY`, `DEGRADED`, `FAILED`, `SHUTTING_DOWN`).
#[derive(Debug, Clone, Serialize)]
pub struct BootstrapStateView {
    pub protocol_version: u32,
    pub instance_id: String,
    pub version: u64,
    pub phase: String,
    pub phase_detail: String,
    pub issued_at_ms: u64,
}

/// Bootstrap projection routes.
pub fn bootstrap_routes() -> Router<AppState> {
    Router::new().route("/api/bootstrap", get(get_bootstrap))
}

/// `GET /api/bootstrap` — latest aggregated bootstrap state.
///
/// Always `200` while the Gateway is still booting: the payload carries
/// the current phase and the caller decides how to react. The handler
/// NEVER fabricates a `READY` — the phase comes straight from the
/// orchestrator's snapshot.
///
/// `503` only when the orchestrator has not been attached yet (a
/// wiring defect; the HTTP server starts after the orchestrator is
/// attached in `Gateway::run`, so in practice this never fires).
pub async fn get_bootstrap(
    State(state): State<AppState>,
) -> Result<Json<BootstrapStateView>, ApiError> {
    let gw = state.gateway_state.read().await;
    let Some(orchestrator) = gw.bootstrap.orchestrator.clone() else {
        return Err(ApiError::service_unavailable(
            "bootstrap orchestrator not initialised",
        ));
    };
    let snap = orchestrator.snapshot();
    Ok(Json(BootstrapStateView {
        protocol_version: snap.protocol_version,
        instance_id: snap.instance_id,
        version: snap.version,
        phase: phase_name(snap.phase).to_string(),
        phase_detail: snap.phase_detail,
        issued_at_ms: snap.issued_at_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use crate::bootstrap::{ReadinessKind, SubsystemReadinessRegistry};
    use crate::bootstrap::orchestrator::BootstrapSnapshot;
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;

    /// Unique temp dir per test — the shared helper's sequence number keeps
    /// parallel tests from sharing a path (a shared path means one test's
    /// `remove_dir_all` deletes another test's open vault → "Cannot create a
    /// file when that file already exists").
    fn unique_temp_dir() -> std::path::PathBuf {
        crate::test_support::unique_temp_dir("bootstrap-api")
    }

    /// AppState with a live orchestrator attached; returns the registry
    /// so tests can drive readiness transitions.
    async fn test_state() -> (AppState, Arc<SubsystemReadinessRegistry>) {
        let dir = unique_temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = Arc::new(RwLock::new(GatewayState::new(&dir.to_string_lossy())));
        let registry = SubsystemReadinessRegistry::new_shared();
        let orch = crate::bootstrap::BootstrapOrchestrator::new(
            "instance-test".to_string(),
            registry.clone(),
        );
        {
            let mut gw = gw_state.write().await;
            gw.bootstrap.orchestrator = Some(orch.clone());
        }
        let state = AppState::new(gw_state, Arc::new(HttpAuth::new(false)));
        (state, registry)
    }

    #[tokio::test]
    async fn returns_booting_snapshot() {
        let (state, _) = test_state().await;
        let resp = get_bootstrap(State(state)).await.unwrap();
        assert_eq!(resp.phase, "BOOTING");
        assert_eq!(resp.instance_id, "instance-test");
        assert_eq!(resp.version, 1);
        assert!(resp.protocol_version >= 1);
    }

    #[tokio::test]
    async fn phase_advances_to_ready() {
        let (state, registry) = test_state().await;
        // All required subsystems ready → phase READY. node.local is
        // intentionally NOT included: under the unified single-machine
        // / remote topology it is no longer a Required subsystem (the
        // local node subsumes the old `node.local` role via its
        // machine-slug entry, e.g. `node.nytb`).
        for id in ["vault", "mqtt", "publisher", "node.nytb"] {
            registry.register(id, ReadinessKind::Required).mark_ready(None);
        }
        // The orchestrator's background listener recomputes
        // asynchronously; poll the projection briefly.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut resp = get_bootstrap(State(state.clone())).await.unwrap();
        while resp.phase != "READY" && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            resp = get_bootstrap(State(state.clone())).await.unwrap();
        }
        assert_eq!(resp.phase, "READY");
        assert!(resp.version >= 2);
        assert!(resp.phase_detail.contains("required ready"));
    }

    #[tokio::test]
    async fn optional_failure_yields_degraded() {
        let (state, registry) = test_state().await;
        for id in ["vault", "mqtt", "publisher", "node.nytb"] {
            registry.register(id, ReadinessKind::Required).mark_ready(None);
        }
        registry
            .register("embedding", ReadinessKind::Optional)
            .mark_failed(Some("boom".into()));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut resp = get_bootstrap(State(state.clone())).await.unwrap();
        while resp.phase != "DEGRADED" && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            resp = get_bootstrap(State(state.clone())).await.unwrap();
        }
        assert_eq!(resp.phase, "DEGRADED");
    }
    #[tokio::test]
    async fn missing_orchestrator_is_503() {
        let dir = unique_temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = Arc::new(RwLock::new(GatewayState::new(&dir.to_string_lossy())));
        let state = AppState::new(gw_state, Arc::new(HttpAuth::new(false)));
        let err = get_bootstrap(State(state)).await.unwrap_err();
        assert_eq!(err.code, StatusCode::SERVICE_UNAVAILABLE.as_u16());
    }

    /// The JSON shape is stable: exactly the 6 protocol fields, phase
    /// serialised as the SCREAMING_SNAKE_CASE proto name.
    #[test]
    fn view_serialises_to_protocol_shape() {
        // ADR-073: the bootstrap instance identity is a UUIDv4, not a
        // human label — pin the exact value through serialisation.
        let instance_id = "4e5f6a7b-8c9d-4e0e-8f1f-c02a3b4c5d6e";
        let view = BootstrapStateView {
            protocol_version: 1,
            instance_id: instance_id.to_string(),
            version: 4,
            phase: "DEGRADED".to_string(),
            phase_detail: "2/5 required ready".to_string(),
            issued_at_ms: 123456,
        };
        let json = serde_json::to_value(&view).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 6, "exactly the 6 protocol fields");
        assert_eq!(obj["phase"], "DEGRADED");
        assert_eq!(obj["instance_id"], instance_id);
        assert_eq!(obj["version"], 4);
    }

    /// The HTTP projection must be field-for-field consistent with the
    /// MQTT protobuf snapshot — consumers on either channel observe the
    /// same `instance_id` / `version` / `phase` (ADR-059 §8.3).
    #[test]
    fn http_projection_matches_mqtt_proto_fields() {
        let snap = BootstrapSnapshot {
            protocol_version: 1,
            instance_id: "5f6a7b8c-9d0e-4f1f-8021-d13b4c5d6e7f".to_string(),
            version: 5,
            phase: BootstrapPhase::Ready,
            phase_detail: "2/2 required ready".to_string(),
            issued_at_ms: 123456,
        };
        let proto = snap.to_proto();
        let view = BootstrapStateView {
            protocol_version: snap.protocol_version,
            instance_id: snap.instance_id,
            version: snap.version,
            phase: phase_name(snap.phase).to_string(),
            phase_detail: snap.phase_detail,
            issued_at_ms: snap.issued_at_ms,
        };
        assert_eq!(view.instance_id, proto.instance_id);
        assert_eq!(view.version, proto.version);
        assert_eq!(view.protocol_version, proto.protocol_version);
        assert_eq!(view.phase_detail, proto.phase_detail);
        assert_eq!(view.issued_at_ms, proto.issued_at_ms);
        assert_eq!(view.phase, "READY");
        assert_eq!(proto.phase, 2); // proto enum Ready
    }

    /// The phase name mapping must match the proto enum names exactly
    /// (including `SHUTTING_DOWN` with its underscore).
    #[test]
    fn phase_name_maps_to_proto_names() {
        for (phase, expected) in [
            (BootstrapPhase::Booting, "BOOTING"),
            (BootstrapPhase::Ready, "READY"),
            (BootstrapPhase::Degraded, "DEGRADED"),
            (BootstrapPhase::Failed, "FAILED"),
            (BootstrapPhase::ShuttingDown, "SHUTTING_DOWN"),
            (BootstrapPhase::Unspecified, "UNSPECIFIED"),
        ] {
            assert_eq!(phase_name(phase), expected);
        }
    }
}
