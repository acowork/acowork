//! Chat session HTTP API (design doc 27 §4.2 / §8, Phase 1).
//!
//! ```text
//! POST   /api/chat-sessions                          create a session
//! GET    /api/chat-sessions/{participant}            sessions of one participant
//! GET    /api/chat-sessions/default?pair=<id>&kind=<..>  caller's default 1:1 chat
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
use crate::search_index::{Category, SearchHit};
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
        .route(
            "/api/chat-sessions",
            post(create_session).get(list_all_project_groups),
        )
        // Static segment wins over `{participant}` in axum's router.
        .route("/api/chat-sessions/default", get(default_session))
        .route("/api/chat-sessions/{participant}", get(list_sessions))
        .route(
            "/api/chat-sessions/{session_id}/messages",
            get(list_messages).post(post_message),
        )
        // WeChat-style in-conversation history search (文本 / 图片 / 文件).
        // Participation is the permission (same §5.3 rule as `messages`);
        // the read hits the disposable `search.sqlite` sidecar, never the log.
        .route("/api/chat-sessions/{session_id}/search", get(search_messages_route))
        // Design 27 §4.5: the Chat surface's seed draft is durable on the
        // session record, not in the client — the frontend copy is a cache
        // of this row. Whole-record replace; participation is the
        // permission (hidden 404 for outsiders, same §5.3 rule).
        .route(
            "/api/chat-sessions/{session_id}/session-config",
            patch(patch_session_config),
        )
        // Group chat persona override (design 29 §3.1 / ADR-099): the group's
        // editable `chat.md`, surfaced in the group's settings panel. Seeded
        // with the built-in default on first read. Participation is the
        // permission (hidden 404 for outsiders, same §5.3 rule); `user_group`
        // / `project_group` only — a DM keeps the agent's own package persona.
        .route(
            "/api/chat-sessions/{session_id}/chat-md",
            get(get_chat_md).put(put_chat_md).delete(delete_chat_md),
        )
        // Plan 5.2: non-default sessions are deletable; the default pair
        // session and project groups refuse in the storage layer. Shares
        // the `{participant}` pattern with the list GET because matchit
        // refuses two different param names at one path position — the
        // segment is the SESSION id for DELETE, the participant for GET.
        .route("/api/chat-sessions/{participant}", delete(delete_session_route))
        // P1 user-side member management for `user_group` sessions (ADR-098
        // D3). The participant relation is the permission (any current member
        // may invite or remove, including themselves); `project_group` stays
        // PM-mediated through the `/internal` sync route and the 1:1 pair
        // kinds cannot hold a third member — both are refused 422.
        .route(
            "/api/chat-sessions/{session_id}/participants",
            post(add_participants_route),
        )
        .route(
            "/api/chat-sessions/{session_id}/participants/{participant_id}",
            delete(remove_participant_route),
        )
        // ADR-098 governance: rename / transfer owner of a `user_group`. Both
        // are owner-or-admin gated, checked atomically in the store. The
        // participant relation is still the entry permission (§5.3): a stranger
        // gets the hidden 404, a non-owner member gets 403.
        .route(
            "/api/chat-sessions/{session_id}/title",
            patch(rename_session_route),
        )
        .route(
            "/api/chat-sessions/{session_id}/owner",
            post(transfer_owner_route),
        )
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
        .route(
            "/internal/chat-sessions/{session_id}/files/{attachment_id}/{message_id}/{target}",
            get(system_download_message_attachment),
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

/// `POST /api/chat-sessions/{session_id}/participants` — invite one or more
/// members to a `user_group` (P1). Ids are typed server-side (an account is
/// Human, anything else an Agent instance); the body never carries identity,
/// mirroring `create_session`.
#[derive(Debug, Deserialize)]
pub struct AddParticipantsRequest {
    pub participant_ids: Vec<String>,
}

/// `PATCH /api/chat-sessions/{session_id}/title` — rename a `user_group`
/// (ADR-098 governance). Owner-or-admin gated in the store.
#[derive(Debug, Deserialize)]
pub struct RenameSessionRequest {
    pub title: String,
}

/// `POST /api/chat-sessions/{session_id}/owner` — transfer ownership of a
/// `user_group` to another (human) member (ADR-098). Owner-or-admin gated in
/// the store; the target must already be a participant.
#[derive(Debug, Deserialize)]
pub struct TransferOwnerRequest {
    pub owner: String,
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

/// `GET /api/chat-sessions/default?pair=<id>&kind=<user_agent|user_user>`
/// (§5.4.4).
#[derive(Debug, Deserialize)]
pub struct DefaultQuery {
    /// The peer id: an agent instance id for `user_agent`, another account's
    /// user id for `user_user`.
    pub pair: String,
    /// Pair-session kind. Absent / empty = `user_agent` (the Agent page's
    /// 1:1 chat, the original behavior); `user_user` resolves a human↔human
    /// DM. Every other kind is rejected — only 1:1 pairs are pair-addressed.
    #[serde(default)]
    pub kind: Option<String>,
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

/// `PUT /api/chat-sessions/{session_id}/chat-md` — the group's chat-layer
/// prompt override (ADR-099, extended). Whole-file replace.
#[derive(Debug, Deserialize)]
pub struct ChatMdRequest {
    pub content: String,
}

/// Response of `GET` / `PUT .../chat-md`.
#[derive(Debug, Serialize)]
pub struct ChatMdResponse {
    /// The editable prompt body — the stored `chat.md` with the non-trimmable
    /// `# Safety` tail stripped off. The tail is appended by the store and the
    /// runtime and is never shown for editing (a member could not change it, so
    /// hiding it beats surfacing text that a save would silently revert).
    pub content: String,
    /// True while the body still equals the built-in default, so the panel
    /// can show a "reset to default" affordance without diffing client-side.
    pub is_default: bool,
    /// Whether the caller may write this prompt (group owner or admin, ADR-098
    /// governance). Read is open to every participant; the panel renders
    /// read-only when this is false instead of letting a plain member try a
    /// save and take the 403.
    pub editable: bool,
}

/// `GET /internal/chat-sessions/{session_id}` view — the durable record plus
/// the derived `chat_prompt` (a group session's `chat.md`, seeded on first
/// read) the Gateway forwards to the Runtime so a per-chat prompt override
/// reaches the chat router. Flattened so existing readers see the same shape.
#[derive(Debug, Serialize)]
struct InternalSession {
    #[serde(flatten)]
    session: ChatSession,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_prompt: Option<String>,
}

/// `GET /api/chat-sessions/{session_id}/search` — in-conversation
/// categorized search (WeChat-style).
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    /// `text` | `image` | `file`; defaults to `text`.
    #[serde(default)]
    pub category: String,
    /// Keyword for the `text` tab, filename substring for the `file` tab.
    /// Ignored by the `image` tab. Empty = browse the whole category.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub limit: Option<usize>,
    /// `line_pos` cursor: return only rows strictly older than this.
    /// Absent = start from the newest.
    #[serde(default)]
    pub before_line: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    /// Total physical line count in the session (lets the frontend convert
    /// a hit's `line_pos` into a `read_messages` offset).
    pub total_lines: u64,
    /// Cursor to continue toward older rows (`None` at the oldest end).
    pub next_before_line: Option<i64>,
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
        SessionKind::UserUser => {}
        // A `user_group` is the standard group; a lone founder is not a group.
        // Enforce the same ≥2 floor the removal route guards, so a group can
        // never be created in the degenerate one-member state whose "leave"
        // would then be refused by that floor (a dead end).
        SessionKind::UserGroup => {
            if req.participants.len() < 2 {
                return Err(ApiError::unprocessable_entity(
                    "a user_group needs at least two participants",
                ));
            }
        }
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
    // A `user_group`'s founder is its first owner (ADR-098); every other kind
    // is system-created and carries no user-side owner.
    let owner = (kind == SessionKind::UserGroup).then(|| caller.clone());
    let session = offload(move || {
        let participants = typed_participants(auth.as_ref(), &req.participants, Some(&caller));
        chat_sessions::create_session(
            &dir,
            kind,
            &participants,
            &req.title,
            owner.as_deref(),
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
    let mut sessions = offload(move || {
        let mut sessions = chat_sessions::list_for(&dir, &participant)?;
        // Sidebar group rows need the last-message meta line; derive it for
        // the project-group rows while we already hold the read lock-free
        // offload thread (one-to-one / agent rows preview elsewhere).
        chat_sessions::attach_group_previews(&dir, &mut sessions);
        Ok(sessions)
    })
    .await?;
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

/// `GET /api/chat-sessions` — admin-only cross-account project-group list
/// (ADR-076 §决策 4, read-only inspection). The sidebar's own list is
/// `GET /api/chat-sessions/{me}`, which only returns groups the caller
/// belongs to; an admin also needs the groups they never joined. Returns
/// every `project_group` session. Reading their messages is allowed by the
/// same admin rule in `list_messages`; posting stays participation-gated, so
/// this is a view, not a way to speak in someone else's project.
async fn list_all_project_groups(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<Vec<ChatSession>>, ApiError> {
    if !ctx.is_admin() {
        return Err(ApiError::forbidden("admin only"));
    }
    let dir = data_dir(&state)?;
    let groups = offload(move || {
        let mut groups = chat_sessions::list_project_groups(&dir);
        chat_sessions::attach_group_previews(&dir, &mut groups);
        Ok(groups)
    })
    .await?;
    Ok(Json(groups))
}

/// `GET /api/chat-sessions/default?pair=<id>&kind=<...>` — resolve the
/// caller's default 1:1 Chat session (design 27 §5.4.4), idempotently
/// creating the pair on first use. Two kinds are pair-addressed by a user
/// token: `user_agent` (self + one agent, the default) and `user_user`
/// (self + one other account) — both ride the same idempotent
/// [`crate::chat_sessions::ensure_pair`]; nothing else is a 1:1 pair.
async fn default_session(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<DefaultQuery>,
) -> Result<Json<ChatSession>, ApiError> {
    let pair = q.pair.trim().to_string();
    if pair.is_empty() {
        return Err(ApiError::unprocessable_entity("pair must be the peer's id"));
    }
    let kind = match q.kind.as_deref() {
        None | Some("") | Some("user_agent") => SessionKind::UserAgent,
        Some("user_user") => SessionKind::UserUser,
        Some(other) => {
            return Err(ApiError::unprocessable_entity(&format!(
                "a default pair session is user_agent or user_user, not '{other}'"
            )));
        }
    };
    let caller = ctx.effective_user_id().to_string();
    let dir = data_dir(&state)?;
    let auth = state.auth_service.clone();
    let session = offload(move || {
        let ids = [caller.clone(), pair.clone()];
        let participants = typed_participants(auth.as_ref(), &ids, Some(&caller));
        // A `user_user` pair is two people. Identity types are resolved
        // server-side (§8.1), so refuse when the peer id did not name a known
        // account — an agent id must never open a human DM.
        if kind == SessionKind::UserUser
            && participants
                .iter()
                .any(|p| p.identity != Some(ParticipantIdentity::Human))
        {
            return Err("a user_user pair needs two human participants".to_string());
        }
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
        // An admin may read a project group they never joined (the
        // read-only half of the `GET /api/chat-sessions` admin list).
        let admin_inspection = ctx.is_admin();
        offload(move || {
            read_messages_guarded(&dir, &session_id, &who, admin_inspection, q.offset, limit)
        })
        .await?
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

/// `GET /api/chat-sessions/{session_id}/search` — in-conversation
/// categorized search. Participation is the permission (a non-member gets
/// the same hidden 404 as `list_messages`); an admin may search a project
/// group they never joined. The query reads the disposable `search.sqlite`
/// sidecar only — never `messages.jsonl`.
async fn search_messages_route(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<SearchPage>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let category = Category::from_param(&q.category);
    let dir = data_dir(&state)?;
    let who = ctx.effective_user_id().to_string();
    let admin_inspection = ctx.is_admin();
    let req = SearchReq {
        category,
        q: q.q.clone(),
        before_line: q.before_line,
        limit,
    };
    let (hits, total_lines) = offload(move || {
        search_messages_guarded(&dir, &session_id, &who, admin_inspection, req)
    })
    .await?;
    // A full page implies older rows remain; the oldest returned row's
    // `line_pos` is the cursor for the next (older) page.
    let next_before_line = if hits.len() == limit {
        hits.last().map(|h| h.line_pos)
    } else {
        None
    };
    Ok(Json(SearchPage {
        hits,
        total_lines,
        next_before_line,
    }))
}

/// One search request's parameters, bundled so the guarded helper stays
/// under clippy's argument ceiling.
struct SearchReq {
    category: Category,
    q: String,
    before_line: Option<i64>,
    limit: usize,
}

/// Participation guard mirroring [`read_messages_guarded`], then the
/// storage-level search. Non-participant and unknown session both surface
/// as `not_a_participant` / "not found" so `offload` maps them to a 404.
fn search_messages_guarded(
    dir: &std::path::Path,
    session_id: &str,
    who: &str,
    admin_inspection: bool,
    req: SearchReq,
) -> Result<(Vec<SearchHit>, u64), String> {
    let session = chat_sessions::load(dir, session_id)?;
    if !session.participants.iter().any(|p| p.id == who)
        && !(admin_inspection && session.kind == SessionKind::ProjectGroup)
    {
        return Err("not_a_participant".to_string());
    }
    chat_sessions::search_messages(
        dir,
        session_id,
        req.category,
        &req.q,
        req.before_line,
        req.limit,
    )
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

/// P1 (design 27 §8.3, the user-managed half): invite members to a
/// `user_group`. Permission is the participant relation — any current member
/// may add, and outsiders get the same hidden 404 the read paths use (§5.3).
/// `project_group` is governed by PM via the `/internal` sync route and the
/// 1:1 pair kinds cannot hold a third member, so both are refused 422.
/// Idempotent: an already-listed id is a no-op and fires no doorbell.
async fn add_participants_route(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Json(req): Json<AddParticipantsRequest>,
) -> Result<Json<ChatSession>, ApiError> {
    let dir = data_dir(&state)?;
    let caller = ctx.effective_user_id().to_string();
    let auth = state.auth_service.clone();
    let ids = req.participant_ids;
    let (session, changed) = offload(move || {
        let s = chat_sessions::load(&dir, &session_id)?;
        if !s.participants.iter().any(|p| p.id == caller) {
            return Err("not_a_participant".to_string());
        }
        if s.kind != SessionKind::UserGroup {
            return Err("only user_group sessions are user-managed".to_string());
        }
        let start = s.participants.len();
        let participants = typed_participants(auth.as_ref(), &ids, None);
        let mut cur = s;
        for p in participants {
            cur = chat_sessions::add_participant(&dir, &session_id, &p, now_unix())?;
        }
        let changed = cur.participants.len() != start;
        Ok((cur, changed))
    })
    .await?;
    if changed {
        // §6.1 doorbell (same rule as `sync_group`): a member-set change is a
        // backend-driven refresh — every human surface viewing the group
        // re-reads (the header member count, and a newcomer's composer
        // unlocks). Typed agents are skipped (no listener).
        for p in session
            .participants
            .iter()
            .filter(|p| p.identity != Some(ParticipantIdentity::Agent))
        {
            crate::mqtt_publisher::notify_chat_changed(&p.id);
        }
    }
    Ok(Json(session))
}

/// P1 (governance revised): remove a member from a `user_group`, or leave it.
/// Inviting stays open to every member, but removing ANOTHER member is
/// owner-or-admin only (the store gate runs atomically under the session
/// lock); anyone may remove themselves. The owner is a permanent member
/// either way — they must transfer ownership before being removed or leaving.
/// Refused once the group would drop below two members. Idempotent no-op for
/// an unknown id.
async fn remove_participant_route(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((session_id, participant_id)): Path<(String, String)>,
) -> Result<Json<ChatSession>, ApiError> {
    let dir = data_dir(&state)?;
    let caller = ctx.effective_user_id().to_string();
    let is_admin = ctx.is_admin();
    let notify_id = participant_id.clone();
    let (session, removed) = offload(move || {
        let s = chat_sessions::load(&dir, &session_id)?;
        if !s.participants.iter().any(|p| p.id == caller) {
            return Err("not_a_participant".to_string());
        }
        if s.kind != SessionKind::UserGroup {
            return Err("only user_group sessions are user-managed".to_string());
        }
        if !s.participants.iter().any(|p| p.id == participant_id) {
            return Ok((s, false));
        }
        // The ≥2 floor is enforced atomically *inside* the store's session
        // lock (`min_participants = 2`), not on this lock-free snapshot — so
        // two concurrent removals can never each pass a `<= 2` check and
        // interleave the group down to one member. `protect_owner = true` also
        // makes the current owner a permanent member (ADR-098): they must
        // transfer ownership before they can be removed or leave. The
        // `Some((caller, is_admin))` actor gates removals of OTHER members on
        // owner-or-admin (self-removal is always allowed); both refusals reach
        // the offload vocabulary (`not_authorized` → 403, floors → 422).
        let s2 = chat_sessions::remove_participant(
            &dir,
            &session_id,
            &participant_id,
            2,
            true,
            Some((&caller, is_admin)),
        )?;
        Ok((s2, true))
    })
    .await?;
    if removed {
        // Notify the remaining humans, and the removed party themselves so
        // their surface drops the row (an unknown id is a no-op, so firing at
        // a removed agent is harmless — it has no listener).
        for p in session
            .participants
            .iter()
            .filter(|p| p.identity != Some(ParticipantIdentity::Agent))
        {
            crate::mqtt_publisher::notify_chat_changed(&p.id);
        }
        crate::mqtt_publisher::notify_chat_changed(&notify_id);
    }
    Ok(Json(session))
}

/// ADR-098 governance: rename a `user_group`. Only the owner (or an admin) may
/// rename — the store checks this atomically under the session lock, so a
/// concurrent transfer cannot flip the gate mid-write. Participation is still
/// the entry permission (a stranger hits the store's `not_a_participant` →
/// hidden 404). On success every human surface gets a doorbell so its header
/// re-reads the new title.
async fn rename_session_route(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Json(req): Json<RenameSessionRequest>,
) -> Result<Json<ChatSession>, ApiError> {
    let dir = data_dir(&state)?;
    let caller = ctx.effective_user_id().to_string();
    let is_admin = ctx.is_admin();
    let title = req.title;
    let session =
        offload(move || chat_sessions::rename_session(&dir, &session_id, &title, &caller, is_admin))
            .await?;
    for p in session
        .participants
        .iter()
        .filter(|p| p.identity != Some(ParticipantIdentity::Agent))
    {
        crate::mqtt_publisher::notify_chat_changed(&p.id);
    }
    Ok(Json(session))
}

/// ADR-098 governance: transfer a `user_group`'s ownership to another human
/// member. Same permission shape as [`rename_session_route`] (owner-or-admin,
/// checked under the lock; a non-owner member gets `not_authorized` → 403, a
/// non-member target gets 422). A doorbell refreshes every human surface so the
/// owner badge and the permanent-member guard update live.
async fn transfer_owner_route(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Json(req): Json<TransferOwnerRequest>,
) -> Result<Json<ChatSession>, ApiError> {
    let dir = data_dir(&state)?;
    let caller = ctx.effective_user_id().to_string();
    let is_admin = ctx.is_admin();
    let new_owner = req.owner;
    let session = offload(move || {
        chat_sessions::transfer_owner(&dir, &session_id, &new_owner, &caller, is_admin)
    })
    .await?;
    for p in session
        .participants
        .iter()
        .filter(|p| p.identity != Some(ParticipantIdentity::Agent))
    {
        crate::mqtt_publisher::notify_chat_changed(&p.id);
    }
    Ok(Json(session))
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

// ── Group chat prompt (`chat.md`, ADR-099 extended) ───────────────────

/// `GET /api/chat-sessions/{session_id}/chat-md` — the group's chat-layer
/// prompt, seeded with the built-in default on first read. Participation is
/// the read permission (§5.3, a stranger gets the hidden 404); `editable` in
/// the body tells the panel whether this caller may write it (owner/admin).
/// `user_group` / `project_group` only (a DM edits the agent's package
/// `prompts/chat.md`).
async fn get_chat_md(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
) -> Result<Json<ChatMdResponse>, ApiError> {
    let dir = data_dir(&state)?;
    let who = ctx.effective_user_id().to_string();
    let is_admin = ctx.is_admin();
    let resp = offload(move || {
        let session = chat_sessions::load(&dir, &session_id)?;
        if !session.participants.iter().any(|p| p.id == who) {
            return Err("not_a_participant".to_string());
        }
        let content = chat_sessions::read_chat_md(&dir, &session)?
            .ok_or_else(|| "this chat session has no chat.md".to_string())?;
        let editable = chat_sessions::can_govern_group(&session, &who, is_admin);
        // `is_default` compares stripped bodies, so it is unaffected by the
        // tail; the response hides the tail from the editor.
        let is_default = chat_sessions::is_default_chat_md(&content);
        Ok(ChatMdResponse {
            content: acowork_core::chat_prompt::strip_chat_prompt_tail(&content).to_string(),
            is_default,
            editable,
        })
    })
    .await?;
    Ok(Json(resp))
}

/// `PUT /api/chat-sessions/{session_id}/chat-md` — replace the group's
/// chat-layer prompt. Read stays open to participants, but the write is
/// governed like every other group mutation: owner-or-admin (ADR-098), checked
/// under the session lock in the store. A non-member gets the hidden 404, a
/// plain member 403; the store enforces the kind and the size cap, writes the
/// normalized bytes (body + safety tail) atomically, and the response strips
/// the tail back off so the editor only ever shows the editable body.
async fn put_chat_md(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Json(req): Json<ChatMdRequest>,
) -> Result<Json<ChatMdResponse>, ApiError> {
    if req.content.len() > chat_sessions::MAX_CHAT_MD_BYTES {
        return Err(ApiError::payload_too_large(&format!(
            "chat.md exceeds {} bytes",
            chat_sessions::MAX_CHAT_MD_BYTES
        )));
    }
    let dir = data_dir(&state)?;
    let who = ctx.effective_user_id().to_string();
    let is_admin = ctx.is_admin();
    let content = req.content;
    let resp = offload(move || {
        // Participation, governance, the cap and the atomic write all happen
        // together under the store's session lock, so a concurrent transfer
        // cannot flip the gate mid-write.
        // The client sends the body only (no tail); the store re-appends the
        // canonical `# Safety` tail, then we strip it back off so the editor
        // keeps showing exactly what the caller can edit.
        let written = chat_sessions::write_chat_md(&dir, &session_id, &content, &who, is_admin)?;
        let is_default = chat_sessions::is_default_chat_md(&written);
        Ok(ChatMdResponse {
            content: acowork_core::chat_prompt::strip_chat_prompt_tail(&written).to_string(),
            is_default,
            editable: true,
        })
    })
    .await?;
    Ok(Json(resp))
}

/// `DELETE /api/chat-sessions/{session_id}/chat-md` — drop the group's prompt
/// override so the next read re-seeds the built-in default ("restore default").
/// Governed exactly like the write (owner-or-admin under the session lock; a
/// non-member gets the hidden 404, a plain member 403). Idempotent: resetting
/// an already-default group is a no-op 204.
async fn delete_chat_md(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let dir = data_dir(&state)?;
    let who = ctx.effective_user_id().to_string();
    let is_admin = ctx.is_admin();
    offload(move || chat_sessions::reset_chat_md(&dir, &session_id, &who, is_admin))
        .await
        .map(|()| StatusCode::NO_CONTENT)
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

/// Download one attachment pinned to a specific durable Chat message.
/// The user service verifies the row's target and attachment membership.
async fn system_download_message_attachment(
    State(state): State<AppState>,
    Path((session_id, attachment_id, message_id, target)): Path<(String, String, String, String)>,
) -> Result<Response, ApiError> {
    let dir = data_dir(&state)?;
    let (dir2, sid2, aid2, mid2, target2) =
        (dir.clone(), session_id.clone(), attachment_id.clone(), message_id.clone(), target.clone());
    let (attachment, blob_path) = offload(move || -> Result<_, String> {
        let row = chat_sessions::message_by_id(&dir2, &sid2, &mid2)?
            .ok_or_else(|| "message not found".to_string())?;
        // The shared group `user_message` row is addressed with an empty
        // `to` and lists its @-targets in `meta.mentions` (§5.3.1). A
        // mentioned agent may therefore pull the attachment off that shared
        // row; a non-mentioned agent (whose `dreq.message_id` also matches no
        // durable row) is still refused — existence is never leaked.
        let addressed = row.to == target2 || row.meta.mentions.contains(&target2);
        if row.kind != "user_message" || !addressed || !row.attachments.iter().any(|a| a.id == aid2) {
            return Err("attachment is not pinned to this target message".to_string());
        }
        chat_sessions::load_attachment(&dir2, &sid2, &aid2)?
            .ok_or_else(|| "attachment not found".to_string())
    })
    .await
    // Same rule as the user-facing download: a bad session / row / id /
    // target coordinate is one answer — 404. Existence is not leaked to a
    // caller the durable row was not addressed to.
    .map_err(|_| ApiError::not_found("attachment not found"))?;
    let file = tokio::fs::File::open(&blob_path).await
        .map_err(|e| ApiError::internal(&format!("open {}: {e}", blob_path.display())))?;
    let mut response = Response::new(Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
        file, DOWNLOAD_CHUNK_BYTES,
    )));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&attachment.mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&attachment.size.to_string()).unwrap());
    headers.insert(header::CONTENT_DISPOSITION, HeaderValue::from_str(
        &crate::chat::content_disposition(&attachment.filename),
    ).unwrap_or_else(|_| HeaderValue::from_static("attachment")));
    headers.insert(HeaderName::from_static("x-content-type-options"), HeaderValue::from_static("nosniff"));
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
            None,
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
        // PM-mediated removal — no user_group floor; a project roster may
        // shrink below two humans.
        chat_sessions::remove_participant(&dir, &session_id, &participant_id, 0, false, None)
    })
    .await?;
    Ok(Json(session))
}

/// `GET /internal/chat-sessions/{session_id}` — session metadata for the
/// Gateway's delivery pipeline (participants for authorization, the `repo`
/// binding for §4.1 injection, and the group's `chat.md` prompt override,
/// ADR-099 extended). Read-only, loopback-only like the rest of the internal
/// surface.
async fn system_load_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<InternalSession>, ApiError> {
    let dir = data_dir_any(&state).await;
    let view = offload(move || {
        let session = chat_sessions::load(&dir, &session_id)?;
        // Seeding side effect on a read is intentional: a group session's
        // `chat.md` is created (with the built-in default) the first time the
        // Gateway loads it, so every group has one without a migration pass.
        let chat_prompt = chat_sessions::read_chat_md(&dir, &session)?;
        Ok(InternalSession {
            session,
            chat_prompt,
        })
    })
    .await?;
    Ok(Json(view))
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
    admin_inspection: bool,
    offset: usize,
    limit: usize,
) -> Result<(Vec<SessionMessage>, u64), String> {
    let session = chat_sessions::load(dir, session_id)?;
    if !session.participants.iter().any(|p| p.id == who) {
        // Admin read-only inspection covers project groups only — the
        // deployment owner sees every project's channel, never a private
        // 1:1 (user_agent / agent_agent) conversation.
        if !(admin_inspection && session.kind == SessionKind::ProjectGroup) {
            return Err("not_a_participant".to_string());
        }
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
            } else if m == "not_authorized" {
                // A member who lacks the governance right (rename / transfer
                // owner is owner-or-admin only, ADR-098). Distinct from the
                // hidden 404: the caller IS a participant, just not the owner.
                ApiError::forbidden("you are not allowed to perform this action")
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
    async fn user_user_default_pair_is_idempotent_and_two_humans() {
        // P0: a human↔human DM is a `user_user` pair session, resolved through
        // the same `GET /default` the Agent page uses. It must be idempotent,
        // order-independent (pair_key), participation-gated on both sides, and
        // refuse an agent id (a DM is two people).
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, bob_token) = make_user(&app, &admin, "bob").await;

        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/default?pair={bob}&kind=user_user"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        let session_id = body["session_id"].as_str().unwrap().to_string();
        assert_eq!(body["kind"].as_str(), Some("user_user"));

        // Idempotent + order-independent: the same request returns the same
        // session, and Bob opening Alice lands on the SAME pair session.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/default?pair={bob}&kind=user_user"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(
            json(resp).await["session_id"].as_str(),
            Some(session_id.as_str())
        );
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/default?pair={alice}&kind=user_user"),
                None,
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(
            json(resp).await["session_id"].as_str(),
            Some(session_id.as_str())
        );

        // Post through the participation-gated human write, read back by Bob.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{session_id}/messages"),
                Some(r#"{"body":"hey bob"}"#),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/messages"),
                None,
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json(resp).await["messages"][0]["body"].as_str(), Some("hey bob"));

        // A non-participant is hidden (404); an agent id cannot open a DM (422).
        let (_carol, carol_token) = make_user(&app, &admin, "carol").await;
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
                "GET",
                "/api/chat-sessions/default?pair=ag-1&kind=user_user",
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn user_group_member_add_and_remove_is_participant_gated() {
        // P1 (governance revised): `user_group` is user-managed. Any member may
        // INVITE (the participant relation is the permission), but removing
        // ANOTHER member is owner-or-admin only — a plain member gets 403;
        // anyone may remove themselves. Outsiders get the hidden 404, ids are
        // typed server-side, the add is idempotent, and a removal past the
        // two-member floor is refused 422.
        use acowork_core::mqtt_proto::user_chat_changed_topic;

        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, bob_token) = make_user(&app, &admin, "bob").await;
        let (carol, carol_token) = make_user(&app, &admin, "carol").await;
        let (mallory, mallory_token) = make_user(&app, &admin, "mallory").await;

        let participants_of = |v: &serde_json::Value, id: &str| -> Option<String> {
            v["participants"]
                .as_array()?
                .iter()
                .find(|p| p["id"].as_str() == Some(id))
                .and_then(|p| p["identity"].as_str().map(|s| s.to_string()))
        };

        // A two-person user_group seeded by Alice.
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
        let sid = json(resp).await["session_id"].as_str().unwrap().to_string();

        // Alice invites Carol (an account → Human) and an agent id (→ Agent).
        let before = crate::mqtt_publisher::test_signals().len();
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/participants"),
                Some(&format!(r#"{{"participant_ids":["{carol}","ag-1"]}}"#)),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        assert_eq!(participants_of(&body, &carol).as_deref(), Some("human"));
        assert_eq!(participants_of(&body, "ag-1").as_deref(), Some("agent"));
        // Doorbell: the human newcomer is woken (uuid topic — only this test
        // can emit it, so the window assertion is concurrency-safe).
        let window = &crate::mqtt_publisher::test_signals()[before..];
        assert!(window.contains(&user_chat_changed_topic(&carol)), "the newcomer is woken");

        // Idempotent re-add leaves Carol once and wakes nobody.
        let before = crate::mqtt_publisher::test_signals().len();
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/participants"),
                Some(&format!(r#"{{"participant_ids":["{carol}"]}}"#)),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        let carols = body["participants"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["id"].as_str() == Some(carol.as_str()))
            .count();
        assert_eq!(carols, 1, "re-add is a no-op");
        let window = &crate::mqtt_publisher::test_signals()[before..];
        assert!(!window.contains(&user_chat_changed_topic(&carol)), "no change, no doorbell");

        // Any member may invite: Bob (not the creator) adds an agent.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/participants"),
                Some(r#"{"participant_ids":["ag-2"]}"#),
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "any member can invite");

        // An outsider cannot touch the roster — hidden 404, never a 403.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/participants"),
                Some(&format!(r#"{{"participant_ids":["{mallory}"]}}"#)),
                Some(&mallory_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Governance (P1 revised): a plain member cannot remove ANOTHER member
        // — Bob (not the owner) removing Carol gets 403, not a hidden 404.
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat-sessions/{sid}/participants/{carol}"),
                None,
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "only owner/admin may remove another member"
        );

        // Anyone may remove themselves (leave) — Carol leaves her own seat.
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat-sessions/{sid}/participants/{carol}"),
                None,
                Some(&carol_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "self-leave is always allowed");

        // Removing an agent works while ≥ 2 humans remain (Alice is the owner).
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat-sessions/{sid}/participants/ag-1"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // A fresh two-person group cannot drop below two.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_group","participants":["{alice}","{bob}"],"title":"duo"}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        let duo = json(resp).await["session_id"].as_str().unwrap().to_string();
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat-sessions/{duo}/participants/{bob}"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a group keeps at least two members"
        );
    }

    #[tokio::test]
    async fn user_group_create_requires_two_members() {
        // ADR-098 D3: a lone founder is not a group. The create route enforces
        // the same ≥2 floor the removal route guards, so a group can never be
        // born in the one-member state whose "leave" would then be refused.
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, _bob_token) = make_user(&app, &admin, "bob").await;

        let solo = format!(r#"{{"kind":"user_group","participants":["{alice}"],"title":"me-only"}}"#);
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&solo),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a one-person user_group is refused"
        );

        // The two-person boundary is accepted.
        let duo = format!(r#"{{"kind":"user_group","participants":["{alice}","{bob}"],"title":"ok"}}"#);
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&duo),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn user_group_create_sets_caller_as_owner() {
        // ADR-098: the founder is the group's first owner, surfaced on the
        // create response.
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
        let body = json(resp).await;
        assert_eq!(body["owner"].as_str(), Some(alice.as_str()));
    }

    #[tokio::test]
    async fn user_group_rename_is_owner_gated() {
        // Owner (or admin) renames; a non-owner member gets 403, an outsider
        // the hidden 404 (§5.3).
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, bob_token) = make_user(&app, &admin, "bob").await;
        let (_mallory, mallory_token) = make_user(&app, &admin, "mallory").await;
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
        let sid = json(resp).await["session_id"].as_str().unwrap().to_string();

        // Owner renames — persisted on the returned row.
        let resp = app
            .clone()
            .oneshot(req(
                "PATCH",
                &format!("/api/chat-sessions/{sid}/title"),
                Some(r#"{"title":"renamed"}"#),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json(resp).await["title"].as_str(), Some("renamed"));

        // A non-owner member is refused 403 (a participant, just not the owner).
        let resp = app
            .clone()
            .oneshot(req(
                "PATCH",
                &format!("/api/chat-sessions/{sid}/title"),
                Some(r#"{"title":"hijack"}"#),
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // An outsider gets the hidden 404, never a signal that it exists.
        let resp = app
            .clone()
            .oneshot(req(
                "PATCH",
                &format!("/api/chat-sessions/{sid}/title"),
                Some(r#"{"title":"nope"}"#),
                Some(&mallory_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn user_group_transfer_owner_flow() {
        // Owner transfers to a human member; a non-owner cannot transfer; an
        // agent or non-member target is refused 422.
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, bob_token) = make_user(&app, &admin, "bob").await;
        let (carol, _carol_token) = make_user(&app, &admin, "carol").await;
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_group","participants":["{alice}","{bob}","{carol}"],"title":"team"}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        let sid = json(resp).await["session_id"].as_str().unwrap().to_string();

        // Alice (owner) transfers to Bob.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/owner"),
                Some(&format!(r#"{{"owner":"{bob}"}}"#)),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json(resp).await["owner"].as_str(), Some(bob.as_str()));

        // Alice is now a plain member — she can no longer transfer.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/owner"),
                Some(&format!(r#"{{"owner":"{alice}"}}"#)),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // The owner (Bob) may not hand the group to an agent.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/owner"),
                Some(r#"{"owner":"ag-1"}"#),
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // ...nor to a non-member.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/owner"),
                Some(r#"{"owner":"stranger"}"#),
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn user_group_owner_is_permanent_until_transfer() {
        // The current owner cannot be removed (or leave) until ownership moves
        // (ADR-098). After transferring, the ex-owner is an ordinary member and
        // the removal is allowed while ≥2 members remain.
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, bob_token) = make_user(&app, &admin, "bob").await;
        let (carol, _carol_token) = make_user(&app, &admin, "carol").await;
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_group","participants":["{alice}","{bob}","{carol}"],"title":"team"}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        let sid = json(resp).await["session_id"].as_str().unwrap().to_string();

        // The owner cannot even leave their own group while they hold it: the
        // self-removal path skips the governance gate but still hits the
        // owner-permanence floor (422) — they must transfer first.
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat-sessions/{sid}/participants/{alice}"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // After transferring to Bob, Alice becomes removable (3 members, so the
        // ≥2 floor allows the removal).
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/owner"),
                Some(&format!(r#"{{"owner":"{bob}"}}"#)),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat-sessions/{sid}/participants/{alice}"),
                None,
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        let alices = body["participants"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["id"].as_str() == Some(alice.as_str()))
            .count();
        assert_eq!(alices, 0, "the ex-owner is now removable");
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
    async fn group_chat_md_is_seeded_editable_and_non_participant_hidden() {
        // ADR-099 (extended) on the wire: a user_group's `chat.md` is seeded
        // with the built-in default on first read, editable only by the owner
        // (a plain member gets 403), hidden (404) from a non-member, and every
        // stored body carries the non-trimmable safety tail.
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, bob_token) = make_user(&app, &admin, "bob").await;
        let (_carol, carol_token) = make_user(&app, &admin, "carol").await;

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

        // First read seeds the built-in default (the whole prompt, scenario
        // section included).
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        assert_eq!(body["is_default"].as_bool(), Some(true));
        assert_eq!(body["editable"].as_bool(), Some(true));
        // The editor shows the writable body (scenario section included) but
        // never the non-trimmable `# Safety` tail it will be saved with.
        let seeded = body["content"].as_str().unwrap();
        assert!(seeded.contains("## Scenario"));
        assert!(!seeded.contains("# Safety"));

        // The owner may replace the whole file. They send the body only; the
        // store re-appends the safety tail on disk, and the response strips it
        // back off — so what they see is exactly what they wrote.
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                Some(r#"{"content":"Custom group kernel."}"#),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json(resp).await;
        let expected = "Custom group kernel.";
        assert_eq!(body["content"].as_str(), Some(expected));
        assert_eq!(body["is_default"].as_bool(), Some(false));
        assert_eq!(body["editable"].as_bool(), Some(true));

        // A plain member reads it (server reports editable = false) but a write
        // is refused 403 — the write is owner/admin only (ADR-098).
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                None,
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json(resp).await["editable"].as_bool(), Some(false));

        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                Some(r#"{"content":"member takeover."}"#),
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Durable across reads.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(
            json(resp).await["content"].as_str(),
            Some(expected)
        );

        // Carol is not a member: 404 for read, write, and reset — no leak.
        for (method, body) in [
            ("GET", None),
            ("PUT", Some(r#"{"content":"sneak"}"#)),
            ("DELETE", None),
        ] {
            let resp = app
                .clone()
                .oneshot(req(
                    method,
                    &format!("/api/chat-sessions/{session_id}/chat-md"),
                    body,
                    Some(&carol_token),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn group_chat_md_reset_restores_default_and_is_owner_gated() {
        // "Restore default" on the wire: a plain member's DELETE is 403, the
        // owner's drops the override so the next read re-seeds the built-in
        // default (Custom → Default badge flips back), and resetting an
        // already-default group is an idempotent 204.
        let dir = test_support::temp_dir("chatsessapi");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (alice, alice_token) = make_user(&app, &admin, "alice").await;
        let (bob, bob_token) = make_user(&app, &admin, "bob").await;

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
        let session_id = json(resp).await["session_id"].as_str().unwrap().to_string();

        // Make it Custom first.
        app.clone()
            .oneshot(req(
                "PUT",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                Some(r#"{"content":"Custom group kernel."}"#),
                Some(&alice_token),
            ))
            .await
            .unwrap();

        // A plain member cannot reset it (403), only the owner/admin can.
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                None,
                Some(&bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // The owner resets: 204, and the next read is the default again.
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        let body = json(resp).await;
        assert_eq!(body["is_default"].as_bool(), Some(true));
        let seeded = body["content"].as_str().unwrap();
        assert!(!seeded.contains("Custom group kernel."));
        assert!(seeded.contains("## Scenario"));
        assert!(!seeded.contains("# Safety"));

        // Idempotent: resetting an already-default group is still 204.
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat-sessions/{session_id}/chat-md"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
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
    async fn search_endpoint_pages_by_cursor_and_hides_membership() {
        // ADR-097 D6/D7⑦: the search route shares the read path's
        // participation guard, pages toward older rows with `before_line`,
        // and reports the authoritative line count for locate math.
        let dir = test_support::temp_dir("chatsess-search");
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
                    r#"{{"kind":"user_group","participants":["{alice}","{bob}"],"title":"search"}}"#
                )),
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let session_id = json(resp).await["session_id"].as_str().unwrap().to_string();
        for body in ["shared alpha", "shared beta", "shared gamma"] {
            let resp = app
                .clone()
                .oneshot(req(
                    "POST",
                    &format!("/api/chat-sessions/{session_id}/messages"),
                    Some(&format!(r#"{{"body":"{body}"}}"#)),
                    Some(&alice_token),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        // First page: newest first, a cursor when the page is full, and the
        // physical line count the frontend turns `line_pos` into `offset`.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/search?q=shared&limit=2"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let page = json(resp).await;
        assert_eq!(page["total_lines"].as_u64(), Some(3));
        assert_eq!(page["hits"].as_array().unwrap().len(), 2);
        assert_eq!(page["hits"][0]["body"].as_str(), Some("shared gamma"));
        let cursor = page["next_before_line"].as_i64().expect("cursor on a full page");

        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/api/chat-sessions/{session_id}/search?q=shared&limit=2&before_line={cursor}"
                ),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let older = json(resp).await;
        assert_eq!(older["hits"].as_array().unwrap().len(), 1);
        assert_eq!(older["hits"][0]["body"].as_str(), Some("shared alpha"));
        assert!(older["next_before_line"].is_null(), "no cursor at the oldest end");

        // An unrecognized category falls back to the text tab (browse mode).
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/search?category=bogus"),
                None,
                Some(&alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json(resp).await["hits"].as_array().unwrap().len(), 3);

        // A non-participant sees the same hidden 404 as every other read.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{session_id}/search?q=shared"),
                None,
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

    /// Design 27 §4.2.1 chat-attachment HTTP pull: the Gateway-facing
    /// internal route streams the byte-exact blob (plus the filename in
    /// `Content-Disposition`) only when a durable `user_message` row
    /// addressed to `target` pins the attachment id — the row is the
    /// authorization. A wrong target or an unknown id answers 404.
    #[tokio::test]
    async fn internal_attachment_stream_requires_pinned_row() {
        let dir = test_support::temp_dir("chatsessatt");
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
        let session_id = json(resp).await["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        let boundary = "----testBoundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"pic.png\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&png);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/chat-sessions/{session_id}/files"))
                    .header("authorization", format!("Bearer {alice_token}"))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let attachment_id = json(resp).await["id"].as_str().unwrap().to_string();

        // Pin the attachment to a durable user_message addressed to the
        // agent instance — exactly what the Gateway writes on delivery.
        let row_body = serde_json::json!({
            "session_id": session_id,
            "message": {
                "seq": 0,
                "ts": 0,
                "from": alice,
                "to": "agent-1",
                "kind": "user_message",
                "body": "see the file",
                "meta": { "message_id": "m-1" },
                "attachment_ids": [attachment_id],
            }
        })
        .to_string();
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/internal/chat-sessions/messages",
                Some(&row_body),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Correct coordinates: byte-exact stream + the real filename.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/internal/chat-sessions/{session_id}/files/{attachment_id}/m-1/agent-1"
                ),
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cd = resp
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(cd.contains("pic.png"), "filename must ride the header: {cd}");
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], &png[..]);

        // A row addressed to somebody else pins nothing for `agent-2`.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/internal/chat-sessions/{session_id}/files/{attachment_id}/m-1/agent-2"
                ),
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // An id the row does not pin: 404, same as a nonexistent one.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/internal/chat-sessions/{session_id}/files/00000000-0000-0000-0000-000000000000/m-1/agent-1"
                ),
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Design 27 §5.3.1 group @ fan-out: one shared `user_message` row is
    /// addressed with an empty `to` and lists its @-targets in
    /// `meta.mentions`. A mentioned agent may therefore pull the attachment
    /// off that shared row; a non-mentioned agent is still refused (404), so
    /// existence is never leaked to someone the fan-out did not target.
    #[tokio::test]
    async fn internal_attachment_stream_allows_mentioned_target_on_shared_row() {
        let dir = test_support::temp_dir("chatsessatt2");
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
        let session_id = json(resp).await["session_id"]
            .as_str()
            .unwrap()
            .to_string();

        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 9, 8, 7];
        let boundary = "----testBoundary2";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"shot.png\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&png);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/chat-sessions/{session_id}/files"))
                    .header("authorization", format!("Bearer {alice_token}"))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let attachment_id = json(resp).await["id"].as_str().unwrap().to_string();

        // The shared mention row: empty `to`, @-targets in `meta.mentions`.
        let row_body = serde_json::json!({
            "session_id": session_id,
            "message": {
                "seq": 0,
                "ts": 0,
                "from": alice,
                "to": "",
                "kind": "user_message",
                "body": "look at this",
                "meta": { "message_id": "row-1", "mentions": ["agent-1"] },
                "attachment_ids": [attachment_id],
            }
        })
        .to_string();
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/internal/chat-sessions/messages",
                Some(&row_body),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // The mentioned agent pulls the bytes off the shared row.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/internal/chat-sessions/{session_id}/files/{attachment_id}/row-1/agent-1"
                ),
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], &png[..]);

        // A non-mentioned agent is refused — existence is not leaked.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/internal/chat-sessions/{session_id}/files/{attachment_id}/row-1/agent-2"
                ),
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
