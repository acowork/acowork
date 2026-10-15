//! Session control plane over HTTP (ADR-076 §决策 4).
//!
//! Session **writes** used to be MQTT-only (`acowork/agents/{id}/sessions/
//! control/{cmd}`). They moved here so the Gateway's auth middleware can
//! authenticate the caller — an MQTT control message carries no identity
//! (the broker has no way to stamp one; see ADR-076 §决策 4 implementation
//! notes), so `create` could not record an owner and `close`/`delete`
//! could not check one.
//!
//! **The Gateway is the only trusted source of identity.** Every request
//! arrives through the reverse proxy with `x-user-id` already stripped of
//! any client-supplied value and re-derived from the caller's access token
//! (`acowork-gateway`'s `auth_middleware`). In `AUTH_MODE=local` the header
//! is absent, which means "no account system" — unfiltered, exactly as
//! before this module existed.
//!
//! Writes are *not* the same as reads: a public session is visible to
//! every account but owned by one. See [`SessionMeta::is_writable_by`].

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};

use std::sync::Arc;

use crate::agent::inbound::InboundMessage;
use crate::conversation::{SessionMeta, SessionScope, SessionVisibility};
use crate::http::server::HttpState;

/// The header the Gateway uses to hand the Runtime the caller's scope.
///
/// Kept in sync with `acowork-gateway`'s `auth_middleware::USER_SCOPE_HEADER`.
pub(crate) const USER_SCOPE_HEADER: &str = "x-user-id";

/// Body of `POST /sessions` — all fields optional.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct CreateSessionBody {
    #[serde(default)]
    pub(crate) workspace_id: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) provider: Option<String>,
    /// ADR-076 §决策 4: omit (or `null`) for the public default.
    #[serde(default)]
    pub(crate) visibility: Option<SessionVisibility>,
}

/// Body of `PUT /sessions/{sid}/visibility`.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct SetVisibilityBody {
    /// `null` clears the field, restoring the public default.
    pub(crate) visibility: Option<SessionVisibility>,
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn err(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

/// Derive the caller's scope from the Gateway-injected header.
pub(crate) fn scope_from_headers(headers: &HeaderMap) -> SessionScope {
    SessionScope::from_header_value(headers.get(USER_SCOPE_HEADER).and_then(|v| v.to_str().ok()))
}

/// Read a session's meta off disk, mapping "not found" to 404.
pub(crate) fn load_meta(state: &HttpState, sid: &str) -> Result<SessionMeta, ApiError> {
    crate::conversation::read_session_meta(&state.work_dir.join("conversations"), sid)
        .map_err(|_| not_found(sid))
}

/// 404 for both "no such session" and "not yours".
///
/// Distinguishing them would turn this endpoint into an existence oracle
/// for sessions the caller cannot read — the same information the
/// listing filter is there to withhold.
fn not_found(sid: &str) -> ApiError {
    err(
        StatusCode::NOT_FOUND,
        format!("session not found: {sid}"),
    )
}

/// `POST /intent-context/close` — design 27 §2.1 T2-a: a PM task reached a
/// terminal state, so its logical execution context ends. The Gateway
/// (acting for the PM service) resolves the target instance and calls this
/// with the registry key (`conv:task:{project}:{task}`); the Runtime closes
/// the mapped execution session and drops the mapping. Idempotent.
///
/// Authorization: Gateway-proxied calls carry the hosting node's
/// `X-ACowork-Node-Token` (stamped by the reverse proxy, the same machine
/// credential the node HTTP surface checks). When the Runtime has a node
/// token configured, a mismatch is rejected; in local mode (no token) the
/// loopback bind is the boundary — the same tiering every other Runtime
/// surface applies.
pub(crate) async fn post_close_intent_context(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let expected = state
        .agent_core
        .read()
        .ok()
        .and_then(|g| g.as_ref().and_then(|c| c.config.mqtt_password.clone()))
        .filter(|t| !t.is_empty());
    if let Some(expected) = expected {
        let provided = headers
            .get(acowork_core::auth::NODE_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            return Err(err(
                StatusCode::UNAUTHORIZED,
                "intent context close requires the node token",
            ));
        }
    }
    let key = req
        .get("key")
        .and_then(|v| v.as_str())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "key is required"))?;
    let sm = session_manager(&state).await?;
    sm.lock()
        .await
        .close_intent_context(key)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "status": "closed", "key": key })))
}

/// Timing-unsafe comparison would leak token bytes over loopback only in
/// theory; keep the node-proxy pattern anyway (same shape as
/// `acowork-node`'s `constant_time_eq`).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Authorize a **read** of `sid`. Returns the meta on success.
pub(crate) fn authorize_read(
    state: &HttpState,
    sid: &str,
    scope: &SessionScope,
) -> Result<SessionMeta, ApiError> {
    let meta = load_meta(state, sid)?;
    if meta.is_readable_by(scope) {
        Ok(meta)
    } else {
        Err(not_found(sid))
    }
}

/// ADR-076 §决策 4: may this caller change who can read `meta`?
///
/// Sharing is the owner's decision, and an unclaimed session has no owner
/// to make it. [`authorize_write`] only asks whether the caller may
/// *modify* the session — deliberately permissive for ownerless data, so
/// nobody is locked out of their pre-account history — but who may
/// *re-share* it is a narrower question:
///
/// * owned session → the owner (already checked by `authorize_write`)
/// * unclaimed session → an administrator only. Otherwise any account
///   could flip a shared ownerless session to private and hide it from
///   everyone, or flip an unclaimed-but-private one back and re-share it.
fn may_change_visibility(meta: &SessionMeta, scope: &SessionScope) -> bool {
    meta.user_id.is_some() || matches!(scope, SessionScope::Unfiltered)
}

/// Authorize a **write** to `sid`. Returns the meta on success.
pub(crate) fn authorize_write(
    state: &HttpState,
    sid: &str,
    scope: &SessionScope,
) -> Result<SessionMeta, ApiError> {
    let meta = load_meta(state, sid)?;
    if meta.is_writable_by(scope) {
        Ok(meta)
    } else {
        Err(not_found(sid))
    }
}

/// The live `SessionManager`, or 503 while the Runtime is still booting.
///
/// Returns the shared handle rather than a guard: `SessionManager`'s
/// mutating methods need `&mut` across an `.await`, so the caller locks.
async fn session_manager(
    state: &HttpState,
) -> Result<Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>, ApiError> {
    state
        .session_manager_slot
        .read()
        .await
        .clone()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "session manager not ready"))
}

/// Build a lifecycle publisher from the Runtime's MQTT client.
///
/// Session lifecycle events (`created` / `deleted` / `opened`) are still
/// published over MQTT even though the *command* now arrives over HTTP:
/// the Desktop subscribes to them, and they are the only signal other
/// observers get. A missing client (broker not connected) degrades to
/// "no event", never to a failed write.
async fn lifecycle_publisher(
    state: &HttpState,
) -> Option<crate::mqtt::MqttChunkPublisher> {
    // Clone the `Arc` out of the slot before awaiting the inner lock, so
    // the slot guard is never held across an await.
    let shared = state.mqtt_client.lock().await.clone()?;
    let client = shared.lock().await;
    Some(crate::mqtt::MqttChunkPublisher::from_runtime_client(&client))
}

async fn publish_lifecycle(
    state: &HttpState,
    event_type: &str,
    payload: acowork_core::mqtt_proto::data_envelope::Payload,
) {
    let Some(publisher) = lifecycle_publisher(state).await else {
        tracing::debug!(event_type, "no MQTT client; skipping lifecycle event");
        return;
    };
    let envelope = acowork_core::mqtt_proto::DataEnvelope {
        version: 1,
        payload: Some(payload),
    };
    if let Err(e) = publisher.publish_lifecycle(event_type, &envelope).await {
        tracing::warn!(event_type, error = %e, "failed to publish session lifecycle event");
    }
}

/// `POST /sessions` — create a session owned by the caller.
pub(crate) async fn post_create_session(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Option<Json<CreateSessionBody>>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let scope = scope_from_headers(&headers);
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let sm = session_manager(&state).await?;
    let owner = scope.user_id().map(str::to_string);
    let sid = sm
        .lock()
        .await
        .create_frontend_session(
            body.workspace_id.as_deref(),
            body.model.as_deref(),
            body.provider.as_deref(),
            owner.as_deref(),
        )
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // ADR-076 §决策 4: a session created by an identified account is
    // private from birth (stamped in `create_frontend_session`); an
    // ownerless one (local mode) stays public. An explicit `visibility` in
    // the body still wins — this is the creation-time override.
    if let Some(visibility) = body.visibility {
        sm.lock()
            .await
            .set_session_visibility(&sid, Some(visibility));
    }

    let created_at = chrono::Utc::now().to_rfc3339();
    publish_lifecycle(
        &state,
        "created",
        acowork_core::mqtt_proto::data_envelope::Payload::SessionCreated(
            acowork_core::mqtt_proto::SessionCreated {
                agent_id: state.agent_id.clone(),
                session_id: sid.clone(),
                title: String::new(),
                created_at: created_at.clone(),
            },
        ),
    )
    .await;

    tracing::info!(session_id = %sid, owner = ?scope.user_id(), "HTTP: session created");
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "session_id": sid,
            "created_at": created_at,
            "owner": scope.user_id(),
            "visibility": body.visibility,
        })),
    ))
}

/// `POST /sessions/{sid}/open` — activate a Closed session (ADR-038).
///
/// ADR-076 §决策 4: **write**-gated, and only ever called by the session's
/// owner (or an admin). A viewer of a public session must NOT activate it:
/// `Active` / `Closed` is per-session **global** state, not per-connection,
/// so a viewer-triggered activation is a lifecycle change nobody can undo —
/// the viewer cannot close it (closing is write-gated, deliberately: a
/// bystander must not tear down the owner's session) and the owner has no
/// idea they are holding it. That would leave a session resident in the
/// Runtime with no one responsible for releasing it.
///
/// Read-only viewing therefore does not need this endpoint at all: history
/// is fetched over HTTP (`GET /messages`, read-gated) and the event stream
/// arrives on the Desktop's wildcard MQTT subscription whenever the session
/// genuinely is Active. See `apps/acowork-desktop/src/stores/chatStore.ts`
/// (`openSession`), which skips this call entirely when `can_write` is
/// false.
pub(crate) async fn post_open_session(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;

    let sm = session_manager(&state).await?;
    let outcome = sm
        .lock()
        .await
        .resume_session(&sid)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        // The session exists on disk (we read its meta above) but has no
        // meta the manager recognizes — a real inconsistency, not a 404.
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("session not found: {sid}")))?;

    let status = match outcome {
        crate::agent::session::SessionOpenOutcome::AlreadyActive => "already_active",
        crate::agent::session::SessionOpenOutcome::ResumedFromDisk => "resumed_from_disk",
    };
    // NOTE: `resume_session` already refreshed this session's retained
    // `SessionConfig` snapshot (so a stale `llm_availability` from the
    // startup race cannot keep the Desktop's "syncing LLM config" banner
    // up) — the invariant lives in the open state machine, not here.

    let (model, provider, last_active_at) = sm
        .lock()
        .await
        .session_metadata_summary(&sid, &state.work_dir.join("conversations"));
    if let Some(publisher) = lifecycle_publisher(&state).await
        && let Err(e) = publisher
            .publish_session_opened(
                &sid,
                status,
                model.clone(),
                provider.clone(),
                last_active_at.clone(),
            )
            .await
    {
        tracing::warn!(session_id = %sid, error = %e, "failed to publish SessionOpened");
    }

    Ok(Json(serde_json::json!({
        "session_id": sid,
        "status": status,
        "model": model,
        "provider": provider,
        "last_active_at": last_active_at,
    })))
}

/// `POST /sessions/{sid}/close` — graceful close (keeps JSONL, distils).
pub(crate) async fn post_close_session(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;

    // Idempotent by design: re-closing an already-closed session is a
    // no-op, not an error (matching the MQTT handler it replaces).
    let sm = session_manager(&state).await?;
    let closed = sm.lock().await.close_session(&sid).await;
    if let Err(e) = &closed {
        tracing::debug!(session_id = %sid, error = %e, "close reported error (idempotent)");
    }

    publish_lifecycle(
        &state,
        "deleted",
        acowork_core::mqtt_proto::data_envelope::Payload::SessionDeleted(
            acowork_core::mqtt_proto::SessionDeleted {
                agent_id: state.agent_id.clone(),
                session_id: sid.clone(),
                deleted_at: chrono::Utc::now().to_rfc3339(),
            },
        ),
    )
    .await;

    Ok(Json(serde_json::json!({ "session_id": sid, "closed": true })))
}

/// `DELETE /sessions/{sid}` — remove the session and its files.
pub(crate) async fn delete_session(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;

    session_manager(&state).await?.lock().await.delete_session(&sid).await;

    publish_lifecycle(
        &state,
        "deleted",
        acowork_core::mqtt_proto::data_envelope::Payload::SessionDeleted(
            acowork_core::mqtt_proto::SessionDeleted {
                agent_id: state.agent_id.clone(),
                session_id: sid.clone(),
                deleted_at: chrono::Utc::now().to_rfc3339(),
            },
        ),
    )
    .await;

    Ok(Json(serde_json::json!({ "session_id": sid, "deleted": true })))
}

/// Which session ids a `DELETE /sessions` may remove for `scope`.
///
/// Split out of [`clear_sessions`] so the ownership filter is testable
/// without a live `SessionManager`.
fn clearable_session_ids(rows: &[(String, SessionMeta)], scope: &SessionScope) -> Vec<String> {
    rows.iter()
        .filter(|(_, meta)| meta.is_writable_by(scope))
        .map(|(sid, _)| sid.clone())
        .collect()
}

/// `DELETE /sessions` — remove every session the caller may write to.
///
/// Bulk form of [`delete_session`]: each session goes through the same
/// `SessionManager::delete_session` path (task close, session-meta row in
/// SQLite, the `{sid}.jsonl` conversation file, latest-session recompute),
/// so a cleared agent leaves no orphaned rows or files behind. The
/// conversation full-text index is purged lazily by its own sweep once the
/// JSONL files are gone — the same contract single-session delete relies on.
pub(crate) async fn clear_sessions(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_from_headers(&headers);
    let conversations_dir = state.work_dir.join("conversations");

    // Enumerate from the session-meta store (SQLite), not memory: most
    // sessions are idle-evicted. `is_writable_by` is the same authorization
    // predicate `authorize_write` applies to a single-session DELETE.
    let ids = clearable_session_ids(
        &crate::conversation::scan_sessions_from_meta(&conversations_dir),
        &scope,
    );

    let sm = session_manager(&state).await?;
    for sid in &ids {
        sm.lock().await.delete_session(sid).await;
    }

    // One lifecycle event per deleted session, mirroring `delete_session`.
    for sid in &ids {
        publish_lifecycle(
            &state,
            "deleted",
            acowork_core::mqtt_proto::data_envelope::Payload::SessionDeleted(
                acowork_core::mqtt_proto::SessionDeleted {
                    agent_id: state.agent_id.clone(),
                    session_id: sid.clone(),
                    deleted_at: chrono::Utc::now().to_rfc3339(),
                },
            ),
        )
        .await;
    }

    tracing::info!(count = ids.len(), "HTTP: cleared all sessions");
    Ok(Json(serde_json::json!({ "deleted": ids.len() })))
}

/// `PUT /sessions/{sid}/visibility` — share or unshare a session.
///
/// Owner-only (`is_writable_by`): handing out read access is a write.
pub(crate) async fn put_session_visibility(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SetVisibilityBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_from_headers(&headers);
    let meta = authorize_write(&state, &sid, &scope)?;

    if !may_change_visibility(&meta, &scope) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "only an administrator may change the visibility of an unowned session",
        ));
    }

    let ok = session_manager(&state)
        .await?
        .lock()
        .await
        .set_session_visibility(&sid, body.visibility);

    if !ok {
        return Err(not_found(&sid));
    }

    Ok(Json(serde_json::json!({
        "session_id": sid,
        "visibility": body.visibility,
    })))
}

/// Body of `PUT /sessions/{sid}/workspace`.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct SetWorkspaceBody {
    pub(crate) workspace_id: String,
}

/// `PUT /sessions/{sid}/workspace` — switch the session's workspace.
///
/// ADR-076 §决策 4: this cannot reuse `PUT /sessions/{sid}/config`. That
/// endpoint funnels into `ConversationSession::apply_config`, which for
/// `workspace_id` only assigns the field and rewrites meta — it does *not*
/// update `current_work_dir` or push the per-session workspace context /
/// prompt file. Doing that is
/// [`SessionManager::route_workspace_switch`]'s job, and it is the
/// documented single entry point for every workspace switch. A config-based
/// "equivalent" would leave tools writing in the old workspace while meta
/// claimed the new one.
pub(crate) async fn put_session_workspace(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SetWorkspaceBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;

    // `route_workspace_switch` validates `workspace_id` against the
    // resolver and falls back to `__agent_home__` for unknown ids, so an
    // invalid id is not an error here — same as the MQTT path it replaces.
    session_manager(&state)
        .await?
        .lock()
        .await
        .route_workspace_switch(&sid, &body.workspace_id);

    Ok(Json(serde_json::json!({
        "session_id": sid,
        "workspace_id": body.workspace_id,
    })))
}

// ─────────────────────────────────────────────────────────────────────
// Session *actions* (ADR-076 §决策 4, second wave)
//
// The seven user-triggered actions below (chat, stop, continue, tool
// approval, question answer, single-tool cancel, compression) used to be
// published on the MQTT control topic. They are not ownership decisions,
// but they *are* user actions — and an MQTT control message carries no
// identity, so the Runtime could neither authorize nor audit them.
//
// They now arrive over the Gateway's authenticated HTTP API and are
// pushed into the very same dispatch channel the MQTT path fed
// (`http_dispatch_rx` → `mqtt_dispatch_tx` → `dispatch_inbound`), so the
// business logic is untouched: this module only does the authorization
// and the envelope.
//
// **The HTTP response acknowledges routing, not execution.** 202 means
// "authorized and queued"; the outcome is reported on the MQTT event
// plane the Desktop already subscribes to (`session_not_opened`, error
// chunk events, …). A 404/403 here is the only new failure the frontend
// has to handle, and it means "not your session".
// ─────────────────────────────────────────────────────────────────────

/// 202 when the action reached the dispatch channel, 503 when the Runtime
/// is not ready to accept it yet (Phase A → D startup race).
fn accepted(session_id: &str, label: &str, queued: bool) -> Result<StatusCode, ApiError> {
    if queued {
        tracing::debug!(session_id, label, "HTTP: session action queued");
        Ok(StatusCode::ACCEPTED)
    } else {
        Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "runtime dispatch channel not ready",
        ))
    }
}

/// Body of `POST /sessions/{sid}/messages` — mirrors the MQTT
/// `ChatMessage` payload one-for-one.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct SendMessageBody {
    #[serde(default)]
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) message_id: String,
    /// Slash-command *name* only; the Runtime resolves it against the
    /// agent's SkillRegistry (client-supplied instructions are ignored).
    #[serde(default)]
    pub(crate) command: String,
    /// Opaque JSON **string** (not an object), stored verbatim as
    /// `params_json` — the Runtime parses `attached_items` /
    /// `content_parts` out of it itself.
    #[serde(default)]
    pub(crate) params_json: String,
}

/// `POST /sessions/{sid}/messages` — send a user chat message.
pub(crate) async fn post_send_message(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<SendMessageBody>>,
) -> Result<StatusCode, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let queued = crate::http::server::dispatch_session_action(
        &state,
        "chat_message",
        &sid,
        InboundMessage::ChatMessage {
            content: body.content,
            message_id: body.message_id,
            command: body.command,
            params_json: body.params_json,
        },
    )
    .await;
    accepted(&sid, "chat_message", queued)
}

/// Body shared by the three optional-reason actions
/// (`stop` / `continue`). An absent or empty reason is normalized to
/// "user_requested" — the same default the MQTT path used.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct ReasonBody {
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

fn normalize_reason(reason: Option<String>) -> String {
    match reason {
        Some(r) if !r.is_empty() => r,
        _ => "user_requested".to_string(),
    }
}

/// `POST /sessions/{sid}/stop` — interrupt the current generation.
pub(crate) async fn post_stop_session(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<ReasonBody>>,
) -> Result<StatusCode, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;

    let reason = normalize_reason(body.map(|Json(b)| b).unwrap_or_default().reason);
    let queued = crate::http::server::dispatch_session_action(
        &state,
        "stop",
        &sid,
        InboundMessage::Stop { reason },
    )
    .await;
    accepted(&sid, "stop", queued)
}

/// `POST /sessions/{sid}/continue` — resume after an iteration-limit pause.
pub(crate) async fn post_continue_execution(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<ReasonBody>>,
) -> Result<StatusCode, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;

    let reason = normalize_reason(body.map(|Json(b)| b).unwrap_or_default().reason);
    let queued = crate::http::server::dispatch_session_action(
        &state,
        "continue_execution",
        &sid,
        InboundMessage::ContinueExecution {
            session_id: sid.clone(),
            reason,
        },
    )
    .await;
    accepted(&sid, "continue_execution", queued)
}

/// Body of `POST /sessions/{sid}/approval`.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct ApprovalBody {
    pub(crate) request_id: String,
    pub(crate) approved: bool,
    #[serde(default)]
    pub(crate) allow_all_session: bool,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

/// `POST /sessions/{sid}/approval` — the user's tool-risk decision.
///
/// This is the endpoint that closes the approval-spoofing hole: over MQTT
/// any client on the broker could publish `approval_decision{approved:
/// true}` into someone else's session. Here the owner check runs first.
pub(crate) async fn post_approval_decision(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<ApprovalBody>>,
) -> Result<StatusCode, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let queued = crate::http::server::dispatch_session_action(
        &state,
        "approval_decision",
        &sid,
        InboundMessage::ApprovalDecision {
            session_id: sid.clone(),
            request_id: body.request_id,
            approved: body.approved,
            allow_all_session: body.allow_all_session,
            reason: body.reason.filter(|r| !r.is_empty()),
        },
    )
    .await;
    accepted(&sid, "approval_decision", queued)
}

/// Body of `POST /sessions/{sid}/answer`.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct AnswerBody {
    pub(crate) request_id: String,
    pub(crate) answer: String,
}

/// `POST /sessions/{sid}/answer` — answer an `ask_user_question` prompt.
pub(crate) async fn post_question_answer(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<AnswerBody>>,
) -> Result<StatusCode, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let queued = crate::http::server::dispatch_session_action(
        &state,
        "question_answer",
        &sid,
        InboundMessage::QuestionAnswer {
            session_id: sid.clone(),
            request_id: body.request_id,
            answer: body.answer,
        },
    )
    .await;
    accepted(&sid, "question_answer", queued)
}

/// Body of `POST /sessions/{sid}/cancel-tool`.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct CancelToolBody {
    pub(crate) tool_call_id: String,
}

/// `POST /sessions/{sid}/cancel-tool` — ADR-045: abort one in-flight tool.
///
/// The surrounding iteration continues; the cancelled tool returns a
/// "Cancelled by user" result. An unknown `tool_call_id` is a no-op
/// (race against natural completion).
pub(crate) async fn post_cancel_tool(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<CancelToolBody>>,
) -> Result<StatusCode, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let queued = crate::http::server::dispatch_session_action(
        &state,
        "cancel_tool",
        &sid,
        InboundMessage::UserOperation(crate::agent::inbound::UserOp::CancelTool {
            tool_call_id: body.tool_call_id,
        }),
    )
    .await;
    accepted(&sid, "cancel_tool", queued)
}

/// Body of `POST /sessions/{sid}/compress`.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct CompressBody {
    /// `CompressType` i32: 0 = UNSPECIFIED, 1 = SUMMARY, 2 = TOOL_RESULTS.
    /// The Desktop's context-usage menu sends 1.
    #[serde(default)]
    pub(crate) compress_type: i32,
}

/// `POST /sessions/{sid}/compress` — user-initiated context compression.
///
/// Replaces both the MQTT `compress_action` command and the vestigial
/// `compact_context` one: the latter's SessionTask arm was byte-identical
/// to `CompressAction(CompressSummary)` and had no caller left.
pub(crate) async fn post_compress_action(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    body: Option<Json<CompressBody>>,
) -> Result<StatusCode, ApiError> {
    let scope = scope_from_headers(&headers);
    authorize_write(&state, &sid, &scope)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let queued = crate::http::server::dispatch_session_action(
        &state,
        "compress_action",
        &sid,
        InboundMessage::CompressAction {
            session_id: sid.clone(),
            compress_type: body.compress_type,
        },
    )
    .await;
    accepted(&sid, "compress_action", queued)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a meta straight from JSON so the fixture tracks the real
    /// on-disk shape (and its serde defaults) instead of a literal struct.
    fn meta(owner: Option<&str>) -> SessionMeta {
        let raw = serde_json::json!({
            "version": 1,
            "session_id": "s",
            "agent_id": "a",
            "created_at": "t",
            "last_active_at": "t",
            // Required by the struct (no serde default), like a real meta
            // file written before `message_count` existed.
            "message_count": 0,
        });
        let mut meta: SessionMeta = serde_json::from_value(raw).expect("meta fixture parses");
        meta.user_id = owner.map(str::to_string);
        meta
    }

    /// ADR-076 §决策 4: re-sharing an unclaimed session is an
    /// administrator's call. `authorize_write` lets any account modify
    /// ownerless data (pre-account history, the agent's cold-start
    /// session) — that must not also hand them the share switch, or one
    /// account could hide a shared session from every other.
    #[test]
    fn only_an_admin_may_re_share_an_unowned_session() {
        let alice = SessionScope::User("u-alice".into());

        assert!(
            may_change_visibility(&meta(Some("u-alice")), &alice),
            "an owner decides their own session's visibility"
        );
        assert!(
            !may_change_visibility(&meta(None), &alice),
            "an unowned session has no owner to decide — not even a reader may"
        );
        assert!(
            may_change_visibility(&meta(None), &SessionScope::Unfiltered),
            "admin / local mode is never blocked"
        );
    }

    /// `DELETE /sessions` (clear-all) must only remove sessions the caller
    /// may write to — the same predicate single-session DELETE enforces.
    #[test]
    fn clear_all_only_targets_writable_sessions() {
        let rows: Vec<(String, SessionMeta)> = [
            ("mine", Some("u-alice")),
            ("theirs", Some("u-bob")),
            ("unowned", None),
        ]
        .into_iter()
        .map(|(sid, owner)| (sid.to_string(), meta(owner)))
        .collect();

        let alice = SessionScope::User("u-alice".into());
        let mut ids = clearable_session_ids(&rows, &alice);
        ids.sort();
        assert_eq!(ids, vec!["mine", "unowned"], "own + unclaimed, not other users'");

        // Admin / local mode clears everything.
        assert_eq!(
            clearable_session_ids(&rows, &SessionScope::Unfiltered).len(),
            rows.len()
        );
    }
}
