//! Agent management HTTP API handlers
//!
//! Implements the Agent CRUD and lifecycle endpoints:
//! - GET    /api/agents           — list all agents with status
//! - GET    /api/agents/:id       — get agent detail
//! - GET    /api/agents/:id/avatar — reverse-proxied to the hosting node
//! - GET/PUT /api/agents/:id/avatar-config — reverse-proxied to the Runtime
//! - POST /api/agents/:id/manifest/{avatar,file} — reverse-proxied to the node
//! - POST   /api/agents/install  — install a .agent package
//! - POST   /api/agents/:id/clone — clone an agent (skeleton or full)
//! - DELETE /api/agents/:id       — uninstall an agent
//! - POST   /api/agents/:id/start — start an agent
//! - POST   /api/agents/:id/stop  — stop a running agent

use axum::{
    Json, Router,
    body::Body,
    extract::{Extension, Multipart, Path, Query, State},
    http::{HeaderMap, Response, StatusCode, header},
    routing::{get, patch, post},
};
use serde::{Deserialize, Serialize};

use crate::error::GatewayError;
use crate::gateway::ownership::{self, OwnerRecord, Visibility};
use crate::http::auth_middleware::AuthContext;
use crate::http::permission;
use crate::http::routes::{ApiError, AppState, OperationAck};
use crate::gateway::state::GatewayState;
use crate::mqtt::node_control::{NodeControlClient, NodeInstallDispatch, NodePackageSource};
use acowork_core::error_codes::StructuredErrorBody;
use acowork_core::operation::{OperationId, OperationRecord};
use acowork_core::AgentManifest;

/// Build the agent management router
pub fn agent_routes() -> Router<AppState> {
    Router::new()
        .route("/api/agents", get(list_agents))
        .route(
            "/api/agents/{id}",
            get(get_agent_detail).delete(uninstall_agent),
        )
        .route("/api/agents/install", post(install_agent))
        // User-driven interaction timestamp touch. See `record_interaction`.
        .route(
            "/api/agents/{id}/interactions",
            post(record_interaction),
        )
        // ADR-073: declarative "make sure this package is installed".
        // Idempotent by construction (unlike `/install`, which always
        // lands one more instance) — repeated callers converge on one
        // instance, and the caller never has to orchestrate that.
        .route("/api/agents/ensure", post(ensure_agent))
        // ADR-055 §3.2: package distribution source — remote Nodes pull
        // the uploaded `.agent` file from the Gateway's registry here.
        .route(
            "/api/packages/{agent_id}/download",
            get(download_package),
        )
        .route("/api/agents/{id}/clone", post(clone_agent))
        .route("/api/agents/{id}/upgrade", post(upgrade_agent))
        .route("/api/agents/{id}/start", post(start_agent))
        .route("/api/agents/{id}/stop", post(stop_agent))
        .route(
            "/api/agents/{id}/restart-debug",
            post(restart_agent_in_debug),
        )
        .route("/api/agents/{id}/model", get(get_agent_model))
        // ADR-087: ownership metadata endpoints.
        .route("/api/agents/{id}/permissions", get(get_agent_permissions))
        .route("/api/agents/{id}/visibility", patch(patch_agent_visibility))
        .route("/api/agents/{id}/guests", patch(patch_agent_guests))
        .route("/api/agents/{id}/owner", patch(patch_agent_owner))
        .route("/api/agents/{id}/claim", post(claim_agent))
        // ADR-034: `PUT /api/agents/{id}/config` is a pure reverse-proxy to
        // Runtime's `PUT /agents/{id}/config`.  The route itself is
        // registered in `proxy::proxy_routes` so all Runtime endpoints
        // stay co-located; this comment is the trace for code review.
        // The Gateway used to re-parse the body, forward only
        // `builtin_tools`, and echo the rest of the fields back — that
        // left per-agent fields like `temperature` / `max_output_tokens`
        // invisible to the Runtime (user-visible as "改动不生效").  All
        // persistence + live-broadcast now lives in the Runtime, so the
        // Gateway just forwards the body unchanged.
        // (intentionally NOT calling `put(...)` here — see proxy.rs.)
        //
        // Win11-MCP-ToolsBugFix (2026-07): the same ADR-034 pattern now also
        // covers `GET/PUT /api/agents/{id}/mcp-servers`. Previously these were
        // bespoke stubs in this module that returned 200 but never persisted
        // (`let _ = (..., resolved_servers)`), causing the user's Tools-panel
        // selection to silently disappear on the next tab remount. Routes
        // are now registered in `proxy::proxy_routes` — see comment there.
        // ADR-034: All Runtime endpoints live as pure reverse-proxy routes in
        // `proxy::proxy_routes`.  The routes that previously had bespoke
        // handlers here (`PUT /api/agents/{id}/config`,
        // `GET /api/agents/{id}/sessions/{session_id}/state`) used to
        // re-parse the body and re-emit a Gateway-side DTO — that was
        // the source of the "改动不生效" bug because the Gateway
        // dropped per-agent fields like `temperature` and
        // `max_output_tokens` instead of forwarding them to the Runtime.
        // ADR-009 §5: the avatar *config* endpoints are a pure reverse
        // proxy to Runtime's `GET/PUT /agents/{id}/avatar-config` (routes
        // live in `proxy::proxy_routes` with the other Runtime endpoints).
        // The Gateway used to own an `avatar_cache.json` for them and
        // mutate the in-memory manifest — a second writer of agent-private
        // data, and (worse) a writer whose value nothing ever pushed into
        // the Runtime, so a new avatar never survived an agent restart.
        // The Runtime now persists the pick to the instance's
        // `.overrides.json` (which also survives upgrades).
        // ADR-055 §6.7 (Phase 4): resolve the LSP relay endpoint of the
        // node hosting this agent, for Desktop code-editing features.
        .route(
            "/api/agents/{id}/lsp-endpoint",
            get(get_agent_lsp_endpoint),
        )
}

// ── Response types ────────────────────────────────────────────────────

/// ADR-085: short wire name for a lifecycle state in HTTP responses.
/// The Desktop gates on these strings (`lifecycleAtLeast` in
/// `agentStore`); keep in sync with `AgentLifecycleState` in
/// `proto/mqtt_payload.proto` and the Tauri `agent_status` event.
pub(crate) fn lifecycle_name(
    state: acowork_core::mqtt_proto::AgentLifecycleState,
) -> &'static str {
    use acowork_core::mqtt_proto::AgentLifecycleState as S;
    match state {
        S::Unspecified => "unspecified",
        S::Offline => "offline",
        S::Starting => "starting",
        S::HttpReady => "http_ready",
        S::SessionsReady => "sessions_ready",
        S::Failed => "failed",
    }
}

/// Agent list entry
#[derive(Serialize)]
pub struct AgentListResponse {
    /// ADR-073: instance identity (UUID v4, immutable, gateway-assigned
    /// on install). The primary key of every agent-scoped registry.
    pub instance_id: String,
    /// ADR-073: package identity (from manifest, immutable).
    pub agent_id: String,
    /// ADR-073: current location (node hosting this instance; mutable
    /// on migration). Positional metadata only — never a registry key.
    pub node_id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub avatar: Option<String>,
    /// Builtin avatar index declared in the manifest (e.g. "icon-05").
    /// Used as the default builtin avatar on first install when `avatar`
    /// (a packaged image path) is not set. The client normalises and
    /// validates this against its bundled icon set.
    pub builtin_avatar: Option<String>,
    pub version: String,
    /// ADR-104: package-declared labels (`manifest.toml` `labels`), lowercase
    /// kebab slugs. Retrieval/filtering metadata only — the client renders
    /// them as chips and may filter on them; nothing in the runtime keys off
    /// them. Empty for packages that declare none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Distributed liveness verdict: whether the Runtime's MQTT session is
    /// reachable at the broker level (payload `online`). Topology
    /// independent — the same answer for local, remote and node-hosted
    /// Runtimes, never a process/PID probe. The Desktop MUST gate
    /// "agent is alive" on this field.
    ///
    /// Auto-sleep was retired in Sept 2026; the only on/off transition
    /// is now stop / start.
    pub alive: bool,
    /// ADR-085: capability progression state — one of `"offline"`,
    /// `"starting"`, `"http_ready"`, `"sessions_ready"`, `"failed"`,
    /// `"unspecified"` (version-mismatch fallback, gates like offline).
    /// Replaces the old `ready: bool`; `alive == false` always reports
    /// `"offline"` (invariant 6).
    pub lifecycle: String,
    /// ADR-085: failure reason when lifecycle == "failed", else empty.
    pub lifecycle_detail: String,
    /// Whether the agent was started with the `--dev-mode` flag (Debug
    /// Protocol enabled at boot).
    ///
    /// ADR-048 follow-up: this is now **startup intent**, not current
    /// capability. To check whether DevMode is actually live for a
    /// running agent, read [`Self::debug_state`] instead. The Desktop
    /// uses `debug_state` to drive the Debug Panel + the "Enable Debug"
    /// button; `dev_mode` is preserved for backwards-compatible
    /// dashboards and operator scripts.
    pub dev_mode: bool,
    /// Whether DevMode is actually live for the running agent right now.
    ///
    /// ADR-048 follow-up: distinct from `dev_mode` so the Desktop can
    /// tell apart "agent was started in dev mode" from "DevMode was
    /// just enabled at runtime" (`POST /api/agents/{id}/debug/enable`).
    /// Serialised as the literal string `"enabled"` or `"disabled"` —
    /// matches the lower-case enum naming the TypeScript side uses for
    /// `AgentStore.dev_mode_state`.
    #[serde(rename = "debug_state")]
    pub debug_state: crate::gateway::state::DebugState,
    /// Debug Protocol port hint (set when dev_mode is true and agent is running).
    ///
    /// ADR-048: no longer bound by Runtime as a WebSocket listener; kept
    /// for API stability and operator dashboards that surface this field.
    pub debug_port: Option<u16>,
    /// RFC3339 timestamp of the last user-driven interaction with this agent
    /// (messages / approval / answer / compress).
    /// `None` for agents the user has never interacted with. Drives the
    /// sidebar sort order: newest first within each running/stopped group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_interaction_at: Option<String>,
    /// ADR-087 D7: whether the caller may manage this agent (owner /
    /// guest / public). `None` = local single-user mode (no auth, every
    /// resource is manageable — the client must treat `None` as true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_manage: Option<bool>,
    /// ADR-087 D7: whether the caller may use (chat with) this agent.
    /// The only permission bit the client actually gates on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_use: Option<bool>,
    /// ADR-087 D5: visibility — `"shared"` or `"private"`.
    pub visibility: String,
    /// ADR-087: the caller's user id is in this agent's guest list.
    pub is_guest: bool,
}

/// Agent detail response
#[derive(Serialize)]
pub struct AgentDetailResponse {
    /// ADR-073: instance identity (UUID v4, immutable).
    pub instance_id: String,
    /// ADR-073: package identity (from manifest).
    pub agent_id: String,
    /// ADR-073: current location (mutable on migration).
    pub node_id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub avatar: Option<String>,
    /// Builtin avatar index declared in the manifest (e.g. "icon-05").
    pub builtin_avatar: Option<String>,
    pub version: String,
    pub description: String,
    pub author: String,
    pub install_path: String,
    /// ADR-104: package-declared labels — same semantics as
    /// [`AgentListResponse::labels`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Distributed liveness verdict — same semantics as
    /// [`AgentListResponse::alive`]: the Runtime's MQTT session is
    /// reachable. Never a process/PID probe (see `alive` doc above).
    pub alive: bool,
    /// ADR-085: capability progression state — same vocabulary as
    /// [`AgentListResponse::lifecycle`].
    pub lifecycle: String,
    /// ADR-085: failure reason when lifecycle == "failed", else empty.
    pub lifecycle_detail: String,
    /// Local process id (diagnostic only — meaningful only when the
    /// Gateway spawned the Runtime on this machine; node-hosted
    /// Runtimes report `0`). NEVER used for liveness.
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    /// Whether the agent was started with the `--dev-mode` flag.
    ///
    /// ADR-048 follow-up: startup intent only. For current DevMode
    /// capability, see [`Self::debug_state`].
    pub dev_mode: bool,
    /// Whether DevMode is live right now (decoupled from `dev_mode` —
    /// can be enabled at runtime via `POST /api/agents/{id}/debug/enable`).
    #[serde(rename = "debug_state")]
    pub debug_state: crate::gateway::state::DebugState,
    /// Debug WebSocket port (set when dev_mode is true and agent is running)
    pub debug_port: Option<u16>,
    /// ADR-087 D7: caller may manage (see [`AgentListResponse::can_manage`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_manage: Option<bool>,
    /// ADR-087 D7: caller may use (chat with) this agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_use: Option<bool>,
    /// ADR-087 D5: `"shared"` | `"private"`.
    pub visibility: String,
    /// ADR-087: caller is in this agent's guest list.
    pub is_guest: bool,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

/// Agent model info response
#[derive(Serialize)]
pub struct AgentModelResponse {
    /// Provider name (e.g. "minimax", "openai")
    pub provider: String,
    /// Currently active model for this agent
    pub model: String,
    /// All available models for this provider
    pub available_models: Vec<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/agents` — list all installed agent instances.
///
/// ADR-073: every entry is an INSTANCE (one install action). A single
/// package may appear multiple times with distinct `instance_id`s.
/// Optional query filters:
/// - `?agent_id=com.foo`  → package view: all instances of that package
/// - `?node_id=node-a`    → all instances currently hosted on that node
///
/// Sort order (sidebar contract):
/// 1. Running agents come before stopped agents.
/// 2. Within each group, agents with `last_interaction_at` come first
///    sorted newest-first; agents that have never been interacted with
///    sink to the bottom of their group, ordered alphabetically by name.
#[derive(Debug, Deserialize)]
pub struct AgentListQuery {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub node_id: Option<String>,
}

pub async fn list_agents(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Query(query): Query<AgentListQuery>,
) -> Json<Vec<AgentListResponse>> {
    let gw = state.gateway_state.read().await;

    // Distributed liveness: the AgentRegistry (MQTT retained status +
    // LWT) is the single authoritative source for "is this agent alive".
    // Take one snapshot for the whole list so every entry sees the same
    // consistent view without per-agent lock contention. `online=true`
    // means the Runtime's MQTT session is reachable, which is the same
    // truth for local, remote and node-hosted Runtimes. No process/PID
    // probing happens here: PID liveness is meaningless across host
    // boundaries.
    let registry_snapshot: std::collections::HashMap<String, crate::mqtt::agent_registry::AgentOnlineState> =
        if let Some(ref reg) = state.agent_registry {
            reg.read().await.snapshot().into_iter().collect()
        } else {
            std::collections::HashMap::new()
        };

    let mut agents: Vec<AgentListResponse> = gw
        .installed_agents
        .values()
        .filter_map(|info| {
            // ADR-087 D5 + §4: the list must honor the SAME Node->Agent
            // view cascade every detail/use route enforces. The single
            // source of truth is `acl::evaluate`. Filtering here on the
            // agent's own row alone (the past `can_view(rec)` shortcut)
            // leaked agents whose hosting node the caller cannot view:
            // those rows appeared in the list yet 404/403 the moment
            // they were opened, and the mobile/desktop per-node grouping
            // had no node to file them under. Routing the read through
            // `evaluate` means the list and the detail can never drift,
            // and an agent is listed only when it AND its node are
            // viewable. FAIL-CLOSED on a missing row (D7: ownerless =>
            // admin-only) is preserved by `evaluate`'s own `ceiling`.
            // `auth == None` means Local mode (ADR-087 D8: the whole
            // authorization layer is a no-op) or a machine actor with no
            // `AuthContext` (permission module invariant 4). Neither is
            // subject to visibility filtering.
            let rec = agent_owner_rec(&state, &info.instance_id);
            if let Some(c) = auth.as_ref()
                && crate::http::acl::evaluate(
                    &state,
                    &c.0,
                    crate::http::acl::Op::View,
                    &crate::http::acl::Resource::Agent(info.instance_id.clone()),
                )
                .is_some()
            {
                return None;
            }
            let can_manage =
                Some(permission::caller_can_manage(auth.as_ref().map(|e| &e.0), rec.as_ref()));
            let can_use =
                Some(permission::caller_can_use(auth.as_ref().map(|e| &e.0), rec.as_ref()));
            let is_guest = ownership::is_guest(
                rec.as_ref(),
                auth.as_ref().map(|e| e.0.user_id.as_str()),
            );
            let visibility = match rec.as_ref().map(|r| r.visibility) {
                Some(Visibility::Private) => "private",
                _ => "shared",
            };
            // ADR-073: registries are keyed by INSTANCE identity. Cross-instance
            // reads are impossible by construction (each instance has
            // its own row in installed_agents and running_agents).
            let running_info = gw.running(&info.instance_id);
            let reg_state = registry_snapshot.get(&info.instance_id);
            // `alive` is the network-level liveness verdict (see
            // `AgentListResponse.alive` doc). It intentionally ignores
            // `running_agents` — that table only tracks process-managed
            // metadata (pid/ready/dev_mode) and is populated by MQTT
            // events anyway.
            let alive = reg_state.map(|s| s.online).unwrap_or(false);
            // ADR-085 invariant 6: `online == false` ⇒ OFFLINE — the
            // lifecycle mirrors the registry verdict, `running_agents`
            // only refines it while online.
            let lifecycle = lifecycle_name(if alive {
                running_info
                    .map(|r| r.lifecycle)
                    .unwrap_or(acowork_core::mqtt_proto::AgentLifecycleState::Offline)
            } else {
                acowork_core::mqtt_proto::AgentLifecycleState::Offline
            });
            // ADR-085: FAILED detail rides the list response so the
            // sidebar can surface the reason after a page refresh.
            let lifecycle_detail = if lifecycle == "failed" {
                running_info.map(|r| r.lifecycle_detail.clone()).unwrap_or_default()
            } else {
                String::new()
            };
            let last_interaction_at = gw
                .get_interaction(&info.instance_id)
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            // ADR-009 §5: user preference first, packaged default second.
            // Both halves are local now — the override came from the Node's
            // retained inventory (stopped agents) or from the Runtime's
            // avatar-config response (running ones), so listing never needs
            // a per-agent HTTP round trip.
            let overrides = gw.overrides_of(&info.instance_id);
            let (eff_avatar, eff_builtin, _) = match overrides {
                Some(ov) if ov.avatar.is_some() || ov.builtin_avatar.is_some() => {
                    (ov.avatar.clone(), ov.builtin_avatar.clone(), "overrides")
                }
                _ => resolve_avatar_from_manifest(&info.manifest),
            };
            let eff_display_name = overrides
                .and_then(|ov| ov.display_name.clone())
                .or_else(|| info.manifest.display_name.clone());
            let resp = AgentListResponse {
                instance_id: info.instance_id.clone(),
                agent_id: info.agent_id.clone(),
                node_id: info.node_id.clone(),
                name: info.name.clone(),
                display_name: eff_display_name,
                role: info.manifest.role.clone(),
                avatar: eff_avatar,
                builtin_avatar: eff_builtin,
                version: info.version.clone(),
                labels: info.manifest.labels.clone(),
                alive,
                lifecycle: lifecycle.to_string(),
                lifecycle_detail,
                dev_mode: running_info.map(|r| r.dev_mode).unwrap_or(false),
                debug_state: running_info
                    .map(|r| r.debug_state)
                    .unwrap_or(crate::gateway::state::DebugState::Disabled),
                debug_port: running_info.and_then(|r| r.debug_port),
                last_interaction_at,
                can_manage,
                can_use,
                visibility: visibility.to_string(),
                is_guest,
            };
            Some(resp)
        })
        .collect();
    // ADR-073: optional query filters (package view / node view).
    if query.agent_id.is_some() || query.node_id.is_some() {
        agents.retain(|a| {
            let pkg_ok = query
                .agent_id
                .as_deref()
                .is_none_or(|pkg| a.agent_id == pkg);
            let node_ok = query
                .node_id
                .as_deref()
                .is_none_or(|n| a.node_id == n);
            pkg_ok && node_ok
        });
    }
    // Diagnostic: if senior-engineer is present, log its state to help
    // trace why frontend polls may not see sessions_ready promptly.
    if let Some(sr) = gw.running("com.acowork.senior-engineer") {
        tracing::info!(
            "[DIAG] list_agents: senior-engineer running_agents entry present lifecycle={:?}",
            sr.lifecycle
        );
    }
    drop(gw);
    sort_agent_list(&mut agents);
    Json(agents)
}

/// Stable sidebar sort. See [`list_agents`] docstring for ordering rules.
fn sort_agent_list(agents: &mut [AgentListResponse]) {
    agents.sort_by(|a, b| {
        // 1) Alive group above dead group (alive = MQTT network signal,
        //    same contract as the sidebar's per-agent dot / name styling).
        if a.alive != b.alive {
            return if a.alive {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        // 2) Within a group: by last_interaction_at DESC; None last;
        //    fall back to name for stable, predictable ordering.
        match (&a.last_interaction_at, &b.last_interaction_at) {
            (Some(ta), Some(tb)) => tb.cmp(ta),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });
}

/// Ownership row for an agent instance, cloned out of the store so no
/// lock guard crosses an await point. `None` = no record (public).
fn agent_owner_rec(
    state: &AppState,
    instance_id: &str,
) -> Option<crate::gateway::ownership::OwnerRecord> {
    ownership::lock_shared(&state.agent_owners)
        .as_ref()
        .and_then(|store| store.get(instance_id).cloned())
}

/// `GET /api/agents/:id` — get agent detail
///
/// ADR-073: `:id` is the INSTANCE identity (UUIDv4).
pub async fn get_agent_detail(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentDetailResponse>, ApiError> {
    let user_id = auth.as_ref().map(|e| e.0.user_id.clone());
    let info = {
        let gw = state.gateway_state.read().await;
        gw.installed(&agent_id)
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?
            .clone()
    };
    // ADR-087 D5: a private agent invisible to the caller answers 404 —
    // the list view already filters it; the detail view must not leak
    // its existence either.
    // Shares `can_view` with the list so the two cannot drift again: this
    // used `is_none_or` (a MISSING row counted as visible) while the list
    // filtered on `is_some_and(Private)`, so an agent with no
    // `agent_owners.json` entry was listed for every account yet 404'd on
    // click — the "listed but permanently loading" report.
    let rec = agent_owner_rec(&state, &info.instance_id);
    let visible = match auth.as_ref() {
        // Local mode / machine actor — unfiltered (ADR-087 D8).
        None => true,
        Some(c) => ownership::can_view(rec.as_ref(), &c.0.user_id, c.0.is_admin()),
    };
    if !visible {
        return Err(ApiError::not_found(&format!("Agent not found: {}", agent_id)));
    }

    let gw = state.gateway_state.read().await;
    let running_info = gw.running(&agent_id);
    // Liveness = MQTT network signal (AgentRegistry), identical to the
    // list view. `running_agents` is process-managed metadata only —
    // never a liveness probe.
    let alive = if let Some(ref reg) = state.agent_registry {
        reg.read().await.is_online(&info.instance_id)
    } else {
        false
    };
    // ADR-085 invariant 6: `online == false` ⇒ OFFLINE.
    let lifecycle = lifecycle_name(if alive {
        running_info
            .map(|r| r.lifecycle)
            .unwrap_or(acowork_core::mqtt_proto::AgentLifecycleState::Offline)
    } else {
        acowork_core::mqtt_proto::AgentLifecycleState::Offline
    });
    // ADR-009 §5: same override-first resolution as `list_agents` — the
    // detail panel and the sidebar must not disagree about the name.
    let overrides = gw.overrides_of(&info.instance_id);
    let (eff_avatar, eff_builtin, _) = match overrides {
        Some(ov) if ov.avatar.is_some() || ov.builtin_avatar.is_some() => {
            (ov.avatar.clone(), ov.builtin_avatar.clone(), "overrides")
        }
        _ => resolve_avatar_from_manifest(&info.manifest),
    };
    let eff_display_name = overrides
        .and_then(|ov| ov.display_name.clone())
        .or_else(|| info.manifest.display_name.clone());
    let resp = AgentDetailResponse {
        instance_id: info.instance_id.clone(),
        agent_id: info.agent_id.clone(),
        node_id: info.node_id.clone(),
        name: info.name.clone(),
        display_name: eff_display_name,
        role: info.manifest.role.clone(),
        avatar: eff_avatar,
        builtin_avatar: eff_builtin,
        version: info.version.clone(),
        description: info.manifest.description.clone(),
        author: info.manifest.author.clone(),
        install_path: info.install_path.clone(),
        labels: info.manifest.labels.clone(),
        alive,
        lifecycle: lifecycle.to_string(),
        lifecycle_detail: if lifecycle == "failed" {
            running_info.map(|r| r.lifecycle_detail.clone()).unwrap_or_default()
        } else {
            String::new()
        },
        pid: running_info.map(|r| r.pid),
        started_at: running_info.map(|r| r.started_at.to_rfc3339()),
        dev_mode: running_info.map(|r| r.dev_mode).unwrap_or(false),
        debug_state: running_info
            .map(|r| r.debug_state)
            .unwrap_or(crate::gateway::state::DebugState::Disabled),
        debug_port: running_info.and_then(|r| r.debug_port),
        can_manage: Some(permission::caller_can_manage(auth.as_ref().map(|e| &e.0), rec.as_ref())),
        can_use: Some(permission::caller_can_use(auth.as_ref().map(|e| &e.0), rec.as_ref())),
        visibility: match rec.as_ref().map(|r| r.visibility) {
            Some(Visibility::Private) => "private",
            _ => "shared",
        }
        .to_string(),
        is_guest: ownership::is_guest(rec.as_ref(), user_id.as_deref()),
    };
    Ok(Json(resp))
}


// ── ADR-073: route variable → instance identity ────────────────

/// Resolve an agent's effective avatar from its manifest only.
///
/// Returns `(avatar, builtin_avatar, source)`. The per-instance
/// `.overrides.json` layer is applied by the callers.
pub(crate) fn resolve_avatar_from_manifest(
    manifest: &AgentManifest,
) -> (Option<String>, Option<String>, &'static str) {
    if manifest.avatar.is_some() || manifest.builtin_avatar.is_some() {
        return (manifest.avatar.clone(), manifest.builtin_avatar.clone(), "manifest");
    }
    (None, None, "fallback")
}

/// Resolve an HTTP route variable (`{id}`) to the canonical instance
/// identity + package identity pair.
///
/// ADR-073: the route variable is the INSTANCE identity (UUIDv4). A
/// package id never matches — callers must use the resolved instance id
/// from the list endpoint, not the package id.
pub(crate) async fn resolve_agent_identity(
    state: &AppState,
    id: &str,
) -> Result<(String, String), ApiError> {
    let gw = state.gateway_state.read().await;
    let inst = gw
        .resolve_installed_key(id)
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", id)))?;
    let aid = gw
        .installed(&inst)
        .map(|i| i.agent_id.clone())
        .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", id)))?;
    Ok((inst, aid))
}

/// Resolve the Gateway's own-machine node id from the NodeRegistry
/// (ADR-075 D5/D6) and fail LOUD when it cannot be resolved.
///
/// The `"local"` anchor is a state placeholder for Gateway-direct agents
/// (ADR-075 D6) — it is NOT a routable MQTT topic (no Node subscribes to
/// `acowork/nodes/local/...`), so a dispatch path must never fall back to
/// it: the command would be published into a topic nobody subscribes to
/// (command timeout + Desktop retry loop). ADR-075 Q2: "install 等默认
/// 落点不得静默落到任一远程节点".
pub(crate) async fn resolve_local_node_id(state: &AppState) -> Result<String, ApiError> {
    let resolved = match state.node_registry.as_ref() {
        Some(reg) => crate::mqtt::node_registry::local_node_id(reg).await,
        None => None,
    };
    resolved.ok_or_else(|| {
        ApiError::conflict(
            "the Gateway's own-machine node is not online — cannot resolve the local node id \
             (no `gateway_managed` node in the node registry); retry once GET /api/bootstrap \
             reports READY, or pass an explicit node_id",
        )
    })
}

/// Resolve the node hosting an agent instance: the install/running record
/// first (ADR-055 §6.5), else the Gateway's own-machine node. Fails loud
/// when neither can be resolved — see [`resolve_local_node_id`].
pub(crate) async fn resolve_agent_node_id(
    state: &AppState,
    instance_id: &str,
) -> Result<String, ApiError> {
    let resolved = {
        let gw = state.gateway_state.read().await;
        gw.installed_agents
            .get(instance_id)
            .map(|i| i.node_id.clone())
            .or_else(|| gw.running_agents.get(instance_id).map(|r| r.node_id.clone()))
    };
    if let Some(node_id) = resolved {
        return Ok(node_id);
    }
    resolve_local_node_id(state).await
}

/// `GET /api/packages/{agent_id}/download` — serve the uploaded `.agent`
/// source file from the package registry (ADR-055 §3.2). This is how a
/// remote Node pulls the package during an asynchronous install.
///
/// ADR-055 Phase 5a: when `mqtt.auth_enabled` is on, the Node must
/// present its long-lived token as `X-ACowork-Node-Token` (any
/// registered node token passes — the registry is node-agnostic at
/// this tier). 401 without the header, 403 on a mismatch.
pub async fn download_package(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response<Body>, ApiError> {
    // agent_id is a slug; reject path separators / traversal.
    if agent_id.is_empty()
        || agent_id.contains('/')
        || agent_id.contains('\\')
        || agent_id.contains("..")
    {
        return Err(ApiError::bad_request("Invalid agent id"));
    }

    // ADR-055 Phase 5a: node-token gate on the package channel.
    let broker_auth = state.gateway_state.read().await.mqtt_broker_auth.clone();
    if let Some(auth) = broker_auth.filter(|a| a.auth_enabled) {
        let provided = headers
            .get("X-ACowork-Node-Token")
            .and_then(|v| v.to_str().ok());
        let Some(token) = provided else {
            return Err(ApiError::unauthorized(
                "Node token required (X-ACowork-Node-Token)",
            ));
        };
        let valid = auth
            .node_tokens
            .lock()
            .map(|store| store.any_token_matches(token))
            .unwrap_or(false);
        if !valid {
            return Err(ApiError {
                error: "Invalid node token".to_string(),
                code: 403,
                structured: None,
            });
        }
    }

    let registry_dir = {
        let gw = state.gateway_state.read().await;
        gw.config
            .as_ref()
            .map(|c| c.package_registry_dir())
            .ok_or_else(|| ApiError::internal("Gateway config unavailable"))?
    };

    let path = registry_dir.join(format!("{}.agent", agent_id));
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| ApiError::not_found(&format!("Package '{}' not in registry: {}", agent_id, e)))?;

    let body = Body::from(bytes);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}.agent\"", agent_id),
        )
        .body(body)
        .map_err(|e| ApiError::internal(&format!("Failed to build download response: {}", e)))
}

/// Read the Gateway's `dev_mode` flag (default `false` = strict
/// signature verification on the node, ADR-055 §6.20).
pub(crate) async fn gateway_dev_mode(state: &AppState) -> bool {
    state
        .gateway_state
        .read()
        .await
        .config
        .as_ref()
        .map(|c| c.dev_mode)
        .unwrap_or(false)
}

/// Enforce ADR-055 §6.9 version negotiation before issuing a control
/// command: the target node's reported protocol version must be at least
/// [`acowork_core::node::NODE_MIN_SUPPORTED_PROTOCOL_VERSION`].
///
/// Unknown-node / missing-info cases are NOT gated here — they are left
/// to the command round-trip (which surfaces a timeout / offline error),
/// because the version gate should only reject nodes whose protocol is
/// known to be too old, not nodes that haven't reported yet.
pub(crate) async fn check_node_compatible(
    state: &AppState,
    node_id: &str,
) -> Result<(), ApiError> {
    let Some(registry) = state.node_registry.as_ref() else {
        // No node registry (MQTT disabled) — the command path fails later
        // with a clearer "control plane unavailable" error.
        return Ok(());
    };
    let reported = {
        let reg = registry.read().await;
        reg.get(node_id)
            .and_then(|n| n.info.as_ref())
            .map(|i| i.protocol_version)
    };
    let Some(proto) = reported else {
        return Ok(());
    };
    if !acowork_core::node::is_node_protocol_compatible(proto) {
        return Err(ApiError::bad_request(&format!(
            "Node '{}' protocol version {} is incompatible (minimum supported {})",
            node_id,
            proto,
            acowork_core::node::NODE_MIN_SUPPORTED_PROTOCOL_VERSION
        )));
    }
    Ok(())
}

/// Response for `GET /api/agents/{id}/lsp-endpoint`.
#[derive(Debug, Serialize)]
pub struct AgentLspEndpointResponse {
    pub agent_id: String,
    /// Node hosting the agent (ADR-055 §6.5); `"local"` for agents the
    /// Gateway spawns directly.
    pub node_id: String,
    /// Gateway LSP relay **proxy** URL for this node
    /// (`http://{advertise_host}:{http_port}/api/nodes/{node_id}/lsp`,
    /// ADR-089 D3) — clients never address the node's relay directly:
    /// the relay binds loopback on its node (D1), so the node-local
    /// address the registry carries would be unreachable from any
    /// other machine. `None` when the relay is not usable, with
    /// [`Self::reason`] naming why.
    pub endpoint: Option<String>,
    /// Whether the node's LSP relay is ready (and the node online).
    pub ready: bool,
    /// ADR-089 D4: why `endpoint` is `None` — `node_offline` /
    /// `relay_not_ready` / `no_lsp_sidecar`. `None` when ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ─── ADR-087: ownership metadata endpoints ──────────────────────────────
//
// The permission middleware (`http/permission.rs`) enforces the tier for
// these routes before the handler runs (visibility/guests → AgentManage,
// owner → AgentTransfer); handlers only validate input and mutate the
// record. In Local mode the middleware is a no-op and ownership edits
// remain available to the single operator (D8).

/// `GET /api/agents/{id}/permissions` — current attribution + guest list.
///
/// Read side of the three PATCH endpoints, gated at `AgentManage` by the
/// permission middleware (classify's default-deny already lands GETs on
/// unregistered segments there — a guest can see WHO manages the agent,
/// attribution writes additionally require owner ∨ admin, D9 R1). The
/// Desktop permission dialog renders from this; `can_attribute` tells
/// the caller whether visibility/guests PATCHes will be accepted.
async fn get_agent_permissions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: Option<Extension<AuthContext>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let instance_id = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(&id)
    }
    .ok_or_else(|| ApiError::not_found(&format!("no such agent instance: {id}")))?;
    let store = ownership::lock_shared(&state.agent_owners)
        .ok_or_else(|| ApiError::internal("ownership store unavailable"))?;
    let rec = store.get(&instance_id);
    let local = auth.is_none();
    let user_id = auth.as_ref().map(|a| a.0.user_id.clone()).unwrap_or_default();
    let is_admin = auth.as_ref().is_some_and(|a| a.0.is_admin());
    let can_attribute = local
        || is_admin
        || rec.and_then(|r| r.owner_user_id.as_deref()) == Some(user_id.as_str());
    Ok(Json(serde_json::json!({
        "owner_user_id": rec.and_then(|r| r.owner_user_id.clone()),
        "guests": rec.map(|r| r.guests.clone()).unwrap_or_default(),
        "visibility": rec
            .map(|r| r.visibility.as_str())
            .unwrap_or(if local { "shared" } else { "private" }),
        "can_attribute": can_attribute,
        // ADR-087 D6/D7 + D8: the client must not have to *discover*
        // that an ownerless row cannot be published by flipping the
        // switch and reading a 409. `can_attribute` answers "may you
        // write attribution at all" (an admin passes it), so on its own
        // it would enable the switch for a row that can only be
        // rejected. Ship the data-state answer alongside it.
        "can_set_visibility": ownership::can_publish(rec, local),
        "ownerless": ownership::is_ownerless(rec),
    })))
}

/// `PATCH /api/agents/{id}/visibility` — `{ "visibility": "shared"|"private" }`.
/// Agents have exactly these two settings (ADR-087 D6: `public` is a
/// node-only concept). The onboarding default agent starts `Shared` but
/// its owner may flip it back to `Private` (D3 — "owner 仍可改回 private").
///
/// **Ownerless guard.** `OwnershipStore::upsert_with` normalises the
/// `ownerless ⇒ not Shared` data-hygiene invariant *after* the caller's
/// mutation runs, so flipping an ownerless agent to `shared` would be
/// silently downgraded back to `private` while this handler answered
/// 200 with the requested value — the Desktop switch then snapped back
/// on its post-save reload, with no error anywhere. Rejecting the write
/// here turns that silent no-op into a diagnosable 409 whose message
/// names the two-call recovery (claim an owner, then set visibility).
///
/// The invariant itself is a data rule, not an authorization one:
/// `ownership::can_view` is `is_admin ∨ can_use ∨ shared`, so an
/// ownerless agent cannot be shared into reach by any other route.
async fn patch_agent_visibility(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let vs = body
        .get("visibility")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::bad_request("missing `visibility`"))?;
    let vis = match vs {
        "shared" => Visibility::Shared,
        "private" => Visibility::Private,
        _ => return Err(ApiError::bad_request("visibility must be `shared` or `private`")),
    };
    // Read the row before editing so the guard can distinguish
    // "ownerless" from "owned but private" — only the former conflicts.
    let instance_id = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(&id)
    }
    .ok_or_else(|| ApiError::not_found(&format!("no such agent instance: {id}")))?;
    if vis == Visibility::Shared {
        let store = ownership::lock_shared(&state.agent_owners)
            .ok_or_else(|| ApiError::internal("ownership store unavailable"))?;
        if ownership::is_ownerless(store.get(&instance_id)) {
            // Reached only by a caller that skipped the
            // `can_set_visibility` bit the permissions endpoint
            // ships (ADR-087 D8) — the Desktop disables the switch in
            // that case. Kept as a 409 so a non-UI client cannot turn
            // the store's silent normalisation into a lie.
            return Err(ApiError::conflict(
                "this agent has no owner yet, so it cannot be shared. \
                 Claim ownership first, then set visibility.",
            ));
        }
    }
    edit_agent_owner(&state, &id, |rec| rec.visibility = vis).await?;
    Ok(Json(serde_json::json!({ "visibility": vs })))
}

/// `PATCH /api/agents/{id}/guests` — `{ "guests": [user_id, ...] }`,
/// full-list replacement (ADR-087 D9: PUT-list semantics).
async fn patch_agent_guests(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let list = body
        .get("guests")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ApiError::bad_request("missing `guests` array"))?;
    let guests: Vec<String> = list
        .iter()
        .filter_map(|v| v.as_str().map(|x| x.to_string()))
        .collect();
    if guests.len() != list.len() {
        return Err(ApiError::bad_request("guests must be user-id strings"));
    }
    edit_agent_owner(&state, &id, |rec| rec.guests = guests.clone())
        .await?;
    Ok(Json(serde_json::json!({ "guests": guests })))
}

/// `PATCH /api/agents/{id}/owner` — `{ "owner_user_id": "..." }` transfer
/// or claim. Admin only (ADR-087 D7: transfer is an authorization change;
/// the owner may share but never hand over mastership). `null` / empty
/// string clears the owner → ownerless (fail-closed). On a resource with
/// no ownership row this creates one — the D7 admin-claim path.
async fn patch_agent_owner(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let new_owner = match body.get("owner_user_id") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) if s.is_empty() => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => {
            return Err(ApiError::bad_request(
                "missing `owner_user_id` (string or null)",
            ))
        }
    };
    edit_agent_owner(&state, &id, |rec| rec.owner_user_id = new_owner.clone())
        .await?;
    Ok(Json(serde_json::json!({ "owner_user_id": new_owner })))
}

/// `POST /api/agents/{id}/claim` — take ownership of an ownerless agent
/// instance (ADR-087 D7, revised). Mirrors `POST /api/nodes/{id}/claim`:
/// - only ownerless instances are claimable — already-owned answers 409;
/// - admins may claim any ownerless instance; any other logged-in user
///   may claim only instances hosted on the Gateway's own machine;
/// - no `AuthContext` (local mode / machine actor): 400.
async fn claim_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: Option<Extension<AuthContext>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ctx = auth
        .map(|e| e.0)
        .ok_or_else(|| ApiError::bad_request("claim requires a logged-in user"))?;
    // Existence gate: `{id}` must resolve to a live inventory instance
    // (same normalization as `edit_agent_owner` — package-id aliases
    // cannot create ghost rows).
    let instance_id = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(&id)
    }
    .ok_or_else(|| ApiError::not_found(&format!("no such agent instance: {id}")))?;
    if !ctx.is_admin() {
        // Hosting node: the denormalized row first, else the live
        // inventory. Awaited before the store lock so no guard crosses
        // an await point.
        let hosting = {
            let rec_node = ownership::lock_shared(&state.agent_owners)
                .and_then(|s| s.get(&instance_id).and_then(|r| r.node_id.clone()));
            match rec_node {
                Some(n) => Some(n),
                None => {
                    let gw = state.gateway_state.read().await;
                    gw.installed(&instance_id).map(|i| i.node_id.clone())
                }
            }
        };
        let local = match state.node_registry.as_ref() {
            Some(reg) => crate::mqtt::node_registry::local_node_id(reg).await,
            None => None,
        };
        if hosting.is_none() || hosting != local {
            return Err(ApiError::forbidden(
                "only admins may claim agents on remote nodes; \
                 other users may claim agents hosted on the local node",
            ));
        }
    }
    let mut store = ownership::lock_shared(&state.agent_owners)
        .ok_or_else(|| ApiError::internal("ownership store unavailable"))?;
    if store
        .get(&instance_id)
        .and_then(|r| r.owner_user_id.as_ref())
        .is_some()
    {
        return Err(ApiError::conflict("agent already has an owner"));
    }
    let owner = ctx.user_id.clone();
    store.upsert_with(
        &instance_id,
        || OwnerRecord::new(Some(owner.clone())),
        |rec| {
            if rec.owner_user_id.is_none() {
                rec.owner_user_id = Some(owner.clone());
            }
        },
    );
    tracing::info!(instance_id = %instance_id, owner = %ctx.user_id, "ADR-087: ownerless agent claimed");
    Ok(Json(serde_json::json!({ "owner_user_id": ctx.user_id })))
}

/// Load an agent's ownership row, apply `f`, persist. The `{id}` path
/// variable is normalized to the canonical instance key (ADR-073) so
/// package-id aliases cannot create ghost rows. When the instance has
/// no row yet (legacy installs predate ADR-087), a default ownerless
/// row is created and `f` applied — this is the admin-claim path of D7
/// (the middleware has already restricted this tier; the instance must
/// exist in the inventory).
async fn edit_agent_owner<F: FnOnce(&mut OwnerRecord)>(
    state: &AppState,
    raw_id: &str,
    f: F,
) -> Result<(), ApiError> {
    let instance_id = {
        let gw = state.gateway_state.read().await;
        gw.resolve_installed_key(raw_id)
    }
    .ok_or_else(|| ApiError::not_found(&format!("no such agent instance: {raw_id}")))?;
    let mut store = ownership::lock_shared(&state.agent_owners)
        .ok_or_else(|| ApiError::internal("ownership store unavailable"))?;
    store.upsert_with(&instance_id, || OwnerRecord::new(None), f);
    Ok(())
}

/// `GET /api/agents/{id}/lsp-endpoint` — resolve the LSP relay endpoint
/// of the node hosting this agent.
///
/// ADR-055 §6.7 (Phase 4): the LSP relay is no longer a Gateway child
/// process — each node runs its own relay sidecar and publishes a
/// retained `acowork/nodes/{node_id}/lsps` envelope, which the Gateway
/// mirrors into [`crate::mqtt::node_registry::NodeInfoState::lsp_endpoint`].
///
/// Node resolution order: running agent's host first, installed-agent
/// inventory as fallback (agent stopped).
pub async fn get_agent_lsp_endpoint(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentLspEndpointResponse>, ApiError> {
    let node_id = {
        let gw = state.gateway_state.read().await;
        gw.running_agents
            .get(&agent_id)
            .map(|r| r.node_id.clone())
            .or_else(|| gw.installed_agents.get(&agent_id).map(|i| i.node_id.clone()))
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?
    };

    let (endpoint, ready, reason) = {
        let node = match state.node_registry.as_ref() {
            Some(registry) => registry.read().await.get(&node_id).cloned(),
            None => None,
        };
        match node {
            // Node unknown to the registry (never enrolled) or offline:
            // the proxy could not forward anyway (ADR-089 D4 gating —
            // `ready` no longer rides on the retained endpoint alone,
            // which stays published after an LWT offline flip).
            None => (
                None,
                false,
                Some("no_lsp_sidecar".to_string()),
            ),
            Some(n) if !n.online => (None, false, Some("node_offline".to_string())),
            // The retained `lsp_endpoint` is the relay's node-local
            // loopback address (ADR-089 D1) — never hand it to a
            // remote client. Assemble the Gateway proxy URL instead.
            Some(n) if n.lsp_endpoint.is_some() => {
                let gw = state.gateway_state.read().await;
                (
                    Some(format!(
                        "http://{}:{}/api/nodes/{}/lsp",
                        gw.advertise_host, gw.http_port, node_id
                    )),
                    true,
                    None,
                )
            }
            Some(_) => (None, false, Some("relay_not_ready".to_string())),
        }
    };

    Ok(Json(AgentLspEndpointResponse {
        agent_id,
        node_id,
        endpoint,
        ready,
        reason,
    }))
}

/// `POST /api/agents/install` — upload and install a .agent package.
///
/// ADR-055 §3.2: asynchronous install. The Gateway spools the upload
/// into its package registry, then issues an `install` command carrying
/// the registry download URL to the target node and returns `202
/// Accepted` immediately. The node downloads, verifies, and installs the
/// package, then publishes a retained `installed` inventory entry — the
/// Gateway aggregates that (installed table + cron triggers) on receipt.
///
/// The Gateway's own `dev_mode` flag is forwarded to the node to select
/// signature strictness (ADR-055 §6.20).
/// Fields shared by `POST /api/agents/install` and
/// `POST /api/agents/ensure`.
struct AgentUploadForm {
    package_bytes: Vec<u8>,
    node_id: String,
    /// ADR-059 §7.3 precondition — only meaningful for the explicit
    /// install path; a declarative ensure has no version precondition to
    /// violate (it does not depend on the caller's view).
    expected_version: Option<u64>,
}

/// Read the install/ensure multipart body.
async fn read_agent_upload(
    state: &AppState,
    mut multipart: Multipart,
) -> Result<AgentUploadForm, ApiError> {
    let mut package_bytes: Option<Vec<u8>> = None;
    let mut node_id: Option<String> = None;
    // ADR-059 §7.3: optional optimistic-concurrency precondition — the
    // BootstrapState `version` the client read before submitting.
    let mut expected_version: Option<u64> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(&format!("Failed to read multipart field: {}", e)))?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "package" => {
                let bytes = field.bytes().await.map_err(|e| {
                    ApiError::bad_request(&format!("Failed to read package field: {}", e))
                })?;
                package_bytes = Some(bytes.to_vec());
            }
            // ADR-055 §3.3: optional target node (defaults to `local`).
            "node_id" => {
                let text = field.text().await.unwrap_or_default();
                if !text.is_empty() {
                    node_id = Some(text);
                }
            }
            "expected_version" => {
                let text = field.text().await.unwrap_or_default();
                if !text.is_empty() {
                    expected_version = text.trim().parse::<u64>().ok();
                }
            }
            _ => {}
        }
    }

    let package_bytes =
        package_bytes.ok_or_else(|| ApiError::bad_request("Missing required field: 'package'"))?;
    if package_bytes.is_empty() {
        return Err(ApiError::bad_request("Package file is empty"));
    }

    Ok(AgentUploadForm {
        package_bytes,
        node_id: match node_id {
            Some(id) => id,
            // ADR-075 D5/Q2: no node_id in the form → resolve the local
            // node's UUID from the registry; loud failure when it is not
            // online (never the non-routable "local" anchor).
            None => resolve_local_node_id(state).await?,
        },
        expected_version,
    })
}

/// The node control-plane client, or a loud failure when MQTT is off.
fn node_control_client(state: &AppState) -> Result<NodeControlClient, ApiError> {
    state
        .node_control
        .clone()
        .ok_or_else(|| ApiError::internal("Node control plane unavailable (MQTT disabled)"))
}

/// ADR-059 §2.3: the install/ensure command is a (fire-and-forget) MQTT
/// publish — if the target Node's control plane has not announced
/// `NodeReady`, the command would be silently dropped. Reject with 409
/// `dependency_not_ready` (structured error, Phase 3.2) so the caller
/// retries once `GET /api/bootstrap` reaches READY. This covers BOTH
/// "never enrolled" and "enrolled but not ready": the node publishes its
/// enroll request before NodeReady (bootstrap order, ADR-059 §7.2), so
/// `node.{id}` cannot be ready while the node is unknown to the
/// registry.
async fn ensure_node_ready(state: &AppState, node_id: &str) -> Result<(), ApiError> {
    let Some(registry) = state.bootstrap_registry.as_ref() else {
        // No bootstrap registry (MQTT disabled) — the dispatch path fails
        // later with a clearer error.
        return Ok(());
    };
    if registry.is_ready(&crate::bootstrap::SubsystemId(format!("node.{}", node_id))) {
        return Ok(());
    }
    let (phase, phase_detail) = state
        .gateway_state
        .read()
        .await
        .bootstrap
        .orchestrator
        .as_ref()
        .map(|o| {
            let s = o.snapshot();
            // SCREAMING_SNAKE_CASE serde name — the same string the HTTP
            // projection uses (`BOOTING`, `READY`, …).
            let phase = serde_json::to_string(&s.phase)
                .map(|p| p.trim_matches('"').to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            (phase, s.phase_detail)
        })
        .unwrap_or_else(|| ("unknown".to_string(), "orchestrator unavailable".to_string()));
    Err(ApiError::conflict_structured(
        StructuredErrorBody::dependency_not_ready(Some(phase), Some(phase_detail), 500),
    ))
}

/// Where a declarative ensure request resolves to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnsureResolution {
    /// An instance of this package is already on the target node — by
    /// ADR-073 instance identity.
    Present(String),
    /// Nothing known locally; the request must be dispatched to the node
    /// (which re-checks the same precondition before touching the disk).
    Dispatch,
}

/// Resolve a declarative ensure against the Gateway's view of the target
/// node's inventory.
///
/// The Gateway's view lags the node (it is fed by retained inventory
/// messages), so this is a fast path and never the authority: a miss here
/// only means "ask the node".
pub(crate) fn resolve_ensure(
    gw: &GatewayState,
    agent_id: &str,
    node_id: &str,
) -> EnsureResolution {
    let mut matches = gw
        .installed_agents
        .values()
        .filter(|info| info.agent_id == agent_id && info.node_id == node_id)
        .map(|info| info.instance_id.clone())
        .collect::<Vec<_>>();
    // Stable pick when one package has several instances on one node: any
    // of them satisfies an existence claim, but the answer must not
    // change between calls.
    matches.sort();
    match matches.into_iter().next() {
        Some(instance_id) => EnsureResolution::Present(instance_id),
        None => EnsureResolution::Dispatch,
    }
}

/// Response for `POST /api/agents/ensure` (ADR-073 declarative install).
#[derive(Debug, Serialize)]
pub struct EnsureAck {
    /// Package whose presence was ensured.
    pub agent_id: String,
    /// Instance this request resolves to. On the already-present path it
    /// is the instance the node holds; on the dispatch path it is the
    /// candidate the Gateway minted — the node may still satisfy the
    /// request from an instance the Gateway has not seen yet.
    pub instance_id: String,
    /// `true` when the package was already present: no install was
    /// dispatched and this request creates nothing.
    pub already_present: bool,
    /// ADR-059 §6 tracking id — present only when an install was
    /// dispatched. The node's terminal event completes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}

/// `POST /api/agents/ensure` — declarative "make sure this package is
/// installed on this node".
///
/// The difference from `/install` is *intent*, and it is the whole point
/// of this endpoint: `/install` always lands one more instance (ADR-073
/// multi-instance is legal, and stays legal), while `/ensure` answers
/// "is it there?" and creates at most one instance no matter how many
/// callers ask. It is idempotent by construction, so a caller may retry
/// freely — serialization and the existence check live on the node
/// (`InstallKind::Ensure` + the install gate), never in the caller.
///
/// Returns `200 OK` with `already_present = true` when the package is
/// already on the target node (nothing published), or `202 Accepted` with
/// an operation id when an install was dispatched.
pub async fn ensure_agent(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    multipart: Multipart,
) -> Result<(StatusCode, Json<EnsureAck>), ApiError> {
    let form = read_agent_upload(&state, multipart).await?;
    let node_id = form.node_id;

    // ADR-087 D4: ensure is an install-intent on the target node →
    // Node-manage, on both the fast and the dispatch path.
    permission::check_node_manage(
        &state,
        auth.as_ref().map(|e| &e.0),
        &node_id,
    )?;

    // The package must be opened to learn its `agent_id`; the staged temp
    // file is deleted when this scope ends, whichever way it ends.
    let staged = StagedPackage::stage(&form.package_bytes)?;
    let agent_id = staged.manifest.agent_id.clone();

    // Fast path: the package is already on the node, so there is nothing
    // to dispatch — and no reason to require the node to be ready.
    let present = {
        let gw = state.gateway_state.read().await;
        resolve_ensure(&gw, &agent_id, &node_id)
    };
    if let EnsureResolution::Present(instance_id) = present {
        return Ok((
            StatusCode::OK,
            Json(EnsureAck {
                agent_id,
                instance_id,
                already_present: true,
                operation_id: None,
            }),
        ));
    }

    let node_control = node_control_client(&state)?;
    ensure_node_ready(&state, &node_id).await?;
    check_node_compatible(&state, &node_id).await?;

    // Persist the source and build the download URL the node will use.
    let package_url = publish_to_registry(&state, &staged, &node_id).await?;

    // ADR-073 决策 5: the Gateway mints the candidate instance identity.
    // The node may still answer "already satisfied" (an instance the
    // Gateway has not seen yet) — that is what `ensure` means.
    let instance_id = uuid::Uuid::new_v4().to_string();
    let record = OperationRecord::new(0);
    let operation_id = record.operation_id.clone();
    if let Some(store) = state.operation_store.as_ref() {
        store.insert(record);
    }

    node_control
        .install_agent_by_url(NodeInstallDispatch {
            node_id: &node_id,
            instance_id: &instance_id,
            agent_id: &agent_id,
            source: NodePackageSource::Url(&package_url),
            dev_mode: gateway_dev_mode(&state).await,
            // The intent: at most one instance, no matter how many
            // callers ask (the node re-checks after dequeue).
            ensure: true,
            operation_id: operation_id.as_str(),
        })
        .await
        .map_err(|e| ApiError::internal(&format!("Ensure dispatch failed: {}", e)))?;

    // ADR-087: a dispatched ensure creates a new instance owned by the
    // caller. The already-present fast path above never touches the
    // owner record (ensure hit must not reassign ownership).
    permission::stage_agent_owner(&state, auth.as_ref().map(|e| &e.0), &instance_id, &agent_id, Some(&node_id));

    Ok((
        StatusCode::ACCEPTED,
        Json(EnsureAck {
            agent_id,
            instance_id,
            already_present: false,
            operation_id: Some(operation_id),
        }),
    ))
}

pub async fn install_agent(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    multipart: Multipart,
) -> Result<(StatusCode, Json<OperationAck>), ApiError> {
    let form = read_agent_upload(&state, multipart).await?;
    let package_bytes = form.package_bytes;
    let node_id = form.node_id;
    let expected_version = form.expected_version;

    // ADR-087 D4: installing onto a node is using that node → Node-manage.
    permission::check_node_manage(
        &state,
        auth.as_ref().map(|e| &e.0),
        &node_id,
    )?;

    // Persist the source into the package registry and build the download
    // URL the node will use (advertise_host = the address other machines
    // reach).
    let (manifest, package_url) =
        register_package_in_registry(&state, &package_bytes, &node_id).await?;
    let agent_id = manifest.agent_id.clone();

    // ADR-073 决策 5: the Gateway generates the instance identity at
    // install time. It decides the node-side landing directory
    // `{agent_id}/{instance_id}/` and the Runtime's
    // `--agent-instance-id`; the node never invents it.
    let instance_id = uuid::Uuid::new_v4().to_string();

    // Dispatch the asynchronous install (fire-and-forget). Completion is
    // observed via the node's retained installed inventory, not here.
    let node_control = state
        .node_control
        .clone()
        .ok_or_else(|| ApiError::internal("Node control plane unavailable (MQTT disabled)"))?;
    check_node_compatible(&state, &node_id).await?;
    // ADR-059 §2.3: the install command is a fire-and-forget MQTT
    // publish — if the target Node's control plane has not announced
    // `NodeReady`, the command would be silently dropped. Reject with
    // 409 `dependency_not_ready` (structured error, Phase 3.2) so the
    // caller retries once `GET /api/bootstrap` reaches READY. This
    // covers BOTH "never enrolled" and "enrolled but not ready": the
    // node publishes its enroll request before NodeReady (bootstrap
    // order, ADR-059 §7.2), so `node.{id}` cannot be ready while the
    // node is unknown to the registry.
    ensure_node_ready(&state, &node_id).await?;
    // ADR-059 §6: open an Accepted operation BEFORE dispatch — the node's
    // NodeEvent reply (same `request_id`) transitions it to
    // Completed/Failed. The ack carries the operation_id so the client
    // can correlate the async outcome.
    let record = OperationRecord::new(expected_version.unwrap_or(0));
    let mut ack = OperationAck::from_record(&record);
    // ADR-073: surface the gateway-generated instance identity in the
    // install ack so the client can address the instance immediately.
    ack.instance_id = Some(instance_id.clone());
    let operation_id = record.operation_id.clone();
    if let Some(store) = state.operation_store.as_ref() {
        store.insert(record);
    }

    node_control
        .install_agent_by_url(NodeInstallDispatch {
            node_id: &node_id,
            instance_id: &instance_id,
            agent_id: &agent_id,
            source: NodePackageSource::Url(&package_url),
            dev_mode: gateway_dev_mode(&state).await,
            // `POST /api/agents/install` is an explicit install of one
            // more copy (ADR-073 multi-instance) — a declarative
            // "ensure present" is a different intent and a different
            // endpoint.
            ensure: false,
            operation_id: operation_id.as_str(),
        })
        .await
        .map_err(|e| ApiError::internal(&format!("Install dispatch failed: {}", e)))?;

    // ADR-087 D2/D3: the caller becomes the owner of the new instance.
    permission::stage_agent_owner(&state, auth.as_ref().map(|e| &e.0), &instance_id, &agent_id, Some(&node_id));

    Ok((StatusCode::ACCEPTED, Json(ack)))
}

/// An uploaded `.agent` spooled to a temp file, with its parsed
/// manifest.
///
/// The upload is only ever a staging area — the durable copy is the
/// registry entry — so the temp file is deleted on drop, including on
/// the paths that return early (ensure's already-present fast path).
struct StagedPackage {
    path: std::path::PathBuf,
    manifest: AgentManifest,
}

impl StagedPackage {
    fn stage(package_bytes: &[u8]) -> Result<Self, ApiError> {
        let path = std::env::temp_dir().join(format!(
            "acowork-install-{}-{}.agent",
            std::process::id(),
            timestamp_nanos(),
        ));
        if let Err(e) = std::fs::write(&path, package_bytes) {
            return Err(ApiError::internal(&format!(
                "Failed to write upload to temp file: {}",
                e
            )));
        }
        // A `.agent` is a ZIP; the manifest must be read out of it before
        // the registry copy gets a stable name (the URL is derived from
        // `agent_id`, which only the manifest knows).
        match extract_manifest_from_package(&path) {
            Ok(manifest) => Ok(Self { path, manifest }),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(ApiError::bad_request(&format!("{}", e)))
            }
        }
    }
}

impl Drop for StagedPackage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Spool package bytes to a temp file, extract the manifest, and persist
/// the source into the package registry. Returns `(manifest, download_url)`.
///
/// Shared by install and upgrade — both persist the uploaded `.agent` to
/// the Gateway's registry and hand the node a download URL (ADR-055 §3.2).
async fn register_package_in_registry(
    state: &AppState,
    package_bytes: &[u8],
    node_id: &str,
) -> Result<(AgentManifest, String), ApiError> {
    let staged = StagedPackage::stage(package_bytes)?;
    let url = publish_to_registry(state, &staged, node_id).await?;
    Ok((staged.manifest.clone(), url))
}

/// Copy a staged package into the Gateway's registry and build the
/// download URL the target node will fetch it from.
///
/// The URL host must be reachable *from the node*: the local node is
/// loopback-bound, so it dials the HTTP bind host — auto-detecting a LAN
/// advertise host (ADR-055 D3) would make the download fail with
/// connection refused whenever the listener is 127.0.0.1-only. Remote
/// nodes get the advertise host, which is what they can route to.
async fn publish_to_registry(
    state: &AppState,
    staged: &StagedPackage,
    node_id: &str,
) -> Result<String, ApiError> {
    let agent_id = &staged.manifest.agent_id;
    let gw = state.gateway_state.read().await;
    let config = gw
        .config
        .as_ref()
        .ok_or_else(|| ApiError::internal("Gateway config unavailable"))?;
    let registry_dir = config.package_registry_dir();
    if let Err(e) = std::fs::create_dir_all(&registry_dir) {
        return Err(ApiError::internal(&format!(
            "Failed to create registry dir: {}",
            e
        )));
    }
    let registry_path = registry_dir.join(format!("{}.agent", agent_id));
    // ADR-075 D5: resolve the Gateway's own-machine node from the
    // registry (loopback dial); None → the "local" anchor (D6).
    let local_node_id = match state.node_registry.as_ref() {
        Some(reg) => crate::mqtt::node_registry::local_node_id(reg).await,
        None => None,
    };
    let url_host = crate::gateway::node_manager::dispatch_url_host(
        local_node_id.as_deref(),
        node_id,
        &config.http.host,
        gw.advertise_host.as_str(),
    );
    let url = format!(
        "http://{}:{}/api/packages/{}/download",
        url_host, config.http.port, agent_id
    );
    if let Err(e) = std::fs::copy(&staged.path, &registry_path) {
        return Err(ApiError::internal(&format!(
            "Failed to store package in registry: {}",
            e
        )));
    }
    Ok(url)
}

/// Extract the `manifest.toml` from a `.agent` package ZIP (path-based).
///
/// ADR-055 Phase 2b.3: the Gateway no longer installs packages itself —
/// it extracts the manifest only to route the node install command and to
/// register cron triggers. The actual extraction/install happens on the
/// node.
pub(crate) fn extract_manifest_from_package(
    package_path: &std::path::Path,
) -> Result<AgentManifest, GatewayError> {
    use std::io::Read;

    let data = std::fs::read(package_path).map_err(|e| {
        GatewayError::Package(format!(
            "Failed to read package '{}': {}",
            package_path.display(),
            e
        ))
    })?;
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| {
        GatewayError::Package(format!(
            "Failed to read ZIP '{}': {}",
            package_path.display(),
            e
        ))
    })?;
    let mut manifest_file = archive.by_name("manifest.toml").map_err(|e| {
        GatewayError::Package(format!("manifest.toml not found in package: {}", e))
    })?;
    let mut manifest_str = String::new();
    manifest_file
        .read_to_string(&mut manifest_str)
        .map_err(|e| GatewayError::Package(format!("Failed to read manifest.toml: {}", e)))?;
    AgentManifest::from_toml(&manifest_str)
        .map_err(|e| GatewayError::Package(format!("Invalid manifest.toml: {}", e)))
}

/// Track a node-hosted running agent in `GatewayState` after a
/// successful node start (ADR-055 §6.2). `pid` is 0 — the Gateway
/// observes liveness via MQTT status/ready topics, not the PID.
/// `agent_id` is the resolved instance identity (ADR-073); callers
/// obtain it from `resolve_agent_identity` before invoking this helper.
async fn track_running_agent(state: &AppState, agent_id: &str, dev_mode: bool) {
    // Resolve everything from an immutable snapshot first; the write
    // lock is only taken for the final upsert (avoids E0502).
    let (resolved_agent_id, workspace, node_id) = {
        let gw = state.gateway_state.read().await;
        let info = gw.installed(agent_id);
        let resolved_agent_id = info
            .map(|i| i.agent_id.clone())
            .unwrap_or_default();
        let workspace = info
            .map(|i| {
                std::path::PathBuf::from(&i.install_path)
                    .join("workspace")
                    .to_string_lossy()
                    .to_string()
            })
            .unwrap_or_default();
        // ADR-055 §6.2: the hosting node comes from the instance's
        // install record; fall back to the "local" anchor only when the
        // record is missing (a Runtime auto-tracked before its inventory
        // was aggregated — ADR-075 D6).
        let node_id = info
            .map(|i| i.node_id.clone())
            .unwrap_or_else(|| acowork_core::node::LOCAL_NODE_ID.to_string());
        (resolved_agent_id, workspace, node_id)
    };

    let mut gw = state.gateway_state.write().await;
    gw.add_running(crate::gateway::state::RunningAgentInfo {
        instance_id: agent_id.to_string(),
        agent_id: resolved_agent_id,
        pid: 0,
        started_at: chrono::Utc::now(),
        workspace,
        node_id,
        // ADR-085: the process is spawned but has not announced any
        // capability yet; the retained status envelope refines this.
        lifecycle: acowork_core::mqtt_proto::AgentLifecycleState::Offline,
        lifecycle_detail: String::new(),
        dev_mode,
        debug_state: if dev_mode {
            crate::gateway::state::DebugState::Enabled
        } else {
            crate::gateway::state::DebugState::Disabled
        },
        debug_port: None,
        workspace_config_json: None,
        current_embed_dim: None,
        migration: None,
    });
}

fn timestamp_nanos() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Clone mode: package content only, or plus the instance's own state.
///
/// The boundary itself is defined in `docs/design/zh/10-debug-protocol.md`
/// §7.2.1 (en §9.3) and implemented node-side in
/// `acowork_node::package::clone`; this enum only carries the choice
/// across the wire.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloneModeParam {
    /// All package-level files; no conversation history or memory.
    Skeleton,
    /// Skeleton plus the conversation history and the private store.
    Full,
}

/// Clone request body
#[derive(Debug, Deserialize)]
pub struct CloneRequest {
    /// Package id for the clone (reverse-domain, e.g.
    /// `com.example.weather-dev`). May equal the source package id —
    /// uniqueness is on the instance, not the package (ADR-073).
    pub new_agent_id: String,
    /// Clone mode: "skeleton" or "full"
    #[serde(default = "default_clone_mode")]
    pub mode: CloneModeParam,
    /// Human-readable name for the clone. Optional; empty (or absent)
    /// keeps the source name suffixed "(clone)". Names are not unique.
    #[serde(default)]
    pub new_name: String,
}

fn default_clone_mode() -> CloneModeParam {
    CloneModeParam::Skeleton
}

/// Clone response
#[derive(Debug, Serialize)]
pub struct CloneResponse {
    pub agent_id: String,
    pub install_path: String,
}

/// `POST /api/agents/:id/clone` — clone an agent. `:id` is the source
/// INSTANCE (ADR-073); the body carries the new PACKAGE id.
pub async fn clone_agent(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Path(agent_id): Path<String>,
    Json(req): Json<CloneRequest>,
) -> Result<(StatusCode, Json<CloneResponse>), ApiError> {
    // ADR-073: the route variable is the INSTANCE identity; `new_agent_id`
    // is a PACKAGE id. They are deliberately NOT required to differ:
    // several instances of one package are a legal state, so cloning onto
    // the source's own package id is a first-class operation (two agents
    // from one package, distinguished only by instance). Uniqueness lives
    // on the instance id, which the node mints.
    let (instance_id, resolved_agent_id) =
        resolve_agent_identity(&state, &agent_id).await?;
    let node_id = {
        let gw = state.gateway_state.read().await;
        gw.installed(&instance_id)
            .map(|i| i.node_id.clone())
            .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?
    };
    // ADR-087 D4: clone creates an instance on the source's node →
    // Node-manage on that node (the Agent-manage gate on the source ran
    // in the permission middleware).
    permission::check_node_manage(
        &state,
        auth.as_ref().map(|e| &e.0),
        &node_id,
    )?;

    let mode = match req.mode {
        CloneModeParam::Skeleton => "skeleton",
        CloneModeParam::Full => "full",
    };

    let node_control = state
        .node_control
        .clone()
        .ok_or_else(|| ApiError::internal("Node control plane unavailable (MQTT disabled)"))?;
    check_node_compatible(&state, &node_id).await?;
    let event = node_control
        .clone_agent(
            &node_id,
            &instance_id,
            &resolved_agent_id,
            &req.new_agent_id,
            mode,
            &req.new_name,
        )
        .await
        .map_err(|e| ApiError::internal(&format!("Clone failed: {}", e)))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&instance_id, &event)
        .map_err(|e| ApiError::internal(&format!("Clone failed: {}", e)))?;

    // The node reports the new install_path via result_json (JSON).
    let install_path = event
        .result_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
        .and_then(|v| {
            v.get("install_path")
                .and_then(|p| p.as_str())
                .map(String::from)
        })
        .unwrap_or_default();

    // ADR-087 D2: the clone is a new instance owned by the caller. The
    // node mints the instance id (ADR-073 keeps it node-side), but the
    // landing directory is `{new_agent_id}/{instance_id}/` — recover the
    // id from the reported path instead of widening the wire protocol.
    let new_instance_id = std::path::Path::new(&install_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if !new_instance_id.is_empty() {
        permission::record_agent_owner(
            &state,
            auth.as_ref().map(|e| &e.0),
            new_instance_id,
            &req.new_agent_id,
            Some(&node_id),
        );
    }

    Ok((
        StatusCode::CREATED,
        Json(CloneResponse {
            agent_id: req.new_agent_id,
            install_path,
        }),
    ))
}

/// `POST /api/agents/:id/upgrade` — upgrade an agent to a new package
///
/// Multipart body: `package` (the new .agent file, required) + optional
/// `node_id` (defaults to the node hosting the agent). The Gateway spools
/// the package to the registry and dispatches an asynchronous upgrade to
/// the node (ADR-055 §3.2) — completion is observed via the node's
/// retained installed inventory with the new version.
pub async fn upgrade_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<MessageResponse>), ApiError> {
    let mut package_bytes: Option<Vec<u8>> = None;
    let mut node_id: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(&format!("Failed to read multipart field: {}", e)))?
    {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "package" => {
                let bytes = field.bytes().await.map_err(|e| {
                    ApiError::bad_request(&format!("Failed to read package field: {}", e))
                })?;
                package_bytes = Some(bytes.to_vec());
            }
            "node_id" => {
                let text = field.text().await.unwrap_or_default();
                if !text.is_empty() {
                    node_id = Some(text);
                }
            }
            _ => {}
        }
    }

    let package_bytes =
        package_bytes.ok_or_else(|| ApiError::bad_request("Missing required field: 'package'"))?;
    if package_bytes.is_empty() {
        return Err(ApiError::bad_request("Package file is empty"));
    }

    // Upgrade targets an existing agent — default to the node hosting it.
    // ADR-073: the route variable is the INSTANCE identity (UUIDv4).
    let (instance_id, resolved_agent_id) =
        resolve_agent_identity(&state, &agent_id).await?;
    let node_id = match node_id {
        Some(n) => n,
        None => {
            let gw = state.gateway_state.read().await;
            gw.installed(&instance_id)
                .map(|i| i.node_id.clone())
                .ok_or_else(|| ApiError::not_found(&format!("Agent not found: {}", agent_id)))?
        }
    };

    // Persist the new package into the registry and verify the manifest's
    // agent_id matches the upgrade target.
    let (manifest, package_url) =
        register_package_in_registry(&state, &package_bytes, &node_id).await?;
    if manifest.agent_id != resolved_agent_id {
        return Err(ApiError::bad_request(&format!(
            "Package agent_id '{}' does not match upgrade target '{}'",
            manifest.agent_id, resolved_agent_id
        )));
    }

    // Dispatch the asynchronous upgrade (fire-and-forget).
    let node_control = state
        .node_control
        .clone()
        .ok_or_else(|| ApiError::internal("Node control plane unavailable (MQTT disabled)"))?;
    check_node_compatible(&state, &node_id).await?;
    node_control
        .upgrade_agent_by_url(
            &node_id,
            &instance_id,
            &resolved_agent_id,
            &package_url,
            gateway_dev_mode(&state).await,
        )
        .await
        .map_err(|e| ApiError::internal(&format!("Upgrade dispatch failed: {}", e)))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(MessageResponse {
            message: format!(
                "Upgrade dispatched to node '{}': {}",
                node_id, resolved_agent_id
            ),
        }),
    ))
}

/// `DELETE /api/agents/:id` — uninstall an agent
///
/// P1-9 fix: Uses spawn_blocking because uninstall_package performs
/// synchronous database operations (CronStore delete_by_agent) that
/// would block the tokio runtime if called directly in an async handler.
pub async fn uninstall_agent(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, ApiError> {
    // Check if agent is running first (lightweight read)
    {
        let gw = state.gateway_state.read().await;
        if gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is running, stop it first",
                agent_id
            )));
        }
    }

    // ADR-055 §6.2: delegate uninstall to the HOSTING node (the agent
    // may live on a remote node — routing to the local node would
    // publish into a topic nobody subscribes to). The node clears the
    // retained installed-info entry; the Gateway drops the agent from
    // installed_agents via the dispatch aggregation path.
    let node_control = state.node_control.clone().ok_or_else(|| {
        ApiError::internal("Node control plane unavailable (MQTT disabled)")
    })?;
    let (instance_id, resolved_agent_id) =
        resolve_agent_identity(&state, &agent_id).await?;
    let node_id = resolve_agent_node_id(&state, &instance_id).await?;
    // ADR-087 D4: uninstall is Node-manage ∧ Agent-manage — the agent
    // half ran in the permission middleware; add the node half here.
    permission::check_node_manage(
        &state,
        auth.as_ref().map(|e| &e.0),
        &node_id,
    )?;
    let event = node_control
        .uninstall_agent(
            &node_id,
            &instance_id,
            &resolved_agent_id,
        )
        .await
        .map_err(|e| ApiError::internal(&format!("Uninstall failed: {}", e)))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&instance_id, &event)
        .map_err(|e| ApiError::internal(&format!("Uninstall failed: {}", e)))?;

    // Drop the installed entry immediately (the node's retained clear is
    // the eventual-consistency backstop for a fresh Gateway).
    state.gateway_state.write().await.remove_installed(&instance_id);
    // ADR-087 D7: the ownership row dies with the instance.
    permission::remove_agent_owner(&state, &instance_id);
    // Wake the inventory-change notifier — subscribers (Desktop) refetch
    // `GET /api/agents` on each signal so the sidebar reflects the
    // uninstall without polling or a tab-switch remount.
    if let Some(t) = state.inventory_trigger.as_ref() {
        t.notify();
    }

    Ok(Json(MessageResponse {
        message: format!("Agent uninstalled: {}", resolved_agent_id),
    }))
}

/// Start agent request body
#[derive(Debug, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct StartAgentRequest {
    /// Start in developer mode (enables Debug Protocol: HTTP RPC + MQTT events per ADR-048)
    pub dev_mode: bool,
}


/// `POST /api/agents/:id/interactions` — record a user-driven interaction
/// for the given `instance_id` (ADR-073). Returns 204 No Content.
///
/// Idempotent within the same second (the persisted timestamp is
/// `Utc::now()`; identical consecutive touches produce the same row).
/// Does not validate `instance_id` against `installed_agents`: a touch
/// for an already-uninstalled agent leaves a harmless orphan entry that
/// `list_agents` never surfaces (it only iterates `installed_agents`).
/// `touch_interaction` itself is best-effort on persistence (warns on
/// disk-save failure but keeps the in-memory update), so this handler
/// stays non-blocking on disk hiccups.
pub async fn record_interaction(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
) -> StatusCode {
    let mut gw = state.gateway_state.write().await;
    gw.touch_interaction(&instance_id, chrono::Utc::now());
    StatusCode::NO_CONTENT
}


/// `POST /api/agents/:id/start` — start an agent
pub async fn start_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<StartAgentRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    // Pre-flight checks — released before we issue the node command.
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_installed(&agent_id) {
            return Err(ApiError::not_found(&format!(
                "Agent not found: {}",
                agent_id
            )));
        }
    }

    // ADR-055 idempotent fast-path: when the Runtime is already online
    // per the MQTT LWT registry (`acowork/agents/{id}/status = AgentStatus{online=true}`),
    // a `control/start` round-trip to the Node is redundant — the Node
    // would reply "already running" anyway, but the HTTP request can
    // hang up to COMMAND_TIMEOUT (30s) while the Node recovers (e.g.
    // after a host suspend/resume; 2026-09-07 incident: repeated clicks
    // each blocked 3-34s with zero UI feedback). Return an idempotent
    // 200 immediately instead.
    //
    // Auto-sleep (`AgentStatus.sleeping`) used to keep the process
    // alive but suspend the session, so a start had to reach the Node to
    // wake it. The auto-sleep subsystem was removed in Sept 2026 —
    // `sleeping` is no longer part of the wire — so every online state
    // is a fast-path candidate now.
    // INVARIANT: the fast-path is ONLY safe when the broker's
    // authoritative view (`agent_registry`) AND the local process
    // table (`running_agents`) agree. The original 2026-09-07 bug was
    // caused by treating `registry.is_online = true` as sufficient
    // without checking that `running_agents` already contained the
    // entry — the result was a 200 on a desynced Gateway whose UI
    // kept showing the agent as offline.
    //
    // Reconciliation rule (post-incident):
    //   registry online ∧ running_agents has entry   → idempotent 200.
    //   registry online ∧ running_agents MISSING     → DESYNC.
    //       Self-heal: call `reconcile_running_agents` (which re-derives
    //       `running_agents` from `agent_registry` — the SoT) and retry
    //       the fast-path once. If the retry still finds desync, that
    //       is a structural inconsistency — fall through to the node
    //       control path so the Node can re-stamp the entry, with a
    //       structured warning logged for the operator.
    //   registry offline ∧ running_agents has entry → not a
    //       fast-path candidate; the guard below rejects the duplicate
    //       start (stop first) until the broker view converges.
    //   registry offline ∧ running_agents MISSING   → not a
    //       fast-path candidate; fall through to node control.
    if let Some(ref reg) = state.agent_registry {
        // Single read so we don't race the reconcile loop on
        // `running_agents` between the snapshot and the entry check.
        let (live_online, has_entry) = {
            let reg_guard = reg.read().await;
            let reg_online = reg_guard.is_online(&agent_id);
            let gw_guard = state.gateway_state.read().await;
            let in_running = gw_guard.is_running(&agent_id);
            (reg_online, in_running)
        };
        if live_online && has_entry {
            tracing::info!(
                agent_id,
                "POST /start short-circuited: agent already online (idempotent, SoT-consistent)"
            );
            return Ok(Json(MessageResponse {
                message: format!("Agent already running: {}", agent_id),
            }));
        }
        if live_online && !has_entry {
            // DESYNC: the broker says the Runtime is up but the Gateway's
            // local view dropped the entry (the 2026-09-07 symptom).
            //
            // Strategy: this is a RECOVERABLE transient. The reconcile
            // loop should have caught it within `RECONCILE_INTERVAL_SECS`,
            // but it is reasonable to trigger an on-demand reconcile so
            // the click feels instant. We fire the reconcile + retry the
            // fast-path; only if the retry still finds desync do we
            // surface the inconsistency by falling through to the node
            // control path (which can re-stamp the entry directly).
            //
            // We deliberately do NOT swallow this as a 200 — that was
            // the bug.
            tracing::warn!(
                agent_id,
                "POST /start: registry online but running_agents missing (desync) — reconciling"
            );
            crate::mqtt::dispatch::reconcile_running_agents(
                &state.gateway_state,
                reg,
            )
            .await;
            let still_desynced = {
                let gw_guard = state.gateway_state.read().await;
                !gw_guard.is_running(&agent_id)
            };
            if !still_desynced {
                tracing::info!(
                    agent_id,
                    "POST /start: reconcile restored running_agents entry; idempotent 200"
                );
                return Ok(Json(MessageResponse {
                    message: format!(
                        "Agent already running: {} (reconciled)",
                        agent_id
                    ),
                }));
            }
            tracing::warn!(
                agent_id,
                "POST /start: reconcile did NOT restore the entry — falling through to node control to re-stamp"
            );
            // Fall through; do NOT return 200.
        }
    }

    // Local process table has an entry the broker view does not
    // consider live-online (registry `offline`, or no registry wired):
    // the idempotent fast-path above does not apply. Reject the
    // duplicate start exactly like the pre-fast-path code did — a stop
    // (or broker-view convergence) must come first.
    if state.gateway_state.read().await.is_running(&agent_id) {
        return Err(ApiError::bad_request(&format!(
            "Agent {} is already running",
            agent_id
        )));
    }

    // ADR-055 §6.2: delegate start to the local node via the control
    // plane instead of spawning the Runtime directly.
    //
    // Error layering (2026-09-07 incident review):
    //   - Every error path surfaces a structured `ApiError` (HTTP
    //     status + machine-readable `StructuredErrorCode` body) so the
    //     Desktop can render a precise toast instead of a generic
    //     "something went wrong".
    //   - TRANSIENT failures (publish hiccup, command timeout) carry a
    //     `retry_hint`; the client retries automatically with backoff
    //     and the user sees nothing.
    //   - STRUCTURAL failures (agent_not_installed on the node side,
    //     unknown_node, bad-request) carry NO retry hint; the user
    //     must act (e.g. install the agent package).
    let node_control = state.node_control.clone().ok_or_else(|| {
        // Broker disabled — there is no control plane. Unrecoverable
        // for this Gateway instance until the operator re-enables MQTT.
        ApiError::structured(
            StatusCode::SERVICE_UNAVAILABLE,
            "Node control plane unavailable (MQTT disabled)",
            StructuredErrorBody {
                code: acowork_core::error_codes::StructuredErrorCode::DependencyNotReady,
                phase_detail: Some("mqtt_control_plane_disabled".to_string()),
                retry_hint: None,
                ..Default::default()
            },
        )
    })?;
    // ADR-073: route the command to the instance identity.
    let (instance_id, resolved_agent_id) =
        resolve_agent_identity(&state, &agent_id).await?;
    // ADR-055 §6.2: route to the node HOSTING the instance — the
    // command topic is per-node, so a hardcoded local node would
    // publish into a topic nobody subscribes to (timeout + Desktop
    // retry loop for an agent installed on a remote node).
    let node_id = resolve_agent_node_id(&state, &instance_id).await?;
    check_node_compatible(&state, &node_id).await?;
    let event = node_control
        .start_agent(
            &node_id,
            &instance_id,
            &resolved_agent_id,
            req.dev_mode,
        )
        .await
        .map_err(|e| map_node_control_error("start", &instance_id, e))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&instance_id, &event)
        .map_err(|e| ApiError::structured(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("Start rejected by node: {}", e),
            StructuredErrorBody {
                code: acowork_core::error_codes::StructuredErrorCode::OperationExpired,
                phase_detail: Some(e.to_string()),
                operation_id: None,
                retry_hint: None,
                ..Default::default()
            },
        ))?;

    // Track the running entry in GatewayState (node-hosted, pid 0).
    track_running_agent(&state, &instance_id, req.dev_mode).await;

    // When starting in debug mode, bump Gateway's log level to DEBUG
    // so the Settings UI reflects the effective log level.
    if req.dev_mode {
        let level = "debug";
        // 1. Update stored config
        {
            let mut gw = state.gateway_state.write().await;
            if let Some(config) = &mut gw.config {
                config.log_level = level.to_string();
            }
        }
        // 2. Apply to Gateway's own tracing subscriber
        if let Some(handle) = &state.log_reload_handle {
            let new_filter = acowork_core::logging::build_env_filter(level);
            if let Err(e) = handle.reload(new_filter) {
                tracing::warn!(
                    "Failed to reload Gateway tracing filter for debug mode: {}",
                    e
                );
            } else {
                tracing::info!(
                    "Gateway log level set to {} (debug mode agent start)",
                    level
                );
            }
        }
    }

    let mode_label = if req.dev_mode { " (dev mode)" } else { "" };
    Ok(Json(MessageResponse {
        message: format!("Agent started: {}{}", agent_id, mode_label),
    }))
}

/// Map a [`crate::mqtt::node_control::NodeControlError`] into a layered
/// [`ApiError`] (2026-09-07 incident follow-up).
///
/// The layering principle (from the incident review):
///
/// | Error variant              | Layer | HTTP | StructuredCode            | Retry hint | UX                      |
/// |----------------------------|-------|------|---------------------------|------------|-------------------------|
/// | `Timeout`                  | trans | 504  | `HandshakeTimeout`        | yes        | invisible (auto-retry)  |
/// | `Publish`                  | trans | 503  | `DependencyNotReady`      | yes        | invisible (auto-retry)  |
/// | `NoClient`                 | struct| 503  | `DependencyNotReady`      | no         | toast: broker disabled  |
/// | `NodeOffline`              | struct| 503  | `DependencyNotReady`      | no         | toast: node enrolling   |
/// | `CommandFailed`            | struct| 422  | `OperationExpired`        | no         | toast: node rejected it |
///
/// The retry hint (`retry_after_ms` + `retry_count`) asks the Desktop
/// to re-issue the command automatically. The wire format carries no
/// backoff multiplier, so the hint is a fixed per-attempt delay (the
/// Desktop may apply its own scaling between attempts).
///
/// `op_name` (`"start"` / `"stop"` / `"restart-debug"`) is included in
/// the human message so the log/UI can attribute the failure without
/// reading the structured body.
fn map_node_control_error(
    op_name: &str,
    agent_id: &str,
    e: crate::mqtt::node_control::NodeControlError,
) -> ApiError {
    use crate::mqtt::node_control::NodeControlError as E;
    use acowork_core::error_codes::{RetryHint, StructuredErrorCode};
    match e {
        // TRANSIENT — command round-trip exceeded the deadline. The
        // node may be slow to recover (sleep/wake, MCP reconnect,
        // cold-start of a large workspace). The retry hint asks the
        // Desktop to re-issue automatically; user sees nothing.
        // retry_after_ms=1500 matches the gateway_command_timeout
        // (10s) / 2 floor so the first retry lands inside the same
        // wake window.
        E::Timeout { request_id } => ApiError::structured(
            StatusCode::GATEWAY_TIMEOUT,
            &format!(
                "{op_name}: node did not answer within the deadline (request_id {request_id})"
            ),
            StructuredErrorBody {
                code: StructuredErrorCode::HandshakeTimeout,
                phase_detail: Some(format!(
                    "{op_name}_node_command_timeout agent={agent_id}"
                )),
                retry_hint: Some(RetryHint {
                    retry_after_ms: Some(1500),
                    retry_count: 5,
                }),
                ..Default::default()
            },
        ),

        // TRANSIENT — the broker is unreachable from the Gateway side.
        // Either the publisher transiently dropped or the broker
        // itself restarted. The retry hint asks the Desktop to
        // re-issue automatically; user sees nothing.
        E::Publish(msg) => ApiError::structured(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{op_name}: MQTT publish failed ({msg})"),
            StructuredErrorBody {
                code: StructuredErrorCode::DependencyNotReady,
                phase_detail: Some(format!("{op_name}_mqtt_publish_transient_failure")),
                retry_hint: Some(RetryHint {
                    retry_after_ms: Some(2000),
                    retry_count: 5,
                }),
                ..Default::default()
            },
        ),

        // STRUCTURAL — no MQTT client wired (broker disabled in
        // config). Operator must act.
        E::NoClient => ApiError::structured(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{op_name}: node control plane unavailable (broker disabled)"),
            StructuredErrorBody {
                code: StructuredErrorCode::DependencyNotReady,
                phase_detail: Some("mqtt_broker_disabled_in_config".to_string()),
                retry_hint: None,
                ..Default::default()
            },
        ),

        // STRUCTURAL — the named node has not announced `NodeReady`
        // yet (or has been demoted). Client should wait for bootstrap
        // to complete; no automatic retry because the node's lifecycle
        // is gated on the bootstrap barrier.
        E::NodeOffline { node_id } => ApiError::structured(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("{op_name}: node '{node_id}' is offline or not yet ready"),
            StructuredErrorBody {
                code: StructuredErrorCode::DependencyNotReady,
                phase_detail: Some(format!("node_offline node_id={node_id}")),
                retry_hint: None,
                ..Default::default()
            },
        ),

        // STRUCTURAL — the node replied with an error (e.g.
        // `agent_not_installed`, `spawn_failed`, `permission_denied`).
        // This is the node-side definitive answer; retrying would
        // produce the same answer.
        E::CommandFailed { agent_id: aid, message } => ApiError::structured(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("{op_name}: node rejected command for '{aid}': {message}"),
            StructuredErrorBody {
                code: StructuredErrorCode::OperationExpired,
                phase_detail: Some(format!("{op_name}_command_failed agent={aid} message={message}")),
                retry_hint: None,
                ..Default::default()
            },
        ),
    }
}

/// `POST /api/agents/:id/stop` — stop a running agent
pub async fn stop_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, ApiError> {
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // ADR-055 §6.2: delegate stop to the local node.
    let node_control = state.node_control.clone().ok_or_else(|| {
        ApiError::structured(
            StatusCode::SERVICE_UNAVAILABLE,
            "stop: node control plane unavailable (MQTT disabled)",
            StructuredErrorBody {
                code: acowork_core::error_codes::StructuredErrorCode::DependencyNotReady,
                phase_detail: Some("mqtt_control_plane_disabled".to_string()),
                retry_hint: None,
                ..Default::default()
            },
        )
    })?;
    let (instance_id, resolved_agent_id) =
        resolve_agent_identity(&state, &agent_id).await?;
    // ADR-055 §6.2: route to the node hosting the instance.
    let node_id = resolve_agent_node_id(&state, &instance_id).await?;
    let event = node_control
        .stop_agent(
            &node_id,
            &instance_id,
            &resolved_agent_id,
            "user",
        )
        .await
        .map_err(|e| map_node_control_error("stop", &instance_id, e))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&instance_id, &event)
        .map_err(|e| ApiError::structured(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("stop: node rejected command: {}", e),
            StructuredErrorBody {
                code: acowork_core::error_codes::StructuredErrorCode::OperationExpired,
                phase_detail: Some(e.to_string()),
                retry_hint: None,
                ..Default::default()
            },
        ))?;

    // Close the liveness race (2026-09-22 stop-UI delay): the node ACK
    // returns before the Runtime's MQTT teardown delivers the LWT, so a
    // `GET /api/agents` issued by the stop caller (Desktop `stopAgent`
    // refetches immediately) can still read `online=true` and clobber the
    // realtime offline event that reaches it milliseconds later — leaving
    // the UI in the chat view until an unrelated refetch converges it.
    // Mark the registry offline synchronously (deterministic for any
    // subsequent list read) and publish the retained `AgentStatus{online=false}`
    // envelope so the normal dispatch path (registry loopback → Desktop
    // subscribers) runs exactly as it would for an LWT. The LWT itself
    // remains the fallback for crash paths and is idempotent here.
    let status_topic = format!("acowork/agents/{instance_id}/status");
    let node_id_owned = node_id.clone();
    let offline_envelope = acowork_core::mqtt_proto::DataEnvelope {
        version: 1,
        payload: Some(
            acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(
                acowork_core::mqtt_proto::AgentStatus {
                    agent_id: resolved_agent_id.clone(),
                    online: false,
                    instance_id: instance_id.clone(),
                    node_id: node_id_owned,
                    // ADR-085 invariant 6: offline ⇒ OFFLINE.
                    state: acowork_core::mqtt_proto::AgentLifecycleState::Offline as i32,
                    detail: String::new(),
                },
            ),
        ),
    };
    let offline_bytes = prost::Message::encode_to_vec(&offline_envelope);
    if let Some(reg) = &state.agent_registry {
        reg.write().await.update_from_mqtt(&status_topic, &offline_bytes);
    }
    if let Some(client) = state.mqtt_gateway_client.as_ref()
        && let Err(e) = client
            .publish_envelope(
                &status_topic,
                &offline_envelope,
                crate::mqtt::MqttQoS::AtLeastOnce,
                true,
            )
            .await
    {
        // Non-fatal: the registry is already updated and the LWT will
        // still fire; only the Desktop's realtime push is degraded.
        tracing::warn!(
            instance_id = %instance_id,
            error = %e,
            "stop: failed to publish offline status"
        );
    }

    // Pre-emptively drop the running entry (mirrors the old stop path).
    state.gateway_state.write().await.remove_running(&agent_id);

    Ok(Json(MessageResponse {
        message: format!("Agent stopped: {}", agent_id),
    }))
}

/// `POST /api/agents/:id/restart-debug` — restart a running agent in debug mode
///
/// ADR-033: gRPC removed. Debug mode is now configured at agent start time
/// (via `POST /api/agents/{id}/start` with `dev_mode: true`). Restart-in-debug
/// requires a full process restart in MQTT mode.
///
/// **ADR-048 follow-up — DEPRECATED.** The Desktop right-click "Restart
/// in Debug" context menu no longer calls this endpoint (removed from
/// `AgentList.tsx`); users flip DevMode on via
/// `POST /api/agents/{id}/debug/enable` instead, which proxies to the
/// Runtime's `/api/debug/enable` without an agent restart. This
/// handler remains operational so any external script or older Desktop
/// build still works, and so the operator escape hatch (full process
/// restart to genuinely reset session state) is still available.
/// Plan to remove in a follow-up release once the Desktop has shipped
/// without the menu for one full minor version.
pub async fn restart_agent_in_debug(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<MessageResponse>, ApiError> {
    {
        let gw = state.gateway_state.read().await;
        if !gw.is_running(&agent_id) {
            return Err(ApiError::bad_request(&format!(
                "Agent {} is not running",
                agent_id
            )));
        }
    }

    // Already in debug mode — no-op
    let already_in_debug = {
        let gw = state.gateway_state.read().await;
        gw.running_agents
            .get(&agent_id)
            .map(|info| info.dev_mode && info.debug_port.is_some())
            .unwrap_or(false)
    };
    if already_in_debug {
        let port = {
            let gw = state.gateway_state.read().await;
            gw.running_agents
                .get(&agent_id)
                .and_then(|info| info.debug_port)
                .unwrap_or(0)
        };
        return Ok(Json(MessageResponse {
            message: format!(
                "Agent {} is already in debug mode (port {})",
                agent_id, port
            ),
        }));
    }

    // ADR-055 §6.2: restart-in-debug = node stop + node start(dev_mode).
    let node_control = state.node_control.clone().ok_or_else(|| {
        ApiError::structured(
            StatusCode::SERVICE_UNAVAILABLE,
            "restart-debug: node control plane unavailable (MQTT disabled)",
            StructuredErrorBody {
                code: acowork_core::error_codes::StructuredErrorCode::DependencyNotReady,
                phase_detail: Some("mqtt_control_plane_disabled".to_string()),
                retry_hint: None,
                ..Default::default()
            },
        )
    })?;

    // Stop current process
    let (instance_id, resolved_agent_id) =
        resolve_agent_identity(&state, &agent_id).await?;
    // ADR-055 §6.2: route to the node hosting the instance.
    let node_id = resolve_agent_node_id(&state, &instance_id).await?;
    let stop_event = node_control
        .stop_agent(
            &node_id,
            &instance_id,
            &resolved_agent_id,
            "debug-restart",
        )
        .await
        .map_err(|e| map_node_control_error("restart-debug:stop", &instance_id, e))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&instance_id, &stop_event)
        .map_err(|e| ApiError::structured(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("restart-debug: stop before restart failed: {}", e),
            StructuredErrorBody {
                code: acowork_core::error_codes::StructuredErrorCode::OperationExpired,
                phase_detail: Some(e.to_string()),
                retry_hint: None,
                ..Default::default()
            },
        ))?;
    state.gateway_state.write().await.remove_running(&agent_id);

    // Start with dev_mode=true
    let start_event = node_control
        .start_agent(
            &node_id,
            &instance_id,
            &resolved_agent_id,
            true,
        )
        .await
        .map_err(|e| map_node_control_error("restart-debug:start", &instance_id, e))?;
    crate::mqtt::node_control::NodeControlClient::check_reply(&instance_id, &start_event)
        .map_err(|e| ApiError::structured(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("restart-debug: start with dev_mode=true failed: {}", e),
            StructuredErrorBody {
                code: acowork_core::error_codes::StructuredErrorCode::OperationExpired,
                phase_detail: Some(e.to_string()),
                retry_hint: None,
                ..Default::default()
            },
        ))?;
    track_running_agent(&state, &agent_id, true).await;

    // Bump Gateway's log level to DEBUG so the Settings UI reflects it.
    {
        let level = "debug";
        {
            let mut gw = state.gateway_state.write().await;
            if let Some(config) = &mut gw.config {
                config.log_level = level.to_string();
            }
        }
        if let Some(handle) = &state.log_reload_handle {
            let new_filter = acowork_core::logging::build_env_filter(level);
            if let Err(e) = handle.reload(new_filter) {
                tracing::warn!(
                    "Failed to reload Gateway tracing filter for debug mode: {}",
                    e
                );
            } else {
                tracing::info!("Gateway log level set to {} (restart-in-debug)", level);
            }
        }
    }

    Ok(Json(MessageResponse {
        message: format!("Agent restarted in debug mode: {}", agent_id),
    }))
}

/// `GET /api/agents/:id/model` — get the current active model for an agent
///
/// Queries the Runtime for per-agent model/provider preferences (stored in
/// workspace/config/agent_model.json). Gateway does NOT decide defaults —
/// default model/provider selection is session-level logic owned by the Runtime.
/// If the Runtime has no preference configured, returns empty strings.
pub async fn get_agent_model(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentModelResponse>, ApiError> {
    let gw = state.gateway_state.read().await;

    // Verify agent exists
    if !gw.installed_agents.contains_key(&agent_id) {
        return Err(ApiError::not_found(&format!(
            "Agent not found: {}",
            agent_id
        )));
    }

    // ADR-033: gRPC removed — per-agent model/provider preferences are
    // read from agent_config.json on startup. Live queries via gRPC
    // are no longer supported. Return empty — Runtime decides defaults.
    let _unused = &agent_id;
    let active_model: Option<String> = None;
    let active_provider: Option<String> = None;

    // If Runtime has no preference, return empty — let the Runtime/Session decide defaults.
    let provider = match active_provider {
        Some(ref ap) if !ap.is_empty() => ap.clone(),
        _ => {
            return Ok(Json(AgentModelResponse {
                provider: String::new(),
                model: String::new(),
                available_models: Vec::new(),
            }));
        }
    };

    // Look up provider config from resource_cache for available_models.
    let available_models: Vec<String> = gw
        .resource_cache
        .provider_list
        .providers
        .iter()
        .find(|p| p.id == provider)
        .map(|cfg| cfg.models.iter().map(|m| m.id.clone()).collect())
        .unwrap_or_default();

    let model = active_model
        .filter(|m| available_models.contains(m))
        .unwrap_or_default();

    Ok(Json(AgentModelResponse {
        provider,
        model,
        available_models,
    }))
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state::{AgentInfo, GatewayState};
    use crate::http::auth::HttpAuth;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Test-only instance identity for "com.acowork.architect" / "com.acowork.senior-engineer".
    const INSTANCE_ARCHITECT: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    const INSTANCE_SENIOR_ENG: &str = "2b3c4d5e-6f7a-4b8c-9d0e-1f2a3b4c5d6e";
    const INSTANCE_FOO_BAZ: &str = "4d5e6f7a-8b9c-4d0e-9f1a-3b4c5d6e7f8a";
    const INSTANCE_WEATHER: &str = "5e6f7a8b-9c0d-4e1f-a02b-4c5d6e7f8a9b";

    fn test_manifest(agent_id: &str) -> acowork_core::AgentManifest {        acowork_core::AgentManifest {
            config: Default::default(),
            labels: Vec::new(),
            mcp_servers: Vec::new(),
            agent_id: agent_id.to_string(),
            version: "1.0.0".to_string(),
            name: "Test Agent".to_string(),
            display_name: None,
            role: None,
            avatar: None,
            builtin_avatar: None,
            description: "test".to_string(),
            author: "test".to_string(),
            runtime_version: "0.1.0".to_string(),
            permissions: vec![],
            triggers: vec![],
            llm: Default::default(),
            memory: Default::default(),
            identity_deps: vec![],
            tools: vec![],
            capabilities: Default::default(),
            resources: Default::default(),
            sandbox: Default::default(),
            dev: false,
            skills: Default::default(),
        }
    }

    /// Build an AppState with an empty GatewayState (no registry —
    /// `list_agents` degrades gracefully when `agent_registry` is None).
    fn test_state() -> AppState {
        let gw = Arc::new(RwLock::new(GatewayState::new(
            "/tmp/acowork-list-test-vault",
        )));
        AppState::new(gw, Arc::new(HttpAuth::new(false)))
    }

    /// AppState with instance `INST` installed and an (initially empty)
    /// agent-ownership store wired in.
    async fn state_with_installed(inst: &str) -> AppState {
        let mut state = test_state();
        let dir = std::env::temp_dir().join(format!("acowork-test-agent-perms-{}", inst));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        state.agent_owners = Some(ownership::new_shared_agent_owners(&dir));
        state
            .gateway_state
            .write()
            .await
            .add_installed(crate::gateway::state::AgentInfo {
                instance_id: inst.to_string(),
                agent_id: "com.foo.bar".to_string(),
                version: "1.0.0".to_string(),
                name: "Foo Bar".to_string(),
                install_path: format!("/tmp/pkg/{}", inst),
                manifest: test_manifest("com.foo.bar"),
                node_id: "local".to_string(),
            });
        state
    }

    /// ADR-087 D7 fail-closed. This is the reported bug verbatim: agents
    /// with NO `agent_owners.json` row were listed for EVERY account
    /// (the old guard only filtered rows that existed — `is_some_and`),
    /// then 404'd on click because the detail route required `can_use`.
    /// Users saw private agents they could not open, stuck loading.
    #[tokio::test]
    async fn list_agents_hides_unowned_agent_from_stranger() {
        let inst = "cccccccc-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await; // no ownership row
        let ctx = Extension(AuthContext {
            user_id: "stranger".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let Json(list) = list_agents(
            State(state),
            Some(ctx),
            Query(AgentListQuery { agent_id: None, node_id: None }),
        )
        .await;
        assert!(
            list.iter().all(|a| a.instance_id != inst),
            "an agent with no ownership row must not be listed to a normal user"
        );
    }

    /// The mirror image: admin must still see it, or an unclaimed agent
    /// could never be found and claimed.
    #[tokio::test]
    async fn list_agents_shows_unowned_agent_to_admin() {
        let inst = "dddddddd-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await; // no ownership row
        let ctx = Extension(AuthContext {
            user_id: "root".to_string(),
            role: acowork_core::account::Role::Admin,
            as_user: None,
        });
        let Json(list) = list_agents(
            State(state),
            Some(ctx),
            Query(AgentListQuery { agent_id: None, node_id: None }),
        )
        .await;
        assert!(
            list.iter().any(|a| a.instance_id == inst),
            "admin must see ownerless agents so they can be claimed"
        );
    }

    /// AppState with instance installed, BOTH ownership stores wired,
    /// and the agent's owner row carrying `node_id` (Shared visibility)
    /// so the only thing that can hide it is its hosting node. The ACL
    /// resolves the `Agent -> Node` parent through the agent row's
    /// denormalized `node_id` — so that is the field these tests set.
    async fn state_with_shared_agent_on_node(inst: &str, node_id: &str) -> AppState {
        let mut state = test_state();
        let dir = std::env::temp_dir().join(format!("acowork-test-agent-node-{}", inst));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        state.agent_owners = Some(ownership::new_shared_agent_owners(&dir));
        state.node_owners = Some(ownership::new_shared_node_owners(&dir));
        state
            .gateway_state
            .write()
            .await
            .add_installed(AgentInfo {
                instance_id: inst.to_string(),
                agent_id: "com.foo.bar".to_string(),
                version: "1.0.0".to_string(),
                name: "Foo Bar".to_string(),
                install_path: format!("/tmp/pkg/{}", inst),
                manifest: test_manifest("com.foo.bar"),
                node_id: node_id.to_string(),
            });
        ownership::lock(&state.agent_owners.clone().unwrap()).put(
            inst,
            OwnerRecord {
                owner_user_id: Some("alice".into()),
                guests: vec![],
                // Shared → viewable on its OWN row by every signed-in
                // account. A leak here is exactly what the cascade fixes.
                visibility: Visibility::Shared,
                created_at: chrono::Utc::now(),
                node_id: Some(node_id.into()),
            },
        );
        state
    }

    fn put_node(state: &AppState, node_id: &str, visibility: Visibility) {
        ownership::lock(&state.node_owners.clone().unwrap()).put(
            node_id,
            OwnerRecord {
                owner_user_id: Some("alice".into()),
                guests: vec![],
                visibility,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
    }

    /// The reported leak, closed: a Shared agent on a node the caller
    /// cannot view must NOT be listed. The list previously filtered on
    /// the agent's own row alone, so this row appeared for everyone yet
    /// 403'd the moment it was opened (the node cascade denied it), and
    /// per-node grouping had no node header to file it under.
    #[tokio::test]
    async fn list_agents_hides_agent_whose_node_is_not_viewable() {
        let inst = "ffffffff-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_shared_agent_on_node(inst, "gpu-1").await;
        put_node(&state, "gpu-1", Visibility::Private);
        let ctx = Extension(AuthContext {
            user_id: "carol".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let Json(list) = list_agents(
            State(state),
            Some(ctx),
            Query(AgentListQuery { agent_id: None, node_id: None }),
        )
        .await;
        assert!(
            list.iter().all(|a| a.instance_id != inst),
            "a shared agent on a node the caller cannot view must not be listed (Node->Agent cascade)"
        );
    }

    /// The allowed side of the same cascade: a Shared agent on a PUBLIC
    /// node passes both tiers, so it stays listed for every account.
    #[tokio::test]
    async fn list_agents_shows_agent_when_its_node_is_public() {
        let inst = "11111111-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_shared_agent_on_node(inst, "gpu-2").await;
        put_node(&state, "gpu-2", Visibility::Public);
        let ctx = Extension(AuthContext {
            user_id: "carol".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let Json(list) = list_agents(
            State(state),
            Some(ctx),
            Query(AgentListQuery { agent_id: None, node_id: None }),
        )
        .await;
        assert!(
            list.iter().any(|a| a.instance_id == inst),
            "a shared agent on a public node is viewable at both tiers — it must stay listed"
        );
    }

    /// Local mode (auth == None) is a no-op: the cascade must not filter
    /// anything, so the same ownerless setup that hides from a stranger
    /// still lists in single-user mode.
    #[tokio::test]
    async fn list_agents_local_mode_skips_the_cascade() {
        let inst = "22222222-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        let Json(list) = list_agents(
            State(state),
            None,
            Query(AgentListQuery { agent_id: None, node_id: None }),
        )
        .await;
        assert!(
            list.iter().any(|a| a.instance_id == inst),
            "Local mode (no AuthContext) lists everything — authorization is a no-op"
        );
    }

    /// A guest keeps the row even when the agent is private — the
    /// sidebar is where a node/agent is acted on.
    #[tokio::test]
    async fn list_agents_shows_private_agent_to_its_guest() {
        let inst = "eeeeeeee-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        ownership::lock(&state.agent_owners.clone().unwrap()).put(
            inst,
            OwnerRecord {
                owner_user_id: Some("alice".into()),
                guests: vec!["bob".into()],
                visibility: Visibility::Private,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        let ctx = Extension(AuthContext {
            user_id: "bob".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let Json(list) = list_agents(
            State(state),
            Some(ctx),
            Query(AgentListQuery { agent_id: None, node_id: None }),
        )
        .await;
        assert!(
            list.iter().any(|a| a.instance_id == inst),
            "a guest must keep seeing the private agent"
        );
    }

    /// List and detail MUST agree. They used opposite defaults
    /// (`is_some_and` vs `is_none_or`), which is what produced rows
    /// that were visible but unopenable.
    #[tokio::test]
    async fn list_and_detail_agree_on_visibility() {
        let inst = "ffffffff-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await; // no ownership row
        let ctx = |id: &str| {
            Extension(AuthContext {
                user_id: id.to_string(),
                role: acowork_core::account::Role::User,
                as_user: None,
            })
        };

        let Json(list) = list_agents(
            State(state.clone()),
            Some(ctx("stranger")),
            Query(AgentListQuery { agent_id: None, node_id: None }),
        )
        .await;
        let listed = list.iter().any(|a| a.instance_id == inst);

        let detail = get_agent_detail(State(state), Some(ctx("stranger")), Path(inst.into())).await;
        let readable = detail.is_ok();

        assert_eq!(
            listed, readable,
            "list visibility and detail visibility must not disagree"
        );
        assert!(!listed && !readable);
    }

    #[tokio::test]
    async fn agent_permissions_owner_caller_gets_full_state_editable() {
        let inst = "aaaaaaaa-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        ownership::lock(&state.agent_owners.clone().unwrap()).put(
            inst,
            OwnerRecord {
                owner_user_id: Some("alice".into()),
                guests: vec!["bob".into()],
                visibility: Visibility::Shared,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        let ctx = Extension(AuthContext {
            user_id: "alice".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let Json(v) = get_agent_permissions(State(state), Path(inst.into()), Some(ctx))
            .await
            .unwrap();
        assert_eq!(v["owner_user_id"], serde_json::json!("alice"));
        assert_eq!(v["guests"], serde_json::json!(["bob"]));
        assert_eq!(v["visibility"], "shared");
        assert_eq!(v["can_attribute"], true);
    }

    #[tokio::test]
    async fn agent_permissions_guest_caller_is_read_only() {
        // ADR-087 D9 R1: guests see the list, cannot change it.
        let inst = "bbbbbbbb-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        ownership::lock(&state.agent_owners.clone().unwrap()).put(
            inst,
            OwnerRecord {
                owner_user_id: Some("alice".into()),
                guests: vec!["bob".into()],
                visibility: Visibility::Private,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        let ctx = Extension(AuthContext {
            user_id: "bob".to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        });
        let Json(v) = get_agent_permissions(State(state), Path(inst.into()), Some(ctx))
            .await
            .unwrap();
        assert_eq!(v["can_attribute"], false);
    }

    #[tokio::test]
    async fn agent_permissions_local_mode_no_row_is_editable() {
        // No ownership row at all (Local mode / never attributed): empty
        // state, editable, visibility reads as `shared` in local mode.
        let inst = "cccccccc-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        let Json(v) = get_agent_permissions(State(state), Path(inst.into()), None)
            .await
            .unwrap();
        assert_eq!(v["owner_user_id"], serde_json::Value::Null);
        assert_eq!(v["guests"], serde_json::json!([]));
        assert_eq!(v["visibility"], "shared");
        assert_eq!(v["can_attribute"], true);
    }

    /// The reported bug: an admin flips the visibility switch to
    /// `shared` in the Desktop permission dialog, presses Save, and the
    /// switch snaps back to off with no error shown.
    ///
    /// `upsert_with` normalised `ownerless ⇒ not Shared` *after* the
    /// caller's mutation had been applied, so an ownerless agent
    /// silently stored `private` — while the handler echoed the
    /// requested value back as 200, and the dialog's post-save
    /// `load()` then re-read the unchanged stored value.
    ///
    /// The invariant is a data-hygiene rule, not an authorization one:
    /// `can_view` already gates on `is_admin ∨ can_use ∨ shared`, so
    /// an ownerless+shared agent is still admin-only for everyone else.
    /// Rejecting the write up front turns a silent no-op into a
    /// diagnosable error.
    #[tokio::test]
    async fn patch_agent_visibility_rejects_shared_on_ownerless_row() {
        let inst = "dddddddd-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        ownership::lock(&state.agent_owners.clone().unwrap()).put(
            inst,
            OwnerRecord {
                owner_user_id: None,
                guests: vec!["babea202".into()],
                visibility: Visibility::Private,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        let err = patch_agent_visibility(
            State(state),
            Path(inst.into()),
            Json(serde_json::json!({ "visibility": "shared" })),
        )
        .await
        .expect_err("ownerless + shared must be rejected, not silently downgraded");
        assert_eq!(err.code, 409, "the client must be able to branch on the status");
        assert!(
            err.error.contains("owner"),
            "the error must name the cause (claim an owner first): {}",
            err.error
        );
    }

    /// The inverse direction must keep working: ownerless ⇒ private is
    /// already the stored value, so the write is a no-op that succeeds.
    /// Only the `shared` side of the invariant needs the guard.
    #[tokio::test]
    async fn patch_agent_visibility_allows_private_on_ownerless_row() {
        let inst = "eeeeeeee-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        ownership::lock(&state.agent_owners.clone().unwrap()).put(
            inst,
            OwnerRecord {
                owner_user_id: None,
                guests: vec![],
                visibility: Visibility::Private,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        let _ = patch_agent_visibility(
            State(state.clone()),
            Path(inst.into()),
            Json(serde_json::json!({ "visibility": "private" })),
        )
        .await
        .expect("private on an ownerless row is the invariant's resting state");
        let store = ownership::lock_shared(&state.agent_owners).unwrap();
        assert_eq!(
            store.get(inst).map(|r| r.visibility),
            Some(Visibility::Private)
        );
    }

    /// The claim path that makes the switch usable again: once an admin
    /// claims ownership, `shared` persists. This is the two-call recovery
    /// the error message points at.
    #[tokio::test]
    async fn visibility_shared_persists_once_an_owner_is_claimed() {
        let inst = "ffffffff-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        ownership::lock(&state.agent_owners.clone().unwrap()).put(
            inst,
            OwnerRecord {
                owner_user_id: None,
                guests: vec![],
                visibility: Visibility::Private,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        // 1. admin claims ownership (D7).
        let _ = patch_agent_owner(
            State(state.clone()),
            Path(inst.into()),
            Json(serde_json::json!({ "owner_user_id": "babea202" })),
        )
        .await
        .unwrap();
        // 2. now the visibility flip sticks.
        let _ = patch_agent_visibility(
            State(state.clone()),
            Path(inst.into()),
            Json(serde_json::json!({ "visibility": "shared" })),
        )
        .await
        .unwrap();
        let store = ownership::lock_shared(&state.agent_owners).unwrap();
        let rec = store.get(inst).unwrap();
        assert_eq!(rec.visibility, Visibility::Shared);
        assert_eq!(rec.owner_user_id.as_deref(), Some("babea202"));
    }

    /// ADR-087 D8: the permissions payload must carry the *data-state*
    /// answer, not just the permission answer. An admin passes
    /// `can_attribute` on an ownerless row, so a client that only reads
    /// that bit would enable the visibility switch and only learn from a
    /// 409 that the write is impossible. `can_set_visibility` is what
    /// lets the dialog disable the control up front.
    #[tokio::test]
    async fn permissions_reports_an_ownerless_row_as_unpublishable() {
        let inst = "aaaaaaaa-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        ownership::lock(&state.agent_owners.clone().unwrap()).put(
            inst,
            OwnerRecord {
                owner_user_id: None,
                guests: vec![],
                visibility: Visibility::Private,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        // multi_user with an admin caller — the mode where the ownerless
        // state is reachable at all. (`auth: None` would be Local mode,
        // where the tables are never evaluated, D8.)
        let admin = Extension(AuthContext {
            user_id: "root".into(),
            role: acowork_core::account::Role::Admin,
            as_user: None,
        });
        let Json(v) = get_agent_permissions(State(state.clone()), Path(inst.into()), Some(admin))
            .await
            .unwrap();
        assert_eq!(v["can_attribute"], serde_json::json!(true));
        assert_eq!(v["can_set_visibility"], serde_json::json!(false));
        assert_eq!(v["ownerless"], serde_json::json!(true));
    }

    /// The counterpart: an owned row is publishable, and the flag flips
    /// with the claim — the client's only signal to re-enable the switch.
    #[tokio::test]
    async fn permissions_reports_an_owned_row_as_publishable() {
        let inst = "bbbbbbbb-0b1b-4c2c-8d3d-9e4e5f6a7b8c";
        let state = state_with_installed(inst).await;
        ownership::lock(&state.agent_owners.clone().unwrap()).put(
            inst,
            OwnerRecord {
                owner_user_id: Some("babea202".into()),
                guests: vec![],
                visibility: Visibility::Private,
                created_at: chrono::Utc::now(),
                node_id: None,
            },
        );
        let admin = Extension(AuthContext {
            user_id: "root".into(),
            role: acowork_core::account::Role::Admin,
            as_user: None,
        });
        let Json(v) = get_agent_permissions(State(state.clone()), Path(inst.into()), Some(admin))
            .await
            .unwrap();
        assert_eq!(v["can_set_visibility"], serde_json::json!(true));
        assert_eq!(v["ownerless"], serde_json::json!(false));
    }

    /// Build an AppState with package `agent_id` installed as instance
    /// `instance_id`, and the MQTT agent registry seeded with a
    /// `DataEnvelope<AgentStatus{online}>` payload for that instance.
    ///
    /// ADR-073: the install table and the MQTT status registry are both
    /// keyed by INSTANCE identity, so the two inputs are passed
    /// separately — using one string for both is exactly the conflation
    /// the ADR removes. Handlers are therefore called with `instance_id`
    /// in the route path. `node_control` is left `None` so a handler that
    /// fails to short-circuit errors with "Node control plane
    /// unavailable" — proving the fast-path was hit.
    async fn state_with_registry_status(
        agent_id: &str,
        instance_id: &str,
        online: bool,
    ) -> AppState {
        let gw = Arc::new(RwLock::new(GatewayState::new(
            "/tmp/acowork-start-test-vault",
        )));
        {
            let mut g = gw.write().await;
            g.installed_agents.insert(
                instance_id.to_string(),
                AgentInfo {
                    instance_id: instance_id.to_string(),
                    agent_id: agent_id.to_string(),
                    version: "1.0.0".to_string(),
                    name: "Test Agent".to_string(),
                    install_path: format!("/tmp/pkg/{}", agent_id),
                    manifest: test_manifest(agent_id),
                    node_id: "local".to_string(),
                },
            );
        }
        let envelope = acowork_core::mqtt_proto::DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(
                    acowork_core::mqtt_proto::AgentStatus {
                        agent_id: agent_id.to_string(),
                        online,
                        instance_id: instance_id.to_string(),
                        node_id: "local".to_string(),
                        state: if online { acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady } else { acowork_core::mqtt_proto::AgentLifecycleState::Offline } as i32,
                        detail: String::new(),
                                            },
                ),
            ),
        };
        let payload = prost::Message::encode_to_vec(&envelope);
        let reg = crate::mqtt::agent_registry::new_shared_registry();
        reg.write().await.update_from_mqtt(
            &format!("acowork/agents/{}/status", instance_id),
            &payload,
        );

        let mut state = AppState::new(gw, Arc::new(HttpAuth::new(false)));
        state.agent_registry = Some(reg);
        state
    }

    #[tokio::test]
    async fn start_agent_short_circuits_when_already_online() {
        // ADR-055 idempotent fast-path: a node-hosted Runtime that is
        // already online (MQTT LWT) must return immediately instead of
        // paying a control/start round-trip that can hang up to
        // COMMAND_TIMEOUT (30s) while the Node recovers.
        let state = state_with_registry_status(
            "com.acowork.senior-engineer",
            INSTANCE_SENIOR_ENG,
            true,
        ).await;
        let result = start_agent(
            State(state),
            Path(INSTANCE_SENIOR_ENG.to_string()),
            Json(StartAgentRequest { dev_mode: false }),
        )
        .await;
        match result {
            Ok(resp) => {
                assert!(
                    resp.message.contains("already running"),
                    "message should say the agent is already running; got: {}",
                    resp.message
                );
            }
            Err(e) => panic!("expected idempotent 200, got error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn start_agent_not_short_circuited_when_offline() {
        // An offline agent has no online short-circuit either — the
        // node-control path must run.
        let state = state_with_registry_status(
            "com.acowork.senior-engineer",
            INSTANCE_SENIOR_ENG,
            false,
        ).await;
        let result = start_agent(
            State(state),
            Path(INSTANCE_SENIOR_ENG.to_string()),
            Json(StartAgentRequest { dev_mode: false }),
        )
        .await;
        match result {
            Err(e) => {
                assert!(
                    e.error.contains("Node control plane"),
                    "expected node-control path error; got: {}",
                    e.error
                );
            }
            Ok(resp) => panic!(
                "offline agent must NOT short-circuit; got idempotent 200: {}",
                resp.message
            ),
        }
    }

    #[tokio::test]
    async fn list_agents_reports_node_hosted_runtime_alive_from_mqtt() {
        // ADR-055: a node-hosted Runtime auto-tracked from its MQTT ready
        // signal has pid=0 (no local Gateway-side process). Liveness must
        // come from the MQTT LWT registry (`acowork/agents/{id}/status`
        // online), never from a process probe — the Gateway may be on a
        // different host entirely. This test pins that `alive` is the
        // registry verdict and is independent of `running_agents.pid`.
        let state =
            state_with_registry_status("com.acowork.senior-engineer", INSTANCE_SENIOR_ENG, true)
                .await;
        {
            let mut gw = state.gateway_state.write().await;
            gw.add_running(crate::gateway::state::RunningAgentInfo {
                instance_id: INSTANCE_SENIOR_ENG.to_string(),
                agent_id: "com.acowork.senior-engineer".to_string(),
                pid: 0,
                started_at: chrono::Utc::now(),
                workspace: String::new(),
                // ADR-075 D6: fallback anchor for the local node.
                node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                lifecycle: acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady,
                lifecycle_detail: String::new(),
                dev_mode: false,
                debug_state: crate::gateway::state::DebugState::Disabled,
                debug_port: None,
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
        }

        let Json(resp) = list_agents(
            State(state),
            None,
            Query(AgentListQuery {
                agent_id: None,
                node_id: None,
            }),
        )
        .await;
        let entry = resp
            .iter()
            .find(|a| a.agent_id == "com.acowork.senior-engineer")
            .expect("senior-engineer must be listed");
        assert!(
            entry.alive,
            "pid=0 node-hosted Runtime must report alive=true from the MQTT registry"
        );
        assert_eq!(entry.lifecycle, "sessions_ready", "lifecycle must mirror the tracked state");
    }

    /// ADR-073: a single package may be installed as MULTIPLE instances
    /// (same Node or cross-Node). `GET /api/agents` must return one
    /// entry per instance (distinct `instance_id`), and the
    /// `?agent_id=` / `?node_id=` filters must slice that list without
    /// collapsing duplicates.
    #[tokio::test]
    async fn list_agents_lists_same_package_instances_and_filters_by_package_and_node() {
        let state = test_state();
        {
            let mut gw = state.gateway_state.write().await;
            // Two instances of the same package, on two different nodes.
            for (inst, node) in [
                ("6a7a8a9a-0b1b-4c2c-8d3d-9e4e5f6a7b8c", "node-a"),
                ("7b8b9c0d-1c2c-4d3d-8e4e-af5f6a7b8c9d", "node-b"),
            ] {
                gw.add_installed(crate::gateway::state::AgentInfo {
                    instance_id: inst.to_string(),
                    agent_id: "com.foo.bar".to_string(),
                    version: "1.0.0".to_string(),
                    name: "Foo Bar".to_string(),
                    install_path: format!("/tmp/pkg/{}", inst),
                    manifest: test_manifest("com.foo.bar"),
                    node_id: node.to_string(),
                });
            }
            // A different package for the node filter cross-check.
            gw.add_installed(crate::gateway::state::AgentInfo {
                instance_id: INSTANCE_FOO_BAZ.to_string(),
                agent_id: "com.foo.baz".to_string(),
                version: "1.0.0".to_string(),
                name: "Foo Baz".to_string(),
                install_path: "/tmp/pkg/inst-b1".to_string(),
                manifest: test_manifest("com.foo.baz"),
                node_id: "node-b".to_string(),
            });
        }

        // Unfiltered: one entry per INSTANCE, never collapsed by agent_id.
        let Json(all) = list_agents(
            State(state.clone()),
            None,
            Query(AgentListQuery {
                agent_id: None,
                node_id: None,
            }),
        )
        .await;
        assert_eq!(all.len(), 3, "3 instances must be listed, none collapsed");
        let foo_instances: Vec<_> = all
            .iter()
            .filter(|a| a.agent_id == "com.foo.bar")
            .map(|a| a.instance_id.clone())
            .collect();
        // HashMap iteration order is unspecified — compare as sets.
        let mut sorted_instances = foo_instances.clone();
        sorted_instances.sort();
        assert_eq!(
            sorted_instances,
            vec!["6a7a8a9a-0b1b-4c2c-8d3d-9e4e5f6a7b8c".to_string(), "7b8b9c0d-1c2c-4d3d-8e4e-af5f6a7b8c9d".to_string()],
            "both instances of the same package must survive the list"
        );

        // Package view: ?agent_id=com.foo.bar → exactly its 2 instances.
        let Json(pkg_view) = list_agents(
            State(state.clone()),
            None,
            Query(AgentListQuery {
                agent_id: Some("com.foo.bar".to_string()),
                node_id: None,
            }),
        )
        .await;
        assert_eq!(pkg_view.len(), 2, "package filter returns both instances");
        assert!(
            pkg_view.iter().all(|a| a.agent_id == "com.foo.bar"),
            "package filter must not leak other packages"
        );

        // Node view: ?node_id=node-b → the 2 instances hosted there.
        let Json(node_view) = list_agents(
            State(state),
            None,
            Query(AgentListQuery {
                agent_id: None,
                node_id: Some("node-b".to_string()),
            }),
        )
        .await;
        assert_eq!(node_view.len(), 2, "node filter returns node-b instances");
        assert!(
            node_view.iter().all(|a| a.node_id == "node-b"),
            "node filter must not leak other nodes"
        );
    }

    #[test]
    fn test_agent_list_response_serialization() {

        let resp = AgentListResponse {
            instance_id: INSTANCE_WEATHER.to_string(),
            agent_id: "com.example.weather".to_string(),
            node_id: "local".to_string(),
            name: "Weather Agent".to_string(),
            display_name: None,
            role: None,
            avatar: None,
            builtin_avatar: Some("icon-05".to_string()),
            version: "1.0.0".to_string(),
            labels: vec!["weather".to_string()],
            alive: false,
            lifecycle: "offline".to_string(),
            lifecycle_detail: String::new(),
            dev_mode: false,
            debug_state: crate::gateway::state::DebugState::Disabled,
            debug_port: None,
            last_interaction_at: None,
            can_manage: None,
            can_use: None,
            visibility: "shared".to_string(),
            is_guest: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("com.example.weather"));
        assert!(json.contains("Weather Agent"));
        assert!(json.contains("icon-05"));
        // ADR-073: three-layer identity surfaces on every list entry.
        // The instance identity is a UUIDv4, never the package id.
        assert!(
            json.contains(&format!("\"instance_id\":\"{}\"", INSTANCE_WEATHER)),
            "instance_id must serialise; got: {}",
            json
        );
        assert!(
            json.contains("\"node_id\":\"local\""),
            "node_id must serialise; got: {}",
            json
        );
        // last_interaction_at is None and skipped on serialization.
        assert!(!json.contains("last_interaction_at"));
        // sleeping_at was removed (auto-sleep subsystem retired Sept 2026)
        // and must not appear in the wire format.
        assert!(!json.contains("sleeping_at"));
        // sleeping was also removed — must not appear either.
        assert!(!json.contains("\"sleeping\""));
        // ADR-104: package labels ride along on the list entry so the client
        // can render chips / filter without a per-agent detail fetch.
        assert!(
            json.contains("\"labels\":[\"weather\"]"),
            "labels must serialise on the list entry; got: {}",
            json
        );
        // ADR-048 follow-up: debug_state serialises as lowercase string.
        assert!(
            json.contains("\"debug_state\":\"disabled\""),
            "debug_state should serialise to lowercase \"disabled\"; got: {}",
            json
        );
    }

    #[test]
    fn test_debug_state_serialises_lowercase() {
        // Pin the exact wire shape the TypeScript `AgentStore.dev_mode_state`
        // mapping relies on. If we ever flip to SCREAMING_SNAKE_CASE the
        // frontend mapping breaks silently — this test makes the breakage
        // visible at PR time.
        assert_eq!(
            serde_json::to_string(&crate::gateway::state::DebugState::Disabled).unwrap(),
            "\"disabled\""
        );
        assert_eq!(
            serde_json::to_string(&crate::gateway::state::DebugState::Enabled).unwrap(),
            "\"enabled\""
        );
    }

    #[test]
    fn test_message_response_serialization() {
        let resp = MessageResponse {
            message: "Agent started".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("Agent started"));
    }

    /// An agent that declares no labels must not serialise an empty array —
    /// the client distinguishes "no labels" from "labels: []" only by
    /// presence, and an empty array in every payload is pure noise.
    #[test]
    fn test_agent_list_response_omits_empty_labels() {
        let resp = entry("i-1", "No Labels", false, None);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("labels"),
            "empty labels must be omitted; got: {}",
            json
        );
    }

    fn entry(id: &str, name: &str, alive: bool, ts: Option<&str>) -> AgentListResponse {
        AgentListResponse {
            // ADR-073: legacy identity shape (instance == package) — the
            // sort contract keys off `agent_id`, so keep both equal here.
            instance_id: id.to_string(),
            agent_id: id.to_string(),
            node_id: "local".to_string(),
            name: name.to_string(),
            display_name: None,
            role: None,
            avatar: None,
            builtin_avatar: None,
            version: "1.0.0".to_string(),
            labels: Vec::new(),
            alive,
            lifecycle: "offline".to_string(),
            lifecycle_detail: String::new(),
            dev_mode: false,
            debug_state: crate::gateway::state::DebugState::Disabled,
            debug_port: None,
            last_interaction_at: ts.map(|s| s.to_string()),
            can_manage: None,
            can_use: None,
            visibility: "shared".to_string(),
            is_guest: false,
        }
    }

    #[test]
    fn sort_groups_alive_before_dead() {
        let mut list = vec![
            entry("com.acowork.stopped1", "Stopped 1", false, Some("2026-06-18T10:00:00Z")),
            entry("com.acowork.running1", "Running 1", true, None),
            entry("com.acowork.stopped2", "Stopped 2", false, None),
            entry("com.acowork.running2", "Running 2", true, Some("2026-06-18T09:00:00Z")),
        ];
        sort_agent_list(&mut list);
        let order: Vec<&str> = list.iter().map(|a| a.agent_id.as_str()).collect();
        // Alive group first, within group time-bearing agents come before None ones;
        // same rule for the dead group.
        assert_eq!(
            order,
            vec![
                "com.acowork.running2",  // alive, has time
                "com.acowork.running1",  // alive, no time (last in alive group)
                "com.acowork.stopped1",  // dead, has time
                "com.acowork.stopped2",  // dead, no time (last overall)
            ]
        );
    }

    #[test]
    fn sort_orders_within_group_by_recency_then_name() {
        let mut list = vec![
            entry("com.acowork.zzz", "Zzz", true, None),
            entry("com.acowork.aaa", "Aaa", true, None),
            entry("com.acowork.bbb", "Bbb", true, Some("2026-06-18T01:00:00Z")),
            entry("com.acowork.ccc", "Ccc", true, Some("2026-06-18T05:00:00Z")),
        ];
        sort_agent_list(&mut list);
        let order: Vec<&str> = list.iter().map(|a| a.agent_id.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "com.acowork.ccc", // 05:00 (newest)
                "com.acowork.bbb", // 01:00
                "com.acowork.aaa", // None, name Aaa first
                "com.acowork.zzz", // None, name Zzz
            ]
        );
    }

    #[test]
    fn sort_falls_back_to_name_when_all_none() {
        let mut list = vec![
            entry("com.acowork.zzz", "Zzz", true, None),
            entry("com.acowork.aaa", "Aaa", true, None),
            entry("com.acowork.mmm", "Mmm", false, None),
        ];
        sort_agent_list(&mut list);
        let order: Vec<&str> = list.iter().map(|a| a.agent_id.as_str()).collect();
        // alive group first (alphabetical), then dead group
        assert_eq!(
            order,
            vec![
                "com.acowork.aaa",
                "com.acowork.zzz",
                "com.acowork.mmm",
            ]
        );
    }

    // ── 2026-09-07 incident follow-up: error layering contract ──────────

    /// The error layering table from `map_node_control_error`'s
    /// docstring is the contract the Desktop client depends on for
    /// retry / toast decisions. Pin every cell at once — a single
    /// regression in any of the four rows will make the Desktop
    /// either retry a non-retryable error (UX bug) or surface a
    /// recoverable one as a permanent failure (visibility bug).
    #[test]
    fn map_node_control_error_layers_transient_failures_with_retry_hint() {
        use crate::mqtt::node_control::NodeControlError;
        use acowork_core::error_codes::StructuredErrorCode;

        // TRANSIENT — timeout → 504 + HandshakeTimeout + retry
        let err = map_node_control_error(
            "start",
            "com.acowork.architect",
            NodeControlError::Timeout {
                request_id: "req-42".to_string(),
            },
        );
        assert_eq!(err.code, 504, "Timeout must map to HTTP 504");
        assert!(
            err.error.contains("req-42"),
            "human message must surface the request_id for log correlation; got: {}",
            err.error
        );
        let s = err.structured.expect("transient errors MUST carry a structured body");
        assert_eq!(s.code, StructuredErrorCode::HandshakeTimeout);
        let hint = s.retry_hint.as_ref().expect("transient must carry retry_hint");
        assert_eq!(hint.retry_after_ms, Some(1500));
        assert_eq!(hint.retry_count, 5);

        // TRANSIENT — MQTT publish hiccup → 503 + DependencyNotReady + retry
        let err = map_node_control_error(
            "stop",
            "com.acowork.architect",
            NodeControlError::Publish("client disconnected".to_string()),
        );
        assert_eq!(err.code, 503, "Publish must map to HTTP 503");
        let s = err.structured.expect("transient errors MUST carry a structured body");
        assert_eq!(s.code, StructuredErrorCode::DependencyNotReady);
        assert_eq!(s.retry_hint.as_ref().map(|r| r.retry_count), Some(5));
    }

    /// Structural failures (NodeOffline, CommandFailed, NoClient) must
    /// surface NO retry_hint — retrying would produce the same answer.
    /// The Desktop relies on the absence of `retry_hint` to render a
    /// permanent toast instead of a silent retry loop.
    #[test]
    fn map_node_control_error_layers_structural_failures_without_retry_hint() {
        use crate::mqtt::node_control::NodeControlError;
        use acowork_core::error_codes::StructuredErrorCode;

        // STRUCTURAL — node offline → 503 + DependencyNotReady, NO retry
        let err = map_node_control_error(
            "start",
            "com.acowork.architect",
            NodeControlError::NodeOffline {
                node_id: "node-x".to_string(),
            },
        );
        assert_eq!(err.code, 503);
        let s = err.structured.expect("structural errors still carry a body for classification");
        assert_eq!(s.code, StructuredErrorCode::DependencyNotReady);
        assert!(
            s.retry_hint.is_none(),
            "NodeOffline is structural — no retry hint; got: {:?}",
            s.retry_hint
        );
        assert!(
            s.phase_detail
                .as_deref()
                .unwrap_or("")
                .contains("node-x"),
            "phase_detail must identify the offending node for the operator toast"
        );

        // STRUCTURAL — broker disabled (NoClient) → 503, NO retry
        let err = map_node_control_error(
            "start",
            "com.acowork.architect",
            NodeControlError::NoClient,
        );
        assert_eq!(err.code, 503);
        let s = err.structured.expect("structural errors still carry a body for classification");
        assert_eq!(s.code, StructuredErrorCode::DependencyNotReady);
        assert!(
            s.retry_hint.is_none(),
            "NoClient is structural — no retry hint"
        );

        // STRUCTURAL — node rejected (CommandFailed) → 422 + OperationExpired, NO retry
        let err = map_node_control_error(
            "start",
            "com.acowork.architect",
            NodeControlError::CommandFailed {
                agent_id: "com.acowork.architect".to_string(),
                message: "agent_not_installed".to_string(),
            },
        );
        assert_eq!(err.code, 422, "CommandFailed must map to HTTP 422");
        let s = err.structured.expect("structural errors still carry a body for classification");
        assert_eq!(s.code, StructuredErrorCode::OperationExpired);
        assert!(
            s.retry_hint.is_none(),
            "CommandFailed is structural — no retry hint"
        );
        assert!(
            s.phase_detail
                .as_deref()
                .unwrap_or("")
                .contains("agent_not_installed"),
            "phase_detail must surface the node's reason to the operator"
        );
    }

    /// The `op_name` parameter must be threaded into the human message
    /// and the structured `phase_detail`. Otherwise the operator toast
    /// cannot tell whether a timeout was a start / stop / restart-debug
    /// — and the log correlation breaks.
    #[test]
    fn map_node_control_error_threads_op_name_into_message_and_phase_detail() {
        use crate::mqtt::node_control::NodeControlError;
        for (op_name, expected_substr) in [
            ("start", "start"),
            ("stop", "stop"),
            ("restart-debug", "restart-debug"),
        ] {
            let err = map_node_control_error(
                op_name,
                "com.acowork.architect",
                NodeControlError::Publish("boom".to_string()),
            );
            assert!(
                err.error.starts_with(op_name),
                "human message must start with op_name={op_name}; got: {}",
                err.error
            );
            let s = err.structured.expect("body");
            assert!(
                s.phase_detail
                    .as_deref()
                    .unwrap_or("")
                    .contains(op_name),
                "phase_detail must contain op_name={op_name}; got: {:?}",
                s.phase_detail
            );
            let _ = expected_substr;
        }
    }

    /// The desync reconcile path: the broker says the agent is online
    /// (registry online) but `running_agents` has no entry (the
    /// 2026-09-07 symptom). The fast-path must:
    ///   1. Detect the desync.
    ///   2. Trigger `reconcile_running_agents`.
    ///   3. Return idempotent 200 with `(reconciled)` suffix —
    ///      NOT the silent short-circuit of the original bug.
    /// We seed `node_control = None` so the fallback path errors on
    /// the node-control plane, which is what proves the fast-path
    /// hit the desync branch.
    #[tokio::test]
    async fn start_agent_reconciles_when_registry_online_but_running_agents_missing() {
        // Pre-condition: registry says online (the broker has the
        // retained snapshot), but `running_agents` is empty.
        let state = state_with_registry_status(
            "com.acowork.architect",
            INSTANCE_ARCHITECT,
            true,
        )
        .await;
        // Sanity: registry online, no entry.
        {
            let reg = state.agent_registry.as_ref().expect("registry seeded");
            assert!(
                reg.read().await.is_online(INSTANCE_ARCHITECT),
                "preflight: registry says online"
            );
        }
        {
            let gw = state.gateway_state.read().await;
            assert!(
                !gw.is_running(INSTANCE_ARCHITECT),
                "preflight: no running_agents entry — DESYNC"
            );
        }

        // Fire the start. Two legitimate outcomes prove the desync
        // branch ran (as opposed to the pre-bug silent 200):
        //   - Ok: the on-demand reconcile restored the entry from the
        //     broker view (the helper seeds `installed_agents`, so the
        //     reconcile knows the install path) and the response is the
        //     idempotent 200 carrying the explicit "(reconciled)"
        //     marker — self-heal succeeded, nothing was hidden.
        //   - Err: the reconcile could not restore (e.g. agent not
        //     installed) and the fall-through reached the node-control
        //     plane, which surfaces a structured 503 (node_control is
        //     None in this state). Any non-200 proves the fast-path
        //     refused to lie about the desync.
        //
        // The one outcome that must NEVER happen is an Ok whose
        // message lacks the "(reconciled)" marker — that would be the
        // silent short-circuit of the original bug.
        let result = start_agent(
            State(state),
            Path(INSTANCE_ARCHITECT.to_string()),
            Json(StartAgentRequest { dev_mode: false }),
        )
        .await;
        match result {
            Ok(resp) => assert!(
                resp.message.contains("(reconciled)"),
                "self-heal 200 must be marked '(reconciled)'; got: {}",
                resp.message
            ),
            Err(e) => {
                // The post-reconcile fall-through surfaces a structured
                // 503 (Node control plane unavailable) — any non-200
                // proves the desync branch ran.
                assert!(
                    e.error.contains("Node control plane"),
                    "expected post-desync node-control error; got: {}",
                    e.error
                );
            }
        }
    }

    /// When the registry says online AND `running_agents` already
    /// has the entry (the steady-state happy path), the fast-path
    /// returns the idempotent 200 immediately — no round-trip to
    /// the node. This is the primary UX win of the fast-path, and
    /// the case the original bug was observed on.
    #[tokio::test]
    async fn start_agent_fast_path_returns_idempotent_200_when_views_agree() {
        // Pre-condition: registry online AND entry present.
        let state =
            state_with_registry_status("com.acowork.architect", INSTANCE_ARCHITECT, true)
                .await;
        {
            let mut gw = state.gateway_state.write().await;
            gw.add_running(crate::gateway::state::RunningAgentInfo {
                instance_id: INSTANCE_ARCHITECT.to_string(),
                agent_id: "com.acowork.architect".to_string(),
                pid: 0,
                started_at: chrono::Utc::now(),
                workspace: String::new(),
                // ADR-075 D6: fallback anchor for the local node.
                node_id: acowork_core::node::LOCAL_NODE_ID.to_string(),
                lifecycle: acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady,
                lifecycle_detail: String::new(),
                dev_mode: false,
                debug_state: crate::gateway::state::DebugState::Disabled,
                debug_port: None,
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
        }
        let result = start_agent(
            State(state),
            Path(INSTANCE_ARCHITECT.to_string()),
            Json(StartAgentRequest { dev_mode: false }),
        )
        .await;
        match result {
            Ok(resp) => assert!(
                resp.message.contains("already running"),
                "fast-path must surface an idempotent 'already running' message; got: {}",
                resp.message
            ),
            Err(e) => panic!("expected idempotent 200, got error: {}", e.error),
        }
    }

    // ── Declarative ensure (ADR-073) ────────────────────────────────

    /// Seed one installed instance into the Gateway's view.
    fn insert_instance(gw: &mut GatewayState, agent_id: &str, instance_id: &str, node_id: &str) {
        gw.installed_agents.insert(
            instance_id.to_string(),
            AgentInfo {
                instance_id: instance_id.to_string(),
                agent_id: agent_id.to_string(),
                version: "1.0.0".to_string(),
                name: agent_id.to_string(),
                install_path: format!("/tmp/pkg/{instance_id}"),
                manifest: test_manifest(agent_id),
                node_id: node_id.to_string(),
            },
        );
    }

    /// The ensure fast path asks an existence question about one package
    /// on ONE node — the answer is an instance identity (ADR-073).
    #[tokio::test]
    async fn resolve_ensure_matches_package_and_node() {
        let mut gw = GatewayState::new("/tmp/acowork-resolve-ensure-vault");
        insert_instance(&mut gw, "com.test.weather", INSTANCE_WEATHER, "local");
        insert_instance(&mut gw, "com.test.weather", INSTANCE_FOO_BAZ, "node-b");

        assert_eq!(
            resolve_ensure(&gw, "com.test.weather", "local"),
            EnsureResolution::Present(INSTANCE_WEATHER.to_string())
        );
        assert_eq!(
            resolve_ensure(&gw, "com.test.weather", "node-b"),
            EnsureResolution::Present(INSTANCE_FOO_BAZ.to_string())
        );
        // A different node holds nothing for this package — the request
        // must be dispatched there rather than satisfied from elsewhere.
        assert_eq!(
            resolve_ensure(&gw, "com.test.weather", "node-c"),
            EnsureResolution::Dispatch
        );
        assert_eq!(
            resolve_ensure(&gw, "com.test.absent", "local"),
            EnsureResolution::Dispatch
        );
    }

    /// ADR-073: several instances of one package on one node are legal —
    /// any of them satisfies the existence claim, and the answer must not
    /// change between calls (hash-map order is not stable).
    #[tokio::test]
    async fn resolve_ensure_is_stable_when_a_package_has_several_instances() {
        let mut gw = GatewayState::new("/tmp/acowork-resolve-ensure-multi-vault");
        insert_instance(&mut gw, "com.acowork.senior-engineer", INSTANCE_SENIOR_ENG, "local");
        insert_instance(&mut gw, "com.acowork.senior-engineer", INSTANCE_ARCHITECT, "local");

        let first = resolve_ensure(&gw, "com.acowork.senior-engineer", "local");
        let expected = EnsureResolution::Present(
            [INSTANCE_ARCHITECT, INSTANCE_SENIOR_ENG]
                .iter()
                .min()
                .expect("two candidates")
                .to_string(),
        );
        assert_eq!(first, expected);
        for _ in 0..8 {
            assert_eq!(resolve_ensure(&gw, "com.acowork.senior-engineer", "local"), expected);
        }
    }
}
