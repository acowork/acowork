//! Chat session HTTP API (design doc 27 §4.2 / §8, Phase 1).
//!
//! ```text
//! POST   /api/chat-sessions                          create a session
//! GET    /api/chat-sessions/{participant}            sessions of one participant
//! GET    /api/chat-sessions/default?pair=<agent_id>  caller's default agent chat
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
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::Response,
    routing::{delete, get, patch, post},
};
use serde::{Deserialize, Serialize};

use crate::auth::context::AuthContext;
use crate::auth::issuer::now_unix;
use crate::chat_sessions::{
    self, ChatSession, IntentMeta, Participant, ParticipantIdentity, SeedConfig, SessionKind,
    SessionMessage,
};
use crate::error::ApiError;
use crate::state::AppState;

/// Same body cap as the user↔user chat endpoint (ADR-076 §决策 8).
const MAX_BODY_CHARS: usize = 8000;
const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 200;
/// Multipart envelope of the largest attachment class (documents, see
/// [`crate::chat::MAX_DOCUMENT_BYTES`]) plus slack. Same rule as
/// `chat_api::UPLOAD_BODY_LIMIT`.
const UPLOAD_BODY_LIMIT: usize = crate::chat::MAX_DOCUMENT_BYTES as usize + 1024 * 1024;
/// Streaming chunk for attachment downloads.
const DOWNLOAD_CHUNK_BYTES: usize = 64 * 1024;

pub fn chat_session_routes() -> Router<AppState> {
    Router::new()
        .route("/api/chat-sessions", post(create_session))
        // Static segment wins over `{participant}` in axum's router.
        .route("/api/chat-sessions/default", get(default_session))
        .route("/api/chat-sessions/{participant}", get(list_sessions))
        .route(
            "/api/chat-sessions/{session_id}/messages",
            get(list_messages).post(post_message),
        )
        // Design 27 §4.5: the Chat surface's seed draft is durable on the
        // session record, not in the client — the frontend copy is a cache
        // of this row. Whole-record replace; participation is the
        // permission (hidden 404 for outsiders, same §5.3 rule).
        .route(
            "/api/chat-sessions/{session_id}/session-config",
            patch(patch_session_config),
        )
        // Plan 5.2: non-default sessions are deletable; the default pair
        // session and project groups refuse in the storage layer. Shares
        // the `{participant}` pattern with the list GET because matchit
        // refuses two different param names at one path position — the
        // segment is the SESSION id for DELETE, the participant for GET.
        .route("/api/chat-sessions/{participant}", delete(delete_session_route))
        // §4.2.1 (plan 3.5): attachments live in the session's `files/`
        // dir; upload before the first message is normal. The per-route
        // body limit covers the multipart envelope of the largest class
        // (documents) without raising the JSON cap on every other route.
        .route(
            "/api/chat-sessions/{session_id}/files",
            post(upload_attachment).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route(
            "/api/chat-sessions/{session_id}/files/{attachment_id}",
            get(download_attachment),
        )
        // System (Gateway-mediated) surface. Loopback-only like every
        // other route in this process (server.rs binds 127.0.0.1), and
        // reachable without an injected identity exactly like
        // `/internal/user-profiles` (ADR-084 §决策 4b): the caller is the
        // Gateway on this host, which holds no user identity. Idempotency
        // by `message_id` bounds any replay damage.
        .route(
            "/internal/chat-sessions/messages",
            post(system_append_message),
        )
        .route("/internal/chat-sessions/pair", post(system_ensure_pair))
        .route("/internal/chat-sessions/group", post(system_ensure_group))
        // Design 27 §8.3: PM member add / remove re-syncs the group's
        // participant set to exactly the project's current members.
        .route("/internal/chat-sessions/group/sync", post(system_sync_group))
        .route("/internal/chat-sessions/repo", post(system_set_repo))
        // Design 27 §8.2/§8.3: the Gateway's scope-injection read (session
        // ids for a participant) and the participant-relation mutations
        // mediated on PM join / remove.
        .route("/internal/chat-sessions", get(system_list_for_participant))
        .route(
            "/internal/chat-sessions/{session_id}/participants",
            post(system_add_participant),
        )
        .route(
            "/internal/chat-sessions/{session_id}/participants/{participant_id}",
            delete(system_remove_participant),
        )
        .route("/internal/chat-sessions/{session_id}", get(system_load_session))
        .route(
            "/internal/chat-sessions/{session_id}/messages",
            get(system_read_messages),
        )
        .route(
            "/internal/chat-sessions/{session_id}/messages/by-message-id/{message_id}",
            get(system_message_by_id),
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
    /// Ids of attachments already uploaded to this session's `files/` dir
    /// (design 27 §4.2.1). The store re-reads name/mime/size from disk —
    /// these ids are the only client-supplied attachment fact.
    #[serde(default)]
    pub attachment_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<usize>,
    /// System read endpoint: the participant id on whose behalf the Gateway
    /// reads (participation is enforced there). Absent on user-facing reads,
    /// where the authenticated identity is used.
    #[serde(default)]
    pub who: Option<String>,
}

/// `GET /api/chat-sessions/{participant}?agent=<instance_id>` — optional
/// agent filter (design 27 D-B/B1: the Agent page's history entry).
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub agent: Option<String>,
}

/// `GET /api/chat-sessions/default?pair=<agent_id>` (§5.4.4).
#[derive(Debug, Deserialize)]
pub struct DefaultQuery {
    pub pair: String,
}

/// `PATCH /api/chat-sessions/{session_id}/session-config` — store the
/// Chat surface's seed draft (design 27 §4.5). Whole-record replace: the
/// body carries the complete draft; an absent, null or empty field means
/// "no opinion" and falls through to the agent's own defaults at seed
/// time. Values are NOT validated against any provider — the Runtime
/// clamps them at execution-session creation (§13-6).
#[derive(Debug, Deserialize, Default)]
pub struct SessionConfigRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
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
    let caller = ctx.effective_user_id().to_string();
    if !req.participants.iter().any(|p| p == &caller) {
        return Err(ApiError::forbidden(
            "you can only create sessions you participate in",
        ));
    }
    // §4.2.3: the human surface creates user sessions and the user's own
    // channel to one agent. `agent_agent` / `project_group` stay
    // system-created — a human token cannot even be a participant of one.
    match kind {
        SessionKind::UserUser | SessionKind::UserGroup => {}
        SessionKind::UserAgent => {
            if req.participants.len() != 2 {
                return Err(ApiError::unprocessable_entity(
                    "a user_agent session is exactly two participants (self + one agent)",
                ));
            }
        }
        _ => {
            return Err(ApiError::unprocessable_entity(
                "project groups and agent channels are created by their owning service",
            ));
        }
    }
    let dir = data_dir(&state)?;
    let auth = state.auth_service.clone();
    let session = offload(move || {
        let participants = typed_participants(auth.as_ref(), &req.participants, Some(&caller));
        chat_sessions::create_session(
            &dir,
            kind,
            &participants,
            &req.title,
            req.project_id.as_deref(),
            now_unix(),
        )
    })
    .await?;
    Ok((StatusCode::CREATED, Json(session)))
}

/// §8.1: identity types are resolved server-side, never taken from the
/// request body. The user service owns the account list, so an id naming
/// a known account is Human and anything else is an agent instance; the
/// token-verified caller is always Human. Local mode has no accounts —
/// there every id classifies as Agent (and the human surface is 503).
fn typed_participants(
    auth: Option<&std::sync::Arc<crate::auth::service::AuthService>>,
    ids: &[String],
    caller: Option<&str>,
) -> Vec<Participant> {
    let accounts = auth.and_then(|a| a.load_accounts().ok());
    ids.iter()
        .map(|id| {
            let identity = if Some(id.as_str()) == caller {
                ParticipantIdentity::Human
            } else {
                match accounts.as_ref().and_then(|l| l.find(id)) {
                    Some(_) => ParticipantIdentity::Human,
                    None => ParticipantIdentity::Agent,
                }
            };
            Participant::new(id.clone(), identity)
        })
        .collect()
}

async fn list_sessions(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(participant): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<ChatSession>>, ApiError> {
    // Listing another participant's sessions is an admin inspection only
    // (same read rule as `chat_api::require_self_or_admin`).
    if !ctx.is_admin() && ctx.effective_user_id() != participant {
        return Err(ApiError::forbidden("not your sessions"));
    }
    let dir = data_dir(&state)?;
    let mut sessions = offload(move || chat_sessions::list_for(&dir, &participant)).await?;
    // D-B/B1: the Agent page's history entry queries as the current user
    // and filters by agent server-side — the agent instance id is not a
    // participant the caller's token may address.
    if let Some(agent) = q.agent.filter(|a| !a.is_empty()) {
        sessions.retain(|s| {
            s.kind == SessionKind::UserAgent && s.participants.iter().any(|p| p.id == agent)
        });
    }
    Ok(Json(sessions))
}

/// `GET /api/chat-sessions/default?pair=<agent_id>` — resolve the caller's
/// default Chat session with an agent (design 27 §5.4.4): the Agent page
/// opens it in one request, idempotently creating the pair on first use.
async fn default_session(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<DefaultQuery>,
) -> Result<Json<ChatSession>, ApiError> {
    let pair = q.pair.trim().to_string();
    if pair.is_empty() {
        return Err(ApiError::unprocessable_entity("pair must be the agent instance id"));
    }
    let caller = ctx.effective_user_id().to_string();
    let dir = data_dir(&state)?;
    let auth = state.auth_service.clone();
    let session = offload(move || {
        let ids = [caller.clone(), pair.clone()];
        let participants = typed_participants(auth.as_ref(), &ids, Some(&caller));
        chat_sessions::ensure_pair(
            &dir,
            participants[0].clone(),
            participants[1].clone(),
            SessionKind::UserAgent,
            now_unix(),
        )
    })
    .await?;
    Ok(Json(session))
}

async fn list_messages(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Result<Json<MessagePage>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let dir = data_dir(&state)?;
    let (messages, total) = {
        let (dir, session_id) = (dir.clone(), session_id.clone());
        let who = ctx.effective_user_id().to_string();
        offload(move || read_messages_guarded(&dir, &session_id, &who, q.offset, limit)).await?
    };
    // §4.6 (plan 3.6): opening the session is the read that clears the
    // reader's unread. Best-effort — a failed clear only re-shows the
    // badge on the next open, it must not fail the page asked for.
    {
        let (dir, session_id) = (dir.clone(), session_id.clone());
        let who = ctx.effective_user_id().to_string();
        let _ = offload(move || chat_sessions::mark_read(&dir, &session_id, &who)).await;
    }
    Ok(Json(MessagePage { messages, total }))
}

/// Plan 5.2: delete a non-default Chat session. Guards (default pair
/// session, project group, non-participant) live in the storage layer.
async fn delete_session_route(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let dir = data_dir(&state)?;
    let who = ctx.effective_user_id().to_string();
    offload(move || chat_sessions::delete_session(&dir, &session_id, &who))
        .await
        .map(|()| StatusCode::NO_CONTENT)
}

async fn post_message(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Json(req): Json<PostMessageRequest>,
) -> Result<(StatusCode, Json<SessionMessage>), ApiError> {
    let body = req.body.trim().to_string();
    if body.is_empty() && req.attachment_ids.is_empty() {
        return Err(ApiError::unprocessable_entity("a message needs a body or an attachment"));
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(ApiError::unprocessable_entity(&format!(
            "message body exceeds {MAX_BODY_CHARS} characters"
        )));
    }
    let from = ctx.effective_user_id().to_string();
    let dir = data_dir(&state)?;
    let attachment_ids = req.attachment_ids;
    let message = offload(move || {
        let msg = SessionMessage {
            seq: 0,
            ts: 0,
            from: from.clone(),
            kind: chat_sessions::KIND_USER_MESSAGE.to_string(),
            body,
            to: String::new(),
            meta: IntentMeta::default(),
            attachment_ids,
            attachments: Vec::new(),
        };
        write_entry(&dir, &session_id, &from, &msg)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(message)))
}

async fn patch_session_config(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Json(req): Json<SessionConfigRequest>,
) -> Result<Json<ChatSession>, ApiError> {
    let who = ctx.effective_user_id().to_string();
    let dir = data_dir(&state)?;
    let cfg = SeedConfig {
        model: req.model,
        provider: req.provider,
        workspace: req.workspace,
        reasoning_effort: req.reasoning_effort,
    };
    // The stored response is the source of truth the frontend cache
    // adopts — normalization (trim / empty → unset) happened on the row,
    // so the client never re-derives it locally.
    let session = offload(move || chat_sessions::set_session_config(&dir, &session_id, &who, cfg))
        .await?;
    Ok(Json(session))
}

// ── Attachments (§4.2.1, plan 3.5) ────────────────────────────────────

/// `POST /api/chat-sessions/{session_id}/files` — multipart upload with a
/// single `file` field. The session id is the address and participation is
/// the permission (same rule as the message endpoints); the sender must be
/// a participant, or the bytes could never be referenced by a member.
async fn upload_attachment(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<crate::chat::Attachment>), ApiError> {
    let from = ctx.effective_user_id().to_string();
    let mut filename = String::new();
    let mut mime = String::new();
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(&format!("malformed multipart body: {e}")))?
    {
        if field.name() != Some("file") {
            continue; // unknown fields are drained, not an error
        }
        filename = field.file_name().unwrap_or_default().to_string();
        mime = field.content_type().unwrap_or_default().to_string();
        bytes = Some(
            field
                .bytes()
                .await
                .map_err(|e| ApiError::bad_request(&format!("unreadable file field: {e}")))?
                .to_vec(),
        );
    }
    let bytes = bytes.ok_or_else(|| ApiError::bad_request("multipart field `file` is missing"))?;
    // Checked here as well as in the store so the caller gets a 413 instead
    // of a generic 500; `limit_for` is the single source of the ceiling.
    let limit = crate::chat::limit_for(&mime);
    if bytes.len() as u64 > limit {
        return Err(ApiError::payload_too_large(&format!(
            "attachment exceeds {limit} bytes"
        )));
    }
    let dir = data_dir(&state)?;
    let attachment = offload(move || {
        chat_sessions::store_attachment(&dir, &session_id, &from, &filename, &mime, &bytes)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(attachment)))
}

/// `GET /api/chat-sessions/{session_id}/files/{attachment_id}` — stream a
/// stored attachment back. `?thumb=1` serves the thumbnail sidecar instead.
/// Same safety shape as `chat_api::download_attachment`: participation is
/// checked, the response is `attachment` disposition + `nosniff` (the stored
/// mime is client-supplied), and the blob streams from disk without
/// round-tripping through heap.
async fn download_attachment(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((session_id, attachment_id)): Path<(String, String)>,
    Query(q): Query<ThumbQuery>,
) -> Result<Response, ApiError> {
    let who = ctx.effective_user_id().to_string();
    let dir = data_dir(&state)?;
    let (dir2, sid2, aid2) = (dir.clone(), session_id.clone(), attachment_id.clone());
    let (attachment, blob_path) = offload(move || chat_sessions::load_attachment(&dir2, &sid2, &aid2))
        .await?
        .ok_or_else(|| ApiError::not_found("attachment not found"))?;
    // Participation: the attachment is part of the session's content.
    let dir3 = dir.clone();
    let sid3 = session_id.clone();
    let is_member = offload(move || {
        Ok::<bool, String>(
            chat_sessions::load(&dir3, &sid3)?
                .participants
                .iter()
                .any(|p| p.id == who),
        )
    })
    .await?;
    if !is_member {
        return Err(ApiError::not_found("attachment not found"));
    }
    let (path, content_type) = if q.thumb {
        let sidecar = chat_sessions::thumb_sidecar(
            &dir,
            &session_id,
            &attachment_id,
            attachment.thumb.as_deref(),
        )
        .ok_or_else(|| ApiError::not_found("no thumbnail"))?;
        let ct = if attachment.thumb.as_deref() == Some("txt") {
            "text/plain; charset=utf-8"
        } else {
            "image/jpeg"
        };
        (sidecar, ct)
    } else {
        (blob_path, attachment.mime.as_str())
    };
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ApiError::internal(&format!("open {}: {e}", path.display())))?;
    let stream = tokio_util::io::ReaderStream::with_capacity(file, DOWNLOAD_CHUNK_BYTES);
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    let disposition = crate::chat::content_disposition(&attachment.filename);
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/// `?thumb=1` / `?thumb=true` (same tolerant flag as `chat_api`).
#[derive(Debug, Deserialize, Default)]
pub struct ThumbQuery {
    #[serde(default, deserialize_with = "flag")]
    pub thumb: bool,
}

fn flag<'de, D>(de: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(de)?;
    Ok(match raw.as_deref() {
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
        None => false,
    })
}

// ── System endpoints (Gateway-mediated) ───────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SystemAppendRequest {
    pub session_id: String,
    /// The full message row. `seq` / `ts` are assigned by the store;
    /// `from` must be a participant of the session (checked there).
    pub message: SessionMessage,
}

/// `POST /internal/chat-sessions/messages` — the system half of the
/// single write entry: Intent request / response rows are written by the
/// Gateway's delivery pipeline, never by a chat client (§2.2, §6.5 R2).
async fn system_append_message(
    State(state): State<AppState>,
    Json(req): Json<SystemAppendRequest>,
) -> Result<(StatusCode, Json<SessionMessage>), ApiError> {
    let dir = data_dir_any(&state).await;
    let message = offload(move || {
        chat_sessions::append_message(&dir, &req.session_id, &req.message, now_unix())
            .map(|(m, _)| m)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(message)))
}

#[derive(Debug, Deserialize)]
pub struct SystemPairRequest {
    pub a: String,
    pub b: String,
    /// Session kind string (`agent_agent` today; the Gateway judges who
    /// the participants are, this store does not).
    #[serde(default = "default_pair_kind")]
    pub kind: String,
}

fn default_pair_kind() -> String {
    "agent_agent".to_string()
}

/// `POST /internal/chat-sessions/pair` — idempotently ensure the shared
/// one-to-one Chat session between two participants (design 27 §5.4.3).
/// Identity types are resolved from the account list, never from the body
/// (§8.1): agent-agent pairs classify both sides as Agent.
async fn system_ensure_pair(
    State(state): State<AppState>,
    Json(req): Json<SystemPairRequest>,
) -> Result<Json<ChatSession>, ApiError> {
    let kind = parse_kind(&req.kind)?;
    let dir = data_dir_any(&state).await;
    let auth = state.auth_service.clone();
    let session = offload(move || {
        let ids = [req.a.clone(), req.b.clone()];
        let participants = typed_participants(auth.as_ref(), &ids, None);
        chat_sessions::ensure_pair(
            &dir,
            participants[0].clone(),
            participants[1].clone(),
            kind,
            now_unix(),
        )
    })
    .await?;
    Ok(Json(session))
}

#[derive(Debug, Deserialize)]
pub struct SystemGroupRequest {
    pub project_id: String,
    /// Group chat title (usually the project title).
    #[serde(default)]
    pub title: String,
    /// Initial participants — the project owner at minimum; members are
    /// added by the Gateway as they join the project.
    pub participants: Vec<String>,
}

/// `POST /internal/chat-sessions/group` — idempotently ensure the one
/// project group chat for a project (design 27 §4.5, Phase 2: "PM 创建项目
/// 时幂等创建唯一项目群聊"). Idempotency key is `project_id`, enforced by
/// the projects index in `chat_sessions::create_session`.
async fn system_ensure_group(
    State(state): State<AppState>,
    Json(req): Json<SystemGroupRequest>,
) -> Result<Json<ChatSession>, ApiError> {
    if req.participants.is_empty() {
        return Err(ApiError::unprocessable_entity(
            "a project group needs at least one participant",
        ));
    }
    let dir = data_dir_any(&state).await;
    let auth = state.auth_service.clone();
    let session = offload(move || {
        let participants = typed_participants(auth.as_ref(), &req.participants, None);
        chat_sessions::create_session(
            &dir,
            SessionKind::ProjectGroup,
            &participants,
            &req.title,
            Some(&req.project_id),
            now_unix(),
        )
    })
    .await?;
    Ok(Json(session))
}

/// `POST /internal/chat-sessions/group/sync` — set the group's participant
/// list to exactly `participants` (design 27 §8.3). The remove case is the
/// revocation half: the dropped id loses the bound execution sessions on
/// its next scope injection.
async fn system_sync_group(
    State(state): State<AppState>,
    Json(req): Json<SystemGroupRequest>,
) -> Result<Json<ChatSession>, ApiError> {
    if req.project_id.trim().is_empty() {
        return Err(ApiError::unprocessable_entity("project_id is required"));
    }
    let dir = data_dir_any(&state).await;
    let auth = state.auth_service.clone();
    let session = offload(move || {
        let participants = typed_participants(auth.as_ref(), &req.participants, None);
        chat_sessions::sync_group(&dir, &req.project_id, &participants, now_unix())
    })
    .await?;
    Ok(Json(session))
}

#[derive(Debug, Deserialize)]
pub struct SystemRepoRequest {
    pub session_id: String,
    /// `None` / absent clears the binding.
    #[serde(default)]
    pub repo: Option<String>,
}

/// `POST /internal/chat-sessions/repo` — bind a project group chat to its
/// repo (design 27 §4.5.1); the Gateway injects it into delivered Intents.
async fn system_set_repo(
    State(state): State<AppState>,
    Json(req): Json<SystemRepoRequest>,
) -> Result<Json<ChatSession>, ApiError> {
    let dir = data_dir_any(&state).await;
    let session = offload(move || {
        chat_sessions::set_repo(&dir, &req.session_id, req.repo.as_deref())
    })
    .await?;
    Ok(Json(session))
}

/// `GET /internal/chat-sessions?participant=<id>` — the Chat session ids
/// `participant` belongs to. The Gateway injects this set as
/// `x-acowork-chat-scope` on agent-proxy requests so the Runtime can
/// resolve `Members` visibility without an ACL table (design 27 §8.2).
/// Ids only — the caller never needs the session bodies here.
async fn system_list_for_participant(
    State(state): State<AppState>,
    Query(q): Query<SystemParticipantQuery>,
) -> Result<Json<Vec<String>>, ApiError> {
    let participant = q.participant;
    if participant.is_empty() {
        return Err(ApiError::unprocessable_entity("participant is required"));
    }
    let dir = data_dir_any(&state).await;
    let ids = offload(move || {
        chat_sessions::list_for(&dir, &participant).map(|sessions| {
            sessions.into_iter().map(|s| s.session_id).collect::<Vec<_>>()
        })
    })
    .await?;
    Ok(Json(ids))
}

#[derive(Debug, Deserialize)]
pub struct SystemParticipantQuery {
    pub participant: String,
}

#[derive(Debug, Deserialize)]
pub struct SystemAddParticipantRequest {
    pub id: String,
    /// `"human"` marks a user participant; anything else (absent included)
    /// records `agent` — the Gateway's join path adds agents by instance id.
    #[serde(default)]
    pub identity: Option<String>,
}

/// `POST /internal/chat-sessions/{session_id}/participants` — add one
/// participant, idempotently (design 27 §8.3, the join half).
async fn system_add_participant(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<SystemAddParticipantRequest>,
) -> Result<Json<ChatSession>, ApiError> {
    let participant = Participant::new(
        req.id,
        if req.identity.as_deref() == Some("human") {
            ParticipantIdentity::Human
        } else {
            ParticipantIdentity::Agent
        },
    );
    let dir = data_dir_any(&state).await;
    let session = offload(move || {
        chat_sessions::add_participant(&dir, &session_id, &participant, now_unix())
    })
    .await?;
    Ok(Json(session))
}

/// `DELETE /internal/chat-sessions/{session_id}/participants/{pid}` —
/// remove one participant (design 27 §8.3, the revoke half). Chat history
/// is kept; bound execution sessions go unreachable for that account on
/// the next scope injection.
async fn system_remove_participant(
    State(state): State<AppState>,
    Path((session_id, participant_id)): Path<(String, String)>,
) -> Result<Json<ChatSession>, ApiError> {
    let dir = data_dir_any(&state).await;
    let session = offload(move || {
        chat_sessions::remove_participant(&dir, &session_id, &participant_id)
    })
    .await?;
    Ok(Json(session))
}

/// `GET /internal/chat-sessions/{session_id}` — session metadata for the
/// Gateway's delivery pipeline (participants for authorization, the `repo`
/// binding for §4.1 injection). Read-only, loopback-only like the rest of
/// the internal surface.
async fn system_load_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<ChatSession>, ApiError> {
    let dir = data_dir_any(&state).await;
    let session = offload(move || chat_sessions::load(&dir, &session_id)).await?;
    Ok(Json(session))
}

/// `GET /internal/chat-sessions/{session_id}/messages?offset&limit&who` —
/// history read for `chat_history_read` (design 27 §5.6): the Gateway passes
/// the requesting agent instance as `who`; participation is enforced here
/// server-side, and `limit` is clamped to `MAX_PAGE` (the §9 hard cap).
async fn system_read_messages(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Result<Json<MessagePage>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let who = q
        .who
        .filter(|w| !w.is_empty())
        .ok_or_else(|| ApiError::unprocessable_entity("who is required"))?;
    let dir = data_dir_any(&state).await;
    let (messages, total) = {
        let (dir, session_id, who) = (dir.clone(), session_id.clone(), who.clone());
        offload(move || {
            let session = chat_sessions::load(&dir, &session_id)?;
            if !session.participants.iter().any(|p| p.id == who) {
                return Err("not_a_participant".to_string());
            }
            chat_sessions::read_messages(&dir, &session_id, q.offset, limit)
        })
        .await?
    };
    // §4.6 (plan 3.6): the read clears the reader's unread — same rule on
    // the system surface, where the reader is an Agent via `chat_history_read`.
    {
        let (dir, session_id, who) = (dir.clone(), session_id.clone(), who.clone());
        let _ = offload(move || chat_sessions::mark_read(&dir, &session_id, &who)).await;
    }
    Ok(Json(MessagePage { messages, total }))
}

/// `GET /internal/chat-sessions/{session_id}/messages/by-message-id/{message_id}?who=`
/// — single-row fetch for the §5.5.4 answer channel (the Gateway resolves
/// the card an `in_reply_to` points at before routing the human answer to
/// the execution side). Participation is enforced like every other read.
async fn system_message_by_id(
    State(state): State<AppState>,
    Path((session_id, message_id)): Path<(String, String)>,
    Query(q): Query<MessageIdQuery>,
) -> Result<Json<Option<SessionMessage>>, ApiError> {
    let who = q
        .who
        .filter(|w| !w.is_empty())
        .ok_or_else(|| ApiError::unprocessable_entity("who is required"))?;
    let dir = data_dir_any(&state).await;
    let found = offload(move || {
        let session = chat_sessions::load(&dir, &session_id)?;
        if !session.participants.iter().any(|p| p.id == who) {
            return Err("not_a_participant".to_string());
        }
        chat_sessions::message_by_id(&dir, &session_id, &message_id)
    })
    .await?;
    Ok(Json(found))
}

#[derive(Debug, Deserialize)]
struct MessageIdQuery {
    who: Option<String>,
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
    if !session.participants.iter().any(|p| p.id == who) {
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
    if !session.participants.iter().any(|p| p.id == who) {
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

/// Data dir for the **system** (Gateway-mediated) endpoints: local mode has
/// no account system, but chat sessions are still the storage the Gateway
/// mediates (design 27 §4.6) — the shared data dir is the same one every
/// other store uses. The human-facing endpoints keep [`data_dir`].
async fn data_dir_any(state: &AppState) -> std::path::PathBuf {
    match state.auth_service.as_ref() {
        Some(s) => s.data_dir().to_path_buf(),
        None => state.shared.read().await.data_dir.clone(),
    }
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
    async fn session_config_patch_roundtrips_and_hides_membership() {
        // Design 27 §4.5 on the wire: the draft is stored on the session,
        // read back through the same `GET /default` the Agent page opens,
        // and a non-participant gets the hidden 404 — never a 403 that
        // would confirm the session id exists.
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (_alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (_carol, carol_token) = make_user(&app, &admin, "carol").await;

        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                "/api/chat-sessions/default?pair=ag-1",
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        let body = json(resp).await;
        let session_id = body["session_id"].as_str().unwrap().to_string();
        // Fresh pair: no draft key at all (skipped when unset).
        assert!(body.get("session_config").is_none());

        let resp = app
            .clone()
            .oneshot(req(
                "PATCH",
                &format!("/api/chat-sessions/{session_id}/session-config"),
                Some(r#"{"model":"m1","provider":"p1"}"#),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        assert_eq!(body["session_config"]["model"].as_str(), Some("m1"));
        assert_eq!(body["session_config"]["provider"].as_str(), Some("p1"));
        assert!(body["session_config"].get("workspace").is_none());

        // Durable: the default endpoint carries it back.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                "/api/chat-sessions/default?pair=ag-1",
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(
            json(resp).await["session_config"]["model"].as_str(),
            Some("m1")
        );

        // Whole-replace with an all-empty body clears the draft.
        let resp = app
            .clone()
            .oneshot(req(
                "PATCH",
                &format!("/api/chat-sessions/{session_id}/session-config"),
                Some(r#"{"model":"  "}"#),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert!(json(resp).await.get("session_config").is_none());

        // Carol is not a participant: 404, existence not leaked.
        let resp = app
            .clone()
            .oneshot(req(
                "PATCH",
                &format!("/api/chat-sessions/{session_id}/session-config"),
                Some(r#"{"model":"sneak"}"#),
                Some(&carol_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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

    #[tokio::test]
    async fn system_endpoints_need_no_identity_and_reach_the_store() {
        let dir = test_support::temp_dir("chatsesssys");
        let app = router(&dir);

        // Pair session, created by "the Gateway" — no token at all.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/internal/chat-sessions/pair",
                Some(r#"{"a":"agent-2","b":"agent-1"}"#),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json(resp).await;
        let session_id = v["session_id"].as_str().unwrap().to_string();
        assert_eq!(v["kind"].as_str().unwrap(), "agent_agent");

        // System append of an agent_message row (Wave 2B vocabulary:
        // the Intent facts are fields on an agent utterance now).
        let body = format!(
            r#"{{"session_id":"{session_id}","message":{{"seq":0,"ts":0,"from":"agent-1","kind":"agent_message","body":"what is 2+2","to":"agent-2","meta":{{"message_id":"m-1","conversation_id":"c-1"}}}}}}"#
        );
        let resp = app
            .clone()
            .oneshot(req("POST", "/internal/chat-sessions/messages", Some(&body), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(json(resp).await["seq"].as_u64().unwrap(), 1);

        // Idempotent replay returns the same row.
        let resp = app
            .clone()
            .oneshot(req("POST", "/internal/chat-sessions/messages", Some(&body), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(json(resp).await["seq"].as_u64().unwrap(), 1);

        // A sender that is not a participant is refused (append_message
        // participation rule still applies to system writes).
        let bad = r#"{"session_id":"not-a-uuid","message":{"seq":0,"ts":0,"from":"ghost","kind":"agent_message","body":"x","to":"agent-1","meta":{"message_id":"m-2"}}}"#;
        let resp = app
            .clone()
            .oneshot(req("POST", "/internal/chat-sessions/messages", Some(bad), None))
            .await
            .unwrap();
        // session id is not a UUID → 422 before participation even matters
        assert!(resp.status().is_client_error(), "{}", resp.status());

        // set_repo on the pair session.
        let resp = app
            .clone()
            .oneshot(
                req(
                    "POST",
                    "/internal/chat-sessions/repo",
                    Some(&format!(
                        r#"{{"session_id":"{session_id}","repo":"https://example.com/x.git"}}"#
                    )),
                    None,
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            json(resp).await["repo"].as_str().unwrap(),
            "https://example.com/x.git"
        );
    }

    #[tokio::test]
    async fn user_can_create_user_agent_session_and_write_as_human() {
        // Wave 1.1/1.2: the human surface opens `user_agent` (self + one
        // agent). Identity types are derived server-side: the caller is
        // Human, the unknown id is the Agent.
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice_ua").await;

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_agent","participants":["{alice}","agent-9"],"title":"with agent"}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = json(resp).await;
        let session_id = v["session_id"].as_str().unwrap().to_string();
        assert_eq!(v["participants"][0]["identity"].as_str(), Some("human"));
        assert_eq!(v["participants"][1]["identity"].as_str(), Some("agent"));

        // The human may talk.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{session_id}/messages"),
                Some(r#"{"body":"hello agent"}"#),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // A human cannot forge an agent response through the system entry
        // (§8.1): the writer's recorded type refuses the kind.
        let bad = format!(
            r#"{{"session_id":"{session_id}","message":{{"seq":0,"ts":0,"from":"{alice}","kind":"agent_message","body":"forged","to":"agent-9","meta":{{"message_id":"m-1","in_reply_to":"m-0"}}}}}}"#
        );
        let resp = app
            .clone()
            .oneshot(req("POST", "/internal/chat-sessions/messages", Some(&bad), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // The agent side may write its response.
        let ok = format!(
            r#"{{"session_id":"{session_id}","message":{{"seq":0,"ts":0,"from":"agent-9","kind":"agent_message","body":"done","to":"{alice}","meta":{{"message_id":"m-2","in_reply_to":"m-0"}}}}}}"#
        );
        let resp = app
            .clone()
            .oneshot(req("POST", "/internal/chat-sessions/messages", Some(&ok), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn user_agent_session_rejects_two_humans_or_three_participants() {
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice_ua2").await;
        let (bob, _bob_token) = make_user(&app, &admin, "bob_ua2").await;

        // Naming a known account as the "agent" side is refused by the
        // store's composition rule (exactly one human + one agent).
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_agent","participants":["{alice}","{bob}"]}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_agent","participants":["{alice}","agent-1","agent-2"]}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn default_endpoint_resolves_the_pair_in_one_request_idempotently() {
        // Wave 1.4 (§5.4.4): opening the Agent page gets the (user, agent)
        // default Chat session; a second call returns the same id.
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (_alice, alice_token) = make_user(&app, &admin, "alice_def").await;

        let first = json(
            app
                .clone()
                .oneshot(
                    req(
                        "GET",
                        "/api/chat-sessions/default?pair=agent-7",
                        None,
                        Some(&alice_token),
                    ),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(first["kind"].as_str(), Some("user_agent"));
        // Participants are stored sorted by id; assert the composition,
        // not the order: exactly one human + one agent (§8.1).
        let identities: Vec<&str> = first["participants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["identity"].as_str().unwrap())
            .collect();
        assert!(
            identities == vec!["human", "agent"] || identities == vec!["agent", "human"],
            "user_agent pair must carry exactly one human and one agent: {first:?}"
        );

        let second = json(
            app
                .clone()
                .oneshot(
                    req(
                        "GET",
                        "/api/chat-sessions/default?pair=agent-7",
                        None,
                        Some(&alice_token),
                    ),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(first["session_id"], second["session_id"], "idempotent pair ensure");
    }

    #[tokio::test]
    async fn list_sessions_agent_filter_keeps_only_user_agent_sessions() {
        // Wave 1.5 / D-B B1: the Agent page queries as the current user
        // with `?agent=`; other kinds never leak into the result.
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice_lst").await;
        let (bob, _bob_token) = make_user(&app, &admin, "bob_lst").await;

        // A user_group session (same agent id not involved) and a
        // user_agent session with agent-3.
        let _ = json(
            app
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
                .unwrap(),
        )
        .await;
        let ua = json(
            app
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/chat-sessions",
                    Some(&format!(
                        r#"{{"kind":"user_agent","participants":["{alice}","agent-3"]}}"#
                    )),
                    Some(&alice_token),
                ))
                .await
                .unwrap(),
        )
        .await;

        let resp = app
            .clone()
            .oneshot(
                req(
                    "GET",
                    &format!("/api/chat-sessions/{alice}?agent=agent-3"),
                    None,
                    Some(&alice_token),
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let listed = json(resp).await;
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["session_id"], ua["session_id"]);

        // Filtering by an agent the user has no session with is empty.
        let resp = app
            .clone()
            .oneshot(
                req(
                    "GET",
                    &format!("/api/chat-sessions/{alice}?agent=agent-none"),
                    None,
                    Some(&alice_token),
                ),
            )
            .await
            .unwrap();
        assert_eq!(json(resp).await.as_array().unwrap().len(), 0);
    }

    /// Design 27 §8.3 over HTTP: the internal participant surface the
    /// Gateway mediates for PM join / remove. Add is idempotent, remove
    /// revokes listing, and the scope-injection list endpoint reflects the
    /// current membership.
    #[tokio::test]
    async fn internal_participants_add_remove_and_scope_list() {
        let dir = test_support::temp_dir("chatsess-part");
        let app = router(&dir);

        // Create a project group through the internal ensure endpoint.
        let resp = app
            .clone()
            .oneshot(
                req(
                    "POST",
                    "/internal/chat-sessions/group",
                    Some(r#"{"project_id":"proj-p","participants":["ag-1","ag-2"]}"#),
                    None,
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let sid = json(resp).await["session_id"].as_str().unwrap().to_string();

        // Scope list: both agents see the group.
        let resp = app
            .clone()
            .oneshot(
                req(
                    "GET",
                    "/internal/chat-sessions?participant=ag-2",
                    None,
                    None,
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json(resp).await.as_array().unwrap().len(), 1);

        // Add ag-3, then a repeat add (idempotent — still 3 participants).
        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(
                    req(
                        "POST",
                        &format!("/internal/chat-sessions/{sid}/participants"),
                        Some(r#"{"id":"ag-3","identity":"agent"}"#),
                        None,
                    ),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        let resp = app
            .clone()
            .oneshot(
                req("GET", &format!("/internal/chat-sessions/{sid}"), None, None),
            )
            .await
            .unwrap();
        assert_eq!(json(resp).await["participants"].as_array().unwrap().len(), 3);

        // Remove ag-2: the scope list for ag-2 goes empty (the revocation
        // half — the Gateway's next injection will not carry the session).
        let resp = app
            .clone()
            .oneshot(
                req(
                    "DELETE",
                    &format!("/internal/chat-sessions/{sid}/participants/ag-2"),
                    None,
                    None,
                ),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                req(
                    "GET",
                    "/internal/chat-sessions?participant=ag-2",
                    None,
                    None,
                ),
            )
            .await
            .unwrap();
        assert_eq!(json(resp).await.as_array().unwrap().len(), 0);
        // ag-3 still lists it; an unknown id gets an empty list, not an error.
        let resp = app
            .clone()
            .oneshot(
                req(
                    "GET",
                    "/internal/chat-sessions?participant=ag-3",
                    None,
                    None,
                ),
            )
            .await
            .unwrap();
        assert_eq!(json(resp).await.as_array().unwrap().len(), 1);
    }
}
