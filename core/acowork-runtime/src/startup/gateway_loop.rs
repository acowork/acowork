//! Phase D: announce ready and enter the main Gateway loop.
//!
//! ADR-033: MQTT-only mode (gRPC removed per ADR-034 §8 Phase 2).
//! ADR-034 §8 Phase 2-1: single dispatch table for InboundMessage.
//! ADR-034 §8 Phase 2-2: gRPC path removed.

use crate::cli::LogReloadHandle;
use crate::config::RuntimeConfig;
use crate::error::Result;
use crate::startup::context::{AgentBootContext, SessionBootContext};
use crate::startup::subsystems::SubsystemHandles;

use std::sync::Arc;

/// Phase D: notify Gateway that the agent is ready, then run the message loop.
///
/// This is the last phase of the startup sequence.  It runs until the
/// MQTT connection is closed or a fatal error occurs.
///
/// ADR-034 §8 Phase 2-2: gRPC path removed; `log_reload_handle` is no
/// longer used (was only consumed by `run_gateway_loop` for gRPC-era
/// log-level push from Gateway).
pub(crate) async fn phase_d_run(
    ctx: &mut AgentBootContext,
    session_ctx: SessionBootContext,
    handles: SubsystemHandles,
    config: &RuntimeConfig,
    _log_reload_handle: Option<LogReloadHandle>,
) -> Result<()> {
    let _span = tracing::info_span!("startup_phase_d").entered();

    let SessionBootContext {
        session_manager,
        committed_lines: _committed_lines,
    } = session_ctx;

    let SubsystemHandles {
        chunk_relay,
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
    } = handles;

    // ADR-033: MQTT control → session dispatch channel.
    let (mqtt_dispatch_tx, mqtt_dispatch_rx) = tokio::sync::mpsc::unbounded_channel();

    // ADR-033: Forward Runtime HTTP dispatch messages to the MQTT dispatch channel.
    if let Some(http_rx) = ctx.http_dispatch_rx.take() {
        let tx = mqtt_dispatch_tx.clone();
        tokio::spawn(async move {
            let mut rx = http_rx;
            while let Some(msg) = rx.recv().await {
                let _ = tx.send(msg);
            }
            tracing::info!("Runtime HTTP dispatch channel closed");
        });
    }

    // ADR-034 §8 Phase 2-1: ControlAction → InboundMessage via single mapper.
    // Replaces the legacy 11-arm match that wrapped every command in
    // SystemNotification { notification_type: ... }.
    let _mqtt_handle = ctx.control_rx.take().map(|ctrl_rx| {
        let tx = mqtt_dispatch_tx.clone();
        tokio::spawn(async move {
            let mut rx = ctrl_rx;
            while let Some((topic, payload)) = rx.recv().await {
                match crate::mqtt::control_handler::parse_control_payload(&topic, &payload) {
                    Some(action) => {
                        if let Some((session_id, msg)) = control_action_to_inbound(action)
                            && tx.send((session_id, msg)).is_err()
                        {
                            tracing::warn!(topic, "MQTT dispatch channel closed");
                        }
                    }
                    None => {
                        tracing::debug!(topic, "Failed to parse MQTT control payload");
                    }
                }
            }
        })
    });

    // ADR-034 §8 Phase 2-2: gRPC path removed. MQTT client is mandatory.
    if ctx.mqtt_client.is_none() {
        return Err(crate::error::RuntimeError::Config(
            "Phase D entered without MQTT client (gRPC path removed per ADR-034 §8 Phase 2)".into(),
        ));
    }

    tracing::info!("All subsystems ready, announcing via MQTT");
    // Lifecycle publisher: used by dispatch_inbound to push SessionCreated /
    // SessionDeleted events to the MQTT broker so the Desktop (and any other
    // subscriber) can update its session list without polling. Cloned cheaply.
    let lifecycle_publisher: crate::mqtt::MqttChunkPublisher =
        if let Some(ref mqtt) = ctx.mqtt_client {
            // Status first — `online` is the "TCP connection + AgentRegistry sees
            // us" signal that the Gateway uses for `online` tracking.
            // The `ready=true` signal was already published at the end of Phase A
            // (see `agent_init.rs::phase_a_init_agent`) so the Gateway could
            // start reverse-proxying Phase-A-ready endpoints (`/workspaces`,
            // `/workspaces/tree`) without waiting on Phase B/C and the workspace
            // FS watcher scan over potentially-large workspace roots. Re-publishing
            // `ready=true` here would be a no-op for the Gateway registry, but
            // skipping it keeps a single source of truth for the lifecycle signal.
            let _ = mqtt.publish_status(true).await;
            tracing::info!(
                "Phase D lifecycle publisher ready for agent={}",
                ctx.agent_id
            );
            crate::mqtt::MqttChunkPublisher::from_runtime_client(mqtt)
        } else {
            // Unreachable: checked above.
            return Err(crate::error::RuntimeError::Config(
                "lifecycle publisher: MQTT client disappeared".into(),
            ));
        };

    // Companion to `lifecycle_publisher`: re-publishes the retained
    // `acowork/agents/{id}/config` snapshot. The MCP-reconnect
    // branch in `mqtt_only_loop` calls this after
    // `connect_mcp_with_reconcile_and_filter` finishes so the
    // Desktop Tools panel refreshes the per-tool list / chevron
    // count without a tab remount (the previous workaround).
    let config_publisher: crate::mqtt::MqttAgentConfigPublisher =
        if let Some(ref mqtt) = ctx.mqtt_client {
            crate::mqtt::MqttAgentConfigPublisher::from_runtime_client(mqtt)
        } else {
            // Unreachable: same guard as `lifecycle_publisher` above.
            return Err(crate::error::RuntimeError::Config(
                "config publisher: MQTT client disappeared".into(),
            ));
        };

    // ADR-099: per-chat router state (one slot per Chat session). Lives as
    // long as the Runtime process: dropping it would only cost one rebuilt
    // history window per chat, but the per-chat serialization is what keeps
    // two turns of one chat from racing on the same anchor.
    let chat_sessions = Arc::new(crate::agent::chat_router::ChatSessionManager::new());

    // The Runtime's half of the acceptance protocol: every inbound Intent it
    // routes is answered back to the Gateway that published it, so a message
    // this process cannot route stops looking delivered (see
    // `agent::intent_accept`).
    let acceptance_reporter = crate::agent::intent_accept::AcceptanceReporter::new(
        config.agent_instance_id.clone(),
        config.gateway_host.as_deref().unwrap_or("127.0.0.1"),
        config.mqtt_password.clone().unwrap_or_default(),
    );

    let result = mqtt_only_loop(
        &session_manager,
        &chat_sessions,
        &lifecycle_publisher,
        &config_publisher,
        mqtt_dispatch_rx,
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
        ctx.mcp_notifier.subscribe(),
        ctx.identity_update_rx.take(),
        ctx.provider_update_rx.take(),
        ctx.embedding_update_rx.take(),
        ctx.lsps_update_rx.take(),
        &config.work_dir,
        config.mqtt_password.as_deref(),
        &acceptance_reporter,
    )
    .await;

    if let Some(handle) = chunk_relay {
        let _ = handle.await;
    }
    result
}

/// ADR-034 §8 Phase 2-1: ControlAction → InboundMessage single mapper.
///
/// ADR-076 §决策 4: after the second wave of the HTTP migration this
/// mapper is nearly empty. The MQTT control channel now carries only
/// `Intent` (Gateway → Runtime: cron triggers, cross-agent messaging).
/// Every **user-initiated** action — chat, stop, continue, approval,
/// question_answer, cancel_tool, compress, and the session lifecycle
/// before them — arrives over the Gateway's authenticated HTTP API and
/// is injected directly into the dispatch channel by
/// `http::server::dispatch_session_action`, so it never passes through
/// `ControlAction` at all.
///
/// The mapper is exhaustive over `ControlAction` — adding a variant in
/// `control_handler.rs` triggers a compile error here, which is the point.
fn control_action_to_inbound(
    action: crate::mqtt::control_handler::ControlAction,
) -> Option<(String, crate::agent::inbound::InboundMessage)> {
    use crate::agent::inbound::{InboundMessage, IntentEnvelope};
    use crate::mqtt::control_handler::ControlAction;

    match action {
        // ── System ─────────────────────────────────────────────────────
        // Uses the empty session id to signal `dispatch_inbound` to route
        // through `session_manager` (system-level) rather than a session
        // task.
        ControlAction::IntentReceived {
            from,
            action,
            params_json,
            message_id,
            conversation_id,
            source_agent_session_id,
            source_chat_session_id,
            repo,
            session_config_json,
            kind,
            in_reply_to,
            status,
            has_human,
            chat_attachment_ids,
        } => {
            let params: serde_json::Value =
                serde_json::from_str(&params_json).unwrap_or(serde_json::json!({}));
            Some((
                // Route to the sender-designated session when present
                // (design 27 §4.2.3 source_agent_session_id); otherwise the
                // empty id keeps the system-level path (cron).
                source_agent_session_id,
                InboundMessage::IntentMessage(Box::new(IntentEnvelope {
                    from,
                    action,
                    params,
                    message_id,
                    conversation_id,
                    source_chat_session_id,
                    repo,
                    session_config_json,
                    kind,
                    in_reply_to,
                    status,
                    has_human,
                    chat_attachment_ids,
                })),
            ))
        }
    }
}

/// ADR-033: MQTT-only gateway loop — no gRPC dependency.
///
/// Listens for MQTT control dispatch and MCP events. The actual
/// chat loop runs in session tasks; this loop just routes messages.
#[allow(clippy::too_many_arguments)]
async fn mqtt_only_loop(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    chat_sessions: &Arc<crate::agent::chat_router::ChatSessionManager>,
    lifecycle_publisher: &crate::mqtt::MqttChunkPublisher,
    config_publisher: &crate::mqtt::MqttAgentConfigPublisher,
    mut mqtt_dispatch_rx: tokio::sync::mpsc::UnboundedReceiver<(
        String,
        crate::agent::inbound::InboundMessage,
    )>,
    mut mcp_startup_rx: Option<
        tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    >,
    mcp_runtime_tx: tokio::sync::mpsc::Sender<crate::tools::mcp_manager::McpConnectResult>,
    mut mcp_runtime_rx: tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    mut mcp_config_rx: tokio::sync::watch::Receiver<()>,
    // ADR-042: forwards `acowork/global/user_profile` retained updates from
    // the MQTT event loop to SessionManager. None when running in tests or
    // when MQTT is unavailable (Standalone mode).
    mut identity_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<acowork_core::protocol::UserProfile>,
    >,
    mut provider_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::ProviderUpdate>,
    >,
    // ADR-033: forwards `acowork/global/embedding_models` retained
    // updates to SessionManager::handle_embedding_config_update so
    // sessions rebuild their embedding provider in-place.
    mut embedding_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::EmbeddingUpdate>,
    >,
    // ADR-055 §6.7 (Phase 4): forwards the node's LSP relay state
    // changes to SessionManager. None when running without `--node-id`
    // (standalone / Gateway-spawned) or when MQTT is unavailable.
    mut lsps_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::LspRelayUpdate>,
    >,
    work_dir: &str,
    // ADR-076: the node_token the Node injected at spawn (carried on
    // `--mqtt-password`). Threaded in only so the MCP hot-reload branch
    // below can authenticate to Gateway-hosted MCP (pm / doc) the same
    // way the startup connect does. `None` standalone.
    node_token: Option<&str>,
    // Design 27 §4.5 / plan D3: tells the Gateway whether each inbound
    // Intent could be routed (`POST /api/intents/{id}/accept`). Without it a
    // routing failure is visible only in this process's own log, while the
    // sender's mailbox says `delivered`.
    acceptance: &crate::agent::intent_accept::AcceptanceReporter,
) -> Result<()> {
    tracing::info!("MQTT-only gateway loop started");
    let work_dir = std::path::PathBuf::from(work_dir);

    loop {
        tokio::select! {
            // ── ADR-034 §8 Phase 2-1: single dispatch table ────────────
            dispatch_result = mqtt_dispatch_rx.recv() => {
                match dispatch_result {
                    Some((session_id, msg)) => {
                        // A request owes the Gateway a routing verdict: the
                        // publish proved only that the bytes arrived. Reported
                        // off the loop (`spawn_report`) so a slow Gateway can
                        // never delay the next inbound message.
                        let receipt = match &msg {
                            crate::agent::inbound::InboundMessage::IntentMessage(env) => {
                                crate::agent::intent_accept::acceptance_target(env)
                            }
                            _ => None,
                        };
                        let outcome = dispatch_inbound(
                            session_manager,
                            chat_sessions,
                            lifecycle_publisher,
                            session_id,
                            msg,
                            &work_dir,
                            node_token.unwrap_or(""),
                        ).await;
                        if let Some(message_id) = receipt {
                            match &outcome {
                                Ok(()) => acceptance.spawn_report(message_id, true, String::new()),
                                Err(e) => acceptance.spawn_report(message_id, false, e.to_string()),
                            }
                        }
                        if let Err(e) = outcome {
                            tracing::warn!(error = %e, "MQTT dispatch failed");
                        }
                    }
                    None => break, // channel closed
                }
            }

            // Initial MCP auto-connect result
            mcp_result = async {
                match &mut mcp_startup_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some((registry, wrappers, specs, failures)) = mcp_result {
                    session_manager.lock().await.apply_mcp_connection_result(
                        registry, wrappers, specs, failures,
                    );
                }
                mcp_startup_rx = None;
            }

            // Runtime MCP connect result
            mcp_runtime_result = mcp_runtime_rx.recv() => {
                if let Some((registry, wrappers, specs, failures)) = mcp_runtime_result {
                    session_manager.lock().await.apply_mcp_connection_result(
                        registry, wrappers, specs, failures,
                    );
                    // MCP reconcile+filter (ADR-069) is now persisted
                    // into `agent_mcp_tools.json` — re-publish the
                    // retained `acowork/agents/{id}/config` so the
                    // Desktop Tools panel refreshes `/mcp-tools` and
                    // shows the chevron + per-tool list without a tab
                    // remount. Source of truth is the on-disk file
                    // (use case result for the patch path, freshly
                    // serialized here for the MCP path — equivalent on
                    // the receiver).
                    if let Ok(Some(cfg)) =
                        crate::agent_config::load_agent_config(std::path::Path::new(&work_dir))
                    {
                        let config_json =
                            serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
                        config_publisher.publish(config_json).await;
                    }
                }
            }

            // MCP config change notification
            _ = mcp_config_rx.changed() => {
                tracing::info!("MCP config change — reconnecting MCP servers (background)");
                let merged = crate::agent_config::load_active_mcp_configs(
                    std::path::Path::new(&work_dir),
                );
                let tx = mcp_runtime_tx.clone();
                let reload_work_dir = std::path::PathBuf::from(&work_dir);
                // Owned so the spawned task outlives the `&str` borrow.
                let node_token = node_token.map(str::to_string);
                tokio::spawn(async move {
                    // ADR-069: hot reload goes through the same
                    // reconcile+filter path as startup — reconcile
                    // agent_mcp_tools.json against the live tools/list,
                    // then expose only enabled tools.
                    let (registry, wrappers, specs, failures) =
                        crate::tools::mcp_manager::connect_mcp_with_reconcile_and_filter(
                            &reload_work_dir,
                            &merged,
                            node_token.as_deref(),
                        )
                        .await;
                    let _ = tx.send((registry, wrappers, specs, failures)).await;
                });
            }

            // ADR-042: `acowork/global/user_profile` retained update →
            // SessionManager::update_user_identity → broadcast to all sessions.
            identity = async {
                match identity_update_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(profile) = identity {
                    tracing::info!(
                        user_id = %profile.user_id,
                        language = %profile.language,
                        "Applying acowork/global/user_profile update to SessionManager"
                    );
                    session_manager.lock().await.update_user_identity(Some(profile));
                }
            }

            // `acowork/global/providers` retained update →
            // SessionManager::update_global_provider_list → broadcast to all sessions.
            provider = async {
                match provider_update_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(update) = provider {
                    tracing::info!(
                        provider_count = update.provider_list.len(),
                        version = update.provider_list_version,
                        key_count = update.provider_key_vault.len(),
                        default_compact_count = update.default_compact_models.len(),
                        "Applying acowork/global/providers update to SessionManager"
                    );
                    session_manager.lock().await.update_global_provider_list(
                        update.provider_list,
                        update.provider_list_version,
                        update.provider_key_vault,
                        update.default_compact_models,
                    );
                }
            }

            // ADR-033: `acowork/global/embedding_models` retained update →
            // SessionManager::handle_embedding_config_update → broadcast
            // UpdateEmbedConfig to every session so they rebuild their
            // embedding provider in-place (embed sidecar became ready, or
            // the active model/dimension switched).
            embedding = async {
                match embedding_update_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(update) = embedding {
                    tracing::info!(
                        endpoint = %update.endpoint,
                        model_id = %update.model_id,
                        dimension = update.dimension,
                        "Applying acowork/global/embedding_models update to SessionManager"
                    );
                    session_manager
                        .lock()
                        .await
                        .handle_embedding_config_update(
                            update.endpoint,
                            update.model_id,
                            update.dimension,
                            update.provider_id,
                            update.api_key,
                        );
                }
            }

            // ADR-055 §6.7 (Phase 4): node LSP relay state change →
            // SessionManager::handle_lsp_relay_update → register /
            // unregister the `codebase` tool in every session.
            lsps = async {
                match lsps_update_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(update) = lsps {
                    tracing::info!(
                        endpoint = update.endpoint.as_deref().unwrap_or("<none>"),
                        "Applying node LSP relay update to SessionManager"
                    );
                    session_manager
                        .lock()
                        .await
                        .handle_lsp_relay_update(update.endpoint);
                }
            }
        }
    }

    tracing::info!("MQTT-only gateway loop ended");
    Ok(())
}

/// ADR-034 §8 Phase 2-1: single dispatch table for `InboundMessage`.
///
/// One arm per `InboundMessage` variant — no notification_type string
/// parsing, no SystemNotification re-mapping for control commands.
///
/// Routes:
/// - `UserMessage` / `Stop` / `ContinueExecution` / `ApprovalDecision` /
///   `QuestionAnswer` / `UserOperation` / `IntentMessage` → session task inbox
/// - `CompressAction` → `SessionMessage::CompressAction(CompressionAction)`
///   with explicit CompressType i32 → CompressionAction mapping
///   (1=SUMMARY → CompressSummary; ADR-052 removed TOOL_RESULTS because
///   tool-result compression is retired)
/// - `SystemNotification` → legacy fallback (Phase 7: no longer produced by control path)
///
/// Session lifecycle (create / open / close / delete / retitle / model /
/// reasoning / workspace) has no arm here on purpose: those are HTTP-only
/// (ADR-076 §决策 4), because an MQTT control message carries no identity
/// for the Runtime to authorize. The proto fields are deleted too, so the
/// capability is unrepresentable rather than checked.
async fn dispatch_inbound(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    chat_sessions: &Arc<crate::agent::chat_router::ChatSessionManager>,
    lifecycle_publisher: &crate::mqtt::MqttChunkPublisher,
    session_id: String,
    msg: crate::agent::inbound::InboundMessage,
    work_dir: &std::path::Path,
    node_token: &str,
) -> crate::error::Result<()> {
    use crate::agent::inbound::InboundMessage;
    use crate::agent::loop_::CompressionAction;
    use crate::agent::session::SessionMessage;
    use crate::cancellation::{CancellationReason, StopSource};
    use crate::error::RuntimeError;

    // ── System-level (session_id empty) ─────────────────────────────
    if session_id.is_empty() {
        return match msg {
            InboundMessage::IntentMessage { .. } => {
                // Design 27 §3.2: an Intent with no designated session is
                // still addressed to an intent context, not to "the system".
                // Route it through the receiver-private registry (chat /
                // conv / one-shot message key) so it gets an execution
                // session instead of being dropped.
                route_intent_inbound(
                    session_manager,
                    chat_sessions,
                    "",
                    msg,
                    work_dir,
                    node_token,
                )
                .await
            }
            // ── ADR-052 §3.5: agent-level config mutations are
            //    GLOBAL, not per-session. Route through SessionManager
            //    so the shared template, the runtime_overrides cache,
            //    every active SessionTask's ContextBuilder, and every
            //    mid-execution AgentLoop all see the change in one
            //    shot. The session-level arm below mirrors this for
            //    defensive uniformity (any per-session producer gets
            //    the same global policy).
            //
            //    The HTTP layer (`http::server::dispatch_agent_level_config`)
            //    produces system-level messages; MQTT producers should
            //    too. Either way, the policy at this table is
            //    "config mutations are agent-scoped".
            InboundMessage::UserOperation(crate::agent::inbound::UserOp::UpdateRuntimeConfig(
                overrides,
            )) => {
                let failed = session_manager
                    .lock()
                    .await
                    .apply_runtime_config_override(&overrides);
                if !failed.is_empty() {
                    tracing::warn!(
                        failed_sessions = failed.len(),
                        "system-level UpdateRuntimeConfig: some sessions missed the broadcast (likely closed)"
                    );
                }
                Ok(())
            }
            InboundMessage::UpdateBuiltinTools { entries } => {
                session_manager
                    .lock()
                    .await
                    .apply_builtin_tools_enabled(&entries);
                Ok(())
            }
            // ADR-063 §3.7.6: main-dialog system prompt hot-reload. Same
            // agent-scoped policy as UpdateRuntimeConfig / UpdateBuiltinTools:
            // route through SessionManager so the shared template AND every
            // active session's ContextBuilder are updated in one shot.
            InboundMessage::UpdateSystemPrompt { system_prompt } => {
                session_manager
                    .lock()
                    .await
                    .apply_system_prompt(&system_prompt);
                Ok(())
            }
            // ADR-094: Progressive-mode skills summary hot-reload.
            // Same agent-scoped policy as `UpdateSystemPrompt` —
            // the skills summary is part of the agent template, not
            // per-session, so the SessionManager owns the broadcast.
            // `None` clears the section (Manual mode hot-toggle).
            InboundMessage::UpdateSkillRegistry { registry } => {
                session_manager.lock().await.apply_skill_registry(registry);
                Ok(())
            }
            InboundMessage::UpdateSkillInstructions { instructions } => {
                session_manager
                    .lock()
                    .await
                    .apply_skill_instructions(instructions.as_deref());
                Ok(())
            }
            other => Err(RuntimeError::Config(format!(
                "system-level dispatch: unsupported variant {:?}",
                std::mem::discriminant(&other)
            ))),
        };
    }

    // ── Session-level: single dispatch table ─────────────────────────
    match msg {
        // ① User chat message → session task inbox
        InboundMessage::UserMessage(text) => {
            forward_to_session_inbound(
                session_manager,
                lifecycle_publisher,
                &session_id,
                "user_message",
                work_dir,
                InboundMessage::UserMessage(text),
            )
            .await
        }

        // ② Stop signal → session task inbox
        InboundMessage::Stop { reason } => {
            // ADR-044 §4.5: flip the session's **current request's** `CancelHandle`
            // *before* forwarding so any currently-blocked `tokio::select!`
            // branch on the session wakes immediately. The handle's level-
            // triggered `Notify` + `AtomicU8` state means the cancel takes
            // effect on the next checkpoint even if the session task is
            // mid-await on something *other* than a Notify — notably inside
            // `provider.chat_stream().await` while establishing a TCP/TLS
            // connection (the TTFT stop bug, ADR §1.3, §4.4).
            //
            // `session_manager.lock().await.cancel_handle(&session_id)` reads through the
            // `Arc<parking_lot::Mutex<CancelHandle>>` slot, so we always
            // target the *current* request's generation — never a stale
            // clone from session creation time (the §4.5 guarantee).
            //
            // We deliberately call `cancel()` regardless of whether the
            // session is currently registered in `cancel_handles`:
            // `None` simply means the session has already been evicted or
            // closed, in which case the cancel is a no-op (no panic, just a
            // debug log) and the subsequent `forward_to_session_inbound`
            // call will surface the same eviction as a structured error.
            // NOTE(deadlock): the cancel handle and the agent_id MUST be
            // hoisted out of the `match` scrutinee. The scrutinee temporary
            // (`MutexGuard`) lives until the end of the whole `match`
            // expression, so re-locking `session_manager` inside a match
            // arm self-deadlocks on the non-reentrant `tokio::sync::Mutex`.
            // This exact bug froze the entire MQTT control plane (Stop /
            // CreateSession / ChatMessage all silently queued) while the
            // session kept running - see the 2026-08-20 incident.
            let agent_id = session_manager.lock().await.agent_id().to_string();
            let cancel_handle = session_manager.lock().await.cancel_handle(&session_id);
            match cancel_handle {
                Some(handle) => {
                    handle.cancel(CancellationReason::UserStop {
                        source: StopSource::ChatPanel {
                            agent_id,
                            session_id: session_id.clone(),
                        },
                        reason: reason.clone(),
                    });
                    tracing::info!(
                        session_id = %session_id,
                        reason = %reason,
                        "ADR-044 §4.5: cancellation handle fired for MQTT Stop signal"
                    );
                }
                None => {
                    tracing::debug!(
                        session_id = %session_id,
                        "Stop signal: no cancel handle registered (session evicted?)"
                    );
                }
            }
            forward_to_session_inbound(
                session_manager,
                lifecycle_publisher,
                &session_id,
                "stop",
                work_dir,
                InboundMessage::Stop { reason },
            )
            .await
        }

        // ③ Continue execution → session task inbox
        InboundMessage::ContinueExecution { reason, .. } => {
            forward_to_session_inbound(
                session_manager,
                lifecycle_publisher,
                &session_id,
                "continue_execution",
                work_dir,
                InboundMessage::ContinueExecution {
                    session_id: session_id.clone(),
                    reason,
                },
            )
            .await
        }

        // ④ Approval decision → session task inbox
        InboundMessage::ApprovalDecision {
            request_id,
            approved,
            allow_all_session,
            reason,
            ..
        } => {
            forward_to_session_inbound(
                session_manager,
                lifecycle_publisher,
                &session_id,
                "approval_decision",
                work_dir,
                InboundMessage::ApprovalDecision {
                    session_id: session_id.clone(),
                    request_id,
                    approved,
                    allow_all_session,
                    reason,
                },
            )
            .await
        }

        // ⑤ Question answer → session task inbox
        InboundMessage::QuestionAnswer {
            request_id, answer, ..
        } => {
            forward_to_session_inbound(
                session_manager,
                lifecycle_publisher,
                &session_id,
                "question_answer",
                work_dir,
                InboundMessage::QuestionAnswer {
                    session_id: session_id.clone(),
                    request_id,
                    answer,
                },
            )
            .await
        }

        // ⑥ UserOperation (StopLoop, ContinueLoop, ApprovalDecision, QuestionAnswer)
        //
        // `UpdateRuntimeConfig` is an agent-level config mutation and is
        // routed globally through `SessionManager::apply_runtime_config_override`
        // (see the system-level arm above for the canonical path; this
        // arm is defensive uniformity - any per-session producer
        // targeting a single session_id gets the same global policy
        // because config mutations are agent-scoped).
        InboundMessage::UserOperation(op) => match op {
            crate::agent::inbound::UserOp::UpdateRuntimeConfig(overrides) => {
                let failed = session_manager
                    .lock()
                    .await
                    .apply_runtime_config_override(&overrides);
                if !failed.is_empty() {
                    tracing::warn!(
                        failed_sessions = failed.len(),
                        "per-session UpdateRuntimeConfig: some sessions missed the broadcast (likely closed)"
                    );
                }
                Ok(())
            }
            _ => {
                forward_to_session_inbound(
                    session_manager,
                    lifecycle_publisher,
                    &session_id,
                    "user_operation",
                    work_dir,
                    InboundMessage::UserOperation(op),
                )
                .await
            }
        },

        // ⑦ IntentMessage → receiver-side session routing (design 27 §4.2.3:
        // the wire addresses the intent context; which execution session
        // answers is the receiving agent's private decision).
        msg @ InboundMessage::IntentMessage { .. } => {
            route_intent_inbound(
                session_manager,
                chat_sessions,
                &session_id,
                msg,
                work_dir,
                node_token,
            )
            .await
        }

        // ── ADR-034 §8 Phase 2 control commands ───────────────────────
        //
        // ⑧ close_session, ⑨ update_session_title, ⑩/⑪ enable_notify /
        // disable_notify are gone. The session-scoped ones are HTTP-only
        // now (ADR-076 §决策 4) — an MQTT control message carries no
        // identity, so the Runtime cannot authorize it; notify suppression
        // was retired by ADR-035 Phase 3 (push drives all streaming).
        // Their proto fields are deleted as well, so this is not a runtime
        // check that could regress — the commands cannot be expressed.
        //
        // ⑫ CompressAction — explicit CompressType i32 → CompressionAction mapping
        // (Phase 2-7: two paths must not cross).
        // CompressType::SUMMARY (1)     → CompressionAction::CompressSummary
        // Anything else is rejected (forwarded to session_task which emits an error).
        InboundMessage::CompressAction { compress_type, .. } => {
            // ADR-083: compress_type = 3 cancels the in-flight compaction.
            //
            // This MUST be dispatched HERE, not forwarded to the session
            // inbox like the other compress types: while a compaction runs the
            // session task is blocked inside `compact_history_if_needed().await`,
            // so an inbox message would sit unread until the very thing we are
            // trying to cancel had already finished. Firing the shared handle
            // directly wakes the compaction's `tokio::select!` on the next poll
            // (same pattern as the Stop arm above).
            if compress_type == 3 {
                // NOTE(deadlock): hoist the handle + agent_id out of the match
                // scrutinee. The guard temporary would live until the end of
                // the whole match expression and re-locking inside an arm
                // self-deadlocks on the non-reentrant `tokio::sync::Mutex`
                // (see the Stop arm's 2026-08-20 incident note).
                let agent_id = session_manager.lock().await.agent_id().to_string();
                let handle = session_manager
                    .lock()
                    .await
                    .compaction_cancel_handle(&session_id);
                match handle {
                    Some(handle) => {
                        handle.cancel(CancellationReason::UserStop {
                            source: StopSource::ChatPanel {
                                agent_id,
                                session_id: session_id.clone(),
                            },
                            reason: "cancel compaction".to_string(),
                        });
                        tracing::info!(
                            session_id = %session_id,
                            "ADR-083: compaction cancel handle fired (compress_type=3)"
                        );
                    }
                    None => {
                        tracing::debug!(
                            session_id = %session_id,
                            "compress_type=3: no compaction cancel handle registered (session evicted?)"
                        );
                    }
                }
                return Ok(());
            }
            let action = match compress_type {
                1 => CompressionAction::CompressSummary,
                other => {
                    return Err(RuntimeError::Config(format!(
                        "CompressAction: invalid compress_type {} (expected 1=SUMMARY or 3=CANCEL)",
                        other
                    )));
                }
            };
            session_manager
                .lock()
                .await
                .send_to_session(&session_id, SessionMessage::CompressAction(action))
                .map_err(|e| RuntimeError::Config(format!("CompressAction: {}", e)))
        }

        // ⑫b UpdateBuiltinTools - ADR-029/ADR-052: agent-level builtin tool
        // enabled mutation. Routed globally through
        // `SessionManager::apply_builtin_tools_enabled` so sessions
        // created AFTER this PUT inherit the new enabled flags (the
        // template is CoW-synced) and every active session's
        // dispatch list + LLM tool_definitions are rebuilt atomically.
        // See the system-level arm above for the canonical routing;
        // this is defensive uniformity.
        InboundMessage::UpdateBuiltinTools { entries } => {
            session_manager
                .lock()
                .await
                .apply_builtin_tools_enabled(&entries);
            Ok(())
        }

        // ADR-063 §3.7.6: main-dialog system prompt hot-reload. Defensive
        // uniformity — any per-session producer targeting a single
        // session_id gets the same global policy (system prompt is
        // agent-scoped, not session-scoped).
        InboundMessage::UpdateSystemPrompt { system_prompt } => {
            session_manager
                .lock()
                .await
                .apply_system_prompt(&system_prompt);
            Ok(())
        }

        // ADR-094: same defensive uniformity as `UpdateSystemPrompt`
        // — even a per-session routing key routes through the
        // SessionManager because the skills summary is agent-scoped.
        InboundMessage::UpdateSkillRegistry { .. } => Err(RuntimeError::Config(
            "UpdateSkillRegistry requires system-level dispatch".to_string(),
        )),
        InboundMessage::UpdateSkillInstructions { instructions } => {
            session_manager
                .lock()
                .await
                .apply_skill_instructions(instructions.as_deref());
            Ok(())
        }

        // ⑭ ChatMessage → SessionMessage::ChatMessage (from MQTT SendMessage)
        InboundMessage::ChatMessage {
            content,
            message_id,
            command,
            params_json,
            answerable_by,
        } => {
            // Parse params_json to extract attached_items (ADR-046 replaces
            // the prior document_ids + content_parts + attached_context
            // fields).
            //
            // ADR-046: `attached_items` is a strongly-typed discriminated
            // union array. Each item carries a `"type"` tag matching the
            // `AttachedItem` serde enum. Legacy `document_ids` and
            // `attached_context` from old desktop clients are NOT accepted
            // — there is no compatibility layer per ADR §4.
            let mut active_skill: Option<String> = None;
            let mut attached_items: Option<Vec<acowork_core::protocol::AttachedItem>> = None;
            let mut content_parts: Option<Vec<acowork_core::providers::traits::ContentPart>> = None;

            if !params_json.is_empty()
                && let Ok(params) = serde_json::from_str::<serde_json::Value>(&params_json)
            {
                if let Some(items) = params.get("attached_items").and_then(|v| v.as_array()) {
                    let parsed: Vec<acowork_core::protocol::AttachedItem> = items
                        .iter()
                        .filter_map(|d| {
                            serde_json::from_value::<acowork_core::protocol::AttachedItem>(
                                d.clone(),
                            )
                            .ok()
                        })
                        .collect();
                    if !parsed.is_empty() {
                        attached_items = Some(parsed);
                    }
                }
                if let Some(parts) = params.get("content_parts").and_then(|v| v.as_array()) {
                    let parsed: Vec<acowork_core::providers::traits::ContentPart> = parts
                        .iter()
                        .filter_map(|p| serde_json::from_value(p.clone()).ok())
                        .collect();
                    if !parsed.is_empty() {
                        content_parts = Some(parsed);
                    }
                }
            }

            // Per-turn skill injection: the frontend sends only the skill
            // NAME in `command`; the runtime owns the instructions (the
            // agent's SkillRegistry, loaded Phase A / injected Phase B).
            // Resolve here so skill content is never trusted to the client.
            if !command.is_empty() {
                let resolved = session_manager
                    .lock()
                    .await
                    .resolve_skill_instructions(&command);
                match resolved {
                    Some(instructions) => {
                        tracing::info!(
                            session_id = %session_id,
                            skill = %command,
                            skill_len = instructions.len(),
                            "ChatMessage: resolved skill command → instructions"
                        );
                        active_skill = Some(instructions);
                    }
                    None => tracing::warn!(
                        session_id = %session_id,
                        command = %command,
                        "ChatMessage: command did not match any loaded skill, ignoring"
                    ),
                }
            }

            // ADR-038 / ADR-100 Rev. 8: the turn needs an **Active**
            // session — the message goes to the in-memory session task, and a
            // session that is only on disk has no task.
            //
            // Reopening is the **write path's** job (`post_send_message`
            // calls `ensure_session_active` on its own request task), not
            // this funnel's: resuming a session reads its whole transcript
            // off disk, and this arm runs inline on the single MQTT/HTTP
            // dispatch loop — a disk-bound resume here would head-of-line
            // block every other inbound message of this Runtime. So the check
            // below is deliberately cheap (`contains_key`).
            //
            // The check and the handoff share one lock on purpose: between two
            // separate critical sections an idle eviction can land, and its
            // only symptom would be a bare "Session not found" with no event —
            // exactly the silent drop `session_not_opened` exists to prevent.
            {
                let mut guard = session_manager.lock().await;
                if !guard.is_session_active(&session_id) {
                    let reason = not_opened_reason(&guard, &session_id, work_dir);
                    tracing::warn!(
                        session_id = %session_id,
                        reason,
                        "ChatMessage: session is not Active — forwarding SessionNotOpened"
                    );
                    spawn_session_not_opened(
                        lifecycle_publisher,
                        &session_id,
                        "chat_message",
                        reason,
                    );
                    return Err(RuntimeError::Config(format!(
                        "session not Active ({}): {}",
                        reason, session_id
                    )));
                }
                let sent = guard.send_to_session(
                    &session_id,
                    SessionMessage::ChatMessage {
                        content,
                        message_id: message_id.clone(),
                        active_skill,
                        attached_items,
                        content_parts,
                        // Written straight against the execution surface: no
                        // Chat row precedes it, so there is nothing to thread
                        // a card under (ADR-100 Rev. 8 / D14). The session's
                        // own chat binding still supplies the chat and the
                        // caller supplies who can answer, so `session_task`
                        // completes the surface there — the reference row and
                        // any interactive card land on it (ADR-106 Rev. 6).
                        // ADR-106 D2: the round id is derived from this turn's
                        // own inbound id, which is the same value `run()` will
                        // mint, so the surface names the round even for a loop
                        // with no conversation writer.
                        surface: crate::agent::intent_turn::TurnSurface::new(
                            String::new(),
                            &message_id,
                            &answerable_by,
                            None,
                        ),
                    },
                );
                if let Err(e) = sent {
                    // The task can also vanish between the check above and this
                    // handoff. When that is what happened, the human's words are
                    // gone either way — publish the same event so the client can
                    // say so and offer a reopen instead of surfacing an error
                    // that explains nothing.
                    if !guard.is_session_active(&session_id) {
                        let reason = not_opened_reason(&guard, &session_id, work_dir);
                        tracing::warn!(
                            session_id = %session_id,
                            reason,
                            "ChatMessage: session vanished before the handoff — forwarding SessionNotOpened"
                        );
                        spawn_session_not_opened(
                            lifecycle_publisher,
                            &session_id,
                            "chat_message",
                            reason,
                        );
                    }
                    return Err(RuntimeError::Config(format!("ChatMessage: {}", e)));
                }
            }
            Ok(())
        }

        // ── Legacy fallback (Phase 7: no longer produced by control path) ──
        InboundMessage::SystemNotification {
            notification_type, ..
        } => {
            tracing::warn!(
                notification_type,
                "SystemNotification received at session level — no longer expected (Phase 7 cleanup)"
            );
            Ok(())
        }
    }
}

/// Design 27 §3.2/§6.1: resolve the execution session that owns an inbound
/// Intent on THIS agent and deliver it as a turn trigger.
///
/// - `request`: the designated session id is the *sender's* — meaningless
///   here. Route via the receiver-private registry keyed on the intent
///   context (chat session > conversation > one-shot message id), creating
///   a fresh session when the context is new or its session has closed.
/// - `response`: the designated session id is the initiator's own session
///   (the Gateway read it back from the mailbox). Use it when it is still
///   open; otherwise fall back to the registry keyed on the conversation.
///
/// Delivery is always `SessionMessage::Intent` — unlike a bare
/// `InboundMessage`, it starts a turn even when the session task is idle
/// (a mid-turn injection would otherwise sit unread in the loop channel).
///
/// ADR-099: a human `user_message` from a Chat session is a router turn, and a
/// router turn is never executed here — this function only queues it (see
/// [`spawn_chat_router_drain`]). Everything else, and every Chat message while
/// the router is switched off, goes straight through
/// [`route_intent_legacy`] on this task.
#[allow(clippy::too_many_arguments)]
async fn route_intent_inbound(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    chat_sessions: &Arc<crate::agent::chat_router::ChatSessionManager>,
    designated_session_id: &str,
    msg: crate::agent::inbound::InboundMessage,
    work_dir: &std::path::Path,
    node_token: &str,
) -> crate::error::Result<()> {
    use crate::agent::inbound::InboundMessage;
    use crate::error::RuntimeError;

    let InboundMessage::IntentMessage(env) = msg else {
        return Err(RuntimeError::Config(
            "route_intent_inbound called with a non-intent message".to_string(),
        ));
    };

    // ADR-106 D6 level 1: the human replied to one of the round reference rows
    // in this chat, so the Gateway already resolved *which* execution session
    // that round ran in and stamped it on the envelope. Explicit addressing is
    // not a routing question, so it is answered before the router is consulted:
    // no queue hop, no model call, and no later stage may re-decide it. A
    // session bound to another chat is refused downstream rather than re-bound
    // (D11) — an address that cannot be served is never answered with a
    // different one.
    if let Some(session_id) = crate::agent::chat_router::runner::addressed_session(&env)
        && !env.source_chat_session_id.is_empty()
    {
        return route_intent_addressed(session_manager, &session_id, env, work_dir).await;
    }

    // ADR-106 D6 level 3: the human addressed this agent and the chat has exactly
    // one session it may continue, so the follow-up is settled by the bindings
    // (D11) rather than by a model — same no-queue-hop, no-LLM property as level 1.
    //
    // Several candidates are a different case: choosing between them is a
    // decision, so the Runtime asks (a choice card) and delivers nothing. The
    // human's row is already durable, so answering the card is a normal second-level
    // `@` and no turn is lost; zero candidates leave nothing to continue and the
    // ladder falls through to levels 4-5.
    //
    // ADR-106 revision 2026-11-04: a turn that quotes `@session` material takes
    // this whole branch away, exactly like `force_new` — the human attached
    // rounds for the router to READ, and letting the bindings auto-continue
    // (or parking a choice card) would answer a question the message did not
    // ask. The turn lands on the router, which can still create, answer, or
    // propose.
    if crate::agent::chat_router::runner::is_chat_turn(&env)
        && !env.source_chat_session_id.is_empty()
        && crate::agent::chat_router::runner::explicit_mention(&env)
        && !crate::agent::chat_router::runner::force_new(&env)
        && crate::agent::chat_router::runner::pinned_chat_refs(&env).is_empty()
    {
        let chat = env.source_chat_session_id.clone();
        let initiator = Some(env.from.as_str()).filter(|s| !s.is_empty());
        let surface = chat_surface(&env);
        let to = env.from.clone();
        let in_reply_to = env.in_reply_to.clone();
        let message_id = env.message_id.clone();
        let candidates = session_manager
            .lock()
            .await
            .chat_continuations(&chat, initiator, surface);
        match candidates.as_slice() {
            [] => {}
            [only] => {
                return route_intent_addressed(session_manager, &only.session_id, env, work_dir).await;
            }
            many => {
                let asked = {
                    let guard = session_manager.lock().await;
                    guard
                        .emit_session_choice(
                            &chat,
                            &to,
                            &in_reply_to,
                            &message_id,
                            "session_choice",
                            many.iter().map(|c| c.choice_option()).collect(),
                        )
                        .await
                };
                match asked {
                    Ok(()) => {
                        tracing::debug!(
                            chat_id = %chat,
                            options = many.len(),
                            "chat level-3 choice card written — the message waits on the answer"
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        // A question that could not be asked must not swallow the
                        // message: degrade to the router, which can still answer it.
                        tracing::warn!(
                            chat_id = %chat,
                            error = %e,
                            "session choice card failed — falling through to the router"
                        );
                    }
                }
            }
        }
    }

    // A router turn is an LLM call plus a couple of HTTP hops. Running it here
    // would stop this agent from receiving *anything* — including a `Stop` —
    // for as long as it takes, so it is queued and this loop keeps reading
    // MQTT. The enable switch is checked inline (a few-KB file read, no lock)
    // so a disabled router takes exactly the pre-ADR-099 path instead of
    // paying a queue hop only to be handed back.
    if crate::agent::chat_router::runner::is_chat_turn(&env)
        && crate::agent::chat_router::runner::router_enabled(work_dir)
    {
        let chat_id = env.source_chat_session_id.clone();
        let turn = crate::agent::chat_router::PendingTurn {
            envelope: env,
            designated_session_id: designated_session_id.to_string(),
            node_token: node_token.to_string(),
        };
        if chat_sessions.push_turn(&chat_id, turn) {
            spawn_chat_router_drain(
                Arc::clone(session_manager),
                Arc::clone(chat_sessions),
                chat_id,
                work_dir.to_path_buf(),
            );
        }
        return Ok(());
    }

    // Router off: the deterministic route, unchanged (ADR-099).
    route_intent_legacy(session_manager, designated_session_id, env, false, work_dir).await
}

/// Run one chat's router turns, strictly in arrival order, on a task of its
/// own (ADR-099 §5, design 27 §5.4.4).
///
/// The per-chat claim (`ChatSessionManager::push_turn` / `finish_drain`) is
/// what guarantees exactly one drainer per chat — two turns of one chat must
/// not read the same history anchor and then race on the write-back, while a
/// slow model in one chat may not stall another chat or this agent's inbound
/// loop. When the router does not own a turn (`PassThrough` / `Fallback`) the
/// deterministic route finishes it here, so a router failure costs latency,
/// never the message.
///
/// Each turn runs in a task of its own: a panic inside one turn is observed as
/// an `Err` and the loop still reaches `finish_drain`, so no chat can be left
/// claimed with a queue nobody will drain.
fn spawn_chat_router_drain(
    session_manager: Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    chat_sessions: Arc<crate::agent::chat_router::ChatSessionManager>,
    chat_id: String,
    work_dir: std::path::PathBuf,
) {
    tokio::spawn(async move {
        loop {
            let Some(turn) = chat_sessions.take_turn(&chat_id) else {
                // `false` = nobody is waiting on this chat any more; `true`
                // means a message arrived while we were finishing, so this
                // drainer keeps the claim and takes it next round.
                if chat_sessions.finish_drain(&chat_id) {
                    continue;
                }
                return;
            };
            let sm = Arc::clone(&session_manager);
            let cs = Arc::clone(&chat_sessions);
            let envelope = turn.envelope;
            let designated = turn.designated_session_id;
            let token = turn.node_token.clone();
            let work_dir = work_dir.clone();
            let turn_result = tokio::spawn(async move {
                let outcome =
                    crate::agent::chat_router::runner::handle_intent(&sm, &cs, &envelope, &token)
                        .await;
                finish_chat_turn(&sm, outcome, &designated, envelope, &work_dir)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;
            match turn_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(chat_id = %chat_id, error = %e, "chat router fallback route failed");
                }
                Err(e) => {
                    tracing::error!(chat_id = %chat_id, error = %e, "chat router turn task failed");
                }
            }
        }
    });
}

/// The degraded route's background reference for a fresh execution session
/// (ADR-107).
///
/// The router could not decide, so there is no composed background — but it did
/// read the conversation before it failed, and that bounded mechanical tail is
/// what it saw. Handing it over as a **reference part** (not as the message)
/// keeps the human's words as the turn's message: the session records what the
/// person asked, and reads the tail as context.
///
/// `None` means "there is nothing to say": the router captured no excerpt, or
/// the turn had neither words nor files — the caller then sends no reference at
/// all. A turn that carried files but no text still gets the tail: the router
/// failing to decide may not degrade into an execution session that was told
/// nothing, because a human who attached something instead of typing still
/// asked for work.
fn degraded_background(
    text: &str,
    has_attachments: bool,
    excerpt: Option<&str>,
) -> Option<acowork_core::MessageRef> {
    const NOTE: &str =
        "(the routing assistant was unavailable; this is the raw tail of the conversation)";
    let excerpt = excerpt?;
    if text.trim().is_empty() && !has_attachments {
        return None;
    }
    Some(acowork_core::MessageRef::chat_background(format!(
        "{NOTE}
{excerpt}"
    )))
}

/// Deliver a queued turn according to what the router decided.
///
/// The router owns a turn it `Handled` — it either answered it or dispatched it.
/// Every other outcome is finished by [`route_intent_legacy`] on this task, so a
/// router that cannot decide costs the chat latency and never the message
/// (ADR-099 §5). A fallback must not re-enter the queue: it is the same
/// envelope, already claimed.
///
/// ADR-106: the degraded route writes no receipt row of its own — the round it
/// eventually opens carries the reference row that tells the human where the
/// message went.
pub(crate) async fn finish_chat_turn(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    outcome: crate::agent::chat_router::runner::Outcome,
    designated_session_id: &str,
    mut envelope: Box<crate::agent::inbound::IntentEnvelope>,
    work_dir: &std::path::Path,
) -> crate::error::Result<()> {
    use crate::agent::chat_router::runner::Outcome;

    match outcome {
        Outcome::Handled => Ok(()),
        Outcome::PassThrough => {
            route_intent_legacy(session_manager, designated_session_id, envelope, false, work_dir)
                .await
        }
        Outcome::Fallback {
            reason,
            chat_excerpt,
        } => {
            tracing::info!(
                reason = %reason,
                "Chat Router fell back to the deterministic route"
            );
            // The degraded route has no LLM-composed background — but when the
            // router HAD read the conversation before it failed, that bounded
            // mechanical tail is what it saw, not what it concluded, so the new
            // session can use it without re-introducing another task's noise.
            //
            // ADR-107: it rides `params.context_refs`, never `params.message`.
            // `message` is the human's own words and stays exactly as
            // delivered — overwriting it (the old behaviour) put the router's
            // framing into the persisted `user` row and therefore into the
            // auto-title. An attachment-only turn needs no special case here:
            // its message is legitimately empty and the reference is the
            // context the model reads.
            //
            // Straight through the router's own reader rather than a second
            // reading of the raw param: the raw array and "did this turn carry
            // files?" disagree on a degenerate element (`[{}]`), and Block C,
            // the decision log and this reference must never disagree about it.
            let has_attachments =
                !crate::agent::chat_router::runner::turn_attachments(&envelope).is_empty();
            let text = envelope
                .params
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            if let Some(background) = degraded_background(&text, has_attachments, chat_excerpt.as_deref())
                && let Some(map) = envelope.params.as_object_mut()
            {
                map.insert(
                    "context_refs".to_string(),
                    serde_json::json!([background]),
                );
            }
            // ADR-099 revision: a chat-sourced human message degraded by the
            // router gets a FRESH execution session, not the mapped one — the
            // mapping is not a semantic verdict and continuing an unrelated old
            // task on a guess is worse than starting a new one. PM tasks and
            // agent-agent intents keep the deterministic reuse (their
            // conversation_id / msg key IS the task identity).
            route_intent_legacy(session_manager, designated_session_id, envelope, true, work_dir)
                .await
        }
    }
}

/// The deterministic route that existed before ADR-099: registry key →
/// execution session → `SessionMessage::Intent`.
///
/// Also the degraded path of a Chat turn the router could not decide, which is
/// why the router's own result is not consulted here — a fallback must never
/// re-enter the queue.
///
/// `fresh` (set only by the router's `Fallback` outcome) routes a Chat-sourced
/// human message to a **newly created** execution session instead of the
/// mapped one (ADR-099 revision): a degraded router has no semantic verdict,
/// and the registry mapping is not one either — continuing an unrelated old
/// task on a guess is worse than starting clean. `PassThrough` (router off,
/// PM, agent-agent, responses) keeps the deterministic reuse unchanged.
async fn route_intent_legacy(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    designated_session_id: &str,
    env: Box<crate::agent::inbound::IntentEnvelope>,
    fresh: bool,
    work_dir: &std::path::Path,
) -> crate::error::Result<()> {
    let target = resolve_legacy_route(session_manager, designated_session_id, &env, fresh).await?;
    deliver_intent_turn(session_manager, target, env, work_dir).await
}

/// ADR-106 D6 level 1: deliver an explicitly addressed turn into that session.
///
/// The chat's reference row named the execution session and the Gateway read
/// the address off it, so this path decides nothing about *which* session runs
/// the work — it only checks that the session still exists (reopening a closed
/// one) and that it belongs to this chat. `chat_dispatch` owns both checks and
/// the D11 refusal, so the address is validated by exactly the same code the
/// router's own `dispatch(existing)` runs through; the two paths must not grow
/// two different notions of a valid address.
async fn route_intent_addressed(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    session_id: &str,
    env: Box<crate::agent::inbound::IntentEnvelope>,
    work_dir: &std::path::Path,
) -> crate::error::Result<()> {
    let chat_session_id = env.source_chat_session_id.clone();
    let initiator = env.from.clone();
    // Guard released before the delivery: `send_to_session` takes the same
    // lock, and a tokio Mutex is not reentrant.
    let target = {
        let mut guard = session_manager.lock().await;
        guard
            .chat_dispatch(
                &chat_session_id,
                crate::agent::chat_router::tool::Target::Existing(session_id.to_string()),
                crate::agent::session::intent_routing::SessionSeed::from_json(
                    &env.session_config_json,
                ),
                Some(initiator.as_str()),
            )
            .await?
    };
    deliver_intent_turn(session_manager, target, env, work_dir).await
}

/// Which execution session the deterministic route delivers into, and whether
/// this call created it (`opened_new`, ADR-106 D3).
async fn resolve_legacy_route(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    designated_session_id: &str,
    env: &crate::agent::inbound::IntentEnvelope,
    fresh: bool,
) -> crate::error::Result<(String, bool)> {
    use crate::agent::session::intent_routing::routing_key;

    let origin = acowork_core::intent::TurnOrigin::parse(&env.kind);
    // Candidate session: the designated one for responses, registry otherwise.
    if origin == acowork_core::intent::TurnOrigin::AgentResponse
        && !designated_session_id.is_empty()
    {
        let guard = session_manager.lock().await;
        if guard.get_session(designated_session_id).is_some() {
            return Ok((designated_session_id.to_string(), false));
        }
    }
    // The initiator's session-config suggestion (§4) is consumed on whichever
    // branch the routing takes: at creation here, or applied to the session the
    // message was routed into (ADR-099).
    let seed = crate::agent::session::intent_routing::SessionSeed::from_json(
        &env.session_config_json,
    );
    let is_chat_human =
        origin.policy().router_eligible && !env.source_chat_session_id.is_empty();
    if is_chat_human {
        // ADR-106 D6/D11: a Chat turn never consults the intent-context
        // registry. Its whole address space is the sessions bound to this chat,
        // and it continues one only when there is exactly one to choose from.
        // The retired `chat:{csid}:{initiator}` mapping used to answer "whatever
        // this chat last ran", which is a guess about intent dressed up as a
        // fact about address; several candidates now means a new session of this
        // turn's own — the same call the router's `dispatch(new)` makes.
        let mut guard = session_manager.lock().await;
        // `fresh` is the degraded-router case: the router read the conversation
        // and failed to decide, so nothing here may continue old work.
        let continuation = if fresh {
            None
        } else {
            guard.chat_continuation(
                &env.source_chat_session_id,
                Some(env.from.as_str()).filter(|s| !s.is_empty()),
                chat_surface(env),
            )
        };
        let target = match continuation {
            Some(session_id) => crate::agent::chat_router::tool::Target::Existing(session_id),
            None => crate::agent::chat_router::tool::Target::New,
        };
        guard
            .chat_dispatch(
                &env.source_chat_session_id,
                target,
                seed,
                Some(env.from.as_str()),
            )
            .await
    } else {
        session_manager
            .lock()
            .await
            .route_intent_session(
                &routing_key(&env.conversation_id, &env.message_id),
                seed,
                // ADR-099: a PM task dispatched from a group chat binds
                // that session to the group, so follow-ups there can find
                // it. Empty for agent-to-agent and cron intents.
                Some(env.source_chat_session_id.as_str()),
                // ADR-076 §决策 4: the session's initiator is the
                // envelope's `from` — the human who sent the message,
                // or the agent instance that started the work. Never
                // the account this Runtime happens to be carrying.
                Some(env.from.as_str()),
            )
            .await
    }
}

/// Which Chat surface an envelope came from. The Gateway stamps the kind on the
/// delivery (ADR-099) — the Runtime must not infer it — and the deterministic
/// route needs it because the candidate filter differs per surface: a group only
/// ever offers the speaker's own sessions.
fn chat_surface(
    env: &crate::agent::inbound::IntentEnvelope,
) -> crate::agent::session::chat_candidates::CandidateSurface {
    use crate::agent::session::chat_candidates::CandidateSurface;

    let kind = env
        .params
        .get("chat_kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if crate::agent::chat_router::prompt::is_group_surface(kind) {
        CandidateSurface::Group
    } else {
        CandidateSurface::Direct
    }
}

/// The reference parts one delivery carries: whatever the sender already
/// stamped into `params.context_refs` (ADR-107 — today the degraded route's
/// background), plus this turn's pinned `@session` material rendered to
/// `referenced_round` parts (ADR-106 revision 2026-11-04).
///
/// The material travels on EVERY delivery path a reference-carrying turn
/// takes — the addressed continuation (levels 1–2, card answers' re-delivery
/// included) and the degraded route alike. One renderer with the router's
/// Block C (`render_pinned_rounds`): the same bodies, the same per-field
/// clamps and the same total budget, so what the router was shown and what
/// this session receives cannot drift.
fn context_refs_with_material(
    params: &serde_json::Value,
    work_dir: &std::path::Path,
) -> Vec<acowork_core::MessageRef> {
    let mut context_refs = crate::agent::intent_turn::context_refs_from_params(params);
    let pinned = crate::agent::chat_router::runner::pinned_chat_refs_from_params(params);
    if !pinned.is_empty() {
        context_refs.extend(
            crate::agent::chat_router::runner::render_pinned_rounds(
                &work_dir.join("conversations"),
                &pinned,
            )
            .into_iter()
            .map(|body| acowork_core::MessageRef::referenced_round(body.trim().to_string()))
            .filter(|part| !part.is_empty()),
        );
    }
    context_refs
}

/// Hand one intent to the session the routing stage chose.
///
/// Split out of [`route_intent_legacy`] so the addressed path (D6 level 1) and
/// the registry path (levels 4–5) share one delivery — and therefore one
/// pending-queue record, one [`TurnSurface`], and one `opened_new` stamp.
async fn deliver_intent_turn(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    target: (String, bool),
    env: Box<crate::agent::inbound::IntentEnvelope>,
    work_dir: &std::path::Path,
) -> crate::error::Result<()> {
    use crate::agent::inbound::IntentEnvelope;
    use crate::agent::intent_turn::render_intent_message;
    use crate::agent::session::SessionMessage;

    let IntentEnvelope {
        from,
        action,
        params,
        message_id,
        conversation_id,
        source_chat_session_id,
        repo,
        session_config_json: _,
        kind,
        in_reply_to,
        status,
        has_human,
        chat_attachment_ids,
    } = *env;

    // A PM dispatch's wake-up playbook is model-facing and ephemeral: it
    // bypasses the rendered message and history entirely, injected into
    // the turn's system block (see `ContextBuilder::set_task_brief`).
    // Origin-gated: a peer's params are never trusted with the system
    // block (`extract_assignment_brief`).
    let assignment_brief = crate::agent::intent_turn::extract_assignment_brief(&kind, &params);
    // ADR-106 D13: the PM task a delivery's conversation names is the block's
    // task chip. Read off the Gateway's routing key, not off `params` — a peer
    // must not be able to label a Chat block with a task it does not own.
    let task_id = acowork_core::intent::task_id_from_conversation(&conversation_id)
        .unwrap_or_default();
    // ADR-107: reference parts the sender attached to this turn (today the
    // router's degraded-route background). Read off `params` for the same
    // reason the attachments are: one opaque object, no wire change for data
    // only the Runtime produces and only the Runtime reads.
    let context_refs = context_refs_with_material(&params, work_dir);
    let content = render_intent_message(
        &from,
        &action,
        &params,
        &message_id,
        &conversation_id,
        &source_chat_session_id,
        &repo,
        &kind,
        &in_reply_to,
        &status,
    );

    // §5.5.5 pending queue: which intents must be shown in the execution
    // side's pending list while that session is busy. A Chat-sourced turn that
    // owes its chat a write-back is exactly that set. ADR-106 retired the
    // routing reaction row this pointer used to feed: the chat coordinate and
    // the participant who can answer are carried on the turn's surface, and the
    // round's reference row is what tells the chat where the message went.
    let pending_from_chat = if !source_chat_session_id.is_empty()
        && crate::agent::intent_turn::turn_kind_semantics(&kind, &message_id).1
    {
        Some((message_id.clone(), from.clone()))
    } else {
        None
    };
    // §5.5.5 pending queue: record BEFORE delivering, mirroring the router
    // path (runner.rs). Recording after the send raced with an idle session
    // that woke and started the turn at once — its empty removal ran first,
    // leaving the row we then pushed stuck in the queue forever. A busy
    // session is active now (row recorded, cleared at dequeue); an idle one
    // is not (record is a no-op — it starts immediately).
    if let Some((pending_message_id, pending_from)) = pending_from_chat.as_ref() {
        session_manager.lock().await.record_pending_intent(
            &target.0,
            crate::agent::session_state::PendingIntent {
                message_id: pending_message_id.clone(),
                from: pending_from.clone(),
                preview: crate::agent::session_state::pending_preview(
                    &crate::agent::session_state::preview_source(
                        params.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                        &context_refs,
                    ),
                ),
                from_chat: true,
            },
        );
    }
    let opened_new = target.1;
    // ADR-100 follow-up: this turn's Chat obligations, produced from the same
    // envelope fields the route already read. One producer for every
    // transport: the loop's card writer, the human-awaiting gate and the
    // reference-row writer all read this value instead of re-deriving it.
    let surface = crate::agent::intent_turn::TurnSurface::new(
        source_chat_session_id,
        &message_id,
        &from,
        has_human,
    );
    session_manager.lock().await.send_to_session(
        &target.0,
        SessionMessage::Intent {
            content,
            message_id,
            surface,
            from,
            action,
            repo,
            kind,
            chat_attachment_ids,
            // A human Chat turn may carry workspace references without a pin
            // (the Chat surface's composer never pins).
            attached_items: crate::agent::intent_turn::attached_items_from_params(&params)
                .unwrap_or_default(),
            in_reply_to,
            assignment_brief,
            task_id,
            // ADR-106 D3: the deterministic route knows whether it created the
            // session it is delivering into; only the turn's reference row can
            // state that to the chat, and only the transport knows it.
            opened_new: Some(opened_new),
            // ADR-107: context the sender supplied that the human did not type.
            context_refs,
        },
    )
}

/// Which lifecycle state to report in a `SessionNotOpened` payload.
///
/// ADR-038: `Closed` (on disk, not in memory) and `NotFound` (no session at
/// all) get different toasts in the Desktop, so the two must not collapse into
/// one string. Callers only ask after they failed to find an in-memory task.
fn not_opened_reason(
    manager: &crate::agent::session::SessionManager,
    session_id: &str,
    work_dir: &std::path::Path,
) -> &'static str {
    match manager.get_lifecycle_state(session_id, work_dir) {
        crate::agent::session::SessionLifecycleState::Closed => "session_closed",
        _ => "session_not_found",
    }
}

/// Publish `SessionNotOpened` for a session-scoped message that found no task,
/// so the client can offer a reopen affordance instead of leaving the human's
/// words in a bubble that never becomes a turn.
///
/// Best-effort and detached: the callers run inline on the single inbound
/// dispatch loop, so nobody may wait on the broker, and a failed publish must
/// not change the error the caller already has.
fn spawn_session_not_opened(
    lifecycle_publisher: &crate::mqtt::MqttChunkPublisher,
    session_id: &str,
    attempted_command: &str,
    reason: &str,
) {
    let publisher = lifecycle_publisher.clone();
    let sid = session_id.to_string();
    let cmd = attempted_command.to_string();
    let reason = reason.to_string();
    tokio::spawn(async move {
        let _ = publisher
            .publish_session_not_opened(&sid, &cmd, &reason)
            .await;
    });
}

/// Forward an `InboundMessage` to the session task's `agent_inbound_tx`.
///
/// ADR-038: when the target session is not Active (Closed or NotFound
/// in `SessionManager`), we return an error AND publish a structured
/// `SessionNotOpened` event so the Desktop can surface a reopen
/// affordance. Without this, a frontend that forgot to send
/// `open_session` first would silently drop the message — the bug we
/// are fixing here.
async fn forward_to_session_inbound(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    lifecycle_publisher: &crate::mqtt::MqttChunkPublisher,
    session_id: &str,
    attempted_command: &str,
    work_dir: &std::path::Path,
    msg: crate::agent::inbound::InboundMessage,
) -> crate::error::Result<()> {
    use crate::error::RuntimeError;
    // Hold the SessionManager lock for the whole body: `SessionHandle`
    // is borrowed from the manager (no Clone impl), and `send_inbound`
    // only does `touch()` + `try_send()` — neither touches the manager,
    // so keeping the lock here is safe and avoids a re-borrow.
    let manager_guard = session_manager.lock().await;
    let handle = match manager_guard.get_session(session_id) {
        Some(h) => h,
        None => {
            // Determine reason: Closed (file exists on disk) vs NotFound
            // (no file) — the Desktop uses this to render the right toast.
            let reason = not_opened_reason(&manager_guard, session_id, work_dir);
            tracing::warn!(
                session_id = %session_id,
                attempted_command = %attempted_command,
                reason = %reason,
                "session not Active: forwarding SessionNotOpened",
            );
            // Best-effort publish — never block on the broker.
            spawn_session_not_opened(lifecycle_publisher, session_id, attempted_command, reason);
            return Err(RuntimeError::Config(format!(
                "session not Active ({}): {}",
                reason, session_id,
            )));
        }
    };
    handle
        .send_inbound(msg)
        .map_err(|e| RuntimeError::Config(format!("send_inbound failed: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::inbound::InboundMessage;
    use crate::mqtt::control_handler::ControlAction;

    /// A router that fell back still owes the fresh execution session the
    /// context it could not compose. The turn that most needs it is the one
    /// with no text at all: the human attached a file and typed nothing, so
    /// "tail only" is the difference between a briefed session and a session
    /// that was told nothing.
    ///
    /// ADR-107: the tail is a REFERENCE, not the message. The old shape wrote
    /// the tail into `params.message` with the human's words glued in front of
    /// it — which is what made the persisted row, and the auto-title, open with
    /// the router's sentence instead of the person's.
    #[test]
    fn a_degraded_turn_carries_the_chat_tail_as_a_reference() {
        let background = degraded_background("", true, Some("U: (nothing)
A: hi")).unwrap();
        assert_eq!(background.kind, acowork_core::REF_KIND_CHAT_BACKGROUND);
        assert!(background.content.starts_with('('), "{}", background.content);
        assert!(background.content.ends_with("A: hi"));
        assert!(!background.content.contains("fix the build"));

        // Text on the turn is not copied into the reference — it is the
        // message, and the reference only says what the router could not.
        let background = degraded_background("fix the build", false, Some("tail")).unwrap();
        assert!(!background.content.contains("fix the build"), "{}", background.content);
        assert!(background.content.ends_with("tail"));
        // Whitespace is not text.
        assert!(degraded_background("  
 ", true, Some("tail")).is_some());

        // Nothing to say: no excerpt captured, or no excerpt and no files.
        assert!(degraded_background("hello", false, None).is_none());
        assert!(degraded_background("   ", false, Some("tail")).is_none());
    }

    /// ADR-076 §决策 4: after the second wave of the HTTP migration the MQTT
    /// control plane holds exactly one variant — `Intent` — and it is
    /// not a user action. A session-scoped command cannot be expressed
    /// at all — there is no `ControlAction` variant and no proto field
    /// for it — so "refused over MQTT" is enforced by the compiler
    /// rather than by an assert.
    ///
    /// What this test pins is the *routing* of the survivor: `Intent`
    /// maps to a system-level (`""`) `IntentMessage`.
    #[test]
    fn only_non_user_signals_map_over_mqtt() {
        let (route, msg) = control_action_to_inbound(ControlAction::IntentReceived {
            from: "cron:agent".into(),
            action: "hourly_check".into(),
            params_json: r#"{"k":1}"#.into(),
            message_id: "m-1".into(),
            conversation_id: String::new(),
            source_agent_session_id: String::new(),
            source_chat_session_id: String::new(),
            repo: String::new(),
            session_config_json: String::new(),
            kind: String::new(),
            in_reply_to: String::new(),
            status: String::new(),
            has_human: None,
            chat_attachment_ids: Vec::new(),
        })
        .expect("Intent must map");
        assert_eq!(
            route, "",
            "cron intent has no target session — system-level"
        );

        match msg {
            InboundMessage::IntentMessage(env) => {
                assert_eq!(env.from, "cron:agent");
                assert_eq!(env.action, "hourly_check");
                assert_eq!(env.params["k"], 1);
                assert_eq!(env.message_id, "m-1");
            }
            other => panic!("expected IntentMessage, got {other:?}"),
        }
    }

    /// Malformed `params_json` must not panic — it degrades to `{}`.
    #[test]
    fn intent_with_bad_params_json_degrades_to_empty_object() {
        let (_, msg) = control_action_to_inbound(ControlAction::IntentReceived {
            from: "cron:x".into(),
            action: "a".into(),
            params_json: "not json".into(),
            message_id: String::new(),
            conversation_id: String::new(),
            source_agent_session_id: String::new(),
            source_chat_session_id: String::new(),
            repo: String::new(),
            session_config_json: String::new(),
            kind: String::new(),
            in_reply_to: String::new(),
            status: String::new(),
            has_human: None,
            chat_attachment_ids: Vec::new(),
        })
        .expect("Intent must still map");

        match msg {
            InboundMessage::IntentMessage(env) => assert!(env.params.is_object()),
            other => panic!("expected IntentMessage, got {other:?}"),
        }
    }

    // ── ADR-099 §5: where a Chat turn runs ─────────────────────────

    use crate::agent::chat_router::ChatSessionManager;
    use crate::agent::inbound::IntentEnvelope;

    /// A human Chat message — the one Intent shape the Chat Router takes.
    fn chat_envelope(chat_id: &str, text: &str) -> Box<IntentEnvelope> {
        Box::new(IntentEnvelope {
            from: "user:u-1".into(),
            action: "user_message".into(),
            params: serde_json::json!({ "message": text, "chat_kind": "dm" }),
            message_id: "m-1".into(),
            conversation_id: String::new(),
            source_chat_session_id: chat_id.into(),
            repo: String::new(),
            session_config_json: String::new(),
            kind: "user_message".into(),
            in_reply_to: String::new(),
            status: String::new(),
            has_human: Some(true),
            chat_attachment_ids: Vec::new(),
        })
    }

    fn chat_user_message(chat_id: &str, text: &str) -> InboundMessage {
        InboundMessage::IntentMessage(chat_envelope(chat_id, text))
    }

    /// A `SessionManager` + `ChatSessionManager` pair over `work_dir`, with the
    /// Gateway host pointed at a non-routable address.
    ///
    /// That last part is not cosmetic: these tests exercise the hand-off, not a
    /// full router turn, and a stray `await` must never let a drain task talk to
    /// the Gateway that happens to be running on the developer's machine.
    fn chat_router_test_pair(
        work_dir: &std::path::Path,
    ) -> (
        Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
        Arc<ChatSessionManager>,
    ) {
        use crate::agent::agent_core::AgentCore;
        use crate::agent::session::SessionManager;
        use crate::tools::workspace_resolver::WorkspaceResolver;

        let config = crate::config::RuntimeConfig {
            work_dir: work_dir.to_string_lossy().to_string(),
            gateway_host: Some("240.0.0.1".into()),
            ..Default::default()
        };
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.chat_router_route"
            version = "1.0.0"
            name = "Test chat router route"
            description = "Pin where a Chat turn is executed"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(acowork_core::providers::mock::MockProvider::single_text(
            "test",
        ));
        let core = Arc::new(AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));
        let mut manager = SessionManager::new(core, Default::default());
        manager.set_resolver(Arc::new(std::sync::RwLock::new(
            WorkspaceResolver::new_for_test(vec![]),
        )));
        (
            Arc::new(tokio::sync::Mutex::new(manager)),
            Default::default(),
        )
    }

    /// The head-of-line fix (ADR-099 §5): a Chat message is queued for a
    /// per-chat drain task and the inbound loop returns immediately — it must
    /// not run an LLM call plus two HTTP hops on the task that reads MQTT, or
    /// the agent would stop receiving everything, `Stop` included.
    ///
    /// The queueing path has no await point, so on this current-thread runtime
    /// the spawned drain is never polled; the runtime teardown drops it. That
    /// is exactly what lets the test observe the hand-off itself.
    #[tokio::test]
    async fn a_chat_turn_is_queued_instead_of_running_on_the_inbound_loop() {
        let dir = tempfile::TempDir::new().expect("temp work dir");
        let (sm, chat_sessions) = chat_router_test_pair(dir.path());

        route_intent_inbound(
            &sm,
            &chat_sessions,
            "",
            chat_user_message("cs-queue", "hello"),
            dir.path(),
            "node-token",
        )
        .await
        .expect("queueing a chat turn cannot fail");

        assert!(
            chat_sessions.chat_is_busy("cs-queue"),
            "the chat must hold a drain claim"
        );
        assert_eq!(
            chat_sessions.pending_turns("cs-queue"),
            1,
            "the message waits in the queue, it is never dropped"
        );
        assert!(
            sm.try_lock()
                .expect("the inbound loop must not hold the manager")
                .active_sessions()
                .is_empty(),
            "nothing may be routed inline while the router is on"
        );
    }

    /// ADR-099 D8 kill switch: with `chat_router.enabled = false` a Chat message
    /// takes the pre-ADR-099 path — inline, deterministic, never queued — so
    /// switching the feature off cannot turn into a message-loss mode, and the
    /// suites that assert on immediate routing keep working.
    #[tokio::test]
    async fn a_disabled_router_routes_a_chat_message_inline() {
        let dir = tempfile::TempDir::new().expect("temp work dir");
        std::fs::create_dir_all(dir.path().join("config")).expect("config dir");
        std::fs::write(
            dir.path().join("config").join("agent_config.json"),
            r#"{"chat_router":{"enabled":false}}"#,
        )
        .expect("write agent_config");
        let (sm, chat_sessions) = chat_router_test_pair(dir.path());

        route_intent_inbound(
            &sm,
            &chat_sessions,
            "",
            chat_user_message("cs-off", "hello"),
            dir.path(),
            "node-token",
        )
        .await
        .expect("the deterministic route must deliver inline");

        assert!(
            !chat_sessions.chat_is_busy("cs-off"),
            "a disabled router must not pay the queue hop"
        );
        assert_eq!(
            sm.try_lock()
                .expect("lock is free once routed")
                .active_sessions()
                .len(),
            1,
            "the message reached an execution session"
        );
    }

    /// ADR-106 revision 2026-11-04: a turn that quotes `@session` material
    /// takes the whole level-3 branch away — the bindings deliver nothing and
    /// no choice card is parked; the message waits on the router instead (a
    /// quoted round is material to read, not a continuation to decide).
    /// Control: the same `@` without material keeps the deterministic
    /// continuation — the suppression must not widen into disabling level 3.
    #[tokio::test]
    async fn quoted_material_takes_the_level_three_branch_away() {
        use crate::agent::chat_router::tool::Target;
        let dir = tempfile::TempDir::new().expect("temp work dir");
        let (sm, chat_sessions) = chat_router_test_pair(dir.path());
        // Exactly one continuation candidate: a session bound to this chat and
        // this speaker, so level 3 would auto-deliver if it ran.
        let bound = {
            let mut guard = sm.lock().await;
            guard
                .chat_dispatch("cs-refs", Target::New, None, Some("user:u-1"))
                .await
                .expect("bind a session to the chat")
                .0
        };

        let mut env = chat_envelope("cs-refs", "把引用的轮次汇总一下");
        env.params[acowork_core::protocol::EXPLICIT_MENTION_PARAM] = serde_json::json!(true);
        env.params[acowork_core::protocol::CHAT_REFS_PARAM] = serde_json::json!([{
            "agent_instance_id": "i-1",
            "session_id": bound,
            "round_id": "r:m-2",
            "terminal": true,
        }]);
        route_intent_inbound(
            &sm,
            &chat_sessions,
            "",
            InboundMessage::IntentMessage(env),
            dir.path(),
            "node-token",
        )
        .await
        .expect("queueing a referenced turn must not fail");
        assert_eq!(
            chat_sessions.pending_turns("cs-refs"),
            1,
            "refs skip level 3 — the turn lands on the router queue, delivered by nobody"
        );
        assert_eq!(
            sm.try_lock().expect("free").active_sessions().len(),
            1,
            "no second session was opened for a suppressed continuation"
        );

        // Control: the same @ with no material still auto-continues — level 3
        // delivers directly, so the turn never joins the router queue. The
        // referenced turn above is still parked there, so the observable is
        // that the queue does not grow, not that it drains.
        let pending_before = chat_sessions.pending_turns("cs-refs");
        let mut plain = chat_envelope("cs-refs", "继续");
        plain.params[acowork_core::protocol::EXPLICIT_MENTION_PARAM] = serde_json::json!(true);
        route_intent_inbound(
            &sm,
            &chat_sessions,
            "",
            InboundMessage::IntentMessage(plain),
            dir.path(),
            "node-token",
        )
        .await
        .expect("an unreferenced @ must keep the deterministic route");
        assert_eq!(
            chat_sessions.pending_turns("cs-refs"),
            pending_before,
            "without refs level 3 still fires — the router is never paid for a settled address"
        );
        assert_eq!(
            sm.try_lock().expect("free").active_sessions().len(),
            1,
            "the continuation reused the bound session"
        );
    }

    /// ADR-106 revision 2026-11-04 + ADR-107: the delivery path's reference
    /// parts are the sender's own refs first, then one `referenced_round` per
    /// pinned quadruple, rendered from disk with the router's own renderer.
    #[test]
    fn delivery_attaches_pinned_material_after_the_senders_own_refs() {
        let dir = tempfile::TempDir::new().expect("temp work dir");
        let conv = dir.path().join("conversations");
        std::fs::create_dir_all(&conv).expect("conversations dir");
        // One recorded round on disk, exactly as the runtime writer leaves it:
        // a transcript whose rows carry their round stamp, plus the side table.
        let entry = |role: &str, content: &str| crate::conversation::ConversationEntry {
            id: format!("{role}-{content}"),
            ts: "2026-11-04T00:00:00.000Z".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            metadata: Some(serde_json::json!({
                crate::rounds::META_KEY_ROUND_ID: "r:m-2"
            })),
            kind: None,
        };
        let lines: Vec<String> = [
            entry("user", "the pinned ask"),
            entry("assistant", "the pinned answer"),
        ]
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
        std::fs::write(conv.join("sess-a.jsonl"), lines.join("\n") + "\n").unwrap();
        crate::rounds::append_record(
            &crate::rounds::rounds_path(&conv, "sess-a"),
            &crate::rounds::RoundRecord {
                round_id: "r:m-2".into(),
                user: 0,
                answer: Some(1),
                card: None,
                terminal: true,
                status: Some("completed".into()),
                ts: "2026-11-04T00:00:01.000Z".into(),
            },
        )
        .unwrap();

        let params = serde_json::json!({
            "message": "汇总一下",
            // The degraded route's background is already stamped in params —
            // the material must append to it, never replace it.
            "context_refs": [{"kind": "chat_background", "content": "the chat tail"}],
            acowork_core::protocol::CHAT_REFS_PARAM: [{
                "agent_instance_id": "i-1",
                "session_id": "sess-a",
                "round_id": "r:m-2",
                "terminal": true,
            }],
        });
        let refs = context_refs_with_material(&params, dir.path());
        assert_eq!(refs.len(), 2, "{refs:?}");
        assert_eq!(refs[0], acowork_core::MessageRef::chat_background("the chat tail"));
        assert_eq!(refs[1].kind, acowork_core::REF_KIND_REFERENCED_ROUND);
        assert!(refs[1].content.starts_with("session sess-a · round r:m-2 · completed"), "{refs:?}");
        assert!(refs[1].content.contains("the pinned ask"), "{refs:?}");

        // No refs on the turn: the function is exactly the old reader.
        let plain = serde_json::json!({"message": "hi"});
        assert!(context_refs_with_material(&plain, dir.path()).is_empty());
    }

    /// ADR-099 §5: the router's failure mode is latency, never message loss.
    /// A turn the router declined or could not decide is finished by the
    /// deterministic route on the drain task; a turn it `Handled` is left alone
    /// — delivering it again would run the user's message twice.
    #[tokio::test]
    async fn a_degraded_router_turn_is_still_delivered() {
        use crate::agent::chat_router::runner::Outcome;

        let dir = tempfile::TempDir::new().expect("temp work dir");
        let (sm, _) = chat_router_test_pair(dir.path());
        let sessions = || sm.try_lock().expect("lock free").active_sessions().len();

        finish_chat_turn(
            &sm,
            Outcome::Handled,
            "",
            chat_envelope("cs-handled", "hello"),
            dir.path(),
        )
        .await
        .expect("a handled turn is already complete");
        assert_eq!(
            sessions(),
            0,
            "the drainer must not re-deliver a handled turn"
        );

        for outcome in [
            Outcome::PassThrough,
            Outcome::Fallback {
                reason: "stub: model unavailable".to_string(),
                chat_excerpt: None,
            },
        ] {
            let chat = match &outcome {
                Outcome::PassThrough => "cs-passthrough",
                _ => "cs-fallback",
            };
            finish_chat_turn(&sm, outcome, "", chat_envelope(chat, "hello"), dir.path())
                .await
                .expect("the deterministic route must deliver a declined turn");
        }
        assert_eq!(
            sessions(),
            2,
            "a degraded router turn costs latency, never the message"
        );
    }
}
