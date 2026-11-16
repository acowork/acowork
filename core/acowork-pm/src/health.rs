//! PM 服务健康检查端点（supervisor 探活契约，ADR-064）。
//!
//! 复用 [`acowork_core::health::HealthResponse`] 契约：Gateway supervisor
//! 通过 `GET /health` 判断 PM 进程是否就绪/存活（与 embed / LSP relay 一致）。

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use serde_json::{Value, json};

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
///
/// `projection` is a live, instance-scoped provider supplied by the store
/// (only `DualPmStore` has one). Passing a closure rather than a snapshot is
/// deliberate: `/health` must reflect the *current* counters on every probe,
/// and must not read another store's process-global metrics.
pub fn health_route(
    data_dir: PathBuf,
    corrupt: CorruptRegistry,
    storage_backend: StorageBackend,
    projection: Option<Arc<dyn Fn() -> Value + Send + Sync>>,
) -> axum::Router {
    axum::Router::new().route(
        "/health",
        axum::routing::get(move || {
            let corrupt = corrupt.clone();
            let projection = projection.clone();
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
                        // ADR-117 D6 step 6: a stale dual projection is a
                        // reconciliation signal, never a service failure.
                        "projection": projection
                            .as_ref()
                            .map(|f| f())
                            .unwrap_or(Value::Null),
                        "corrupt_count": files.len(),
                        // ADR-117 D4: redacted projection. The registry holds
                        // absolute paths + raw IO errors for the process log,
                        // but `/health` is an unauthenticated probe and must
                        // not disclose the host's directory layout.
                        "corrupt_files": files
                            .iter()
                            .map(|f| f.public_details(&data_dir))
                            .collect::<Vec<_>>(),
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
            None,
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

    /// ADR-117 D4: the health probe must not echo absolute host paths or
    /// raw IO error text from the corrupt-file registry.
    #[tokio::test]
    async fn health_redacts_corrupt_file_details() {
        use crate::store::corrupt::CorruptRecord;
        use chrono::Utc;

        let data_dir = PathBuf::from("/private/host/.acowork/acowork-pm");
        let absolute = data_dir
            .join("projects/p-1/tasks/t-1/task.json")
            .display()
            .to_string();
        let registry: CorruptRegistry = Arc::new(RwLock::new(vec![CorruptRecord {
            project_id: "p-1".into(),
            task_id: "t-1".into(),
            path: absolute,
            quarantine_path: Some("/private/host/.acowork/acowork-pm/.corrupt/x".into()),
            error: "trailing characters at line 25 column 2".into(),
            detected_at: Utc::now(),
        }]));
        let router = health_route(data_dir.clone(), registry, StorageBackend::FileTree, None);
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["details"]["corrupt_count"], 1);
        let entry = &json["details"]["corrupt_files"][0];
        assert_eq!(entry["project_id"], "p-1");
        assert_eq!(entry["reason"], "invalid_json");
        assert!(entry["relative_path"].as_str().unwrap().contains("t-1"));
        assert!(entry.get("path").is_none(), "absolute path leaked");
        assert!(entry.get("error").is_none(), "raw error leaked");
        assert!(
            entry.get("quarantine_path").is_none(),
            "quarantine path leaked"
        );
    }

    /// The projection payload is read live from *this* store on every probe,
    /// not from a process-global counter that another store could pollute.
    #[tokio::test]
    async fn health_reads_the_live_instance_scoped_projection() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let counter = Arc::new(AtomicU64::new(0));
        let provider_counter = counter.clone();
        let provider: Arc<dyn Fn() -> Value + Send + Sync> = Arc::new(move || {
            json!({
                "ok": provider_counter.load(Ordering::Relaxed),
                "consecutive_failures": 0,
            })
        });
        let registry: CorruptRegistry = Arc::new(RwLock::new(Vec::new()));
        let router = health_route(
            PathBuf::from("/tmp/pm-health-projection-test"),
            registry,
            StorageBackend::Dual,
            Some(provider),
        );
        let get = || {
            router.clone().oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
        };
        let first = axum::body::to_bytes(get().await.unwrap().into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&first).unwrap()["details"]["projection"]["ok"],
            0
        );

        counter.store(3, Ordering::Relaxed);
        let second = axum::body::to_bytes(get().await.unwrap().into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice::<Value>(&second).unwrap();
        assert_eq!(value["details"]["projection"]["ok"], 3);
        assert_eq!(value["details"]["projection"]["consecutive_failures"], 0);
    }
}
