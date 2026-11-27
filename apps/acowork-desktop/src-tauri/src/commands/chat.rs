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
///
/// `on_progress` carries accepted-byte counts while the upload runs, mirroring
/// `download_attachment`'s channel so one chip can render either direction.
/// Required rather than optional: `Option<Channel<_>>` does not implement
/// `Deserialize`, and there is exactly one caller.
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
    on_progress: tauri::ipc::Channel<u64>,
) -> Result<FileUploadResponse, String> {
    let client = state.gateway.read().await;
    client
        .upload_file(
            &agent_id,
            &session_id,
            &file_path,
            &format,
            width,
            height,
            Some(&on_progress),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Stop an upload in progress.
///
/// Records the intent and returns: an `invoke` already in flight cannot be
/// aborted, so `upload_file`'s chunk loop notices at its next chunk boundary
/// (≤ 2 MiB away), releases the chunks that landed, and fails with "upload
/// cancelled". The caller removes the chip itself — waiting for the loop to
/// come back would leave a frozen 40% on screen for a whole chunk.
#[tauri::command]
pub async fn cancel_upload(state: State<'_, AppState>, file_path: String) -> Result<(), String> {
    // Deliberately not `state.gateway.read()`: the upload that is being
    // cancelled holds that read lock for its whole duration.
    state.upload_cancels.request(&file_path);
    Ok(())
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
pub async fn get_upload_limit_bytes(state: State<'_, AppState>) -> Result<u64, String> {
    // Ask the Gateway, not this process: the Desktop app does not read the
    // Gateway's TOML and is not the process that enforces the ceiling, so its
    // own env would answer 1 GiB on a deployment that configured less.
    let client = state.gateway.read().await;
    match client.get_config().await {
        Ok(cfg) => Ok(cfg.max_upload_bytes),
        // Gateway unreachable — the local default still beats refusing to
        // answer, and `upload_file` fails on the same connection anyway.
        Err(_) => Ok(acowork_core::upload::max_file_bytes()),
    }
}
