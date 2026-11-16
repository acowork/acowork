//! `attachments` handlers + multipart upload。
//! 内部路径不带 `/api` 前缀，公开路径为 `/api/pm/attachments/*`
//! （见 [`routes::pm_router`]）。
//!
//! 职责边界（T2-10 决策）：
//! - **handler**：multipart 解析、大小/MIME 校验、sha256、缩略图生成、meta 构造
//! - **store**：物理文件写入/读取 + 元数据注册（持有 `attachments/{att_id}/` 布局）
//!
//! 参考：[`docs/design/zh/21-pm-project-management.md`] §3.6 / §5。

use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::types::{AttachmentId, AttachmentKind, AttachmentMeta};

use super::ApiState;
use crate::error::{PmError, Result};

// ────────────────────────────────────────────────────────────────────────────
// 常量：MIME 白名单 / 危险类型黑名单
// ────────────────────────────────────────────────────────────────────────────

/// 自动生成缩略图的图片扩展名白名单（与设计文档 §3.6 对齐）。
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// 明确拒绝的可执行/脚本类型（安全边界：不因附件上传引入代码执行面）。
const BLOCKED_CONTENT_TYPES: &[&str] = &[
    "application/x-msdownload",
    "application/x-msdos-program",
    "application/vnd.microsoft.portable-executable",
    "application/x-sh",
    "text/x-sh",
    "application/x-shellscript",
    "application/x-msi",
    "application/x-httpd-php",
    "application/x-python-code",
    "text/x-python",
];

// ────────────────────────────────────────────────────────────────────────────
// GET /tasks/:tid/attachments
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state))]
pub async fn list(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
) -> Result<Json<Vec<AttachmentMeta>>> {
    let tid = tid.parse::<crate::types::TaskId>()?;
    let metas = state.store.list_attachments(&tid).await?;
    Ok(Json(metas))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /tasks/:tid/attachments
// ────────────────────────────────────────────────────────────────────────────

/// 上传附件（multipart/form-data）。
///
/// Form 字段：
/// - `file` —— 必需，文件二进制（`filename` / `content_type` 由 multipart 提供）
///
/// 返回持久化后的 [`AttachmentMeta`]。
#[tracing::instrument(skip(state, headers, multipart))]
pub async fn upload(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<AttachmentMeta>> {
    let tid = tid.parse::<crate::types::TaskId>()?;
    let actor = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| PmError::Internal("missing X-Actor header".to_string()))?
        .to_string();

    // 解析 multipart：只取 `file` 字段
    let mut filename: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| PmError::Multipart(format!("read field: {e}")))?
    {
        if field.name() == Some("file") {
            filename = field.file_name().map(sanitize_filename);
            content_type = field.content_type().map(|s| s.to_string());
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
    let filename = filename.unwrap_or_else(|| "attachment.bin".to_string());
    let content_type = content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    // 大小校验（config.max_attachment_size，默认 10 MiB）
    let max = state.config.max_attachment_size;
    if bytes.len() as u64 > max {
        return Err(PmError::AttachmentTooLarge {
            size: bytes.len() as u64,
            max,
        });
    }

    // MIME 安全校验（拒绝可执行/脚本）
    validate_mime(&content_type)?;

    // sha256（完整性 + 物理去重基础）
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha256 = hex::encode(hasher.finalize());

    // 图片 → 生成缩略图 + 尺寸；其他 → File
    let ext = std::path::Path::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let is_image = content_type.starts_with("image/") && IMAGE_EXTS.contains(&ext.as_str());

    let (kind, thumb_bytes, width, height) = if is_image {
        let (thumb, w, h) = generate_thumb(&bytes, state.config.thumbnail_max_edge)?;
        (AttachmentKind::Image, thumb, w, h)
    } else {
        (AttachmentKind::File, None, None, None)
    };

    // 构造 storage_path / thumb_path（相对任务目录，与 store 布局约定一致）
    let att_id = AttachmentId::generate();
    let storage_ext = if ext.is_empty() { "bin" } else { &ext };
    let att_dir = format!("attachments/{}", att_id.as_str());
    let storage_path = format!("{att_dir}/original.{storage_ext}");
    let thumb_path = if kind == AttachmentKind::Image && thumb_bytes.is_some() {
        Some(format!("{att_dir}/thumb.jpg"))
    } else {
        None
    };

    let meta = AttachmentMeta {
        id: att_id,
        filename,
        kind,
        content_type,
        size: bytes.len() as u64,
        sha256,
        storage_path,
        thumb_path,
        width,
        height,
        uploaded_by: actor,
        uploaded_at: Utc::now(),
    };

    let saved = state
        .store
        .write_attachment(&tid, meta, bytes, thumb_bytes)
        .await?;
    Ok(Json(saved))
}

// ────────────────────────────────────────────────────────────────────────────
// GET /attachments/:aid
// ────────────────────────────────────────────────────────────────────────────

/// 下载附件（`?download=1` 强制 `attachment` 头；`?thumb=1` 返回缩略图）。
#[tracing::instrument(skip(state))]
pub async fn download(
    State(state): State<ApiState>,
    Path(aid): Path<String>,
    query: Query<DownloadQuery>,
) -> Result<Response> {
    let aid = aid.parse::<AttachmentId>()?;
    let (meta, bytes) = state
        .store
        .read_attachment_bytes(&aid, query.thumb)
        .await?
        .ok_or_else(|| PmError::AttachmentNotFound(aid.to_string()))?;

    let mut header_map = HeaderMap::new();
    header_map.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&meta.content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    header_map.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&bytes.len().to_string()).unwrap(),
    );

    let disposition = if query.download {
        format!(
            "attachment; filename=\"{}\"",
            disposition_safe(&meta.filename)
        )
    } else {
        format!("inline; filename=\"{}\"", disposition_safe(&meta.filename))
    };
    header_map.insert(CONTENT_DISPOSITION, HeaderValue::from_str(&disposition).unwrap());

    Ok((header_map, Body::from(bytes)).into_response())
}

#[derive(Debug, serde::Deserialize)]
pub struct DownloadQuery {
    #[serde(default)]
    pub download: bool,
    /// `?thumb=1` 返回缩略图（仅图片）。
    #[serde(default)]
    pub thumb: bool,
}

// ────────────────────────────────────────────────────────────────────────────
// DELETE /attachments/:aid
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state))]
pub async fn delete(
    State(state): State<ApiState>,
    Path(aid): Path<String>,
) -> Result<axum::http::StatusCode> {
    let aid = aid.parse::<AttachmentId>()?;
    state.store.delete_attachment(&aid).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ────────────────────────────────────────────────────────────────────────────
// 内部辅助
// ────────────────────────────────────────────────────────────────────────────

/// MIME 安全校验：拒绝黑名单中的可执行/脚本类型，其余放行。
fn validate_mime(content_type: &str) -> Result<()> {
    let ct = content_type.to_ascii_lowercase();
    if BLOCKED_CONTENT_TYPES.iter().any(|b| ct.starts_with(b)) {
        return Err(PmError::AttachmentMimeRejected(ct));
    }
    Ok(())
}

/// 缩略图生成结果：`(jpeg 字节, 宽, 高)`；`image-thumb` 关闭时返回 `(None, None, None)`。
type ThumbResult = (Option<Vec<u8>>, Option<u32>, Option<u32>);

/// 缩略图生成（feature-gated）。
///
/// `image-thumb` feature 关闭时返回 `(None, None, None)` —— 前端预览直接拉原图。
#[cfg(feature = "image-thumb")]
fn generate_thumb(
    bytes: &[u8],
    max_edge: u32,
) -> Result<ThumbResult> {
    use image::GenericImageView;

    let img = image::load_from_memory(bytes).map_err(|e| PmError::Image(e.to_string()))?;
    let (w, h) = img.dimensions();
    let thumb = if w.max(h) > max_edge {
        img.thumbnail(max_edge, max_edge)
    } else {
        img
    };
    let mut out = Vec::new();
    thumb
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg)
        .map_err(|e| PmError::Image(e.to_string()))?;
    Ok((Some(out), Some(w), Some(h)))
}

#[cfg(not(feature = "image-thumb"))]
fn generate_thumb(
    _bytes: &[u8],
    _max_edge: u32,
) -> Result<ThumbResult> {
    Ok((None, None, None))
}

/// 文件名净化：去路径、去不可见字符，仅保留文件名字面部分。
fn sanitize_filename(raw: &str) -> String {
    let base = std::path::Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Content-Disposition 的 filename 值：剥离非 ASCII 与引号，防 header 注入。
fn disposition_safe(filename: &str) -> String {
    filename
        .chars()
        .filter(|c| c.is_ascii() && !['"', '\\', '\r', '\n'].contains(c))
        .collect::<String>()
        .trim()
        .to_string()
}
