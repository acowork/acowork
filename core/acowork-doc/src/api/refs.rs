//! Doc-reference validation (`POST /refs/validate`) — design 32 §3 / plan §7 G4.
//!
//! `pm_submit_task` gained a structured `doc_refs` field: an agent names its
//! deliverables as doc ids or library paths instead of burying them in free
//! text. A field that can silently point at nothing would be worse than the
//! free text it replaces — the desktop panel renders each ref as a link and a
//! dead link is indistinguishable from a deleted document — so the PM service
//! asks **this** endpoint (the doc side owns the fact) whether every ref
//! resolves, and refuses the submission when one does not.
//!
//! Contract:
//! - one result per input ref, in input order, always present;
//! - `found` carries the canonical `doc_id` and the human-readable path the
//!   panel links against;
//! - `not_found` covers both "no such document" and "no such directory chain"
//!   — the caller cannot tell the two apart, and the submission is refused
//!   either way;
//! - `denied` is deliberately a third state: a ref that exists but the caller
//!   may not view must not be reported as missing (that would let the PM
//!   surface leak existence information through a different error), and it
//!   must not be silently dropped (the submitter would believe a link landed);
//! - `invalid` is a malformed ref (no `.md` suffix, empty segment) — a
//!   client-side bug, not a library fact.
//!
//! The route is reached through the Gateway doc proxy as
//! `/api/doc/api/refs/validate`; internal service callers (PM) arrive as the
//! trusted `human` actor and bypass ACLs exactly like every other REST path
//! under local mode.

use axum::Json;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::api::{actor_from, ApiResult, ApiState};
use crate::error::DocError;
use crate::mcp::tools::resolve_doc_ref;
use crate::service::{DirectoryService, DocumentService};
use crate::state::DocState;
use crate::types::DocId;

/// Max refs per validation call. A submission names its deliverables, not an
/// export; the cap bounds the worst-case disk sweep (each ref costs at most
/// one directory walk + one listing).
const MAX_REFS: usize = 50;

#[derive(Debug, Deserialize)]
pub struct ValidateRefsBody {
    /// Doc ids (`doc-…`) or library paths (`研发/纪要.md`).
    pub refs: Vec<String>,
}

/// Per-ref outcome. `status` is the discriminator; the other fields are
/// present only for the states that have something to say.
#[derive(Debug, Serialize)]
pub struct RefOutcome {
    /// The ref as submitted (trimmed), so the caller can align results with
    /// its own inputs even if it sent them in bulk.
    pub r#ref: String,
    /// `found` | `not_found` | `denied` | `invalid`
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<DocId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ValidateRefsResponse {
    pub results: Vec<RefOutcome>,
    /// `true` when at least one ref did not resolve — the PM-side fast path
    /// for "refuse the submission" without re-reading every row.
    pub all_found: bool,
}

/// `POST /refs/validate` — resolve a batch of doc references.
pub async fn validate_refs(
    state: ApiState,
    headers: HeaderMap,
    Json(body): Json<ValidateRefsBody>,
) -> ApiResult<Json<ValidateRefsResponse>> {
    if body.refs.len() > MAX_REFS {
        return Err(crate::api::ApiError(DocError::BadRequest(format!(
            "too many refs: {} (max {MAX_REFS})",
            body.refs.len()
        ))));
    }
    let actor = actor_from(&headers);
    let mut results = Vec::with_capacity(body.refs.len());
    let mut all_found = true;
    for reference in body.refs {
        let outcome = validate_one(&state, &actor, reference).await;
        all_found &= outcome.status == "found";
        results.push(outcome);
    }
    Ok(Json(ValidateRefsResponse {
        results,
        all_found,
    }))
}

async fn validate_one(
    state: &DocState,
    actor: &crate::service::acl::ActorCtx,
    reference: String,
) -> RefOutcome {
    let trimmed = reference.trim().to_string();
    match resolve_doc_ref(state.dirs.as_ref(), state.docs.as_ref(), &trimmed).await {
        Ok((_dir_id, doc_id)) => {
            if !state.acl.can_view_doc(actor, &doc_id).await {
                return RefOutcome {
                    r#ref: trimmed,
                    status: "denied".into(),
                    doc_id: None,
                    path: None,
                    reason: Some("document exists but the caller may not view it".into()),
                };
            }
            let path = human_path(state, &doc_id).await.unwrap_or_default();
            RefOutcome {
                r#ref: trimmed,
                status: "found".into(),
                doc_id: Some(doc_id),
                path: Some(path),
                reason: None,
            }
        }
        // A missing directory segment and a missing document are the same
        // answer for the caller: the ref does not resolve.
        Err(DocError::DocNotFound(_)) | Err(DocError::DirNotFound(_)) => RefOutcome {
            r#ref: trimmed,
            status: "not_found".into(),
            doc_id: None,
            path: None,
            reason: None,
        },
        // Malformed ref (`""`, no `.md`, empty stem): say so distinctly —
        // silently calling it "not found" would send the agent looking in
        // the library for a document whose address was never valid.
        Err(DocError::BadRequest(m)) => RefOutcome {
            r#ref: trimmed,
            status: "invalid".into(),
            doc_id: None,
            path: None,
            reason: Some(m),
        },
        Err(e) => RefOutcome {
            r#ref: trimmed,
            status: "invalid".into(),
            doc_id: None,
            path: None,
            reason: Some(e.to_string()),
        },
    }
}

/// Human-readable path of a document (`研发/纪要.md`), rendered from the
/// same raw `{dir_id}/{name}.md` layout the MCP `PathCtx` translates. Kept
/// local to this endpoint: the MCP module owns its memoised variant because
/// it renders paths in bulk; a validation call resolves at most `MAX_REFS`
/// docs and one upward chain per doc is not a cost worth a cache.
async fn human_path(state: &DocState, doc_id: &str) -> Result<String, DocError> {
    use crate::types::ROOT_DIR_ID;
    let raw = state.docs.path_of(doc_id).await?;
    let (dir_id, file) = raw.rsplit_once('/').unwrap_or((ROOT_DIR_ID, raw.as_str()));
    let dir = state.dirs.path_of(dir_id).await?;
    Ok(if dir.is_empty() {
        file.to_string()
    } else {
        format!("{dir}/{file}")
    })
}
