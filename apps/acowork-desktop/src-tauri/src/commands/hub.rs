//! Agent Hub commands — catalog read and the one-step "download → install".
//!
//! PRD: docs/prd/zh/prd-agent-hub-install-dialog.md. The whole Hub path is
//! Rust-side on purpose (D6/F1): the catalog and the package are content-type
//! checked, sha256 verified and timeout bounded here, and the package bytes
//! never travel through the webview.
//!
//! Submission reuses the existing Gateway endpoints unchanged — `/ensure` for
//! the idempotent primary action, `/install` for an explicit extra instance —
//! including the ADR-059 `dependency_not_ready` retry and the wait for the
//! Gateway inventory to actually flip.

use std::time::Instant;

use serde::Serialize;
use tauri::State;

use crate::commands::agent::{INSTALL_WAIT_SECS, submit_with_retry};
use crate::gateway_client::{EnsureAck, OperationAck};
use crate::hub_client::{self, HubIndex, LocalPackageInfo};
use crate::state::AppState;

/// What a Hub install resolved to. `instance_id` is the key the frontend must
/// select with — the package `agent_id` is ambiguous under ADR-073 and is
/// never a registry key (F8).
#[derive(Debug, Clone, Serialize)]
pub struct HubInstallResult {
    pub agent_id: String,
    pub instance_id: String,
    /// `true` when the package was already on the target node: this click
    /// created nothing and the existing instance was returned.
    pub already_present: bool,
}

/// `GET {hub}/data/index.json` — the catalog the Agent 库 tab renders.
///
/// Failures are logged with the elapsed time: the Hub pull success rate has
/// no telemetry pipeline behind it (PRD §10), so the Desktop log is the only
/// record of a bad day at the CDN.
#[tauri::command]
pub async fn fetch_hub_index() -> Result<HubIndex, String> {
    let started = Instant::now();
    match hub_client::fetch_index().await {
        Ok(index) => {
            tracing::info!(
                "[HUB] catalog fetched: {} agent(s) in {:.1}s",
                index.agents.len(),
                started.elapsed().as_secs_f64()
            );
            Ok(index)
        }
        Err(e) => {
            tracing::warn!(
                "[HUB] catalog fetch failed after {:.1}s: {}",
                started.elapsed().as_secs_f64(),
                e
            );
            Err(e.to_string())
        }
    }
}

/// Summarize a local `.agent` file for the Local tab (name / size / package
/// id) before the user commits to installing it.
#[tauri::command]
pub async fn inspect_local_package(package_path: String) -> Result<LocalPackageInfo, String> {
    hub_client::inspect_local_package(&package_path).map_err(|e| e.to_string())
}

/// Download a Hub package, verify it, and install it on the target node.
///
/// `ensure` selects the endpoint (D2): `true` (the card's primary button) is
/// idempotent — at most one instance exists on the node afterwards; `false`
/// (the card's "install another instance" menu entry) always lands another
/// instance, which is legal, not an error.
///
/// `agent_id` is the catalog package id the card was built from. It is used as
/// the cross-check against the downloaded manifest (an entry that points at
/// another package must not install silently); the result then names the
/// identity the bytes themselves carry — `/ensure` echoes it in its ack, and
/// `/install` answers a bare `OperationAck`.
#[tauri::command]
pub async fn install_from_hub(
    state: State<'_, AppState>,
    agent_id: String,
    package_file: String,
    sha256: String,
    node_id: String,
    ensure: Option<bool>,
) -> Result<HubInstallResult, String> {
    crate::commands::agent::require_node_id(&node_id)?;
    let ensure = ensure.unwrap_or(true);
    let started = Instant::now();
    let label = if ensure { "HUB_ENSURE" } else { "HUB_INSTALL" };

    let bytes = hub_client::download_package(&package_file)
        .await
        .map_err(|e| format!("[{label}] {e}"))?;
    // REQ-015: a mismatch aborts here — nothing reaches the Gateway.
    hub_client::verify_sha256(&bytes, &sha256).map_err(|e| format!("[{label}] {e}"))?;
    // A matching hash only proves the bytes are what this catalog entry
    // describes — the path and the digest both come from that entry, so an
    // entry pointing at another package's file would still verify. Read the
    // identity out of the downloaded manifest and refuse the mismatch: PRD §8
    // makes `agent_id` the cross-check, not a label to echo back.
    let manifest_id = crate::commands::agent::read_agent_id_from_package(&bytes)
        .map_err(|e| format!("[{label}] {e}"))?;
    if manifest_id != agent_id {
        return Err(format!(
            "[{label}] Catalog entry {agent_id} points at a package whose manifest says \
             {manifest_id}; nothing was submitted"
        ));
    }
    tracing::info!(
        "[{label}] downloaded {} bytes for {package_file} — checksum and manifest id ok",
        bytes.len()
    );

    let result = if ensure {
        let state_for_op = state.clone();
        let ack: EnsureAck = submit_with_retry(&state, label, move || {
            let state = state_for_op.clone();
            let bytes = bytes.clone();
            let node_id = node_id.clone();
            async move {
                let client = state.gateway.read().await;
                client.ensure_agent(&bytes, &node_id).await
            }
        })
        .await?;
        if ack.already_present {
            // Nothing was dispatched, so there is nothing to wait for — the
            // node already holds this instance.
            HubInstallResult {
                agent_id: ack.agent_id,
                instance_id: ack.instance_id,
                already_present: true,
            }
        } else {
            let instance_id = ack.instance_id.clone();
            wait_until_installed(&state, label, &instance_id, ack.operation_id.as_deref()).await?;
            HubInstallResult {
                agent_id: ack.agent_id,
                instance_id,
                already_present: false,
            }
        }
    } else {
        let state_for_op = state.clone();
        let ack: OperationAck = submit_with_retry(&state, label, move || {
            let state = state_for_op.clone();
            let bytes = bytes.clone();
            let node_id = node_id.clone();
            async move {
                let client = state.gateway.read().await;
                // `dev_mode` is not a client-side knob — the Gateway reads
                // its own config (F3). Pass the historical default.
                client.install_agent(&bytes, false, &node_id).await
            }
        })
        .await?;
        // ADR-073: the inventory is keyed by the instance id the ack carries;
        // polling the package id would hang for the whole wait budget.
        let instance_id = ack.instance_id.clone().ok_or_else(|| {
            format!(
                "[{label}] Install accepted (operation {}) but the Gateway reported no instance id",
                ack.operation_id
            )
        })?;
        wait_until_installed(&state, label, &instance_id, Some(ack.operation_id.as_str())).await?;
        HubInstallResult {
            agent_id: manifest_id,
            instance_id,
            already_present: false,
        }
    };

    tracing::info!(
        "[{label}] {package_file} resolved to instance {} in {:.1}s (already_present={})",
        result.instance_id,
        started.elapsed().as_secs_f64(),
        result.already_present
    );
    Ok(result)
}

/// Wait until the instance lands in the Gateway inventory (ADR-059 §6).
///
/// A timeout is not a failure of the request — the Node may still be
/// installing — so the message says so and keeps the operation id, which is
/// what an operator greps the logs for.
async fn wait_until_installed(
    state: &AppState,
    label: &str,
    instance_id: &str,
    operation_id: Option<&str>,
) -> Result<(), String> {
    let op = operation_id.unwrap_or("n/a");
    let client = state.gateway.read().await;
    client
        .wait_for_agent_installed(
            instance_id,
            std::time::Duration::from_secs(INSTALL_WAIT_SECS),
        )
        .await
        .map(|_| ())
        .map_err(|e| {
            format!(
                "[{label}] submitted (operation {op}) but instance {instance_id} did not appear \
                 within {INSTALL_WAIT_SECS}s: {e}"
            )
        })
}
