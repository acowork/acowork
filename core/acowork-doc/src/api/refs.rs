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
//! `/api/doc/refs/validate` (the Gateway strips its own `/api/doc` prefix
//! and forwards the rest verbatim — see
//! [`acowork_core::doc_layout::DOC_REFS_VALIDATE_ROUTE`]); internal
//! service callers (PM) arrive as the trusted `human` actor and bypass
//! ACLs exactly like every other REST path under local mode.

use axum::Json;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::api::{actor_from, ApiResult, ApiState};
use crate::error::DocError;
use crate::mcp::tools::{resolve_doc_ref, resolve_entry};
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

/// 一次调用能解析的 ref 上限。两条 ref 路由共用 —— 上限是传输/磁盘成本的策略，
/// 不是某一条路由的语义，所以它只有一处。
fn ensure_ref_budget(count: usize) -> ApiResult<()> {
    if count > MAX_REFS {
        return Err(crate::api::ApiError(DocError::BadRequest(format!(
            "too many refs: {count} (max {MAX_REFS})"
        ))));
    }
    Ok(())
}

/// `POST /refs/validate` — resolve a batch of doc references.
pub async fn validate_refs(
    state: ApiState,
    headers: HeaderMap,
    Json(body): Json<ValidateRefsBody>,
) -> ApiResult<Json<ValidateRefsResponse>> {
    ensure_ref_budget(body.refs.len())?;
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
            // A path that fails to render is reported as absent, not as
            // the empty string: a caller that stored `Some("")` as a link
            // target would hand the panel a dead anchor that looks valid.
            let path = human_path(state, &doc_id).await.ok();
            RefOutcome {
                r#ref: trimmed,
                status: "found".into(),
                doc_id: Some(doc_id),
                path,
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

// ── POST /refs/resolve — 地址解析（链接/打开侧）────────────────────────

/// Per-ref result of `/refs/resolve`: the entry the ref points at, or why it does not.
///
/// Deliberately a different shape from [`RefOutcome`]: a resolver has to hand the
/// caller enough to **open** the entry (`doc_id` + human path + `ext`), while a
/// validator only answers "does it land". Sharing one struct would push
/// always-null fields into PM's contract.
#[derive(Debug, Serialize)]
pub struct ResolveOutcome {
    pub r#ref: String,
    /// `found` | `not_found` | `denied` | `invalid`
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<DocId>,
    /// Human-readable library path (`研发/设计.pdf`) — never an internal `dir_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Original extension of an imported file; **always present** on `found`
    /// (`null` = a Markdown document). The opener dispatches on it, so an absent
    /// field and "markdown" must not be the same thing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ResolveRefsResponse {
    pub results: Vec<ResolveOutcome>,
    /// `true` when every ref landed — the caller's fast path for "nothing to
    /// toast". A single miss is not an error: the caller decides per row.
    pub all_found: bool,
}

/// `POST /refs/resolve` — resolve a batch of doc references to entries.
///
/// Same address grammar and the same implementation as `/refs/validate`
/// (`resolve_entry`), **without** PM's markdown gate: a link may point at any
/// entry the library holds (pdf / docx / xlsx / pptx / image). See the P0 section
/// of the doc-domain consolidation design.
pub async fn resolve_refs(
    state: ApiState,
    headers: HeaderMap,
    Json(body): Json<ValidateRefsBody>,
) -> ApiResult<Json<ResolveRefsResponse>> {
    ensure_ref_budget(body.refs.len())?;
    let actor = actor_from(&headers);
    let mut results = Vec::with_capacity(body.refs.len());
    let mut all_found = true;
    for reference in body.refs {
        let outcome = resolve_one(&state, &actor, reference).await;
        all_found &= outcome.status == "found";
        results.push(outcome);
    }
    Ok(Json(ResolveRefsResponse { results, all_found }))
}

async fn resolve_one(
    state: &DocState,
    actor: &crate::service::acl::ActorCtx,
    reference: String,
) -> ResolveOutcome {
    let trimmed = reference.trim().to_string();
    match resolve_entry(state.dirs.as_ref(), state.docs.as_ref(), &trimmed).await {
        Ok(entry) => {
            if !state.acl.can_view_doc(actor, &entry.meta.doc_id).await {
                // Existence is not disclosed to a caller that may not view it —
                // the same three-state answer `/refs/validate` gives.
                return ResolveOutcome {
                    r#ref: trimmed,
                    status: "denied".into(),
                    doc_id: None,
                    path: None,
                    name: None,
                    ext: None,
                    reason: Some("document exists but the caller may not view it".into()),
                };
            }
            let path = human_path(state, &entry.meta.doc_id).await.ok();
            ResolveOutcome {
                r#ref: trimmed,
                status: "found".into(),
                doc_id: Some(entry.meta.doc_id.clone()),
                path,
                name: Some(entry.meta.name.clone()),
                ext: Some(entry.meta.ext.clone()),
                reason: None,
            }
        }
        Err(DocError::DocNotFound(_)) | Err(DocError::DirNotFound(_)) => ResolveOutcome {
            r#ref: trimmed,
            status: "not_found".into(),
            doc_id: None,
            path: None,
            name: None,
            ext: None,
            reason: None,
        },
        Err(DocError::BadRequest(m)) => ResolveOutcome {
            r#ref: trimmed,
            status: "invalid".into(),
            doc_id: None,
            path: None,
            name: None,
            ext: None,
            reason: Some(m),
        },
        Err(e) => ResolveOutcome {
            r#ref: trimmed,
            status: "invalid".into(),
            doc_id: None,
            path: None,
            name: None,
            ext: None,
            reason: Some(e.to_string()),
        },
    }
}

/// 人读路径（`研发/纪要.md`）—— 交给 [`DocPathRenderer`]，与 REST / MCP 同源。
///
/// path 在 refs 的应答里是**展示字段**（打开靠 `doc_id`），所以祖先链断掉时
/// 降级成 `None` 而不是让整批失败：一条 ref 的展示路径不该拖垮其余条目。
async fn human_path(state: &DocState, doc_id: &str) -> Result<String, DocError> {
    crate::service::path_renderer::DocPathRenderer::new(state.dirs.as_ref())
        .doc_path(state.docs.as_ref(), doc_id)
        .await
}
