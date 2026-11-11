//! Best-effort receipt push to the Gateway (plan §7 G5).
//!
//! A `doc_write` that parked as `pending_review` handed the agent a
//! `request_id` and then went silent: the review landing produced only
//! `acowork/doc/tree/changed`, a UI event no agent subscribes to. The
//! Gateway's `/api/intents/doc-review` route turns the durable review fact
//! into one intent wake at the proposer's instance; this module is the
//! doc-side caller.
//!
//! The push is **fire-and-forget by contract**: the review decision is
//! already durable in the request record when this runs, so nothing here
//! may fail the approve/reject response — a Gateway that is restarting must
//! not make a human's completed review look like an error. The failure
//! surfaces as a `warn` log line; the agent-side fallback (T2: the
//! `request_id` recorded in the task activity, polled with
//! `doc_check_request`) still converges.
//!
//! Unconfigured gateway (standalone doc without `--gateway-url`) is not an
//! error — there is nobody to push to and no agent could have reached this
//! service over MCP without the Gateway in the loop anyway.

use crate::state::DocState;
use crate::types::UpdateRequest;

/// POST the review receipt. Spawns the HTTP call and returns immediately;
/// every failure mode is a `warn` line, never a `Result`.
pub fn spawn_review_receipt(state: &DocState, req: &UpdateRequest, result: &'static str) {
    let Some(gateway) = state.gateway.clone() else {
        return;
    };
    // A human-authored write never enters the review queue, so a request
    // without an agent-addressable proposer has nothing to notify.
    if req.submitted_by.trim().is_empty() {
        return;
    }
    let request_id = req.request_id.clone();
    let doc_id = req.doc_id.clone();
    let path = req.path.clone();
    let submitted_by = req.submitted_by.clone();
    let reviewed_by = req.reviewed_by.clone().unwrap_or_default();
    let review_note = req.review_note.clone();

    tokio::spawn(async move {
        let body = serde_json::json!({
            "request_id": request_id,
            "doc_id": doc_id,
            "path": path,
            "result": result,
            "submitted_by": submitted_by,
            "reviewed_by": reviewed_by,
            "review_note": review_note,
        });
        let mut call = gateway
            .client
            .post(format!("{}/api/intents/doc-review", gateway.url))
            .timeout(std::time::Duration::from_secs(10))
            .json(&body);
        if let Some(token) = &gateway.token {
            call = call.bearer_auth(token);
        }
        match call.send().await {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    tracing::warn!(
                        %status,
                        "doc review receipt rejected by gateway (agent must poll doc_check_request)"
                    );
                    return;
                }
                // `skipped` is a fine outcome (proposer uninstalled); log
                // it at info so the operator can trace a silent receipt.
                match resp.json::<serde_json::Value>().await {
                    Ok(v) => tracing::info!(
                        request_id = %body["request_id"],
                        gateway_status = %v,
                        "doc review receipt pushed"
                    ),
                    Err(e) => tracing::warn!(error = %e, "doc review receipt response not JSON"),
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    request_id = %body["request_id"],
                    "doc review receipt push unreachable (agent must poll doc_check_request)"
                );
            }
        }
    });
}
