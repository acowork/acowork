//! Intent response-deadline sweeper (design doc 27 §7.1 "Runtime 崩溃 /
//! 无任何事件" row, plan §10-1 / §13-12).
//!
//! A responder's answer is written back by the Runtime at turn end. If the
//! responder's Runtime crashes — or the turn ends without any event — that
//! write-back never happens, and because the initiator is push-only
//! (§7.1: no polling) the conversation would go silent forever. This loop
//! closes the hole: a delivered request with no answer past
//! `[intent] response_timeout_secs` gets a system-generated `failed`
//! receipt — the same durable chat row and push a real response would
//! have produced, with the fixed fallback wording.
//!
//! Semantics mirror `deliver_response` (the write-back entry):
//! - chat row on the request's `source_chat_session_id`, `in_reply_to`
//!   the request id, `status=failed`, `fallback=true`;
//! - push to the initiator only when the initiator is an installed agent
//!   and the conversation is not a `task:` relay (§5.1 / §9-8);
//! - the receipt id is deterministic (`fallback-{request_id}`), so the
//!   chat store's `message_id` dedup (§8) makes a retry after a partial
//!   failure idempotent.

use std::time::Duration;

use crate::http::routes::AppState;
use crate::intent::mailbox::MailboxEntry;
use crate::intent::{chat_client, delivery};

/// How often stale requests are looked for. Cheap (one pass over the
/// mailbox) and five orders of magnitude below the default deadline.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// The §7.1 fixed fallback wording. User-facing text, no internal detail.
const FALLBACK_BODY: &str = "No answer received before the response deadline — \
the responder's execution may have crashed or been lost. This is a system \
fallback receipt (design 27 §7.1).";

/// Start the loop. Called once from the HTTP server startup with the
/// same AppState the delivery entry uses.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_INTERVAL).await;
            sweep_once(&state).await;
        }
    });
}

async fn sweep_once(state: &AppState) {
    let (mailbox, timeout_secs, user_port) = {
        let gw = state.gateway_state.read().await;
        let Some(mb) = gw.intent_mailbox.clone() else { return };
        let timeout = gw
            .config
            .as_ref()
            .map(|c| c.intent.response_timeout_secs)
            .unwrap_or(3600);
        let port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
        (mb, timeout, port)
    };
    // Without the control plane a push cannot land; the chat row alone
    // would still be worth writing, but the next tick (plane up) then
    // finds the request already answered — keep both-or-nothing by
    // skipping until the plane is back.
    if state.mqtt_gateway_client.is_none() || state.agent_registry.is_none() {
        return;
    }
    // RFC3339 cutoff: same UTC clock and format as every mailbox stamp,
    // so the lexicographic compare in `stale_unanswered` is chronological.
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(timeout_secs as i64)).to_rfc3339();
    for req in mailbox.stale_unanswered(&cutoff) {
        emit_fallback_receipt(state, &mailbox, user_port, &req).await;
    }
}

async fn emit_fallback_receipt(
    state: &AppState,
    mailbox: &crate::intent::mailbox::IntentMailbox,
    user_port: Option<u16>,
    req: &MailboxEntry,
) {
    // Deterministic id → the chat store dedups a retry after a partial
    // failure (append ok, mark_answered failed) with no second receipt.
    let receipt_id = format!("fallback-{}", req.message_id);

    // 1. The durable answer: the `intent_response` chat row.
    if !req.source_chat_session_id.is_empty() {
        let Some(port) = user_port else {
            tracing::debug!(request_id = %req.message_id, "fallback receipt deferred: user service down");
            return;
        };
        let meta = serde_json::json!({
            "message_id": receipt_id,
            "conversation_id": req.conversation_id,
            "in_reply_to": req.message_id,
            "status": "failed",
            "fallback": true,
        });
        if let Err(e) = chat_client::append_message(
            port,
            &req.source_chat_session_id,
            &chat_client::AppendMessage {
                from: req.to_instance.clone(),
                to: req.from_instance.clone(),
                kind: "intent_response".to_string(),
                body: FALLBACK_BODY.to_string(),
                meta,
            },
        )
        .await
        {
            tracing::warn!(error = %e, request_id = %req.message_id, "fallback receipt chat row failed — retrying next tick");
            return;
        }
    }

    // 2. The push back to the initiator — same addressing as a real
    //    response, skipped for task relays (§9-8) and human initiators
    //    (§5.1: the chat row renders for them; nobody is woken).
    let initiator_is_agent = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(&req.from_instance).is_some()
    };
    if !req.conversation_id.starts_with("task:") && initiator_is_agent {
        let ctx = delivery::DeliveryContext {
            state: &state.gateway_state,
            registry: state.agent_registry.as_ref().expect("checked in sweep_once"),
            node_control: state.node_control.as_ref(),
            mqtt: Some(state.mqtt_gateway_client.as_ref().expect("checked in sweep_once")),
            mailbox: Some(mailbox),
        };
        let dreq = delivery::DeliveryRequest {
            message_id: receipt_id.clone(),
            conversation_id: req.conversation_id.clone(),
            from_instance: req.to_instance.clone(),
            from_session_id: req.from_session_id.clone(),
            target_instance: req.from_instance.clone(),
            action: "chat".to_string(),
            params_json: serde_json::json!({ "message": FALLBACK_BODY }).to_string(),
            source_chat_session_id: req.source_chat_session_id.clone(),
            repo: String::new(),
            session_config_json: String::new(),
            kind: "response".to_string(),
            in_reply_to: req.message_id.clone(),
            status: "failed".to_string(),
        };
        // A failed push does not undo the durable chat row; the initiator
        // can still read it via the Chat history surface.
        if let Err(e) = delivery::wake_and_deliver(
            &ctx,
            &dreq,
            acowork_core::timeout_config::constants::INTENT_DEFAULT,
        )
        .await
        {
            tracing::warn!(error = %e, request_id = %req.message_id, "fallback receipt push failed (chat row already written)");
        }
    }

    // 3. Close the request. After this the sweeper never revisits it.
    if let Err(e) = mailbox.mark_answered(&req.message_id) {
        // The deterministic receipt id makes the next tick's retry safe.
        tracing::warn!(error = %e, request_id = %req.message_id, "mark_answered failed — receipt retried next tick");
        return;
    }
    tracing::warn!(
        request_id = %req.message_id,
        conversation_id = %req.conversation_id,
        responder = %req.to_instance,
        "Intent response deadline passed — system fallback receipt emitted (§7.1)"
    );
}
