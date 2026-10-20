//! User↔user chat HTTP API (ADR-076 §决策 8, §决策 9) — Wave 6.2 adapter.
//!
//! ```text
//! GET  /api/users/{user_id}/chats                             → { chats }
//! GET  /api/users/{user_id}/chats/{chat_id}/messages?offset&limit
//! POST /api/users/{user_id}/chats/{chat_id}/messages          { body, attachments[] }
//! POST /api/users/{user_id}/chats/{chat_id}/read              → 204
//! POST /api/users/{user_id}/chats/{chat_id}/files             multipart → Attachment
//! GET  /api/users/{user_id}/chats/{chat_id}/files/{id}        → blob
//! ```
//!
//! Since Wave 6.2 this is a thin adapter over [`crate::chat_sessions`]:
//! the wire `chat_id` stays the canonical `min__max` pair id (the Desktop
//! computes it client-side with `chatIdOf`), and it resolves to the pair's
//! `UserUser` session through the pair index. The old `chat.rs` layout is
//! folded into the same store at startup (`chat_sessions::migrate_legacy`),
//! so this router and the `/api/chat-sessions*` surface share one
//! authoritative write entry — the chat domain no longer has two.
//!
//! Reads are self-or-admin (the admin "view as user" scope of ADR-076
//! §决策 4); writes are **self only** — an admin may not post as someone
//! else, and `from` always comes from the verified token, never the body.
//!
//! Attachments are uploaded **before** the message that carries them: the
//! client posts the file, gets an id back, then sends a message referencing
//! ids only. Every attribute of a message attachment (name, mime, size) is
//! therefore the service's own record of the bytes it received, and `kind`
//! is derived from that rather than accepted from the caller.
//!
//! Registered only under `AUTH_MODE=multi_user`, alongside `account_api`
//! (ADR-076 §决策 12).

use std::path::PathBuf;

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::auth::context::AuthContext;
use crate::auth::issuer::now_unix;
use crate::auth::service::AuthService;
use crate::chat::{self, Attachment, ChatMessage};
use crate::chat_sessions::{self, Participant, ParticipantIdentity, SessionKind, KIND_USER_MESSAGE};
use crate::error::ApiError;
use crate::state::AppState;

/// Max message length, in characters (ADR-076 §决策 8).
const MAX_BODY_CHARS: usize = 8000;
const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 200;

/// Body cap for the upload route: the largest document plus room for the
/// multipart envelope. The global [`crate::http::GLOBAL_BODY_LIMIT`]
/// (64 MiB) is below the 100 MB documents of ADR-076 §决策 9, so this route
/// raises it for itself rather than lifting the cap for every endpoint.
/// ponytail: the envelope slack is a guess (boundaries + a filename); a file
/// within ~1 MiB of the ceiling can still be rejected by the outer limit.
const UPLOAD_BODY_LIMIT: usize = chat::MAX_DOCUMENT_BYTES as usize + 1024 * 1024;

/// The chat routes (merged only under `multi_user`).
pub fn chat_routes() -> Router<AppState> {
    Router::new()
        .route("/api/users/{user_id}/chats", get(list_chats))
        .route(
            "/api/users/{user_id}/chats/{chat_id}/messages",
            get(list_messages).post(send_message),
        )
        .route("/api/users/{user_id}/chats/{chat_id}/read", post(mark_read))
        .route(
            "/api/users/{user_id}/chats/{chat_id}/files",
            post(upload_attachment),
        )
        .route(
            "/api/users/{user_id}/chats/{chat_id}/files/{attachment_id}",
            get(download_attachment),
        )
        .layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT))
}

// ── Request / response types ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    #[serde(default)]
    pub body: String,
    /// Attachment ids from `POST .../files`. Ids only: the Gateway already
    /// knows the name, mime type and size of what it stored.
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ChatListResponse {
    pub chats: Vec<ChatSummary>,
}

#[derive(Debug, Serialize)]
pub struct ChatSummary {
    pub chat_id: String,
    /// The participant that is not `{user_id}`.
    pub peer_user_id: String,
    /// `display_name` → `username` → raw id. Resolved server-side: a
    /// non-admin caller cannot read `/api/users`, so it has no other way
    /// to label a peer.
    pub peer_display_name: String,
    /// Custom avatar path (mirrors `UserAccount.avatar`). Empty string
    /// means "no custom avatar"; `None` means "we couldn't resolve the
    /// peer's account" (peer account deleted). Frontend treats the
    /// absence-of-custom-avatar the same as no avatar at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_avatar: Option<String>,
    /// Builtin avatar icon id (mirrors `UserAccount.builtin_avatar`).
    /// Same "missing peer" semantics as `peer_avatar`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_builtin_avatar: Option<String>,
    pub last_active_at: i64,
    pub last_message_preview: String,
    pub unread_count: u32,
}

#[derive(Debug, Serialize)]
pub struct MessagesResponse {
    pub chat_id: String,
    /// Chronological within the page, oldest first.
    pub messages: Vec<ChatMessage>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

// ── Plumbing ───────────────────────────────────────────────────────────

fn service(state: &AppState) -> Result<std::sync::Arc<AuthService>, ApiError> {
    state.auth_service.clone().ok_or_else(|| {
        ApiError::service_unavailable("the account system is disabled (AUTH_MODE=local)")
    })
}

fn data_dir(state: &AppState) -> Result<PathBuf, ApiError> {
    Ok(service(state)?.data_dir().to_path_buf())
}

/// Offload a blocking filesystem step off the reactor.
async fn offload<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(&format!("chat task failed: {e}")))?
        .map_err(|m| ApiError::internal(&m))
}

/// Reads: an admin may inspect any user's chats, everyone else only their own.
fn require_self_or_admin(ctx: &AuthContext, user_id: &str) -> Result<(), ApiError> {
    if ctx.is_admin() || ctx.user_id == user_id {
        Ok(())
    } else {
        Err(ApiError::forbidden("not your conversation"))
    }
}

/// Writes: nobody acts as anyone else, not even an admin (ADR-076 §决策 4).
fn require_self(ctx: &AuthContext, user_id: &str) -> Result<(), ApiError> {
    if ctx.user_id == user_id {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "an administrator may read another account's chats but not write as them",
        ))
    }
}

/// Decode the wire `chat_id` and require `user_id` to be one of the pair.
/// A malformed id, a non-participant, or a session that does not exist all
/// answer the same hidden 404 — the caller must not learn which (design 27
/// §5.3, the rule the new store already enforces on every surface).
fn pair_of(chat_id: &str, user_id: &str) -> Result<(String, String), ApiError> {
    let (lo, hi) = chat::parse_chat_id(chat_id)
        .ok_or_else(|| ApiError::not_found("chat not found"))?;
    if lo != user_id && hi != user_id {
        return Err(ApiError::not_found("chat not found"));
    }
    Ok((lo, hi))
}

/// The pair session for a wire `chat_id`, or the same hidden 404. Read
/// paths use this (never `ensure_pair` — a GET must not mint sessions).
async fn resolve_session(
    state: &AppState,
    chat_id: &str,
    user_id: &str,
) -> Result<(PathBuf, String), ApiError> {
    let (lo, hi) = pair_of(chat_id, user_id)?;
    let dir = data_dir(state)?;
    let probe = dir.clone();
    let session = offload(move || Ok(chat_sessions::pair_lookup(&probe, &lo, &hi)))
        .await?
        .ok_or_else(|| ApiError::not_found("chat not found"))?;
    Ok((dir, session.session_id))
}

/// The idempotent write path's session: the pair is created on first send
/// or upload, exactly where the old store created the pair directory.
async fn ensure_session(
    state: &AppState,
    user_id: &str,
    peer: &str,
) -> Result<(PathBuf, String), ApiError> {
    let dir = data_dir(state)?;
    let (a, b) = (user_id.to_string(), peer.to_string());
    let mdir = dir.clone();
    let session = offload(move || {
        chat_sessions::ensure_pair(
            &mdir,
            Participant::new(a, ParticipantIdentity::Human),
            Participant::new(b, ParticipantIdentity::Human),
            SessionKind::UserUser,
            now_unix(),
        )
    })
    .await?;
    Ok((dir, session.session_id))
}

/// Project one durable row onto the legacy wire shape. The old surface
/// never learned about message kinds beyond `text` / `image` / `document`
/// (derived from the first attachment), and `seq` is the physical-line
/// number the suffix-diff refresh keys on — both carry over unchanged.
fn to_wire(m: &chat_sessions::SessionMessage) -> ChatMessage {
    ChatMessage {
        seq: m.seq,
        ts: m.ts,
        from: m.from.clone(),
        kind: m
            .attachments
            .first()
            .map(Attachment::kind)
            .unwrap_or("text")
            .to_string(),
        body: m.body.clone(),
        attachments: m.attachments.clone(),
    }
}

// ── Handlers ───────────────────────────────────────────────────────────

/// `GET /api/users/{user_id}/chats` — conversations, most recent first.
async fn list_chats(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(user_id): Path<String>,
) -> Result<Json<ChatListResponse>, ApiError> {
    require_self_or_admin(&ctx, &user_id)?;
    let auth = service(&state)?;
    let dir = auth.data_dir().to_path_buf();

    // `UserUser` sessions only — the Agent page's conversations live in
    // the same store but are not this surface (design 27 §5.4.4).
    // The preview is write-time derived state on the session record
    // (`commit_row` maintains it per append; [`chat_sessions::ensure_preview`]
    // heals pre-field sessions once), so listing never reads a log.
    let me = user_id.clone();
    let rows = offload(move || {
        let sessions = chat_sessions::list_for(&dir, &me)?;
        let mut out: Vec<(chat_sessions::ChatSession, Option<String>)> = Vec::new();
        for mut s in sessions
            .into_iter()
            .filter(|s| s.kind == SessionKind::UserUser)
        {
            chat_sessions::ensure_preview(&dir, &mut s);
            let preview = s.last_message_preview.clone();
            out.push((s, preview));
        }
        Ok(out)
    })
    .await?;

    // Resolve peer presentation fields once for the whole page. A peer
    // whose account is gone still has history, so the raw id is the
    // last-resort label and the avatar fields stay None (frontend
    // falls back to a deterministic builtin icon from the display
    // name, which is the same UX the user has in `/api/users/directory`).
    let accounts = auth.load_accounts().map_err(|e| ApiError::internal(&e))?;
    struct PeerMeta<'a> {
        label: &'a str,
        avatar: Option<&'a str>,
        builtin_avatar: Option<&'a str>,
    }
    let peers: std::collections::HashMap<&str, PeerMeta<'_>> = accounts
        .accounts
        .iter()
        .map(|a| {
            let label = if a.display_name.trim().is_empty() {
                a.username.as_str()
            } else {
                a.display_name.as_str()
            };
            // An empty string is the wire contract for "user cleared this
            // field"; treat it as missing so the frontend picks its
            // deterministic fallback instead of trying to load "".
            let avatar = a.avatar.as_deref().filter(|s| !s.is_empty());
            let builtin_avatar = a.builtin_avatar.as_deref().filter(|s| !s.is_empty());
            (
                a.user_id.as_str(),
                PeerMeta {
                    label,
                    avatar,
                    builtin_avatar,
                },
            )
        })
        .collect();

    let mut chats: Vec<ChatSummary> = rows
        .into_iter()
        .filter_map(|(s, last)| {
            let peer_user_id = s
                .participants
                .iter()
                .map(|p| p.id.clone())
                .find(|id| *id != user_id)?;
            let meta = peers.get(peer_user_id.as_str());
            let peer_display_name = meta
                .map(|m| m.label.to_string())
                .unwrap_or_else(|| peer_user_id.clone());
            let peer_avatar = meta.and_then(|m| m.avatar).map(str::to_string);
            let peer_builtin_avatar = meta
                .and_then(|m| m.builtin_avatar)
                .map(str::to_string);
            let last_message_preview = last.unwrap_or_default();
            Some(ChatSummary {
                chat_id: chat::chat_id(&user_id, &peer_user_id),
                unread_count: s.unread.get(&user_id).copied().unwrap_or(0),
                peer_user_id,
                peer_display_name,
                peer_avatar,
                peer_builtin_avatar,
                last_active_at: s.last_active_at,
                last_message_preview,
            })
        })
        .collect();
    chats.sort_by_key(|c| std::cmp::Reverse(c.last_active_at));
    Ok(Json(ChatListResponse { chats }))
}

/// `GET /api/users/{user_id}/chats/{chat_id}/messages` — one page, newest
/// window first-in-file order (the paging contract of the store).
async fn list_messages(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((user_id, chat_id)): Path<(String, String)>,
    Query(page): Query<PageQuery>,
) -> Result<Json<MessagesResponse>, ApiError> {
    require_self_or_admin(&ctx, &user_id)?;
    let (dir, session_id) = resolve_session(&state, &chat_id, &user_id).await?;

    let limit = page.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let offset = page.offset;
    let (messages, total) =
        offload(move || chat_sessions::read_messages(&dir, &session_id, offset, limit)).await?;

    Ok(Json(MessagesResponse {
        chat_id,
        messages: messages.iter().map(to_wire).collect(),
        total: total as usize,
        offset,
        limit,
    }))
}

/// `POST /api/users/{user_id}/chats/{chat_id}/messages` — send as `user_id`.
async fn send_message(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((user_id, chat_id)): Path<(String, String)>,
    Json(req): Json<SendMessageRequest>,
) -> Result<(StatusCode, Json<ChatMessage>), ApiError> {
    require_self(&ctx, &user_id)?;
    let (lo, hi) = pair_of(&chat_id, &user_id)?;
    let peer = if user_id == lo { hi } else { lo };

    let body = req.body.trim().to_string();
    if body.is_empty() && req.attachments.is_empty() {
        return Err(ApiError::unprocessable_entity(
            "a message needs a body or an attachment",
        ));
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(ApiError::unprocessable_entity(&format!(
            "message body exceeds {MAX_BODY_CHARS} characters"
        )));
    }
    // A peer whose account is gone has no inbox to deliver into.
    service(&state)?
        .account(&peer)
        .map_err(|_| ApiError::not_found("recipient account not found"))?;

    let (dir, session_id) = ensure_session(&state, &user_id, &peer).await?;
    // Reported as the caller's mistake before the write, not as a fault
    // after it. The store re-checks at the write entry (single caller).
    {
        let unknown = req
            .attachments
            .iter()
            .any(|id| !chat_sessions::attachment_exists(&dir, &session_id, id));
        if unknown {
            return Err(ApiError::unprocessable_entity(
                "message references an attachment that was not uploaded here",
            ));
        }
    }
    let msg = chat_sessions::SessionMessage {
        seq: 0,
        ts: 0,
        from: user_id.clone(),
        kind: KIND_USER_MESSAGE.to_string(),
        body,
        to: peer.clone(),
        meta: Default::default(),
        attachment_ids: req.attachments,
        attachments: Vec::new(),
    };
    let (stored, _fresh) =
        offload(move || chat_sessions::append_message(&dir, &session_id, &msg, now_unix()))
            .await?;
    // The store's append already doorbelled the peer. The sender's OTHER
    // devices have an echo to show — the device that posted this already
    // holds it, which is why the response is the stored record (design 25
    // §8). An empty signal by design: no ids, no body.
    crate::mqtt_publisher::notify_chat_changed(&user_id);
    Ok((StatusCode::CREATED, Json(to_wire(&stored))))
}

/// `POST /api/users/{user_id}/chats/{chat_id}/files` — upload an attachment.
///
/// One `file` field. The declared `Content-Type` picks the ceiling (image vs
/// document); it is sanitised before it is stored, because it is echoed back
/// as a response header on download.
async fn upload_attachment(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((user_id, chat_id)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<Attachment>), ApiError> {
    require_self(&ctx, &user_id)?;
    let (lo, hi) = pair_of(&chat_id, &user_id)?;
    let peer = if user_id == lo { hi } else { lo };
    // Upload before the first message is normal, but the recipient still
    // has to exist — otherwise the file can never be delivered.
    service(&state)?
        .account(&peer)
        .map_err(|_| ApiError::not_found("recipient account not found"))?;

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
    if bytes.is_empty() {
        return Err(ApiError::unprocessable_entity("attachment is empty"));
    }

    // Checked here as well as in the store so the caller gets a 413 instead
    // of a generic 500; `limit_for` is the single source of the ceiling.
    let limit = chat::limit_for(&mime);
    if bytes.len() as u64 > limit {
        return Err(ApiError::payload_too_large(&format!(
            "attachment exceeds {limit} bytes"
        )));
    }

    // Uploading before the first message is normal: the pair session is
    // minted here exactly where the old store minted the pair directory.
    let (dir, session_id) = ensure_session(&state, &user_id, &peer).await?;
    let attachment = offload(move || {
        chat_sessions::store_attachment(&dir, &session_id, &user_id, &filename, &mime, &bytes)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(attachment)))
}

/// `GET /api/users/{user_id}/chats/{chat_id}/files/{attachment_id}`.
///
/// Served as `Content-Disposition: attachment` with `nosniff`: the stored
/// mime type is client-supplied, and a document that renders as HTML on the
/// Gateway's own origin would be a live script with the caller's token in
/// reach of it. Images keep their type so the Desktop can still show them
/// inline — `<img>` with a disposition of `attachment` renders fine.
///
/// `?thumb=1` serves the generated thumbnail sidecar instead (ADR-076
/// §决策 9), which is what the chat row renders. Same auth, same `nosniff`
/// disposition, but the mime is the *derived* one (jpeg / plain text), not
/// the client-supplied original — the sidecar is written by this service.
#[derive(Debug, Deserialize, Default)]
pub struct ThumbQuery {
    /// Accepts `1` as well as `true`, because `?thumb=1` is the flag spelling
    /// the rest of the platform already uses (acowork-pm's attachment
    /// download). A plain `bool` here rejected `1` outright — serde's bool
    /// deserializer wants the literal `true` / `false` — and the request came
    /// back 400 *before any handler code ran*, which from the outside is
    /// indistinguishable from "the thumbnail route is broken".
    #[serde(default, deserialize_with = "flag")]
    pub thumb: bool,
}

/// `true` / `1` / `yes` / `on` (any case) → true; absent or anything else →
/// false.
///
/// Deserialized as a string because the query value arrives quoted in some
/// clients and bare in others, and a flag is not a value worth rejecting a
/// request over: the caller's fallback (fetch the whole blob) is always
/// correct, so an unrecognised spelling must be no worse than sending none.
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

/// Chunk size for `Body::from_stream` reads. 64 KiB is the sweet spot
/// for our workload: large enough that a 7.5 MB PDF still gets ~120
/// chunks (visible progress, no per-chunk IPC overhead worth caring
/// about), small enough that the kernel can hand us a 64 KiB slice
/// without us holding the whole 100 MiB attachment in the request
/// worker's heap. Aligned with the typical TCP send buffer (16 KiB × 4).
const DOWNLOAD_CHUNK_BYTES: usize = 64 * 1024;

async fn download_attachment(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((user_id, chat_id, attachment_id)): Path<(String, String, String)>,
    Query(q): Query<ThumbQuery>,
) -> Result<Response, ApiError> {
    require_self_or_admin(&ctx, &user_id)?;
    let (dir, session_id) = resolve_session(&state, &chat_id, &user_id).await?;

    // Metadata-only load — keeps the request worker from pinning the
    // blob in heap. The blob is streamed straight from disk to socket
    // below via `ReaderStream`.
    let (attachment, blob_path) = match offload({
        let (d, s, id) = (dir.clone(), session_id.clone(), attachment_id.clone());
        move || chat_sessions::load_attachment(&d, &s, &id)
    })
    .await?
    {
        Some(pair) => pair,
        None => return Err(ApiError::not_found("attachment not found")),
    };

    // `?thumb=1` swaps the blob for its sidecar. The sidecar path is
    // rebuilt from the already-validated id against a closed extension set
    // (see `chat_sessions::thumb_sidecar`) — nothing from the query or the
    // attachment record reaches the filesystem unchecked.
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

    // Stream the blob through `ReaderStream`. `Body::from_stream`
    // forces chunked transfer encoding, so hyper will not wait for the
    // whole file before sending headers — the browser / Tauri WebView
    // sees real `data:` chunks and can drive a progress bar from the
    // growing `response.body`.
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ApiError::internal(&format!("open {}: {e}", path.display())))?;
    // `ReaderStream` is re-exported by `tokio-util`, not `tokio-stream`
    // (tokio-stream 0.1.x deliberately defers `AsyncRead`/`AsyncWrite`
    // adapters to `tokio-util`); gated behind the `io` feature there.
    let stream = tokio_util::io::ReaderStream::with_capacity(file, DOWNLOAD_CHUNK_BYTES);
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    // Both values were sanitised on the way in, so these cannot fail; the
    // fallbacks keep a corrupt record from turning into a 500 on download.
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    let disposition = chat::content_disposition(&attachment.filename);
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    // No `Content-Length` — `Body::from_stream` produces a body of
    // unknown length, and axum will reject the response at runtime if
    // we try to pair the two. hyper falls back to chunked encoding,
    // which is what we want anyway (lets the client start rendering the
    // download before we've read the whole file).
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/// `POST /api/users/{user_id}/chats/{chat_id}/read` — clear own unread count.
async fn mark_read(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((user_id, chat_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_self(&ctx, &user_id)?;
    let (dir, session_id) = resolve_session(&state, &chat_id, &user_id).await?;
    offload(move || chat_sessions::mark_read(&dir, &session_id, &user_id).map(|_| ())).await?;
    // The store doorbelled only when the counter actually changed — badge
    // clears on every device of this account, and a no-op read stays silent
    // so the refetch→mark_read cycle cannot re-signal forever.
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_router;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use std::path::Path;
    use tower::ServiceExt;

    const PWD: &str = "s3cret123";

    fn temp_dir() -> PathBuf {
        crate::test_support::temp_dir("chatapi")
    }

    /// Named `multi_user_state` in the Gateway's copy, but it always seeded
    /// the `root` admin — which is what the chat fixtures log in as.
    fn multi_user_state(dir: &Path) -> AppState {
        crate::test_support::admin_state(dir)
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
                Some(&format!(
                    r#"{{"username":"{username}","password":"{password}"}}"#
                )),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "login as {username}");
        json(resp).await["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Admin (`root`) plus `alice` and `bob`, all with passwords. Ids are
    /// minted by the server, so they are read back from the create response.
    struct Fixture {
        _dir: PathBuf,
        router: Router,
        admin: String,
        alice: String,
        alice_token: String,
        bob: String,
        bob_token: String,
    }

    async fn fixture() -> Fixture {
        let dir = temp_dir();
        let router = build_router(multi_user_state(&dir));
        let admin = login(&router, "root", PWD).await;
        let mut ids = Vec::new();
        // CreateAccountRequest doesn't accept presentation fields
        // (ADR-076 §决策 1 keeps them on UserAccount, gated by the
        // auth path), so we set avatars with PUT after creation.
        for name in ["alice", "bob"] {
            let resp = router
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/users",
                    Some(&format!(
                        r#"{{"username":"{name}","display_name":"{name}","password":"{PWD}"}}"#
                    )),
                    Some(&admin),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED, "create {name}");
            ids.push(
                json(resp).await["account"]["user_id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        let [alice, bob]: [String; 2] = ids.try_into().unwrap();
        assert_ne!(alice, bob);
        // Avatar values per user — Alice has only a builtin icon, Bob
        // has both a builtin icon AND a custom file. The chat list
        // endpoint must surface both fields, so the per-field
        // assertions in `send_read_and_list_across_two_accounts`
        // catch regressions when one half of the resolution is wrong.
        let alice_avatar = router
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/users/{alice}"),
                Some(r#"{"builtin_avatar":"icon-05"}"#),
                Some(&admin),
            ))
            .await
            .unwrap();
        assert_eq!(alice_avatar.status(), StatusCode::OK, "put alice avatar");
        let bob_avatar = router
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/users/{bob}"),
                Some(r#"{"builtin_avatar":"icon-07","avatar":"assets/avatar-bob.png"}"#),
                Some(&admin),
            ))
            .await
            .unwrap();
        assert_eq!(bob_avatar.status(), StatusCode::OK, "put bob avatar");
        let alice_token = login(&router, "alice", PWD).await;
        let bob_token = login(&router, "bob", PWD).await;
        Fixture {
            _dir: dir,
            router,
            admin,
            alice,
            alice_token,
            bob,
            bob_token,
        }
    }

    fn chats_uri(user: &str) -> String {
        format!("/api/users/{user}/chats")
    }

    fn messages_uri(user: &str, peer: &str) -> String {
        format!(
            "/api/users/{user}/chats/{}/messages",
            chat::chat_id(user, peer)
        )
    }

    fn files_uri(user: &str, peer: &str) -> String {
        format!(
            "/api/users/{user}/chats/{}/files",
            chat::chat_id(user, peer)
        )
    }

    const BOUNDARY: &str = "----acowork";

    /// A one-field multipart form naming the file, as a browser would send it.
    fn multipart(field: &str, filename: &str, mime: &str, bytes: &[u8]) -> Vec<u8> {
        let mut body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{field}\"; \
             filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    fn upload_req(uri: &str, body: Vec<u8>, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            );
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body)).unwrap()
    }

    #[tokio::test]
    async fn send_read_and_list_across_two_accounts() {
        let f = fixture().await;
        let (router, alice, bob, admin) = (&f.router, &f.alice, &f.bob, &f.admin);
        let (alice_token, bob_token) = (&f.alice_token, &f.bob_token);

        // Alice → Bob.
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, bob),
                Some(r#"{"body":"  hello bob  "}"#),
                Some(alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let sent = json(resp).await;
        assert_eq!(sent["from"], alice.as_str(), "from is the token identity");
        assert_eq!(sent["body"], "hello bob", "body is trimmed");
        assert_eq!(sent["seq"], 1, "the send echo carries the assigned seq (ADR-092 decision 1)");

        // Bob sees one unread; Alice sees none.
        let resp = router
            .clone()
            .oneshot(req("GET", &chats_uri(bob), None, Some(bob_token)))
            .await
            .unwrap();
        let list = json(resp).await;
        assert_eq!(list["chats"][0]["peer_user_id"], alice.as_str());
        assert_eq!(
            list["chats"][0]["peer_display_name"], "alice",
            "peer label is resolved server-side for non-admin callers"
        );
        // Alice's avatar fields surface to Bob's chat list — Alice has
        // only a builtin icon, no custom file.
        assert_eq!(
            list["chats"][0]["peer_builtin_avatar"], "icon-05",
            "builtin avatar is propagated so the inbox can render Alice's icon"
        );
        assert!(
            list["chats"][0].get("peer_avatar").is_none()
                || list["chats"][0]["peer_avatar"].is_null(),
            "absent custom avatar stays absent (skip_serializing_if + missing field)"
        );
        assert_eq!(list["chats"][0]["unread_count"], 1);
        assert_eq!(list["chats"][0]["last_message_preview"], "hello bob");

        let resp = router
            .clone()
            .oneshot(req("GET", &chats_uri(alice), None, Some(alice_token)))
            .await
            .unwrap();
        assert_eq!(json(resp).await["chats"][0]["unread_count"], 0);

        // Bob reads it.
        let read_uri = format!("/api/users/{bob}/chats/{}/read", chat::chat_id(alice, bob));
        let resp = router
            .clone()
            .oneshot(req("POST", &read_uri, None, Some(bob_token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = router
            .clone()
            .oneshot(req("GET", &chats_uri(bob), None, Some(bob_token)))
            .await
            .unwrap();
        assert_eq!(json(resp).await["chats"][0]["unread_count"], 0);

        // History reads back in chronological order.
        let resp = router
            .clone()
            .oneshot(req("GET", &messages_uri(alice, bob), None, Some(alice_token)))
            .await
            .unwrap();
        let page = json(resp).await;
        assert_eq!(page["total"], 1);
        assert_eq!(page["messages"][0]["body"], "hello bob");
        assert_eq!(page["messages"][0]["seq"], 1, "GET pages carry the stable seq");

        // The admin "view as user" read scope reaches the same history.
        let resp = router
            .clone()
            .oneshot(req("GET", &chats_uri(alice), None, Some(admin)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let alice_view = json(resp).await;
        assert_eq!(alice_view["chats"][0]["peer_user_id"], bob.as_str());
        // Bob's chat list (from Alice's view) surfaces both avatar
        // fields — custom file *and* builtin icon — so the frontend
        // can prefer the custom file and fall back to the icon.
        assert_eq!(
            alice_view["chats"][0]["peer_avatar"], "assets/avatar-bob.png",
            "custom avatar path is propagated to chat list"
        );
        assert_eq!(
            alice_view["chats"][0]["peer_builtin_avatar"], "icon-07",
            "builtin avatar id is propagated alongside custom avatar"
        );
    }

    /// A send is a wake-up for BOTH mailboxes; a read for one. And the signal
    /// never names the thread: a `chat_id` is `min__max` of the two account
    /// ids, so publishing it would hand the whole contact graph to every
    /// account able to subscribe `acowork/users/#` (the embedded broker has no
    /// subscription ACL — its only hook is CONNECT).
    ///
    /// Asserted as *contains* over the window rather than an exact slice: the
    /// test sink is process-wide and cargo runs tests on several threads.
    #[tokio::test]
    async fn send_wakes_both_mailboxes_and_read_wakes_only_the_reader() {
        use acowork_core::mqtt_proto::user_chat_changed_topic;

        let f = fixture().await;
        let (router, alice, bob) = (&f.router, &f.alice, &f.bob);

        let before = crate::mqtt_publisher::test_signals().len();
        router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, bob),
                Some(r#"{"body":"hi"}"#),
                Some(&f.alice_token),
            ))
            .await
            .unwrap();
        let after_send = crate::mqtt_publisher::test_signals();
        let sent = &after_send[before..];
        assert!(
            sent.contains(&user_chat_changed_topic(alice)),
            "the sender's OTHER devices have an echo to show; only the device \
             that posted the message already holds it"
        );
        assert!(
            sent.contains(&user_chat_changed_topic(bob)),
            "the recipient is the whole point of the signal"
        );
        assert!(
            after_send.iter().all(|t| !t.contains("__")),
            "a `chat_id` in a signal topic would publish who talks to whom"
        );

        // Opening the thread clears the badge on every device of THIS account,
        // and must not ping the peer: a read receipt is not a message.
        let before = after_send.len();
        router
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/users/{bob}/chats/{}/read", chat::chat_id(alice, bob)),
                None,
                Some(&f.bob_token),
            ))
            .await
            .unwrap();
        let after_read = crate::mqtt_publisher::test_signals();
        let read = &after_read[before..];
        assert!(
            read.contains(&user_chat_changed_topic(bob)),
            "otherwise a desktop keeps a lit dot until its next pull"
        );
        assert!(
            !read.contains(&user_chat_changed_topic(alice)),
            "Alice has nothing to refresh: her own unread did not change"
        );
    }

    #[tokio::test]
    async fn third_party_and_admin_cannot_write_as_someone_else() {
        let f = fixture().await;
        let (router, alice, bob, admin) = (&f.router, &f.alice, &f.bob, &f.admin);
        router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, bob),
                Some(r#"{"body":"hi"}"#),
                Some(&f.alice_token),
            ))
            .await
            .unwrap();

        // Carol can log in but is not a participant of the alice__bob chat.
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/users",
                Some(r#"{"username":"carol","display_name":"carol","password":"s3cret123"}"#),
                Some(admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let carol_id = json(resp).await["account"]["user_id"]
            .as_str()
            .unwrap()
            .to_string();
        let carol = login(router, "carol", PWD).await;

        // Carol asking for *alice's* view is forbidden (it is not her view).
        let resp = router
            .clone()
            .oneshot(req("GET", &messages_uri(alice, bob), None, Some(&carol)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Through her own view the alice__bob chat is not hers → 404.
        let resp = router
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/api/users/{carol_id}/chats/{}/messages",
                    chat::chat_id(alice, bob)
                ),
                None,
                Some(&carol),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = router
            .clone()
            .oneshot(req("GET", &messages_uri(alice, bob), None, Some(&f.bob_token)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "bob asking for alice's own view is not his"
        );

        // An admin cannot post as alice (write scope is self-only).
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, bob),
                Some(r#"{"body":"impersonated"}"#),
                Some(admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn rejects_self_chat_empty_body_and_missing_token() {
        let f = fixture().await;
        let (router, alice, bob) = (&f.router, &f.alice, &f.bob);
        let alice_token = &f.alice_token;

        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, alice),
                Some(r#"{"body":"hi me"}"#),
                Some(alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "self chat is not a chat"
        );

        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, bob),
                Some(r#"{"body":"   "}"#),
                Some(alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, bob),
                Some(r#"{"body":"x"}"#),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "no token, no chat");
    }

    #[tokio::test]
    async fn attachment_round_trips_from_upload_to_download() {
        let f = fixture().await;
        let (router, alice, bob, admin) = (&f.router, &f.alice, &f.bob, &f.admin);
        let (alice_token, bob_token) = (&f.alice_token, &f.bob_token);
        let png = b"\x89PNG\r\n\x1a\nnot really a png";

        // Upload, then reference the id — never the name or the size.
        let resp = router
            .clone()
            .oneshot(upload_req(
                &files_uri(alice, bob),
                multipart("file", "设计稿 v2.png", "image/png", png),
                Some(alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let att = json(resp).await;
        let id = att["id"].as_str().unwrap().to_string();
        assert_eq!(att["filename"], "设计稿 v2.png");
        assert_eq!(att["mime"], "image/png");
        assert_eq!(att["size"], png.len());

        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, bob),
                Some(&format!(r#"{{"body":"","attachments":["{id}"]}}"#)),
                Some(alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let sent = json(resp).await;
        assert_eq!(sent["kind"], "image", "kind is derived, not sent");
        assert_eq!(sent["attachments"][0]["filename"], "设计稿 v2.png");
        // The chat list shows something even for a caption-less image.
        let resp = router
            .clone()
            .oneshot(req("GET", &chats_uri(bob), None, Some(bob_token)))
            .await
            .unwrap();
        assert_eq!(
            json(resp).await["chats"][0]["last_message_preview"],
            "[image] 设计稿 v2.png"
        );

        // Bob downloads it through his own view; the bytes are unchanged.
        let url = format!("{}/{id}", files_uri(bob, alice));
        let resp = router
            .clone()
            .oneshot(req("GET", &url, None, Some(bob_token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "image/png"
        );
        assert_eq!(
            resp.headers().get("x-content-type-options").unwrap(),
            "nosniff",
            "a stored mime type is client-supplied; never let a browser sniff it"
        );
        let disposition = resp
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            disposition.starts_with("attachment;"),
            "download, never inline render: {disposition}"
        );
        assert!(disposition.contains("filename*=UTF-8''%E8%AE%BE%E8%AE%A1%E7%A8%BF%20v2.png"));
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&body[..], png);

        // Admin read scope reaches it too (ADR-076 §决策 4).
        let resp = router
            .clone()
            .oneshot(req(
                "GET",
                &format!("{}/{id}", files_uri(alice, bob)),
                None,
                Some(admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `?thumb=1` must serve the sidecar, and must not 400.
    ///
    /// The flag spelling is the whole point of this test: a `bool` field
    /// rejects `1` at the extractor, so the request failed with 400 *before
    /// any handler line ran* — indistinguishable from a dead route when you
    /// only have the status code. That is exactly how the thumbnail shipped
    /// looking broken.
    #[tokio::test]
    async fn the_thumb_flag_serves_the_sidecar_for_every_spelling() {
        let f = fixture().await;
        let (router, alice, bob) = (&f.router, &f.alice, &f.bob);
        let (alice_token, bob_token) = (&f.alice_token, &f.bob_token);

        // A real image, so the service actually produces a JPEG sidecar —
        // a junk header would store a blob with `thumb: None` and the route
        // would (correctly) 404, proving nothing about the flag.
        let img = image::ImageBuffer::from_fn(4, 4, |_, _| image::Rgba([0, 128, 255, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let resp = router
            .clone()
            .oneshot(upload_req(
                &files_uri(alice, bob),
                multipart("file", "chart.png", "image/png", &png),
                Some(alice_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let att = json(resp).await;
        let id = att["id"].as_str().unwrap().to_string();
        assert_eq!(att["thumb"], "jpg", "an image upload records its sidecar");

        let url = format!("{}/{id}", files_uri(bob, alice));

        // The spelling the client uses, plus the others a caller might pick.
        for flag in ["thumb=1", "thumb=true", "thumb=yes", "thumb=on"] {
            let resp = router
                .clone()
                .oneshot(req("GET", &format!("{url}?{flag}"), None, Some(bob_token)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{flag} must serve the sidecar");
            assert_eq!(
                resp.headers().get("content-type").unwrap(),
                "image/jpeg",
                "{flag} must report the derived mime, not the stored one"
            );
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            assert_eq!(&body[..3], &[0xFF, 0xD8, 0xFF], "{flag} must be the JPEG");
            // And it is the *thumbnail*, not the original re-labelled: the
            // unflagged request below returns these exact `png` bytes.
            assert_ne!(&body[..], &png[..], "{flag} must not be the original blob");
        }

        // No flag: the original, with the client-supplied mime.
        let resp = router
            .clone()
            .oneshot(req("GET", &url, None, Some(bob_token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&body[..], &png[..], "unflagged must be the original bytes");

        // A nonsense flag is "no thumbnail", never a 400 — the caller's
        // fallback (fetch the whole blob) is always correct, so an
        // unrecognised spelling must not be worse than sending none.
        let resp = router
            .clone()
            .oneshot(req("GET", &format!("{url}?thumb=maybe"), None, Some(bob_token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");

        // The sidecar is still behind the same auth as the blob: a
        // non-participant must not reach it by adding a query parameter.
        let resp = router
            .clone()
            .oneshot(req("GET", &format!("{url}?thumb=1"), None, Some(&f.admin)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "admin read scope (ADR-076 §决策 4)");
        let stranger = format!("{}/{id}", files_uri(&f.admin, &f.bob));
        let resp = router
            .clone()
            .oneshot(req("GET", &format!("{stranger}?thumb=1"), None, Some(bob_token)))
            .await
            .unwrap();
        // 403, not 404: `require_self_or_admin` rejects Bob before the
        // chat is even resolved. Either way the sidecar is not served — the
        // point of the assertion is that a query parameter is not a way
        // around the authz the blob itself is behind.
        assert!(
            resp.status() == StatusCode::FORBIDDEN
                || resp.status() == StatusCode::NOT_FOUND,
            "a chat Bob is not in must not serve its sidecar, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn attachment_writes_are_self_only_and_reads_are_scoped() {
        let f = fixture().await;
        let (router, alice, bob) = (&f.router, &f.alice, &f.bob);
        let (alice_token, bob_token) = (&f.alice_token, &f.bob_token);

        // An admin uploading "as alice" is still alice's own token or nothing.
        let resp = router
            .clone()
            .oneshot(upload_req(
                &files_uri(alice, bob),
                multipart("file", "x.png", "image/png", b"png"),
                Some(&f.admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = router
            .clone()
            .oneshot(upload_req(
                &files_uri(alice, bob),
                multipart("file", "x.png", "image/png", b"png"),
                Some(alice_token),
            ))
            .await
            .unwrap();
        let id = json(resp).await["id"].as_str().unwrap().to_string();

        // A third party cannot reach it, and cannot even tell it exists.
        let carol = {
            let resp = router
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/users",
                    Some(r#"{"username":"carol","display_name":"carol","password":"s3cret123"}"#),
                    Some(&f.admin),
                ))
                .await
                .unwrap();
            json(resp).await["account"]["user_id"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let carol_token = login(router, "carol", PWD).await;
        let resp = router
            .clone()
            .oneshot(req(
                "GET",
                &format!("{}/{id}", files_uri(&carol, alice)),
                None,
                Some(&carol_token),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "a non-participant must not learn that this attachment exists"
        );

        // A well-formed but unknown id answers the same way.
        let resp = router
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "{}/00000000-0000-4000-8000-000000000000",
                    files_uri(bob, alice)
                ),
                None,
                Some(bob_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // And a traverse-y id is not a path.
        let resp = router
            .clone()
            .oneshot(req(
                "GET",
                &format!("{}/..%2Fconversation.json", files_uri(bob, alice)),
                None,
                Some(bob_token),
            ))
            .await
            .unwrap();
        assert!(
            resp.status().is_client_error(),
            "expected a 4xx, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn attachment_size_and_kind_are_enforced_server_side() {
        let f = fixture().await;
        let (router, alice, bob) = (&f.router, &f.alice, &f.bob);
        let token = &f.alice_token;

        // Empty payload → 422, not a stored zero-byte attachment.
        let resp = router
            .clone()
            .oneshot(upload_req(
                &files_uri(alice, bob),
                multipart("file", "empty.png", "image/png", b""),
                Some(token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // Over the image ceiling → 413. 25 MiB, so the global body limit is
        // not what rejects it — the per-kind check is.
        let too_big = vec![0u8; chat::MAX_IMAGE_BYTES as usize + 1];
        let resp = router
            .clone()
            .oneshot(upload_req(
                &files_uri(alice, bob),
                multipart("file", "big.png", "image/png", &too_big),
                Some(token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        drop(too_big);

        // No `file` field at all.
        let resp = router
            .clone()
            .oneshot(upload_req(
                &files_uri(alice, bob),
                multipart("nope", "x.png", "image/png", b"png"),
                Some(token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // A message may not reference an id it never uploaded.
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, bob),
                Some(
                    r#"{"body":"","attachments":["00000000-0000-4000-8000-000000000000"]}"#,
                ),
                Some(token),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a made-up attachment id is the caller's mistake, not a server fault"
        );

        // No body and no attachment is still not a message.
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                &messages_uri(alice, bob),
                Some(r#"{"body":"   "}"#),
                Some(token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// ADR-076 §决策 9 allows 100 MB documents, but the Gateway's root
    /// router caps every body at [`GLOBAL_BODY_LIMIT`] (64 MiB). The upload
    /// route raises that for itself only — this is the regression guard for
    /// that override, since a silently-restored 64 MiB would only ever show
    /// up as a 413 on a large file.
    #[tokio::test]
    async fn the_upload_route_raises_the_global_body_limit() {
        let f = fixture().await;
        let (router, alice, bob) = (&f.router, &f.alice, &f.bob);

        // A field nobody reads is still bytes off the wire: the parser drains
        // it, so this body must clear the body limit without storing 65 MiB.
        let mut body = vec![b'x'; crate::http::GLOBAL_BODY_LIMIT + 1024 * 1024];
        body.extend_from_slice(&multipart("file", "small.txt", "text/plain", b"ok"));

        let resp = router
            .clone()
            .oneshot(upload_req(&files_uri(alice, bob), body, Some(&f.alice_token)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "the upload route must accept bodies over the global 64 MiB cap"
        );
        assert_eq!(json(resp).await["size"], 2);
    }
}
