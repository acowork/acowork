//! Unified cache management — the Gateway's aggregate + forward surface
//! (ADR-123 §3).
//!
//! Two public endpoints, and no third job between them:
//!
//! | verb | path | what it does |
//! |---|---|---|
//! | `GET` | `/api/cache/report` | merges every participating service's `storage-report` + the volume watermark |
//! | `POST` | `/api/cache/cleanup` | forwards the clicked category ids to the service that owns them |
//!
//! ## What the Gateway is NOT allowed to do here
//!
//! ADR-123 §1.3, all four red lines:
//!
//! 1. It does not walk any service's data directory. Every byte figure in the
//!    report came from the owner over HTTP. The one local scan is the Gateway's
//!    own `data_dir` (row `gateway`) — plus the volume-level watermark, which
//!    reads no directory at all.
//! 2. It never offers a delete for a data-class row (the owners mark those
//!    `cleanable: false`, and the forwarder refuses them anyway).
//! 3. It carries no "how much" semantics: [`CleanupBody::ids`] names categories
//!    and nothing else; the retention policy lives in the owner.
//! 4. It takes no business action of its own — it aggregates, forwards, and
//!    alarms (the alarm is `crate::storage_watch`).
//!
//! ## Which services participate
//!
//! Only services that own **cleanable** bytes: `gateway` (own logs), `user`
//! (aborted chunks + past-TTL transfers), `embed` (downloaded ONNX models) and
//! `node:<id>` (node logs + the Runtimes it hosts). memory / pm / doc are
//! deliberately absent — they hold nothing a regenerating sweep may reclaim, so
//! a report endpoint for them would be an interface with no possible action
//! behind it (they still enforce their own data rules; ADR-123 D4 keeps that
//! data out of every delete path).
//!
//! A service that is *not running* still gets a row (`reachable: false`), so the
//! card distinguishes "0 bytes" from "nothing answered".
//!
//! ## Authorization
//!
//! Bearer-gated like every other `/api/*` route; no ownership tier applies
//! (`extract_target` → `Target::None`, the same bucket as `/api/services/*`).
//! The blast radius is what ADR-123 allows it to be: a caller can read disk
//! sizes and can ask a service to run the retention sweep it was already going
//! to run — never delete a data-class byte.

use std::time::Duration;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

use acowork_core::storage::{
    CleanupRequest, CleanupResult, LOG_RETENTION_SECS, StorageItem, StorageKind, StorageReport,
    dir_bytes, purge_files_older_than,
};

use crate::config::GatewayConfig;
use crate::http::proxy::{node_http_endpoint, node_token_for, runtime_http_client};
use crate::http::routes::{ApiError, AppState};
use crate::storage_watch::sample_watermark;

/// Per-service budget for one report fetch. A wedged service must not hang the
/// card: its row comes back `reachable: false` and the user still sees the rest.
const SERVICE_TIMEOUT: Duration = Duration::from_secs(3);

/// Row key of the Gateway's own storage (the one place a local scan is legal).
const SERVICE_GATEWAY: &str = "gateway";
/// Prefix for the per-node rows; the suffix is the node id.
const SERVICE_NODE_PREFIX: &str = "node:";

/// One service (or one node) in the aggregated report.
#[derive(Debug, Serialize)]
pub struct ServiceStorageRow {
    /// Routing key, echoed back verbatim by the cleanup call.
    pub service: String,
    /// Set only for `node:<id>` rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// False = no answer (process not running / node offline / request failed).
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub items: Vec<StorageItem>,
}

/// `GET /api/cache/report` response.
#[derive(Debug, Serialize)]
pub struct CacheReportResponse {
    /// The volume-level watermark this Gateway is alarming on (ADR-123 D5/D7).
    pub disk: acowork_core::storage::DiskWatermark,
    pub services: Vec<ServiceStorageRow>,
}

/// `POST /api/cache/cleanup` body.
#[derive(Debug, Deserialize)]
pub struct CleanupBody {
    /// The `service` value from the report — the routing key, nothing else.
    pub service: String,
    /// Category ids to clean. Empty is rejected: a no-op "success" would show
    /// the user a freed-bytes number over untouched files.
    #[serde(default)]
    pub ids: Vec<String>,
}

pub fn cache_routes() -> Router<AppState> {
    Router::new()
        .route("/api/cache/report", get(cache_report))
        .route("/api/cache/cleanup", post(cache_cleanup))
}

/// `GET /api/cache/report`.
async fn cache_report(State(state): State<AppState>) -> Json<CacheReportResponse> {
    let (config, services) = supervised_services(&state).await;
    let items = match config.as_ref() {
        Some(cfg) => gateway_self_items(cfg).await,
        None => Vec::new(),
    };

    let mut rows = vec![ServiceStorageRow {
        service: SERVICE_GATEWAY.to_string(),
        node_id: None,
        reachable: config.is_some(),
        error: config
            .is_none()
            .then(|| "gateway config unavailable".to_string()),
        items,
    }];

    // Concurrent fan-out: every row has its own `SERVICE_TIMEOUT`, so a
    // sequential walk would cost N×3 s on a bad day (one wedged service, or
    // several nodes offline behind a slow relay). `join_all` keeps the worst
    // case at one timeout regardless of how many rows answer late.
    let node_ids = online_nodes(&state).await;
    rows.extend(join_all(services.iter().map(fetch_row)).await);
    rows.extend(join_all(node_ids.iter().map(|id| fetch_node_row(&state, id))).await);

    let probe = config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let threshold = config.as_ref().map(|c| c.cache.disk_alarm_percent);
    let disk = sample_watermark(&probe, threshold.unwrap_or(90.0));

    Json(CacheReportResponse { disk, services: rows })
}

/// `POST /api/cache/cleanup` — forward, then report what the owner released.
async fn cache_cleanup(
    State(state): State<AppState>,
    Json(body): Json<CleanupBody>,
) -> Result<Json<CleanupResult>, ApiError> {
    if body.ids.is_empty() {
        return Err(ApiError::bad_request("cleanup needs at least one category id"));
    }
    let ids = CleanupRequest {
        ids: body.ids.clone(),
    };

    if body.service == SERVICE_GATEWAY {
        return Ok(Json(gateway_self_cleanup(&state, &ids).await?));
    }
    if let Some(node_id) = body.service.strip_prefix(SERVICE_NODE_PREFIX) {
        return Ok(Json(forward_node_cleanup(&state, node_id, &ids).await?));
    }

    let (_, services) = supervised_services(&state).await;
    let Some(target) = services.iter().find(|t| t.service == body.service) else {
        return Err(ApiError::bad_request(&format!(
            "unknown or unavailable service '{}'",
            body.service
        )));
    };
    let url = format!("{}{}", target.base, target.cleanup_path);
    let resp = runtime_http_client()
        .post(&url)
        .timeout(SERVICE_TIMEOUT)
        .json(&ids)
        .send()
        .await
        .map_err(|e| {
            ApiError::service_unavailable(&format!("{}: storage cleanup unreachable: {e}", target.service))
        })?;
    if !resp.status().is_success() {
        return Err(ApiError::internal(&format!(
            "{}: storage cleanup answered {}",
            target.service,
            resp.status()
        )));
    }
    let result = resp.json::<CleanupResult>().await.map_err(|e| {
        ApiError::internal(&format!("{}: unreadable cleanup answer: {e}", target.service))
    })?;
    tracing::info!(
        service = %target.service,
        freed_bytes = result.freed_bytes,
        ids = ?ids.ids,
        "cache cleanup forwarded"
    );
    Ok(Json(result))
}

/// A service the Gateway supervises and can ask for storage verbs.
struct ServiceTarget {
    service: &'static str,
    /// `http://127.0.0.1:{port}`
    base: String,
    report_path: &'static str,
    cleanup_path: &'static str,
}

/// Every ready supervised service that owns cleanable bytes, plus the Gateway
/// config (cloned out from under the state lock).
async fn supervised_services(state: &AppState) -> (Option<GatewayConfig>, Vec<ServiceTarget>) {
    let gw = state.gateway_state.read().await;
    let config = gw.config.clone();
    let mut targets = Vec::new();
    let mut push = |service: &'static str, port: Option<u16>, report: &'static str, cleanup: &'static str| {
        if let Some(port) = port {
            targets.push(ServiceTarget {
                service,
                base: format!("http://127.0.0.1:{port}"),
                report_path: report,
                cleanup_path: cleanup,
            });
        }
    };
    push(
        "user",
        gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port),
        "/internal/storage-report",
        "/internal/storage-cleanup",
    );
    push(
        "embed",
        gw.embed_process.as_ref().filter(|p| p.ready).map(|p| p.port),
        "/storage/report",
        "/storage/cleanup",
    );
    (config, targets)
}

/// Node ids the registry holds as online.
async fn online_nodes(state: &AppState) -> Vec<String> {
    let Some(registry) = state.node_registry.as_ref() else {
        return Vec::new();
    };
    registry
        .read()
        .await
        .list_nodes()
        .into_iter()
        .filter(|n| n.online)
        .map(|n| n.node_id)
        .collect()
}

async fn fetch_row(target: &ServiceTarget) -> ServiceStorageRow {
    let url = format!("{}{}", target.base, target.report_path);
    let row = |reachable, error, items| ServiceStorageRow {
        service: target.service.to_string(),
        node_id: None,
        reachable,
        error,
        items,
    };
    match runtime_http_client()
        .get(&url)
        .timeout(SERVICE_TIMEOUT)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.json::<StorageReport>().await {
            Ok(report) => row(true, None, report.items),
            Err(e) => row(false, Some(format!("unreadable report: {e}")), Vec::new()),
        },
        Ok(resp) => row(false, Some(format!("answered {}", resp.status())), Vec::new()),
        Err(e) => row(false, Some(format!("unreachable: {e}")), Vec::new()),
    }
}

async fn fetch_node_row(state: &AppState, node_id: &str) -> ServiceStorageRow {
    let service = format!("{SERVICE_NODE_PREFIX}{node_id}");
    let row = |reachable, error, items| ServiceStorageRow {
        service: service.clone(),
        node_id: Some(node_id.to_string()),
        reachable,
        error,
        items,
    };

    let (endpoint, token) = match node_endpoint_and_token(state, node_id).await {
        Ok(pair) => pair,
        Err(e) => return row(false, Some(e), Vec::new()),
    };
    match runtime_http_client()
        .get(format!("{endpoint}/storage/report"))
        .header(acowork_core::auth::NODE_TOKEN_HEADER, token)
        .timeout(SERVICE_TIMEOUT)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.json::<StorageReport>().await {
            Ok(report) => row(true, None, report.items),
            Err(e) => row(false, Some(format!("unreadable report: {e}")), Vec::new()),
        },
        Ok(resp) => row(false, Some(format!("answered {}", resp.status())), Vec::new()),
        Err(e) => row(false, Some(format!("unreachable: {e}")), Vec::new()),
    }
}

async fn forward_node_cleanup(
    state: &AppState,
    node_id: &str,
    req: &CleanupRequest,
) -> Result<CleanupResult, ApiError> {
    let (endpoint, token) = node_endpoint_and_token(state, node_id)
        .await
        .map_err(|e| ApiError::service_unavailable(&e))?;
    let resp = runtime_http_client()
        .post(format!("{endpoint}/storage/cleanup"))
        .header(acowork_core::auth::NODE_TOKEN_HEADER, token)
        .timeout(SERVICE_TIMEOUT)
        .json(req)
        .send()
        .await
        .map_err(|e| ApiError::service_unavailable(&format!("node '{node_id}' unreachable: {e}")))?;
    if !resp.status().is_success() {
        return Err(ApiError::internal(&format!(
            "node '{node_id}' cleanup answered {}",
            resp.status()
        )));
    }
    resp.json::<CleanupResult>()
        .await
        .map_err(|e| ApiError::internal(&format!("node '{node_id}': unreadable cleanup answer: {e}")))
}

async fn node_endpoint_and_token(
    state: &AppState,
    node_id: &str,
) -> Result<(String, String), String> {
    let endpoint = node_http_endpoint(state, node_id)
        .await
        .map_err(|e| format!("node '{node_id}': {}", e.error))?;
    let token = node_token_for(state, node_id)
        .await
        .ok_or_else(|| format!("node '{node_id}' has no token yet"))?;
    Ok((endpoint, token))
}

/// The Gateway's own rows — the **only** local directory scan in this protocol
/// (ADR-123 §1.3 red line 1's exception for the Gateway's own data).
///
/// `logs` is the one cleanable row. The other two are data-class: the package
/// registry is the `.bot` distribution source every node install downloads from
/// (deleting it turns installs into 404s, so it is *not* a "download cache" —
/// ADR-123 §3.1's wording is corrected here), and `apps` holds installed hosted
/// applications.
async fn gateway_self_items(config: &GatewayConfig) -> Vec<StorageItem> {
    let data_dir = std::path::PathBuf::from(&config.data_dir);
    vec![
        StorageItem {
            id: "logs".to_string(),
            kind: StorageKind::Cache,
            bytes: dir_bytes(&data_dir.join("logs")).await,
            cleanable: true,
        },
        StorageItem {
            id: "package-registry".to_string(),
            kind: StorageKind::Data,
            bytes: dir_bytes(&config.package_registry_dir()).await,
            cleanable: false,
        },
        StorageItem {
            id: "apps".to_string(),
            kind: StorageKind::Data,
            bytes: dir_bytes(&data_dir.join("apps")).await,
            cleanable: false,
        },
    ]
}

async fn gateway_self_cleanup(
    state: &AppState,
    req: &CleanupRequest,
) -> Result<CleanupResult, ApiError> {
    let data_dir = {
        let gw = state.gateway_state.read().await;
        gw.config.as_ref().map(|c| c.data_dir.clone())
    };
    let Some(data_dir) = data_dir else {
        return Err(ApiError::service_unavailable("gateway config unavailable"));
    };

    let mut freed_bytes = 0u64;
    for id in &req.ids {
        match id.as_str() {
            "logs" => {
                freed_bytes +=
                    purge_files_older_than(&std::path::PathBuf::from(&data_dir).join("logs"), LOG_RETENTION_SECS)
                        .await
            }
            other => {
                return Err(ApiError::bad_request(&format!(
                    "'{other}' is not a cleanable gateway category"
                )));
            }
        }
    }
    tracing::info!(freed_bytes, ids = ?req.ids, "gateway storage cleanup ran");
    Ok(CleanupResult { freed_bytes })
}
#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use tokio::sync::RwLock;

    use crate::http::auth::HttpAuth;
    use crate::http::routes::AppState;

    /// A Gateway state whose `data_dir` is a fresh temp dir, plus that dir.
    async fn state_with_data_dir() -> (AppState, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = crate::test_support::unique_temp_dir(format!(
            "acowork-test-cache-api-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config = crate::config::GatewayConfig {
            data_dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        };
        let mut gw = crate::gateway::state::GatewayState::new(&config.data_dir.clone());
        gw.config = Some(config);

        let state = AppState::new(
            Arc::new(RwLock::new(gw)),
            Arc::new(HttpAuth::new(false)),
        );
        (state, dir)
    }

    /// The Gateway's own rows: `logs` is the only cleanable one, and the
    /// package registry is **data** — it is the `.bot` distribution source
    /// every node install downloads from, so a "clean it" button would turn
    /// installs into 404s (ADR-123 §3.1's "download cache" wording was wrong).
    #[tokio::test]
    async fn package_registry_is_data_and_logs_is_the_only_cleanable_row() {
        let (state, dir) = state_with_data_dir().await;
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join("logs").join("gateway.log"), b"0123456789").unwrap();
        std::fs::create_dir_all(dir.join("package-registry")).unwrap();
        std::fs::write(dir.join("package-registry").join("a.agent"), b"abc").unwrap();
        std::fs::create_dir_all(dir.join("apps")).unwrap();

        let Json(resp) = cache_report(State(state)).await;
        let gateway = resp
            .services
            .iter()
            .find(|r| r.service == SERVICE_GATEWAY)
            .expect("gateway row present");
        assert!(gateway.reachable);

        let row = |id: &str| gateway.items.iter().find(|i| i.id == id).expect(id);
        assert_eq!(row("logs").kind, StorageKind::Cache);
        assert!(row("logs").cleanable);
        assert_eq!(row("logs").bytes, 10);
        assert_eq!(row("package-registry").kind, StorageKind::Data);
        assert!(!row("package-registry").cleanable);
        assert_eq!(row("package-registry").bytes, 3);
        assert_eq!(row("apps").kind, StorageKind::Data);
        assert!(!row("apps").cleanable);
        assert_eq!(gateway.items.iter().filter(|i| i.cleanable).count(), 1);
    }

    /// A data-class or unknown category is refused instead of silently
    /// "succeeding" with 0 bytes (ADR-123 §1.3 red line 2).
    #[tokio::test]
    async fn cleanup_refuses_a_non_cleanable_category() {
        let (state, _dir) = state_with_data_dir().await;
        let err = gateway_self_cleanup(&state, &CleanupRequest { ids: vec!["apps".into()] })
            .await
            .expect_err("data-class id must be refused");
        assert_eq!(err.code, 400);
        assert!(err.error.contains("not a cleanable"));
    }

    /// The cleanable sweep frees exactly what the retention rule collects —
    /// an old log file goes, a fresh one stays.
    #[tokio::test]
    async fn cleanup_drops_old_logs_only() {
        let (state, dir) = state_with_data_dir().await;
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let old = dir.join("logs").join("old.log");
        std::fs::write(&old, b"0123456789").unwrap();
        let mtime = std::time::SystemTime::now()
            - std::time::Duration::from_secs(LOG_RETENTION_SECS + 3600);
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(mtime).unwrap();
        drop(file);
        let fresh = dir.join("logs").join("fresh.log");
        std::fs::write(&fresh, b"abc").unwrap();

        let result = gateway_self_cleanup(&state, &CleanupRequest { ids: vec!["logs".into()] })
            .await
            .expect("logs is cleanable");
        assert_eq!(result.freed_bytes, 10);
        assert!(!old.exists());
        assert!(fresh.exists());
    }
}
