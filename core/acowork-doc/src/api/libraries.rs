//! Library (top-level node) REST handlers.
//!
//! The library root is not a peer of the directories below it: **its
//! children are documents libraries**, and only below a library does the
//! plain directory tree resume. A library therefore carries metadata a
//! directory does not — icon, description, owner — plus the member ACL that
//! already exists on every node. Modelling it as "a root-level directory
//! with library metadata" keeps one storage format and one tree, while
//! giving the UI a first-class object with room to grow (permissions,
//! retention, defaults) without a second tree.
//!
//! Endpoints (design §4, gateway path `/api/doc/libraries`):
//!
//! | method | path | purpose |
//! |--------|------|---------|
//! | GET    | `/libraries`          | list visible libraries |
//! | POST   | `/libraries`          | create one (name/icon/description/members/owner) |
//! | PATCH  | `/libraries/{dir_id}` | rename / re-icon / re-describe / transfer |
//!
//! Deletion stays on `DELETE /dirs/{dir_id}` — a library is the same node
//! type, and a second delete path would be a second place to get trash and
//! ACL cleanup wrong.
//!
//! Create orchestration order is load-bearing: members are an ACL write,
//! which is gated on the node's **creator**, and the owner transfer
//! rewrites that creator. So it is create → members → owner, never in
//! another order (the project create dialog encodes the same rule).

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Deserializer, Serialize};

use crate::api::{actor_from, ApiError, ApiResult, ApiState};
use crate::api::dto::DirMetaDto;
use crate::path::{validate_dir_id, validate_library_description, validate_library_icon};
use crate::service::acl::AclTarget;
use crate::service::directory::{CreateDirectoryInput, DirectoryService, UpdateDirectoryMeta};
use crate::types::{AclRole, Ace, DirMeta, ROOT_DIR_ID};

// ── Request bodies ──────────────────────────────────────────────────────

/// `POST /libraries` body.
#[derive(Debug, Deserialize)]
pub struct CreateLibraryBody {
    pub name: String,
    /// Builtin icon id (`icon-07`) or any opaque UI key; `null` = default.
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Initial members (user ids). They receive `edit`; the owner implicitly
    /// keeps full access through the creator gate.
    #[serde(default)]
    pub members: Option<Vec<String>>,
    /// Owner after creation. Omitted / equal to the caller = no transfer.
    #[serde(default)]
    pub owner: Option<String>,
}

/// `PATCH /libraries/{dir_id}` body.
///
/// Double options carry three intents per field: key absent = leave alone,
/// `null` = clear, value = set. A plain `Option<String>` collapses the first
/// two and would make "remove the description" indistinguishable from
/// "don't touch it".
#[derive(Debug, Deserialize, Default)]
pub struct UpdateLibraryBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "some_option")]
    pub icon: Option<Option<String>>,
    #[serde(default, deserialize_with = "some_option")]
    pub description: Option<Option<String>>,
    /// Transfer ownership (rewrites the node's creator). Gated on the
    /// current owner / an admin, not on plain `edit`.
    #[serde(default)]
    pub owner: Option<String>,
}

/// Deserialize an explicit `null` as `Some(None)` instead of the default
/// `None`. Paired with `#[serde(default)]` this distinguishes "absent" from
/// "present and null".
fn some_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// `GET /libraries` response — the visible libraries only, no root entry.
#[derive(Debug, Serialize)]
pub struct LibraryListDto {
    pub libraries: Vec<DirMetaDto>,
}

// ── Handlers ────────────────────────────────────────────────────────────

/// `GET /libraries` — every root child the caller may `view`.
pub async fn list_libraries(
    state: ApiState,
    headers: HeaderMap,
) -> ApiResult<Json<LibraryListDto>> {
    let actor = actor_from(&headers);
    let tree = state
        .dirs
        .list_tree(ROOT_DIR_ID)
        .await
        .map_err(ApiError::from)?;
    let mut libraries = Vec::with_capacity(tree.dirs.len());
    for dir in tree.dirs {
        // Per-node check instead of `filter_tree`: the list is flat, so a
        // denied library is skipped rather than hidden inside a tree.
        if state
            .acl
            .check(&actor, AclTarget::Dir(&dir.dir_id), AclRole::View)
            .await
            .is_ok()
        {
            libraries.push(DirMetaDto::from(dir));
        }
    }
    Ok(Json(LibraryListDto { libraries }))
}

/// `POST /libraries` — create a library under the root.
pub async fn create_library(
    state: ApiState,
    headers: HeaderMap,
    Json(body): Json<CreateLibraryBody>,
) -> ApiResult<(StatusCode, Json<DirMetaDto>)> {
    let actor = actor_from(&headers);
    if let Some(icon) = body.icon.as_deref() {
        validate_library_icon(icon).map_err(ApiError::from)?;
    }
    if let Some(description) = body.description.as_deref() {
        validate_library_description(description).map_err(ApiError::from)?;
    }
    // Creating under the root is an edit on the root (design §9).
    state
        .acl
        .check(&actor, AclTarget::Dir(ROOT_DIR_ID), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;

    let meta: DirMeta = state
        .dirs
        .create(CreateDirectoryInput {
            parent_dir_id: ROOT_DIR_ID.to_string(),
            name: body.name,
            creator: actor.id.clone(),
            icon: body.icon,
            description: body.description,
        })
        .await
        .map_err(ApiError::from)?;

    // Members first: the ACL `set` gate trusts the creator, which is still
    // the caller at this point.
    let members = normalized_members(body.members.as_deref());
    if !members.is_empty() {
        state
            .acl
            .set(&actor, AclTarget::Dir(&meta.dir_id), Some(members))
            .await
            .map_err(ApiError::from)?;
    }

    // Owner transfer last: it rewrites the creator, i.e. the authority the
    // two calls above needed.
    let mut meta = meta;
    if let Some(owner) = body.owner.filter(|o| !o.trim().is_empty())
        && actor.id.as_deref() != Some(owner.as_str())
    {
        meta = state
            .dirs
            .update_meta(
                &meta.dir_id,
                UpdateDirectoryMeta {
                    owner: Some(owner),
                    ..Default::default()
                },
            )
            .await
            .map_err(ApiError::from)?;
    }

    Ok((StatusCode::CREATED, Json(DirMetaDto::from(meta))))
}

/// `PATCH /libraries/{dir_id}` — rename / icon / description / owner.
pub async fn update_library(
    state: ApiState,
    headers: HeaderMap,
    Path(dir_id): Path<String>,
    Json(body): Json<UpdateLibraryBody>,
) -> ApiResult<Json<DirMetaDto>> {
    validate_dir_id(&dir_id)?;
    if dir_id == ROOT_DIR_ID {
        return Err(ApiError::from(crate::error::DocError::BadRequest(
            String::from("the root is not a library"),
        )));
    }
    let actor = actor_from(&headers);
    if let Some(Some(icon)) = &body.icon {
        validate_library_icon(icon).map_err(ApiError::from)?;
    }
    if let Some(Some(description)) = &body.description {
        validate_library_description(description).map_err(ApiError::from)?;
    }

    // Content-level edits need `edit`; a rename is structural (same tier as
    // the `PATCH /dirs/{id}/name` route); the owner transfer is governance
    // and needs `manage`.
    if body.owner.is_some() {
        state
            .acl
            .check_manage(&actor, AclTarget::Dir(&dir_id))
            .await
            .map_err(ApiError::from)?;
    }
    state
        .acl
        .check(&actor, AclTarget::Dir(&dir_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;

    let meta = state
        .dirs
        .update_meta(
            &dir_id,
            UpdateDirectoryMeta {
                name: body.name,
                icon: body.icon,
                description: body.description,
                owner: body.owner,
            },
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(DirMetaDto::from(meta)))
}

/// Deduplicate a member list and drop blanks, keeping the first occurrence.
fn normalized_members(members: Option<&[String]>) -> Vec<Ace> {
    let mut out: Vec<Ace> = Vec::new();
    for principal in members.unwrap_or(&[]) {
        let principal = principal.trim();
        if principal.is_empty() {
            continue;
        }
        if out.iter().any(|a| a.principal == principal) {
            continue;
        }
        out.push(Ace {
            principal: principal.to_string(),
            role: AclRole::Edit,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_are_deduplicated_and_trimmed() {
        let members = vec!["u-a".to_string(), " u-a ".to_string(), "".to_string(), "u-b".to_string()];
        let acl = normalized_members(Some(&members));
        assert_eq!(acl.len(), 2);
        assert_eq!(acl[0].principal, "u-a");
        assert!(acl.iter().all(|a| a.role == AclRole::Edit));
    }
}