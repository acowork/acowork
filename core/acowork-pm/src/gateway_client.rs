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
    pub async fn ensure_group(
        &self,
        project_id: &str,
        title: &str,
        participants: &[String],
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
    /// is logged, never surfaced.
    pub async fn sync_group(&self, project_id: &str, participants: &[String]) {
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
                "participants": participants,
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

    /// Dispatch a PM task as a group-chat Intent (design 27 §5.4.2 / §9):
    /// the Gateway keys the execution context on
    /// `task:{project_id}:{task_id}`, writes the `intent_request` row into
    /// the project group chat, injects the repo server-side, and wakes the
    /// target agent. Best-effort like provisioning — dispatch failures are
    /// logged, never surfaced (the agent can still claim/poll the task the
    /// traditional way; the push is an accelerator).
    ///
    /// Returns the Gateway `message_id` on delivery.
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_task(
        &self,
        project_id: &str,
        task_id: &str,
        target_agent: &str,
        owner: &str,
        title: &str,
        message: &str,
        assignment_brief: &str,
    ) -> Option<String> {
        let mut req = self.client.post(format!(
            "{}/api/intents/task-dispatch",
            self.gateway_url
        ));
        if let Some(token) = &self.auth_token {
            req = req.bearer_auth(token);
        }
        let resp = match req
            .json(&serde_json::json!({
                "project_id": project_id,
                "task_id": task_id,
                "target_agent": target_agent,
                "owner": owner,
                "title": title,
                "message": message,
                "assignment_brief": assignment_brief,
            }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, task_id, "task dispatch unreachable");
                return None;
            }
        };
        let status = resp.status();
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, task_id, %status, "task dispatch response not JSON");
                return None;
            }
        };
        if !status.is_success() || body.get("status").and_then(|v| v.as_str()) != Some("delivered") {
            tracing::warn!(
                task_id,
                %status,
                body = %body,
                "task dispatch rejected or undelivered by Gateway"
            );
            return None;
        }
        let mid = body
            .get("message_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        tracing::info!(task_id, target = target_agent, message_id = ?mid, "task dispatched to project group chat");
        mid
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
}
