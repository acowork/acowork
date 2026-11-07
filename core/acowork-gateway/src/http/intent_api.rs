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

use acowork_core::protocol::{
    ChatAttachmentMeta, CHAT_ATTACHMENTS_PARAM, CHAT_REFS_PARAM, TARGET_SESSION_PARAM,
};

use crate::http::proxy::runtime_json_get;
use crate::http::routes::{ApiError, AppState};
use crate::intent::accept;
use crate::intent::chat_client;
use crate::intent::chat_turn;
use crate::intent::gate::{
    attribute_sender, permission_denial, validate_repo, validate_session_config,
};
use crate::intent::delivery::{self, DeliveryContext};
/// Ceilings on `attached_items`. An attachment list is ids plus metadata,
/// and it rides the MQTT envelope, which the Runtime size-caps on arrival.
/// Clamped here so an oversized send is a readable 400 rather than a turn
/// silently truncated downstream.
const MAX_CHAT_ATTACHMENTS: usize = 32;
const MAX_CHAT_ATTACHMENTS_JSON_LEN: usize = 32 * 1024;

/// ADR-106 revision 2026-11-04: how many `@session` references one human
/// utterance may attach. The material is assembled into the receiving
/// session's context under one total byte budget; more references than a
/// model can read in one turn are a paste job, not an addressing job.
const MAX_CHAT_REFS: usize = 5;

/// The longest accepted referenced session id — the Runtime's own
/// `MAX_SESSION_ID_BYTES` (http/rounds.rs): the Gateway pins the coordinate
/// through that endpoint, so a longer id is refused here with the same 400
/// the Runtime would answer, before any row is written.
const MAX_REF_SESSION_ID_BYTES: usize = 128;


/// Is `id` acceptable as a client-supplied Chat row id (ADR-100)?
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

/// Project the ADR-046 `AttachedItem` union of a chat turn onto the labels
/// the Chat row may carry. See
/// [`acowork_core::protocol::AttachmentLabel`] for what that projection is and
/// why a path can never be part of it.
///
/// An item with no displayable name is dropped rather than rendered as a
/// blank chip: an unlabelled "attachment" is noise the reader cannot act on.
fn exec_attachment_labels(items: &[serde_json::Value]) -> Vec<acowork_core::protocol::AttachmentLabel> {
    items
        .iter()
        .filter_map(acowork_core::protocol::AttachmentLabel::from_value)
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
        // Design 27 §4.4: PM archived the project, so its group chat may now
        // be deleted by its owner — the chat domain cannot see PM's tree.
        .route(
            "/api/intents/project-group/deleted",
            post(mark_project_group_deleted),
        )
        .route("/api/intents/task-dispatch", post(dispatch_task))
        .route("/api/intents/task-close", post(close_task_context))
        .route("/api/intents/{message_id}", get(intent_status))
        .route("/api/intents/{message_id}/response", post(deliver_response))
        // The receiving Runtime's acceptance decision — the fact that turns
        // "the broker took the bytes" into "the target could route it".
        .route("/api/intents/{message_id}/accept", post(accept_intent))
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
    let session = chat_client::ensure_group(
        port,
        &req.project_id,
        &req.title,
        &req.participants,
        req.owner.as_deref(),
    )
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
    /// The project title, converged onto the session row by both ensure and
    /// sync. Empty = leave the stored title alone.
    #[serde(default)]
    title: String,
    participants: Vec<String>,
    /// The project's current owner (`Project::owner()`), forwarded to the
    /// user service so the group's ADR-098 governance follows the project.
    #[serde(default)]
    owner: Option<String>,
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
    let session = chat_client::sync_group(
        port,
        &req.project_id,
        &req.participants,
        &req.title,
        req.owner.as_deref(),
    )
    .await
    .map_err(|e| ApiError::unprocessable_entity(&e))?;
    Ok(Json(serde_json::json!({
        "session_id": session.session_id,
        "participants": session.participants,
    })))
}

/// `POST /api/intents/project-group/deleted` — PM archived the project; stamp
/// its group chat so the owner's delete is legal (design 27 §4.4). Same
/// internal-service auth as ensure / sync (PM acts with the node token). The
/// chat is *not* deleted here: the history belongs to its members until a
/// human decides, and PM deleting someone's chat history as a side effect of
/// archiving a project is not a decision this layer gets to make.
async fn mark_project_group_deleted(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !token_presents_node(&state, &headers).await {
        return Err(ApiError::unauthorized(
            "project group deletion requires a valid node token",
        ));
    }
    let project_id = req
        .get("project_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if project_id.is_empty() {
        return Err(ApiError::bad_request("project_id is required"));
    }
    let gw = state.gateway_state.read().await;
    let user_port = gw.user_process.as_ref().filter(|p| p.ready).map(|p| p.port);
    drop(gw);
    let port = user_port
        .ok_or_else(|| ApiError::service_unavailable("project groups require the user service"))?;
    let session = chat_client::mark_project_deleted(port, &project_id)
        .await
        .map_err(|e| ApiError::unprocessable_entity(&e))?;
    Ok(Json(serde_json::json!({
        "session_id": session.as_ref().map(|s| s.session_id.clone()),
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
/// separator-bearing) degrades to minting — never an error, the worst case
/// is the old one-row-per-call behavior. The character rule is
/// `is_valid_row_id`'s: this id travels into URL paths (the receiving
/// Runtime posts its acceptance receipt to `/api/intents/{id}/accept`) and
/// into store keys, so a value smuggling `/ ? #` must not be adopted as an
/// identity. PM mints `pm-{uuid}` (39 chars), well inside the 64 cap.
fn dispatch_message_id(supplied: Option<&str>) -> String {
    match supplied.map(str::trim) {
        Some(s) if is_valid_row_id(s) => s.to_string(),
        _ => uuid::Uuid::new_v4().to_string(),
    }
}

/// The meta a dispatch row carries.
///
/// The key set is a storage contract: every key must be a declared field on the
/// user service's `IntentMeta` (undeclared keys are silently dropped at that
/// boundary — design 27 decision 29). `message_id` is stamped by
/// [`chat_turn::open`], which is also the coordinate the delivery is keyed to.
/// `task_id` makes the row the thread's **origin** when the author is an agent
/// (ADR-106 D15) and is the in-chat search key; the task *chip* renders on the
/// block header (D13). `project_id` is deliberately absent: the row already
/// lives in that project's own group chat.
fn dispatch_turn_meta(task_id: &str, conversation_id: &str, repo: &str) -> serde_json::Value {
    serde_json::json!({
        "conversation_id": conversation_id,
        "task_id": task_id,
        "repo": repo,
    })
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
                // Same rule as the owner below: `req.title` is the *task* title,
                // not `Project::title()`, and the user service converges the
                // title it is handed. Passing it would rename the project
                // channel to the dispatched task on every dispatch. `""` = "this
                // caller does not know it" — a cold create then has no title
                // until PM's ensure / sync (or the boot backfill) sets it, which
                // is strictly better than naming a channel after one task.
                "",
                &participants,
                // No project owner here: `req.owner` is the task's *creator*
                // (`Task::created_by`, an agent for MCP-created tasks), not
                // `Project::owner()`. Writing it would hand the group prompt's
                // ADR-098 governance to whoever filed the task. `None` leaves
                // the stored owner alone; PM's own ensure / sync sets it.
                None,
            )
            .await
            .map_err(|e| ApiError::unprocessable_entity(&e))?;
            session.session_id
        }
    };
    let session = chat_client::load_session(port, &chat_session_id)
        .await
        .map_err(|e| ApiError::not_found(&format!("chat session not loadable: {e}")))?;
    let repo = session.repo.clone().unwrap_or_default();

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
    // A dispatch is a Chat turn before it is an execution turn (ADR-106 D12
    // rev. 2026-11-06). The project group is where a project's work is read, so
    // "task t-X goes to this agent" is a sentence of that conversation — what the
    // transcript shows, what the search index holds, what the unread count
    // counts — and it is the coordinate the round draws on: the reference row
    // the receiving Runtime appends names THIS row as its trigger (D3), so the
    // block hangs under the message that asked for the work instead of standing
    // alone in the flow.
    //
    // D12's earlier text dropped this row on the grounds that "the message is in
    // the execution session and the projection makes it visible". That held for
    // text visibility and broke everything else: the reference row's
    // `in_reply_to` named a mailbox id (`pm-…`) that no chat holds, so the block
    // had no trigger, the transcript held no sentence, and the projection was
    // the only place the task survived.
    //
    // The voice is `Task::created_by` (`req.owner`) — whoever filed the task is
    // the one speaking — and the row's kind follows from that identity inside
    // [`chat_turn::open`]: a human-filed task speaks as the human, a task the PM
    // agent created speaks as the PM (an *assignment*, ADR-106 D15). `task_id` rides the
    // row as that thread's origin marker and as the search key; the chip itself
    // renders on the block header (D13). The wake-up playbook never rides here —
    // it is the envelope's HIGH-TRUST `assignment_brief` (design 27 §5.4.5).
    //
    // Written before the delivery, because the receiving Runtime's reference row
    // has to name a row that already exists (ADR-106 D15).
    if let Err(e) = chat_turn::open(
        port,
        &session,
        chat_turn::Turn {
            session_id: &chat_session_id,
            from: &req.owner,
            to: &req.target_agent,
            message_id: &message_id,
            body: &req.message,
            meta: dispatch_turn_meta(&req.task_id, &conversation_id, &repo),
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

    // Registered before the publish, after every early exit above (see the
    // wait below): a warm Runtime can answer within microseconds, and a
    // listener attached afterwards would miss it and degrade a good answer
    // into "no receipt".
    let acceptance_waiter = crate::intent::accept::AcceptanceWaiter::register(&message_id);
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
        // ADR-106 D3/D12 rev.: the dispatch row written above IS the coordinate
        // — the delivery is made under its id, so the receiving Runtime threads
        // the round's reference row under the sentence that asked for the work.
        chat_row_id: message_id.clone(),
    };
    let outcome =
        delivery::wake_and_deliver(&ctx, &dreq, acowork_core::timeout_config::constants::INTENT_DEFAULT)
            .await;
    // The publish proves the broker took the bytes — not that the target
    // could route them (the incident: a Runtime whose session-meta store
    // lacked `intent_key` rejected every Intent and the task was stamped
    // delivered anyway, so the PM scanner never retried). Wait for the
    // receiving Runtime's decision, bounded by `INTENT_ACCEPT`:
    //
    //   accepted          → delivered
    //   rejected(reason)  → failed / intent_rejected → PM's existing
    //                       backoff retry takes over, no PM change needed
    //   no receipt (timeout, pre-receipt Runtime) → delivered, as before
    //
    // The waiter was registered before the publish above, so a fast receiver
    // cannot answer before we listen.
    let acceptance = if outcome.is_ok() {
        acceptance_waiter
            .wait(acowork_core::timeout_config::constants::INTENT_ACCEPT)
            .await
    } else {
        None
    };
    let (status, error_code, error) = match outcome {
        Ok(()) => match acceptance {
            Some(a) if !a.accepted => {
                tracing::warn!(
                    task_id = %req.task_id,
                    message_id = %message_id,
                    target = %req.target_agent,
                    reason = %a.error,
                    "target runtime rejected the task dispatch"
                );
                ("failed", Some("intent_rejected".to_string()), Some(a.error))
            }
            _ => ("delivered", None, None),
        },
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
    /// `"execution_round"` | `"system_notice"` | `"agent_message"`.
    /// The first is the contentless reference row a Runtime writes when one of
    /// its rounds belongs to this chat (ADR-106 D3); `agent_message` is a
    /// chat-native answer — since ADR-106 D12 that is only the Chat Router
    /// replying in its own words, because an execution turn's answer and the
    /// agent-to-agent exchange live in the round's projection, not in this
    /// channel. `user_message` stays on the human surface — this relay exists
    /// for Runtime-owned kinds only.
    pub kind: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub body: String,
    /// Row meta; `message_id` is required (the store's idempotency key —
    /// a redelivered card or pointer never doubles).
    pub meta: serde_json::Value,
}

/// Kinds the Runtime may write through the chat-append relay (ADR-106 D1).
/// `execution_round` is the reference row a round of execution work opens in the
/// chat — the only `exec → chat` write that remains — `agent_message` is a
/// chat-native answer (the router replying itself), and `system_notice` is a
/// channel fact (`budget_exhausted` / `truncated` / `unavailable`; a failed
/// delivery is a live alert since 2026-11-06, not a row). Decision cards, the terminal echo of an execution turn and
/// the agent-to-agent exchange are no longer chat rows at all: their content
/// lives in the round's projection, so copying them here would put a second,
/// staler answer in the transcript. `user_message` is never a
/// Runtime-owned kind (the human surface writes it).
fn chat_append_kind_allowed(kind: &str) -> bool {
    acowork_core::intent::chat_kinds::is_appendable(kind)
}

/// The mailbox request a Runtime-owned chat row closes, if any.
///
/// Since ADR-106 D12 exactly one row on this relay can answer a tracked
/// request: the Chat Router's own `agent_message`, threaded under the human
/// `user_message` it replied to — and that row's id IS the request id, because
/// the Gateway delivers a chat turn under the id of the row it wrote. The
/// round's reference row closes nothing (its turn answers through the
/// write-back), and a notice answers nothing either.
fn answered_request(kind: &str, meta: &serde_json::Value) -> Option<String> {
    if kind != acowork_core::intent::chat_kinds::AGENT_MESSAGE {
        return None;
    }
    meta.get("in_reply_to")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `POST /api/intents/chat-append` — the Runtime's chat-row relay
/// (design 27 §5.5.4, plan 3.0). The round's reference row, the router's own
/// answer and the notices are business facts, and §5.5.5 names the Runtime
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
            "chat-append accepts execution_round, system_notice and agent_message only",
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
    // The router's own answer is the answer the mailbox is still waiting for —
    // capture the coordinate before `meta` moves into the append.
    let answers_request = answered_request(&req.kind, &req.meta);
    // Remember who wrote the row before `req` moves into the append: the
    // writer's identity is the responder-side half of the activity stamp.
    let writer = req.from.clone();
    let chat_session_id = req.chat_session_id.clone();
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
    // Design 27 §5.5.1 scopes the sweeper's fallback receipt to "Runtime
    // crashed / no event at all". A row the responder just wrote through
    // this relay is the opposite of no event: its process is up and its
    // turn is producing facts. Re-arm that responder's open requests in
    // this Chat session so the response deadline measures silence, not
    // wall-clock time since delivery — otherwise a long turn (approval
    // cards, a long tool chain) is declared crashed while it is still
    // working, and the Chat surface shows a `failed` beside a live session.
    //
    // Done after the append, never before: a row the store rejected is not
    // evidence of anything, and the ordering discipline is the same one the
    // `mark_answered` below follows (§7.1 — close only once the answer is on
    // the surface).
    {
        let gw = state.gateway_state.read().await;
        if let Some(mb) = gw.intent_mailbox.as_deref()
            && mb.touch_activity(&writer, &chat_session_id) > 0
        {
            tracing::debug!(
                responder = %writer,
                chat_session_id = %chat_session_id,
                "intent request deadline re-armed by responder activity"
            );
        }
    }
    // §7.1: close the request only now that the answer is on the surface. A row
    // the Runtime wrote through this relay can be the ONLY answer a chat turn
    // leaves — the router replies without opening a round, so there is no
    // write-back beside it — and without this its mailbox entry would stay open
    // until the sweeper filed a "no answer will arrive" receipt beside a chat
    // that had already read the real one.
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
        write_limit_receipt(user_port, &req, &conversation_id, max_requests).await;
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
        // §5.4.5 R2: a peer request is a private channel between two agents —
        // it writes no Chat row, so the receiver's turn is an *origin* and its
        // reference row says so instead of naming a coordinate no transcript
        // holds.
        chat_row_id: String::new(),
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

/// §6.5 R4 receipt: a `budget_exhausted` notice naming the would-be request
/// id. ADR-106 D1: a limit the chat hit is a system fact, not agent speech,
/// so it rides the notice kind the vocabulary already has (an
/// `intent_response` row is no longer a writable kind at the store boundary).
/// Best-effort — the refusal is already returned to the sender over HTTP; the
/// receipt only matters for a group chat where humans watch.
async fn write_limit_receipt(
    user_port: Option<u16>,
    req: &IntentRequest,
    conversation_id: &str,
    max_requests: u32,
) {
    let (Some(port), Some(session_id)) = (user_port, req.source_chat_session_id.as_deref()) else {
        return;
    };
    if session_id.is_empty() {
        return;
    }
    // No `in_reply_to`: the refused request is a peer request, and since
    // ADR-106 D12 it has no row in the source chat — threading the notice to
    // the request's mailbox id would name a coordinate no transcript holds,
    // which is the defect the Chat write contract now refuses. The receipt is
    // an origin: a limit of the channel, reported where the channel is read.
    let meta = serde_json::json!({
        "message_id": uuid::Uuid::new_v4().to_string(),
        "conversation_id": conversation_id,
        "notice_type": "budget_exhausted",
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
            kind: acowork_core::intent::chat_kinds::SYSTEM_NOTICE.to_string(),
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
/// 2. closes the mailbox request against the fallback sweeper;
/// 3. routes a `kind=response` Intent back to the initiator, addressed to
///    the initiator's execution session (`source_agent_session_id`),
///    waking it if needed (§7.1: push, not polling).
///
/// It writes no Chat row (ADR-106 D12). The answer to a peer request is
/// content of the responder's round, and the responder already referenced
/// that round on the chat its turn was accountable to; the Chat Router's own
/// speech is written by the Runtime through `chat-append`, which is where
/// the agent-authorship check belongs. Copying either of them here would put
/// an execution fact in a process that cannot observe it.
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

    // §7.1/§13-12: close the request against the fallback sweeper. Nothing
    // here waits on a chat row any more — the responder's own reference row
    // is the durable chat-side fact, written before its turn ran (D3), so a
    // write-back that reaches this point has already landed its evidence.
    if let Some(mb) = mailbox.as_deref() {
        let _ = mb.mark_answered(&request_id);
    }

    // Design 27 §9-8: a PM task response is *relayed*, not pushed — the
    // initiator is the human owner the dispatch was sent on behalf of. The
    // answer's durable home is the responder's round, which the project
    // group's block projects (§9-7: the reply is associated with the dispatch
    // by the conversation id). Nobody is woken here.
    // Same relay semantics for a human-initiated conversation (group-chat
    // mention, §5.1: the response renders for the human, wakes no agent —
    // the initiator is not an installed agent to begin with).
    let initiator_is_agent = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(&entry.from_instance).is_some()
    };
    if acowork_core::intent::is_task_conversation(&entry.conversation_id) || !initiator_is_agent {
        return Ok(Json(IntentDeliveryResponse {
            message_id: response_id,
            status: "delivered".to_string(),
            conversation_id: entry.conversation_id.clone(),
            error_code: None,
            error: None,
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
        // A response answers a mailbox request, not a Chat row: the initiator's
        // turn owes no write-back (`TurnOrigin::AgentResponse`) and threads
        // nothing.
        chat_row_id: String::new(),
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
    ///
    /// **Empty is meaningful** (ADR-105): the human addressed nobody, so the
    /// group's default responder answers — or, with no responder configured,
    /// the message is stored and no agent wakes (§5.3.1's historical rule).
    /// Empty therefore no longer means "not a fan-out"; it means "you decide".
    #[serde(default)]
    pub targets: Vec<String>,
    /// ADR-106 D7's second-level `@`: which execution session the human named,
    /// per mentioned agent (`agent instance id → session id`). An address is a
    /// fact the human supplied, so it is never re-decided: it routes at D6 level
    /// 2 and the turn never reaches the Chat Router. A key that is not in
    /// `targets` is refused rather than ignored — dropping an address the human
    /// typed would silently turn "continue that session" into "guess one".
    ///
    /// **Answering channel only** (ADR-106 revision 2026-11-04, narrow
    /// retirement): the `@` picker no longer produces this field — a typed
    /// session reference is material (`refs`), not a delivery. The field stays
    /// because answering a level-3 `session_choice` card IS a delivery, and that
    /// address was chosen by the human in reply to the agent's own question,
    /// not typed by the picker.
    #[serde(default)]
    pub target_sessions: std::collections::HashMap<String, String>,
    /// ADR-106 revision 2026-11-04 (reference redrawing): the execution-session
    /// rounds the human attached as **material** via the second level of the
    /// `@` — a reference, not a delivery. Each entry arrives as
    /// `{agent_instance_id, session_id}` and the Gateway pins the newest
    /// terminal round before the row is written; the pinned coordinates are
    /// stored on the row (`meta.refs`) and forwarded per target under
    /// [`CHAT_REFS_PARAM`]. A ref whose agent is not in `targets` is refused
    /// like a stray `target_sessions` key, and the same agent may not both
    /// receive a delivery (`target_sessions`) and carry material — one
    /// sentence cannot ask "deliver this there" and "quote that here" at once.
    /// `round_id`/`terminal` may be echoed back pre-pinned by an addressing-card
    /// answer (the re-delivery of the same utterance carries the original row's
    /// `meta.refs` verbatim — the material of a sentence is not re-decided).
    #[serde(default)]
    pub refs: Vec<MentionRef>,
    /// ADR-106 D6 level 5's cheap escape: "continue nothing, this is new work".
    /// It is always offered next to an `exec_session_propose_continue`
    /// confirmation, and here
    /// it takes the ladder's continuation branch away (level 3 is skipped) while
    /// the new session's brief still comes from the Chat Router.
    ///
    /// **Per target, on purpose.** A fan-out delivers one utterance to several
    /// agents, and the human can only refuse continuation for the one they were
    /// answering: a blanket flag would make `@A @B` with "new task" picked for A
    /// silently start a new session for B too, rewriting a routing decision nobody
    /// addressed. A target named here must not also appear in `target_sessions`
    /// (an address and a refusal to continue are contradictory), and an id that is
    /// not a target of this message is refused like a stray `target_sessions` key.
    #[serde(default)]
    pub force_new_for: Vec<String>,
    /// The message text (already archived as a `user_message` row by
    /// the chat write entry; this endpoint only fans out Intents).
    pub text: String,
    /// ADR-106 D6 level 1: the id of the chat row this message answers. When
    /// that row is an `execution_round` reference, the message is addressed to
    /// the round's execution session and the turn never reaches the Chat
    /// Router — replying to a block is addressing, not a guess.
    ///
    /// Empty, or a row that is not a reference, means "not addressed": the
    /// ladder continues at its next level and the sender's `@` (or the group's
    /// default responder) decides. A reply to a block therefore outranks an
    /// `@` typed into the same composer, which is the honest precedence — the
    /// human pointed at a specific round of work.
    #[serde(default)]
    pub in_reply_to: String,
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
    /// ADR-106 D6: answering a Chat choice card is a **re-delivery of the human's
    /// own utterance**, now addressed — and a re-delivery must not become a second
    /// row. The original row's id is therefore passable here exactly as it is on
    /// `chat-message`: the store dedupes on it, so the transcript keeps one message
    /// while the addressed turn still goes out. Empty = the Gateway mints one.
    #[serde(default)]
    pub message_id: Option<String>,
}

/// One `@session` reference attached to a human utterance (ADR-106 revision
/// 2026-11-04 · reference redrawing).
///
/// The client names only the **coordinates it knows** — the agent instance and
/// the execution session the human picked in the `@` list. The round is not the
/// human's to guess: the Gateway pins "the newest terminal round at send time"
/// through the Runtime's `latest=1` read before the row is written, and the
/// pinned quadruple is what lands in `meta.refs` and in `params.chat_refs`.
/// A pre-pinned echo (`round_id` + `terminal` present) is the card-answer
/// re-delivery of the same utterance: the material was decided when the row was
/// written and is never re-pinned.
#[derive(Debug, Clone, Deserialize)]
pub struct MentionRef {
    /// The agent instance that owns the referenced session — must be one of
    /// this message's targets (the material travels to the agent the human
    /// named, never to a bystander).
    pub agent_instance_id: String,
    /// The referenced execution session id, as the picker's candidate list
    /// offered it (the Runtime's own id charset is re-checked server-side).
    pub session_id: String,
    /// Pinned round, only from an echoing card answer. Absent = the Gateway
    /// pins it now.
    #[serde(default)]
    pub round_id: Option<String>,
    /// Whether the pinned round had reached terminal, echoed with `round_id`.
    /// The projection renders a non-terminal pin as "progress as of then".
    #[serde(default)]
    pub terminal: Option<bool>,
}

/// ADR-106 D6/D7: refuse a group address the Gateway could not honour.
///
/// An address that cannot be attached to a target of this message must not be
/// dropped — dropping one silently turns "continue the session the human named"
/// into "let a model guess one", which is exactly the failure D6 exists to
/// remove. So a stray key, a blank session id, and the contradiction of naming a
/// session while also refusing continuation are all client bugs and answer 400.
fn validate_mention_addresses(
    target_sessions: &std::collections::HashMap<String, String>,
    force_new_for: &[String],
    targets: &[String],
    refs: &[MentionRef],
) -> Result<(), ApiError> {
    for (target, sid) in target_sessions {
        if sid.trim().is_empty() {
            return Err(ApiError::bad_request(&format!(
                "`target_sessions`['{target}'] is empty — an address must name a session"
            )));
        }
        if !targets.iter().any(|t| t == target) {
            return Err(ApiError::bad_request(&format!(
                "`target_sessions` names '{target}', which this message does not deliver to"
            )));
        }
    }
    for target in force_new_for {
        if !targets.iter().any(|t| t == target) {
            return Err(ApiError::bad_request(&format!(
                "`force_new_for` names '{target}', which this message does not deliver to"
            )));
        }
        if target_sessions.contains_key(target) {
            return Err(ApiError::bad_request(&format!(
                "'{target}' has both an addressed session and `force_new_for` — an address already decides"
            )));
        }
    }
    validate_refs(refs, targets, target_sessions)
}

/// ADR-106 revision 2026-11-04: validate `@session` reference material with the
/// same "never guess, answer 400" discipline as [`validate_mention_addresses`].
///
/// A reference is a fact the human supplied, like an address — so a ref naming
/// an agent this message does not deliver to, a blank or path-shaped session id,
/// a duplicate (agent, session) pair, or the contradiction of the same agent
/// both receiving a delivery and carrying material are all client bugs, refused
/// before anything is written. `force_new_for` is NOT a contradiction: "new
/// task" plus "quote this round while you do it" is a coherent instruction
/// (unlike an address, a reference never decides where the sentence goes).
///
/// The delivery-vs-quote contradiction is checked only for a FRESH ref (one the
/// Gateway is about to pin). A pre-pinned echo — a card answer re-delivering the
/// human's own sentence, with that sentence's already-pinned coordinates — is a
/// fact about an utterance already on disk, not a new delivery instruction, so it
/// rides beside the answer's target. Refusing it would 400 the only way to answer
/// a `confirm_continue` card the human reached by quoting material.
fn validate_refs(
    refs: &[MentionRef],
    targets: &[String],
    target_sessions: &std::collections::HashMap<String, String>,
) -> Result<(), ApiError> {
    if refs.len() > MAX_CHAT_REFS {
        return Err(ApiError::bad_request(&format!(
            "refs accepts at most {MAX_CHAT_REFS} entries, got {}",
            refs.len()
        )));
    }
    let mut seen = std::collections::HashSet::new();
    for r in refs {
        if !targets.iter().any(|t| t == &r.agent_instance_id) {
            return Err(ApiError::bad_request(&format!(
                "`refs` names agent '{}', which this message does not deliver to",
                r.agent_instance_id
            )));
        }
        if !valid_ref_session_id(&r.session_id) {
            return Err(ApiError::bad_request(&format!(
                "`refs` session id {:?} is not a well-formed execution session id",
                r.session_id
            )));
        }
        if r.round_id.as_deref().is_some_and(|s| s.trim().is_empty()) {
            return Err(ApiError::bad_request(
                "`refs` carries a blank round_id — a pinned echo must name the round it pins",
            ));
        }
        // A pre-pinned echo is the re-delivered sentence's own coordinates (a
        // card answer that names a session also carries them back) — that is a
        // fact of an already-written turn, not a fresh delivery-vs-quote intent,
        // so only a FRESH ref beside a delivery to the same agent is the
        // contradiction refused here.
        let pre_pinned = r
            .round_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        if !pre_pinned && target_sessions.contains_key(&r.agent_instance_id) {
            return Err(ApiError::bad_request(&format!(
                "'{}' both receives a `target_sessions` delivery and carries `refs` material — one sentence cannot deliver to and quote the same agent",
                r.agent_instance_id
            )));
        }
        if !seen.insert((r.agent_instance_id.clone(), r.session_id.clone())) {
            return Err(ApiError::bad_request(&format!(
                "duplicate ref for ({}, {}) — the same session must be quoted once",
                r.agent_instance_id, r.session_id
            )));
        }
    }
    Ok(())
}

/// The Runtime's session-id charset ([`MAX_REF_SESSION_ID_BYTES`],
/// `[A-Za-z0-9_-]`): the id travels into a URL path to the pinning read, so it
/// is validated with the endpoint's own rule here — a `..` never reaches either
/// the path build or the filesystem behind it.
fn valid_ref_session_id(sid: &str) -> bool {
    !sid.is_empty()
        && sid.len() <= MAX_REF_SESSION_ID_BYTES
        && sid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// ADR-106 revision 2026-11-04: pin each `@session` reference to the round it
/// quotes, **before the chat row is written**.
///
/// The client names (agent, session); the round is resolved here as "the newest
/// terminal round now, or the newest running round if nothing has terminated"
/// through the Runtime's bounded `latest=1` read ([`runtime_json_get`], same
/// addressing as the chat's own round reads). A terminal round is immutable, so
/// the pinned coordinate cannot rot under the row.
///
/// Refuse-not-degrade: a reference whose round cannot be resolved answers 400
/// (the session is gone, unreadable, or has never run a round — the picker's
/// list was stale, not the node down) or 502 (Runtime/node unreachable), and
/// **no row is written**. The coordinates on the row are facts; if the Gateway
/// cannot establish the fact it must not record a sentence that claims it.
///
/// A ref that arrives already pinned (`round_id` echoed by a card answer — the
/// same utterance re-delivered) is passed through verbatim: re-deciding the
/// material of a sentence that was already written is exactly the re-guessing
/// this endpoint exists to prevent.
async fn pin_reference_rounds(
    state: &AppState,
    refs: &[MentionRef],
    verified_user: &str,
    chat_session_id: &str,
) -> Result<Vec<serde_json::Value>, ApiError> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    // The read is the human's own scope, same pair the auth middleware injects
    // for proxied round reads: owner-match plus this chat's membership. Local
    // mode (empty verified user) sends neither header and gets `Unfiltered`.
    // A value that is not a legal header value is refused as a client bug —
    // both ids are validated elsewhere, so reaching here with one means the
    // caller built a request nothing on this path could honour.
    let mut scope = axum::http::HeaderMap::new();
    if !verified_user.is_empty() {
        let header = |name, value: &str| -> Result<axum::http::HeaderValue, ApiError> {
            axum::http::HeaderValue::from_str(value).map_err(|_| {
                ApiError::bad_request(&format!(
                    "refusing the pinning read: '{name}' is not a legal header value"
                ))
            })
        };
        scope.insert(
            crate::http::auth_middleware::USER_SCOPE_HEADER,
            header("user id", verified_user)?,
        );
        scope.insert(
            crate::http::auth_middleware::CHAT_SCOPE_HEADER,
            header("chat session id", chat_session_id)?,
        );
    }
    let mut pinned = Vec::with_capacity(refs.len());
    for r in refs {
        // The scoped `rounds?latest=1` read is an authorization gate for BOTH
        // arms, not just the fresh one. It runs `authorize_read` against the
        // caller's user + chat scope (design 27 §8.2), so a `session_id` the
        // human cannot read — one bound to a different Chat on this host —
        // answers 404 here instead of having its material injected from a
        // forged pre-pinned echo coordinate. Passing the gate does NOT re-decide
        // an already-pinned round; it only proves the caller may read the session.
        let path = format!("/sessions/{}/rounds", r.session_id);
        let body = runtime_json_get(state, &r.agent_instance_id, &path, "latest=1", &scope)
            .await
            .map_err(|(status, message)| {
                tracing::warn!(
                    agent = %r.agent_instance_id,
                    session = %r.session_id,
                    status = %status,
                    error = %message,
                    "@session reference pinning read failed — row not written"
                );
                if status == axum::http::StatusCode::NOT_FOUND {
                    ApiError::bad_request(&format!(
                        "referenced session '{}' cannot be quoted: no such session or not readable",
                        r.session_id
                    ))
                } else {
                    ApiError::bad_gateway(&format!(
                        "cannot pin the referenced round now: {message}"
                    ))
                }
            })?;
        let (round_id, terminal) = match r.round_id.as_deref().map(str::trim) {
            // Pre-pinned echo: the gate above proved this session is readable;
            // the sentence's frozen coordinates win and are passed through
            // verbatim, never re-resolved.
            Some(id) if !id.is_empty() => (id.to_string(), r.terminal.unwrap_or(true)),
            _ => {
                let latest = &body["latest"];
                let round_id = latest["round_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ApiError::bad_request(&format!(
                            "referenced session '{}' has no round to quote",
                            r.session_id
                        ))
                    })?
                    .to_string();
                let terminal = latest["terminal"].as_bool().unwrap_or(false);
                (round_id, terminal)
            }
        };
        pinned.push(serde_json::json!({
            "agent_instance_id": r.agent_instance_id,
            "session_id": r.session_id,
            "round_id": round_id,
            "terminal": terminal,
        }));
    }
    Ok(pinned)
}

/// The `params` payload of a chat delivery — the one place the Chat Router's
/// input keys are assembled.
///
/// Both chat entries (`chat-message` 1:1 and `chat-mention` fan-out) hand the
/// round to the Runtime as `params_json`, and the Runtime reads keys out of it
/// by name: `message`, `chat_attachment_ids`, `chat_attachments`, `chat_kind`,
/// `chat_prompt`. There is no shared struct on the wire, so a key renamed (or
/// mistyped) on this side is not a compile error — and because the Runtime's
/// reader is deliberately tolerant, it is not a runtime error either: the turn
/// simply routes as if it carried nothing. Building both entries here, with the
/// attachment key taken from [`CHAT_ATTACHMENTS_PARAM`], is what keeps the two
/// halves of that contract in one place.
///
/// `attachments` is the **stored** metadata (the user service re-reads it from
/// the session's `files/` dir), never a client claim; ids travel separately on
/// the envelope, and the bytes never travel at all (design 27 §4.2.1).
///
/// `address` is ADR-106 D6/D7's explicit addressing: the execution session the
/// human named (by replying to that round's block, or by picking the session in
/// the second-level `@`), whether the agent was addressed at all, and whether
/// continuation was refused. The session rides `params` under
/// [`TARGET_SESSION_PARAM`] so addressing needs no wire change, and the Runtime
/// reads it **before** the Chat Router — an addressed turn is never guessed at.
/// What one Chat delivery was addressed to (ADR-106 D6/D7). Grouped into one
/// argument because the three facts always travel together: an address, whether
/// the human named the agent at all, and whether they refused continuation.
#[derive(Debug, Clone, Copy, Default)]
struct DeliveryAddress<'a> {
    /// Execution session named by a reply to its block (D6 level 1) or by the
    /// second level of the `@` (level 2).
    session: Option<&'a str>,
    /// The agent was explicitly named by the human. With no address this is what
    /// lets the Runtime apply level 3 ("exactly one routable session → deliver"),
    /// a rule that must never fire for a message nobody addressed.
    explicit_mention: bool,
    /// The human asked for new work, so no session may be continued.
    force_new: bool,
    /// Pinned `@session` reference material for **this** target (ADR-106
    /// revision 2026-11-04): the `meta.refs` entries whose
    /// `agent_instance_id` equals the delivery's agent, nothing else — a
    /// quoted round travels to the agent the human quoted it for, the same
    /// per-target discipline as `session`. The Runtime reads the key to
    /// suppress level 3 and to assemble the material.
    refs: &'a [serde_json::Value],
}

fn chat_delivery_params(
    text: &str,
    attachment_ids: &[String],
    attachments: &[ChatAttachmentMeta],
    chat_kind: &str,
    chat_prompt: Option<&str>,
    address: DeliveryAddress<'_>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "message": text,
        "chat_attachment_ids": attachment_ids,
        "chat_kind": chat_kind,
        // ADR-099 (extended): a `chat.md` prompt override, when the session
        // has one, replaces the whole chat-layer prompt for this turn.
        "chat_prompt": chat_prompt,
    });
    params[CHAT_ATTACHMENTS_PARAM] = serde_json::to_value(attachments)
        // `Vec<ChatAttachmentMeta>` has no map keys to fail on; degrade to
        // "no attachments" rather than to a broken params object.
        .unwrap_or(serde_json::Value::Null);
    // Absent rather than empty: an empty session id is not "addressed at
    // nothing", it is malformed, and the Runtime's reader must be able to tell
    // the two apart.
    if let Some(sid) = address.session.map(str::trim).filter(|s| !s.is_empty()) {
        params[TARGET_SESSION_PARAM] = serde_json::Value::String(sid.to_string());
    }
    // Written only when true: the two flags are routing opinions, and a params
    // object full of `false` noise would put them in the model's prompt for
    // every turn that never had one.
    if address.explicit_mention {
        params[acowork_core::protocol::EXPLICIT_MENTION_PARAM] = serde_json::Value::Bool(true);
    }
    if address.force_new {
        params[acowork_core::protocol::FORCE_NEW_PARAM] = serde_json::Value::Bool(true);
    }
    // Absent when empty for the same reason as the session address: a key that
    // says "this turn quotes nothing" is noise the model would have to ignore.
    if !address.refs.is_empty() {
        params[CHAT_REFS_PARAM] = serde_json::Value::Array(address.refs.to_vec());
    }
    params
}

/// ADR-106 D6 level 1: replying to a block IS addressing.
///
/// The `execution_round` reference row names the execution session and the agent
/// instance that owns it, so a reply to that row can be delivered without asking
/// a model what it was about. Returns `(agent instance, execution session)` when
/// the fetched row is a reference row carrying both coordinates; anything else
/// (a card, a plain utterance, a row that is not in this session) yields `None`
/// and the caller keeps its own path.
///
/// Scope is what makes this safe rather than convenient: the row was read
/// through a participant-checked fetch **inside this chat session**, so a
/// reference cannot be smuggled in from another chat (acceptance 9).
fn addressed_execution_round(row: Option<&serde_json::Value>) -> Option<(String, String)> {
    let row = row?;
    if row["kind"].as_str() != Some(acowork_core::intent::chat_kinds::EXECUTION_ROUND) {
        return None;
    }
    let meta = &row["meta"];
    let agent = meta["agent_instance_id"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    let session = meta["agent_session_id"].as_str().filter(|s| !s.is_empty())?;
    Some((agent.to_string(), session.to_string()))
}

/// `POST /api/intents/mentions` — the group-chat send entry (§5): the human
/// utterance is ONE `user_message` row carrying the agents the sender named in
/// its `mentions` field (§5.3.1); each named agent gets its own routed delivery
/// (initiator role, §5.4.5) and opens its own `execution_round` reference row
/// under that row (ADR-106 D12 — the turn's answer is read back through the
/// round's projection, it is not a second chat row). The session's repo binding
/// is injected server-side (§4.1).
///
/// **Un-addressed messages come here too** (ADR-105). `targets: []` means the
/// human named nobody, which is the overwhelmingly common way to answer an
/// agent that just spoke — and which used to be handed to the plain chat write
/// entry, where it was stored and forgotten. The group's configured default
/// responder takes the turn; with none configured the row is archived exactly
/// as before, and the response says so (`routed: "archived"`).
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
    // A group message may carry text, attachments, or both — but not neither.
    // Targets are optional since ADR-105: an empty list asks the Gateway to
    // resolve the group's default responder (or archive, when it has none).
    if req.text.trim().is_empty() && req.attachment_ids.is_empty() {
        return Err(ApiError::bad_request(
            "a group message needs text and/or attachments",
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
    let repo = session.repo.clone().unwrap_or_default();

    // ── Routing (ADR-105) ───────────────────────────────────────────────
    // An explicit @ is the human's own answer to "who handles this", so the
    // targets are used as given. With no @ the group's default responder — if
    // it has one and it is still a live agent member — answers instead; with
    // neither, the message keeps the historical archive-only behaviour (§5.3.1
    // before ADR-105): it is stored, no agent wakes, no error.
    //
    // Resolved BEFORE the row is written because the decision belongs on the
    // row: `auto_routed_to` is a fact of that utterance ("this one went to X
    // without being addressed to X"), not a separate event to reconcile later.
    //
    // ADR-106 D6: the ladder's first rung is checked first. A reply to a block
    // names both the agent and the execution session, so it resolves the target
    // the same way an `@` does — from a durable row, never from a guess.
    let replied_round = if req.in_reply_to.trim().is_empty() {
        None
    } else {
        chat_client::message_by_id(port, &req.chat_session_id, &req.from_user, req.in_reply_to.trim())
            .await
            .ok()
            .flatten()
    };
    let addressed = addressed_execution_round(replied_round.as_ref());
    let (auto_routed_to, targets): (Option<String>, Vec<String>) = match &addressed {
        // The session the reply named travels in `addressed` itself, which the
        // fan-out reads per target — it is not this decision's business.
        Some((agent, _)) => (Some(agent.clone()), vec![agent.clone()]),
        None => {
            let responder = if req.targets.is_empty() {
                session.live_default_responder().map(str::to_string)
            } else {
                None
            };
            let targets: Vec<String> = if req.targets.is_empty() {
                responder.iter().cloned().collect()
            } else {
                req.targets.clone()
            };
            (responder, targets)
        }
    };

    // ADR-106 D7: an address is a fact the human supplied, so it is never
    // dropped and never re-decided — see [`validate_mention_addresses`].
    validate_mention_addresses(&req.target_sessions, &req.force_new_for, &targets, &req.refs)?;

    // ADR-106 revision 2026-11-04: pin the reference material BEFORE the row
    // exists. A pin that cannot be resolved answers 400/502 and the row is
    // never written — the coordinates on a durable row are facts, and a row
    // that could not establish its own facts is a lie with an id.
    let pinned_refs =
        pin_reference_rounds(&state, &req.refs, &verified_user, &req.chat_session_id).await?;

    // §5.3.1 (Wave 2B): one human utterance is ONE `user_message` row;
    // the mentioned targets are a field on it (`mentions`), not N copies
    // of the same text. 落库先于投递 (§5.1): if the row cannot be written
    // the whole fan-out is refused — the message would have no durable
    // address and no reply coordinate.
    //
    // `mentions` keeps its meaning — the agents the HUMAN named — so an
    // auto-routed target never appears there (the transcript would draw a
    // chip the sender never typed). The auto decision travels in
    // `auto_routed_to`, which records where the sentence went and drives no
    // rendering: a surface's wait line claims its OWN send only, keyed by the
    // `message_id` returned below and ended by the first row that answers it
    // (ADR-106 D9 / D14, superseding ADR-105 D4's evidence-slot reading).
    let row_id = match req.message_id.as_deref().map(str::trim) {
        None | Some("") => uuid::Uuid::new_v4().to_string(),
        Some(id) if is_valid_row_id(id) => id.to_string(),
        Some(_) => {
            return Err(ApiError::bad_request(
                "message_id must be 1-64 chars of [A-Za-z0-9_-]",
            ));
        }
    };
    let mut meta = serde_json::json!({
        "message_id": row_id,
        "mentions": req.targets,
        "repo": repo,
    });
    // ADR-106 D6 level 1: the block this utterance answers, so the transcript
    // can render the reply inside that block instead of beside it.
    //
    // ADR-106 D15 (2026-11-06) made the coordinate a durable fact the store
    // vetoes: a row may only name a coordinate that resolves to a row in this
    // log. So the quote is recorded *only when it resolved* — `replied_round`
    // is the row the ladder found above. When the human answered a card that is
    // not in the log (cleaned up, or a stale client id), the routing already
    // fell through to a plain turn (§5.5.4: the utterance is never dropped);
    // the row must then carry no coordinate at all — an origin row, which D15
    // always accepts. Recording the dangling id would have the store reject the
    // append and turn a human's plain message into a 5xx, which is the opposite
    // of "not dropped".
    if replied_round.is_some() && !req.in_reply_to.trim().is_empty() {
        meta["in_reply_to"] = serde_json::Value::String(req.in_reply_to.trim().to_string());
    }
    // The pinned quadruples, durable with the row: the transcript renders them
    // as source chips and an addressing-card answer echoes them back verbatim
    // so the re-delivery carries the same material it asked about (ADR-106
    // revision 2026-11-04). Coordinates only — never round content.
    if !pinned_refs.is_empty() {
        meta["refs"] = serde_json::Value::Array(pinned_refs.clone());
    }
    if let Some(responder) = auto_routed_to.as_deref()
        && let Some(map) = meta.as_object_mut()
    {
        map.insert(
            "auto_routed_to".to_string(),
            serde_json::Value::String(responder.to_string()),
        );
    }
    // The stored row's attachment metadata (name / mime / size, re-read from
    // the session's `files/` dir by the store) rides along to every mentioned
    // agent, so its Chat Router sees what this turn actually carried. Ids still
    // travel on the envelope; the bytes never travel at all (design 27 §4.2.1).
    let opened = match chat_turn::open(
        port,
        &session,
        chat_turn::Turn {
            session_id: &req.chat_session_id,
            from: &req.from_user,
            to: "",
            message_id: &row_id,
            body: &req.text,
            meta,
            attachment_ids: req.attachment_ids.clone(),
        },
    )
    .await
    {
        Ok(opened) => opened,
        Err(e) => {
            return Err(ApiError::service_unavailable(&format!(
                "user_message chat row append failed — mention fan-out refused: {e}"
            )));
        }
    };
    let chat_attachments = opened.attachments;

    // Archive-only: the row is the whole outcome. Reported explicitly so a
    // caller can tell "nobody was asked" from "everybody was and they all
    // failed" — the two used to be indistinguishable (and both silent).
    if targets.is_empty() {
        return Ok(Json(serde_json::json!({
            "message_id": row_id,
            "routed": "archived",
            "results": [],
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

    let mut results = Vec::new();
    for target in &targets {
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
            report_delivery_alert(
                port,
                &req.chat_session_id,
                target,
                &row_id,
                "target_not_installed",
            )
            .await;
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
            report_delivery_alert(
                port,
                &req.chat_session_id,
                target,
                &row_id,
                "not_authorized",
            )
            .await;
            continue;
        }

        // Per-target delivery coordinate: the mailbox entry (and thus the
        // write-back + sweeper tracking) addresses this uuid; the durable
        // row it answers is the shared `user_message` above (`in_reply_to`
        // → recorded as `result_ref`, read back by `deliver_response`).
        let message_id = uuid::Uuid::new_v4().to_string();
        // This target's place on the ladder (ADR-106 D6/D7). A level-1 reply
        // names the round's own session; a level-2 pick names the session the
        // human chose for this agent. A refusal to continue is dropped wherever
        // an address exists: the Runtime must never read "continue exactly this
        // session" and "start a new one" in one turn. Reference material is
        // filtered per target for the same reason an address is per-target:
        // `@A @B` quoting A's round hands material to A, not to B.
        let target_refs: Vec<serde_json::Value> = pinned_refs
            .iter()
            .filter(|v| v["agent_instance_id"].as_str() == Some(target.as_str()))
            .cloned()
            .collect();
        let address = match &addressed {
            Some((agent, sid)) if agent == target => DeliveryAddress {
                session: Some(sid.as_str()),
                explicit_mention: true,
                force_new: false,
                refs: target_refs.as_slice(),
            },
            _ => {
                let picked = req.target_sessions.get(target).map(String::as_str);
                DeliveryAddress {
                    session: picked,
                    // Per-target on purpose: in a fan-out the human may have @-ed
                    // two agents while a third arrives as the group's default
                    // responder (ADR-105). Auto-routing is configuration, not an
                    // address, so level 3 must not fire for it.
                    explicit_mention: req.targets.iter().any(|t| t == target),
                    force_new: picked.is_none()
                        && req.force_new_for.iter().any(|f| f == target),
                    refs: target_refs.as_slice(),
                }
            }
        };
        let params = chat_delivery_params(
            &req.text,
            &req.attachment_ids,
            &chat_attachments,
            &chat_kind,
            session.chat_prompt.as_deref(),
            address,
        );
        let dreq = delivery::DeliveryRequest {
            message_id: message_id.clone(),
            // §4.2: a human-initiated round carries no agent conversation
            // — it never counts against the R4 budget; continuity lives in the
            // Chat transcript's reference rows and the addressing ladder
            // (ADR-106 D6), not in a conversation id.
            conversation_id: String::new(),
            from_instance: req.from_user.clone(),
            from_session_id: String::new(),
            target_instance: target.clone(),
            action: "chat".to_string(),
            params_json: params.to_string(),
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
            // ADR-106 D3: the shared `user_message` row this fan-out serves.
            // `message_id` above is this target's own delivery uuid and names
            // no row — the declared coordinate is what the Runtime threads the
            // round's reference row and any card under.
            chat_row_id: row_id.clone(),
            // Group fan-out stays with the router: which of the mentioned
            // agents owns which execution session is not something the
            // Gateway can attribute to a single id (ADR-100).
        };
        let outcome = delivery::wake_and_deliver(
            &ctx,
            &dreq,
            acowork_core::timeout_config::constants::INTENT_DEFAULT,
        )
        .await;
        let failed = match outcome {
            Ok(()) => None,
            Err(e) => Some((e.code(), e.to_string())),
        };
        results.push(match failed.as_ref() {
            None => serde_json::json!({
                "target": target, "message_id": message_id, "status": "delivered",
            }),
            Some((code, error)) => serde_json::json!({
                "target": target, "message_id": message_id, "status": "failed",
                "error_code": code, "error": error,
            }),
        });
        if let Some((code, _)) = failed {
            report_delivery_alert(port, &req.chat_session_id, target, &row_id, code).await;
        }
    }

    Ok(Json(serde_json::json!({
        "message_id": row_id,
        "routed": if auto_routed_to.is_some() { "responder" } else { "mentions" },
        "results": results,
    })))
}

/// Report a failed delivery **live** (ADR-105 D5, revision 2026-11-06).
///
/// A surface may read this fan-out's `results` to decide whether its OWN send is
/// worth claiming as in flight (ADR-106 D9), but that response is seen by the
/// posting device alone. So the failure has to reach the Chat — but as news, not
/// as history: the receipt used to be a durable `system_notice` row, written on
/// the **target's** behalf, and nothing ever retracted it. A send that failed and
/// was then retried successfully still carried "the message wasn't delivered" in
/// the transcript weeks later, and every reload painted it again.
///
/// What replaces it is an in-memory alert on the user service, attached to the
/// human row this delivery owed and served as `delivery.alert` until it expires
/// (`acowork_user::chat_alerts`). Nothing is appended, so nothing has to be
/// cleaned up later: the receipt stops existing exactly when it stops being true.
///
/// Best-effort: an alert that cannot be recorded costs the receipt, never the
/// send (the human row is already durable).
async fn report_delivery_alert(
    port: u16,
    chat_session_id: &str,
    target: &str,
    row_id: &str,
    error_code: &str,
) {
    if let Err(e) =
        chat_client::record_delivery_alert(port, chat_session_id, row_id, target, error_code).await
    {
        tracing::warn!(
            chat_id = %chat_session_id,
            target = %target,
            error = %e,
            "delivery_failed alert not recorded — non-fatal"
        );
    }
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
    /// ADR-106 D6 level 1: the round block this message answers, as its
    /// reference row's `message_id`. A reply to a reference row names its
    /// execution session and the agent that owns it, so the turn is delivered
    /// there deterministically and no model has to guess what it was about.
    /// It is NOT an answer mechanism: a card is no longer a chat row (D8), and
    /// answering one goes to the execution session's own write entry.
    /// Empty = a plain message.
    #[serde(default)]
    pub in_reply_to: String,
    /// ADR-106 D7's second-level `@`, 1:1 form: the execution session the human
    /// picked for this message. With one agent in the chat the first level is
    /// degenerate, so the picker opens straight on sessions. Empty = no address
    /// (and a plain DM message is therefore a level-5 turn, not a continuation).
    /// **Answering channel** (ADR-106 revision 2026-11-04, narrow retirement):
    /// the `@` picker no longer produces this field — it delivers level-3
    /// continuation-card answers only, the group form being `target_sessions`.
    #[serde(default)]
    pub target_session: Option<String>,
    /// ADR-106 revision 2026-11-04: the 1:1 form of the `@session` material
    /// references — see [`MentionRequest::refs`]. With one agent in the chat
    /// every ref must name that agent; a ref beside a `target_session` pick is
    /// the same contradiction the group form refuses.
    #[serde(default)]
    pub refs: Vec<MentionRef>,
    /// ADR-106 D6's cheap escape from continuation — "new work, continue
    /// nothing". Consulted only when this message carries no session address.
    #[serde(default)]
    pub force_new: bool,
    /// on a chat message that opens a new execution turn (model / workspace
    /// / reasoning-effort chosen on the Chat surface before sending). Only
    /// a shape-checked object of string values; the Runtime clamps it to the
    /// target's allowed set at execution-session creation (§13-6). Absent /
    /// null = target defaults.
    #[serde(default)]
    pub session_config: Option<serde_json::Value>,
    /// Design 27 §4.2.1: ids of attachments already uploaded to this Chat
    /// session's `files/` dir. The Gateway pins them on the durable
    /// `user_message` row (so history renders and downloads them) and
    /// forwards their bytes into the execution turn (design 27 §4.2.1
    /// chat-attachment path). Empty = a plain text message.
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    /// Client-supplied row id: the composer's optimistic bubble already minted
    /// one, and the SAME id lands on the durable row. The value is also the
    /// store's idempotency key and the mailbox key, so a retry settles on one
    /// row instead of two. Empty = the Gateway generates one.
    #[serde(default)]
    pub message_id: Option<String>,
    /// ADR-046 `AttachedItem` union the composer carried - uploaded blobs (a
    /// content-hash `documentId` into this Agent's `files/`) and workspace
    /// references (a node-local `absPath`). The `Intent` arm carries them into
    /// whichever execution session takes the turn, so "Add to Chat" reaches the
    /// Agent; the bytes are never copied into the Chat store.
    #[serde(default)]
    pub attached_items: Vec<serde_json::Value>,
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
    // A workspace reference is a node-local path, not a chat upload: the bytes
    // ride the Intent arm into whichever execution session takes the turn, and
    // the Chat row keeps only the display label for it.
    let attached_items: Vec<serde_json::Value> = {
        if req.attached_items.len() > MAX_CHAT_ATTACHMENTS {
            return Err(ApiError::bad_request(&format!(
                "attached_items accepts at most {MAX_CHAT_ATTACHMENTS} entries"
            )));
        }
        let attached_json = serde_json::to_string(&req.attached_items).unwrap_or_default();
        if attached_json.len() > MAX_CHAT_ATTACHMENTS_JSON_LEN {
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
    };
    // A display-only projection for the Chat row (see
    // [`exec_attachment_labels`]). The execution session stays the attachment's
    // only home and the chat records only that something was attached.
    let exec_attachments = exec_attachment_labels(&attached_items);
    // Content gate — the same three proofs the Chat store accepts (§4.2:
    // `body` / `attachments` / `meta.exec_attachments`), computed from what this
    // turn actually carries. A composer may legitimately hand the
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
    // §5.5.4 answer channel / ADR-106 D6 level 1: whatever this message replies
    // to is read once, server-side, and the same row answers every question
    // asked of it — which agent a group answer belongs to, whether it resolves
    // a live card, whether it addresses a block. Fetching twice would let those
    // answers disagree.
    let replied = if req.in_reply_to.trim().is_empty() {
        None
    } else {
        chat_client::message_by_id(
            port,
            &req.chat_session_id,
            &req.from_user,
            req.in_reply_to.trim(),
        )
        .await
        .ok()
        .flatten()
    };
    // Level 1: a reply to a round's reference row names its execution session and
    // the agent that owns it, so no model has to work out what it was about.
    let addressed = addressed_execution_round(replied.as_ref());
    let target: String = match session.kind.as_str() {
        "user_agent" => session
            .participants
            .iter()
            .map(|p| p.id().to_string())
            .find(|id| id != &req.from_user)
            .ok_or_else(|| ApiError::not_found("no agent participant in this session"))?,
        // Group surfaces reuse this channel ONLY to answer a round's block
        // (D6 level 1), which names its own agent. Plain group messages are
        // archive-only via the chat write entry; @-fan-out goes to
        // `/api/intents/mentions`. A card is not a chat row any more (D8), so
        // nothing else in a group is answerable through here.
        "project_group" | "user_group" => addressed
            .as_ref()
            .map(|(agent, _)| agent.clone())
            .ok_or_else(|| {
                // Which agent answers is not the Gateway's guess: it is the
                // block the human is replying to, or an @-mention. A bare
                // group message has no target, so it fails here rather than
                // picking one.
                ApiError::bad_request(
                    "chat-message in a group replies to an execution_round block only; plain group messages post to /api/chat-sessions/{id}/messages and @-fan-out goes to /api/intents/mentions",
                )
            })?,
        other => {
            return Err(ApiError::bad_request(&format!(
                "chat-message does not target '{other}' sessions"
            )));
        }
    };
    // The addressed session travels only when it belongs to the agent taking
    // this turn. A reference row naming another agent is a different
    // conversation, and explicit addressing is never re-interpreted.
    let addressed_session = addressed
        .filter(|(agent, _)| agent == &target)
        .map(|(_, session)| session);
    let repo = session.repo.clone().unwrap_or_default();

    // ADR-106 revision 2026-11-04: the 1:1 reference material. The single agent
    // is the only legal ref owner, so it stands in for the group's `targets`;
    // a `target_session` pick beside refs is the same delivery-vs-quote
    // contradiction the group form refuses. The pin runs before the row is
    // written and a failed pin refuses the send (400/502, no row).
    let picked = req
        .target_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut dm_target_sessions = std::collections::HashMap::new();
    if let Some(picked_sid) = picked {
        dm_target_sessions.insert(target.clone(), picked_sid.to_string());
    }
    validate_refs(&req.refs, std::slice::from_ref(&target), &dm_target_sessions)?;
    let pinned_refs =
        pin_reference_rounds(&state, &req.refs, &verified_user, &req.chat_session_id).await?;

    let (Some(mqtt), Some(registry)) = (mqtt.as_ref(), registry.as_ref()) else {
        return Err(ApiError::internal("Gateway MQTT control plane is not available"));
    };

    // 落库先于投递 (§5.1): the durable `user_message` row exists before
    // the Agent may act on it. The Gateway generates the id so the
    // mailbox entry and the write-back share one coordinate. The row is
    // the durable record of a card answer too — resolution is derived
    // from it, never from a second write (§5.5.4).
    //
    // ADR-100: the composer supplies its own id so the bubble it already
    // rendered reconciles with this row (and with the execution session's copy
    // of the same message) instead of duplicating.
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
    // `attached_items` / `exec_attachments` were normalized above, ahead of
    // the content gate, so the row and the delivery describe one and the
    // same turn.
    let mut meta = serde_json::json!({
        "message_id": message_id,
        "exec_attachments": exec_attachments,
    });
    // ADR-106 D15: a durable row may only name a coordinate that resolves to a
    // row in this log. `replied` is that resolution, read once above. When the
    // human answered a card that is not in the log (cleaned up, or a stale
    // client id), the routing already fell through to a plain turn (§5.5.4: the
    // utterance is never dropped) — the row must then carry no coordinate at
    // all, an origin row D15 always accepts. Recording the dangling id would
    // have the store reject the append and turn a human's plain message into a
    // 5xx, the opposite of "not dropped".
    if replied.is_some() && !req.in_reply_to.trim().is_empty() {
        meta["in_reply_to"] = serde_json::Value::String(req.in_reply_to.trim().to_string());
    }
    // Pinned reference coordinates, durable with the row — the 1:1 twin of the
    // group form: chips render from them and the row is the source of truth for
    // what the turn quoted (ADR-106 revision 2026-11-04).
    if !pinned_refs.is_empty() {
        meta["refs"] = serde_json::Value::Array(pinned_refs.clone());
    }
    // Stored attachment metadata, not the client's word: the store re-reads
    // name / mime / size from this session's `files/` dir. Forwarded so the
    // receiving Chat Router can see what the turn carried (its history window
    // shows the same line for every older row, so the current turn must match).
    let opened = match chat_turn::open(
        port,
        &session,
        chat_turn::Turn {
            session_id: &req.chat_session_id,
            from: &req.from_user,
            to: &target,
            message_id: &message_id,
            body: &req.text,
            meta,
            attachment_ids: req.attachment_ids.clone(),
        },
    )
    .await
    {
        Ok(opened) => opened,
        Err(e) => {
            return Err(ApiError::service_unavailable(&format!(
                "user_message chat row append failed — delivery refused: {e}"
            )));
        }
    };
    let chat_attachments = opened.attachments;

    // The human's row is durable and the turn is addressed. ADR-106 D8: there
    // is no card-answer short-circuit any more — a card is a fact of the round
    // (the projection's `card` field), and answering it writes the execution
    // session directly, not this channel.
    let ctx = DeliveryContext {
        state: &state.gateway_state,
        registry,
        node_control: node_control.as_ref(),
        mqtt: Some(mqtt),
        mailbox: mailbox.as_deref(),
    };
    // `attached_items` is the Intent's attachment arm: the workspace
    // references the composer carried ride it into whichever execution session
    // takes the turn.
    // ADR-106 D7: in a 1:1 the first level of the `@` is degenerate, so a session
    // pick arrives as `target_session` — that is level 2. A plain DM message is
    // NOT an address: D6 puts 单聊 at level 5, where the Router may only answer or
    // open new work. `explicit_mention` therefore stays false here, so the
    // deterministic level-3 shortcut never swallows a DM's first "hi".
    let address = DeliveryAddress {
        session: addressed_session.as_deref().or(picked),
        explicit_mention: false,
        // Meaningless beside an address: the human already chose. Carrying both
        // would let the Runtime read "continue exactly this session" and
        // "continue nothing" out of one turn.
        force_new: addressed_session.is_none() && picked.is_none() && req.force_new,
        // The single agent owns every ref here — validation enforced it.
        refs: pinned_refs.as_slice(),
    };
    let mut params = chat_delivery_params(
        &req.text,
        &req.attachment_ids,
        &chat_attachments,
        &session.kind,
        session.chat_prompt.as_deref(),
        address,
    );
    {
        let object = params.as_object_mut().expect("chat params are an object");
        object.insert("attached_items".to_string(), serde_json::json!(attached_items));
    }
    let dreq = delivery::DeliveryRequest {
        message_id: message_id.clone(),
        conversation_id: String::new(),
        from_instance: req.from_user.clone(),
        from_session_id: String::new(),
        target_instance: target.clone(),
        action: "chat".to_string(),
        params_json: params.to_string(),
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
        // ADR-106 D3: this delivery is the row it was written for — the
        // Gateway generated the id, put it in the row and delivers under it, so
        // the declared coordinate and `message_id` are one value.
        chat_row_id: message_id.clone(),
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

/// Body of `POST /api/intents/{id}/accept`.
#[derive(Debug, Deserialize)]
struct IntentAcceptRequest {
    /// The reporting instance. Verified like any sender claim (D4): the
    /// value is never adopted from the body, only matched against the
    /// registered identity.
    sender: String,
    /// `true` = routed and handed to a session; `false` = refused.
    accepted: bool,
    /// Refusal reason, verbatim. Ignored when `accepted`.
    #[serde(default)]
    error: String,
}

/// `POST /api/intents/{message_id}/accept` — the target Runtime states
/// whether it took the Intent.
///
/// This closes the gap that stranded the PM task on 2026-11-04: the Gateway
/// could observe its own publish but not the receiver's routing decision, so
/// a Runtime answering `no such column: intent_key` looked delivered and the
/// sender, having no error, never retried.
///
/// Only the recorded target may speak for a message. A rejection is what
/// makes the sender retry, so an unrelated caller must be able neither to
/// fake one nor to overwrite a real one. Recording is the whole job — the
/// handler answers as soon as the fact is stored (and wakes a waiting
/// `task-dispatch`); it never tries to deliver anything itself.
async fn accept_intent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(message_id): Path<String>,
    Json(req): Json<IntentAcceptRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    verify_sender_identity(&state, &headers, &req.sender).await?;

    let mailbox = {
        let gw = state.gateway_state.read().await;
        gw.intent_mailbox.clone()
    };
    let mailbox = mailbox
        .as_ref()
        .ok_or_else(|| ApiError::internal("intent mailbox is not initialised"))?;
    let entry = mailbox
        .get(&message_id)
        .ok_or_else(|| ApiError::not_found("no such intent message id"))?;
    let reporter_is_target = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(&req.sender).as_deref() == Some(entry.to_instance.as_str())
    };
    if !reporter_is_target {
        return Err(ApiError::forbidden(
            "only the recorded target of the intent may accept or reject it",
        ));
    }

    let acceptance = if req.accepted {
        accept::Acceptance::accepted()
    } else {
        accept::Acceptance::rejected(req.error.trim())
    };
    mailbox
        .record_acceptance(&message_id, &acceptance)
        .map_err(|e| ApiError::internal(&format!("mailbox write failed: {e}")))?;
    // Wakes the dispatch waiting on this id; a receipt nobody waits for
    // (every non-PM intent) is a no-op.
    let woken = accept::notify(&message_id, acceptance.clone());

    tracing::info!(
        message_id = %message_id,
        to = %entry.to_instance,
        status = acceptance.status(),
        woken,
        "intent acceptance recorded"
    );
    Ok(Json(serde_json::json!({
        "message_id": message_id,
        "status": "recorded",
        "accepted": acceptance.accepted,
    })))
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
        // ADR-100: `delivered` alone cannot tell a closed round from one
        // the sweeper still owes a receipt for. A turn composed in the execution
        // view has no Intent write-back, so its answer arrives as a chat row
        // and this is the only place an operator can see the request was
        // really answered.
        "answered": entry.answered(),
        "answered_at": entry.answered_at,
        // The receiving Runtime's acceptance decision, when it reported one
        // (`POST /api/intents/{id}/accept`). `accepted == null` means *no
        // receipt* — a pre-receipt Runtime or one that died before routing —
        // which is not the same as a refusal and must not be read as one.
        "accepted": if entry.acceptance.is_empty() {
            None
        } else {
            Some(!entry.acceptance.is_rejected())
        },
        "accepted_at": entry.acceptance.at,
        "accept_error": entry.acceptance.error,
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
mod chat_append_tests {
    use super::*;

    /// ADR-100: which Runtime-owned chat row closes a pending mailbox
    /// request. Since ADR-106 D12 the router's own answer arrives through this
    /// channel instead of an Intent write-back, so this is the only thing that
    /// keeps §7.1's sweeper from filing a "no answer will arrive" receipt
    /// beside a chat that already read the real answer — and the only
    /// thing that keeps a card's execution-side request id (which is not a chat
    /// row at all) from being mistaken for one.
    #[test]
    fn only_an_echo_that_answers_a_human_row_closes_the_request() {
        let echo = serde_json::json!({
            "message_id": "reply:m-1",
            "in_reply_to": "m-1",
        });
        assert_eq!(
            answered_request("agent_message", &echo).as_deref(),
            Some("m-1"),
            "the router's answer threaded under the human's row"
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
        let standalone = serde_json::json!({ "message_id": "reply:m-2" });
        assert_eq!(
            answered_request("agent_message", &standalone),
            None,
            "an answer with no request threaded above it closes nothing"
        );
        let blank = serde_json::json!({ "message_id": "n-1", "in_reply_to": "" });
        assert_eq!(answered_request("agent_message", &blank), None);
        assert_eq!(answered_request("agent_message", &serde_json::json!({})), None);
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
        assert!(chat_append_kind_allowed("execution_round"));
        assert!(
            !chat_append_kind_allowed("interaction_request"),
            "a card is the round's state, not a chat row (ADR-106 D8)"
        );
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

    // ADR-106 D8: the card-answer routing tests are gone with the mechanism.
    // A card is no longer a Chat row, so nothing in this file resolves one or
    // maps a reply text onto an execution-session endpoint.

    // ── ADR-100: a client-supplied Chat row id ───────────────────────

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
    fn attachment_labels_name_a_file_and_only_a_workspace_arm_carries_a_path() {
        // Both union arms are named: an uploaded blob by `filename`, a
        // workspace reference by its path alone when nothing better was sent.
        // That path is reduced to its last component: a display name is not a
        // location, and a Windows path is the case a `/`-only split would
        // silently miss.
        //
        // The full path is still published, but only in `absPath` and only for
        // a workspace arm — it is the desktop's click coordinate (ADR-100 D12
        // Rev. 2026-11), never text. An upload arm carries none: its bytes
        // belong to one run, which is also why this type has no `documentId`.
        let items = vec![
            serde_json::json!({"type": "file_upload", "filename": "report.pdf", "documentId": "ab12cd34ef56-1", "format": "pdf", "sizeBytes": 12345}),
            serde_json::json!({"type": "attached_file", "absPath": "C:\\work\\src\\main.rs", "name": ""}),
        ];
        let labels = exec_attachment_labels(&items);
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].filename, "report.pdf");
        assert_eq!(labels[0].kind, "file_upload");
        assert_eq!(labels[0].size_bytes, Some(12345));
        assert_eq!(labels[0].abs_path, None, "an upload arm carries no path");
        assert_eq!(labels[1].filename, "main.rs");
        assert_eq!(labels[1].kind, "attached_file");
        assert_eq!(
            labels[1].abs_path.as_deref(),
            Some(r"C:\work\src\main.rs"),
            "the workspace arm keeps the path the client opens"
        );
        // The blob id never survives, and the path appears in exactly one
        // field of exactly one arm.
        let rendered = serde_json::to_string(&labels).unwrap();
        assert!(!rendered.contains("ab12cd34ef56-1"));
        assert_eq!(rendered.matches("work").count(), 1, "the path rides only in absPath: {rendered}");
        assert!(!labels[1].filename.contains("work"), "the name is still a name");
    }

    #[test]
    fn a_workspace_attachment_is_labelled_by_its_display_name() {
        // The workspace arms carry a `name` the execution view already shows
        // the user — a workspace-relative path, which is display text, not a
        // node-local layout, so it is quoted whole instead of trimmed. The
        // node-local path rides beside it in its own field, for the click.
        let items = vec![serde_json::json!({
            "type": "attached_selection",
            "name": "src/lib.rs",
            "absPath": "C:\\work\\project\\src\\lib.rs",
            "startLine": 12,
            "endLine": 25,
        })];
        let labels = exec_attachment_labels(&items);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].filename, "src/lib.rs");
        assert_eq!(labels[0].kind, "attached_selection");
        // The line range rides along: it is the one part of a selection a chip
        // can show without opening anything, and what the click uses to land
        // on the right line once it does open.
        assert_eq!(labels[0].start_line, Some(12));
        assert_eq!(labels[0].end_line, Some(25));
        assert_eq!(
            labels[0].abs_path.as_deref(),
            Some(r"C:\work\project\src\lib.rs"),
            "a selection opens at a path, so it carries one"
        );
        assert!(!labels[0].filename.contains("work"));
    }

    #[test]
    fn an_unnamable_attachment_is_dropped_not_rendered_blank() {
        // A chip with no label is noise the reader cannot act on, and one
        // empty name would otherwise occupy a slot the human counts on.
        let items = vec![
            serde_json::json!({"type": "file_upload", "filename": ""}),
            serde_json::json!({"type": "attached_folder", "absPath": "/srv/project", "name": ""}),
            serde_json::json!({"type": "image_upload"}),
        ];
        let labels = exec_attachment_labels(&items);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].filename, "project");
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

    /// The dispatch row's meta key set is a storage contract two parties read:
    /// the chat store (an undeclared key is silently dropped at that boundary —
    /// design 27 decision 29) and the surfaces. The *kind* and the *voice* are
    /// decided by `chat_turn::open` from the author's identity, and pinned
    /// there; what this pins is the payload.
    #[test]
    fn a_dispatch_row_carries_only_keys_the_store_declares() {
        let meta = dispatch_turn_meta("t-1", "task:p-1:t-1", "git@example/repo.git");
        assert_eq!(meta["conversation_id"], "task:p-1:t-1");
        assert_eq!(meta["task_id"], "t-1", "the row's origin marker and search key");
        assert_eq!(meta["repo"], "git@example/repo.git");
        assert!(
            meta.get("project_id").is_none(),
            "the row already lives in that project's own group"
        );
        assert!(
            meta.get("assignment_brief").is_none(),
            "a playbook in the chat log is the defect this row must not reintroduce"
        );
    }
}

#[cfg(test)]
mod chat_params_tests {
    use super::*;

    /// The Chat Router reads these keys out of `params_json` by name. Both
    /// chat entries build it here now, so this is the one place the wire shape
    /// is decided — and the one place it can be pinned.
    #[test]
    fn the_shared_builder_writes_every_key_the_router_reads() {
        let ids = vec!["att-1".to_string()];
        let stored = vec![ChatAttachmentMeta {
            filename: "report.pdf".into(),
            mime: "application/pdf".into(),
            size: 12_288,
        }];

        let params = chat_delivery_params(
            "look at this",
            &ids,
            &stored,
            "user_agent",
            Some("custom"),
            DeliveryAddress::default(),
        );

        assert_eq!(params["message"], "look at this");
        assert_eq!(params["chat_attachment_ids"][0], "att-1");
        assert_eq!(params["chat_kind"], "user_agent");
        assert_eq!(params["chat_prompt"], "custom");
        assert_eq!(
            params[CHAT_ATTACHMENTS_PARAM],
            serde_json::json!([
                {"filename": "report.pdf", "mime": "application/pdf", "size": 12_288}
            ]),
            "the stored metadata rides under the shared key, three fields only"
        );
    }

    /// The key is the half of the contract the compiler cannot check: the
    /// Runtime's reader tolerates every wrong shape, so a rename on one side
    /// reads as "this turn carried nothing" instead of failing. Pin the
    /// literal, so changing it is a deliberate two-sided decision.
    #[test]
    fn the_attachment_key_is_the_agreed_wire_name() {
        assert_eq!(CHAT_ATTACHMENTS_PARAM, "chat_attachments");
    }

    /// A turn without files must say so with an empty list, not by omitting
    /// the key or writing `null`: the Runtime reads "absent" and "empty" the
    /// same way, but the two are distinguishable in a captured payload — and
    /// only one of them survives a future `params` round-trip comparison.
    #[test]
    fn a_turn_without_files_still_carries_an_empty_list() {
        let params =
            chat_delivery_params("just text", &[], &[], "user_group", None, DeliveryAddress::default());
        assert_eq!(params[CHAT_ATTACHMENTS_PARAM], serde_json::json!([]));
        // An absent `chat.md` override keeps its pre-existing wire shape.
        assert_eq!(params["chat_prompt"], serde_json::Value::Null);
    }

    /// ADR-106 D6 level 1: replying to a block is addressing, so the reference
    /// row must yield both coordinates the delivery needs — the agent to wake
    /// and the session to continue.
    #[test]
    fn a_reply_to_a_reference_row_addresses_that_round() {
        let row = serde_json::json!({
            "kind": acowork_core::intent::chat_kinds::EXECUTION_ROUND,
            "from": "inst-a",
            "meta": {"message_id": "round:r:m-1", "round_id": "r:m-1",
                     "agent_session_id": "s-9", "agent_instance_id": "inst-a"},
        });
        assert_eq!(
            addressed_execution_round(Some(&row)),
            Some(("inst-a".to_string(), "s-9".to_string()))
        );
    }

    /// Everything that is not a reference row resolves to *nothing*, and the
    /// caller then falls to the next rung of the ladder. A decision card is
    /// answered through the §5.5.4 channel; a plain utterance is a quote; a
    /// reference missing either coordinate names no session, because a
    /// half-known address is not an address — delivering on it would put the
    /// human's words in a session nobody chose.
    #[test]
    fn only_a_complete_reference_row_addresses_a_session() {
        let card = serde_json::json!({"kind": "interaction_request",
            "meta": {"agent_session_id": "s-9", "agent_instance_id": "inst-a"}});
        assert_eq!(addressed_execution_round(Some(&card)), None);
        let plain = serde_json::json!({"kind": "user_message", "meta": {"message_id": "m-1"}});
        assert_eq!(addressed_execution_round(Some(&plain)), None);
        let torn = serde_json::json!({"kind": acowork_core::intent::chat_kinds::EXECUTION_ROUND,
            "meta": {"agent_session_id": "", "agent_instance_id": "inst-a"}});
        assert_eq!(addressed_execution_round(Some(&torn)), None);
        assert_eq!(addressed_execution_round(None), None);
    }

    /// The addressed session is the whole difference between "the human pointed
    /// at a round" and "a model guessed one", so its absence must be literal:
    /// an unaddressed turn carries no key at all, and a blank id must not
    /// smuggle in an empty session the Runtime would have to re-parse.
    #[test]
    fn an_addressed_turn_carries_the_session_and_an_unaddressed_one_does_not() {
        let addressed = chat_delivery_params(
            "go on",
            &[],
            &[],
            "user_agent",
            None,
            DeliveryAddress {
                session: Some("s-9"),
                ..DeliveryAddress::default()
            },
        );
        assert_eq!(addressed[TARGET_SESSION_PARAM], "s-9");
        let free = chat_delivery_params(
            "go on",
            &[],
            &[],
            "user_agent",
            None,
            DeliveryAddress::default(),
        );
        assert!(free.get(TARGET_SESSION_PARAM).is_none());
        let blank = chat_delivery_params(
            "go on",
            &[],
            &[],
            "user_agent",
            None,
            DeliveryAddress {
                session: Some("  "),
                ..DeliveryAddress::default()
            },
        );
        assert!(blank.get(TARGET_SESSION_PARAM).is_none());
        // One constant, two sides: the Runtime reads this exact spelling.
        assert_eq!(TARGET_SESSION_PARAM, "target_session_id");
    }

    /// The two routing flags are written only when they are true, and the keys
    /// are read by name across a process boundary the compiler cannot check: a
    /// rename on one side silently disables level 3 (every addressed turn pays an
    /// LLM) or re-enables continuation after the human refused it.
    #[test]
    fn the_routing_flags_are_written_under_the_agreed_keys() {
        let mentioned = chat_delivery_params(
            "go on",
            &[],
            &[],
            "user_agent",
            None,
            DeliveryAddress {
                explicit_mention: true,
                ..DeliveryAddress::default()
            },
        );
        assert_eq!(
            mentioned[acowork_core::protocol::EXPLICIT_MENTION_PARAM],
            serde_json::Value::Bool(true)
        );
        assert!(mentioned.get(acowork_core::protocol::FORCE_NEW_PARAM).is_none());
        let fresh = chat_delivery_params(
            "new thing",
            &[],
            &[],
            "user_agent",
            None,
            DeliveryAddress {
                force_new: true,
                ..DeliveryAddress::default()
            },
        );
        assert_eq!(
            fresh[acowork_core::protocol::FORCE_NEW_PARAM],
            serde_json::Value::Bool(true)
        );
        assert!(fresh.get(acowork_core::protocol::EXPLICIT_MENTION_PARAM).is_none());
        assert_eq!(acowork_core::protocol::EXPLICIT_MENTION_PARAM, "explicit_mention");
        assert_eq!(acowork_core::protocol::FORCE_NEW_PARAM, "force_new");
    }

    /// ADR-106 D7: the address is refused when the Gateway could not honour it,
    /// and never quietly re-decided. A fan-out may name a session for one agent
    /// while refusing continuation for another — that combination is the whole
    /// point of per-target addressing, so it must pass.
    #[test]
    fn an_address_is_accepted_only_for_targets_this_message_delivers_to() {
        let targets = vec!["a".to_string(), "b".to_string()];
        let mut sessions = std::collections::HashMap::new();
        sessions.insert("a".to_string(), "s-1".to_string());
        assert!(validate_mention_addresses(&sessions, &["b".to_string()], &targets, &[]).is_ok());

        // Nothing addressed is the normal case, not an error: it is what keeps
        // the turn on the ladder's lower levels.
        assert!(validate_mention_addresses(
            &std::collections::HashMap::new(),
            &[],
            &targets,
            &[]
        )
        .is_ok());

        // An address for a target this message does not deliver to would be
        // dropped by a lenient server — turning "continue that session" into
        // "guess one".
        let mut stray = std::collections::HashMap::new();
        stray.insert("c".to_string(), "s-9".to_string());
        assert!(validate_mention_addresses(&stray, &[], &targets, &[]).is_err());
        assert!(validate_mention_addresses(&sessions, &["c".to_string()], &targets, &[]).is_err());

        // A blank session id is not "no address": it names nothing while
        // claiming to name something.
        let mut blank = std::collections::HashMap::new();
        blank.insert("a".to_string(), "  ".to_string());
        assert!(validate_mention_addresses(&blank, &[], &targets, &[]).is_err());

        // And the contradiction: an address and a refusal to continue, in one
        // turn. The Runtime would have to pick, which is the guess D6 removes.
        assert!(validate_mention_addresses(&sessions, &["a".to_string()], &targets, &[]).is_err());
    }
}

/// `POST /api/intents/{id}/accept` — the receiving Runtime's routing verdict.
///
/// The contract these pin is what makes the PM retry machine work: a
/// rejection must be *recorded* (so `GET /api/intents/{id}` can explain a
/// silent task), the reason must survive verbatim (it is the only bug report
/// the sender ever gets), and it must not touch `delivered_at` (the publish
/// did happen; the two facts are allowed to disagree).
#[cfg(test)]
mod accept_route_tests {
    use super::*;
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use crate::intent::accept::AcceptanceWaiter;
    use crate::intent::mailbox::IntentMailbox;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    const TARGET: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER: &str = "22222222-2222-4222-8222-222222222222";

    fn manifest(agent_id: &str) -> acowork_core::AgentManifest {
        let toml = format!(
            "agent_id = \"{agent_id}\"\nversion = \"1.0.0\"\nname = \"Test\"\ndescription = \"d\"\nauthor = \"t\"\nruntime_version = \"0.1.0\"\n"
        );
        acowork_core::AgentManifest::from_toml(&toml).expect("minimal manifest")
    }

    async fn state_with(mailbox: Arc<IntentMailbox>) -> AppState {
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-intent-accept-{}",
            std::process::id()
        ));
        let mut gw = GatewayState::new(&dir.to_string_lossy());
        gw.intent_mailbox = Some(Arc::clone(&mailbox));
        for (instance, agent_id) in [(TARGET, "com.test.target"), (OTHER, "com.test.other")] {
            gw.installed_agents.insert(
                instance.to_string(),
                crate::gateway::state::AgentInfo {
                    instance_id: instance.to_string(),
                    agent_id: agent_id.to_string(),
                    version: "1.0.0".to_string(),
                    name: "Test".to_string(),
                    install_path: format!("/tmp/pkg/{agent_id}"),
                    manifest: manifest(agent_id),
                    node_id: "local".to_string(),
                },
            );
        }
        let mut state = AppState::new(
            Arc::new(tokio::sync::RwLock::new(gw)),
            Arc::new(HttpAuth::new(false)),
        );
        // Both instances are asserted online by their own MQTT session; the
        // identity check refuses a sender it cannot attribute.
        let reg = crate::mqtt::agent_registry::new_shared_registry();
        for instance in [TARGET, OTHER] {
            let envelope = acowork_core::mqtt_proto::DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(
                    acowork_core::mqtt_proto::AgentStatus {
                        agent_id: "com.test.target".to_string(),
                        online: true,
                        instance_id: instance.to_string(),
                        node_id: "local".to_string(),
                        state: acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady as i32,
                        detail: String::new(),
                    },
                )),
            };
            reg.write().await.update_from_mqtt(
                &format!("acowork/agents/{instance}/status"),
                &prost::Message::encode_to_vec(&envelope),
            );
        }
        state.agent_registry = Some(reg);
        state
    }

    fn entry(message_id: &str) -> crate::intent::mailbox::MailboxEntry {
        crate::intent::mailbox::MailboxEntry {
            message_id: message_id.to_string(),
            conversation_id: String::new(),
            from_instance: "owner".into(),
            from_session_id: String::new(),
            to_instance: TARGET.into(),
            body: "{}".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            delivered_at: chrono::Utc::now().to_rfc3339(),
            acceptance: Default::default(),
            result_ref: String::new(),
            kind: "task".into(),
            status: String::new(),
            source_chat_session_id: String::new(),
            answered_at: String::new(),
            last_activity_at: String::new(),
            has_human: None,
        }
    }

    async fn post(
        state: AppState,
        message_id: &str,
        sender: &str,
        accepted: bool,
        error: &str,
    ) -> StatusCode {
        let body = serde_json::json!({
            "sender": sender,
            "accepted": accepted,
            "error": error,
        });
        let request = Request::builder()
            .method("POST")
            .uri(format!("/api/intents/{message_id}/accept"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        intent_routes()
            .with_state(state)
            .oneshot(request)
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_rejection_is_recorded_verbatim_and_wakes_the_waiter() {
        let mailbox = Arc::new(IntentMailbox::in_memory());
        let message_id = "msg-rejected";
        mailbox.record(entry(message_id)).unwrap();
        let waiter = AcceptanceWaiter::register(message_id);

        let status = post(
            state_with(Arc::clone(&mailbox)).await,
            message_id,
            TARGET,
            false,
            "no such column: intent_key",
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let stored = mailbox.get(message_id).unwrap();
        assert!(stored.acceptance.is_rejected());
        assert_eq!(stored.acceptance.error, "no such column: intent_key");
        // The publish still happened: acceptance is an extra fact, not a
        // revision of the delivery one.
        assert!(stored.delivered());

        let acceptance = waiter
            .wait(acowork_core::timeout_config::constants::INTENT_ACCEPT)
            .await
            .expect("the waiting dispatch must be woken");
        assert!(!acceptance.accepted);
        assert_eq!(acceptance.error, "no such column: intent_key");
    }

    #[tokio::test]
    async fn only_the_recorded_target_may_speak_for_a_message() {
        let mailbox = Arc::new(IntentMailbox::in_memory());
        let message_id = "msg-forged";
        mailbox.record(entry(message_id)).unwrap();

        let status = post(
            state_with(Arc::clone(&mailbox)).await,
            message_id,
            OTHER,
            false,
            "forced failure",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Neither a forged rejection nor a forged acceptance may land.
        assert!(mailbox.get(message_id).unwrap().acceptance.is_empty());
    }

    #[tokio::test]
    async fn accepting_leaves_no_rejection_reason_behind() {
        let mailbox = Arc::new(IntentMailbox::in_memory());
        let message_id = "msg-accepted";
        mailbox.record(entry(message_id)).unwrap();

        let status = post(
            state_with(Arc::clone(&mailbox)).await,
            message_id,
            TARGET,
            true,
            "stale text from a previous attempt",
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let stored = mailbox.get(message_id).unwrap();
        assert_eq!(stored.acceptance.status, "accepted");
        assert!(stored.acceptance.error.is_empty());
        assert!(!stored.acceptance.at.is_empty());
    }

    #[tokio::test]
    async fn an_unknown_message_id_is_404() {
        let mailbox = Arc::new(IntentMailbox::in_memory());
        let status = post(
            state_with(Arc::clone(&mailbox)).await,
            "msg-never-sent",
            TARGET,
            true,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
/// `POST /api/intents/project-group/deleted` — the PM's delete-stamp push
/// (design 27 §4.4). The PM now retries these verdicts within a warm-up
/// window, so the three refusals are contract-relevant, not trivia: 401
/// means "wrong credential" (retrying cannot help), 400 means "malformed
/// body" (never heals), and 503 is exactly the cold-start case the retry
/// window exists to ride out. Pinning all three keeps the retry design
/// honest: a refactor that swaps 503 for a permanent code would silently
/// turn a warm-up miss into a permanently undeletable group chat.
#[cfg(test)]
mod project_group_deleted_route_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use tower::ServiceExt;

    fn state(user_ready: bool) -> AppState {
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-intent-deleted-{}",
            std::process::id()
        ));
        let mut gw = GatewayState::new(&dir.to_string_lossy());
        // A configured service token fails closed on callers that present
        // nothing — local no-token mode would skip the check entirely.
        gw.internal_service_token = Some("svc-token".to_string());
        gw.user_process = user_ready.then_some(crate::lifecycle::user_supervisor::UserProcessState {
            pid: 0,
            port: 1,
            ready: true,
        });
        AppState::new(
            std::sync::Arc::new(tokio::sync::RwLock::new(gw)),
            std::sync::Arc::new(HttpAuth::new(false)),
        )
    }

    async fn post(state: AppState, body: serde_json::Value, bearer: Option<&str>) -> StatusCode {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/intents/project-group/deleted")
            .header("content-type", "application/json");
        if let Some(t) = bearer {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        intent_routes()
            .with_state(state)
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn missing_credential_is_401() {
        let status = post(state(true), serde_json::json!({ "project_id": "p-1" }), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_project_id_is_400() {
        let status = post(
            state(true),
            serde_json::json!({ "project_id": "  " }),
            Some("svc-token"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn user_service_not_ready_is_503() {
        let status = post(
            state(false),
            serde_json::json!({ "project_id": "p-1" }),
            Some("svc-token"),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
}

/// ADR-106 revision 2026-11-04: the `@session` reference validation, pinned to
/// the same "no guessing, direct 400" discipline as the address rules it sits
/// beside. Every refusal here is a client bug caught **before the row is
/// written** — a shape that reaches the Runtime would be a durable row claiming
/// a coordinate nobody established.
#[cfg(test)]
mod mention_refs_tests {
    use super::*;

    fn r(agent: &str, session: &str) -> MentionRef {
        MentionRef {
            agent_instance_id: agent.to_string(),
            session_id: session.to_string(),
            round_id: None,
            terminal: None,
        }
    }

    fn check(
        target_sessions: &[(&str, &str)],
        force_new_for: &[&str],
        targets: &[&str],
        refs: Vec<MentionRef>,
    ) -> Result<(), ApiError> {
        let ts: std::collections::HashMap<String, String> = target_sessions
            .iter()
            .map(|(a, s)| (a.to_string(), s.to_string()))
            .collect();
        let fnf: Vec<String> = force_new_for.iter().map(|s| s.to_string()).collect();
        let tt: Vec<String> = targets.iter().map(|s| s.to_string()).collect();
        validate_mention_addresses(&ts, &fnf, &tt, &refs)
    }

    #[test]
    fn plain_refs_pass() {
        assert!(check(
            &[],
            &[],
            &["A"],
            vec![r("A", "20260101_aaa"), r("A", "20260102_bbb")],
        )
        .is_ok());
    }

    #[test]
    fn refs_beside_force_new_are_coherent() {
        // "new task" + "quote this round while you do it" — a reference never
        // decides where the sentence goes, so it is not a contradiction.
        assert!(check(&[], &["A"], &["A"], vec![r("A", "s-1")]).is_ok());
    }

    #[test]
    fn ref_naming_a_bystander_agent_is_400() {
        let e = check(&[], &[], &["A"], vec![r("B", "s-1")]).unwrap_err();
        assert_eq!(e.code, 400);
    }

    #[test]
    fn same_agent_delivered_and_quoted_is_400() {
        // Delivery (`target_sessions`, the card-answer channel) and material
        // for one agent in one sentence are contradictory instructions.
        let e = check(&[("A", "s-x")], &[], &["A"], vec![r("A", "s-1")]).unwrap_err();
        assert_eq!(e.code, 400);
    }

    #[test]
    fn blank_path_or_oversized_session_ids_are_400() {
        for bad in ["", "  ", "..", "a/b", "a b", &"s".repeat(200)] {
            let e = check(&[], &[], &["A"], vec![r("A", bad)]).unwrap_err();
            assert_eq!(e.code, 400, "session id {bad:?} must be refused");
        }
    }

    #[test]
    fn blank_round_echo_is_400() {
        let mut pinned = r("A", "s-1");
        pinned.round_id = Some("  ".to_string());
        let e = check(&[], &[], &["A"], vec![pinned]).unwrap_err();
        assert_eq!(e.code, 400);
    }

    #[test]
    fn pinned_echo_passes_through() {
        let mut pinned = r("A", "s-1");
        pinned.round_id = Some("r:m-7".to_string());
        pinned.terminal = Some(true);
        assert!(check(&[], &[], &["A"], vec![pinned]).is_ok());
    }

    #[test]
    fn card_answer_echo_with_delivery_is_accepted() {
        // C-1: answering a `confirm_continue` card re-delivers the human's own
        // sentence, which already quoted material. That echo (round_id present)
        // rides beside the answer's target_session for the SAME agent — it is a
        // fact of an already-written turn, not a fresh delivery-vs-quote intent.
        // The `same_agent_delivered_and_quoted_is_400` case above is a FRESH ref
        // (round_id None); the two arms must not be conflated.
        let mut echo = r("A", "s-1");
        echo.round_id = Some("r:m-7".to_string());
        echo.terminal = Some(true);
        assert!(check(&[("A", "s-x")], &[], &["A"], vec![echo]).is_ok());
    }

    #[test]
    fn duplicates_are_400() {
        let e = check(&[], &[], &["A"], vec![r("A", "s-1"), r("A", "s-1")]).unwrap_err();
        assert_eq!(e.code, 400);
        // Same session under two agents is two distinct references — legal.
        assert!(check(&[], &[], &["A", "B"], vec![r("A", "s-1"), r("B", "s-1")]).is_ok());
    }

    #[test]
    fn ref_count_is_clamped() {
        let refs: Vec<MentionRef> = (0..=MAX_CHAT_REFS)
            .map(|i| r("A", &format!("s-{i}")))
            .collect();
        let e = check(&[], &[], &["A"], refs).unwrap_err();
        assert_eq!(e.code, 400, "{MAX_CHAT_REFS}+1 refs must be refused");
    }

    #[test]
    fn refs_describe_the_pinned_wire_shape() {
        // The params contract the Runtime's `turn_refs` reads: the quadruple,
        // per-target filtered. A rename on this side without the reader's is a
        // silent "turn quotes nothing" — this test is the tripwire.
        let pinned = serde_json::json!({
            "agent_instance_id": "A",
            "session_id": "s-1",
            "round_id": "r:m-9",
            "terminal": true,
        });
        let params = chat_delivery_params(
            "hi",
            &[],
            &[],
            "user_group",
            None,
            DeliveryAddress {
                refs: std::slice::from_ref(&pinned),
                ..Default::default()
            },
        );
        assert_eq!(params[CHAT_REFS_PARAM].as_array().unwrap().len(), 1);
        assert_eq!(params[CHAT_REFS_PARAM][0]["round_id"], "r:m-9");
        // Empty refs write no key at all ("quotes nothing" is noise).
        let bare = chat_delivery_params(
            "hi",
            &[],
            &[],
            "user_group",
            None,
            DeliveryAddress::default(),
        );
        assert!(bare.get(CHAT_REFS_PARAM).is_none());
    }
}

/// ADR-106 revision · S-1 hardening: the scoped pinning read is now an
/// authorization gate for a pre-pinned echo too. A forged `{session_id, round_id}`
/// naming a session the caller cannot read (a different Chat on this host) must
/// be refused, while a legible echo keeps its own frozen coordinates and is
/// never re-resolved.
#[cfg(test)]
mod pin_reference_rounds_tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn echo(session: &str, round: &str) -> MentionRef {
        MentionRef {
            agent_instance_id: "agent-pin".to_string(),
            session_id: session.to_string(),
            round_id: Some(round.to_string()),
            terminal: Some(true),
        }
    }

    async fn state_with_stub() -> AppState {
        // A stub "Runtime": every session resolves to round r:m-9 EXCEPT one
        // whose path names `foreign`, which answers 404 (unreadable / other Chat).
        let runtime_app = axum::Router::new().route(
            "/sessions/{sid}/rounds",
            axum::routing::get(|uri: axum::http::Uri| async move {
                if uri.path().contains("foreign") {
                    return (axum::http::StatusCode::NOT_FOUND, "session not found").into_response();
                }
                axum::Json(serde_json::json!({
                    "session_id": "s-1",
                    "latest": { "round_id": "r:m-9", "terminal": true, "ts": "2026-01-01T00:00:00Z" },
                }))
                .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, runtime_app).await.unwrap() });

        let dir = std::env::temp_dir().join(format!(
            "acowork-test-pin-refs-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let mut state = crate::http::routes::AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        let registry = crate::http::proxy::new_shared_registry();
        registry
            .write()
            .await
            .register("agent-pin", &format!("http://127.0.0.1:{port}"));
        state.runtime_http_registry = Some(registry);
        state
    }

    #[tokio::test]
    async fn echo_passes_frozen_coordinates_through_the_gate() {
        // A readable echo is proven legible by the gate, then passed through
        // verbatim — the stub's own r:m-9 must NOT replace the client's r:m-7.
        let state = state_with_stub().await;
        let pinned = pin_reference_rounds(&state, &[echo("s-1", "r:m-7")], "u-alice", "chat-1")
            .await
            .expect("readable echo passes the gate");
        assert_eq!(pinned[0]["round_id"], "r:m-7", "echo is never re-resolved");
        assert_eq!(pinned[0]["terminal"], true);
    }

    #[tokio::test]
    async fn forged_echo_into_unreadable_session_is_400() {
        // S-1: before this change the echo arm read nothing, so a forged
        // coordinate into a session of another Chat was injected unchecked. The
        // gate read now runs for it too, and its 404 is the caller's 400.
        let state = state_with_stub().await;
        let e = pin_reference_rounds(
            &state,
            &[echo("foreign-s", "r:m-7")],
            "u-alice",
            "chat-1",
        )
        .await
        .expect_err("unreadable echo session must be refused");
        assert_eq!(e.code, 400);
    }
}
