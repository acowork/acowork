//! Gateway client (design doc 27 Phase 2/3).
//!
//! Thin HTTP client the PM service uses to reach the Gateway's intent
//! surface: project group chat provisioning (`POST /api/intents/project-group`,
//! task dispatch (`POST /api/intents/task-dispatch`) and logical-context
//! close (`POST /api/intents/task-close`), authenticated with the
//! internal-service token the supervisor injects via
//! `ACOWORK_PM_GATEWAY_TOKEN`. The chat store is the user service's, not
//! PM's — PM only calls back into the Gateway, which owns the chat domain
//! writes. Idempotent on `project_id` / task coordinates, so a retry
//! after a crash is harmless.
//!
//! Best-effort by design: a project without its group chat still works
//! (the chat is the message surface, not a task dependency); failures are
//! logged, never surfaced to the project-creation response.

use std::time::Duration;

use acowork_core::timeout_config::constants::{INTENT_ACCEPT, INTENT_DEFAULT};

/// Request budget for [`GatewayClient::dispatch_task`]. The Gateway path is
/// bounded by design at `wake_and_deliver` (up to `INTENT_DEFAULT` — a cold
/// target may spend it all in `wait_online`) plus the acceptance wait
/// (`INTENT_ACCEPT`): 35s before the answer. The client-side default (10s)
/// MUST NOT cut under that: aborting while the first publish is still
/// completing server-side makes the retry — same `message_id`, target
/// already routing — re-deliver the Intent, which is exactly the failure
/// the receipt protocol exists to prevent. The margin covers connection
/// setup and response transfer.
fn dispatch_timeout() -> Duration {
    INTENT_DEFAULT + INTENT_ACCEPT + Duration::from_secs(5)
}

/// Talks to the Gateway's project-group endpoint.
#[derive(Clone)]
pub struct GatewayClient {
    gateway_url: String,
    auth_token: Option<String>,
    client: reqwest::Client,
}

impl GatewayClient {
    /// `gateway_url` is the Gateway HTTP base (no `/api` suffix).
    pub fn new(gateway_url: String, auth_token: Option<String>) -> Self {
        Self {
            gateway_url: gateway_url.trim_end_matches('/').to_string(),
            auth_token,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client build"),
        }
    }

    /// Ensure the unique group chat for `project_id` with `participants`
    /// (the project owner at minimum; re-calling with more participants
    /// adds them to the same session). Returns the chat session id when
    /// the Gateway answered; `None` on any failure (logged).
    ///
    /// `owner` is `Project::owner()` (`owner_id ?? created_by`). The chat
    /// domain stores it as the group's ADR-098 governor — the same id that
    /// may write the group's `chat.md` — so every ensure re-asserts it and a
    /// transfer or a missed sync cannot leave the chat pointing at the old
    /// owner.
    pub async fn ensure_group(
        &self,
        project_id: &str,
        title: &str,
        participants: &[String],
        owner: &str,
    ) -> Option<String> {
        let mut req = self.client.post(format!(
            "{}/api/intents/project-group",
            self.gateway_url
        ));
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = match req
            .json(&serde_json::json!({
                "project_id": project_id,
                "title": title,
                "participants": participants,
                "owner": owner,
            }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, project_id, "project group chat provisioning unreachable");
                return None;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                project_id,
                %status,
                body = %body,
                "project group chat provisioning rejected by Gateway"
            );
            return None;
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, project_id, "project group chat response not JSON");
                return None;
            }
        };
        let sid = body
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        tracing::info!(project_id, session_id = ?sid, "project group chat ensured");
        sid
    }

    /// Design 27 §8.3: re-sync the project group's participant set after a
    /// member joins or is removed. Best-effort like provisioning — the PM
    /// store is the source of truth and the next sync converges; a failure
    /// is logged, never surfaced. `owner` (`Project::owner()`) travels with
    /// the roster so the group's ADR-098 governor — who may write its
    /// `chat.md` — follows the project on the same call, including a
    /// transfer that leaves the roster intact. `title` travels for the same
    /// reason: a project channel has no user-side rename, so this is the only
    /// path by which a project rename reaches the group's name.
    pub async fn sync_group(&self, project_id: &str, title: &str, participants: &[String], owner: &str) {
        let mut req = self.client.post(format!(
            "{}/api/intents/project-group/sync",
            self.gateway_url
        ));
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = match req
            .json(&serde_json::json!({
                "project_id": project_id,
                "title": title,
                "participants": participants,
                "owner": owner,
            }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, project_id, "project group participant sync unreachable");
                return;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                project_id,
                %status,
                body = %body,
                "project group participant sync rejected by Gateway"
            );
            return;
        }
        tracing::info!(project_id, "project group participants synced");
    }

    /// Tell the chat domain that the project was archived, so its group chat
    /// is now deletable by its owner (design 27 §4.4). The chat channel is
    /// **not** deleted here — the history stays until a human removes it, and
    /// only a project with nothing left to re-create the channel may be
    /// deleted at all; that precondition lives in the user service and this
    /// push is what satisfies it.
    ///
    /// Returns whether the Gateway accepted the stamp. The caller retries on
    /// `false` ([`crate::api::projects::mark_group_deleted`]): unlike
    /// provisioning, a lost stamp is not a missing convenience but a hard
    /// gate — the group chat stays undeletable for every account (the
    /// project-alive check is identity-blind), and no live project will
    /// ever push the mark again.
    pub async fn mark_group_deleted(&self, project_id: &str) -> bool {
        let mut req = self.client.post(format!(
            "{}/api/intents/project-group/deleted",
            self.gateway_url
        ));
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = match req
            .json(&serde_json::json!({ "project_id": project_id }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, project_id, "project group delete-mark unreachable");
                return false;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                project_id,
                %status,
                body = %body,
                "project group delete-mark rejected by Gateway"
            );
            return false;
        }
        tracing::info!(project_id, "project group marked deletable");
        true
    }

    /// Dispatch a PM task as a group-chat Intent (design 27 §5.4.2 / §9):
    /// the Gateway keys the execution context on
    /// `task:{project_id}:{task_id}`, writes the `intent_request` row into
    /// the project group chat, injects the repo server-side, and wakes the
    /// target agent. Best-effort like provisioning — dispatch failures are
    /// logged, never surfaced (the agent can still claim/poll the task the
    /// traditional way; the push is an accelerator).
    ///
    /// A **human** assignee goes through this same call: the Gateway sees an
    /// account id in the group roster, writes the row and returns
    /// `delivered` without waking anything (an account has no Runtime). PM
    /// does not branch — one endpoint, one receipt.
    ///
    /// Returns `Ok(message_id)` on delivery, `Err(reason)` otherwise —
    /// the reason feeds the scanner's give-up log; each failed attempt is
    /// already `warn`-logged here with the Gateway's full body.
    ///
    /// `message_id` is the caller's dispatch-generation identity (see
    /// `Task::dispatch_message_id`): replaying the same id on every retry
    /// makes the Gateway dedupe the chat row and the mailbox budget on it,
    /// so a failing delivery can neither pile up rows nor burn the
    /// conversation's request budget. An empty string lets the Gateway
    /// mint one (pre-idempotency behavior).
    ///
    /// Carries its own request timeout ([`dispatch_timeout`]): this call
    /// blocks on the target's wake *and* its acceptance receipt, so the
    /// client default would abort a still-healthy dispatch and provoke a
    /// duplicate delivery on retry.
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_task(
        &self,
        project_id: &str,
        task_id: &str,
        target_agent: &str,
        owner: &str,
        project_owner: &str,
        message: &str,
        assignment_brief: &str,
        message_id: &str,
    ) -> Result<String, String> {
        let mut req = self
            .client
            .post(format!("{}/api/intents/task-dispatch", self.gateway_url))
            .timeout(dispatch_timeout());
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = match req
            .json(&serde_json::json!({
                "project_id": project_id,
                "task_id": task_id,
                "target_agent": target_agent,
                "owner": owner,
                // ADR-076 §决策 4 (rev.): `Project::owner()` (a human), used
                // by the Gateway as the execution session's initiator so the
                // project owner can write into it. Kept separate from
                // `owner` (the task creator) above, which drives the Chat
                // row's voice and the target's `intent:receive` check.
                "project_owner": project_owner,
                "message": message,
                "assignment_brief": assignment_brief,
                "message_id": message_id,
            }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, task_id, "task dispatch unreachable");
                return Err(format!("unreachable: {e}"));
            }
        };
        let status = resp.status();
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, task_id, %status, "task dispatch response not JSON");
                return Err(format!("bad response ({status}): {e}"));
            }
        };
        if !status.is_success() || body.get("status").and_then(|v| v.as_str()) != Some("delivered") {
            tracing::warn!(
                task_id,
                %status,
                body = %body,
                "task dispatch rejected or undelivered by Gateway"
            );
            let code = body
                .get("error_code")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            // The Gateway relays the receiving Runtime's own refusal text
            // (`intent_rejected` carries e.g. `no such column: intent_key`).
            // It is the only description of the breakage the operator will
            // ever see — it lands in `task.dispatch_error` — so it is kept,
            // not collapsed into the code.
            let detail = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
            return Err(if detail.is_empty() {
                format!("gateway rejected ({status}): {code}")
            } else {
                format!("gateway rejected ({status}): {code}: {detail}")
            });
        }
        let mid = body
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or(message_id)
            .to_string();
        tracing::info!(task_id, target = target_agent, message_id = %mid, "task dispatched to project group chat");
        Ok(mid)
    }

    /// Design 27 §2.1 T2-a: a task reached a terminal state, so the target
    /// agent's logical execution context for it ends. Best-effort: the
    /// Runtime self-heals dead mappings on the next dispatch, so a failed
    /// close only leaves the session mapped until that next dispatch.
    pub async fn close_task_context(&self, project_id: &str, task_id: &str, target_agent: &str) {
        let mut req = self.client.post(format!(
            "{}/api/intents/task-close",
            self.gateway_url
        ));
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        match req
            .json(&serde_json::json!({
                "project_id": project_id,
                "task_id": task_id,
                "target_agent": target_agent,
            }))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                tracing::info!(task_id, target = target_agent, "task execution context closed");
            }
            Ok(r) => {
                tracing::warn!(task_id, status = %r.status(), "task context close rejected by Gateway");
            }
            Err(e) => {
                tracing::warn!(error = %e, task_id, "task context close unreachable");
            }
        }
    }

    /// Plan §7 G4: validate `pm_submit_task`'s `doc_refs` against the doc
    /// service, through the Gateway's doc reverse proxy
    /// (`POST /api/doc/api/refs/validate`). The doc side owns the fact of
    /// whether a reference resolves; PM only asks and refuses.
    ///
    /// Returns `Err(reason)` for a transport / non-2xx answer (the caller
    /// must refuse the submission — an unvalidated ref is a dead link by
    /// default), and `Ok(outcomes)` on a well-formed response. The caller
    /// inspects `status` per ref: only `"found"` may be submitted.
    pub async fn validate_doc_refs(&self, refs: &[String]) -> Result<Vec<serde_json::Value>, String> {
        let mut req = self.client.post(format!(
            "{}/api/doc/api/refs/validate",
            self.gateway_url
        ));
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = match req.json(&serde_json::json!({ "refs": refs })).send().await {
            Ok(r) => r,
            Err(e) => return Err(format!("doc service unreachable: {e}")),
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("doc validation rejected: {status} {body}"));
        }
        let value: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Err(format!("doc validation malformed response: {e}")),
        };
        let results = value["results"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        // One outcome per input ref, in order — the contract of the
        // endpoint. A shorter array means the doc side broke it; refuse
        // rather than silently approving the refs that went unreported.
        if results.len() != refs.len() {
            return Err(format!(
                "doc validation returned {} outcomes for {} refs",
                results.len(),
                refs.len()
            ));
        }
        Ok(results)
    }
}
