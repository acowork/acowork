//! Intent turn support (design doc 27 §5.4.5, §7.1).
//!
//! A turn triggered by an inbound Intent `request` puts the agent in the
//! **responder** role: `intent_send` / `ask_user_question` are hidden from
//! the tool set (R1), human-awaiting pause states are refused (R3), and the
//! turn's final output is written back to the Gateway as a `response` by
//! the *system* — never by a model tool call (§7.1).
//!
//! This module owns the pieces that are not loop mechanics: the context
//! struct, the user-visible rendering of an envelope, and the Gateway
//! write-back call.

use acowork_core::defaults::GATEWAY_HTTP_PORT;
use serde_json::Value;

/// The request a responder turn must answer. Built when a `request`
/// IntentMessage starts a turn; consumed at turn end for the write-back.
#[derive(Debug, Clone)]
pub struct IntentTurnContext {
    /// The request's stable message id — the write-back coordinate.
    pub request_id: String,
    pub conversation_id: String,
    pub source_chat_session_id: String,
    /// Sender instance (the response is addressed back to it).
    pub from: String,
    pub action: String,
    /// §7.1 outcome, mutated by the loop when a human-awaiting pause is
    /// refused. Default "completed".
    pub outcome: String,
}

impl IntentTurnContext {
    pub fn new(
        request_id: &str,
        conversation_id: &str,
        source_chat_session_id: &str,
        from: &str,
        action: &str,
    ) -> Self {
        Self {
            request_id: request_id.to_string(),
            conversation_id: conversation_id.to_string(),
            source_chat_session_id: source_chat_session_id.to_string(),
            from: from.to_string(),
            action: action.to_string(),
            outcome: "completed".to_string(),
        }
    }
}

/// Render an Intent envelope as the user-role message the receiving model
/// sees. Shared by the mid-turn injection path (`loop_inbound`) and the
/// new-turn path (`session_task`) so both renderings stay identical.
#[allow(clippy::too_many_arguments)]
pub fn render_intent_message(
    from: &str,
    action: &str,
    params: &Value,
    message_id: &str,
    conversation_id: &str,
    source_chat_session_id: &str,
    repo: &str,
    kind: &str,
    in_reply_to: &str,
    status: &str,
) -> String {
    // Design 27 §3.2: the receiver sees the full envelope — reply
    // coordinates (conversation / source sessions) and the authorized repo
    // are part of the message, not ambient.
    let mut rendered = if kind == "response" || kind == "intent_response" {
        format!(
            "[intent_response:{from}:{action}] {params}
(in_reply_to: {in_reply_to}, status: {})",
            if status.is_empty() { "completed" } else { status }
        )
    } else {
        format!("[intent:{from}:{action}] {params}")
    };
    if !message_id.is_empty() {
        rendered.push_str(&format!("\n(message_id: {message_id})"));
    }
    if !conversation_id.is_empty() {
        rendered.push_str(&format!("\n(conversation_id: {conversation_id})"));
    }
    if !source_chat_session_id.is_empty() {
        rendered.push_str(&format!(
            "\n(source_chat_session_id: {source_chat_session_id})"
        ));
    }
    if !repo.is_empty() {
        rendered.push_str(&format!("\n(repo: {repo})"));
    }
    rendered
}

/// POST the responder's final output back through the Gateway write entry
/// (`POST /api/intents/{request_id}/response`). The Gateway verifies the
/// caller is the recorded target, appends the `intent_response` chat row,
/// and pushes the response to the initiator (§7.1).
///
/// Best-effort by contract: a failed write-back is logged by the caller;
/// the responder's turn is already over and must not re-run.
pub async fn post_intent_response(
    instance_id: &str,
    gateway_host: &str,
    node_token: &str,
    session_id: &str,
    ctx: &IntentTurnContext,
    body: &str,
) -> Result<(), String> {
    let url = format!(
        "http://{gateway_host}:{GATEWAY_HTTP_PORT}/api/intents/{}/response",
        ctx.request_id
    );
    let payload = serde_json::json!({
        "sender": instance_id,
        "sender_session_id": session_id,
        "target": ctx.from,
        "action": ctx.action,
        "params": { "message": body },
        "kind": "response",
        "in_reply_to": ctx.request_id,
        "status": ctx.outcome,
    });
    let mut req = reqwest::Client::new().post(&url).json(&payload);
    if !node_token.is_empty() {
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, node_token);
    }
    let resp = req.send().await.map_err(|e| format!("gateway write-back failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("gateway rejected write-back (HTTP {status}): {body}"))
    }
}

/// Truncate a response body to the byte ceiling with an explicit marker
/// (design 27 §5.5.1 length bound; the initiator asks for more with a new
/// request rather than the wire carrying an unbounded turn).
pub fn truncate_response_body(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }
    // Cut on a char boundary.
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_carries_envelope_coordinates() {
        let s = render_intent_message(
            "inst-a",
            "query",
            &serde_json::json!({"message": "hi"}),
            "m1",
            "c1",
            "cs1",
            "repo1",
            "request",
            "",
            "",
        );
        assert!(s.contains("[intent:inst-a:query]"));
        assert!(s.contains("(message_id: m1)"));
        assert!(s.contains("(conversation_id: c1)"));
        assert!(s.contains("(source_chat_session_id: cs1)"));
        assert!(s.contains("(repo: repo1)"));
    }

    #[test]
    fn render_response_variant() {
        let s = render_intent_message(
            "inst-b",
            "query",
            &serde_json::json!("answer"),
            "m2",
            "c1",
            "",
            "",
            "response",
            "m1",
            "completed",
        );
        assert!(s.contains("[intent_response:inst-b:query]"));
        assert!(s.contains("in_reply_to: m1"));
        assert!(!s.contains("source_chat_session_id"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "héllo wörld";
        let t = truncate_response_body(s, 4);
        assert!(t.ends_with("[truncated]"));
        assert!(s.starts_with(&t[..t.len() - "\n[truncated]".len()]));
        assert_eq!(truncate_response_body(s, 100), s);
    }
}
