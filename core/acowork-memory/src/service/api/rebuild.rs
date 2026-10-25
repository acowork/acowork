//! Embedding-index rebuild routes (ADR-101 P1d).
//!
//! `POST /memory/rebuild-embeddings` starts a **detached** re-embed and returns
//! `202` immediately; `GET /memory/rebuild-progress` is the polling half. A
//! synchronous rebuild would trip the Gateway's proxy timeout on a large store,
//! which is why the work lives in a background task and the state is a slot on
//! [`MemoryState`] rather than a response.
//!
//! The request body carries the embedding model the operator has *just*
//! switched to, so the migration embeds against that target rather than the
//! provider the service loaded at startup — see [`crate::service::embedding`].

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use acowork_core::EmbeddingProvider;

use crate::MemoryAdminService;
use crate::admin::RebuildScope;
use crate::service::api::{InstanceQuery, resolve_instance_id};
use crate::service::embedding::HttpEmbeddingClient;
use crate::service::error::{Result, ServiceError};
use crate::service::state::MemoryState;
use crate::wire::RebuildProgress;

/// Request body of `POST /memory/rebuild-embeddings`.
#[derive(Debug, Deserialize)]
pub struct RebuildBody {
    /// Embedding endpoint, e.g. `http://127.0.0.1:18080/v1`.
    #[serde(default)]
    pub endpoint: String,
    /// Embedding model id.
    #[serde(default)]
    pub model_id: String,
    /// Target embedding dimension.
    pub dimension: usize,
    /// Which rows to re-embed. Absent on the wire means `All` — the only scope
    /// that existed before the panel grew targeted repair.
    #[serde(default)]
    pub scope: RebuildScope,
}

/// `POST /memory/rebuild-embeddings` — start a background re-embed.
pub async fn rebuild_embeddings(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
    Json(body): Json<RebuildBody>,
) -> Result<(StatusCode, Json<Value>)> {
    let id = resolve_instance_id(query.as_deref())?;
    // Opening the store is part of accepting the request: a namespace whose
    // file cannot be opened must fail here (500) rather than leave a progress
    // slot that flips to an error after the client was told "started".
    let store = state.store(&id)?;

    tracing::info!(
        target: "migration_diag",
        instance_id = %id,
        endpoint = %body.endpoint,
        model_id = %body.model_id,
        dimension = body.dimension,
        ?body.scope,
        "rebuild_embeddings: received start request"
    );

    // Reserve the per-instance slot and reject a second concurrent rebuild of
    // the same store (409): two migrations over the same rows would double the
    // embedding cost and overwrite each other's progress numbers.
    state.begin_rebuild(&id)?;

    let task_state = state.clone();
    let endpoint = body.endpoint.clone();
    let model_id = body.model_id.clone();
    let dimension = body.dimension;
    let scope = body.scope;
    tokio::spawn(async move {
        let progress_state = task_state.clone();
        let progress_id = id.clone();
        let progress: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(move |rebuilt, total| {
            progress_state.report_rebuild(
                &progress_id,
                RebuildProgress {
                    rebuilt,
                    total,
                    done: false,
                    error: None,
                },
            );
        });

        let outcome = migrate(
            store,
            &endpoint,
            &model_id,
            dimension,
            scope,
            Some(progress),
        )
        .await;
        // The migration just proved this endpoint/model/width works against the
        // live store, so make it the service's provider: `model_dim` (what the
        // panel compares against `stored_dim`) then follows the model the
        // operator switched to immediately instead of waiting for the next
        // global-resources pull — otherwise a clean rebuild leaves the banner
        // up and the user clicks rebuild forever. Only on a fully clean run: a
        // rebuild with per-node errors used an endpoint that half-fails and
        // must not replace a working provider.
        if let Ok(stats) = &outcome
            && stats.errors == 0
            && let Ok(client) = HttpEmbeddingClient::new(
                &endpoint,
                None,
                &model_id,
                dimension,
                Duration::from_millis(task_state.config.embedding.timeout_ms.max(1)),
            )
        {
            task_state.set_embedding(Some(Arc::new(client)));
            task_state.instances().set_embedding_dim(dimension);
        }
        let (rebuilt, total, error) = match outcome {
            Ok(stats) => {
                tracing::info!(
                    target: "migration_diag",
                    instance_id = %id,
                    total_scanned = stats.total_scanned,
                    rebuilt = stats.rebuilt,
                    skipped_no_embedding = stats.skipped_no_embedding,
                    errors = stats.errors,
                    "rebuild_embeddings: completed"
                );
                (stats.rebuilt, stats.total_scanned, None)
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(
                    target: "migration_diag",
                    instance_id = %id,
                    error = %msg,
                    "rebuild_embeddings: background task failed"
                );
                (0, 0, Some(msg))
            }
        };
        task_state.report_rebuild(
            &id,
            RebuildProgress {
                rebuilt,
                total,
                done: true,
                error,
            },
        );
    });

    Ok((StatusCode::ACCEPTED, Json(json!({"status": "started"}))))
}

/// `GET /memory/rebuild-progress` — state of this instance's last rebuild.
///
/// A 404 means the instance has never run one *in this process* (the slot map
/// is in-memory, so a service restart clears it) — the same "no record" answer
/// the Runtime's empty slot produced. Deliberately does not open the store: a
/// polling loop must not create database files.
pub async fn rebuild_progress(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
) -> Result<Json<RebuildProgress>> {
    let id = resolve_instance_id(query.as_deref())?;
    match state.rebuild_progress(&id) {
        Some(p) => Ok(Json(p)),
        None => Err(ServiceError::RebuildNotFound(id)),
    }
}

/// Re-embed the rows named by `scope`, exactly as the Runtime's
/// `MemoryAdminAdapter::rebuild_embeddings_with_progress` did.
///
/// The migration is synchronous (`MemoryAdminService`) while embedding is not,
/// so it runs on a blocking thread and bridges each embed call through
/// `Handle::block_on`. A node whose embedding call fails is skipped with a
/// warning rather than aborting the whole rebuild — a partially healed store
/// beats no repair at all, and `errors` reports how many rows still need work.
async fn migrate(
    store: Arc<crate::sqlite::SqliteStore>,
    endpoint: &str,
    model_id: &str,
    dimension: usize,
    scope: RebuildScope,
    progress: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
) -> std::result::Result<crate::admin::RebuildStats, ServiceError> {
    let admin: Arc<dyn MemoryAdminService> = store;

    // No api key: this mirrors the Runtime, which built the migration provider
    // with `None`. The rebuild is driven by the local acowork-embed sidecar; a
    // cloud model needs credentials handed to the service, which is a P2 item
    // (the Gateway owns the global resource vault) and is not silently invented
    // here.
    let provider = Arc::new(
        HttpEmbeddingClient::new(
            endpoint,
            None,
            model_id,
            dimension,
            acowork_core::Timeouts::default().tool_http(),
        )
        .map_err(|e| ServiceError::Internal(format!("failed to build embedding provider: {e}")))?,
    );

    let endpoint_owned = endpoint.to_string();
    let model_owned = model_id.to_string();
    tokio::task::spawn_blocking(move || {
        let handle = tokio::runtime::Handle::current();
        let provider_for_fn = provider.clone();
        let embed_fn = move |text: &str| -> Option<Vec<f32>> {
            let text_owned = text.to_string();
            match handle.block_on(provider_for_fn.embed(&text_owned)) {
                Ok(vec) => Some(vec),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "rebuild: embedding failed, skipping node"
                    );
                    None
                }
            }
        };
        let progress_cb: Option<&dyn Fn(u64, u64)> =
            progress.as_ref().map(|cb| &**cb as &dyn Fn(u64, u64));
        admin
            .migrate_embedding_dimension_scoped(&embed_fn, dimension, scope, progress_cb)
            .map_err(|e| ServiceError::Internal(format!("embedding migration failed: {e}")))
    })
    .await
    .map_err(|e| {
        ServiceError::Internal(format!(
            "rebuild task panicked for {endpoint_owned}/{model_owned}: {e}"
        ))
    })?
}

