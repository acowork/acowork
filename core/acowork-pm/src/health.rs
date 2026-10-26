//! PM 服务健康检查端点（supervisor 探活契约，ADR-064）。
//!
//! 复用 [`acowork_core::health::HealthResponse`] 契约：Gateway supervisor
//! 通过 `GET /health` 判断 PM 进程是否就绪/存活（与 embed / LSP relay 一致）。

use std::path::PathBuf;

use axum::Json;
use serde_json::json;

/// 构建 PM 服务的 `/health` 路由。
///
/// `data_dir` 被捕获进响应 `details` 供诊断（supervisor 日志可确认 PM
/// 数据目录解析正确，与 `acowork-gateway/`、`acowork-node/` 平级）。
pub fn health_route(data_dir: PathBuf) -> axum::Router {
    axum::Router::new().route(
        "/health",
        axum::routing::get(move || async move {
            Json(acowork_core::health::HealthResponse {
                status: "ok".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                process: "acowork-pm".to_string(),
                details: Some(json!({
                    "data_dir": data_dir.display().to_string(),
                    "dispatch": crate::dispatch::metrics_snapshot(),
                })),
            })
        }),
    )
}
