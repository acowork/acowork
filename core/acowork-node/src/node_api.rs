//! Node identity HTTP service (ADR-075 D4).
//!
//! `PATCH /node/name` — change this node's **display** name
//! (`node_name`). `node_id` is a UUID routing key and NEVER changes, so
//! this touches no topic, no client_id, no installed inventory and
//! needs no daemon restart (unlike the pre-ADR-075 slug identity).
//!
//! Why HTTP and not MQTT: the rename is a user-initiated, permission-
//! gated write (ADR-087 Node-manage). The Gateway checks ownership on
//! `PATCH /api/nodes/{id}` and only then forwards here with the node
//! token — the same machine mutation path `fs_browse` already uses. The
//! offline equivalent stays the CLI (`acowork-node rename <name>`).
//!
//! Auth: `X-ACowork-Node-Token`, validated by the shared fail-closed
//! [`crate::proxy::authorize`] — one policy for every node-local route.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::patch,
};
use serde::{Deserialize, Serialize};

use acowork_core::mqtt_proto::{data_envelope, DataEnvelope};
use acowork_core::node::{node_info_topic, node_name_is_valid};

use crate::state::NodeHttpState;

/// Body of `PATCH /node/name`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameNodeBody {
    /// New `node_name` slug. Rejected with 400 when
    /// [`node_name_is_valid`] does not hold, so an invalid name never
    /// reaches `identity.json`.
    pub node_name: String,
}

#[derive(Debug, Serialize)]
struct RenameNodeResponse {
    node_name: String,
}

pub fn router(state: NodeHttpState) -> Router {
    Router::new()
        .route("/node/name", patch(rename_node))
        .with_state(state)
}

/// `PATCH /node/name` — `{ "nodeName": "gpu-2" }`.
async fn rename_node(
    State(state): State<NodeHttpState>,
    headers: HeaderMap,
    Json(body): Json<RenameNodeBody>,
) -> Response {
    // `own_node_id` is the auth-failure label only; this route is
    // node-scoped so there is no agent id in the path.
    if let Some(denied) = crate::proxy::authorize(&state, &headers, "node").await {
        return denied;
    }

    let new_name = body.node_name.trim().to_string();
    if !node_name_is_valid(&new_name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Invalid node name: must be 2-32 chars of [a-z0-9-], \
                          no consecutive '--', no leading/trailing hyphen, \
                          and must not be the reserved word 'local'",
            })),
        )
            .into_response();
    }

    // Persist BEFORE publishing: a node that renames in memory but dies
    // before the write leaves a heartbeat that resurrects the old name.
    let (node_id, identity, agent_count) = {
        let mut identity = state.identity.write().await;
        let old = identity.node_name.clone();
        if old == new_name {
            // Idempotent (ADR-075 D4): nothing to persist or publish.
            return Json(RenameNodeResponse {
                node_name: new_name,
            })
            .into_response();
        }
        identity.node_name = new_name.clone();
        if let Err(e) = identity.save(&state.config.home) {
            // Roll the in-memory value back so the next heartbeat cannot
            // overwrite `identity.json` with a name that was rejected.
            identity.node_name = old;
            tracing::warn!(error = %e, "Failed to persist node rename to identity.json");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        tracing::info!(node_id = %identity.node_id, node_name = %new_name, "Node display name renamed over HTTP");
        let snapshot = identity.clone();
        let count = state.node.read().await.agents.len() as u32;
        (snapshot.node_id.clone(), snapshot, count)
    };

    // Republish the retained info snapshot so the Gateway's NodeRegistry
    // converges immediately instead of on the next (60 s) heartbeat. The
    // daemon re-reads identity.json each heartbeat, so this is a
    // convergence shortcut, not the source of truth.
    let live_host = state
        .node
        .read()
        .await
        .live_advertise_host
        .lock()
        .map(|h| h.clone())
        .unwrap_or_default();
    let info = crate::control::build_node_info(&identity, &state.config, &live_host, agent_count);
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeInfo(info)),
    };
    if let Err(e) = crate::control::dispatcher::publish_envelope(
        node_info_topic(&node_id),
        envelope,
        true,
    )
    .await
    {
        // The name is already durable; the heartbeat will converge.
        tracing::warn!(node_id = %node_id, error = %e, "Failed to republish node info after rename");
    }

    Json(RenameNodeResponse { node_name: new_name }).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::config::NodeConfig;
    use crate::identity::{EnrollmentState, NodeIdentity};
    use crate::state::NodeState;

    const TEST_TOKEN: &str = "test-token";

    fn state(home: &std::path::Path) -> NodeHttpState {
        NodeHttpState {
            node: Arc::new(tokio::sync::RwLock::new(NodeState::new(8))),
            config: NodeConfig {
                home: home.to_path_buf(),
                ..NodeConfig::default()
            },
            identity: Arc::new(tokio::sync::RwLock::new(NodeIdentity {
                node_id: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
                node_name: "node-1".to_string(),
                node_token: Some(TEST_TOKEN.to_string()),
                gateway_addr: None,
                enrollment: EnrollmentState::Enrolled,
                created_at: chrono::Utc::now(),
                enrolled_at: None,
            })),
            // Not under test here: an empty registry over the fixture home and
            // a relay handle on an unused port.
            lsp: crate::lsp_registry::for_tests(home, None),
            lsp_relay: crate::state::LspRelayControl::detached("127.0.0.1", 1),
        }
    }

    fn patch(uri: &str, body: serde_json::Value, token: Option<&str>) -> axum::http::Request<axum::body::Body> {
        let mut b = axum::http::Request::builder()
            .uri(uri)
            .method("PATCH")
            .header("Content-Type", "application/json");
        if let Some(t) = token {
            b = b.header("X-ACowork-Node-Token", t);
        }
        b.body(axum::body::Body::from(body.to_string())).unwrap()
    }

    /// The rename is the whole point of the route: a valid slug must
    /// land in BOTH `identity.json` (durable — the daemon re-reads it
    /// every heartbeat) and the live identity the daemon publishes from.
    #[tokio::test]
    async fn rename_persists_and_updates_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let app = router(state.clone());

        let resp = app
            .clone()
            .oneshot(patch(
                "/node/name",
                serde_json::json!({ "nodeName": "gpu-2" }),
                Some(TEST_TOKEN),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        assert_eq!(state.identity.read().await.node_name, "gpu-2");
        // Durable: read back from disk, not just from memory.
        let disk = NodeIdentity::load(tmp.path()).unwrap().unwrap();
        assert_eq!(disk.node_name, "gpu-2");
        // node_id is the routing key — a rename must never touch it.
        assert_eq!(disk.node_id, state.identity.read().await.node_id);
    }

    /// An invalid slug must not reach identity.json, or a typo would
    /// poison the retained info the Gateway renders as the display name.
    #[tokio::test]
    async fn invalid_name_is_refused_and_nothing_is_written() {
        for bad in ["", "A", "has space", "double--hyphen", "trailing-", "local"] {
            let tmp = tempfile::tempdir().unwrap();
            let state = state(tmp.path());
            let resp = router(state.clone())
                .oneshot(patch(
                    "/node/name",
                    serde_json::json!({ "nodeName": bad }),
                    Some(TEST_TOKEN),
                ))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "expected 400 for {bad:?}"
            );
            assert_eq!(state.identity.read().await.node_name, "node-1");
            assert!(!NodeIdentity::path(tmp.path()).exists());
        }
    }

    /// Fail-closed, same as every other node-local route: the rename
    /// writes a machine-visible identity, so an unauthenticated caller
    /// must not reach it.
    #[tokio::test]
    async fn rename_refuses_a_wrong_or_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        for token in [None, Some("wrong")] {
            let resp = router(state.clone())
                .oneshot(patch(
                    "/node/name",
                    serde_json::json!({ "nodeName": "gpu-2" }),
                    token,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }
        assert_eq!(state.identity.read().await.node_name, "node-1");
    }
}
