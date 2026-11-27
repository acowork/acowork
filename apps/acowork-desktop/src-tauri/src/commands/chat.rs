//! Chat commands

use tauri::State;

use crate::gateway_client::FileUploadResponse;
use crate::state::AppState;

/// Upload a single attachment (document or image) to a session.
///
/// ADR-046: replaces the legacy `upload_document` Tauri command which only
/// handled PDF/DOCX/PPTX/XLSX. The same code path now serves images too — the
/// frontend distinguishes them via the `format` field and supplies optional
/// `width` / `height` for images that the desktop measured via `new Image()`.
///
/// Reaches the runtime via `POST /api/agents/{agent_id}/sessions/{sid}/files`.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn upload_file(
    state: State<'_, AppState>,
    agent_id: String,
    session_id: String,
    file_path: String,
    format: String,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<FileUploadResponse, String> {
    let client = state.gateway.read().await;
    client
        .upload_file(&agent_id, &session_id, &file_path, &format, width, height)
        .await
        .map_err(|e| e.to_string())
}

/// Return the size of a file on disk in bytes.
///
/// Used by the chat panel as a pre-flight check before `upload_file`:
/// nothing in the transport refuses the file until the *last* hop
/// (ADR-122 D3, `complete`), so an oversized file would spend its whole
/// upload first. The panel compares against
/// [`get_upload_limit_bytes`] and answers with a toast instead.
///
/// Returns an error if the path does not exist or is not a regular
/// file (mirrors the existence check inside `upload_file` itself so
/// the two paths stay consistent).
#[tauri::command]
pub async fn get_file_size(file_path: String) -> Result<u64, String> {
    let path = std::path::Path::new(&file_path);
    let metadata = std::fs::metadata(path)
        .map_err(|e| format!("Cannot read file metadata: {}", e))?;
    if !metadata.is_file() {
        return Err(format!("Not a regular file: {}", file_path));
    }
    Ok(metadata.len())
}

/// The single-file ceiling in force for the local runtime (ADR-122 D6).
///
/// The chat panel used to mirror `MAX_UPLOAD_BYTES` in TS — a constant that had
/// already drifted from the backend once, and that could never be right once
/// the ceiling became deployment-configurable (`ACOWORK_MAX_UPLOAD_BYTES`).
/// Asking the side that enforces it removes the mirror instead of syncing it.
#[tauri::command]
pub async fn get_upload_limit_bytes() -> Result<u64, String> {
    Ok(acowork_core::upload::max_file_bytes())
}
