//! PM 服务健康检查端点（supervisor 探活契约，ADR-064）。
//!
//! 复用 [`acowork_core::health::HealthResponse`] 契约：Gateway supervisor
//! 通过 `GET /health` 判断 PM 进程是否就绪/存活（与 embed / LSP relay 一致）。

use std::path::PathBuf;

use axum::Json;
use serde_json::json;

use crate::config::StorageBackend;
use crate::store::corrupt::CorruptRegistry;

/// 构建 PM 服务的 `/health` 路由。
///
/// `data_dir` 被捕获进响应 `details` 供诊断（supervisor 日志可确认 PM
/// 数据目录解析正确，与 `acowork-gateway/`、`acowork-node/` 平级）。
/// `corrupt` 是最近一次索引重建发现的坏文件清单（ADR-117 D4 第 1 层）。
/// `storage_backend` makes the effective ADR-117 D6 choice observable, so a
/// rollout can prove which backend is actually serving instead of trusting
/// the config file.
pub fn health_route(
    data_dir: PathBuf,
    corrupt: CorruptRegistry,
    storage_backend: StorageBackend,
) -> axum::Router {
    axum::Router::new().route(
        "/health",
        axum::routing::get(move || {
            let corrupt = corrupt.clone();
            async move {
                let files = corrupt.read().clone();
                Json(acowork_core::health::HealthResponse {
                    status: "ok".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    process: "acowork-pm".to_string(),
                    details: Some(json!({
                        "data_dir": data_dir.display().to_string(),
                        "storage_backend": storage_backend.as_str(),
                        "dispatch": crate::dispatch::metrics_snapshot(),
                        "corrupt_count": files.len(),
                        "corrupt_files": files,
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
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// ADR-117 D6: the effective backend is observable, so an operator can
    /// prove which config the process actually started with.
    #[tokio::test]
    async fn health_exposes_the_effective_storage_backend() {
        let registry: CorruptRegistry = Arc::new(RwLock::new(Vec::new()));
        let router = health_route(
            PathBuf::from("/tmp/pm-health-contract-test"),
            registry,
            StorageBackend::Dual,
        );
        let resp = router
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
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["details"]["storage_backend"], "dual");
        assert_eq!(json["details"]["corrupt_count"], 0);
        assert_eq!(json["details"]["corrupt_files"], serde_json::json!([]));
    }
}
