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
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};
use crate::intent::chat_client;
use crate::intent::gate::{attribute_sender, permission_denial, validate_repo};
use crate::intent::delivery::{self, DeliveryContext};

pub fn intent_routes() -> Router<AppState> {
    Router::new()
        .route("/api/intents", post(deliver_intent))
        .route("/api/intents/mentions", post(deliver_mentions))
        .route("/api/intents/chat-history", get(chat_history_read))
        .route("/api/intents/project-group", post(ensure_project_group))
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
    if !token_presents_node(&state, &headers) {
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

/// `POST /api/intents/task-dispatch` — PM task dispatch as a group-chat
/// Intent (design 27 §6.2 / §9). The task is the execution-context
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
    /// Human-readable dispatch text (task title + description today).
    message: String,
    /// Project group chat session; ensured idempotently when absent.
    #[serde(default)]
    chat_session_id: Option<String>,
}

async fn dispatch_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TaskDispatchRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !token_presents_node(&state, &headers) {
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
    let conversation_id = format!("task:{}:{}", req.project_id, req.task_id);
    let message_id = uuid::Uuid::new_v4().to_string();
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

    // The intent_request chat row (§4: the chat log is the message record).
    let meta = serde_json::json!({
        "message_id": message_id,
        "conversation_id": conversation_id,
        "task_id": req.task_id,
        "project_id": req.project_id,
        "repo": repo,
    });
    if let Err(e) = chat_client::append_message(
        port,
        &chat_session_id,
        &chat_client::AppendMessage {
            from: req.owner.clone(),
            to: req.target_agent.clone(),
            kind: "intent_request".to_string(),
            body: req.message.clone(),
            meta,
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
            "project_id": req.project_id,
        })
        .to_string(),
        source_chat_session_id: chat_session_id.clone(),
        repo,
        kind: "request".to_string(),
        in_reply_to: String::new(),
        status: String::new(),
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
fn token_presents_node(state: &AppState, headers: &HeaderMap) -> bool {
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
    let gw = state.gateway_state.blocking_read();
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
    // them. Byte budget is approximate (whole rows dropped past the cap).
    const MAX_COUNT: usize = 50;
    const MAX_BYTES: usize = 32 * 1024;
    let count = q.count.clamp(1, MAX_COUNT);
    let offset = q.offset;
    let page = chat_client::read_messages(port, &q.session_id, &q.sender, offset, count)
        .await
        .map_err(|e| ApiError::unprocessable_entity(&e))?;
    let mut bytes = 0usize;
    let mut kept: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    if let Some(arr) = page.get("messages").and_then(|v| v.as_array()) {
        for m in arr {
            let len = serde_json::to_string(m).map(|s| s.len()).unwrap_or(0);
            if bytes + len > MAX_BYTES {
                truncated = true;
                break;
            }
            bytes += len;
            kept.push(m.clone());
        }
    }
    Ok(Json(serde_json::json!({
        "session_id": q.session_id,
        "offset": offset,
        "count": kept.len(),
        "truncated": truncated,
        "messages": kept,
    })))
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

/// A delivered / failed Intent as returned to the calling Runtime.
#[derive(Debug, Serialize)]
pub struct IntentDeliveryResponse {
    pub message_id: String,
    /// `delivered` | `failed` — the mailbox status at return time.
    pub status: String,
    /// The conversation this request belongs to (design 27 §6.5 R4):
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

/// Body of `POST /api/intents`. Field set follows design doc 27 §3.2
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
    /// Source-declared repo (design 27 §4.1). Format-validated here; the
    /// target-side authorized-directory boundary is the enforcement point.
    #[serde(default)]
    pub repo: Option<String>,
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
    if let Some(port) = user_port {
        if source_chat_session_id.is_empty() {
            match chat_client::ensure_pair(port, &req.sender, &req.target).await {
                Ok(s) => source_chat_session_id = s.session_id,
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
                    if !session.participants.iter().any(|p| p == &req.sender) {
                        return Err(ApiError::forbidden(
                            "intent sender is not a participant of the source chat session",
                        ));
                    }
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
        // an unlogged request.
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
                    kind: "intent_request".to_string(),
                    body: intent_body(&req),
                    meta,
                },
            )
            .await
            {
                tracing::warn!(
                    error = %e, session_id = %source_chat_session_id,
                    "intent_request chat row append failed — delivery refused"
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
        params_json: serde_json::to_string(&serde_json::Value::Object(req.params)).unwrap_or_default(),
        source_chat_session_id,
        repo,
        kind: "request".to_string(),
        in_reply_to: String::new(),
        status: String::new(),
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

/// `POST /api/intents/task-close` — design 27 §2.1 T2-a, PM-facing twin of
/// `/api/intents/task-dispatch`: a task reached a terminal state
/// (`Done` / `Rejected` / `Cancelled`), so the target agent's logical
/// execution context for that task ends. The Gateway does not know the
/// physical session id (agent-private state, ADR-009) — it forwards the
/// registry key to the target's Runtime, which closes and unmaps it.
/// Idempotent; an unreachable Runtime is reported, not retried (the next
/// dispatch on a dead mapping already self-heals, and T1/T2 rotation
/// bounds the residue).
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
    if !token_presents_node(&state, &headers) {
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
    // (the same `task:` prefix `dispatch_task` stamps).
    let key = format!("conv:task:{}:{}", req.project_id, req.task_id);
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
/// (design 27 §4: routing facts live in `meta`, the body is the message).
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

// ── Response write-back (design 27 §7.1) ──────────────────────────────

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

    let response_id = uuid::Uuid::new_v4().to_string();
    let status = req.status.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "completed".to_string());

    // §7.1/§13-12: close the request against the fallback sweeper the
    // moment the real answer is accepted, so a slow chat append or push
    // never races a `failed` receipt onto the same request.
    if let Some(mb) = mailbox.as_deref() {
        let _ = mb.mark_answered(&request_id);
    }

    // Chat row (best-effort like the request path's pair-ensure, but the
    // response IS the durable answer — a failed append on a bound session
    // is reported, delivery still proceeds because the initiator's turn
    // result is carried by the push below).
    let mut chat_append_error: Option<String> = None;
    if let (Some(port), session_id) = (user_port, entry.source_chat_session_id.clone())
        && !session_id.is_empty()
    {
        let meta = serde_json::json!({
            "message_id": response_id,
            "conversation_id": entry.conversation_id,
            "in_reply_to": request_id.clone(),
            "sender_session_id": req.sender_session_id.clone().unwrap_or_default(),
            "status": status,
        });
        if let Err(e) = chat_client::append_message(
            port,
            &session_id,
            &chat_client::AppendMessage {
                from: entry.to_instance.clone(),
                to: entry.from_instance.clone(),
                kind: "intent_response".to_string(),
                body: intent_body(&req),
                meta,
            },
        )
        .await
        {
            tracing::warn!(error = %e, session_id, "intent_response chat row append failed");
            chat_append_error = Some(e);
        }
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
    if entry.conversation_id.starts_with("task:") || !initiator_is_agent {
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
        params_json: serde_json::to_string(&serde_json::Value::Object(req.params))
            .unwrap_or_default(),
        source_chat_session_id: entry.source_chat_session_id.clone(),
        repo: String::new(),
        kind: "response".to_string(),
        in_reply_to: request_id,
        status,
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

// ── Group-chat mentions (design 27 §5) ────────────────────────────────

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
}

/// `POST /api/intents/mentions` — @Agent fan-out in a group chat (§5):
/// each mentioned agent gets its own `intent_request` chat row and its
/// own routed Intent; the session's repo binding is injected
/// server-side (§4.1). Ordinary (unmentioned) group messages never reach
/// this endpoint — they are archive-only by construction.
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
    if req.text.trim().is_empty() || req.targets.is_empty() {
        return Err(ApiError::bad_request("a mention needs text and at least one target"));
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
    if !session.participants.iter().any(|p| p == &req.from_user) {
        return Err(ApiError::forbidden("mention sender is not a participant"));
    }
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

    let conversation_id = match req.conversation_id.as_deref() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => uuid::Uuid::new_v4().to_string(),
    };
    let max_requests = intent_max_conversation_requests(&state).await;

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

        let message_id = uuid::Uuid::new_v4().to_string();
        if let Some(mb) = mailbox.as_deref()
            && mb.conversation_request_count(&conversation_id) >= max_requests
        {
            results.push(serde_json::json!({
                "target": target, "message_id": message_id, "status": "failed",
                "error_code": "conversation_limit_exceeded",
            }));
            continue;
        }

        // The intent_request row: from = the human, to = the agent.
        let meta = serde_json::json!({
            "message_id": message_id,
            "conversation_id": conversation_id,
            "repo": repo,
        });
        if let Err(e) = chat_client::append_message(
            port,
            &req.chat_session_id,
            &chat_client::AppendMessage {
                from: req.from_user.clone(),
                to: target.clone(),
                kind: "intent_request".to_string(),
                body: req.text.clone(),
                meta,
            },
        )
        .await
        {
            results.push(serde_json::json!({
                "target": target, "message_id": message_id, "status": "failed",
                "error_code": "chat_log_unavailable", "error": e,
            }));
            continue;
        }

        let dreq = delivery::DeliveryRequest {
            message_id: message_id.clone(),
            conversation_id: conversation_id.clone(),
            from_instance: req.from_user.clone(),
            from_session_id: String::new(),
            target_instance: target.clone(),
            action: "chat".to_string(),
            params_json: serde_json::json!({ "message": req.text }).to_string(),
            source_chat_session_id: req.chat_session_id.clone(),
            repo: repo.clone(),
            kind: "request".to_string(),
            in_reply_to: String::new(),
            status: String::new(),
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
        "conversation_id": conversation_id,
        "results": results,
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


