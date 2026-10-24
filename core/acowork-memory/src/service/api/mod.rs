//! Panel-facing REST API (ADR-100 P1d) — the Runtime `/memory/*` endpoints.
//!
//! These routes are a **port, not a redesign**. The Desktop memory panel and the
//! Gateway reverse proxy already speak this contract, so response shapes, field
//! names and status codes are kept byte-for-byte compatible with the handlers in
//! `acowork-runtime::http::server`; the phase that removes those handlers (P3)
//! must not need a frontend change. Where the Runtime had a single process per
//! Agent, the service has one process for every instance, so the same semantics
//! are expressed per namespace instead of per process (see [`instance_store`]).
//!
//! ## Identity
//!
//! Every route takes `?instance_id=`, injected by the Gateway `memory_proxy`
//! after it resolves the public `/api/agents/{id}/memory/*` path and validated
//! here as a directory name (ADR-100 decision 3). A missing or unusable id is a
//! 400 — the request never reaches a store, so it can never read another
//! Agent's memories. Unlike the MCP surface there is no 403 case: this API is
//! reachable only through the Gateway, which owns the authorization decision.
//!
//! ## Errors
//!
//! Most routes answer with a **bare status code and an empty body**, because
//! that is what `Result<Json<_>, StatusCode>` produced in the Runtime; the
//! detailed reason goes to the log ([`crate::service::error::ServiceError`]'s
//! `IntoResponse`). Two routes (`PUT /memory/nodes/{nid}`, and the distill
//! trigger in the pipeline step) return `{"error": …}` bodies, and they keep
//! doing exactly that — the panel reads those bodies.
//!
//! One route is not panel-facing: `/admin/bootstrap` is called by the Runtime on
//! an agent's first start ([`admin`]), and `/admin/import` by the Node's one-shot
//! migration pass ([`import_legacy`]). They share the router because they share
//! the instance-namespace rules, not because the panel can reach them.

pub mod admin;
pub mod consolidation;
pub mod import_legacy;
pub mod nodes;
pub mod rebuild;
pub mod search;

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use serde::Deserialize;

use crate::MemoryAdminService;
use crate::service::error::{Result, ServiceError};
use crate::service::state::MemoryState;
use crate::sqlite::SqliteStore;

/// Build the panel REST router, merged into
/// [`crate::service::server::MemoryService::router`] alongside the MCP routes.
pub fn rest_router(state: MemoryState) -> Router {
    Router::new()
        .route(
            "/memory/nodes",
            get(nodes::list_nodes).post(nodes::create_node),
        )
        .route(
            "/memory/nodes/{nid}",
            get(nodes::get_node)
                .put(nodes::update_node)
                .delete(nodes::delete_node),
        )
        .route("/memory/stats", get(nodes::stats))
        .route("/memory/graph", get(nodes::graph))
        .route(
            "/memory/consolidation/status",
            get(consolidation::status).put(consolidation::update_status),
        )
        .route("/memory/distill", post(consolidation::distill))
        .route(
            "/memory/rebuild-embeddings",
            post(rebuild::rebuild_embeddings),
        )
        .route("/memory/rebuild-progress", get(rebuild::rebuild_progress))
        .route("/memory/search", get(search::search))
        .route("/admin/bootstrap", post(admin::bootstrap))
        // The legacy-store upload is the only route that carries a file, so the
        // 2 MiB default is raised for this path alone — a per-route layer, not a
        // global one, so a runaway panel request still cannot buffer 64 MiB.
        .route(
            "/admin/import",
            post(import_legacy::import_legacy)
                .layer(DefaultBodyLimit::max(import_legacy::MAX_BODY_BYTES as usize)),
        )
        .with_state(state)
}

/// `?instance_id=` for the routes whose own parameters live in the path or the
/// body. Deserialized leniently so an absent id stays a 400 from
/// [`instance_store`] rather than a 400 from the extractor.
#[derive(Debug, Default, Deserialize)]
pub struct InstanceQuery {
    #[serde(default)]
    pub instance_id: Option<String>,
}

impl InstanceQuery {
    /// The raw id, if the caller sent one.
    pub fn as_deref(&self) -> Option<&str> {
        self.instance_id.as_deref()
    }
}

/// Resolve `?instance_id=` to a validated namespace key, without touching the
/// filesystem.
///
/// Separate from [`instance_store`] because a read-only poll (rebuild
/// progress) must not create a database file as a side effect of asking a
/// question.
pub(crate) fn resolve_instance_id(raw: Option<&str>) -> Result<String> {
    let id = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ServiceError::BadRequest("instance_id is required".into()))?;
    crate::service::state::validate_instance_id(id)?;
    Ok(id.to_string())
}

/// Resolve `?instance_id=` to the instance's open store.
///
/// Returns the validated id alongside the handle — the id is echoed in
/// responses (the graph's `agent_id`) and is the key of the rebuild-progress
/// map, so callers must not re-validate or, worse, re-derive it.
///
/// The Runtime answered 503 while its store slot was still being populated at
/// startup; a service request either resolves a namespace or fails outright
/// (400 unusable id, 500 store could not open), so no route here has a
/// "not ready yet" state to paper over.
pub(crate) fn instance_store(
    state: &MemoryState,
    raw: Option<&str>,
) -> Result<(String, Arc<SqliteStore>)> {
    let id = resolve_instance_id(raw)?;
    let store = state.store(&id)?;
    Ok((id, store))
}

/// The admin surface of one instance store.
///
/// `SqliteStore` implements [`MemoryAdminService`] (list / get / CRUD / stats /
/// embedding migration), which is what every route below drives — the panel
/// endpoints are thin protocol converters over it, exactly as they were in the
/// Runtime's `MemoryAdminAdapter`.
pub(crate) fn admin(store: Arc<SqliteStore>) -> Arc<dyn MemoryAdminService> {
    store
}

/// Embedding dimension the live provider produces; 0 when none is installed.
///
/// This is the `model_dim` the panel compares against `stored_dim` to decide
/// whether to show the "Rebuild Index" banner. Read from the provider rather
/// than cached beside the store, because a model switch changes it with no
/// notice to anyone holding a copy.
pub(crate) fn model_dim(state: &MemoryState) -> u64 {
    state.embedding().map(|p| p.dimension() as u64).unwrap_or(0)
}

/// Embed a query text for the semantic read paths.
///
/// `None` on an absent provider **or** a failed call, which is what makes the
/// engine fall back to BM25 instead of failing the request: a panel search that
/// returns lexical hits beats an error page.
pub(crate) async fn embed_query(state: &MemoryState, q: &str) -> Option<Vec<f32>> {
    let provider = state.embedding()?;
    match provider.embed(q).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "memory service: embedding failed, falling back to BM25");
            None
        }
    }
}

/// The `(StatusCode, Json)` error envelope the two body-carrying routes use.
pub(crate) type JsonError = (axum::http::StatusCode, axum::Json<serde_json::Value>);

/// Result alias for the routes that answer errors with a JSON body.
pub(crate) type JsonErrorResult<T> = std::result::Result<T, JsonError>;

/// `{"error": code}` body, as the Runtime's `err_json` helper produced it.
pub(crate) fn err_json(status: axum::http::StatusCode, code: &str) -> JsonError {
    (status, axum::Json(serde_json::json!({ "error": code })))
}
