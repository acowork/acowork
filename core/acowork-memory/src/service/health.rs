//! acowork-memory service health endpoint (ADR-101).
//!
//! Reuses the [`acowork_core::health::HealthResponse`] contract: the Gateway
//! supervisor probes `GET /health` to decide whether the memory process is
//! ready / alive (same as PM / doc / user / embed).

use std::path::PathBuf;

use axum::Json;
use serde_json::json;

/// Build the memory service `/health` route.
///
/// `data_dir` is captured into `details` so the supervisor log can confirm the
/// data-directory resolution (instance stores live under `{data_dir}/instances`).
pub fn health_route(data_dir: PathBuf) -> axum::Router {
    axum::Router::new().route(
        "/health",
        axum::routing::get(move || async move {
            Json(acowork_core::health::HealthResponse {
                status: "ok".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                process: "acowork-memory".to_string(),
                details: Some(json!({
                    "data_dir": data_dir.display().to_string(),
                })),
            })
        }),
    )
}

