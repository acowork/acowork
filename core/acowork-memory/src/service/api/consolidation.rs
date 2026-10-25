//! Consolidation status + distill trigger routes (ADR-101 P1e).
//!
//! `GET /memory/consolidation/status` and `POST /memory/distill` are ports of the
//! Runtime handlers of the same name: same field names, same nesting, same
//! status codes and the same `{"error": …}` bodies, because the Desktop panel
//! parses them today and P3 removes the Runtime handlers without a frontend
//! change. The one deliberate addition is `distiller.models` in the GET response
//! (see below) and the `PUT` that goes with it.
//!
//! ## Per-instance instead of per-process
//!
//! The Runtime had one `ConsolidationTimer` per process and answered 503 while it
//! was still being built. This service has one timer per instance namespace,
//! created on demand, so there is no "not ready yet" state to report and the only
//! failure left is an unusable `instance_id` (400).
//!
//! ## Why `PUT` exists here and not in the Runtime
//!
//! The panel's "记忆蒸馏" / "遗忘" cards wrote to the agent's session config, and
//! the Runtime merged that into its scheduler at turn time. That layer goes away
//! with the rest of the Runtime memory code (P3), so the policy must be persisted
//! by the process that acts on it — hence `PUT /memory/consolidation/status`,
//! stored as `{instance_dir}/consolidation.json` and applied to the live timer
//! immediately. `models` was never readable from the Runtime's status response
//! (the cards read it out of the session config); now that the candidate chain is
//! resolved here, the GET echoes it back so a card can show what is in force.
//!
//! The PUT body takes a **patch**, not a full section: the two cards each own a
//! subset of the fields, and a whole-section replace would have one card
//! silently resetting the other's thresholds. Unknown keys are still a 400 — the
//! run statistics in the GET response are not accepted here, so a card must send
//! the fields it means to change.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use acowork_core::protocol::CompactModelRef;

use crate::service::api::{JsonErrorResult, err_json, resolve_instance_id};
use crate::service::error::{Result, ServiceError};
use crate::service::pipeline::{DistillerSettings, ForgettingSettings, InstanceSettings};
use crate::service::state::MemoryState;

/// Query parameters: the panel routes carry only `instance_id` here.
#[derive(Debug, Default, Deserialize)]
pub struct StatusQuery {
    #[serde(default)]
    pub instance_id: Option<String>,
}

/// `GET /memory/consolidation/status` — report one instance's scheduling state.
pub async fn status(
    State(state): State<MemoryState>,
    Query(params): Query<StatusQuery>,
) -> Result<Json<Value>> {
    let id = resolve_instance_id(params.instance_id.as_deref())?;
    Ok(Json(state.pipeline().status(&id).await?))
}

/// The `distiller` half of a `PUT` body. Absent field = keep the current value.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistillerPatch {
    pub enabled: Option<bool>,
    pub interval_secs: Option<u64>,
    pub accumulation_threshold: Option<usize>,
    pub idle_secs: Option<u64>,
    pub batch_size: Option<usize>,
    pub min_pending_age_hours: Option<u64>,
    /// Ordered candidate chain (layer 1 of [`crate::service::llm::resolve_candidates`]).
    pub models: Option<Vec<CompactModelRef>>,
}

/// The `forgetting` half of a `PUT` body. Absent field = keep current.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgettingPatch {
    pub enabled: Option<bool>,
    pub half_life_days: Option<u64>,
    pub dormant_threshold: Option<f32>,
    pub archive_days: Option<u64>,
    pub interval_secs: Option<u64>,
}

/// Body of `PUT /memory/consolidation/status`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusUpdate {
    #[serde(default)]
    pub distiller: DistillerPatch,
    #[serde(default)]
    pub forgetting: ForgettingPatch,
}

impl DistillerPatch {
    pub(crate) fn apply(&self, base: &mut DistillerSettings) {
        let Self {
            enabled,
            interval_secs,
            accumulation_threshold,
            idle_secs,
            batch_size,
            min_pending_age_hours,
            models,
        } = self;
        *base = DistillerSettings {
            enabled: enabled.unwrap_or(base.enabled),
            interval_secs: interval_secs.unwrap_or(base.interval_secs),
            accumulation_threshold: accumulation_threshold.unwrap_or(base.accumulation_threshold),
            idle_secs: idle_secs.unwrap_or(base.idle_secs),
            batch_size: batch_size.unwrap_or(base.batch_size),
            min_pending_age_hours: min_pending_age_hours.unwrap_or(base.min_pending_age_hours),
            models: models.clone().unwrap_or_else(|| base.models.clone()),
        };
    }
}

impl ForgettingPatch {
    fn apply(&self, base: &mut ForgettingSettings) {
        let Self {
            enabled,
            half_life_days,
            dormant_threshold,
            archive_days,
            interval_secs,
        } = self;
        *base = ForgettingSettings {
            enabled: enabled.unwrap_or(base.enabled),
            half_life_days: half_life_days.unwrap_or(base.half_life_days),
            dormant_threshold: dormant_threshold.unwrap_or(base.dormant_threshold),
            archive_days: archive_days.unwrap_or(base.archive_days),
            interval_secs: interval_secs.unwrap_or(base.interval_secs),
        };
    }
}

/// `PUT /memory/consolidation/status` — change one instance's policy.
///
/// Answers with the new status body, so the caller never needs a second
/// round-trip to confirm what is in force.
pub async fn update_status(
    State(state): State<MemoryState>,
    Query(params): Query<StatusQuery>,
    Json(body): Json<StatusUpdate>,
) -> Result<Json<Value>> {
    let id = resolve_instance_id(params.instance_id.as_deref())?;
    let pipeline = state.pipeline();
    let mut settings = pipeline.settings(&id);
    body.distiller.apply(&mut settings.distiller);
    body.forgetting.apply(&mut settings.forgetting);
    validate_settings(&settings)?;
    pipeline.save_settings(&id, &settings)?;
    Ok(Json(pipeline.status(&id).await?))
}

/// Reject a policy the pipeline could not act on.
///
/// A zero interval would make the gate fire on every poll (a full-table decay
/// scan per tick), and a zero batch size would scan nothing forever — both are
/// typos, not configurations.
pub(crate) fn validate_settings(settings: &InstanceSettings) -> Result<()> {
    if settings.distiller.interval_secs == 0 {
        return Err(ServiceError::BadRequest(
            "distiller.interval_secs must be >= 1".into(),
        ));
    }
    if settings.distiller.batch_size == 0 {
        return Err(ServiceError::BadRequest(
            "distiller.batch_size must be >= 1".into(),
        ));
    }
    if settings.forgetting.interval_secs == 0 {
        return Err(ServiceError::BadRequest(
            "forgetting.interval_secs must be >= 1".into(),
        ));
    }
    if !(0.0 < settings.forgetting.dormant_threshold
        && settings.forgetting.dormant_threshold <= 1.0)
    {
        return Err(ServiceError::BadRequest(
            "forgetting.dormant_threshold must be in (0.0, 1.0]".into(),
        ));
    }
    if settings
        .distiller
        .models
        .iter()
        .any(|m| m.model_id.trim().is_empty())
    {
        return Err(ServiceError::BadRequest(
            "distiller.models entries need a non-empty model_id".into(),
        ));
    }
    Ok(())
}

/// `POST /memory/distill` — force one EpisodicDistiller pass now.
///
/// Status codes are the Runtime's:
/// - `200` run completed (an empty backlog is still a completed run)
/// - `409` run not possible (memory / embedding provider unavailable, or the
///   distiller is disabled)
/// - `500` run failed
/// - `503` the run produced no result at all (`distiller_not_available`)
pub async fn distill(
    State(state): State<MemoryState>,
    Query(params): Query<StatusQuery>,
) -> JsonErrorResult<Json<Value>> {
    let id = resolve_instance_id(params.instance_id.as_deref())
        .map_err(|e| err_json(StatusCode::BAD_REQUEST, &e.to_string()))?;
    match state.pipeline().distill_once(&id).await {
        Ok(Some(result)) => Ok(Json(serde_json::json!({
            "started": true,
            "episodes_scanned": result.episodes_scanned,
            "facts_promoted": result.facts_promoted,
            "preferences_promoted": result.preferences_promoted,
            "relations_promoted": result.relations_promoted,
            "procedures_promoted": result.procedures_promoted,
            "autobio_promoted": result.autobio_promoted,
            "episodes_marked_consolidated": result.episodes_marked_consolidated,
            // The counters say what came out; the funnel says where the rest
            // went. A manual trigger is exactly when someone is watching.
            "funnel": result.funnel,
        }))),
        Ok(None) => Err(err_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "distiller_not_available",
        )),
        Err(e) => {
            tracing::warn!(instance_id = %id, error = %e, "Manual distiller run failed");
            if e.contains("memory provider not initialized")
                || e.contains("embedding provider not initialized")
                || e.contains("distiller is disabled")
            {
                Err(err_json(StatusCode::CONFLICT, &e))
            } else {
                Err(err_json(StatusCode::INTERNAL_SERVER_ERROR, &e))
            }
        }
    }
}

