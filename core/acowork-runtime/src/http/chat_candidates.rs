//! ADR-106 D7 — the second level of the `@`: which execution sessions this Chat
//! may address.
//!
//! The `@` carries two facts: *which agent* and *which of its sessions*. The
//! first is chat membership (the client already has it); the second lives only
//! on the node that hosts the agent, because it is read off the execution
//! sessions' own rows (D11). So this endpoint is the second level's data, and
//! the filter is the permission: what a human may address is exactly what
//! [`crate::agent::session::chat_candidates`] computes for the Chat Router, and
//! re-implementing it client-side would let a surface offer a session the
//! router would have been refused.
//!
//! Two walls, both kept:
//!
//! * **Chat membership.** The caller's chat scope (Gateway-injected, never a
//!   client header) must contain the chat being asked about. Without it the
//!   endpoint would be a reader for any group's session titles.
//! * **Initiator scoping.** The candidate filter is run as *the caller*, so the
//!   list is "my runs in this chat" on every surface — the same rule that keeps
//!   one group member out of another's session (ADR-076 §决策 4).
//!
//! What this adds on top of the permission set is D7's **display** filter: a
//! session is offered when it is running, broken, or has been touched inside
//! the recency window. `Errored` stays visible on purpose — "you need to go
//! back and look at that one" is a useful signal, not noise.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;

use crate::http::server::HttpState;
use crate::http::session_control::{scope_from_headers, session_manager};

/// How long an idle session stays offerable in the picker (ADR-106 D7: `T`,
/// default 7 days). Older work is still addressable by replying to its block
/// (level 1 — the transcript is the durable address), it just no longer clutters
/// the list of things this human might mean.
const DISPLAY_MAX_AGE_SECS: i64 = 7 * 24 * 3600;

/// Most rows one picker read returns. The candidate filter is already ordered
/// newest first, so this truncates to "the recent ones", and a session beyond it
/// is reachable by replying to its block instead.
const DISPLAY_MAX_ROWS: usize = 20;

/// Query for `GET /chat-candidates`.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct CandidatesQuery {
    /// The Chat session whose sessions are being addressed. Required: the
    /// candidate filter is per-chat by construction, and "every session this
    /// account has" is a different question with different rules.
    #[serde(default)]
    pub(crate) chat_session_id: Option<String>,
    /// `"group"` or anything else = `Direct`. The surface decides which, because
    /// the filter differs: a group only ever offers sessions bound to that
    /// group, while a 1:1 also lists the human's own hand-started ones.
    #[serde(default)]
    pub(crate) surface: Option<String>,
}

/// `GET /chat-candidates?chat_session_id=&surface=` — the second-level `@` list.
pub(crate) async fn get_chat_candidates(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(q): Query<CandidatesQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let chat = q.chat_session_id.unwrap_or_default();
    let chat = chat.trim();
    if chat.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "chat_session_id is required",
        ));
    }
    let scope = scope_from_headers(&headers);
    if !in_chat_scope(&scope, chat) {
        // Same shape as the session reads: "not yours" and "does not exist" are
        // one answer, so this cannot be probed for which chats exist.
        return Err(err(StatusCode::NOT_FOUND, "chat session not found"));
    }
    let surface = if q.surface.as_deref() == Some("group") {
        crate::agent::session::chat_candidates::CandidateSurface::Group
    } else {
        crate::agent::session::chat_candidates::CandidateSurface::Direct
    };
    let initiator = scope_uid(&scope);

    // The two disk scans happen before the lock (design 29 §5): this is a
    // filesystem walk over the agent's sessions and must not stall `POST
    // /sessions` behind it. The live-status pass is what needs the manager, so
    // it happens inside.
    let work_dir = state.work_dir.to_string_lossy().to_string();
    let rows = crate::agent::session::chat_candidates::scan_rows(&work_dir);
    let bindings = crate::agent::session::chat_candidates::scan_bindings(&work_dir);
    let manager = session_manager(&state).await?;
    let candidates = manager.lock().await.resolve_candidates(
        &rows,
        &bindings,
        chat,
        initiator.as_deref(),
        surface,
        usize::MAX,
    );

    let now = chrono::Utc::now().timestamp();
    let visible: Vec<serde_json::Value> = candidates
        .iter()
        .filter(|c| offer_on_display(c, now))
        .take(DISPLAY_MAX_ROWS)
        .map(|c| {
            serde_json::json!({
                "session_id": c.session_id,
                "label": c.title,
                "status": c.status.label(),
                "origin": c.origin.label(),
                "last_active_secs": c.last_active_secs,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "chat_session_id": chat,
        "surface": q.surface.as_deref().unwrap_or("direct"),
        "candidates": visible,
    })))
}

/// D7's display filter over the permission set.
///
/// `Closed` here is the candidate filter's marker for "not resident in this
/// process", not for "the human archived it": such a session is addressable and
/// the dispatcher reopens it (that is why level 3 uses
/// [`crate::agent::session::chat_candidates::deliverable_candidates`] at all),
/// so recency — not residency — decides whether it is worth listing.
fn offer_on_display(
    candidate: &crate::agent::session::chat_candidates::SessionCandidate,
    now_secs: i64,
) -> bool {
    use crate::agent::session::chat_candidates::CandidateStatus;
    match candidate.status {
        CandidateStatus::Working | CandidateStatus::Errored => true,
        CandidateStatus::Idle | CandidateStatus::Closed => {
            candidate.last_active_secs > 0
                && now_secs.saturating_sub(candidate.last_active_secs) <= DISPLAY_MAX_AGE_SECS
        }
    }
}

/// The caller's own chats, as the Gateway injected them.
fn in_chat_scope(scope: &acowork_core::session_meta::SessionScope, chat: &str) -> bool {
    use acowork_core::session_meta::SessionScope;
    match scope {
        // Local mode has no accounts and no membership set — the loopback bind
        // is the boundary, same as every other Runtime surface.
        SessionScope::Unfiltered => true,
        SessionScope::User { chats, .. } | SessionScope::Admin { chats, .. } => {
            chats.iter().any(|c| c == chat)
        }
    }
}

/// The initiator the candidate filter must run as. `None` in local mode, where
/// no session carries an initiator to compare against.
fn scope_uid(scope: &acowork_core::session_meta::SessionScope) -> Option<String> {
    use acowork_core::session_meta::SessionScope;
    match scope {
        SessionScope::Unfiltered => None,
        SessionScope::User { uid, .. } | SessionScope::Admin { uid, .. } => {
            Some(uid.clone()).filter(|u| !u.is_empty())
        }
    }
}

fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::chat_candidates::{CandidateOrigin, CandidateStatus, SessionCandidate};
    use acowork_core::session_meta::SessionScope;

    fn candidate(status: CandidateStatus, last_active_secs: i64) -> SessionCandidate {
        SessionCandidate {
            session_id: "s-1".to_string(),
            title: "fix the pager".to_string(),
            status,
            origin: CandidateOrigin::Chat,
            last_active_secs,
        }
    }

    /// Busy and broken are always offerable; everything else earns its place by
    /// recency, and a session with no readable activity stamp never does.
    #[test]
    fn display_keeps_working_and_errored_and_recently_active() {
        let now = 1_700_000_000;
        assert!(offer_on_display(&candidate(CandidateStatus::Working, 0), now));
        assert!(offer_on_display(&candidate(CandidateStatus::Errored, 0), now));
        assert!(offer_on_display(
            &candidate(CandidateStatus::Idle, now - 3_600),
            now
        ));
        assert!(offer_on_display(
            &candidate(CandidateStatus::Closed, now - DISPLAY_MAX_AGE_SECS),
            now
        ));
        assert!(
            !offer_on_display(
                &candidate(CandidateStatus::Idle, now - DISPLAY_MAX_AGE_SECS - 1),
                now
            ),
            "stale idle work is reached through its block, not through the picker"
        );
        assert!(!offer_on_display(&candidate(CandidateStatus::Idle, 0), now));
    }

    /// The chat membership wall: an empty scope lists nothing, and a caller in
    /// another chat is refused exactly like a caller asking about a chat that
    /// does not exist.
    #[test]
    fn the_chat_scope_is_the_first_wall() {
        assert!(in_chat_scope(&SessionScope::Unfiltered, "chat-1"));
        assert!(!in_chat_scope(&SessionScope::user("u-1"), "chat-1"));
        let member = SessionScope::User {
            uid: "u-1".to_string(),
            chats: vec!["chat-1".to_string()],
        };
        assert!(in_chat_scope(&member, "chat-1"));
        assert!(!in_chat_scope(&member, "chat-2"));
        assert!(in_chat_scope(
            &SessionScope::Admin {
                uid: "a".to_string(),
                chats: vec!["chat-1".to_string()],
            },
            "chat-1"
        ));
    }

    /// The candidate filter runs as the caller. Local mode has no caller to
    /// name, and an empty uid must not become "match sessions owned by `""`".
    #[test]
    fn the_initiator_comes_from_the_scope() {
        assert_eq!(scope_uid(&SessionScope::Unfiltered), None);
        assert_eq!(scope_uid(&SessionScope::user("u-1")).as_deref(), Some("u-1"));
        assert_eq!(
            scope_uid(&SessionScope::User {
                uid: String::new(),
                chats: vec![],
            }),
            None
        );
    }
}
