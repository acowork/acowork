//! Chat session HTTP API (design doc 27 §2.2 / §8, Phase 1).
//!
//! ```text
//! POST   /api/chat-sessions                          create a session
//! GET    /api/chat-sessions/{participant}            sessions of one participant
//! GET    /api/chat-sessions/{session_id}/messages?offset&limit
//! POST   /api/chat-sessions/{session_id}/messages    append a user_message
//! ```
//!
//! Identity follows the `chat_api` rule (ADR-076 §决策 4): the sender of a
//! written message always comes from the verified token, never the body —
//! this is the same spoofing surface the Intent delivery entry closed for
//! `from` (tech-debt plan D4). Reads and writes require participation
//! (§5.3: the participant relation is the permission; the session id is
//! not).
//!
//! The human-facing write endpoint accepts `user_message` only. Intent
//! request / response rows are written by the system paths (Gateway
//! delivery, response write-back) through [`crate::chat_sessions`]
//! directly — a model or a chat client must never be able to forge an
//! Intent into a chat log (§6.5 R2: `intent_send` only produces requests
//! through the delivery entry, and responses are system-generated).

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::auth::context::AuthContext;
use crate::auth::issuer::now_unix;
use crate::chat_sessions::{self, ChatSession, IntentMeta, SessionKind, SessionMessage};
use crate::error::ApiError;
use crate::state::AppState;

/// Same body cap as the user↔user chat endpoint (ADR-076 §决策 8).
const MAX_BODY_CHARS: usize = 8000;
const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 200;

pub fn chat_session_routes() -> Router<AppState> {
    Router::new()
        .route("/api/chat-sessions", post(create_session))
        .route("/api/chat-sessions/{participant}", get(list_sessions))
        .route(
            "/api/chat-sessions/{session_id}/messages",
            get(list_messages).post(post_message),
        )
}

// ── Request / response types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    /// One of the `chat_sessions` kind strings (`user_group`, …).
    pub kind: String,
    pub participants: Vec<String>,
    #[serde(default)]
    pub title: String,
    /// Required for `project_group`; the idempotency key (§3.3).
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PostMessageRequest {
    pub body: String,
    /// Receiver participant id; empty = broadcast to the session (the only
    /// form a `user_message` uses today).
    #[serde(default)]
    pub to: String,
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct MessagePage {
    pub messages: Vec<SessionMessage>,
    pub total: u64,
}

// ── Handlers ──────────────────────────────────────────────────────────

async fn create_session(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<ChatSession>), ApiError> {
    let kind = parse_kind(&req.kind)?;
    // A token holder may only create sessions they themselves are in.
    // Project groups and agent channels are created by the owning
    // services through the Gateway, not by arbitrary users.
    if !req.participants.iter().any(|p| p == ctx.effective_user_id()) {
        return Err(ApiError::forbidden(
            "you can only create sessions you participate in",
        ));
    }
    if req.participants.len() > 1 && kind != SessionKind::UserGroup && kind != SessionKind::UserUser
    {
        return Err(ApiError::unprocessable_entity(
            "multi-participant sessions are limited to user_group / user_user",
        ));
    }
    let dir = data_dir(&state)?;
    let session = offload(move || {
        chat_sessions::create_session(
            &dir,
            kind,
            &req.participants,
            &req.title,
            req.project_id.as_deref(),
            now_unix(),
        )
    })
    .await?;
    Ok((StatusCode::CREATED, Json(session)))
}

async fn list_sessions(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(participant): Path<String>,
) -> Result<Json<Vec<ChatSession>>, ApiError> {
    // Listing another participant's sessions is an admin inspection only
    // (same read rule as `chat_api::require_self_or_admin`).
    if !ctx.is_admin() && ctx.effective_user_id() != participant {
        return Err(ApiError::forbidden("not your sessions"));
    }
    let dir = data_dir(&state)?;
    let sessions = offload(move || chat_sessions::list_for(&dir, &participant)).await?;
    Ok(Json(sessions))
}

async fn list_messages(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Result<Json<MessagePage>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let dir = data_dir(&state)?;
    let (messages, total) =
        offload(move || read_messages_guarded(&dir, &session_id, ctx.effective_user_id(), q.offset, limit))
            .await?;
    Ok(Json(MessagePage { messages, total }))
}

async fn post_message(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Json(req): Json<PostMessageRequest>,
) -> Result<(StatusCode, Json<SessionMessage>), ApiError> {
    let body = req.body.trim().to_string();
    if body.is_empty() {
        return Err(ApiError::unprocessable_entity("a message needs a body"));
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(ApiError::unprocessable_entity(&format!(
            "message body exceeds {MAX_BODY_CHARS} characters"
        )));
    }
    let from = ctx.effective_user_id().to_string();
    let dir = data_dir(&state)?;
    let message = offload(move || {
        let msg = SessionMessage {
            seq: 0,
            ts: 0,
            from: from.clone(),
            kind: chat_sessions::KIND_USER_MESSAGE.to_string(),
            body,
            to: String::new(),
            meta: IntentMeta::default(),
        };
        write_entry(&dir, &session_id, &from, &msg)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(message)))
}

// ── Helpers ───────────────────────────────────────────────────────────

fn parse_kind(s: &str) -> Result<SessionKind, ApiError> {
    match s {
        "user_user" => Ok(SessionKind::UserUser),
        "user_group" => Ok(SessionKind::UserGroup),
        "project_group" => Ok(SessionKind::ProjectGroup),
        "user_agent" => Ok(SessionKind::UserAgent),
        "agent_agent" => Ok(SessionKind::AgentAgent),
        other => Err(ApiError::unprocessable_entity(&format!(
            "unknown chat session kind '{other}'"
        ))),
    }
}

fn read_messages_guarded(
    dir: &std::path::Path,
    session_id: &str,
    who: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<SessionMessage>, u64), String> {
    let session = chat_sessions::load(dir, session_id)?;
    if !session.participants.iter().any(|p| p == who) {
        return Err("not_a_participant".to_string());
    }
    chat_sessions::read_messages(dir, session_id, offset, limit)
}

/// The single backend write entry as reached from HTTP: participation is
/// checked against the session, then [`chat_sessions::append_message`]
/// applies idempotency + sequencing + durable append (§8).
fn write_entry(
    dir: &std::path::Path,
    session_id: &str,
    who: &str,
    msg: &SessionMessage,
) -> Result<SessionMessage, String> {
    let session = chat_sessions::load(dir, session_id)?;
    if !session.participants.iter().any(|p| p == who) {
        return Err("not_a_participant".to_string());
    }
    chat_sessions::append_message(dir, session_id, msg, now_unix()).map(|(m, _)| m)
}

fn data_dir(state: &AppState) -> Result<std::path::PathBuf, ApiError> {
    state
        .auth_service
        .as_ref()
        .map(|s| s.data_dir().to_path_buf())
        .ok_or_else(|| {
            ApiError::service_unavailable("the account system is disabled (AUTH_MODE=local)")
        })
}

/// Offload a blocking filesystem step off the reactor (same helper shape as
/// `chat_api::offload`, with the error mapped through `internal`).
async fn offload<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(&format!("chat session task failed: {e}")))?
        .map_err(|m| {
            // Map the storage layer's refusal vocabulary onto HTTP codes;
            // an unknown session is 404, a rejected write is 422.
            if m.contains("not found")
                || m.contains("does not exist")
                || m.contains("InvalidData")
                || m == "not_a_participant"
            {
                // A non-participant must not learn whether the session
                // exists (§5.3): same 404 either way.
                ApiError::not_found("chat session not found")
            } else {
                ApiError::unprocessable_entity(&m)
            }
        })
}

#[cfg(test)]
mod tests {
    use crate::test_support;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn router(dir: &std::path::Path) -> Router {
        test_support::build_router(test_support::admin_state(dir))
    }

    fn req(method: &str, uri: &str, body: Option<&str>, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(match body {
            Some(s) => Body::from(s.to_string()),
            None => Body::empty(),
        })
        .unwrap()
    }

    async fn json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    async fn login(router: &Router, username: &str, password: &str) -> String {
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/auth/login",
                Some(&format!(r#"{{"username":"{username}","password":"{password}"}}"#)),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "login as {username}");
        json(resp).await["access_token"].as_str().unwrap().to_string()
    }

    /// Create a user through the account API, returning (id, token).
    async fn make_user(router: &Router, admin_token: &str, username: &str) -> (String, String) {
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/users",
                Some(&format!(
                    r#"{{"username":"{username}","password":"s3cret123","role":"user"}}"#
                )),
                Some(admin_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = json(resp).await;
        let id = v["account"]["user_id"].as_str().unwrap().to_string();
        let token = login(router, username, "s3cret123").await;
        (id, token)
    }

    #[tokio::test]
    async fn create_post_and_read_roundtrip() {
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, _bob_token) = make_user(&app, &admin, "bob").await;

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_group","participants":["{alice}","{bob}"],"title":"team"}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let session_id = json(resp).await["session_id"].as_str().unwrap().to_string();

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{session_id}/messages"),
                Some(r#"{"body":"hello team"}"#),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let m = json(resp).await;
        assert_eq!(m["seq"].as_u64(), Some(1));
        assert_eq!(m["kind"].as_str(), Some("user_message"));

        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/messages"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let page = json(resp).await;
        assert_eq!(page["total"].as_u64(), Some(1));
        assert_eq!(page["messages"][0]["body"].as_str(), Some("hello team"));
    }

    #[tokio::test]
    async fn non_participant_cannot_read_or_write() {
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, _bob_token) = make_user(&app, &admin, "bob").await;
        let (_carol, carol_token) = make_user(&app, &admin, "carol").await;

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_group","participants":["{alice}","{bob}"]}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        let session_id = json(resp).await["session_id"].as_str().unwrap().to_string();

        // Carol is not in the session: 404, existence not leaked.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/messages"),
                None,
                Some(&carol_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{session_id}/messages"),
                Some(r#"{"body":"sneak in"}"#),
                Some(&carol_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cannot_create_session_without_self() {
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice2").await;
        let (bob, _bob_token) = make_user(&app, &admin, "bob2").await;
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_group","participants":["{alice}","{bob}"]}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        // A group the caller is not in: refused.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(r#"{{"kind":"user_group","participants":["{bob}","x"]}}"#)),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
