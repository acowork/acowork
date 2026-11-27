//! `GET /storage/report` + `POST /storage/cleanup` — the Node's rows in
//! ADR-123's unified cache-management report, plus its **fan-out** over the
//! Runtimes it hosts on this machine.
//!
//! The Gateway cannot reach a Runtime directly: the `{instance_id} → loopback
//! port` mapping belongs to the Node (ADR-055 §6.4), and a Runtime's `work_dir`
//! is agent-private (ADR-009). So the chain is Gateway → Node → Runtime, and
//! this file is the middle hop. It is a *pass-through aggregator*, not a
//! governor: the retention policy for a Runtime's uploads and logs stays in the
//! Runtime (ADR-123 §1.3 red line 3), and nothing here deletes anything itself.
//!
//! | id | source | class | cleanable |
//! |---|---|---|---|
//! | `node-logs` | `{home}/logs` + `{home}/runtime-logs` (stdout capture) | cache | **yes** |
//! | `packages` | `{home}/packages` — installed `.agent` packages | data | no |
//! | `runtime-uploads` | Σ each Runtime's `uploads` row | cache | **yes** |
//! | `runtime-logs` | Σ each Runtime's `logs` row | cache | **yes** |
//! | `runtime-data` | Σ each Runtime's `data` row | data | no |
//!
//! Per-node summary, not per-Agent detail: ADR-123 §6 Q4 chose that for the
//! first version because a per-Agent breakdown would expose one agent's
//! workspace size to whatever surface renders the card.
//!
//! The LSP asset directory is deliberately **not** reported: it holds install
//! *scripts* (a few KB each) — the tools they install live in the global
//! npm/rustup caches, outside any directory this Node owns. A row for it would
//! be a number that never explains a full disk.
//!
//! Auth is the Node's existing boundary: this router is mounted on the same
//! `:19900` listener as `/fs/browse`, so a caller must present the Gateway's
//! node token (ADR-055 §6.8).

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use acowork_core::storage::{
    CleanupRequest, CleanupResult, LOG_RETENTION_SECS, StorageItem, StorageKind, StorageReport,
    dir_bytes, purge_files_older_than,
};

use crate::state::NodeHttpState;

/// Per-Runtime budget for the fan-out. A Runtime whose HTTP loop is stalled
/// must not hang the card; its bytes simply don't show up this round.
const RUNTIME_TIMEOUT: Duration = Duration::from_secs(3);

const ITEM_NODE_LOGS: &str = "node-logs";
const ITEM_PACKAGES: &str = "packages";
const ITEM_RUNTIME_UPLOADS: &str = "runtime-uploads";
const ITEM_RUNTIME_LOGS: &str = "runtime-logs";
const ITEM_RUNTIME_DATA: &str = "runtime-data";

const KNOWN_ITEMS: &[&str] = &[
    ITEM_NODE_LOGS,
    ITEM_PACKAGES,
    ITEM_RUNTIME_UPLOADS,
    ITEM_RUNTIME_LOGS,
    ITEM_RUNTIME_DATA,
];

/// Loopback ports of every Runtime currently in the process table.
async fn runtime_ports(state: &NodeHttpState) -> Vec<u16> {
    state
        .node
        .read()
        .await
        .agents
        .values()
        .map(|slot| slot.http_port)
        .collect()
}

/// `GET /storage/report`.
pub async fn storage_report(State(state): State<NodeHttpState>) -> Json<StorageReport> {
    let home = &state.config.home;
    let node_logs = dir_bytes(&home.join("logs")).await + dir_bytes(&home.join("runtime-logs")).await;
    let packages = dir_bytes(&state.config.packages_dir()).await;

    let client = reqwest::Client::new();
    let (mut up, mut lg, mut dt) = (0u64, 0u64, 0u64);
    for port in runtime_ports(&state).await {
        let Some(report) = fetch_report(&client, port).await else {
            continue;
        };
        for item in &report.items {
            match item.id.as_str() {
                "uploads" => up += item.bytes,
                "logs" => lg += item.bytes,
                "data" => dt += item.bytes,
                _ => {}
            }
        }
    }

    Json(StorageReport {
        items: vec![
            StorageItem {
                id: ITEM_NODE_LOGS.to_string(),
                kind: StorageKind::Cache,
                bytes: node_logs,
                cleanable: true,
            },
            StorageItem {
                id: ITEM_PACKAGES.to_string(),
                kind: StorageKind::Data,
                bytes: packages,
                cleanable: false,
            },
            StorageItem {
                id: ITEM_RUNTIME_UPLOADS.to_string(),
                kind: StorageKind::Cache,
                bytes: up,
                cleanable: true,
            },
            StorageItem {
                id: ITEM_RUNTIME_LOGS.to_string(),
                kind: StorageKind::Cache,
                bytes: lg,
                cleanable: true,
            },
            StorageItem {
                id: ITEM_RUNTIME_DATA.to_string(),
                kind: StorageKind::Data,
                bytes: dt,
                cleanable: false,
            },
        ],
    })
}

/// `POST /storage/cleanup` — forward to the Runtimes, clean the node's own logs.
pub async fn storage_cleanup(
    State(state): State<NodeHttpState>,
    Json(req): Json<CleanupRequest>,
) -> Result<Json<CleanupResult>, StatusCode> {
    if req.ids.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let home = state.config.home.clone();

    let mut own_logs = false;
    let mut runtime_ids: Vec<&'static str> = Vec::new();
    for id in &req.ids {
        match id.as_str() {
            ITEM_NODE_LOGS => own_logs = true,
            // Forwarded verbatim: the Runtime decides what its own TTL collects.
            ITEM_RUNTIME_UPLOADS => runtime_ids.push("uploads"),
            ITEM_RUNTIME_LOGS => runtime_ids.push("logs"),
            ITEM_PACKAGES | ITEM_RUNTIME_DATA => return Err(StatusCode::CONFLICT),
            other => {
                tracing::warn!(category = other, known = ?KNOWN_ITEMS, "unknown storage category");
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    let mut freed_bytes = 0u64;

    if own_logs {
        freed_bytes += purge_files_older_than(&home.join("logs"), LOG_RETENTION_SECS).await;
        freed_bytes +=
            purge_files_older_than(&home.join("runtime-logs"), LOG_RETENTION_SECS).await;
    }

    if !runtime_ids.is_empty() {
        let client = reqwest::Client::new();
        for port in runtime_ports(&state).await {
            for id in &runtime_ids {
                freed_bytes += forward_cleanup(&client, port, id).await;
            }
        }
    }

    tracing::info!(freed_bytes, ids = ?req.ids, "node storage cleanup ran");
    Ok(Json(CleanupResult { freed_bytes }))
}

async fn fetch_report(client: &reqwest::Client, port: u16) -> Option<StorageReport> {
    let url = format!("http://127.0.0.1:{port}/internal/storage-report");
    match client.get(&url).timeout(RUNTIME_TIMEOUT).send().await {
        Ok(resp) if resp.status().is_success() => resp.json::<StorageReport>().await.ok(),
        Ok(resp) => {
            tracing::warn!(port, status = %resp.status(), "runtime storage report refused");
            None
        }
        Err(e) => {
            tracing::warn!(port, error = %e, "runtime storage report unreachable");
            None
        }
    }
}

async fn forward_cleanup(client: &reqwest::Client, port: u16, id: &str) -> u64 {
    let url = format!("http://127.0.0.1:{port}/internal/storage-cleanup");
    let body = CleanupRequest {
        ids: vec![id.to_string()],
    };
    match client
        .post(&url)
        .timeout(RUNTIME_TIMEOUT)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp
            .json::<CleanupResult>()
            .await
            .map(|r| r.freed_bytes)
            .unwrap_or(0),
        Ok(resp) => {
            tracing::warn!(port, id, status = %resp.status(), "runtime cleanup refused");
            0
        }
        Err(e) => {
            tracing::warn!(port, id, error = %e, "runtime cleanup unreachable");
            0
        }
    }
}

/// Routes for the node HTTP listener (`:19900`).
pub fn router(state: NodeHttpState) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/storage/report", get(storage_report))
        .route("/storage/cleanup", post(storage_cleanup))
        .with_state(state)
}
