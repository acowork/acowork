//! Gateway → acowork-user Chat-session client (design 27 §2.2, §6.3).
//!
//! The Chat session store lives in the user service; the Gateway is its
//! only system writer — Intent request / response rows are appended here
//! from the delivery pipeline, never by a Runtime or a chat client. The
//! calls ride the same loopback internal surface as
//! [`crate::lifecycle::user_profile_sync`] (no identity: the caller is the
//! Gateway process on this host; `message_id` idempotency bounds replay).
//!
//! Every method degrades to `Err(String)`: the caller decides whether a
//! chat-log write failure blocks delivery. Per design 27 §6.3 the Intent
//! itself is the chat row — a session that cannot be written means the
//! message has no durable address, so the delivery entry treats append
//! failures on the request path as fatal-to-delivery, and pair-ensure
//! failures as "deliver without a chat binding" (one-off cross-agent
//! messages keep working when the user service is down).

use serde::{Deserialize, Serialize};

/// The subset of `ChatSession` the Gateway reads. Unknown fields ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub participants: Vec<String>,
    #[serde(default)]
    pub repo: Option<String>,
}

/// The subset of `SessionMessage` the Gateway writes (system append).
/// `seq` / `ts` are assigned by the store.
#[derive(Debug, Clone, Serialize)]
pub struct AppendMessage {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub body: String,
    pub meta: serde_json::Value,
}

/// Resolve the user service's loopback port, or `None` when the service is
/// not up (local single-user mode without accounts included — the user
/// service is spawned whenever accounts are enabled; chat sessions exist
/// only there).
pub async fn service_port(
    state: &tokio::sync::RwLock<crate::gateway::state::GatewayState>,
) -> Option<u16> {
    let gw = state.read().await;
    gw.user_process
        .as_ref()
        .filter(|p| p.ready)
        .map(|p| p.port)
}

fn client() -> reqwest::Client {
    // No global timeout here: the delivery path already bounds itself with
    // the intent wake deadline.
    reqwest::Client::new()
}

/// `POST /internal/chat-sessions/pair` — idempotently ensure the shared
/// agent-agent Chat session between two instances (design 27 §6.3).
pub async fn ensure_pair(port: u16, a: &str, b: &str) -> Result<SessionInfo, String> {
    let resp = client()
        .post(format!("http://127.0.0.1:{port}/internal/chat-sessions/pair"))
        .json(&serde_json::json!({ "a": a, "b": b, "kind": "agent_agent" }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

/// `POST /internal/chat-sessions/group` — idempotently ensure the project
/// group chat (design 27 Phase 2). Called by the Gateway when PM creates a
/// project, and when a member joins (participant add).
pub async fn ensure_group(
    port: u16,
    project_id: &str,
    title: &str,
    participants: &[String],
) -> Result<SessionInfo, String> {
    let resp = client()
        .post(format!("http://127.0.0.1:{port}/internal/chat-sessions/group"))
        .json(&serde_json::json!({
            "project_id": project_id,
            "title": title,
            "participants": participants,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

/// `POST /internal/chat-sessions/messages` — append one system row.
/// The store dedups on `meta.message_id` (§8), so a retried delivery
/// never double-writes the chat log.
pub async fn append_message(
    port: u16,
    session_id: &str,
    msg: &AppendMessage,
) -> Result<(), String> {
    let resp = client()
        .post(format!(
            "http://127.0.0.1:{port}/internal/chat-sessions/messages"
        ))
        .json(&serde_json::json!({
            "session_id": session_id,
            "message": {
                "seq": 0,
                "ts": 0,
                "from": msg.from,
                "kind": msg.kind,
                "body": msg.body,
                "to": msg.to,
                "meta": msg.meta,
            }
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("chat append {status}: {body}"))
    }
}

/// `GET /internal/chat-sessions/{id}` — session metadata (repo binding,
/// participants) for delivery-time injection and authorization.
pub async fn load_session(port: u16, session_id: &str) -> Result<SessionInfo, String> {
    let resp = client()
        .get(format!(
            "http://127.0.0.1:{port}/internal/chat-sessions/{session_id}"
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

/// `GET /internal/chat-sessions/{id}/messages?who=` — the read path behind
/// the `chat_history_read` tool (design 27 §9). Returns the raw page JSON;
/// the tool layer renders it.
pub async fn read_messages(
    port: u16,
    session_id: &str,
    who: &str,
    offset: usize,
    limit: usize,
) -> Result<serde_json::Value, String> {
    let resp = client()
        .get(format!(
            "http://127.0.0.1:{port}/internal/chat-sessions/{session_id}/messages"
        ))
        .query(&[("who", who), ("offset", &offset.to_string()), ("limit", &limit.to_string())])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

async fn read_json<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T, String> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("chat service {status}: {body}"));
    }
    resp.json::<T>().await.map_err(|e| e.to_string())
}
