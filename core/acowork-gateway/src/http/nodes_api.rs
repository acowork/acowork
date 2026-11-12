//! Node management API (ADR-055 §6.13.3 / Phase 3g).
//!
//! Exposes the Gateway's [`crate::mqtt::node_registry::NodeRegistry`]
//! (LWT-driven online state + retained `NodeInfo` metadata) over HTTP so
//! the Desktop "Node Management" page and the install node-picker can
//! render the node topology without talking MQTT directly. The registry
//! is the Gateway-side source of truth for "which nodes exist / are
//! online" (Phase 2a) — this endpoint is a read-only projection of it.
//!
//! Prior to this endpoint the only node view was the `acowork-gateway
//! nodes list` CLI, which drains retained topics straight from the broker
//! (no daemon state). The HTTP endpoint reads the *daemon's* in-memory
//! registry instead, so it reflects the live Gateway's view.

use axum::{
    extract::{Extension, Path, State},
    routing::{get, patch, post},
    Json, Router,
};

use serde::Serialize;

use crate::gateway::ownership::{self, Visibility};
use crate::http::auth_middleware::AuthContext;
use crate::http::permission;
use crate::http::routes::{ApiError, AppState};

/// Response for `GET /api/nodes` — a single Node Agent's live view.
///
/// Fields that depend on the retained `NodeInfo` snapshot (hostname, os,
/// arch, version, counts, endpoint) are `None` until the node publishes
/// its first info message; a node discovered only via the status topic
/// still appears with its `node_id` + `online` state.
///
/// Field names are snake_case: the payload is consumed verbatim by the
/// Desktop's `NodeInfo` type via `fetchNodes`
/// (`apps/acowork-desktop/src/lib/types.ts`). Do NOT add
/// `#[serde(rename_all = "camelCase")]` — an earlier revision did and
/// silently broke `node_id`/`agent_count` parsing (guarded by the
/// serialization regression test below).
#[derive(Debug, Serialize)]
pub struct NodeResponse {
    /// Logical node id (UUID v4 routing key, ADR-075 D1; the `"local"`
    /// literal only ever appears on Gateway-direct agent records).
    pub node_id: String,
    /// Whether the node is currently online (status topic / LWT).
    pub online: bool,
    /// UTC timestamp of the last online transition (RFC 3339, None while
    /// the node has never been observed online).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub online_since: Option<String>,
    /// Display name (slug, renameable, ADR-075 D2) from the info snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    /// ADR-113: the node's control plane has announced `NodeReady` and
    /// the `node.{id}` bootstrap subsystem is READY. This is the
    /// protocol-side "can accept an install command" signal — clients
    /// use it to build the set of usable targets instead of guessing
    /// from `online` alone. No deployment/launcher facts involved.
    ///
    /// Tri-state on the wire: `Some(true/false)` when a bootstrap registry
    /// is wired, and **absent** when the Gateway has no registry (MQTT
    /// disabled / minimal server). In that case `ensure_node_ready()` is
    /// fail-open, so reporting `false` would contradict the install gate
    /// and make every online node look unusable to clients. Absence means
    /// "unknown — let the Gateway's own gate decide", the same semantics
    /// a pre-ADR-113 Gateway's missing field has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_ready: Option<bool>,
    /// Node hostname (info snapshot).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Node OS (`std::env::consts::OS`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Node architecture (`std::env::consts::ARCH`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// Version of the acowork-node binary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_version: Option<String>,
    /// Node control-plane protocol version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
    /// Capability tags (grows over the ADR-055 phases).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Maximum concurrent Runtime processes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_agents: Option<u32>,
    /// Current running agent count (informational heartbeat).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_count: Option<u32>,
    /// This node's reverse-proxy base URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_endpoint: Option<String>,
    /// ADR-055 §6.7: this node's LSP relay base URL, from the retained
    /// `acowork/nodes/{node_id}/lsps` envelope. `Some` only while the
    /// node's relay is ready.
    ///
    /// The Desktop's harness LSP panel needs this to offer a per-node
    /// relay picker: without it the panel can only resolve a relay
    /// through `GET /api/agents/{id}/lsp-endpoint`, which is agent-scoped
    /// and therefore silently shows "the selected agent's node" with no
    /// way to inspect any other node (or to see which one it is looking
    /// at). Classified as machine-identifying metadata alongside
    /// `http_endpoint` under ADR-087 D5, so it is manage-list only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp_endpoint: Option<String>,
    /// ADR-087 D8: caller may manage this node (owner / guest / admin,
    /// or anyone in Local mode). Server-computed — the client renders
    /// from this boolean and never re-derives from owner/guest lists.
    pub can_manage: bool,
    /// ADR-087 D9: caller is on the manage list *as a guest* (not owner)
    /// — drives the "shared with you" badge; attribution edits stay
    /// owner/admin-only regardless.
    pub is_guest: bool,
    /// ADR-087 D2: `"private"` (default — machine metadata trimmed for
    /// non-manage callers) or `"public"`.
    pub visibility: String,
}

/// `GET /api/nodes` — list all known nodes (online + offline).
///
/// Reads the daemon's in-memory [`crate::mqtt::SharedNodeRegistry`],
/// sorted by node_id (stable ordering for the Desktop table).
pub async fn list_nodes(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
) -> Json<Vec<NodeResponse>> {
    let nodes = match state.node_registry.as_ref() {
        Some(registry) => registry.read().await.list_nodes(),
        // No node registry (MQTT disabled) — empty topology, not an error.
        None => Vec::new(),
    };

    let ctx = auth.as_ref().map(|e| &e.0);
    let resp = nodes
        .into_iter()
        .filter_map(|n| {
            let info = n.info.as_ref();
            let rec = node_owner_rec(&state, &n.node_id);
            // ADR-087 D5/B: a `private` node is not merely field-trimmed,
            // it is ABSENT from the list for callers outside its sharing
            // circle. `can_view` is the same predicate the agent list and
            // detail use, so the two resources cannot drift again.
            //
            // Why hide rather than trim: the fields a non-manager could
            // still read (node_name / online / agent_count) tell them a
            // machine exists and how busy it is, and the sidebar offers
            // no action they are allowed to take — install and every
            // other machine-touching route is Node-manage gated. Showing
            // an inert row leaks topology for zero benefit. Managers
            // (owner ∨ guest ∨ admin) keep the row, since the sidebar is
            // the only place a node can be acted on.
            // `ctx == None` means Local mode (ADR-087 D8: the whole
            // authorization layer is a no-op) or a machine actor with
            // no `AuthContext` (module invariant 4). Neither is subject
            // to visibility filtering — only an authenticated
            // multi-user caller is.
            if let Some(c) = ctx
                && !ownership::can_view(rec.as_ref(), &c.user_id, c.is_admin())
            {
                return None;
            }
            let can_manage = permission::caller_can_manage(ctx, rec.as_ref());
            let is_guest = ownership::is_guest(rec.as_ref(), ctx.map(|c| c.user_id.as_str()));
            // ADR-087 D5 table: machine-identifying metadata (hostname /
            // OS / arch / endpoint) is manage-list only. Local mode (no
            // ctx) sees everything. Operational fingerprints
            // (node_version / protocol_version / capabilities / max_agents
            // / agent_count) are pruned too — they identify the machine
            // and its capacity just as concretely as the hostname.
            let (hostname, os, arch, http_endpoint, node_version,
                 protocol_version, capabilities, max_agents, agent_count,
                 lsp_endpoint) =
                if can_manage {
                    (
                        info.map(|i| i.hostname.clone()),
                        info.map(|i| i.os.clone()),
                        info.map(|i| i.arch.clone()),
                        info.map(|i| i.http_endpoint.clone()),
                        info.map(|i| i.node_version.clone()),
                        info.map(|i| i.protocol_version),
                        info.map(|i| i.capabilities.clone()).unwrap_or_default(),
                        info.map(|i| i.max_agents),
                        info.map(|i| i.agent_count),
                        // Straight off the registry entry (not `info`) —
                        // the lsps topic is a separate envelope from the
                        // NodeInfo snapshot, and `n` is the live record.
                        n.lsp_endpoint.clone(),
                    )
                } else {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Vec::new(),
                        None,
                        None,
                        None,
                    )
                };
            // ADR-113: readiness is protocol state (bootstrap registry),
            // not a property of whoever launched the node. An online
            // node whose control plane has not announced NodeReady yet
            // is visible but not a valid install target.
            // `None` = no registry wired = readiness unknown. Must stay
            // absent on the wire: `ensure_node_ready()` is fail-open in
            // exactly this configuration, so a hard `false` here would
            // have the node list and the install gate disagree.
            let control_ready = state.bootstrap_registry.as_ref().map(|reg| {
                reg.is_ready(&crate::bootstrap::SubsystemId(format!("node.{}", n.node_id)))
            });
            Some(NodeResponse {
                node_id: n.node_id,
                online: n.online,
                online_since: n.online_since.map(|t| t.to_rfc3339()),
                node_name: n.node_name.clone(),
                control_ready,
                hostname,
                os,
                arch,
                node_version,
                protocol_version,
                capabilities,
                max_agents,
                agent_count,
                http_endpoint,
                lsp_endpoint,
                can_manage,
                is_guest,
                visibility: rec
                    .map(|r| r.visibility.as_str().to_string())
                    .unwrap_or_else(|| "private".into()),
            })
        })
        .collect();

    Json(resp)
}

// ---- ADR-087: node ownership endpoints -------------------------------
//
// The permission middleware gates the three attribution PATCHes as
// Node-transfer (owner / admin) before the handler runs; handlers
// validate input and mutate the row only.

/// Ownership row for a node, cloned out of the store (no guard crosses an
/// await). `None` = no record (ownerless -> admin-only, fail-closed).
pub(crate) fn node_owner_rec(
    state: &AppState,
    node_id: &str,
) -> Option<crate::gateway::ownership::OwnerRecord> {
    ownership::lock_shared(&state.node_owners)
        .as_ref()
        .and_then(|store| store.get(node_id).cloned())
}

/// Load a node's ownership row, apply `f`, persist. When the node has no
/// row yet (legacy nodes enrolled before ADR-087), a default ownerless
/// row is created and `f` applied — the D7 admin-claim path, symmetric
/// with the agent side. The middleware has already gated this to admin.
async fn edit_node_owner<F: FnOnce(&mut ownership::OwnerRecord)>(
    state: &AppState,
    id: &str,
    f: F,
) -> Result<(), ApiError> {
    // Existence gate (mirrors the agent side's `resolve_installed_key`):
    // never create an ownership row for a node id that was never seen —
    // otherwise arbitrary PATCHes would litter the store with ghost rows.
    let known = ownership::lock_shared(&state.node_owners)
        .map(|s| s.get(id).is_some())
        .unwrap_or(false);
    let known = known
        || match state.node_registry.as_ref() {
            Some(reg) => reg.read().await.get(id).is_some(),
            None => false,
        };
    if !known {
        return Err(ApiError::not_found(&format!("no such node: {id}")));
    }
    let mut store = ownership::lock_shared(&state.node_owners)
        .ok_or_else(|| ApiError::internal("ownership store unavailable"))?;
    store.upsert_with(id, || ownership::OwnerRecord::new(None), f);
    Ok(())
}

/// `POST /api/nodes/enrollment-tokens` — mint a one-time enrollment token
/// bound to the caller (ADR-087 D2). Body: optional `{ "ttl_seconds": n }`
/// (default 3600, capped at 24h). The plaintext is returned exactly once.
async fn create_enrollment_token(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    body: Option<Json<serde_json::Value>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ttl_secs = body
        .as_ref()
        .and_then(|Json(v)| v.get("ttl_seconds").and_then(|x| x.as_u64()))
        .unwrap_or(3600)
        .min(86_400);
    let store = state
        .enrollment_tokens
        .as_ref()
        .ok_or_else(|| ApiError::internal("enrollment token store unavailable"))?;
    let owner = auth.map(|e| e.0.user_id);
    let plaintext = store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .create_token(std::time::Duration::from_secs(ttl_secs), owner);
    Ok(Json(serde_json::json!({ "token": plaintext })))
}

/// `GET /api/nodes/{id}/permissions` — current attribution + guest list.
///
/// Read side of the three PATCH endpoints, gated at `NodeManage` by the
/// permission middleware (registered in `extract_target`). The Desktop
/// permission dialog renders from this; `can_edit` tells the caller
/// whether the PATCH endpoints will actually accept their write
/// (attribution ops additionally require owner ∨ admin — ADR-087 D9 R1).
async fn get_node_permissions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: Option<Extension<AuthContext>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = ownership::lock_shared(&state.node_owners)
        .ok_or_else(|| ApiError::internal("ownership store unavailable"))?;
    // No row = never attributed. In Local mode (no auth context) the
    // resource is implicitly manageable; in multi-user mode an
    // ownerless node is admin-only, so the dialog shows an empty state.
    let rec = store.get(&id);
    let local = auth.is_none();
    let user_id = auth.as_ref().map(|a| a.0.user_id.clone()).unwrap_or_default();
    let is_admin = auth.as_ref().is_some_and(|a| a.0.is_admin());
    let can_attribute = local
        || is_admin
        || rec.and_then(|r| r.owner_user_id.as_deref()) == Some(user_id.as_str());
    Ok(Json(serde_json::json!({
        "owner_user_id": rec.and_then(|r| r.owner_user_id.clone()),
        "guests": rec.map(|r| r.guests.clone()).unwrap_or_default(),
        "visibility": rec.map(|r| r.visibility.as_str()).unwrap_or("private"),
        "can_attribute": can_attribute,
        // ADR-087 D6/D8 — see `agents.rs::get_agent_permissions`.
        "can_set_visibility": ownership::can_publish(rec, local),
        "ownerless": ownership::is_ownerless(rec),
    })))
}

/// `PATCH /api/nodes/{id}` — `{ "node_name": "gpu-2" }`: change the
/// node's DISPLAY name (ADR-075 D4).
///
/// **Node-manage** — declared in [`permission::extract_target`] as
/// `("PATCH", "") => Target::Node(id)`, so the middleware has already
/// run the ADR-087 owner ∨ admin check by the time this handler sees the
/// request. Nothing here re-derives authority.
///
/// The write itself is forwarded to the node's own HTTP service
/// (`PATCH {http_endpoint}/node/name`, ADR-075 D4) with the node token
/// attached — the same machine-mutation path `/api/fs/browse` uses. It
/// cannot be done here: `node_name` lives in the node's `identity.json`,
/// and the daemon re-reads that file every heartbeat, so a Gateway-side
/// cache would be clobbered within 60 s. The node answers 200 only after
/// the name is durable on disk.
async fn patch_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let new_name = body
        .get("node_name")
        .or_else(|| body.get("nodeName"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .ok_or_else(|| ApiError::bad_request("missing `node_name`"))?
        .to_string();

    // Validate at the boundary so an obvious typo costs no network hop;
    // the node re-validates anyway (it owns the file being written).
    if !acowork_core::node::node_name_is_valid(&new_name) {
        return Err(ApiError::bad_request(
            "invalid node name: must be 2-32 chars of [a-z0-9-], no consecutive \
             '--', no leading/trailing hyphen, and not the reserved word 'local'",
        ));
    }

    let endpoint = crate::http::proxy::node_http_endpoint(&state, &id).await?;
    let token = crate::http::proxy::node_token_for(&state, &id)
        .await
        .ok_or_else(|| {
            ApiError::service_unavailable(&format!("Node '{id}' has no token yet"))
        })?;

    let client = crate::http::proxy::runtime_http_client();
    let resp = client
        .patch(format!("{endpoint}/node/name"))
        .header("X-ACowork-Node-Token", token)
        .json(&serde_json::json!({ "nodeName": new_name }))
        .send()
        .await
        .map_err(|e| {
            ApiError::service_unavailable(&format!("Failed to reach node '{id}': {e}"))
        })?;

    let status = resp.status();
    if !status.is_success() {
        // Pass the node's own reason through — it is the only side that
        // can say WHY (e.g. the file was not writable).
        let detail = resp.text().await.unwrap_or_default();
        return Err(ApiError::internal(&format!(
            "Node '{id}' refused the rename ({status}): {detail}"
        )));
    }

    tracing::info!(node_id = %id, node_name = %new_name, "Node display name renamed");
    Ok(Json(serde_json::json!({ "node_name": new_name })))
}

/// `PATCH /api/nodes/{id}/visibility` — `{ "visibility": "public"|"private" }`.
///
/// **Ownerless guard** — same rationale as the agent-side handler
/// (`agents.rs::patch_agent_visibility`): `upsert_with` normalises
/// `ownerless ⇒ not published` after the mutation, so publishing an
/// ownerless node would answer 200 while silently storing `private`,
/// and the Desktop switch would snap back with no error. Reject it with
/// a 409 that names the claim-first recovery instead.
async fn patch_node_visibility(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let vs = body
        .get("visibility")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::bad_request("missing `visibility`"))?;
    let vis = match vs {
        "public" => Visibility::Public,
        "private" => Visibility::Private,
        _ => {
            return Err(ApiError::bad_request(
                "visibility must be `public` or `private`",
            ))
        }
    };
    if vis == Visibility::Public {
        let store = ownership::lock_shared(&state.node_owners)
            .ok_or_else(|| ApiError::internal("ownership store unavailable"))?;
        if ownership::is_ownerless(store.get(&id)) {
            // Non-UI fallback only — the Desktop reads
            // `can_set_visibility` and disables the switch.
            return Err(ApiError::conflict(
                "this node has no owner yet, so it cannot be made public. \
                 Claim ownership first, then set visibility.",
            ));
        }
    }
    edit_node_owner(&state, &id, |rec| rec.visibility = vis).await?;
    Ok(Json(serde_json::json!({ "visibility": vs })))
}

/// `PATCH /api/nodes/{id}/guests` — `{ "guests": [user_id, ...] }`,
/// full-list replacement (ADR-087 D9: PUT-list semantics).
async fn patch_node_guests(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let list = body
        .get("guests")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ApiError::bad_request("missing `guests` array"))?;
    let guests: Vec<String> = list
        .iter()
        .filter_map(|v| v.as_str().map(|x| x.to_string()))
        .collect();
    if guests.len() != list.len() {
        return Err(ApiError::bad_request("guests must be user-id strings"));
    }
    edit_node_owner(&state, &id, |rec| rec.guests = guests.clone()).await?;
    Ok(Json(serde_json::json!({ "guests": guests })))
}

/// `PATCH /api/nodes/{id}/owner` — `{ "owner_user_id": "..." }` transfer
/// (owner / admin, enforced by the middleware). `null`/empty -> ownerless.
async fn patch_node_owner(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let new_owner = match body.get("owner_user_id") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) if s.is_empty() => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => {
            return Err(ApiError::bad_request(
                "missing `owner_user_id` (string or null)",
            ))
        }
    };
    edit_node_owner(&state, &id, |rec| rec.owner_user_id = new_owner.clone()).await?;
    Ok(Json(serde_json::json!({ "owner_user_id": new_owner })))
}

/// `POST /api/nodes/{id}/claim` — take ownership of an ownerless node
/// (ADR-087 D7, revised). The last-resort path for nodes that landed
/// ownerless (created before any admin account existed). Rules, enforced
/// here (the middleware declares `Tier::Claim` and passes through):
/// - only ownerless nodes are claimable — already-owned answers 409;
/// - admins may claim any ownerless node; any other logged-in user may
///   claim only the Gateway's own-machine node (the Desktop "first login
///   claims the local node" flow — no token ever surfaces);
/// - no `AuthContext` (local mode / machine actor): 400 — there is no
///   account system to attribute to.
async fn claim_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: Option<Extension<AuthContext>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ctx = auth
        .map(|e| e.0)
        .ok_or_else(|| ApiError::bad_request("claim requires a logged-in user"))?;
    // Existence gate (mirrors `edit_node_owner`): never create a row for
    // a node id that was never seen.
    let known = ownership::lock_shared(&state.node_owners)
        .map(|s| s.get(&id).is_some())
        .unwrap_or(false);
    let known = known
        || match state.node_registry.as_ref() {
            Some(reg) => reg.read().await.get(&id).is_some(),
            None => false,
        };
    if !known {
        return Err(ApiError::not_found(&format!("no such node: {id}")));
    }
    if !ctx.is_admin() {
        let local = match state.node_registry.as_ref() {
            Some(reg) => crate::mqtt::node_registry::local_node_id(reg).await,
            None => None,
        };
        if local.as_deref() != Some(id.as_str()) {
            return Err(ApiError::forbidden(
                "only admins may claim remote nodes; other users may claim the local node",
            ));
        }
    }
    let mut store = ownership::lock_shared(&state.node_owners)
        .ok_or_else(|| ApiError::internal("ownership store unavailable"))?;
    if store
        .get(&id)
        .and_then(|r| r.owner_user_id.as_ref())
        .is_some()
    {
        return Err(ApiError::conflict("node already has an owner"));
    }
    let owner = ctx.user_id.clone();
    store.upsert_with(&id, || ownership::OwnerRecord::new(Some(owner.clone())), |rec| {
        if rec.owner_user_id.is_none() {
            rec.owner_user_id = Some(owner.clone());
        }
    });
    tracing::info!(node_id = %id, owner = %ctx.user_id, "ADR-087: ownerless node claimed");
    Ok(Json(serde_json::json!({ "owner_user_id": ctx.user_id })))
}

/// Route definitions for the node management API.
pub fn nodes_routes() -> Router<AppState> {
    Router::new()
        .route("/api/nodes", get(list_nodes))
        .route("/api/nodes/enrollment-tokens", post(create_enrollment_token))
        .route("/api/nodes/{id}/permissions", get(get_node_permissions))
        .route("/api/nodes/{id}", patch(patch_node))
        .route("/api/nodes/{id}/visibility", patch(patch_node_visibility))
        .route("/api/nodes/{id}/guests", patch(patch_node_guests))
        .route("/api/nodes/{id}/owner", patch(patch_node_owner))
        .route("/api/nodes/{id}/claim", post(claim_node))
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;
    use super::*;
    use crate::http::routes::AppState;
    use crate::mqtt::node_registry::new_shared_registry;
    use acowork_core::mqtt_proto::NodeInfo;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-nodes-api-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let mut state = AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        state.node_registry = Some(new_shared_registry());
        state
    }

    fn info(node_id: &str) -> NodeInfo {
        NodeInfo {
            node_id: node_id.to_string(),
            hostname: "gpu-box".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            node_version: "0.1.0".to_string(),
            protocol_version: 1,
            capabilities: vec!["process".to_string(), "package".to_string()],
            max_agents: 16,
            agent_count: 2,
            http_endpoint: "http://10.0.0.2:19900".to_string(),
            node_name: "gpu-1".to_string(),
        }
    }

    #[tokio::test]
    async fn empty_registry_returns_empty_list() {
        let state = test_app_state();
        let resp = list_nodes(State(state), None).await;
        assert!(resp.0.is_empty());
    }

    #[tokio::test]
    async fn status_only_node_has_online_without_info() {
        let state = test_app_state();
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/local/status", b"online");
        }
        let resp = list_nodes(State(state), None).await;
        assert_eq!(resp.0.len(), 1);
        let node = &resp.0[0];
        assert_eq!(node.node_id, "local");
        assert!(node.online);
        assert!(node.online_since.is_some());
        assert!(node.hostname.is_none());
        assert!(node.agent_count.is_none());
    }

    #[tokio::test]
    async fn info_populates_metadata_fields() {
        let state = test_app_state();
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
            let envelope = acowork_core::mqtt_proto::DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::NodeInfo(
                    info("gpu-1"),
                )),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            reg.update_info_from_mqtt("acowork/nodes/gpu-1/info", &bytes);
        }
        let resp = list_nodes(State(state), None).await;
        assert_eq!(resp.0.len(), 1);
        let node = &resp.0[0];
        assert_eq!(node.hostname.as_deref(), Some("gpu-box"));
        assert_eq!(node.os.as_deref(), Some("linux"));
        assert_eq!(node.arch.as_deref(), Some("x86_64"));
        assert_eq!(node.node_version.as_deref(), Some("0.1.0"));
        assert_eq!(node.protocol_version, Some(1));
        assert_eq!(node.capabilities, vec!["process", "package"]);
        assert_eq!(node.max_agents, Some(16));
        assert_eq!(node.agent_count, Some(2));
        assert_eq!(node.http_endpoint.as_deref(), Some("http://10.0.0.2:19900"));
    }

    /// Feed a node the retained `lsps` envelope (ADR-055 §6.7).
    fn publish_lsps(reg: &mut crate::mqtt::node_registry::NodeRegistry, node_id: &str, endpoint: &str) {
        let lsps = acowork_core::mqtt_proto::AvailableLsps {
            version: 1,
            endpoint: endpoint.to_string(),
            ready: true,
        };
        let envelope = acowork_core::mqtt_proto::DataEnvelope {
            version: 1,
            payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AvailableLsps(
                lsps,
            )),
        };
        let bytes = prost::Message::encode_to_vec(&envelope);
        reg.update_lsps_from_mqtt(&format!("acowork/nodes/{}/lsps", node_id), &bytes);
    }

    #[tokio::test]
    async fn lsps_endpoint_surfaces_for_manager_and_is_pruned_otherwise() {
        // The harness LSP panel resolves a relay per node, so `/api/nodes`
        // must carry `lsp_endpoint`. It is machine-identifying metadata
        // (ADR-087 D5), so it follows `http_endpoint`: present for a
        // manager, absent for a merely-visible caller.
        let state = state_with_node_owner_pub("gpu-1", Some("alice"), &[], Visibility::Public);
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
            let envelope = acowork_core::mqtt_proto::DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::NodeInfo(
                    info("gpu-1"),
                )),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            reg.update_info_from_mqtt("acowork/nodes/gpu-1/info", &bytes);
            publish_lsps(&mut reg, "gpu-1", "http://10.0.0.2:19878");
        }

        let owner = Extension(AuthContext {
            user_id: "alice".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let resp = list_nodes(State(state.clone()), Some(owner)).await;
        assert_eq!(
            resp.0[0].lsp_endpoint.as_deref(),
            Some("http://10.0.0.2:19878"),
            "a manager must see the relay endpoint"
        );

        let stranger = Extension(AuthContext {
            user_id: "stranger".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let resp = list_nodes(State(state), Some(stranger)).await;
        assert_eq!(resp.0.len(), 1);
        assert!(
            resp.0[0].lsp_endpoint.is_none(),
            "a non-manager must not see the relay endpoint"
        );
    }

    #[tokio::test]
    async fn private_node_is_absent_for_non_manager() {
        // ADR-087 D5 (B): a `private` node is not field-trimmed for an
        // outsider — it is ABSENT from the list. A non-manager has no
        // action available on the row (install / fs browse / rename are
        // all Node-manage gated), so keeping it would leak topology for
        // no benefit.
        let state = state_with_node_owner_pub("gpu-1", Some("alice"), &[], Visibility::Private);
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
        }
        let ctx = Extension(AuthContext {
            user_id: "stranger".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let resp = list_nodes(State(state), Some(ctx)).await;
        assert!(
            resp.0.is_empty(),
            "private node must not be listed to a non-manager"
        );
    }

    #[tokio::test]
    async fn public_node_is_listed_but_not_manageable() {
        // `public` opens metadata to every logged-in account, and the
        // machine-identifying / capacity fields are STILL pruned —
        // visibility never implies manage (ADR-087 D6).
        let state = state_with_node_owner_pub("gpu-1", Some("alice"), &[], Visibility::Public);
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
            let envelope = acowork_core::mqtt_proto::DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::NodeInfo(
                    info("gpu-1"),
                )),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            reg.update_info_from_mqtt("acowork/nodes/gpu-1/info", &bytes);
        }
        let ctx = Extension(AuthContext {
            user_id: "stranger".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let resp = list_nodes(State(state), Some(ctx)).await;
        assert_eq!(resp.0.len(), 1);
        let node = &resp.0[0];
        assert_eq!(node.node_id, "gpu-1");
        assert!(node.online);
        assert!(!node.can_manage, "public never implies manage");
        assert_eq!(node.visibility, "public");
        assert!(node.hostname.is_none());
        assert!(node.os.is_none());
        assert!(node.arch.is_none());
        assert!(node.http_endpoint.is_none());
        assert!(node.agent_count.is_none());
    }

    #[tokio::test]
    async fn guest_and_admin_keep_seeing_a_private_node() {
        // A guest is use-tier: they must still *see* the private node
        // (can_view keys off can_use, which includes guests) — the
        // sidebar is the only place a granted resource is reachable —
        // but seeing is not managing: a guest holds no manage rights.
        async fn mark_online(state: &AppState) {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
        }

        let guest_state = state_with_node_owner_pub("gpu-1", Some("alice"), &["bob"], Visibility::Private);
        mark_online(&guest_state).await;
        let bob = Extension(AuthContext {
            user_id: "bob".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let resp = list_nodes(State(guest_state), Some(bob)).await;
        assert_eq!(resp.0.len(), 1, "guest must keep the private node visible");
        assert!(!resp.0[0].can_manage, "a guest is use-tier, not manage");
        assert!(resp.0[0].is_guest);

        // Admin sees ownerless rows too — otherwise an unclaimed node
        // could never be found and claimed (ADR-087 D7).
        let admin_state = state_with_node_owner_pub("gpu-1", None, &[], Visibility::Private);
        mark_online(&admin_state).await;
        let admin = Extension(AuthContext {
            user_id: "root".to_string(),
            role: acowork_core::account::Role::Admin,
            as_user: None,
        });
        let resp = list_nodes(State(admin_state), Some(admin)).await;
        assert_eq!(resp.0.len(), 1, "admin must see the ownerless node");
        assert!(resp.0[0].can_manage);
    }

    /// Seed a node-ownership row with an explicit visibility.
    fn state_with_node_owner_pub(
        node_id: &str,
        owner: Option<&str>,
        guests: &[&str],
        visibility: Visibility,
    ) -> AppState {
        let mut state = test_app_state();
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-node-vis-{}-{}",
            std::process::id(),
            node_id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = ownership::new_shared_node_owners(&dir);
        ownership::lock(&store).put(
            node_id,
            ownership::OwnerRecord {
                owner_user_id: owner.map(|s| s.to_string()),
                guests: guests.iter().map(|s| s.to_string()).collect(),
                visibility,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        state.node_owners = Some(store);
        state
    }

    /// `PATCH /api/nodes/{id}` (rename) is a **boundary** validation:
    /// the handler must reject a bad slug itself, so an obvious typo
    /// never becomes a network hop to the node — and, more importantly,
    /// so a rename can never be persisted with a name the node would
    /// refuse (the node is the only writer of `identity.json`).
    #[tokio::test]
    async fn patch_node_rejects_invalid_names_before_reaching_the_node() {
        for bad in ["", "A", "has space", "double--hyphen", "-lead", "trail-", "local"] {
            let state = test_app_state();
            let err = patch_node(
                State(state),
                Path("gpu-1".to_string()),
                Json(serde_json::json!({ "node_name": bad })),
            )
            .await
            .expect_err(&format!("expected rejection for {bad:?}"));
            assert_eq!(err.code, 400, "for {bad:?}");
        }
    }

    /// The node is the only writer of `identity.json`, so a rename that
    /// cannot reach it must fail loudly rather than answer 200 and let
    /// the UI show a name that will be overwritten by the next
    /// heartbeat. `node_http_endpoint` answers 503 for an offline node —
    /// that is the whole contract: rename needs the node online.
    #[tokio::test]
    async fn patch_node_requires_an_online_node() {
        let state = test_app_state();
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            // Known node (online creates the record, an offline status
            // alone does not — `update_status_from_mqtt` drops a stale
            // LWT replay), then flipped back offline.
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"offline");
        }
        let err = patch_node(
            State(state),
            Path("gpu-1".to_string()),
            Json(serde_json::json!({ "node_name": "gpu-2" })),
        )
        .await
        .expect_err("an offline node cannot be renamed");
        assert_eq!(err.code, 503);
    }

    #[tokio::test]
    async fn ownerless_node_is_absent_for_normal_user() {
        // No row / ownerless row ⇒ admin-only (ADR-087 D7). Before the
        // `can_view` fix this case leaked the node to everyone.
        let state = test_app_state();
        // node_owners is None here — no record for any node, which the
        // resolver reads as ownerless (admin-only, fail-closed).
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
            let envelope = acowork_core::mqtt_proto::DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::NodeInfo(
                    info("gpu-1"),
                )),
            };
            let bytes = prost::Message::encode_to_vec(&envelope);
            reg.update_info_from_mqtt("acowork/nodes/gpu-1/info", &bytes);
        }
        let ctx = Extension(AuthContext {
            user_id: "stranger".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let resp = list_nodes(State(state), Some(ctx)).await;
        assert!(
            resp.0.is_empty(),
            "ownerless node is admin-only and must not be listed (D7)"
        );
    }

    /// Wire a fresh node-ownership store into the test state and seed one row.
    fn state_with_node_owner(node_id: &str, owner: Option<&str>, guests: &[&str]) -> AppState {
        // mutable: ownership store is wired in after construction
        let mut state = test_app_state();
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-node-perms-{}-{}",
            std::process::id(),
            node_id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = ownership::new_shared_node_owners(&dir);
        ownership::lock(&store).put(
            node_id,
            ownership::OwnerRecord {
                owner_user_id: owner.map(|s| s.to_string()),
                guests: guests.iter().map(|s| s.to_string()).collect(),
                visibility: ownership::Visibility::Private,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        state.node_owners = Some(store);
        state
    }

    #[tokio::test]
    async fn node_permissions_owner_caller_gets_full_state_editable() {
        let state = state_with_node_owner("gpu-1", Some("alice"), &["bob"]);
        let ctx = Extension(AuthContext {
            user_id: "alice".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let Json(v) = get_node_permissions(State(state), Path("gpu-1".into()), Some(ctx)).await.unwrap();
        assert_eq!(v["owner_user_id"], serde_json::json!("alice"));
        assert_eq!(v["guests"], serde_json::json!(["bob"]));
        assert_eq!(v["visibility"], "private");
        assert_eq!(v["can_attribute"], true);
    }

    #[tokio::test]
    async fn node_permissions_guest_caller_is_read_only() {
        // ADR-087 D9 R1: a manage-guest may SEE the list but not change it.
        let state = state_with_node_owner("gpu-1", Some("alice"), &["bob"]);
        let ctx = Extension(AuthContext {
            user_id: "bob".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let Json(v) = get_node_permissions(State(state), Path("gpu-1".into()), Some(ctx)).await.unwrap();
        assert_eq!(v["can_attribute"], false);
    }

    #[tokio::test]
    async fn node_permissions_local_mode_no_row_is_editable() {
        // Local mode (no AuthContext) + never-attributed node → empty state,
        // editable (every resource is manageable in single-user mode).
        let state = state_with_node_owner("other", None, &[]);
        let Json(v) = get_node_permissions(State(state), Path("gpu-1".into()), None).await.unwrap();
        assert_eq!(v["owner_user_id"], serde_json::Value::Null);
        assert_eq!(v["guests"], serde_json::json!([]));
        assert_eq!(v["can_attribute"], true);
    }

    /// The node-side twin of `patch_agent_visibility_rejects_shared_on_
    /// ownerless_row`: publishing an ownerless node used to answer 200
    /// while `upsert_with` stored `private`, so the Desktop switch
    /// snapped back with no error. Must be a diagnosable 409.
    #[tokio::test]
    async fn patch_node_visibility_rejects_public_on_ownerless_row() {
        let state = state_with_node_owner_pub("gpu-1", None, &[], Visibility::Private);
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
        }
        let err = patch_node_visibility(
            State(state),
            Path("gpu-1".into()),
            Json(serde_json::json!({ "visibility": "public" })),
        )
        .await
        .expect_err("ownerless + public must be rejected, not silently downgraded");
        assert_eq!(err.code, 409, "the client must be able to branch on the status");
        assert!(
            err.error.contains("owner"),
            "the error must name the cause (claim an owner first): {}",
            err.error
        );
    }

    /// The claim-then-publish path the 409 points at must work.
    #[tokio::test]
    async fn node_visibility_public_persists_once_an_owner_is_claimed() {
        let state = state_with_node_owner_pub("gpu-1", None, &[], Visibility::Private);
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/gpu-1/status", b"online");
        }
        let _ = patch_node_owner(
            State(state.clone()),
            Path("gpu-1".into()),
            Json(serde_json::json!({ "owner_user_id": "alice" })),
        )
        .await
        .unwrap();
        let _ = patch_node_visibility(
            State(state.clone()),
            Path("gpu-1".into()),
            Json(serde_json::json!({ "visibility": "public" })),
        )
        .await
        .unwrap();
        let store = ownership::lock_shared(&state.node_owners).unwrap();
        let rec = store.get("gpu-1").unwrap();
        assert_eq!(rec.visibility, Visibility::Public);
        assert_eq!(rec.owner_user_id.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn nodes_are_sorted_by_id() {
        let state = test_app_state();
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/zeta/status", b"online");
            reg.update_status_from_mqtt("acowork/nodes/alpha/status", b"online");
        }
        let resp = list_nodes(State(state), None).await;
        let ids: Vec<&str> = resp.0.iter().map(|n| n.node_id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
    }

    /// `control_ready: None` = unknown, the state a Desktop must treat
    /// as "let the Gateway's install gate decide".
    fn sample_node_response() -> NodeResponse {
        NodeResponse {
            node_id: "nicholas-pc".to_string(),
            online: true,
            online_since: Some("2026-09-13T05:10:21+00:00".to_string()),
            node_name: Some("nicholas-pc".to_string()),
            control_ready: None,
            hostname: Some("NICHOLAS-PC".to_string()),
            os: Some("windows".to_string()),
            arch: Some("x86_64".to_string()),
            node_version: Some("0.1.0".to_string()),
            protocol_version: Some(1),
            capabilities: vec!["control_plane".to_string()],
            max_agents: Some(16),
            agent_count: Some(2),
            http_endpoint: Some("http://127.0.0.1:19900".to_string()),
            lsp_endpoint: Some("http://127.0.0.1:19878".to_string()),
            can_manage: true,
            is_guest: false,
            visibility: "private".to_string(),
        }
    }

    #[test]
    fn node_response_serializes_with_snake_case_fields() {
        // Contract: the Desktop `NodeInfo` type (snake_case) consumes this
        // payload verbatim via `fetchNodes`. A camelCase `rename_all` here
        // silently broke `node_id`/`agent_count` parsing — every node
        // failed to match its agent bucket, collapsing the remote-mode
        // sidebar into one gray "unknown node" group.
        let resp = NodeResponse {
            control_ready: Some(true),
            ..sample_node_response()
        };

        let json = serde_json::to_value(&resp).expect("NodeResponse serializes");
        let obj = json.as_object().expect("serializes to a JSON object");

        for key in [
            "node_id",
            "online",
            "online_since",
            "node_name",
            "control_ready",
            "hostname",
            "os",
            "arch",
            "node_version",
            "protocol_version",
            "capabilities",
            "max_agents",
            "agent_count",
            "http_endpoint",
            "lsp_endpoint",
        ] {
            assert!(obj.contains_key(key), "missing snake_case field `{key}`");
        }
        for key in [
            "nodeId",
            "onlineSince",
            "nodeName",
            "controlReady",
            "gatewayManaged",
            "gateway_managed",
            "machineUid",
            "nodeVersion",
            "protocolVersion",
            "maxAgents",
            "agentCount",
            "httpEndpoint",
        ] {
            assert!(!obj.contains_key(key), "unexpected camelCase field `{key}`");
        }
        assert_eq!(obj["node_id"], "nicholas-pc");
        assert_eq!(obj["agent_count"], 2);
        assert_eq!(obj["control_ready"], true);
    }

    #[test]
    fn unknown_control_ready_is_absent_not_false() {
        // ADR-113: `false` is a *decision* (the node's control plane is
        // not ready); absence is *no information*. Collapsing the two
        // would make every node on a registry-less Gateway unusable.
        let json = serde_json::to_value(sample_node_response()).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("control_ready"));
    }

    #[tokio::test]
    async fn list_nodes_marks_readiness_unknown_without_a_registry() {
        // `ensure_node_ready()` is fail-open when no bootstrap registry is
        // wired (MQTT disabled / minimal server). GET /api/nodes must not
        // contradict it by asserting `false` for the same nodes.
        let state = test_app_state();
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/alpha/status", b"online");
        }
        assert!(state.bootstrap_registry.is_none());

        let resp = list_nodes(State(state), None).await;
        assert_eq!(resp.0.len(), 1);
        assert_eq!(resp.0[0].control_ready, None);
    }

    #[tokio::test]
    async fn list_nodes_projects_node_ready_from_the_bootstrap_registry() {
        use crate::bootstrap::{ReadinessKind, SubsystemReadinessRegistry};

        let mut state = test_app_state();
        let registry = SubsystemReadinessRegistry::new_shared();
        registry
            .register("node.alpha", ReadinessKind::Optional)
            .mark_ready(None);
        state.bootstrap_registry = Some(registry);
        {
            let mut reg = state.node_registry.as_ref().unwrap().write().await;
            reg.update_status_from_mqtt("acowork/nodes/alpha/status", b"online");
            reg.update_status_from_mqtt("acowork/nodes/beta/status", b"online");
        }

        let resp = list_nodes(State(state), None).await;
        let by_id = |id: &str| {
            resp.0
                .iter()
                .find(|n| n.node_id == id)
                .unwrap_or_else(|| panic!("missing node {id}"))
                .control_ready
        };
        assert_eq!(by_id("alpha"), Some(true));
        assert_eq!(by_id("beta"), Some(false));
    }

    // ── ADR-087 (revised): POST /api/nodes/{id}/claim ─────────────────

    fn admin_ctx() -> Extension<AuthContext> {
        Extension(AuthContext {
            user_id: "admin-1".to_string(),
            role: acowork_core::account::Role::Admin,
            as_user: None,
        })
    }

    fn user_ctx(id: &str) -> Extension<AuthContext> {
        Extension(AuthContext {
            user_id: id.to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        })
    }

    async fn mark_online(state: &AppState, node_id: &str) {
        let mut reg = state.node_registry.as_ref().unwrap().write().await;
        reg.update_status_from_mqtt(
            &format!("acowork/nodes/{node_id}/status"),
            b"online",
        );
    }

    #[tokio::test]
    async fn admin_claims_ownerless_node() {
        let state = state_with_node_owner_pub(
            "gpu-1",
            None,
            &[],
            Visibility::Private,
        );
        mark_online(&state, "gpu-1").await;
        let resp = claim_node(
            State(state.clone()),
            Path("gpu-1".to_string()),
            Some(admin_ctx()),
        )
        .await
        .expect("admin claim of an ownerless node succeeds");
        assert_eq!(resp.0["owner_user_id"], "admin-1");
        let store = ownership::lock_shared(&state.node_owners).unwrap();
        assert_eq!(
            store.get("gpu-1").unwrap().owner_user_id.as_deref(),
            Some("admin-1")
        );
    }

    #[tokio::test]
    async fn claim_of_owned_node_conflicts() {
        let state = state_with_node_owner_pub(
            "gpu-1",
            Some("alice"),
            &[],
            Visibility::Private,
        );
        mark_online(&state, "gpu-1").await;
        let resp = claim_node(
            State(state.clone()),
            Path("gpu-1".to_string()),
            Some(admin_ctx()),
        )
        .await;
        assert!(resp.is_err(), "an owned node is not claimable");
        let store = ownership::lock_shared(&state.node_owners).unwrap();
        assert_eq!(
            store.get("gpu-1").unwrap().owner_user_id.as_deref(),
            Some("alice"),
            "a rejected claim must not touch the row"
        );
    }

    #[tokio::test]
    async fn normal_user_claims_local_node_only() {
        // The Desktop "first login claims the local node" flow: a plain
        // user may claim the own-machine node (anchor) but nothing else.
        let state = state_with_node_owner_pub(
            "local-1",
            None,
            &[],
            Visibility::Private,
        );
        mark_online(&state, "local-1").await;
        mark_online(&state, "remote-1").await;
        state
            .node_registry
            .as_ref()
            .unwrap()
            .write()
            .await
            .set_local_node_anchor("local-1");
        let resp = claim_node(
            State(state.clone()),
            Path("local-1".to_string()),
            Some(user_ctx("bob")),
        )
        .await
        .expect("a plain user may claim the local node");
        assert_eq!(resp.0["owner_user_id"], "bob");
        let err = claim_node(
            State(state.clone()),
            Path("remote-1".to_string()),
            Some(user_ctx("carol")),
        )
        .await
        .expect_err("remote ownerless nodes stay admin-only");
        let resp: axum::response::Response = err.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn claim_requires_logged_in_user() {
        // Local mode / machine actor: no AuthContext → 400 (there is no
        // account system to attribute to).
        let state = test_app_state();
        mark_online(&state, "gpu-1").await;
        let err = claim_node(State(state), Path("gpu-1".to_string()), None)
            .await
            .expect_err("claim without a user must fail");
        let resp: axum::response::Response = err.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }
}
