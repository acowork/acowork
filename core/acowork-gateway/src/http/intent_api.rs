//! Intent delivery API — `POST /api/intents`, `GET /api/intents/{id}`
//! (intent-delivery-tech-debt-plan D1/D3/D4/D8)
//!
//! This is the single entry point for agent-to-agent Intent delivery.
//! It replaces the Runtime-side raw MQTT publish (which had no online
//! check, no permission check, and a self-declared `from` — D1/D3/D4/D8).
//!
//! ## Why HTTP, not MQTT
//!
//! Delivery needs three things only the Gateway holds: the retained
//! AgentStatus registry (online truth, ADR-085), the node routing table
//! (who hosts the target), and the installed manifests (both sides'
//! declared permissions). The control topic
//! `acowork/agents/{id}/sessions/control/intent` keeps exactly one
//! publisher — the Gateway — so a broker-level client can no longer
//! inject an Intent into any Runtime.
//!
//! ## Identity (D4)
//!
//! `sender` is the Runtime's own instance identity, declared in the body
//! but **verified against the session**: when the call carries a node
//! token, the sender must be online *and* hosted on that token's node
//! (AgentStatus.node_id — the Runtime asserted it via its own MQTT
//! session). A mismatch is rejected; the body value is never adopted.
//! Same-node sibling spoofing is the known ceiling of this phase (the
//! node token is shared per machine; ADR-087 Phase 5b per-process
//! identity closes it). Without a node token the call is accepted only
//! when the sender is online at all — the local-mode no-op boundary
//! (ADR-087 D8).
//!
//! ## Permissions (D8)
//!
//! Both sides' manifest declarations are checked before delivery:
//! sender `intent:send:<target>` (or bare `intent:send`) and receiver
//! `intent:receive` (bare, or scoped to the sender). Enforcement mode
//! comes from `[intent] enforce_permissions` (default false — warn-only
//! gray rollout per the plan). A denial returns `not_authorized` when
//! enforced; warn-only logs and delivers.
//!
//! ## Result (D3)
//!
//! The response carries the real delivery outcome — `delivered` /
//! `failed` with a §10 error code — because delivery is synchronous here
//! (wake-then-deliver waits for the target's AgentStatus flip up to the
//! intent timeout). The mailbox records the fact either way.

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};
use crate::intent::chat_client;
use crate::intent::gate::{
    attribute_sender, permission_denial, validate_repo, validate_session_config,
};
use crate::intent::delivery::{self, DeliveryContext};

/// Longest execution-session id accepted in `for_execution_session`
/// (ADR-100 D12). Session ids are short generated strings (`20261023_131156_a3bb7d`);
/// the cap only keeps a pathological client value from travelling down the
/// MQTT bus — a longer one is dropped and the turn routes the ordinary way.
const MAX_PINNED_SESSION_ID_LEN: usize = 128;

/// ADR-100 D12: ceilings on the two fields only an execution view's composer
/// sends. A skill is *named*, never described, and an attachment list is ids
/// plus metadata — and that list rides the MQTT envelope, which the Runtime
/// size-caps on arrival. Clamped here so an oversized composer send is a
/// readable 400 rather than a turn silently truncated downstream.
const MAX_PINNED_COMMAND_LEN: usize = 128;
const MAX_PINNED_ATTACHMENTS: usize = 32;
const MAX_PINNED_ATTACHMENTS_JSON_LEN: usize = 32 * 1024;

/// ADR-100 D12: is `id` an Agent participant of this Chat session?
///
/// A pinned turn sent from a group names its own executor, and the Gateway
/// must not deliver on a bare claim: the instance has to be a participant of
/// the very chat the message is filed in (its reply lands there, so a stranger
/// would get a conversation it does not belong to). Legacy untyped rows skip
/// the type check exactly as the write entry's §8.1 rule does — membership is
/// still required, and a group whose participants were never typed must not
/// become unsendable.
fn is_agent_participant(session: &chat_client::SessionInfo, id: &str) -> bool {
    session.participants.iter().any(|p| match p {
        chat_client::ParticipantRef::Legacy(s) => s == id,
        chat_client::ParticipantRef::Typed {
            id: pid,
            identity,
        } => pid == id && identity.as_deref() != Some("human"),
    })
}

/// Is `id` acceptable as a client-supplied Chat row id (ADR-100 D12)?
/// Bounded and restricted to id characters: the value becomes the store's
/// idempotency key and the mailbox key, so a hostile one must not be able to
/// smuggle separators into either.
fn is_valid_row_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Project the ADR-046 `AttachedItem` union of a pinned turn onto the labels
/// the Chat row may carry (ADR-100 D12): a display name plus which arm of the
/// union it came from, and nothing else.
///
/// This is deliberately not an attachment reference. An uploaded blob lives
/// under a content hash in one Agent's `files/` dir and a workspace reference
/// at a node-local path; the Chat store belongs to the user service, the
/// Gateway never buffers attachment bytes (`/api/intents/chat-attachments/…`
/// exists precisely so the Runtime streams them instead), and reading a node
/// path from the Gateway fails the moment Gateway and Node sit on different
/// hosts. So the chat shows that the human handed something to the task — the
/// execution view remains the only place it can actually be opened.
///
/// An item with no displayable name is dropped rather than rendered as a
/// blank chip: an unlabelled "attachment" is noise the reader cannot act on.
fn exec_attachment_labels(items: &[serde_json::Value]) -> Vec<serde_json::Value> {
    items
        .iter()
        .filter_map(|item| {
            // `filename` on the upload arms, `name` on the workspace ones —
            // both are what the execution view already showed the user, so
            // they are quoted as-is (the wire shape is camelCase per each
            // variant's `rename_all`).
            let label = item
                .get("filename")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("name").and_then(|v| v.as_str()))
                .filter(|s| !s.is_empty())
                // Last resort: an item that sent only its path. The directory
                // part is a node-local layout the chat cannot read, so this
                // arm alone is trimmed to its last component — and a Windows
                // path is the case a `/`-only split would silently miss.
                .map(|s| s.to_string())
                .or_else(|| {
                    let raw = item
                        .get("absPath")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())?;
                    Some(raw.rsplit(['/', '\\']).next().unwrap_or(raw).to_string())
                })?;
            Some(serde_json::json!({
                "filename": label,
                "kind": item.get("type").and_then(|v| v.as_str()).unwrap_or(""),
            }))
        })
        .collect()
}

pub fn intent_routes() -> Router<AppState> {
    Router::new()
        .route("/api/intents", post(deliver_intent))
        .route("/api/intents/mentions", post(deliver_mentions))
        .route("/api/intents/chat-message", post(deliver_chat_message))
        .route(
            "/api/intents/chat-attachments/{target}/{session_id}/{attachment_id}/{message_id}",
            get(download_chat_attachment),
        )
        .route("/api/intents/chat-append", post(chat_append))
        .route("/api/intents/chat-history", get(chat_history_read))
        .route("/api/intents/project-group", post(ensure_project_group))
        // Design 27 §8.3: PM member add / remove re-syncs the group's
        // participant set; removal revokes the bound execution sessions.
        .route("/api/intents/project-group/sync", post(sync_project_group))
        .route("/api/intents/task-dispatch", post(dispatch_task))
        .route("/api/intents/task-close", post(close_task_context))
        .route("/api/intents/{message_id}", get(intent_status))
        .route("/api/intents/{message_id}/response", post(deliver_response))
}

/// Design 27 Phase 2: "PM 创建项目时幂等创建唯一项目群聊" — the PM
/// service calls this after a project is created (or a member joins) so
/// the group chat exists in the chat domain without PM ever speaking to
/// the user service directly. Idempotent on `project_id`; a repeat call
/// with extra participants adds them to the same session.
async fn ensure_project_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProjectGroupRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // The caller is a Gateway-managed service, not a user: the node token
    // is the machine credential (same boundary as the runtime-facing
    // intent endpoints; local single-machine mode has no token to present).
    if !token_presents_node(&state, &headers).await {
        return Err(ApiError::unauthorized(
            "project group creation requires a valid node token",
        ));
    }
    if req.project_id.trim().is_empty() || req.participants.is_empty() {
        return Err(ApiError::bad_request(
            "project_id and at least one participant are required",
        ));
    }
    let gw = state.gateway_state.read().await;
    let user_port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
    drop(gw);
    let port = user_port
        .ok_or_else(|| ApiError::service_unavailable("project groups require the user service"))?;
    let session = chat_client::ensure_group(port, &req.project_id, &req.title, &req.participants)
        .await
        .map_err(|e| ApiError::unprocessable_entity(&e))?;
    Ok(Json(serde_json::json!({
        "session_id": session.session_id,
        "participants": session.participants,
    })))
}

#[derive(Debug, Deserialize)]
struct ProjectGroupRequest {
    project_id: String,
    #[serde(default)]
    title: String,
    participants: Vec<String>,
}

/// `POST /api/intents/project-group/sync` — set the project group's
/// participants to exactly `participants` (design 27 §8.3). Same internal
/// auth as `ensure_project_group` (PM acts with the node token). An empty
/// set is legal here: a project can lose every member.
async fn sync_project_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProjectGroupRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !token_presents_node(&state, &headers).await {
        return Err(ApiError::unauthorized(
            "project group sync requires a valid node token",
        ));
    }
    if req.project_id.trim().is_empty() {
        return Err(ApiError::bad_request("project_id is required"));
    }
    let gw = state.gateway_state.read().await;
    let user_port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
    drop(gw);
    let port = user_port
        .ok_or_else(|| ApiError::service_unavailable("project groups require the user service"))?;
    let session = chat_client::sync_group(port, &req.project_id, &req.participants)
        .await
        .map_err(|e| ApiError::unprocessable_entity(&e))?;
    Ok(Json(serde_json::json!({
        "session_id": session.session_id,
        "participants": session.participants,
    })))
}

/// `POST /api/intents/task-dispatch` — PM task dispatch as a group-chat
/// Intent (design 27 §5.4.2 / §9). The task is the execution-context
/// boundary: `conversation_id = "task:{project}:{task}"`, which the
/// receiving Runtime keys its logical session map on (one session chain
/// per task, even inside a shared project group chat). The project group
/// chat is the durable message surface; the repo binding is injected
/// server-side from the session (§9-4).
///
/// Auth is the internal-service boundary (same as
/// `/api/intents/project-group`): the PM service acts for the verified
/// project owner from its own domain data — the Gateway does not
/// re-validate `owner` against PM, mirroring §9-2's trusted-service rule.
#[derive(Debug, Deserialize)]
struct TaskDispatchRequest {
    project_id: String,
    task_id: String,
    /// Target agent instance identity (ADR-073 UUID).
    target_agent: String,
    /// Dispatching human identity (project owner) — the chat row's `from`.
    owner: String,
    #[serde(default)]
    title: String,
    /// Human-readable dispatch text (task title + description).
    message: String,
    /// Model-facing wake-up playbook (claim → work → submit). Delivered
    /// to the Runtime as an envelope field, injected into the system
    /// block — never written to a chat surface or persisted with the
    /// conversation (see `acowork_core::intent::TurnOrigin::PmTask`).
    #[serde(default)]
    assignment_brief: String,
    /// Project group chat session; ensured idempotently when absent.
    #[serde(default)]
    chat_session_id: Option<String>,
    /// Caller-provided dispatch identity. The PM scanner persists one per
    /// dispatch generation and replays it on every retry, which makes the
    /// whole chain idempotent: the chat row dedupes on `meta.message_id`
    /// (the store's write entry returns `already_written` without
    /// appending) and the mailbox dedupes on the same id (3a), so a
    /// delivery-failure retry can neither pile up rows nor re-burn the
    /// §6.5 R4 budget. Absent → the Gateway mints one (old PM senders get
    /// the pre-idempotency behavior, one row per call, unchanged).
    #[serde(default)]
    message_id: Option<String>,
}

/// The meta object on a PM dispatch chat row. The key set is a storage
/// contract: every key here must be a declared field on the user
/// service's `IntentMeta` (undeclared keys are silently dropped at that
/// boundary — design 27 decision 29), and `task_id` is what the Desktop
/// renders as the task chip. `project_id` is deliberately absent: the row
/// already lives in that project's own group chat.
///
/// Pinned by `dispatch_row_meta_keys_are_the_storage_contract` below; the
/// matching round-trip test lives with `IntentMeta` in acowork-user.
fn dispatch_row_meta(
    message_id: &str,
    conversation_id: &str,
    task_id: &str,
    repo: &str,
) -> serde_json::Value {
    serde_json::json!({
        "message_id": message_id,
        "conversation_id": conversation_id,
        "task_id": task_id,
        "repo": repo,
    })
}

/// Drop the high-trust `assignment_brief` key from **agent-supplied**
/// params (`/api/intents/send` and the response relay — the two places a
/// peer's arbitrary JSON reaches an envelope). The key is a system-slot
/// field: only the task-dispatch pipeline above may set it. The Runtime
/// already gates extraction on the turn origin, but a mixed-version fleet
/// may run an older Runtime that extracts unconditionally, so the Gateway
/// strips at the edge too (design 27 §5.4.5 trust boundary: high-trust
/// envelope fields are honored from system producers only).
fn strip_untrusted_brief(
    mut params: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    params.remove("assignment_brief");
    params
}

/// The dispatch row's identity: a usable caller-supplied id is reused so
/// retries dedupe at the chat store and the mailbox (see
/// `TaskDispatchRequest::message_id`); an unusable one (absent, blank,
/// oversized) degrades to minting — never an error, the worst case is the
/// old one-row-per-call behavior.
fn dispatch_message_id(supplied: Option<&str>) -> String {
    match supplied.map(str::trim) {
        Some(s) if !s.is_empty() && s.len() <= 128 => s.to_string(),
        _ => uuid::Uuid::new_v4().to_string(),
    }
}

async fn dispatch_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TaskDispatchRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !token_presents_node(&state, &headers).await {
        return Err(ApiError::unauthorized(
            "task dispatch requires a valid internal-service token",
        ));
    }
    if req.project_id.trim().is_empty()
        || req.task_id.trim().is_empty()
        || req.target_agent.trim().is_empty()
        || req.message.trim().is_empty()
    {
        return Err(ApiError::bad_request(
            "project_id, task_id, target_agent and message are required",
        ));
    }
    let gw = state.gateway_state.read().await;
    let mailbox = gw.intent_mailbox.clone();
    let user_port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
    let (mqtt, registry) = (state.mqtt_gateway_client.clone(), state.agent_registry.clone());
    let node_control = state.node_control.clone();
    drop(gw);
    // §9-3: the group chat is the Intent's source surface — task dispatch
    // has no mailbox-only degradation.
    let port = user_port.ok_or_else(|| {
        ApiError::service_unavailable("task dispatch requires the user service")
    })?;

    // Chat session: use the PM-provided one, else ensure the project group
    // (idempotent on project_id in the user service).
    let chat_session_id = match req.chat_session_id.as_deref() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => {
            let participants = [req.owner.clone(), req.target_agent.clone()];
            let session = chat_client::ensure_group(
                port,
                &req.project_id,
                if req.title.is_empty() {
                    "Project group chat"
                } else {
                    &req.title
                },
                &participants,
            )
            .await
            .map_err(|e| ApiError::unprocessable_entity(&e))?;
            session.session_id
        }
    };
    let session = chat_client::load_session(port, &chat_session_id)
        .await
        .map_err(|e| ApiError::not_found(&format!("chat session not loadable: {e}")))?;
    let repo = session.repo.unwrap_or_default();

    let target_info = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(&req.target_agent)
            .and_then(|k| gw.installed(&k).cloned())
    };
    let Some(target_info) = target_info else {
        return Ok(Json(serde_json::json!({
            "status": "failed", "error_code": "target_not_installed",
        })));
    };
    if intent_enforce_permissions(&state).await
        && !target_info.manifest.permissions.iter().any(|p| {
            use acowork_core::Permission;
            match p {
                Permission::IntentReceive(None) => true,
                Permission::IntentReceive(Some(s)) => {
                    s == &target_info.manifest.agent_id
                        || s == &target_info.instance_id
                        || s == &req.owner
                }
                _ => false,
            }
        })
    {
        return Ok(Json(serde_json::json!({
            "status": "failed", "error_code": "not_authorized",
        })));
    }

    let (Some(mqtt), Some(registry)) = (mqtt.as_ref(), registry.as_ref()) else {
        return Err(ApiError::internal("Gateway MQTT control plane is not available"));
    };
    let ctx = DeliveryContext {
        state: &state.gateway_state,
        registry,
        node_control: node_control.as_ref(),
        mqtt: Some(mqtt),
        mailbox: mailbox.as_deref(),
    };

    // §6.2: the task IS the conversation. Follow-up dispatches on the same
    // task share the conversation (and its §6.5 R4 request budget).
    let conversation_id = acowork_core::intent::task_conversation_id(&req.project_id, &req.task_id);
    // A caller-supplied identity makes retries idempotent end to end (see
    // `TaskDispatchRequest::message_id`); anything unusable falls back to
    // minting.
    let message_id = dispatch_message_id(req.message_id.as_deref());
    let max_requests = intent_max_conversation_requests(&state).await;
    if let Some(mb) = mailbox.as_deref()
        && mb.conversation_request_count(&conversation_id) >= max_requests
    {
        return Ok(Json(serde_json::json!({
            "message_id": message_id,
            "status": "failed",
            "error_code": "conversation_limit_exceeded",
            "conversation_id": conversation_id,
        })));
    }

    // The dispatch chat row (§4: the chat log is the message record).
    // §5.4.5 role table: a PM dispatch is human input — the owner speaks,
    // the agent answers. The row carries the human-readable task body
    // only; the assignment playbook stays on the envelope.
    let meta = dispatch_row_meta(&message_id, &conversation_id, &req.task_id, &repo);
    if let Err(e) = chat_client::append_message(
        port,
        &chat_session_id,
        &chat_client::AppendMessage {
            from: req.owner.clone(),
            to: req.target_agent.clone(),
            kind: "user_message".to_string(),
            body: req.message.clone(),
            meta,
            attachment_ids: Vec::new(),
        },
    )
    .await
    {
        return Ok(Json(serde_json::json!({
            "message_id": message_id,
            "status": "failed",
            "error_code": "chat_log_unavailable",
            "error": e,
        })));
    }

    let dreq = delivery::DeliveryRequest {
        message_id: message_id.clone(),
        conversation_id: conversation_id.clone(),
        from_instance: req.owner.clone(),
        from_session_id: String::new(),
        target_instance: req.target_agent.clone(),
        action: "task".to_string(),
        params_json: serde_json::json!({
            "message": req.message,
            "task_id": req.task_id,
            "assignment_brief": req.assignment_brief,
        })
        .to_string(),
        source_chat_session_id: chat_session_id.clone(),
        repo,
        session_config_json: String::new(),
        // A human dispatch, not a peer request: `pm_task` keeps the
        // Runtime on initiator semantics (full tool set, human text
        // rendered) while the task key stays the routing contract.
        kind: acowork_core::intent::TurnOrigin::PmTask.as_str().to_string(),
        in_reply_to: String::new(),
        status: String::new(),
        // PM dispatch lands in a project group the human owner belongs to.
        has_human: Some(true),
        // PM attachments live in the PM store, not the Chat session's
        // `files/` dir — nothing to pin here (the chat row is written with
        // an empty attachment list too).
        chat_attachment_ids: Vec::new(),
        for_execution_session: String::new(),
    };
    let outcome =
        delivery::wake_and_deliver(&ctx, &dreq, acowork_core::timeout_config::constants::INTENT_DEFAULT)
            .await;
    let (status, error_code, error) = match outcome {
        Ok(()) => ("delivered", None, None),
        Err(e) => ("failed", Some(e.code().to_string()), Some(e.to_string())),
    };
    Ok(Json(serde_json::json!({
        "message_id": message_id,
        "status": status,
        "conversation_id": conversation_id,
        "chat_session_id": chat_session_id,
        "error_code": error_code,
        "error": error,
    })))
}

/// True when the caller presents a credential of a trusted internal actor:
/// an enrolled node token, or the internal-service Bearer token the
/// Gateway hands its spawned services (PM today). With neither store
/// configured (local single-machine mode) the boundary is a no-op — the
/// same rule every other mechanism applies at that tier.
///
/// Async: `gateway_state` is a `tokio::sync::RwLock`, so this takes the read
/// lock with `.await`. Every caller is an HTTP handler running on the tokio
/// runtime — a `blocking_read()` here would panic ("Cannot block the current
/// thread from within a runtime") and abort the request mid-connection.
async fn token_presents_node(state: &AppState, headers: &HeaderMap) -> bool {
    let node = headers
        .get("x-acowork-node-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let gw = state.gateway_state.read().await;
    let has_node_store = gw
        .mqtt_broker_auth
        .as_ref()
        .and_then(|a| a.node_tokens.lock().ok())
        .map(|store| !node.is_empty() && store.any_token_matches(node))
        .unwrap_or(false);
    if has_node_store {
        return true;
    }
    match gw.internal_service_token.as_deref() {
        Some(expected) => {
            !bearer.is_empty()
                && crate::mqtt::enrollment::constant_time_eq(expected.as_bytes(), bearer.as_bytes())
        }
        // No node store and no service token configured: local mode.
        None => false,
    }
}

/// Design 27 §7.3 / §9: an agent reads the Chat session history that its
/// Intent envelopes point at (`chat_history_read` tool). The caller must
/// verify its own node identity, and the user service enforces that the
/// `who` instance is a participant — the Gateway never trusts a claimed
/// session id. Count and bytes are hard-capped server-side so a model
/// cannot slurp an entire group chat into one context.
async fn chat_history_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ChatHistoryQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    verify_sender_identity(&state, &headers, &q.sender).await?;
    let gw = state.gateway_state.read().await;
    let user_port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
    drop(gw);
    let port = user_port
        .ok_or_else(|| ApiError::service_unavailable("chat history requires the user service"))?;
    // Hard caps (§9): the tool description states them, the server enforces
    // them. The byte budget is approximate and is spent **from the newest row
    // backwards**: the store returns a page oldest-first (it is the tail window
    // of the conversation), and the rows no reader can afford to lose are the
    // most recent ones. Budgeting from the front would hand the Chat Router a
    // page that silently stops before the message it is being asked to route —
    // a short page is strictly better than a stale one. The newest row is kept
    // even when it alone exceeds the budget, so one oversized message can never
    // degrade into an empty page.
    const MAX_COUNT: usize = 50;
    const MAX_BYTES: usize = 32 * 1024;
    let count = q.count.clamp(1, MAX_COUNT);
    let offset = q.offset;
    let page = chat_client::read_messages(port, &q.session_id, &q.sender, offset, count)
        .await
        .map_err(|e| ApiError::unprocessable_entity(&e))?;
    let (kept, truncated) = apply_page_budget(
        page.get("messages").and_then(|v| v.as_array()),
        MAX_BYTES,
    );
    Ok(Json(serde_json::json!({
        "session_id": q.session_id,
        "offset": offset,
        "count": kept.len(),
        "truncated": truncated,
        "messages": kept,
    })))
}

/// Spend a chat-history page's byte budget from the **newest** row backwards
/// and report whether older rows had to be dropped.
///
/// `messages` arrives oldest-first (it is the tail window of the conversation),
/// so the front of the slice is the oldest context and the back is what the
/// reader is actually asking about. Budgeting from the front would silently
/// remove the newest rows — for the Chat Router that means routing a message
/// whose own row was dropped (ADR-099). The newest row is always kept, even if
/// it alone exceeds `max_bytes`: one oversized message must never degrade into
/// an empty page. The output stays chronological.
pub(crate) fn apply_page_budget(
    messages: Option<&Vec<serde_json::Value>>,
    max_bytes: usize,
) -> (Vec<serde_json::Value>, bool) {
    let mut kept: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    if let Some(arr) = messages {
        let mut bytes = 0usize;
        for m in arr.iter().rev() {
            let len = serde_json::to_string(m).map(|s| s.len()).unwrap_or(0);
            if !kept.is_empty() && bytes + len > max_bytes {
                truncated = true;
                break;
            }
            bytes += len;
            kept.push(m.clone());
        }
        kept.reverse();
    }
    (kept, truncated)
}

#[derive(Debug, Deserialize)]
struct ChatHistoryQuery {
    /// The calling agent's instance identity (participant-checked by the
    /// user service; verified against the node token by the Gateway).
    sender: String,
    session_id: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_history_count")]
    count: usize,
}

fn default_history_count() -> usize {
    20
}

/// Body of `POST /api/intents/chat-append` (plan 3.0).
#[derive(Debug, Deserialize)]
struct ChatAppendRequest {
    /// The agent instance writing the row — a participant of the session.
    pub from: String,
    pub chat_session_id: String,
    /// `"interaction_request"` | `"system_notice"` | `"agent_message"`.
    /// The first two are cards and turn pointers; `agent_message` is a
    /// chat-bound execution session echoing a locally-composed turn's output
    /// back to the chat it was started from (ADR-100 D9). A routed turn's
    /// answer still goes through the response write-back; `user_message` stays
    /// on the human surface — this relay exists for Runtime-owned kinds only.
    pub kind: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub body: String,
    /// Row meta; `message_id` is required (the store's idempotency key —
    /// a redelivered card or pointer never doubles).
    pub meta: serde_json::Value,
}

/// Kinds the Runtime may write through the chat-append relay (D4 same-source).
/// Decision cards (§5.5.4), the terminal `agent_message` a chat-bound execution
/// session echoes for a locally-composed turn (ADR-100 D9) — and `system_notice`
/// only for legacy rows: since the ADR-100 revision no new dispatch/start/fallback
/// notice is written, because intermediate milestones are runtime state, not
/// history. `user_message` is never a Runtime-owned kind (the human surface
/// writes it).
fn chat_append_kind_allowed(kind: &str) -> bool {
    matches!(kind, "interaction_request" | "system_notice" | "agent_message")
}

/// The mailbox request a Runtime-owned chat row closes, if any (ADR-100 D12).
///
/// Only a terminal `agent_message` answers anything, and it answers the request
/// named in its own `meta.in_reply_to` — for a pinned turn that is the id of the
/// human `user_message` the Gateway delivered, which is what makes the chat read
/// as one round (§5.5.5). Two other rows ride this relay with an `in_reply_to`
/// and must close nothing: a decision card points at the *execution-side*
/// request id, not a chat row, and a resolution notice points at its own card.
/// An echo with no `in_reply_to` is a locally-composed turn (D9), which never
/// opened a request to answer.
fn answered_request(kind: &str, meta: &serde_json::Value) -> Option<String> {
    if kind != "agent_message" {
        return None;
    }
    meta.get("in_reply_to")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `POST /api/intents/chat-append` — the Runtime's chat-row relay
/// (design 27 §5.5.4, plan 3.0). Decision cards and the terminal echo of a
/// locally-composed turn are business facts, and §5.5.5 names the Runtime
/// as their writer; the Chat store itself is loopback-only behind the Gateway's
/// system surface. Same-source discipline as the §5.5.1 write-back: the
/// Runtime never talks to the user service directly. Identity follows
/// D4 — the node token must attribute `from`; the body is not trusted.
async fn chat_append(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatAppendRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    verify_sender_identity(&state, &headers, &req.from).await?;
    if !chat_append_kind_allowed(&req.kind) {
        return Err(ApiError::unprocessable_entity(
            "chat-append accepts interaction_request, system_notice and agent_message only",
        ));
    }
    if req.meta.get("message_id").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
        return Err(ApiError::unprocessable_entity(
            "chat-append requires meta.message_id (idempotency key)",
        ));
    }
    let user_port = {
        let gw = state.gateway_state.read().await;
        gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port)
    };
    let port = user_port
        .ok_or_else(|| ApiError::service_unavailable("chat store requires the user service"))?;
    let session = chat_client::load_session(port, &req.chat_session_id)
        .await
        .map_err(|e| ApiError::unprocessable_entity(&e))?;
    if !session.participants.iter().any(|p| p.id() == req.from) {
        return Err(ApiError::forbidden(
            "writer is not a participant of the chat session",
        ));
    }
    // ADR-100 D12: the terminal echo of a pinned turn is the answer the mailbox
    // is still waiting for — capture the coordinate before `meta` moves into the
    // append. It is what closes the request the Gateway opened when it delivered
    // the human's `user_message`.
    let answers_request = answered_request(&req.kind, &req.meta);
    chat_client::append_message(
        port,
        &req.chat_session_id,
        &chat_client::AppendMessage {
            from: req.from,
            to: req.to,
            kind: req.kind,
            body: req.body,
            meta: req.meta,
            attachment_ids: Vec::new(),
        },
    )
    .await
    .map_err(|e| ApiError::unprocessable_entity(&e))?;
    // §7.1: close the request only now that the answer is on the surface — the
    // same ordering discipline as the Intent write-back, which learned that
    // closing before the append inverts the failure mode into a turn that never
    // ends. A pinned turn has no write-back (the Agent skipped the router, so
    // there is no request to answer), and without this its mailbox entry would
    // stay open until the sweeper filed a "no answer will arrive" receipt
    // beside a chat that already read the real one.
    if let Some(reply_to) = answers_request {
        let gw = state.gateway_state.read().await;
        if let Some(mb) = gw.intent_mailbox.as_deref() {
            let _ = mb.mark_answered(&reply_to);
        }
    }
    Ok(Json(serde_json::json!({ "status": "ok" })))
}

/// A delivered / failed Intent as returned to the calling Runtime.
#[derive(Debug, Serialize)]
pub struct IntentDeliveryResponse {
    pub message_id: String,
    /// `delivered` | `failed` — the mailbox status at return time.
    pub status: String,
    /// The conversation this request belongs to (design 27 §5.4.5 R4):
    /// either the one the initiator passed or the one the Gateway just
    /// assigned. The initiator copies it into follow-up requests so the
    /// target answers in the same execution session and the budget gate
    /// sees one conversation.
    #[serde(default)]
    pub conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Body of `POST /api/intents`. Field set follows design doc 27 §4.2.3
/// (PR-D adds `repo`).
#[derive(Debug, Deserialize)]
pub struct IntentRequest {
    /// The calling Runtime's own instance identity — verified, never
    /// trusted (see module docs).
    pub sender: String,
    #[serde(default)]
    pub sender_session_id: Option<String>,
    /// Target agent instance identity (ADR-073 UUID) — the only
    /// addressable form.
    pub target: String,
    pub action: String,
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub source_chat_session_id: Option<String>,
    /// Source-declared repo (design 27 §4.5.1). Format-validated here; the
    /// target-side authorized-directory boundary is the enforcement point.
    #[serde(default)]
    pub repo: Option<String>,
    /// Initiator's session-config suggestion (design 27 §4.5: model /
    /// reasoning / skills 初值). Shape-checked here (small JSON object of
    /// string values); the responder treats it as a suggestion within its
    /// own allowed set (§13-6). Absent / null = use target defaults.
    #[serde(default)]
    pub session_config: Option<serde_json::Value>,
    /// "request" (default) | "response" (§6.5 R2). Responses are
    /// system-generated — the responder's Runtime posts one at turn
    /// end; `intent_send` never sets this.
    #[serde(default)]
    pub kind: Option<String>,
    /// For a response: the request `message_id` it answers (§4).
    #[serde(default)]
    pub in_reply_to: Option<String>,
    /// For a response: §7.1 outcome (completed | failed | aborted |
    /// timeout). Empty = completed.
    #[serde(default)]
    pub status: Option<String>,
}

async fn deliver_intent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IntentRequest>,
) -> Result<Json<IntentDeliveryResponse>, ApiError> {
    let kind = req.kind.clone().unwrap_or_default();
    if kind == "response" {
        // Responses belong on the dedicated route: the request id is a
        // path coordinate there, not a body claim.
        return Err(ApiError::bad_request(
            "responses must be posted to /api/intents/{request_id}/response",
        ));
    }

    let (sender_info, target_info) = {
        let gw = state.gateway_state.read().await;
        let sender = gw
            .installed(&req.sender)
            .ok_or_else(|| ApiError::unauthorized("intent sender is not an installed agent instance"))?
            .clone();
        let target = gw
            .resolve_installed_key(&req.target)
            .and_then(|k| gw.installed(&k).cloned())
            .ok_or_else(|| ApiError::bad_request("target agent instance is not installed"))?;
        (sender, target)
    };

    // --- D4: sender identity vs session --------------------------------
    verify_sender_identity(&state, &headers, &req.sender).await?;

    // --- D8: both sides' manifest declarations --------------------------
    if let Some(reason) = permission_denial(&sender_info, &target_info) {
        if intent_enforce_permissions(&state).await {
            return Err(ApiError::forbidden(&format!("intent denied: {reason}")));
        }
        tracing::warn!(
            target: "acowork::gateway::intent",
            sender = %req.sender,
            target_agent = %req.target,
            reason = %reason,
            "intent permission would deny (warn-only mode) — delivering anyway"
        );
    }

    // --- PR-D: repo format check (only when sender-declared) ------------
    let mut repo = match req.repo.as_deref() {
        Some(r) => {
            validate_repo(r).map_err(|e| ApiError::unprocessable_entity(&e))?;
            r.to_string()
        }
        None => String::new(),
    };

    // --- design 27 §4.5: session_config shape check (only when declared) --
    if let Some(sc) = req.session_config.as_ref() {
        validate_session_config(sc).map_err(|e| {
            ApiError::unprocessable_entity(&format!("session_config_invalid: {e}"))
        })?;
    }

    // --- §6.5 R4: conversation is created here if absent, then gated ----
    let conversation_id = match req.conversation_id.as_deref() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => uuid::Uuid::new_v4().to_string(),
    };
    let max_requests = intent_max_conversation_requests(&state).await;

    // Everything delivery needs lives on AppState as Options (the MQTT
    // plane may still be starting); absence is `delivery_unavailable`.
    let gw = state.gateway_state.read().await;
    let mailbox = gw.intent_mailbox.clone();
    let user_port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
    drop(gw);
    let (Some(mqtt), Some(registry)) =
        (state.mqtt_gateway_client.as_ref(), state.agent_registry.as_ref())
    else {
        return Ok(Json(IntentDeliveryResponse {
            message_id: String::new(),
            status: "failed".into(),
            conversation_id: conversation_id.clone(),
            error_code: Some("delivery_unavailable".into()),
            error: Some("Gateway MQTT control plane is not available".into()),
        }));
    };
    let ctx = DeliveryContext {
        state: &state.gateway_state,
        registry,
        node_control: state.node_control.as_ref(),
        mqtt: Some(mqtt),
        mailbox: mailbox.as_deref(),
    };

    let message_id = uuid::Uuid::new_v4().to_string();

    if let Some(mb) = mailbox.as_deref()
        && mb.conversation_request_count(&conversation_id) >= max_requests
    {
        // §6.5 R4: refuse — the conversation has spent its request budget.
        // A receipt is the only durable trace (the request itself never
        // reaches the mailbox, so a retry with the same coordinates is
        // refused the same way).
        write_limit_receipt(user_port, &req, &message_id, &conversation_id, max_requests).await;
        return Ok(Json(IntentDeliveryResponse {
            message_id,
            status: "failed".into(),
            error_code: Some("conversation_limit_exceeded".into()),
            error: Some(format!(
                "conversation {conversation_id} already has {max_requests} requests (limit)"
            )),
            conversation_id,
        }));
    }

    // --- §6.3 / §4.1: Chat session binding -------------------------------
    // A Chat session is the message's durable address whenever the user
    // service is up: an explicit one (group chat — participants + repo
    // binding are the server-side truth) or an ensured agent-agent pair.
    // No user service (local single-machine mode) degrades to a
    // mailbox-only delivery; the Intent still routes.
    let mut source_chat_session_id = req.source_chat_session_id.clone().unwrap_or_default();
    // §5.5.4 answerability fact for the responder's turn (plan 2b.3):
    // derived from the session we just bound to, never from the sender.
    let mut has_human: Option<bool> = None;
    if let Some(port) = user_port {
        if source_chat_session_id.is_empty() {
            match chat_client::ensure_pair(port, &req.sender, &req.target, "agent_agent").await {
                Ok(s) => {
                    has_human = s.has_human_participant();
                    source_chat_session_id = s.session_id;
                }
                Err(e) => tracing::warn!(
                    error = %e, sender = %req.sender, target = %req.target,
                    "agent-agent pair session unavailable — delivering without chat binding"
                ),
            }
        } else {
            // Server-side authority over an explicit session: participant
            // check + repo injection (§4.1 — the sender cannot forge it).
            match chat_client::load_session(port, &source_chat_session_id).await {
                Ok(session) => {
                    if !session.participants.iter().any(|p| p.id() == req.sender) {
                        return Err(ApiError::forbidden(
                            "intent sender is not a participant of the source chat session",
                        ));
                    }
                    has_human = session.has_human_participant();
                    if let Some(r) = session.repo.filter(|r| !r.is_empty()) {
                        repo = r;
                    }
                }
                Err(e) => {
                    return Err(ApiError::unprocessable_entity(&format!(
                        "source chat session is not loadable: {e}"
                    )));
                }
            }
        }
        // The Intent request row: the chat log IS the message record
        // (§2.2 single write entry). A failed append means the message
        // has no durable address — refuse delivery rather than deliver
        // an unlogged request. Wave 2B (§4.2): an agent asking another
        // agent is agent speech — `agent_message` carrying the Intent
        // fields, not a separate kind.
        if !source_chat_session_id.is_empty() {
            let meta = serde_json::json!({
                "message_id": message_id,
                "conversation_id": conversation_id,
                "sender_session_id": req.sender_session_id.clone().unwrap_or_default(),
                "repo": repo,
            });
            if let Err(e) = chat_client::append_message(
                port,
                &source_chat_session_id,
                &chat_client::AppendMessage {
                    from: req.sender.clone(),
                    to: req.target.clone(),
                    kind: "agent_message".to_string(),
                    body: intent_body(&req),
                    meta,
                    attachment_ids: Vec::new(),
                },
            )
            .await
            {
                tracing::warn!(
                    error = %e, session_id = %source_chat_session_id,
                    "agent_message chat row append failed — delivery refused"
                );
                return Ok(Json(IntentDeliveryResponse {
                    message_id,
                    status: "failed".into(),
                    conversation_id: conversation_id.clone(),
                    error_code: Some("chat_log_unavailable".into()),
                    error: Some(e),
                }));
            }
        }
    }

    let dreq = delivery::DeliveryRequest {
        message_id: message_id.clone(),
        conversation_id: conversation_id.clone(),
        from_instance: req.sender.clone(),
        from_session_id: req.sender_session_id.clone().unwrap_or_default(),
        target_instance: req.target.clone(),
        action: req.action.clone(),
        params_json: serde_json::to_string(&serde_json::Value::Object(strip_untrusted_brief(
            req.params,
        )))
        .unwrap_or_default(),
        source_chat_session_id,
        repo,
        session_config_json: req
            .session_config
            .map(|v| v.to_string())
            .unwrap_or_default(),
        kind: "request".to_string(),
        in_reply_to: String::new(),
        status: String::new(),
        has_human,
        chat_attachment_ids: Vec::new(),
        for_execution_session: String::new(),
    };
    let outcome = delivery::wake_and_deliver(
        &ctx,
        &dreq,
        acowork_core::timeout_config::constants::INTENT_DEFAULT,
    )
    .await;

    let (status, error_code, error) = match outcome {
        Ok(()) => ("delivered".to_string(), None, None),
        Err(e) => ("failed".to_string(), Some(e.code().to_string()), Some(e.to_string())),
    };
    Ok(Json(IntentDeliveryResponse {
        message_id,
        status,
        conversation_id,
        error_code,
        error,
    }))
}

/// `POST /api/intents/task-close` — design 27 §4.1 T2-a, PM-facing twin of
/// `/api/intents/task-dispatch`: a task reached a terminal state
/// (`Done` / `Rejected` / `Cancelled`), so the target agent's logical
/// execution context for that task ends. The Gateway does not know the
/// physical session id (agent-private state, ADR-009) — it forwards the
/// registry key to the target's Runtime, which closes and unmaps it.
/// Idempotent; an unreachable Runtime is reported, not retried (the next
/// dispatch on a dead mapping already self-heals).
#[derive(Debug, Deserialize)]
struct TaskCloseRequest {
    project_id: String,
    task_id: String,
    target_agent: String,
}

async fn close_task_context(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TaskCloseRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !token_presents_node(&state, &headers).await {
        return Err(ApiError::unauthorized(
            "task context close requires a valid internal-service token",
        ));
    }
    if req.project_id.trim().is_empty() || req.task_id.trim().is_empty() || req.target_agent.trim().is_empty() {
        return Err(ApiError::bad_request(
            "project_id, task_id and target_agent are required",
        ));
    }
    // The key format is the Runtime's routing contract: `routing_key`
    // yields `conv:task:{project}:{task}` for task-scoped conversations
    // (the same coordinate `dispatch_task` stamps).
    let key = acowork_core::intent::task_routing_key(&req.project_id, &req.task_id);
    let endpoint = {
        let registry = state
            .runtime_http_registry
            .as_ref()
            .ok_or_else(|| ApiError::service_unavailable("runtime registry unavailable"))?;
        let reg = registry.read().await;
        reg.get_endpoint(&req.target_agent)
    };
    let Some(endpoint) = endpoint else {
        return Ok(Json(serde_json::json!({
            "status": "skipped", "reason": "agent_not_running", "key": key,
        })));
    };
    let client = crate::http::proxy::runtime_http_client();
    let mut request = client.post(format!("{}/intent-context/close", endpoint)).json(&serde_json::json!({ "key": key }));
    if let Some(token) = crate::http::proxy::resolve_node_token(&state, &req.target_agent).await {
        request = request.header(acowork_core::auth::NODE_TOKEN_HEADER, token);
    }
    match request.send().await {
        Ok(r) if r.status().is_success() => Ok(Json(serde_json::json!({
            "status": "closed", "key": key,
        }))),
        Ok(r) => Ok(Json(serde_json::json!({
            "status": "failed", "key": key,
            "error": format!("runtime answered {}", r.status()),
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "status": "failed", "key": key, "error": e.to_string(),
        }))),
    }
}

/// The plain-text content of an Intent as it appears in the chat log
/// (design 27 §4.5: routing facts live in `meta`, the body is the message).
/// `params.message` is the human-readable form; otherwise the params
/// object is logged verbatim so nothing is silently dropped.
fn intent_body(req: &IntentRequest) -> String {
    match req.params.get("message").and_then(|v| v.as_str()) {
        Some(m) => m.to_string(),
        None => serde_json::to_string(&serde_json::Value::Object(req.params.clone()))
            .unwrap_or_default(),
    }
}

/// §6.5 R4 receipt: an `intent_response` row with status `rejected`,
/// addressed to the would-be request id. Best-effort — the refusal is
/// already returned to the sender over HTTP; the receipt only matters
/// for a group chat where humans watch.
async fn write_limit_receipt(
    user_port: Option<u16>,
    req: &IntentRequest,
    message_id: &str,
    conversation_id: &str,
    max_requests: u32,
) {
    let (Some(port), Some(session_id)) = (user_port, req.source_chat_session_id.as_deref()) else {
        return;
    };
    if session_id.is_empty() {
        return;
    }
    let meta = serde_json::json!({
        "message_id": uuid::Uuid::new_v4().to_string(),
        "conversation_id": conversation_id,
        "in_reply_to": message_id,
        "status": "rejected",
    });
    let body = format!(
        "Delivery refused: this conversation reached its limit of {max_requests} requests (§6.5 R4)."
    );
    if let Err(e) = chat_client::append_message(
        port,
        session_id,
        &chat_client::AppendMessage {
            from: req.target.clone(),
            to: req.sender.clone(),
            kind: "intent_response".to_string(),
            body,
            meta,
            attachment_ids: Vec::new(),
        },
    )
    .await {
        tracing::warn!(error = %e, "conversation-limit receipt append failed");
    }
}

/// §6.5 R4 cap accessor (config `[intent] max_conversation_requests`).
async fn intent_max_conversation_requests(state: &AppState) -> u32 {
    let gw = state.gateway_state.read().await;
    gw.config
        .as_ref()
        .map(|c| c.intent.max_conversation_requests)
        .filter(|v| *v > 0)
        .unwrap_or(6)
}

// ── Response write-back (design 27 §5.5.1) ──────────────────────────────

/// `POST /api/intents/{message_id}/response` — the responder's Runtime
/// calls this at the end of an intent turn; the Gateway then:
///
/// 1. verifies the caller is the *target* of the recorded request (only
///    the responder may answer — a random instance cannot forge a reply
///    into someone else's conversation);
/// 2. appends the `intent_response` chat row (same single write entry as
///    requests);
/// 3. routes a `kind=response` Intent back to the initiator, addressed to
///    the initiator's execution session (`source_agent_session_id`),
///    waking it if needed (§7.1: push, not polling).
///
/// The conversation / chat binding are taken from the mailbox entry —
/// never from the responder's body.
async fn deliver_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    Json(req): Json<IntentRequest>,
) -> Result<Json<IntentDeliveryResponse>, ApiError> {
    // The path is authoritative for what is answered; a mismatching body
    // `in_reply_to` is a producer bug, not a second coordinate.
    let _ = &req.in_reply_to;
    // Identity: the responder stamps itself; verified like a request.
    verify_sender_identity(&state, &headers, &req.sender).await?;

    let gw = state.gateway_state.read().await;
    let mailbox = gw.intent_mailbox.clone();
    let user_port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
    let (mqtt, registry) = (state.mqtt_gateway_client.clone(), state.agent_registry.clone());
    let node_control = state.node_control.clone();
    drop(gw);

    let entry = mailbox
        .as_ref()
        .and_then(|mb| mb.get(&request_id))
        .ok_or_else(|| ApiError::not_found("no recorded request for this response"))?;
    let responder_is_target = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(&req.sender).as_deref() == Some(entry.to_instance.as_str())
    };
    if !responder_is_target {
        return Err(ApiError::forbidden(
            "only the recorded target of the request may answer it",
        ));
    }

    let response_id = format!("resp-{request_id}");
    let status = req.status.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "completed".to_string());

    // Chat row (best-effort like the request path's pair-ensure, but the
    // response IS the durable answer — a failed append on a bound session
    // is reported, delivery still proceeds because the initiator's turn
    // result is carried by the push below). Wave 2B (§4.2): the answer is
    // an `agent_message` row; `status` / `in_reply_to` are fields. The
    // reply coordinate is the row the delivery answered — the request
    // itself for agent-to-agent pairs, the single `user_message` row for
    // a group fan-out (recorded as `result_ref` at delivery time).
    let mut chat_append_error: Option<String> = None;
    if let (Some(port), session_id) = (user_port, entry.source_chat_session_id.clone())
        && !session_id.is_empty()
    {
        let in_reply_to = if entry.result_ref.is_empty() {
            request_id.clone()
        } else {
            entry.result_ref.clone()
        };
        let meta = serde_json::json!({
            "message_id": response_id,
            "conversation_id": entry.conversation_id,
            "in_reply_to": in_reply_to,
            "sender_session_id": req.sender_session_id.clone().unwrap_or_default(),
            "status": status,
        });
        if let Err(e) = chat_client::append_message(
            port,
            &session_id,
            &chat_client::AppendMessage {
                from: entry.to_instance.clone(),
                to: entry.from_instance.clone(),
                kind: "agent_message".to_string(),
                body: intent_body(&req),
                meta,
                attachment_ids: Vec::new(),
            },
        )
        .await
        {
            tracing::warn!(error = %e, session_id, "agent_message chat row append failed");
            chat_append_error = Some(e);
        }
    }

    // §7.1/§13-12: close the request against the fallback sweeper once the
    // answer is actually on the Chat surface. Closing it *before* the append
    // inverted the failure mode: an append that then failed left a closed
    // request with no terminal row — a turn that never ends for the user, with
    // nobody left to repair it. Leaving it open costs at most one sweeper tick
    // — and only an append slower than the sweep interval could interleave with
    // it at all, whereas an orphan never heals by itself.
    if chat_append_error.is_none()
        && let Some(mb) = mailbox.as_deref()
    {
        let _ = mb.mark_answered(&request_id);
    }

    // Design 27 §9-8: a PM task response is *relayed*, not pushed — the
    // initiator is the human owner the dispatch was sent on behalf of.
    // The `intent_response` chat row above is the durable answer (§9-7:
    // reply appended to the project group chat, associated with the
    // dispatch); the PM service reads it to advance the task. Nobody is
    // woken here; a lost chat row means the answer has no surface left,
    // so it is reported as a failure rather than silently dropped.
    // Same relay semantics for a human-initiated conversation (group-chat
    // mention, §5.1: the response renders for the human, wakes no agent —
    // the initiator is not an installed agent to begin with).
    let initiator_is_agent = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(&entry.from_instance).is_some()
    };
    if acowork_core::intent::is_task_conversation(&entry.conversation_id) || !initiator_is_agent {
        let (out_status, error_code, error) = match chat_append_error {
            None => ("delivered".to_string(), None, None),
            Some(e) => ("failed".to_string(), Some("chat_log_unavailable".to_string()), Some(e)),
        };
        return Ok(Json(IntentDeliveryResponse {
            message_id: response_id,
            status: out_status,
            conversation_id: entry.conversation_id.clone(),
            error_code,
            error,
        }));
    }

    let (Some(mqtt), Some(registry)) = (mqtt.as_ref(), registry.as_ref()) else {
        return Ok(Json(IntentDeliveryResponse {
            message_id: response_id,
            status: "failed".into(),
            conversation_id: entry.conversation_id.clone(),
            error_code: Some("delivery_unavailable".into()),
            error: Some("Gateway MQTT control plane is not available".into()),
        }));
    };
    let ctx = DeliveryContext {
        state: &state.gateway_state,
        registry,
        node_control: node_control.as_ref(),
        mqtt: Some(mqtt),
        mailbox: mailbox.as_deref(),
    };
    let dreq = delivery::DeliveryRequest {
        message_id: response_id.clone(),
        conversation_id: entry.conversation_id.clone(),
        from_instance: entry.to_instance.clone(),
        // The response is addressed to the initiator's execution session
        // — the request's `from_session_id` is exactly that (D5 envelope).
        from_session_id: entry.from_session_id.clone(),
        target_instance: entry.from_instance.clone(),
        action: req.action.clone(),
        params_json: serde_json::to_string(&serde_json::Value::Object(strip_untrusted_brief(
            req.params,
        )))
            .unwrap_or_default(),
        source_chat_session_id: entry.source_chat_session_id.clone(),
        repo: String::new(),
        session_config_json: String::new(),
        kind: "response".to_string(),
        in_reply_to: request_id,
        status,
        // Same session the request was bound to — reuse its fact.
        has_human: entry.has_human,
        chat_attachment_ids: Vec::new(),
        for_execution_session: String::new(),
    };
    let outcome = delivery::wake_and_deliver(
        &ctx,
        &dreq,
        acowork_core::timeout_config::constants::INTENT_DEFAULT,
    )
    .await;
    let (out_status, error_code, error) = match outcome {
        Ok(()) => ("delivered".to_string(), None, None),
        Err(e) => ("failed".to_string(), Some(e.code().to_string()), Some(e.to_string())),
    };
    Ok(Json(IntentDeliveryResponse {
        message_id: response_id,
        status: out_status,
        conversation_id: entry.conversation_id.clone(),
        error_code,
        error,
    }))
}

// ── Group-chat mentions (design 27 §5.3) ────────────────────────────────

/// Body of `POST /api/intents/mentions`.
#[derive(Debug, Deserialize)]
pub struct MentionRequest {
    /// The group Chat session the message was posted in.
    pub chat_session_id: String,
    /// The human sender's user id — must be a participant.
    pub from_user: String,
    /// Explicit target agent instances (the client resolves @name to
    /// instance ids; the Gateway never parses free text — §5).
    pub targets: Vec<String>,
    /// The message text (already archived as a `user_message` row by
    /// the chat write entry; this endpoint only fans out Intents).
    pub text: String,
    /// Optional conversation continuation (empty → new conversation).
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Design 27 §4.2.1 / §5.3: ids of attachments already uploaded to this
    /// group Chat session's `files/` dir. The Gateway pins them on the shared
    /// `user_message` row (so history renders and downloads them) and
    /// forwards their ids into each mentioned agent's execution turn — the
    /// Runtime then pulls the bytes over node-auth HTTP against the shared
    /// row (`in_reply_to`). Empty = a plain text mention.
    #[serde(default)]
    pub attachment_ids: Vec<String>,
}

/// `POST /api/intents/mentions` — @Agent fan-out in a group chat (§5):
/// the human utterance is ONE `user_message` row carrying the mentioned
/// targets in its `mentions` field (§5.3.1); each mentioned agent gets
/// its own routed delivery (initiator role, §5.4.5) and its own
/// write-back `agent_message` answering that row. The session's repo
/// binding is injected server-side (§4.1). Ordinary (unmentioned) group
/// messages never reach this endpoint — they are archive-only by
/// construction.
async fn deliver_mentions(
    State(state): State<AppState>,
    auth: Option<axum::extract::Extension<crate::http::auth_middleware::AuthContext>>,
    Json(req): Json<MentionRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Identity: a mention is a user action. In multi-user mode the auth
    // middleware has verified the user token and inserted the `AuthContext`
    // extension (ADR-076 §决策 4 — the token-derived identity is read from
    // the extension, never from a raw request header); `from_user` must
    // equal that verified identity — never a client claim (D4 discipline).
    let verified_user = auth
        .as_ref()
        .map(|axum::extract::Extension(c)| c.effective_user_id().to_string())
        .unwrap_or_default();
    if verified_user.is_empty() || verified_user != req.from_user {
        return Err(ApiError::unauthorized(
            "mention sender must match the authenticated user",
        ));
    }
    // An @ mention may carry text, attachments, or both — but not neither.
    // Targets stay mandatory (a mention with no target is not a fan-out).
    if (req.text.trim().is_empty() && req.attachment_ids.is_empty()) || req.targets.is_empty() {
        return Err(ApiError::bad_request(
            "a mention needs text and/or attachments and at least one target",
        ));
    }

    let gw = state.gateway_state.read().await;
    let mailbox = gw.intent_mailbox.clone();
    let user_port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
    let (mqtt, registry) = (state.mqtt_gateway_client.clone(), state.agent_registry.clone());
    let node_control = state.node_control.clone();
    drop(gw);
    let port = user_port.ok_or_else(|| {
        ApiError::service_unavailable("chat sessions require the user service")
    })?;

    // Server-side truth about the session: participants + repo binding.
    let session = chat_client::load_session(port, &req.chat_session_id)
        .await
        .map_err(|e| ApiError::not_found(&format!("chat session not loadable: {e}")))?;
    if !session.participants.iter().any(|p| p.id() == req.from_user) {
        return Err(ApiError::forbidden("mention sender is not a participant"));
    }
    // ADR-099: the surface travels with the delivery — the receiving router
    // picks its prompt variant from it and must never infer it.
    let chat_kind = session.kind.clone();
    let repo = session.repo.unwrap_or_default();

    let (Some(mqtt), Some(registry)) = (mqtt.as_ref(), registry.as_ref()) else {
        return Err(ApiError::internal("Gateway MQTT control plane is not available"));
    };
    let ctx = DeliveryContext {
        state: &state.gateway_state,
        registry,
        node_control: node_control.as_ref(),
        mqtt: Some(mqtt),
        mailbox: mailbox.as_deref(),
    };

    // §5.3.1 (Wave 2B): one human utterance is ONE `user_message` row;
    // the mentioned targets are a field on it (`mentions`), not N copies
    // of the same text. 落库先于投递 (§5.1): if the row cannot be written
    // the whole fan-out is refused — the message would have no durable
    // address and no reply coordinate.
    let row_id = uuid::Uuid::new_v4().to_string();
    let meta = serde_json::json!({
        "message_id": row_id,
        "mentions": req.targets,
        "repo": repo,
    });
    if let Err(e) = chat_client::append_message(
        port,
        &req.chat_session_id,
        &chat_client::AppendMessage {
            from: req.from_user.clone(),
            to: String::new(),
            kind: "user_message".to_string(),
            body: req.text.clone(),
            meta,
            attachment_ids: req.attachment_ids.clone(),
        },
    )
    .await
    {
        return Err(ApiError::service_unavailable(&format!(
            "user_message chat row append failed — mention fan-out refused: {e}"
        )));
    }

    let mut results = Vec::new();
    for target in &req.targets {
        // Resolve + target-side permission view.
        let target_info = {
            let gw = state.gateway_state.read().await;
            gw.resolve_installed_key(target)
                .and_then(|k| gw.installed(&k).cloned())
        };
        let Some(target_info) = target_info else {
            results.push(serde_json::json!({
                "target": target, "status": "failed",
                "error_code": "target_not_installed",
            }));
            continue;
        };
        if intent_enforce_permissions(&state).await
            && !target_info.manifest.permissions.iter().any(|p| {
                use acowork_core::Permission;
                match p {
                    Permission::IntentReceive(None) => true,
                    Permission::IntentReceive(Some(s)) => {
                        s == &target_info.manifest.agent_id
                            || s == &target_info.instance_id
                            || s == &req.from_user
                    }
                    _ => false,
                }
            })
        {
            // A human mention is not an agent send — only the receive
            // side can deny it (§5: the owner of the conversation is the
            // human message).
            results.push(serde_json::json!({
                "target": target, "status": "failed",
                "error_code": "not_authorized",
            }));
            continue;
        }

        // Per-target delivery coordinate: the mailbox entry (and thus the
        // write-back + sweeper tracking) addresses this uuid; the durable
        // row it answers is the shared `user_message` above (`in_reply_to`
        // → recorded as `result_ref`, read back by `deliver_response`).
        let message_id = uuid::Uuid::new_v4().to_string();
        let dreq = delivery::DeliveryRequest {
            message_id: message_id.clone(),
            // §4.2: a human-initiated round carries no agent conversation
            // — it never counts against the R4 budget; continuity lives
            // in the Chat session (`routing_key` → `chat:{session}`).
            conversation_id: String::new(),
            from_instance: req.from_user.clone(),
            from_session_id: String::new(),
            target_instance: target.clone(),
            action: "chat".to_string(),
            params_json: serde_json::json!({
                "message": req.text,
                "chat_attachment_ids": req.attachment_ids,
                "chat_kind": chat_kind,
            })
            .to_string(),
            source_chat_session_id: req.chat_session_id.clone(),
            repo: repo.clone(),
            session_config_json: String::new(),
            // §5.4.5 role table: a group @ is human input — the target
            // answers as initiator, not responder (keeps `intent_send`,
            // and `ask_user_question` per answerability).
            kind: "user_message".to_string(),
            in_reply_to: row_id.clone(),
            status: String::new(),
            // A human wrote the row this delivery answers.
            has_human: Some(true),
            // Forward the attachment ids so the Runtime pulls the bytes
            // against the shared row (`in_reply_to` = `row_id`).
            chat_attachment_ids: req.attachment_ids.clone(),
            // Group fan-out stays with the router: which of the mentioned
            // agents owns which execution session is not something the
            // Gateway can attribute to a single id (ADR-100 D12).
            for_execution_session: String::new(),
        };
        let outcome = delivery::wake_and_deliver(
            &ctx,
            &dreq,
            acowork_core::timeout_config::constants::INTENT_DEFAULT,
        )
        .await;
        results.push(match outcome {
            Ok(()) => serde_json::json!({
                "target": target, "message_id": message_id, "status": "delivered",
            }),
            Err(e) => serde_json::json!({
                "target": target, "message_id": message_id, "status": "failed",
                "error_code": e.code(), "error": e.to_string(),
            }),
        });
    }

    Ok(Json(serde_json::json!({
        "message_id": row_id,
        "results": results,
    })))
}

/// Body of `POST /api/intents/chat-message`.
#[derive(Debug, Deserialize)]
pub struct ChatMessageRequest {
    /// The `user_agent` Chat session the message is posted in (§5.4.4).
    pub chat_session_id: String,
    /// The human sender — must match the authenticated user.
    pub from_user: String,
    /// The message text.
    pub text: String,
    /// When set, the message answers the `interaction_request` card with
    /// this `message_id` (§5.5.4 answer channel): the answer is routed to
    /// the execution side's pending request instead of opening a new
    /// turn. Empty = a plain message (or a quote of a non-card row).
    #[serde(default)]
    pub in_reply_to: String,
    /// Design 27 §4.5: the initiator's `session_config` suggestion carried
    /// on a chat message that opens a new execution turn (model / workspace
    /// / reasoning-effort chosen on the Chat surface before sending). Only
    /// a shape-checked object of string values; the Runtime clamps it to the
    /// target's allowed set at execution-session creation (§13-6). Absent /
    /// null = target defaults. Ignored on the card-answer path (no new
    /// session is created there).
    #[serde(default)]
    pub session_config: Option<serde_json::Value>,
    /// Design 27 §4.2.1: ids of attachments already uploaded to this Chat
    /// session's `files/` dir. The Gateway pins them on the durable
    /// `user_message` row (so history renders and downloads them) and
    /// forwards their bytes into the execution turn (design 27 §4.2.1
    /// chat-attachment path). Empty = a plain text message.
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    /// Design 27 §5.5.5 / ADR-100 D12: deliver this turn straight to the
    /// named execution session instead of letting the receiving agent's Chat
    /// Router pick one. Set by an execution view's composer, where the human
    /// already knows which task they are talking to. Advisory — a session
    /// that is not loaded falls back to the ordinary route, so a stale id
    /// costs a hop, never the message. Ignored on the card-answer path.
    #[serde(default)]
    pub for_execution_session: Option<String>,
    /// Client-supplied row id (ADR-100 D12). An execution view's composer
    /// already minted an id for the bubble it shows optimistically, and the
    /// SAME id lands on the execution session's JSONL row once the Intent
    /// runs — pinning it here is what lets the two surfaces settle on one
    /// bubble instead of two. Empty = the Gateway generates one, which is
    /// what the Chat surface's own composer does.
    #[serde(default)]
    pub message_id: Option<String>,
    /// ADR-100 D12: the per-turn skill *name* the execution composer selected
    /// (the Runtime resolves it against the agent's `SkillRegistry` — client
    /// supplied instructions are never trusted). Honored only with a pin: a
    /// routed turn is consumed as an Intent, which has no skill arm.
    #[serde(default)]
    pub command: String,
    /// ADR-100 D12: the ADR-046 `AttachedItem` union the execution composer
    /// carried — uploaded blobs (a content-hash `documentId` into this
    /// Agent's `files/`) and workspace references (a node-local `absPath`).
    /// Forwarded verbatim to a pinned turn so it loses no capability the same
    /// send had against `POST /sessions/{sid}/messages`; the bytes are never
    /// copied into the Chat store. Honored only with a pin.
    #[serde(default)]
    pub attached_items: Vec<serde_json::Value>,
    /// ADR-100 D12: the Agent instance that owns the pinned execution session.
    /// Only consulted for a pinned send into a GROUP chat, where there is no
    /// single agent participant to fall back to and no card to read the answer
    /// target off. Never trusted: it must name an Agent participant of this
    /// session (see [`is_agent_participant`]), and the receiving Runtime still
    /// validates that the named session is bound to this chat. Empty / a 1:1
    /// chat = the Gateway resolves the target from the session's participants as
    /// before.
    #[serde(default)]
    pub pinned_agent: String,
}

/// §5.5.4 answer channel: the Runtime resolve endpoint and body a card
/// answer maps to, derived from the card's `interaction.kind`. Pure so
/// the routing table is testable without a live Runtime. `None` means no
/// resolve applies: `pause`/`retry` cards outlive their turn (the turn
/// ended with outcome `waiting`), so their answer is a normal delivery
/// that opens a fresh turn carrying the answer text.
fn card_answer_route(
    card_kind: &str,
    request_id: &str,
    answer: &str,
) -> Option<(String, serde_json::Value)> {
    if request_id.is_empty() {
        return None;
    }
    match card_kind {
        "ask_question" => Some((
            "answer".to_string(),
            serde_json::json!({ "request_id": request_id, "answer": answer }),
        )),
        "approval" => Some((
            "approval".to_string(),
            serde_json::json!({
                "request_id": request_id,
                "approved": is_affirmative(answer),
                "reason": answer,
            }),
        )),
        _ => None,
    }
}

/// Approval cards are answered with the button label as text (the chat
/// composer has no button surface yet); the mapping lives in one place.
/// Anything not on the approve list is a reject — fail-closed.
fn is_affirmative(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "approve" | "approved" | "yes" | "allow" | "同意" | "批准" | "允许"
    )
}

/// Attempt to resolve a card answer against the execution side. Returns
/// `true` only when the row is an `interaction_request` card AND the
/// Runtime accepted the answer (the Agent's in-flight turn continues —
/// no replay, no new turn). `false` falls through to normal delivery: a
/// stale answer (request already timed out, turn already ended) opens a
/// fresh turn that carries the answer text as a plain human message.
async fn resolve_card_answer(
    state: &AppState,
    port: u16,
    chat_session_id: &str,
    from_user: &str,
    card_id: &str,
    answer: &str,
) -> bool {
    let Ok(Some(card)) =
        chat_client::message_by_id(port, chat_session_id, from_user, card_id).await
    else {
        return false;
    };
    if card["kind"].as_str() != Some("interaction_request") {
        return false; // an `in_reply_to` at a plain row is a quote, not an answer
    }
    let meta = &card["meta"];
    let Some(interaction) = meta.get("interaction") else {
        return false;
    };
    let Some((path, body)) = card_answer_route(
        interaction["kind"].as_str().unwrap_or(""),
        interaction["request_id"].as_str().unwrap_or(""),
        answer,
    ) else {
        return false;
    };
    let agent_session_id = meta["agent_session_id"].as_str().unwrap_or("");
    let agent_instance = card["from"].as_str().unwrap_or("");
    if agent_session_id.is_empty() || agent_instance.is_empty() {
        return false;
    }
    // Same endpoint + node-token path the reverse proxy uses (ADR-055):
    // the Runtime authorizes the session by the `x-user-id` scope the
    // Gateway derives from the verified human sender (injected via the
    // single trusted writer, `auth_middleware::runtime_scope_headers`).
    let Some(registry) = state.runtime_http_registry.as_ref() else {
        return false;
    };
    let endpoint = {
        let reg = registry.read().await;
        reg.get_endpoint(agent_instance)
    };
    let Some(endpoint) = endpoint else {
        return false; // Runtime not up — a stale answer delivers as a new turn
    };
    // The caller's Chat membership, exactly as the proxy injects it: the
    // execution session is `Members`-visible and the resolve endpoint
    // writes it, so the owner fallback alone would 404 for a non-owner
    // participant answering a card. A failed lookup stays fail-closed.
    let chats: Vec<String> =
        match crate::intent::chat_client::list_session_ids(port, from_user).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::debug!(error = %e, card_id, "chat scope lookup failed for card answer");
                Vec::new()
            }
        };
    let mut req = crate::http::proxy::runtime_http_client()
        .post(format!("{endpoint}/sessions/{agent_session_id}/{path}"))
        .headers(crate::http::auth_middleware::runtime_scope_headers(
            from_user, &chats,
        ))
        .json(&body);
    if let Some(node_token) =
        crate::http::proxy::resolve_node_token(state, agent_instance).await
    {
        req = req.header("X-ACowork-Node-Token", node_token);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => true,
        Ok(resp) => {
            tracing::debug!(
                status = %resp.status(),
                card_id,
                "card answer not accepted — delivering as a new turn"
            );
            false
        }
        Err(e) => {
            tracing::debug!(error = %e, card_id, "card answer forward failed — delivering as a new turn");
            false
        }
    }
}

/// Stream an attachment pinned to a durable user_message to its target Runtime.
///
/// Design 27 §4.2.1: the Intent envelope carries attachment ids only — the
/// bytes travel over this node-authenticated HTTP pull, never over MQTT.
/// Authorization mirrors `verify_sender_identity`'s tier discipline: with a
/// node-token store (multi-machine), the token must resolve to the node
/// hosting `target`; without one (local single-machine mode) the boundary is
/// a no-op, same as every other isolation mechanism at that tier. The user
/// service then re-verifies that the durable row addressed to `target`
/// actually pins `attachment_id` — an id the agent was never sent 404s.
async fn download_chat_attachment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((target, session_id, attachment_id, message_id)): Path<(String, String, String, String)>,
) -> Result<Response, ApiError> {
    let token = headers.get("x-acowork-node-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let gw = state.gateway_state.read().await;
    if let Some(auth) = gw.mqtt_broker_auth.as_ref() {
        let store = auth.node_tokens.lock().map_err(|_| ApiError::unauthorized("node token invalid"))?;
        let node_id = store.node_id_for_token(token).ok_or_else(|| ApiError::unauthorized("node token invalid"))?;
        let hosted_node = gw.installed_agents.get(&target).map(|a| a.node_id.as_str())
            .or_else(|| gw.running_agents.get(&target).map(|a| a.node_id.as_str()));
        if hosted_node != Some(node_id.as_str()) {
            return Err(ApiError::forbidden("runtime not hosted by authenticated node"));
        }
    }
    let port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port)
        .ok_or_else(|| ApiError::service_unavailable("user service unavailable"))?;
    drop(gw);
    let upstream = chat_client::download_message_attachment(port, &session_id, &attachment_id, &message_id, &target)
        .await.map_err(|e| ApiError::not_found(&e))?;
    // Snapshot the pass-through headers before the body stream consumes
    // `upstream`.
    let content_type = upstream.headers().get(axum::http::header::CONTENT_TYPE).cloned();
    let content_length = upstream.headers().get(axum::http::header::CONTENT_LENGTH).cloned();
    let content_disposition = upstream.headers().get(axum::http::header::CONTENT_DISPOSITION).cloned();
    let mut response = Response::new(Body::from_stream(upstream.bytes_stream().map_err(std::io::Error::other)));
    {
        let h = response.headers_mut();
        if let Some(v) = content_type {
            h.insert(axum::http::header::CONTENT_TYPE, v);
        }
        if let Some(v) = content_length {
            h.insert(axum::http::header::CONTENT_LENGTH, v);
        }
        if let Some(v) = content_disposition {
            h.insert(axum::http::header::CONTENT_DISPOSITION, v);
        }
    }
    response.headers_mut().insert(axum::http::header::X_CONTENT_TYPE_OPTIONS, axum::http::HeaderValue::from_static("nosniff"));
    Ok(response)
}

/// `POST /api/intents/chat-message` — a human message in a 1:1
/// user-agent Chat session, the Agent page's default conversation
/// (design 27 §5.4.4). The message is **not** an Intent request: it
/// archives as a `user_message` row and triggers the Agent's execution
/// session in the initiator role, so the turn keeps `ask_user_question`
/// and can pause for the human (§13-13). Delivery rides the Intent
/// transport with `kind = "user_message"`, which also registers the
/// in-flight mailbox entry the fallback sweeper acts on (plan 2a.4).
async fn deliver_chat_message(
    State(state): State<AppState>,
    auth: Option<axum::extract::Extension<crate::http::auth_middleware::AuthContext>>,
    Json(req): Json<ChatMessageRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Identity: a chat message is a user action; the token-derived
    // identity is the only truth (D4, same discipline as mentions).
    let verified_user = auth
        .as_ref()
        .map(|axum::extract::Extension(c)| c.effective_user_id().to_string())
        .unwrap_or_default();
    if verified_user.is_empty() || verified_user != req.from_user {
        return Err(ApiError::unauthorized(
            "chat message sender must match the authenticated user",
        ));
    }
    // ── ADR-100 D12: the execution composer's own turn facts ──────────
    //
    // Normalized BEFORE the content gate below, because what the human sent is
    // not what this turn will deliver: an un-pinned request loses `command` and
    // `attached_items` outright, and a pinned one keeps only the items that have
    // a displayable name. Gating on the raw request would accept a send the Chat
    // store then refuses — a request-shape error surfacing as a 503, with the
    // turn never delivered and the chat left holding a head row that never
    // existed.
    //
    // The pin itself the Gateway cannot verify — which session is bound to which
    // chat is the Runtime's private fact — so it stays a hint the receiving
    // Runtime validates against the session's own binding (see
    // `deliver_pinned_turn`): an unloaded or misbound pin falls back to the
    // ordinary route, so a stale id costs a hop, never the message.
    let for_execution_session = req
        .for_execution_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= MAX_PINNED_SESSION_ID_LEN)
        .unwrap_or_default()
        .to_string();
    // The two execution-composer fields mean nothing without a pin: a routed
    // turn is consumed as an `Intent`, which has neither a skill arm nor an
    // attachment one, so accepting them on that path would show the chat a
    // chip the Agent never received. Dropped to empty instead of rejected —
    // the text of the turn is still exactly what the human wrote, and an
    // unpinning client's overshoot is not this turn's business.
    let pinned = !for_execution_session.is_empty();
    let command = if pinned {
        let command = req.command.trim();
        if command.chars().count() > MAX_PINNED_COMMAND_LEN {
            return Err(ApiError::bad_request(
                "command must be at most 128 chars — a skill name, not instructions",
            ));
        }
        command.to_string()
    } else {
        String::new()
    };
    let attached_items: Vec<serde_json::Value> = if pinned {
        if req.attached_items.len() > MAX_PINNED_ATTACHMENTS {
            return Err(ApiError::bad_request(&format!(
                "attached_items accepts at most {MAX_PINNED_ATTACHMENTS} entries"
            )));
        }
        let attached_json = serde_json::to_string(&req.attached_items).unwrap_or_default();
        if attached_json.len() > MAX_PINNED_ATTACHMENTS_JSON_LEN {
            return Err(ApiError::bad_request(
                "attached_items payload is too large — attach files to the chat session instead",
            ));
        }
        // Validate the ADR-046 union here rather than letting the Runtime drop
        // what it cannot parse: the Chat row's labels are projected from THIS
        // list, so an entry the Agent would silently discard must never reach
        // the transcript as evidence of something that was never delivered.
        // One check on each side, one agreed set of items.
        for item in &req.attached_items {
            serde_json::from_value::<acowork_core::protocol::AttachedItem>(item.clone())
                .map_err(|e| {
                    ApiError::bad_request(&format!(
                        "attached_items contains an entry the Runtime could not read: {e}"
                    ))
                })?;
        }
        req.attached_items.clone()
    } else {
        Vec::new()
    };
    // A display-only projection for the Chat row (see
    // [`exec_attachment_labels`]). The execution session stays the attachment's
    // only home and the chat records only that something was attached.
    let exec_attachments = exec_attachment_labels(&attached_items);
    // Content gate — the same three proofs the Chat store accepts (§4.2:
    // `body` / `attachments` / `meta.exec_attachments`), computed from what this
    // turn actually carries. An execution composer may legitimately hand the
    // task a file and say nothing, so a label-only row is content, not an empty
    // send.
    if req.text.trim().is_empty() && req.attachment_ids.is_empty() && exec_attachments.is_empty() {
        return Err(ApiError::bad_request(
            "a chat message needs text or an attachment",
        ));
    }
    // design 27 §4.5: reject a malformed session_config before the durable
    // row is written (same gate as `intent_send`) so a bad suggestion cannot
    // reach the Runtime or cost a message an address it never gets.
    if let Some(sc) = req.session_config.as_ref() {
        validate_session_config(sc).map_err(|e| {
            ApiError::unprocessable_entity(&format!("session_config_invalid: {e}"))
        })?;
    }

    let gw = state.gateway_state.read().await;
    let mailbox = gw.intent_mailbox.clone();
    let user_port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
    let (mqtt, registry) = (state.mqtt_gateway_client.clone(), state.agent_registry.clone());
    let node_control = state.node_control.clone();
    drop(gw);
    let port = user_port.ok_or_else(|| {
        ApiError::service_unavailable("chat sessions require the user service")
    })?;

    // Server-side truth: the session must be this user's 1:1 with one
    // Agent; the agent participant is the only delivery target.
    let session = chat_client::load_session(port, &req.chat_session_id)
        .await
        .map_err(|e| ApiError::not_found(&format!("chat session not loadable: {e}")))?;
    if !session.participants.iter().any(|p| p.id() == req.from_user) {
        return Err(ApiError::forbidden("sender is not a participant"));
    }
    let target: String = match session.kind.as_str() {
        "user_agent" => session
            .participants
            .iter()
            .map(|p| p.id().to_string())
            .find(|id| id != &req.from_user)
            .ok_or_else(|| ApiError::not_found("no agent participant in this session"))?,
        // Group surfaces reuse this channel ONLY to answer a pending card
        // (§5.5.4): the card names its own agent, so groups need no target
        // resolution rule. Plain group messages are archive-only via the
        // chat write entry; @-fan-out goes to `/api/intents/mentions`.
        "project_group" | "user_group" => {
            if req.in_reply_to.is_empty() {
                // ADR-100 D12: a PINNED turn names its own executor, so the
                // question a card exists to settle — which agent answers this —
                // is already settled by the execution view the human is standing
                // in. An execution session legitimately binds to a project group
                // (a PM task dispatched from it, ADR-099), so refusing every
                // group send would break exactly that follow-up. The claim is
                // checked, never trusted: the instance must be an Agent
                // participant of this chat, and the receiving Runtime still has
                // to find that session bound to this chat before it takes the
                // turn. A group message with no pin and no card still has no
                // target, so it keeps failing here.
                let claimed = req.pinned_agent.trim();
                if !pinned || claimed.is_empty() {
                    return Err(ApiError::bad_request(
                        "chat-message in a group answers a card only; plain group messages post to /api/chat-sessions/{id}/messages and @-fan-out goes to /api/intents/mentions",
                    ));
                }
                if !is_agent_participant(&session, claimed) {
                    return Err(ApiError::forbidden(
                        "the pinned turn names an instance that is not an agent participant of this chat",
                    ));
                }
                claimed.to_string()
            } else {
                let card = chat_client::message_by_id(port, &req.chat_session_id, &req.from_user, &req.in_reply_to)
                    .await
                    .ok()
                    .flatten()
                    .filter(|c| c["kind"].as_str() == Some("interaction_request"))
                    .ok_or_else(|| {
                        ApiError::bad_request("group answer must reply to an interaction_request card in this session")
                    })?;
                card["from"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| ApiError::bad_request("the answered card names no agent"))?
            }
        }
        other => {
            return Err(ApiError::bad_request(&format!(
                "chat-message does not target '{other}' sessions"
            )));
        }
    };
    let repo = session.repo.unwrap_or_default();

    let (Some(mqtt), Some(registry)) = (mqtt.as_ref(), registry.as_ref()) else {
        return Err(ApiError::internal("Gateway MQTT control plane is not available"));
    };

    // 落库先于投递 (§5.1): the durable `user_message` row exists before
    // the Agent may act on it. The Gateway generates the id so the
    // mailbox entry and the write-back share one coordinate. The row is
    // the durable record of a card answer too — resolution is derived
    // from it, never from a second write (§5.5.4).
    //
    // ADR-100 D12: an execution view supplies its own id so the bubble it
    // already rendered reconciles with this row (and with the execution
    // session's copy of the same message) instead of duplicating.
    let message_id = match req.message_id.as_deref().map(str::trim) {
        // Absent (or an empty string) = the Chat surface's own path.
        None | Some("") => uuid::Uuid::new_v4().to_string(),
        Some(id) if is_valid_row_id(id) => id.to_string(),
        Some(_) => {
            return Err(ApiError::bad_request(
                "message_id must be 1-64 chars of [A-Za-z0-9_-]",
            ));
        }
    };
    // ADR-100 D12: `for_execution_session` / `pinned` / `command` /
    // `attached_items` / `exec_attachments` were normalized above, ahead of the
    // content gate, so the row and the delivery describe one and the same turn.
    let meta = serde_json::json!({
        "message_id": message_id,
        "in_reply_to": req.in_reply_to,
        "exec_attachments": exec_attachments,
    });
    if let Err(e) = chat_client::append_message(
        port,
        &req.chat_session_id,
        &chat_client::AppendMessage {
            from: req.from_user.clone(),
            to: target.clone(),
            kind: "user_message".to_string(),
            body: req.text.clone(),
            meta,
            attachment_ids: req.attachment_ids.clone(),
        },
    )
    .await
    {
        return Err(ApiError::service_unavailable(&format!(
            "user_message chat row append failed — delivery refused: {e}"
        )));
    }

    // §5.5.4 answer channel: an answer to a live card resolves the
    // execution side's pending request and the Agent's current turn
    // continues — no new turn is delivered. A stale/unknown card falls
    // through: the answer delivers as a plain human message and opens a
    // fresh turn.
    if !req.in_reply_to.is_empty()
        && resolve_card_answer(
            &state,
            port,
            &req.chat_session_id,
            &req.from_user,
            &req.in_reply_to,
            &req.text,
        )
        .await
    {
        return Ok(Json(serde_json::json!({
            "message_id": message_id, "status": "answered",
        })));
    }

    let ctx = DeliveryContext {
        state: &state.gateway_state,
        registry,
        node_control: node_control.as_ref(),
        mqtt: Some(mqtt),
        mailbox: mailbox.as_deref(),
    };
    // Design 27 §4.2.1 chat-attachment forwarding: read the referenced
    // attachments back and inline their bytes into the delivery params so
    // the execution turn materializes them into the agent-home `files/` dir
    // (the Chat session's own dir lives in the user service, which the
    // Runtime cannot read). Empty when the message carries no attachments.
    // `conversation_id` stays empty: 1:1 continuity lives in the Chat
    // session itself — `routing_key` maps `chat:{session}` onto one
    // execution session (§5.4.4 轮转对单聊生效).
    //
    let dreq = delivery::DeliveryRequest {
        message_id: message_id.clone(),
        conversation_id: String::new(),
        from_instance: req.from_user.clone(),
        from_session_id: String::new(),
        target_instance: target.clone(),
        action: "chat".to_string(),
        params_json: serde_json::json!({
            "message": req.text,
            "chat_attachment_ids": req.attachment_ids,
            "chat_kind": session.kind,
            // ADR-100 D12: the execution composer's own turn facts, forwarded
            // verbatim so a pinned turn is indistinguishable from the same
            // send made against the execution surface's write endpoint. Empty
            // / absent unless this turn is pinned (see above).
            "command": command,
            "attached_items": attached_items,
        }).to_string(),
        source_chat_session_id: req.chat_session_id.clone(),
        repo: repo.clone(),
        session_config_json: req
            .session_config
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default(),
        kind: "user_message".to_string(),
        in_reply_to: String::new(),
        status: String::new(),
        has_human: Some(true),
        chat_attachment_ids: req.attachment_ids.clone(),
        // ADR-100 D12: the human typed this while standing in one execution
        // session's own view, so "who answers it" is already settled.
        for_execution_session,
    };
    let outcome = delivery::wake_and_deliver(
        &ctx,
        &dreq,
        acowork_core::timeout_config::constants::INTENT_DEFAULT,
    )
    .await;
    match outcome {
        Ok(()) => Ok(Json(serde_json::json!({
            "message_id": message_id, "status": "delivered",
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "message_id": message_id, "status": "failed",
            "error_code": e.code(), "error": e.to_string(),
        }))),
    }
}

/// D3 observability: the mailbox truth for one message id.
async fn intent_status(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let gw = state.gateway_state.read().await;
    let mailbox = gw
        .intent_mailbox
        .as_ref()
        .ok_or_else(|| ApiError::internal("intent mailbox is not initialised"))?;
    let entry = mailbox
        .get(&message_id)
        .ok_or_else(|| ApiError::not_found("no such intent message id"))?;
    Ok(Json(serde_json::json!({
        "message_id": entry.message_id,
        "from": entry.from_instance,
        "to": entry.to_instance,
        "conversation_id": entry.conversation_id,
        "created_at": entry.created_at,
        "delivered_at": entry.delivered_at,
        "delivered": entry.delivered(),
        // ADR-100 D12: `delivered` alone cannot tell a closed round from one
        // the sweeper still owes a receipt for. A pinned turn has no Intent
        // write-back, so its answer arrives as a chat row and this is the only
        // place an operator can see the request was really answered.
        "answered": entry.answered(),
        "answered_at": entry.answered_at,
    })))
}

/// D4 verification. The node token resolves which enrolled machine the
/// caller speaks for; the claimed sender must be online *and* hosted
/// there. Without a node token (local single-machine mode), an online
/// sender claim is accepted — the same no-op boundary every other
/// isolation mechanism keeps at that tier.
async fn verify_sender_identity(
    state: &AppState,
    headers: &HeaderMap,
    sender: &str,
) -> Result<(), ApiError> {
    let token = headers
        .get("x-acowork-node-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let caller_node: Option<String> = {
        let gw = state.gateway_state.read().await;
        gw.mqtt_broker_auth.as_ref().and_then(|auth| {
            let store = auth.node_tokens.lock().ok()?;
            if token.is_empty() {
                None
            } else {
                store.node_id_for_token(token)
            }
        })
    };

    let status = match state.agent_registry.as_ref() {
        Some(reg) => reg.read().await.instance(sender).cloned(),
        None => None,
    };

    attribute_sender(caller_node, status.as_ref())
        .map_err(|reason| ApiError::unauthorized(&reason))
}

async fn intent_enforce_permissions(state: &AppState) -> bool {
    let gw = state.gateway_state.read().await;
    gw.config
        .as_ref()
        .map(|c| c.intent.enforce_permissions)
        .unwrap_or(false)
}



#[cfg(test)]
mod card_answer_tests {
    use super::*;

    /// ADR-100 D12: which Runtime-owned chat row closes a pending mailbox
    /// request. The pinned turn has no Intent write-back, so this is the ONLY
    /// thing that keeps §7.1's sweeper from filing a "no answer will arrive"
    /// receipt beside a chat that already read the real answer — and the only
    /// thing that keeps a card's execution-side request id (which is not a chat
    /// row at all) from being mistaken for one.
    #[test]
    fn only_the_pinned_echo_closes_a_pending_request() {
        let pinned = serde_json::json!({
            "message_id": "exec:m-1",
            "sender_session_id": "s-1",
            "status": "completed",
            "in_reply_to": "m-1",
        });
        assert_eq!(
            answered_request("agent_message", &pinned).as_deref(),
            Some("m-1"),
            "the echo of a pinned turn answers the human's row"
        );
        let card = serde_json::json!({
            "message_id": "c-1",
            "in_reply_to": "approval-9",
            "interaction": { "kind": "approval", "request_id": "approval-9" },
        });
        assert_eq!(
            answered_request("interaction_request", &card),
            None,
            "a card coordinate is not a chat request"
        );
        let standalone = serde_json::json!({ "message_id": "exec:m-2", "status": "completed" });
        assert_eq!(
            answered_request("agent_message", &standalone),
            None,
            "a D9 local turn opened no request to close"
        );
        let blank = serde_json::json!({ "message_id": "n-1", "in_reply_to": "" });
        assert_eq!(answered_request("agent_message", &blank), None);
        assert_eq!(answered_request("agent_message", &serde_json::json!({})), None);
    }

    /// ADR-100 D12: a pinned send into a group names its own executor, and that
    /// claim is checked against the chat's participants — a reply would otherwise
    /// be written into a conversation the named instance is not part of.
    #[test]
    fn a_pinned_group_send_may_name_only_an_agent_participant() {
        let session = chat_client::SessionInfo {
            session_id: "grp-1".into(),
            kind: "project_group".into(),
            participants: vec![
                chat_client::ParticipantRef::Typed {
                    id: "u-1".into(),
                    identity: Some("human".into()),
                },
                chat_client::ParticipantRef::Typed {
                    id: "agent-a".into(),
                    identity: Some("agent".into()),
                },
                chat_client::ParticipantRef::Legacy("agent-legacy".into()),
            ],
            repo: None,
        };
        assert!(is_agent_participant(&session, "agent-a"));
        assert!(
            !is_agent_participant(&session, "u-1"),
            "a human participant cannot be the executor of a turn"
        );
        assert!(
            is_agent_participant(&session, "agent-legacy"),
            "rows written before §8.1 carry no type — membership still governs"
        );
        assert!(
            !is_agent_participant(&session, "agent-stranger"),
            "an instance outside this chat is never a target"
        );
    }

    #[test]
    fn chat_append_kind_whitelist_includes_continuation_echo() {
        // ADR-100 D9: the Runtime writes a continuation echo as a standalone
        // `agent_message` through chat-append. If this kind is dropped from
        // the whitelist the echo silently 422s at the boundary (logged, never
        // fatal) and the chat stops at the abort — the exact regression this
        // guards. Cards / pointers stay accepted; a human-speech `user_message`
        // must never be Runtime-writable.
        assert!(chat_append_kind_allowed("agent_message"));
        assert!(chat_append_kind_allowed("system_notice"));
        assert!(chat_append_kind_allowed("interaction_request"));
        assert!(!chat_append_kind_allowed("user_message"));
        assert!(!chat_append_kind_allowed("intent_request"));
    }

    /// ADR-099: a page over the byte budget loses its OLDEST rows, never its
    /// newest. The Chat Router reads the tail window to decide where a message
    /// belongs, and the message that triggered the turn is the last row of that
    /// window — dropping it from the back would make the router decide without
    /// the thing it is supposed to route.
    #[test]
    fn a_history_page_over_budget_drops_the_oldest_rows_and_stays_chronological() {
        let rows: Vec<serde_json::Value> = (0..6)
            .map(|i| serde_json::json!({ "seq": i, "body": "x".repeat(100) }))
            .collect();
        // Three rows cost well over 350 bytes, six rows over 700.
        let (kept, truncated) = apply_page_budget(Some(&rows), 350);
        assert!(truncated, "rows were dropped, and the reader is told");
        let seqs: Vec<i64> = kept.iter().map(|m| m["seq"].as_i64().unwrap()).collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "the page stays oldest-first on the wire: {seqs:?}"
        );
        assert_eq!(
            *seqs.last().unwrap(),
            5,
            "the newest row survives: {seqs:?}"
        );
        assert!(!seqs.contains(&0), "the oldest row is the one that pays");

        // Fits: nothing dropped, nothing reported as truncated.
        let (all, truncated) = apply_page_budget(Some(&rows), 100_000);
        assert_eq!(all.len(), 6);
        assert!(!truncated);

        // One oversized newest row is still delivered — an empty page would
        // degrade a readable conversation into an unreadable one.
        let big = vec![
            serde_json::json!({ "seq": 0, "body": "small" }),
            serde_json::json!({ "seq": 1, "body": "y".repeat(5_000) }),
        ];
        let (kept, truncated) = apply_page_budget(Some(&big), 100);
        assert_eq!(kept.len(), 1, "the oversized tail row is kept");
        assert_eq!(kept[0]["seq"], 1);
        assert!(truncated, "the older row was dropped, and that is reported");

        // No `messages` array at all is an empty page, not an error.
        assert_eq!(apply_page_budget(None, 100), (Vec::new(), false));
    }

    #[test]
    fn ask_question_routes_to_answer_endpoint() {
        let (path, body) = card_answer_route("ask_question", "q-1", "Option B").unwrap();
        assert_eq!(path, "answer");
        assert_eq!(body["request_id"], "q-1");
        assert_eq!(body["answer"], "Option B");
    }

    #[test]
    fn approval_routes_to_approval_endpoint_with_affirmative_mapping() {
        let (path, body) = card_answer_route("approval", "a-1", "批准").unwrap();
        assert_eq!(path, "approval");
        assert_eq!(body["request_id"], "a-1");
        assert_eq!(body["approved"], true);
        let (_, body) = card_answer_route("approval", "a-1", "no").unwrap();
        assert_eq!(body["approved"], false); // fail-closed
    }

    #[test]
    fn pause_and_unknown_cards_have_no_resolve_route() {
        // The turn already ended with `waiting`; the answer delivers as a
        // fresh turn, not a resolve.
        assert!(card_answer_route("pause", "p-1", "continue").is_none());
        assert!(card_answer_route("retry", "r-1", "go").is_none());
    }

    #[test]
    fn missing_request_id_is_not_routable() {
        assert!(card_answer_route("ask_question", "", "answer").is_none());
    }

    #[test]
    fn affirmative_is_case_and_whitespace_tolerant() {
        assert!(is_affirmative(" Approve "));
        assert!(is_affirmative("YES"));
        assert!(is_affirmative("同意"));
        assert!(!is_affirmative("reject"));
        assert!(!is_affirmative(""));
    }

    // ── ADR-100 D12: a turn pinned to one execution session ──────────

    #[test]
    fn a_client_row_id_must_be_a_bounded_id_and_nothing_else() {
        // The id becomes the store's idempotency key and the coordinate the
        // execution session and the optimistic bubble reconcile on. Anything
        // outside the id charset is rejected rather than stored.
        assert!(is_valid_row_id("0f3a-1_b"));
        assert!(!is_valid_row_id(""), "empty means the Gateway generates one");
        assert!(!is_valid_row_id("row/../../etc"));
        assert!(!is_valid_row_id(&"x".repeat(65)));
    }

    #[test]
    fn attachment_labels_carry_a_basename_never_a_path() {
        // Both union arms are named: an uploaded blob by `filename`, a
        // workspace reference by its path alone when nothing better was sent.
        // That path is reduced to its last component — the chat must not
        // publish a node-local filesystem layout to a surface that cannot read
        // it, and a Windows path is the case a `/`-only split would silently
        // miss.
        let items = vec![
            serde_json::json!({"type": "file_upload", "filename": "report.pdf", "documentId": "ab12cd34ef56-1"}),
            serde_json::json!({"type": "attached_file", "absPath": "C:\\work\\src\\main.rs"}),
        ];
        let labels = exec_attachment_labels(&items);
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0]["filename"], "report.pdf");
        assert_eq!(labels[0]["kind"], "file_upload");
        assert_eq!(labels[1]["filename"], "main.rs");
        assert_eq!(labels[1]["kind"], "attached_file");
        // Neither the blob id nor the path survived, which is the whole point.
        let rendered = serde_json::to_string(&labels).unwrap();
        assert!(!rendered.contains("ab12cd34ef56-1"));
        assert!(!rendered.contains("work"));
    }

    #[test]
    fn a_workspace_attachment_is_labelled_by_its_display_name() {
        // The workspace arms carry a `name` the execution view already shows
        // the user — a workspace-relative path, which is display text, not a
        // node-local layout, so it is quoted whole instead of trimmed.
        let items = vec![serde_json::json!({
            "type": "attached_selection",
            "name": "src/lib.rs",
            "absPath": "C:\\work\\project\\src\\lib.rs",
        })];
        let labels = exec_attachment_labels(&items);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0]["filename"], "src/lib.rs");
        assert_eq!(labels[0]["kind"], "attached_selection");
        let rendered = serde_json::to_string(&labels).unwrap();
        assert!(!rendered.contains("work"));
    }

    #[test]
    fn an_unnamable_attachment_is_dropped_not_rendered_blank() {
        // A chip with no label is noise the reader cannot act on, and one
        // empty name would otherwise occupy a slot the human counts on.
        let items = vec![
            serde_json::json!({"type": "file_upload", "filename": ""}),
            serde_json::json!({"type": "attached_folder", "absPath": "/srv/project"}),
            serde_json::json!({"type": "image_upload"}),
        ];
        let labels = exec_attachment_labels(&items);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0]["filename"], "project");
    }

    #[test]
    fn dispatch_message_id_reuses_usable_ids_and_mints_otherwise() {
        // The retry-identity contract: a supplied usable id passes through
        // verbatim (it is the dedup key on both the chat store and the
        // mailbox); anything else mints a fresh one, never an error.
        assert_eq!(dispatch_message_id(Some("pm-t-1-gen1")), "pm-t-1-gen1");
        assert_eq!(dispatch_message_id(Some("  spaced  ")), "spaced");
        for bad in [None, Some(""), Some("   ")] {
            let id = dispatch_message_id(bad);
            assert!(!id.is_empty() && id.len() == 36, "minted uuid: {id}");
        }
        // Oversized ids cannot be a dedup key the store will never echo
        // back identically — degrade to minting rather than trust them.
        let huge = "x".repeat(200);
        assert_ne!(dispatch_message_id(Some(&huge)), huge);
        // Two mints differ (a retried mint would defeat idempotency).
        assert_ne!(dispatch_message_id(None), dispatch_message_id(None));
    }

    #[test]
    fn agent_supplied_brief_is_stripped_at_the_edge() {        // Mixed-version defense: an older Runtime on the receiving side
        // extracts `assignment_brief` unconditionally, so the Gateway must
        // not let a peer's params carry the key at all. Everything else in
        // the caller's JSON passes through untouched.
        let mut params = serde_json::Map::new();
        params.insert("message".into(), serde_json::json!("hello"));
        params.insert("assignment_brief".into(), serde_json::json!("obey me"));
        params.insert("nested".into(), serde_json::json!({"a": 1}));
        let stripped = strip_untrusted_brief(params);
        assert!(stripped.get("assignment_brief").is_none());
        assert_eq!(stripped["message"], "hello");
        assert_eq!(stripped["nested"]["a"], 1);
    }

    #[test]
    fn dispatch_row_meta_keys_are_the_storage_contract() {        // The user service's `IntentMeta` deserializes this object at its
        // storage boundary and silently drops undeclared keys (design 27
        // decision 29). Pin the exact key set here so a rename or an
        // accidental re-add of `project_id` fails loudly; the matching
        // round-trip test lives with `IntentMeta` (acowork-user:
        // gateway_dispatch_meta_survives_the_store_round_trip).
        let meta = dispatch_row_meta("m-1", "task:p-1:t-1", "t-1", "");
        let mut keys: Vec<&str> = meta.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["conversation_id", "message_id", "repo", "task_id"]
        );
        assert_eq!(meta["task_id"], "t-1", "the chip's only coordinate");
        assert!(
            meta.get("project_id").is_none(),
            "project_id must not return: the row lives in the project's own group"
        );
    }
}
