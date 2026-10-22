//! Permissions REST handlers (design §9).
//!
//! `GET/PUT /api/permissions/{kind}/{id}` — read and replace the explicit
//! ACL of one library node (`kind` = `doc` | `dir`). Management itself is
//! gated on the node's creator or an admin (see `service::acl`); a GET
//! additionally requires `view` on the node, so ACLs cannot be probed on
//! nodes the caller cannot see.

use axum::extract::Path as AxumPath;
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{actor_from, ApiError, ApiResult, ApiState};
use crate::service::acl::{AclTarget, PermissionsView};
use crate::types::Ace;

/// `PUT /api/permissions/{kind}/{id}` body.
///
/// `acl: null` clears the explicit ACL (back to inheritance);
/// `acl: []` makes the node private (creator + admin only).
#[derive(Debug, Deserialize)]
pub struct SetAclBody {
    pub acl: Option<Vec<Ace>>,
}

/// The `{kind}` path segment, validated before touching the ACL engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum NodeKind {
    Doc,
    Dir,
}

impl NodeKind {
    fn parse(s: &str) -> Result<Self, ApiError> {
        match s {
            "doc" => Ok(NodeKind::Doc),
            "dir" => Ok(NodeKind::Dir),
            other => Err(ApiError(crate::error::DocError::BadRequest(format!(
                "unknown permission target kind: {other}"
            )))),
        }
    }
}

/// `GET /api/permissions/{kind}/{id}` — explicit + effective ACL and
/// whether the caller may manage this node.
pub async fn get_permissions(
    state: ApiState,
    headers: HeaderMap,
    AxumPath((kind, id)): AxumPath<(String, String)>,
) -> ApiResult<Json<PermissionsView>> {
    let kind = NodeKind::parse(&kind)?;
    validate_id(kind, &id)?;
    let actor = actor_from(&headers);
    let target = target_for(kind, &id);
    let view = state
        .acl
        .get(&actor, target)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(view))
}

/// `PUT /api/permissions/{kind}/{id}` — replace the node's explicit ACL.
pub async fn put_permissions(
    state: ApiState,
    headers: HeaderMap,
    AxumPath((kind, id)): AxumPath<(String, String)>,
    Json(body): Json<SetAclBody>,
) -> ApiResult<axum::http::StatusCode> {
    let kind = NodeKind::parse(&kind)?;
    validate_id(kind, &id)?;
    let actor = actor_from(&headers);
    let target = target_for(kind, &id);
    state
        .acl
        .set(&actor, target, body.acl)
        .await
        .map_err(ApiError::from)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

fn target_for(kind: NodeKind, id: &str) -> AclTarget<'_> {
    match kind {
        NodeKind::Doc => AclTarget::Doc(id),
        NodeKind::Dir => AclTarget::Dir(id),
    }
}

fn validate_id(kind: NodeKind, id: &str) -> Result<(), ApiError> {
    match kind {
        NodeKind::Doc => crate::path::validate_doc_id(id).map_err(ApiError),
        NodeKind::Dir => crate::path::validate_dir_id(id).map_err(ApiError),
    }
}
