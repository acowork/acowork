//! Runtime → Gateway acceptance receipts for inbound Intents.
//!
//! The counterpart of `POST /api/intents/{id}/accept` on the Gateway side and
//! of [`crate::intent::accept`] there. Its job is narrow: after the Runtime
//! has finished *routing* an Intent it received over MQTT — resolved the peer,
//! picked or created a session, queued the turn — say so. Before this existed
//! the routing result stopped at a `tracing::warn!` in the dispatch loop, so
//! the Gateway's mailbox said `delivered` for messages the Runtime had thrown
//! away, and the sender (the PM task scanner) had no error to retry on.
//!
//! ## Contract
//!
//! * **Best-effort.** A receipt that cannot be posted is logged at `debug`
//!   and dropped: the Runtime's real work has already happened or failed, and
//!   a diagnostics channel must never turn into a retry loop of its own.
//! * **Off the dispatch path.** [`AcceptanceReporter::spawn_report`] returns
//!   immediately; the MQTT loop must not wait on an HTTP round trip.
//! * **Requests only.** Responses are already an answer, and pre-message-id
//!   senders have no address to answer about — see
//!   [`acceptance_target`].
//!
//! Reporting is unconditional (every request Intent, not just PM dispatches):
//! the fact is cheap, and it is what `GET /api/intents/{id}` shows an operator
//! when a message that "was delivered" never ran.

use acowork_core::auth::NODE_TOKEN_HEADER;
use acowork_core::defaults::GATEWAY_HTTP_PORT;
use acowork_core::intent::TurnOrigin;

use crate::agent::inbound::IntentEnvelope;

/// Bound on one receipt POST. Short by design: the receipt is a fact write
/// (one mailbox update), and a Gateway that cannot take it within seconds is
/// one whose answer would arrive too late to matter anyway.
const ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Percent-encode one URL path segment (RFC 3986 unreserved set only).
/// The sending Gateway validates its message ids today, but a mixed-version
/// Gateway may still publish a value bearing `/ ? #` — that would aim this
/// POST at another route while it carries the node token, so the id is
/// never pasted raw into the path.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Reports an inbound Intent's routing decision to the Gateway.
#[derive(Debug, Clone)]
pub struct AcceptanceReporter {
    instance_id: String,
    node_token: String,
    /// `http://<gateway_host>:<port>` — the Gateway that published to us.
    base_url: String,
}

impl AcceptanceReporter {
    pub fn new(
        instance_id: impl Into<String>,
        gateway_host: &str,
        node_token: impl Into<String>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            node_token: node_token.into(),
            base_url: format!("http://{gateway_host}:{GATEWAY_HTTP_PORT}"),
        }
    }

    /// Report without blocking the caller. Errors are logged, never returned:
    /// see the module contract.
    pub fn spawn_report(&self, message_id: String, accepted: bool, error: String) {
        if message_id.is_empty() {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.report(&message_id, accepted, &error).await {
                tracing::debug!(
                    message_id = %message_id,
                    accepted,
                    error = %e,
                    "intent acceptance receipt was not recorded"
                );
            }
        });
    }

    /// POST the decision. `error` is the routing failure verbatim; it is the
    /// only description of the breakage the sender will ever see.
    pub async fn report(
        &self,
        message_id: &str,
        accepted: bool,
        error: &str,
    ) -> Result<(), String> {
        let url = format!(
            "{}/api/intents/{}/accept",
            self.base_url,
            encode_path_segment(message_id)
        );
        let mut request = reqwest::Client::new().post(&url).timeout(ACCEPT_TIMEOUT).json(
            &serde_json::json!({
                "sender": self.instance_id,
                "accepted": accepted,
                "error": if accepted { "" } else { error },
            }),
        );
        if !self.node_token.is_empty() {
            request = request.header(NODE_TOKEN_HEADER, &self.node_token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("acceptance post failed: {e}"))?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("gateway rejected acceptance (HTTP {status}): {body}"))
        }
    }
}

/// The message id to report acceptance for, if this inbound message is one the
/// Gateway can match a decision against.
///
/// A **request** with a Gateway-minted id qualifies. A response does not: it is
/// already the answer to something, and reporting on it would let a chatty
/// responder overwrite the receipt its own request earned. A legacy sender with
/// no `message_id` does not either — there is no mailbox row to stamp.
pub fn acceptance_target(envelope: &IntentEnvelope) -> Option<String> {
    if envelope.message_id.is_empty() {
        return None;
    }
    match TurnOrigin::parse(&envelope.kind) {
        TurnOrigin::AgentResponse => None,
        _ => Some(envelope.message_id.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(kind: &str, message_id: &str) -> IntentEnvelope {
        IntentEnvelope {
            from: "peer".into(),
            action: "task".into(),
            params: serde_json::json!({}),
            message_id: message_id.to_string(),
            conversation_id: String::new(),
            source_chat_session_id: String::new(),
            repo: String::new(),
            session_config_json: String::new(),
            kind: kind.to_string(),
            in_reply_to: String::new(),
            status: String::new(),
            has_human: None,
            chat_attachment_ids: Vec::new(),
            chat_row_id: String::new(),
            aware_users: Vec::new(),
        }
    }

    #[test]
    fn requests_are_reported_responses_are_not() {
        assert_eq!(
            acceptance_target(&envelope("task", "msg-1")),
            Some("msg-1".to_string())
        );
        assert_eq!(
            acceptance_target(&envelope(TurnOrigin::AgentResponse.as_str(), "resp-1")),
            None
        );
    }

    #[test]
    fn a_message_without_an_id_has_nothing_to_report() {
        assert_eq!(acceptance_target(&envelope("task", "")), None);
    }

    fn reporter() -> AcceptanceReporter {
        AcceptanceReporter::new("inst-1", "127.0.0.1", "tok")
    }

    #[test]
    fn the_receipt_addresses_the_publishing_gateway() {
        let r = reporter();
        assert_eq!(r.base_url, format!("http://127.0.0.1:{GATEWAY_HTTP_PORT}"));
        assert_eq!(r.instance_id, "inst-1");
        assert_eq!(r.node_token, "tok");
    }

    #[test]
    fn a_hostile_id_cannot_retarget_the_receipt_post() {
        // Separator-bearing ids stay inside the `{message_id}` segment:
        // the encoded value round-trips to the same route (axum's Path
        // percent-decodes the capture), whatever the id bears.
        assert_eq!(encode_path_segment("pm-1234_ab"), "pm-1234_ab");
        assert_eq!(encode_path_segment("a/b?c#d"), "a%2Fb%3Fc%23d");
        assert_eq!(encode_path_segment("%"), "%25");
    }

    #[tokio::test]
    async fn an_unreachable_gateway_is_an_error_not_a_hang() {
        // Port 1 is closed on every platform: connect fails fast, and the
        // caller sees `Err` rather than a stuck turn.
        let r = AcceptanceReporter {
            base_url: "http://127.0.0.1:1".to_string(),
            ..reporter()
        };
        assert!(r.report("msg-x", false, "boom").await.is_err());
    }
}