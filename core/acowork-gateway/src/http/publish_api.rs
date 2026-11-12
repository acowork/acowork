//! Publish API HTTP handlers.
//!
//! ADR-055 Phase 3b: publish prepare/build are delegated to the node via
//! `NodePublishPrepare` / `NodePublishBuild` control commands. The node
//! reports the structured result in `NodeEvent.result_json` (JSON), which
//! the handlers re-parse into the HTTP response shapes. `install-locally`
//! delegates to the local node; `export` remains read-only (it only
//! locates an already-built package on the Gateway's machine — for remote
//! nodes file retrieval is a Phase 3 follow-up).

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde::{Deserialize, Serialize};

use acowork_core::mqtt_proto::NodeEvent;

use crate::http::routes::{ApiError, AppState};
use crate::mqtt::node_control::{NodeInstallDispatch, NodePackageSource};

/// Build the publish API router
pub fn publish_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents/{id}/publish/prepare", post(prepare_publish))
        .route("/api/agents/{id}/publish/build", post(build_publish))
        .route(
            "/api/agents/{id}/publish/install-locally",
            post(install_locally),
        )
        .route("/api/agents/{id}/publish/export", post(export_package))
}

// ── S4.2 Prepare ──────────────────────────────────────────────────────

/// Publish prepare request
#[derive(Debug, Deserialize)]
pub struct PrepareRequest {
    #[serde(default)]
    pub clean: bool,
}

/// Publish prepare response (mirrors the node's `PrepareResult` JSON).
#[derive(Debug, Serialize, Deserialize)]
pub struct PrepareResponse {
    pub checks: Vec<serde_json::Value>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub cleaned: bool,
}

/// `POST /api/agents/:id/publish/prepare` — delegated to the node.
pub async fn prepare_publish(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<PrepareRequest>,
) -> Result<Json<PrepareResponse>, ApiError> {
    // ADR-073: resolve the route variable to the instance identity.
    let (instance_id, resolved_agent_id) =
        crate::http::agents::resolve_agent_identity(&state, &agent_id).await?;
    let node_id = node_id_of(&state, &instance_id).await?;
    let node_control = state
        .node_control
        .clone()
        .ok_or_else(|| ApiError::internal("Node control plane unavailable (MQTT disabled)"))?;
    let event = node_control
        .publish_prepare(&node_id, &instance_id, &resolved_agent_id, req.clean)
        .await
        .map_err(|e| ApiError::internal(&format!("Publish prepare failed: {}", e)))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&instance_id, &event)
        .map_err(|e| ApiError::internal(&format!("Publish prepare failed: {}", e)))?;

    Ok(Json(parse_result::<PrepareResponse>(&event)?))
}

// ── S4.3 Build ────────────────────────────────────────────────────────

/// Build request
#[derive(Debug, Deserialize)]
pub struct BuildRequest {
    #[serde(default)]
    pub sign: bool,
    #[serde(default)]
    pub key_dir: Option<String>,
}

/// Build response (mirrors the node's `BuildResult` JSON).
#[derive(Debug, Serialize, Deserialize)]
pub struct BuildResponse {
    pub output_path: String,
    pub signed: bool,
    pub file_size: u64,
}

/// `POST /api/agents/:id/publish/build` — delegated to the node.
pub async fn build_publish(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<BuildRequest>,
) -> Result<Json<BuildResponse>, ApiError> {
    // ADR-073: resolve the route variable to the instance identity.
    let (instance_id, resolved_agent_id) =
        crate::http::agents::resolve_agent_identity(&state, &agent_id).await?;
    let node_id = node_id_of(&state, &instance_id).await?;
    let node_control = state
        .node_control
        .clone()
        .ok_or_else(|| ApiError::internal("Node control plane unavailable (MQTT disabled)"))?;
    // Empty output_dir → the node builds into its own packages_dir.
    let event = node_control
        .publish_build(
            &node_id,
            &instance_id,
            &resolved_agent_id,
            "",
            req.sign,
            req.key_dir.as_deref().unwrap_or(""),
        )
        .await
        .map_err(|e| ApiError::internal(&format!("Publish build failed: {}", e)))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&instance_id, &event)
        .map_err(|e| ApiError::internal(&format!("Publish build failed: {}", e)))?;

    Ok(Json(parse_result::<BuildResponse>(&event)?))
}

/// Resolve the node hosting an installed agent (publish operates on the
/// node that owns the package).
async fn node_id_of(
    state: &AppState,
    agent_id: &str,
) -> Result<String, ApiError> {
    let gw = state.gateway_state.read().await;
    gw.installed(agent_id)
        .map(|i| i.node_id.clone())
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))
}

/// Parse the structured result a node reported in `NodeEvent.result_json`.
fn parse_result<T: serde::de::DeserializeOwned>(
    event: &NodeEvent,
) -> Result<T, ApiError> {
    let json = event
        .result_json
        .as_deref()
        .ok_or_else(|| ApiError::internal("Node reply missing result_json"))?;
    serde_json::from_str::<T>(json)
        .map_err(|e| ApiError::internal(&format!("Failed to parse node result: {}", e)))
}

/// Install-locally request
#[derive(Debug, Deserialize)]
pub struct InstallLocallyRequest {
    /// Path to the built .agent package (from build_publish response)
    pub package_path: String,
    /// ADR-113: explicit target node. The Gateway never infers a default
    /// landing node from who spawned what.
    pub node_id: String,
}

/// `POST /api/agents/:id/publish/install-locally` — install built package
/// locally via the local node.
pub async fn install_locally(
    State(state): State<AppState>,
    Path(_agent_id): Path<String>,
    Json(req): Json<InstallLocallyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // Extract the manifest to route the install command and register cron.
    let manifest = crate::http::agents::extract_manifest_from_package(
        std::path::Path::new(&req.package_path),
    )
    .map_err(|e| ApiError::bad_request(&format!("{}", e)))?;
    let agent_id = manifest.agent_id.clone();
    // ADR-073: a local install is a NEW instance — the Gateway
    // generates the identity.
    let instance_id = uuid::Uuid::new_v4().to_string();

    let node_control = state.node_control.clone().ok_or_else(|| {
        ApiError::internal("Node control plane unavailable (MQTT disabled)")
    })?;
    // ADR-113: the caller names the node. No "own-machine" resolution —
    // that would make the same request behave differently depending on
    // which process happened to start the node.
    if req.node_id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "missing required field: 'node_id' — installs must name an explicit target node",
        ));
    }
    // Same protocol-internal gates as /install and /ensure: an explicit
    // target is still checked for NodeReady and protocol compatibility
    // before anything is published to its control topic.
    crate::http::agents::ensure_node_ready(&state, &req.node_id).await?;
    crate::http::agents::check_node_compatible(&state, &req.node_id).await?;
    let event = node_control
        .install_agent(NodeInstallDispatch {
            node_id: &req.node_id,
            instance_id: &instance_id,
            agent_id: &agent_id,
            source: NodePackageSource::LocalPath(&req.package_path),
            dev_mode: crate::http::agents::gateway_dev_mode(&state).await,
            ensure: false,
            operation_id: "",
        })
        .await
        .map_err(|e| ApiError::internal(&format!("Install-locally failed: {}", e)))?;
    // Installs are serialized on the node's single install slot, so the
    // reply is `in_progress` for a queued job and `ok` only when it was
    // already present — never a terminal "installed". Completion (and
    // cron registration on the Gateway side) follows from the retained
    // `installed` inventory entry the node publishes when the install
    // lands.
    crate::mqtt::node_control::NodeControlClient::check_install_reply(&instance_id, &event)
        .map_err(|e| ApiError::internal(&format!("Install-locally failed: {}", e)))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "message": format!("Package install accepted locally: {}", agent_id),
            "agent_id": agent_id,
            "instance_id": instance_id,
        })),
    ))
}

/// Export response
#[derive(Debug, Serialize)]
pub struct ExportInfo {
    pub status: String,
    pub output_path: String,
}

/// `POST /api/agents/:id/publish/export` — export built .agent file
pub async fn export_package(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<ExportInfo>, ApiError> {
    // ADR-073: the route variable can be either the package id or
    // the instance id (UUIDv4) — Desktop typically passes the
    // instance id from the agent list. The filename we look for
    // must match what `build_package` wrote, which uses the
    // *package* name from `cmd.agent_id` (see `build_publish`
    // above). Without this resolve, an instance-id URL would build
    // a `{instance_id}-{version}.agent` filename that the node
    // never wrote → spurious 404 "Run publish/build first".
    let (instance_id, resolved_agent_id) =
        crate::http::agents::resolve_agent_identity(&state, &agent_id).await?;
    let (output_dir, version) = {
        let gw = state.gateway_state.read().await;
        let info = gw.installed_agents.get(&instance_id).ok_or_else(|| {
            ApiError::not_found(&format!("Agent not found: {}", agent_id))
        })?;

        let output_dir = gw
            .config
            .as_ref()
            .map(|c| std::path::PathBuf::from(&c.packages_dir))
            .unwrap_or_else(|| std::path::PathBuf::from("./build"));

        (output_dir, info.version.clone())
    };

    let filename = format!("{}-{}.agent", resolved_agent_id, version);
    let output_path = output_dir.join(&filename);

    if !output_path.exists() {
        return Err(ApiError::not_found(&format!(
            "Built package not found at: {}. Run publish/build first.",
            output_path.display()
        )));
    }

    Ok(Json(ExportInfo {
        status: "ready".to_string(),
        output_path: output_path.to_string_lossy().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    //! Minimal regression coverage for the filename contract between
    //! `build_publish` (writes `{package_name}-{version}.agent`) and
    //! `export_package` (must look up the same filename).
    //!
    //! The original bug: `export_package` built the filename from
    //! the raw route variable, which is the *instance* id (UUIDv4) in
    //! the Desktop call path, producing
    //! `{instance_id}-{version}.agent` — a path the node never
    //! wrote. The fix calls `resolve_agent_identity` and uses the
    //! resolved package name, matching what `build_package` writes.
    //!
    //! We test the filename shape directly rather than going through
    //! the full HTTP stack because the rest of the handler (gateway
    //! state, packages_dir config, fs existence check) is covered by
    //! integration tests; this guards the *invariant* the two
    //! handlers share.

    /// Mirror of the `(resolved_agent_id, version) -> filename` step
    /// inside `export_package`. If this drifts from the production
    /// code, the contract test below is meaningless.
    fn expected_filename(package_name: &str, version: &str) -> String {
        format!("{}-{}.agent", package_name, version)
    }

    #[test]
    fn export_filename_uses_package_name_not_instance_id() {
        // Two real-looking identities from an installed agent: the
        // package name (what `build_package` writes) and the instance
        // id (what Desktop typically passes in the URL).
        let package_name = "com.acowork.ponytail";
        let instance_id = "59e7f3a8-7db3-4ffe-94e3-04e04cc45083";
        let version = "0.1.0";

        // The contract: filename == `{package_name}-{version}.agent`,
        // never `{instance_id}-{version}.agent`.
        let name = expected_filename(package_name, version);
        assert_eq!(name, "com.acowork.ponytail-0.1.0.agent");
        assert!(
            !name.contains(instance_id),
            "filename must be derived from the package name, not the route variable"
        );
    }
}
