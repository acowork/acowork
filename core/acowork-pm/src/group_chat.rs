//! Project group chat provisioning (design doc 27 Phase 2).
//!
//! "PM 创建项目时幂等创建唯一项目群聊" — the chat store is the user
//! service's, not PM's. PM calls back into the Gateway
//! (`POST /api/intents/project-group`, authenticated with the
//! internal-service token the supervisor injects via
//! `ACOWORK_PM_GATEWAY_TOKEN`), and the Gateway ensures the session in
//! the chat domain. Idempotent on `project_id`, so a retry after a crash
//! is harmless.
//!
//! Best-effort by design: a project without its group chat still works
//! (the chat is the message surface, not a task dependency); failures are
//! logged, never surfaced to the project-creation response.

use std::time::Duration;

/// Talks to the Gateway's project-group endpoint.
#[derive(Clone)]
pub struct GroupChatProvisioner {
    gateway_url: String,
    auth_token: Option<String>,
    client: reqwest::Client,
}

impl GroupChatProvisioner {
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
}
