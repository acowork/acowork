//! Gateway → acowork-user Chat-session client (design 27 §4.2, §6.3).
//!
//! The Chat session store lives in the user service; the Gateway is its
//! only system writer — Intent request / response rows are appended here
//! from the delivery pipeline, never by a Runtime or a chat client. The
//! calls ride the same loopback internal surface as
//! [`crate::lifecycle::user_profile_sync`] (no identity: the caller is the
//! Gateway process on this host; `message_id` idempotency bounds replay).
//!
//! Every method degrades to `Err(String)`: the caller decides whether a
//! chat-log write failure blocks delivery. Per design 27 §5.4.3 the Intent
//! itself is the chat row — a session that cannot be written means the
//! message has no durable address, so the delivery entry treats append
//! failures on the request path as fatal-to-delivery, and pair-ensure
//! failures as "deliver without a chat binding" (one-off cross-agent
//! messages keep working when the user service is down).

use acowork_core::protocol::ChatAttachmentMeta;
use serde::{Deserialize, Serialize};

/// A session participant as the user service returns it: a bare id
/// (rows written before design 27 §8.1) or `{id, identity}`. The Gateway
/// only needs membership, never the identity type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParticipantRef {
    Legacy(String),
    Typed {
        id: String,
        /// `"human"` | `"agent"` (user service `ParticipantIdentity`);
        /// absent on rows written before §8.1 typing.
        #[serde(default)]
        identity: Option<String>,
    },
}

impl ParticipantRef {
    pub fn id(&self) -> &str {
        match self {
            ParticipantRef::Legacy(s) => s,
            ParticipantRef::Typed { id, .. } => id,
        }
    }
}

impl SessionInfo {
    /// Design 27 §5.5.4 / plan 2b.3: whether this Chat session has a human
    /// participant who could answer a decision card. `None` = undecidable
    /// (legacy untyped rows, or the session was never loaded) — callers
    /// fail open and keep human-facing tools.
    pub fn has_human_participant(&self) -> Option<bool> {
        if self.participants.is_empty() {
            return None;
        }
        let mut typed_any = false;
        for p in &self.participants {
            match p {
                ParticipantRef::Legacy(_) => return None,
                ParticipantRef::Typed { identity, .. } => {
                    typed_any = true;
                    if identity.as_deref() == Some("human") {
                        return Some(true);
                    }
                }
            }
        }
        typed_any.then_some(false)
    }
}

/// The subset of `ChatSession` the Gateway reads. Unknown fields ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub participants: Vec<ParticipantRef>,
    #[serde(default)]
    pub repo: Option<String>,
    /// A group session's `chat.md` chat-prompt override (ADR-099, extended),
    /// forwarded to the Runtime so the per-chat prompt reaches the chat
    /// router. Absent for 1:1 kinds and for a group that never had one
    /// seeded.
    #[serde(default)]
    pub chat_prompt: Option<String>,
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
    /// Ids of attachments already uploaded to this session (design 27
    /// §4.2.1). The store re-reads the metadata from disk — ids are the
    /// only client-supplied attachment fact.
    #[serde(default)]
    pub attachment_ids: Vec<String>,
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
/// one-to-one Chat session between two participants (design 27 §5.4.3).
/// The caller declares the session kind: `agent_agent` for the Intent
/// channel between two instances, `user_agent` for a user's channel to
/// one agent (the user service resolves who is human from its accounts).
pub async fn ensure_pair(port: u16, a: &str, b: &str, kind: &str) -> Result<SessionInfo, String> {
    let resp = client()
        .post(format!("http://127.0.0.1:{port}/internal/chat-sessions/pair"))
        .json(&serde_json::json!({ "a": a, "b": b, "kind": kind }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

/// `POST /internal/chat-sessions/group` — idempotently ensure the project
/// group chat (design 27 Phase 2). Called by the Gateway when PM creates a
/// project, and when a member joins (participant add). `owner` is the
/// project's current owner as PM reports it — the user service converges it
/// onto the session row, because ADR-098 governance of the group's `chat.md`
/// checks it (`None` = the caller does not know it; leave the row alone).
pub async fn ensure_group(
    port: u16,
    project_id: &str,
    title: &str,
    participants: &[String],
    owner: Option<&str>,
) -> Result<SessionInfo, String> {
    let resp = client()
        .post(format!("http://127.0.0.1:{port}/internal/chat-sessions/group"))
        .json(&serde_json::json!({
            "project_id": project_id,
            "title": title,
            "participants": participants,
            "owner": owner,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

/// `POST /internal/chat-sessions/messages` — append one system row.
/// The store dedups on `meta.message_id` (§8), so a retried delivery
/// never double-writes the chat log.
///
/// Returns the **stored** attachment metadata, which is the only trustworthy
/// description of what the user attached: the store re-reads name / mime /
/// size from the session's `files/` dir, so the values do not come from a
/// client body. The caller forwards them to the Runtime so its Chat Router can
/// see this turn's attachments (a row already in the transcript gets the same
/// metadata from the history projection).
///
/// An unparseable response body yields an empty list rather than an error: a
/// delivery that carries the message without attachment *labels* is a worse
/// route, not a lost message.
pub async fn append_message(
    port: u16,
    session_id: &str,
    msg: &AppendMessage,
) -> Result<Vec<ChatAttachmentMeta>, String> {
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
                "attachment_ids": msg.attachment_ids,
            }
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("chat append {status}: {body}"));
    }
    let stored: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "chat append returned an unreadable body — delivering without attachment metadata");
            return Ok(Vec::new());
        }
    };
    Ok(attachment_meta_from_row(&stored))
}

/// Project a stored row's `attachments` onto the shared router-side shape.
/// Unknown fields (`id`, `thumb`) are ignored; a missing array means no
/// attachments, which is also the answer for every non-speech row.
fn attachment_meta_from_row(row: &serde_json::Value) -> Vec<ChatAttachmentMeta> {
    row.get("attachments")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    serde_json::from_value::<ChatAttachmentMeta>(item.clone()).ok()
                })
                .collect()
        })
        .unwrap_or_default()
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

/// `GET /internal/chat-sessions/{sid}/files/{aid}/{mid}/{target}` streams an
/// attachment only when the durable user_message row addressed to `target`
/// pins that attachment id. The Gateway and Runtime never buffer file bytes.
pub async fn download_message_attachment(
    port: u16,
    session_id: &str,
    attachment_id: &str,
    message_id: &str,
    target: &str,
) -> Result<reqwest::Response, String> {
    let resp = client().get(format!(
        "http://127.0.0.1:{port}/internal/chat-sessions/{session_id}/files/{attachment_id}/{message_id}/{target}"
    )).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("chat attachment download {}", resp.status()));
    }
    Ok(resp)
}

/// `GET /internal/chat-sessions/{id}/messages/by-message-id/{mid}?who=` —
/// single-row fetch for the §5.5.4 answer channel: the Gateway resolves
/// the `interaction_request` card a human's `in_reply_to` points at before
/// routing the answer to the execution side. `Ok(None)` when no row carries
/// the id (a quote of a deleted/unknown message — deliver as plain text).
pub async fn message_by_id(
    port: u16,
    session_id: &str,
    who: &str,
    message_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    let resp = client()
        .get(format!(
            "http://127.0.0.1:{port}/internal/chat-sessions/{session_id}/messages/by-message-id/{message_id}"
        ))
        .query(&[("who", who)])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("chat message fetch {status}: {body}"));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(if v.is_null() { None } else { Some(v) })
}

/// `GET /internal/chat-sessions/{id}/messages?who=` — the read path behind
/// the `chat_history_read` tool (design 27 §5.6). Returns the raw page JSON;
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

/// `GET /internal/chat-sessions?participant=<id>` — the Chat session ids
/// the participant belongs to. The auth middleware injects the result as
/// `x-acowork-chat-scope` on agent-proxy requests so the Runtime can
/// resolve `Members` visibility (design 27 §8.2). An error here is
/// deliberately swallowed by the caller: an absent scope means only that
/// no `Members` session is readable through the chat path — the owner
/// path is unaffected, and the request itself must not fail.
pub async fn list_session_ids(port: u16, participant: &str) -> Result<Vec<String>, String> {
    let resp = client()
        .get(format!("http://127.0.0.1:{port}/internal/chat-sessions"))
        .query(&[("participant", participant)])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

/// `POST /internal/chat-sessions/{id}/participants` — add one participant
/// to a Chat session (design 27 §8.3, the join half). Idempotent.
pub async fn add_participant(
    port: u16,
    session_id: &str,
    id: &str,
    identity: Option<&str>,
) -> Result<SessionInfo, String> {
    let resp = client()
        .post(format!(
            "http://127.0.0.1:{port}/internal/chat-sessions/{session_id}/participants"
        ))
        .json(&serde_json::json!({ "id": id, "identity": identity }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

/// `DELETE /internal/chat-sessions/{id}/participants/{pid}` — remove one
/// participant (design 27 §8.3, the revoke half). The removed account
/// loses access to every execution session bound to that Chat session on
/// its next scope injection.
pub async fn remove_participant(
    port: u16,
    session_id: &str,
    participant_id: &str,
) -> Result<SessionInfo, String> {
    let resp = client()
        .delete(format!(
            "http://127.0.0.1:{port}/internal/chat-sessions/{session_id}/participants/{participant_id}"
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

/// `POST /internal/chat-sessions/group/sync` — set a project group's
/// participant list to exactly `participants` (design 27 §8.3). The PM
/// calls the Gateway on member add / remove; the Gateway mediates. `owner`
/// is the project's current owner as PM reports it, converged onto the
/// session row for the same call (ADR-098 governance of the group prompt),
/// and so is `title` — a project channel has no user-side rename, so PM's
/// title is the only source for the group's name.
pub async fn sync_group(
    port: u16,
    project_id: &str,
    participants: &[String],
    title: &str,
    owner: Option<&str>,
) -> Result<SessionInfo, String> {
    let resp = client()
        .post(format!("http://127.0.0.1:{port}/internal/chat-sessions/group/sync"))
        .json(&serde_json::json!({
            "project_id": project_id,
            "title": title,
            "participants": participants,
            "owner": owner,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    read_json(resp).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Both delivery entries (`chat-message`, `chat-mention`) forward whatever
    // `append_message` returned, so this projection is the single producer of
    // `params.chat_attachments` — and the Runtime's history projection reads
    // the same row through the same shape. Keeping the projection faithful is
    // what makes the current turn and the transcript read identically.

    #[test]
    fn the_stored_row_projects_name_mime_and_size_in_order() {
        // A real store row: it also carries `id` and `thumb`, which the router
        // neither needs nor may see (ADR-096 — metadata only, no payloads).
        let row = json!({
            "id": "m-1",
            "kind": "user_message",
            "body": "look at this",
            "attachments": [
                {"id": "att-1", "filename": "report.pdf", "mime": "application/pdf", "size": 12288, "thumb": "data:image/png;base64,AAAA"},
                {"id": "att-2", "filename": "shot.png", "mime": "image/png", "size": 348160},
            ],
        });

        assert_eq!(
            attachment_meta_from_row(&row),
            vec![
                ChatAttachmentMeta {
                    filename: "report.pdf".into(),
                    mime: "application/pdf".into(),
                    size: 12_288,
                },
                ChatAttachmentMeta {
                    filename: "shot.png".into(),
                    mime: "image/png".into(),
                    size: 348_160,
                },
            ]
        );
    }

    #[test]
    fn a_row_without_attachments_projects_nothing() {
        // Every non-speech row, and every turn from before attachments existed.
        assert!(attachment_meta_from_row(&json!({"id": "m-1", "body": "hi"})).is_empty());
        assert!(attachment_meta_from_row(&json!({"attachments": null})).is_empty());
        assert!(attachment_meta_from_row(&json!({"attachments": []})).is_empty());
        // Wrong container type: the store never writes this, but a degraded
        // store must cost the metadata, not the delivery.
        assert!(attachment_meta_from_row(&json!({"attachments": "att-1"})).is_empty());
    }

    #[test]
    fn an_unreadable_element_is_skipped_and_the_rest_survive() {
        let row = json!({
            "attachments": [
                {"filename": "good.png", "mime": "image/png", "size": 10},
                "not-an-object",
                {"filename": "also-good.pdf", "mime": "application/pdf", "size": 20},
            ],
        });
        let meta = attachment_meta_from_row(&row);
        assert_eq!(meta.len(), 2);
        assert_eq!(meta[0].filename, "good.png");
        assert_eq!(meta[1].filename, "also-good.pdf");
    }
}
