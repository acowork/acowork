//! PM 服务健康检查端点（supervisor 探活契约，ADR-064）。
//!
//! 复用 [`acowork_core::health::HealthResponse`] 契约：Gateway supervisor
//! 通过 `GET /health` 判断 PM 进程是否就绪/存活（与 embed / LSP relay 一致）。

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use serde_json::{Value, json};

use crate::config::STORAGE_BACKEND;

/// 构建 PM 服务的 `/health` 路由。
///
/// `data_dir` 进入 `details` 供 supervisor 确认数据目录解析正确。
/// `diagnostics` 是 store 提供的实时 SQLite 诊断（schema 版本 / 行数 /
/// `props` 解码失败数 / integrity），每次探活重新读取，而不是快照。
pub fn health_route(
    data_dir: PathBuf,
    diagnostics: Arc<dyn Fn() -> Value + Send + Sync>,
) -> axum::Router {
    axum::Router::new().route(
        "/health",
        axum::routing::get(move || {
            let diagnostics = diagnostics.clone();
            async move {
                Json(acowork_core::health::HealthResponse {
                    status: "ok".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    process: "acowork-pm".to_string(),
                    details: Some(json!({
                        "data_dir": data_dir.display().to_string(),
                        "storage_backend": STORAGE_BACKEND,
                        "dispatch": crate::dispatch::metrics_snapshot(),
                        "storage": diagnostics(),
                    })),
                })
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::ServiceExt;

    async fn probe(router: &axum::Router) -> Value {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_reports_the_sqlite_backend_and_storage_diagnostics() {
        let provider: Arc<dyn Fn() -> Value + Send + Sync> = Arc::new(|| {
            json!({
                "schema_version": 1,
                "integrity": "ok",
                "rows": { "tasks": 3 },
                "props_decode_failures": 0,
            })
        });
        let router = health_route(PathBuf::from("/tmp/pm-health-contract-test"), provider);
        let value = probe(&router).await;
        assert_eq!(value["details"]["storage_backend"], "db");
        assert_eq!(value["details"]["storage"]["schema_version"], 1);
        assert_eq!(value["details"]["storage"]["rows"]["tasks"], 3);
        assert_eq!(value["details"]["storage"]["props_decode_failures"], 0);
    }

    /// Diagnostics must be read live on every probe, not captured once at
    /// startup — a stale health payload is worse than no payload.
    #[tokio::test]
    async fn health_reads_diagnostics_live_on_every_probe() {
        let counter = Arc::new(AtomicU64::new(0));
        let c = counter.clone();
        let provider: Arc<dyn Fn() -> Value + Send + Sync> =
            Arc::new(move || json!({ "props_decode_failures": c.load(Ordering::Relaxed) }));
        let router = health_route(PathBuf::from("/tmp/pm-health-live-test"), provider);
        assert_eq!(
            probe(&router).await["details"]["storage"]["props_decode_failures"],
            0
        );
        counter.store(2, Ordering::Relaxed);
        assert_eq!(
            probe(&router).await["details"]["storage"]["props_decode_failures"],
            2
        );
    }
}
