//! Document REST handlers (design §4).
//!
//! Each handler is a thin shell: parse + validate input, delegate to
//! `DocState::docs`, render the result as a DTO. **Never** touch
//! `crate::store::*` / `crate::types` directly — go through the
//! `DocumentService` trait so business rules (version concurrency,
//! rename ↔ filename invariants) stay in one auditable place.

use axum::extract::{Multipart, Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::api::body::DocJson;
use crate::limit;
use serde::Deserialize;

use crate::api::dto::{
    CreateDocBody, DocMetaDto, DocReadDto, MoveDocBody, RenameDocBody, UpdateDocBody,
};
use crate::api::{actor_from, ApiError, ApiResult, ApiState};
use crate::error::DocError;
use crate::path::{parse_import_file_name, validate_doc_id};
use crate::service::acl::AclTarget;
use crate::service::document::{
    CreateDocumentInput, CreateFileInput, DocumentService, UpdateDocumentInput,
};
use crate::types::{AclRole, DocMeta};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub dir_id: String,
}

/// `POST /api/docs` — create a document.
pub async fn create_doc(
    state: ApiState,
    headers: HeaderMap,
    DocJson(body): DocJson<CreateDocBody>,
) -> ApiResult<(StatusCode, Json<DocMetaDto>)> {
    let actor = actor_from(&headers);
    // Adding a doc is an edit on the hosting directory (design §9).
    state
        .acl
        .check(&actor, AclTarget::Dir(&body.parent_dir_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    // Document ceiling, measured on the decoded content (see `crate::limit`).
    limit::ensure_document_size(body.content.len(), state.config.max_doc_size_bytes)
        .map_err(ApiError::from)?;
    let meta: DocMeta = state
        .docs
        .create(CreateDocumentInput {
            parent_dir_id: body.parent_dir_id,
            title: body.title,
            content: body.content,
            import: body.import,
            creator: actor.id.clone(),
        })
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(DocMetaDto::from(meta))))
}

/// `GET /api/docs/:doc_id` — read a document (meta + content).
pub async fn read_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<DocReadDto>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let read = state.docs.read(&doc_id).await.map_err(ApiError::from)?;
    Ok(Json(DocReadDto::from(read)))
}

/// `PUT /api/docs/:doc_id` — update content + (optionally) title.
pub async fn update_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
    DocJson(body): DocJson<UpdateDocBody>,
) -> ApiResult<Json<DocMetaDto>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    limit::ensure_document_size(body.content.len(), state.config.max_doc_size_bytes)
        .map_err(ApiError::from)?;
    let meta = state
        .docs
        .update(
            &doc_id,
            UpdateDocumentInput {
                base_version: body.base_version,
                title: body.title,
                content: body.content,
            },
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(DocMetaDto::from(meta)))
}

/// `PATCH /api/docs/:doc_id/title` — rename a document.
pub async fn rename_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
    Json(body): Json<RenameDocBody>,
) -> ApiResult<Json<DocMetaDto>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let meta = state
        .docs
        .rename(&doc_id, &body.new_title, body.base_version)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(DocMetaDto::from(meta)))
}

/// `POST /api/docs/:doc_id/move` — move to another directory.
pub async fn move_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
    Json(body): Json<MoveDocBody>,
) -> ApiResult<Json<DocMetaDto>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    // Moving removes the doc from its current dir (edit there) and adds
    // it to the target (edit there) — both gates apply (design §9).
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    state
        .acl
        .check(&actor, AclTarget::Dir(&body.target_dir_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let meta = state
        .docs
        .move_doc(&doc_id, &body.target_dir_id, None, body.overwrite)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(DocMetaDto::from(meta)))
}

/// `DELETE /api/docs/:doc_id` — soft-delete (moves to `.trash/`).
pub async fn delete_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
) -> ApiResult<StatusCode> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    state.docs.delete(&doc_id).await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/docs/:doc_id/path` — return the on-disk relative path.
pub async fn doc_path(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let path = state.docs.path_of(&doc_id).await.map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "path": path })))
}

/// `GET /api/docs?dir_id=...` — list docs under a directory.
pub async fn list_docs(
    state: ApiState,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<DocMetaDto>>> {
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Dir(&q.dir_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let mut list = state.docs.list(&q.dir_id).await.map_err(ApiError::from)?;
    state.acl.filter_docs(&actor, &q.dir_id, &mut list).await;
    Ok(Json(list.into_iter().map(DocMetaDto::from).collect()))
}

// ── file import / raw bytes ───────────────────────────────────────────────

/// `POST /api/docs/import?dir_id=…` — upload one file into a directory.
///
/// `multipart/form-data` with a single `file` part — the shape a browser's
/// `FormData` / `<input type="file">` produces, so the Desktop can hand the
/// bytes over without base64. The part's filename supplies the stem and the
/// extension; text formats (`.md` / `.markdown` / `.txt`) become an ordinary
/// Markdown document, everything else a file entry served by
/// `GET /docs/{id}/file`.
///
/// Adding an entry is an edit on the hosting directory — same gate as
/// `POST /docs`.
pub async fn import_doc(
    state: ApiState,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
    mut multipart: Multipart,
) -> ApiResult<(StatusCode, Json<DocMetaDto>)> {
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Dir(&q.dir_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;

    // Exactly one part matters; extra fields are ignored on purpose (a
    // single-file form is all the picker produces, and a second upload is a
    // second request).
    let mut upload: Option<(String, axum::body::Bytes)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::from(DocError::BadRequest(format!("invalid multipart body: {e}"))))?
    {
        if field.name() != Some("file") || upload.is_some() {
            continue;
        }
        let file_name = field
            .file_name()
            .ok_or_else(|| ApiError::from(DocError::BadRequest("file part has no filename".into())))?
            .to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| ApiError::from(DocError::BadRequest(format!("reading upload failed: {e}"))))?;
        upload = Some((file_name, bytes));
    }
    let (file_name, bytes) =
        upload.ok_or_else(|| ApiError::from(DocError::BadRequest("missing `file` part".into())))?;

    let (stem, ext) = parse_import_file_name(&file_name).map_err(ApiError::from)?;
    let kind = crate::types::import_kind(&ext).expect("parse_import_file_name checked the table");

    let meta = if kind.as_text {
        // Text uploads have no separate storage shape: the library's text
        // form IS a Markdown document. Invalid UTF-8 is replaced rather than
        // rejected — a `.txt` from a legacy encoding still opens, and losing
        // a replacement char beats refusing the import.
        let content = String::from_utf8_lossy(&bytes).into_owned();
        limit::ensure_document_size(content.len(), state.config.max_doc_size_bytes)
            .map_err(ApiError::from)?;
        state
            .docs
            .create(CreateDocumentInput {
                parent_dir_id: q.dir_id,
                title: stem,
                content,
                import: None,
                creator: actor.id.clone(),
            })
            .await
            .map_err(ApiError::from)?
    } else {
        limit::ensure_document_size(bytes.len(), limit::UPLOAD_CEILING).map_err(ApiError::from)?;
        state
            .docs
            .create_file(CreateFileInput {
                parent_dir_id: q.dir_id,
                name: stem,
                ext,
                bytes: bytes.to_vec(),
                creator: actor.id.clone(),
            })
            .await
            .map_err(ApiError::from)?
    };
    Ok((StatusCode::CREATED, Json(DocMetaDto::from(meta))))
}

/// `GET /api/docs/:doc_id/file` — the raw bytes of one entry.
///
/// This is what the Desktop's document viewers (and the "save as" escape
/// hatch) read: the JSON read endpoint carries Markdown only, and an imported
/// pdf / docx / image has no Markdown body. Served `inline` with the recorded
/// MIME so an `<img>` / pdf viewer can consume the blob directly.
pub async fn doc_file(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
) -> ApiResult<Response> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let meta = state.docs.stat(&doc_id).await.map_err(ApiError::from)?.meta;
    let path = state.docs.absolute_path(&doc_id).await.map_err(ApiError::from)?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ApiError::from(DocError::Io(e)))?;
    let mime = meta
        .ext
        .as_deref()
        .and_then(crate::types::import_kind)
        .map(|k| k.mime)
        .unwrap_or("text/markdown; charset=utf-8");
    Ok((
        [
            ("content-type", mime.to_string()),
            ("content-disposition", "inline".to_string()),
            // The MIME is derived from the extension, and an uploaded `svg`
            // can carry script — never let a browser sniff a different type.
            ("x-content-type-options", "nosniff".to_string()),
        ],
        bytes,
    )
        .into_response())
}
