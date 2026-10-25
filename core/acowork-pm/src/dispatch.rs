//! Start-time-driven task dispatch scanner.
//!
//! Replaces the old event hooks in `api/tasks.rs` (create / review-approve)
//! that pushed an intent the moment a task became assignable. Those hooks
//! fired too early: a `pending` task with `start_at` in the future must not
//! wake its assignee yet, and a task whose assignee arrives later via PATCH
//! was never dispatched at all (the Desktop create flow is create-then-PATCH).
//!
//! New single trigger source: a periodic scan. Every tick the scanner takes
//! the in-memory index (no disk reads for filtering), selects tasks that are
//! `pending` + agent-assigned + creation-approved + undispatched +
//! `start_at` reached, and pushes a wake-up intent per task. On delivery the
//! task is stamped `dispatched_at` (idempotence); the stamp is cleared
//! whenever the task leaves `pending` or is reassigned, so a rollback to
//! `pending` re-dispatches naturally.
//!
//! The lifecycle this closes:
//! create (pending, undispatched) → scanner wakes assignee at `start_at` →
//! agent `pm_claim_task` (pending → in_progress) → work → `pm_submit_task`
//! (→ submitted) → human review (→ done). The wake-up splits in two: the
//! human-readable task body rides the chat row / user message, the tool-call
//! playbook is the ephemeral `assignment_brief` (see
//! [`build_dispatch_message`] / [`build_assignment_brief`]).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::gateway_client::GatewayClient;
use crate::store::tree::{PmStore, TreePmStore};
use crate::types::Task;

/// The human-readable dispatch text — what the project group chat shows
/// and what the model sees as the user message (design 27 §5.4.4 for the
/// rendering rule; the assignment playbook is a separate field, see
/// [`build_assignment_brief`]).
pub fn build_dispatch_message(task: &Task) -> String {
    if task.description.trim().is_empty() {
        task.title.clone()
    } else {
        format!("{}\n{}", task.title, task.description.trim())
    }
}

/// The wake-up playbook, sent as the envelope's `assignment_brief` and
/// injected into the model's system block — never persisted, never
/// displayed on a chat surface. Four elements, in order:
/// 1. the `task_id` coordinate the tools require (`project_id` is
///    deliberately absent: claim/submit take only the task id, and
///    echoing it inside a project group is noise);
/// 2. the claim → work → submit playbook with exact tool names;
/// 3. the 409 branch (review gate / dependency gate) so a rejected claim
///    degrades to "report and stop" instead of a retry loop;
/// 4. delivery contract: results go through `pm_submit_task`, not chat text.
pub fn build_assignment_brief(task: &Task) -> String {
    let tid = task.id.as_str();
    format!(
        "You are being woken to execute PM task \"{tid}\". Follow this workflow exactly:\n\
         1. Claim it first: call the MCP tool `pm_claim_task` with task_id=\"{tid}\".\n\
            Until the claim succeeds the task is NOT yours and you must not start work.\n\
         2. Do the work in the repository attached to this conversation.\n\
         3. Deliver with the MCP tool `pm_submit_task` (task_id=\"{tid}\", text=<result summary>).\n\
            A chat reply alone is NOT a delivery; the human approves the submitted result to close the task.\n\
         \n\
         If `pm_claim_task` fails:\n\
         - 409 review_not_approved → the task still awaits human approval; say so in this chat and stop.\n\
         - 409 dependency_not_satisfied → a blocking task is unfinished; say so in this chat and stop.\n\
         Do not retry the claim in a loop."
    )
}

/// Spawn the dispatch scanner loop. No-op in standalone mode (no Gateway).
///
/// `interval` is the tick period (second-level is plenty — dispatch latency
/// tolerance is human-scale); `per_tick` caps intents pushed per tick so a
/// large due-backlog drains gradually instead of flooding the Gateway's
/// per-conversation request budget.
pub fn spawn_scanner(
    store: Arc<TreePmStore>,
    gateway: (String, Option<String>),
    interval: Duration,
    per_tick: usize,
) {
    let client = GatewayClient::new(gateway.0, gateway.1);
    tokio::spawn(async move {
        // Per-task retry backoff: a dispatch that the Gateway rejects (agent
        // uninstalled, budget exhausted, …) would otherwise be retried every
        // tick forever. Failed tasks are retried after 60s.
        let mut retry_after: std::collections::HashMap<crate::types::TaskId, std::time::Instant> =
            std::collections::HashMap::new();
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            let due = store.dispatch_due_ids(Utc::now());
            if due.is_empty() {
                retry_after.clear();
                continue;
            }
            let now = std::time::Instant::now();
            let mut dispatched = 0usize;
            for tid in due {
                if dispatched >= per_tick {
                    break;
                }
                if retry_after.get(&tid).is_some_and(|t| *t > now) {
                    continue;
                }
                // Re-read the full task for the message body; cheap because
                // the candidate set is only "due and undispatched".
                let Ok(Some(task)) = store.get_task(&tid).await else {
                    continue;
                };
                if task.status != crate::types::TaskStatus::Pending
                    || task.dispatched_at.is_some()
                {
                    continue; // raced with a claim/reassign between filter and read
                }
                let Some(assignee) = task.assignee.as_deref() else {
                    continue;
                };
                // Dependency gate: blocked tasks wait for their blocker's own
                // dispatch cycle; waking the agent now would only 409.
                if !store.compute_blocked_by(&tid).await.unwrap_or_default().is_empty() {
                    continue;
                }
                let message = build_dispatch_message(&task);
                let brief = build_assignment_brief(&task);
                let delivered = client
                    .dispatch_task(
                        task.project_id.as_str(),
                        task.id.as_str(),
                        assignee,
                        &task.created_by,
                        &task.title,
                        &message,
                        &brief,
                    )
                    .await;
                if delivered.is_some() {
                    retry_after.remove(&tid);
                    if let Err(e) = store.mark_dispatched(&tid).await {
                        tracing::warn!(task = %tid, error = %e, "failed to stamp dispatched_at; may re-dispatch");
                    }
                    dispatched += 1;
                } else {
                    retry_after.insert(tid.clone(), now + Duration::from_secs(60));
                }
            }
            if dispatched > 0 {
                tracing::info!(count = dispatched, "PM dispatch scanner pushed task intents");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task() -> Task {
        serde_json::from_value(serde_json::json!({
            "id": "t-test1",
            "project_id": "p-test1",
            "title": "Implement X",
            "description": "Do X in repo Y",
            "type": "task",
            "status": "pending",
            "review_status": "not_required",
            "priority": "normal",
            "assignee": "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d",
            "created_by": "human",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn human_text_is_the_task_body_alone() {
        // The group chat renders this verbatim — no playbook, no
        // coordinates (the chip carries the task id from row meta).
        let msg = build_dispatch_message(&sample_task());
        assert_eq!(msg, "Implement X\nDo X in repo Y");
        assert!(!msg.contains("pm_claim_task"), "playbook must not leak");
        assert!(!msg.contains("p-test1"), "project_id must be gone");
    }

    #[test]
    fn brief_carries_task_coordinate_and_playbook() {
        let brief = build_assignment_brief(&sample_task());
        assert!(brief.contains("t-test1"), "task_id must appear");
        assert!(brief.contains("pm_claim_task"), "claim step must appear");
        assert!(brief.contains("pm_submit_task"), "submit step must appear");
        assert!(brief.contains("review_not_approved"), "409 branch must appear");
        assert!(!brief.contains("p-test1"), "project_id is not needed to claim");
    }
}
