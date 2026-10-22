//! `icon` handlers — project-level icon image assets (custom upload).
//!
//! 内部路径不带 `/api` 前缀，公开路径为 `/api/pm/projects/*`
//! （见 [`routes::pm_router`]）。
//!
//! 与头像自定义上传对齐（`acowork-user` `profile_api`），但作用域是**项目**：
//! 上传的图片落在 `{data_dir}/projects/{pid}/icon/icon-NN.{ext}`，引用由前端
//! 存进 `project.metadata.icon`（本模块只管物理字节，不写 metadata）。
//!
//! - `POST /projects/{pid}/icon`          —— multipart 上传，返回相对路径（不自动选中）
//! - `GET  /projects/{pid}/icon-assets`   —— 列出该项目已上传图标
//! - `GET  /projects/{pid}/icon-file`     —— 按 `?path=` 取回字节（命名空间守卫）

use std::collections::BTreeSet;

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::ApiState;
use crate::error::{PmError, Result};
use crate::store::tree::PmStore;
use crate::types::ProjectId;

/// 项目图标允许的上传扩展名（与 store 侧 `icon_ext` 白名单一致）。
const ICON_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg"];

/// `{data_dir}` 相对的项目图标目录前缀（存进 `metadata.icon.path`、serve 校验用）。
fn icon_rel_prefix(pid: &ProjectId) -> String {
    format!("projects/{}/icon/", pid.as_str())
}

fn is_allowed_ext(ext: &str) -> bool {
    ICON_EXTS.contains(&ext.to_ascii_lowercase().as_str())
}

/// 计算项目 `icon/` 目录下下一个可用 `icon-NN.{ext}` 文件名（仿头像 `next_avatar_name`）。
fn next_icon_name(existing: &[String], ext: &str) -> String {
    let mut used = BTreeSet::new();
    for name in existing {
        if let Some(cap) = name.strip_prefix("icon-")
            && let Some(num) = cap.split('.').next()
            && let Ok(n) = num.parse::<u32>()
        {
            used.insert(n);
        }
    }
    let mut n = 1u32;
    while used.contains(&n) {
        n = n.saturating_add(1);
    }
    format!("icon-{:02}.{}", n, ext)
}

/// 确保项目存在（不存在 → 404）。图标面复用项目存在性，不引入新的权限轴
/// （PM 无 ACL，写权限即 REST update 面）。
async fn ensure_project(state: &ApiState, pid: &ProjectId) -> Result<()> {
    state
        .store
        .get_project(pid)
        .await?
        .map(|_| ())
        .ok_or_else(|| PmError::ProjectNotFound(pid.to_string()))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /projects/{pid}/icon
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
pub struct IconAssetEntry {
    pub relative_path: String,
}

/// 上传项目图标（multipart/form-data，字段 `file`）。
///
/// 只做物理落盘 + 返回相对路径；**不**写 `metadata.icon`——前端拿到路径后
/// 合并进 metadata 再 `PATCH /projects/{pid}`（与头像上传一致）。
pub async fn upload(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<IconAssetEntry>> {
    let pid = pid.parse::<ProjectId>()?;
    ensure_project(&state, &pid).await?;

    let mut filename: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| PmError::Multipart(format!("read field: {e}")))?
    {
        if field.name() == Some("file") {
            filename = field.file_name().map(|s| s.to_string());
            bytes = Some(
                field
                    .bytes()
                    .await
                    .map_err(|e| PmError::Multipart(format!("read bytes: {e}")))?
                    .to_vec(),
            );
            break;
        }
    }

    let bytes = bytes.ok_or_else(|| PmError::BadRequest("missing 'file' field".to_string()))?;
    if bytes.is_empty() {
        return Err(PmError::BadRequest("file is empty".to_string()));
    }

    // 大小上限：复用附件配置（默认 10 MiB，图标远小于此）。
    let max = state.config.max_attachment_size;
    if bytes.len() as u64 > max {
        return Err(PmError::AttachmentTooLarge {
            size: bytes.len() as u64,
            max,
        });
    }

    let ext = std::path::Path::new(&filename.unwrap_or_default())
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "png".to_string());
    if !is_allowed_ext(&ext) {
        return Err(PmError::AttachmentMimeRejected(ext));
    }

    let existing = state.store.list_project_icons(&pid).await?;
    let name = next_icon_name(&existing, &ext);
    state.store.write_project_icon(&pid, &name, &bytes).await?;

    let relative_path = format!("{}{}", icon_rel_prefix(&pid), name);
    tracing::info!(path = %relative_path, "project icon uploaded");
    Ok(Json(IconAssetEntry { relative_path }))
}

// ────────────────────────────────────────────────────────────────────────────
// GET /projects/{pid}/icon-assets
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
pub struct IconAssetsResponse {
    pub assets: Vec<IconAssetEntry>,
}

/// 列出某项目已上传的图标（供选择器 Custom tab 网格）。
pub async fn list_assets(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
) -> Result<Json<IconAssetsResponse>> {
    let pid = pid.parse::<ProjectId>()?;
    ensure_project(&state, &pid).await?;
    let prefix = icon_rel_prefix(&pid);
    let names = state.store.list_project_icons(&pid).await?;
    let assets = names
        .into_iter()
        .map(|n| IconAssetEntry {
            relative_path: format!("{prefix}{n}"),
        })
        .collect();
    Ok(Json(IconAssetsResponse { assets }))
}

// ────────────────────────────────────────────────────────────────────────────
// GET /projects/{pid}/icon-file?path=<relative>
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub struct IconFileQuery {
    pub path: String,
}

/// 服务端字节。命名空间守卫：`path` 必须落在本项目的 `icon/` 目录下，
/// 且文件名为净化后的单段（拒绝 `..` / 额外路径分隔），扩展名属白名单。
pub async fn serve(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    Query(q): Query<IconFileQuery>,
) -> Result<Response> {
    let pid = pid.parse::<ProjectId>()?;
    let relative = q.path.trim_start_matches('/');
    let prefix = icon_rel_prefix(&pid);
    let name = relative
        .strip_prefix(prefix.as_str())
        .ok_or_else(|| PmError::BadRequest("icon path outside project namespace".to_string()))?;
    // `name` 必须是单段、无遍历、无空。
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(PmError::PathTraversal(name.to_string()));
    }

    let (content_type, bytes) = state
        .store
        .read_project_icon(&pid, name)
        .await?
        .ok_or_else(|| PmError::AttachmentNotFound(name.to_string()))?;

    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&content_type).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
    Ok((StatusCode::OK, headers, Body::from(bytes)).into_response())
}
