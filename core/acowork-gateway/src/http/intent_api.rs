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
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::gateway::state::AgentInfo;
use crate::http::routes::{ApiError, AppState};
use crate::intent::delivery::{self, DeliveryContext};

pub fn intent_routes() -> Router<AppState> {
    Router::new()
        .route("/api/intents", post(deliver_intent))
        .route("/api/intents/{message_id}", get(intent_status))
}

/// A delivered / failed Intent as returned to the calling Runtime.
#[derive(Debug, Serialize)]
pub struct IntentDeliveryResponse {
    pub message_id: String,
    /// `delivered` | `failed` — the mailbox status at return time.
    pub status: String,
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
    /// Source-side authorized repo (design 27 §3.2). Validated against
    /// the sender's own workspace before delivery (PR-D).
    #[serde(default)]
    pub repo: Option<String>,
}

async fn deliver_intent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IntentRequest>,
) -> Result<Json<IntentDeliveryResponse>, ApiError> {
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

    // --- PR-D: repo boundary (only when present) ------------------------
    let repo = match req.repo.as_deref() {
        Some(r) => {
            validate_repo(&sender_info, r).map_err(|e| ApiError::unprocessable_entity(&e))?;
            r.to_string()
        }
        None => String::new(),
    };

    // Everything delivery needs lives on AppState as Options (the MQTT
    // plane may still be starting); absence is `delivery_unavailable`.
    let gw = state.gateway_state.read().await;
    let mailbox = gw.intent_mailbox.clone();
    drop(gw);
    let (Some(mqtt), Some(registry)) =
        (state.mqtt_gateway_client.as_ref(), state.agent_registry.as_ref())
    else {
        return Ok(Json(IntentDeliveryResponse {
            message_id: String::new(),
            status: "failed".into(),
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
    let dreq = delivery::DeliveryRequest {
        message_id: message_id.clone(),
        conversation_id: req.conversation_id.clone().unwrap_or_default(),
        from_instance: req.sender.clone(),
        from_session_id: req.sender_session_id.clone().unwrap_or_default(),
        target_instance: req.target.clone(),
        action: req.action.clone(),
        params_json: serde_json::to_string(&serde_json::Value::Object(req.params)).unwrap_or_default(),
        source_chat_session_id: req.source_chat_session_id.clone().unwrap_or_default(),
        repo,
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
        error_code,
        error,
    }))
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

/// Pure attribution logic (D4), split out for testing:
/// `caller_node` = node resolved from the presented node token (None =
/// no token / unenrolled), `sender_status` = the retained AgentStatus
/// entry for the claimed sender (None = never seen online).
pub fn attribute_sender(
    caller_node: Option<String>,
    sender_status: Option<&crate::mqtt::agent_registry::AgentOnlineState>,
) -> Result<(), String> {
    match caller_node {
        Some(node_id) => match sender_status {
            Some(st) if st.node_id == node_id => Ok(()),
            Some(st) => Err(format!(
                "intent sender is hosted on node {}, not the caller's node",
                st.node_id
            )),
            None => Err("intent sender is not online — cannot attribute".to_string()),
        },
        // Local mode / unenrolled caller: require the sender to be an
        // online instance (an offline claim can't be session-verified).
        None => {
            if sender_status.is_some() {
                Ok(())
            } else {
                Err("intent sender is not online — cannot attribute".to_string())
            }
        }
    }
}

/// D8 static check against the two manifests. Returns Some(reason) when
/// the declared permissions do not cover this send.
pub fn permission_denial(sender: &AgentInfo, target: &AgentInfo) -> Option<String> {
    use acowork_core::Permission;

    let sender_ok = sender.manifest.permissions.iter().any(|p| match p {
        Permission::IntentSend(None) => true,
        Permission::IntentSend(Some(scope)) => {
            scope == &target.manifest.agent_id || scope == &target.instance_id
        }
        _ => false,
    });
    if !sender_ok {
        return Some(format!(
            "sender manifest lacks intent:send for target {}",
            target.instance_id
        ));
    }
    let target_ok = target.manifest.permissions.iter().any(|p| match p {
        Permission::IntentReceive(None) => true,
        Permission::IntentReceive(Some(scope)) => {
            scope == &sender.manifest.agent_id || scope == &sender.instance_id
        }
        _ => false,
    });
    if !target_ok {
        return Some(format!(
            "target manifest lacks intent:receive for sender {}",
            sender.instance_id
        ));
    }
    None
}

async fn intent_enforce_permissions(state: &AppState) -> bool {
    let gw = state.gateway_state.read().await;
    gw.config
        .as_ref()
        .map(|c| c.intent.enforce_permissions)
        .unwrap_or(false)
}

/// PR-D repo validation: the repo must be a git URL the sender's own
/// workspace is actually cloned from (the source side is the only
/// authority on what it can hand over). Design 27 §3.2: no Gateway-side
/// repo existence check, no branch field.
fn validate_repo(sender: &AgentInfo, repo: &str) -> Result<(), String> {
    if repo.trim().is_empty() || repo.len() > 512 {
        return Err("repo must be a non-empty git URL (<= 512 chars)".into());
    }
    let looks_like_url =
        repo.starts_with("https://") || repo.starts_with("git@") || repo.starts_with("ssh://");
    if !looks_like_url {
        return Err("repo must be an https/ssh git URL".into());
    }
    // Boundary: the sender's workspace must be a clone of this repo.
    // `install_path` is node-local (ADR-055) — reachable here only in
    // single-machine deployments; in split topologies the Gateway cannot
    // see the path, and the target-side boundary check at execution time
    // (design 27 §3.2) is the enforcement point there. An unreachable
    // workspace therefore passes with a debug note.
    let ws = std::path::Path::new(&sender.install_path);
    if ws.join(".git").exists() {
        match git_remote_url(ws) {
            Some(r) if normalize_git_url(&r) == normalize_git_url(repo) => Ok(()),
            Some(r) => Err(format!("repo does not match the sender workspace remote ({r})")),
            None => Err("sender workspace has no readable origin remote".into()),
        }
    } else {
        tracing::debug!(
            install_path = %sender.install_path,
            "repo check skipped: sender workspace not visible from the Gateway (node-local path)"
        );
        Ok(())
    }
}

fn git_remote_url(dir: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

fn normalize_git_url(u: &str) -> String {
    let mut s = u.trim().to_lowercase();
    if let Some(rest) = s.strip_prefix("https://") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("ssh://") {
        s = rest.trim_start_matches("git@").to_string();
    } else if let Some(rest) = s.strip_prefix("git@") {
        s = rest.replace(':', "/");
    }
    if let Some(rest) = s.strip_suffix(".git") {
        s = rest.to_string();
    }
    s
}


