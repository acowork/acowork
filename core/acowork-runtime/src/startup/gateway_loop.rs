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
            for_execution_session,
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
                    for_execution_session,
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
) -> Result<()> {
    tracing::info!("MQTT-only gateway loop started");
    let work_dir = std::path::PathBuf::from(work_dir);

    loop {
        tokio::select! {
            // ── ADR-034 §8 Phase 2-1: single dispatch table ────────────
            dispatch_result = mqtt_dispatch_rx.recv() => {
                match dispatch_result {
                    Some((session_id, msg)) => {
                        if let Err(e) = dispatch_inbound(
                            session_manager,
                            chat_sessions,
                            lifecycle_publisher,
                            session_id,
                            msg,
                            &work_dir,
                            node_token.unwrap_or(""),
                        ).await {
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

            session_manager
                .lock()
                .await
                .send_to_session(
                    &session_id,
                    SessionMessage::ChatMessage {
                        content,
                        message_id,
                        active_skill,
                        attached_items,
                        content_parts,
                        // Written straight against the execution surface: no
                        // Chat row precedes it, so there is nothing to thread
                        // a card under (ADR-100 D12). The session's own chat
                        // binding still supplies the chat and the caller
                        // supplies who can answer, so `session_task` completes
                        // the surface there and the card lands standalone —
                        // otherwise a group chat never saw the question a
                        // resumed run had stopped on (ADR-100 Rev. 6).
                        surface: crate::agent::intent_turn::TurnSurface::new(
                            String::new(),
                            String::new(),
                            &answerable_by,
                            None,
                        ),
                    },
                )
                .map_err(|e| RuntimeError::Config(format!("ChatMessage: {}", e)))
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

    let InboundMessage::IntentMessage(mut env) = msg else {
        return Err(RuntimeError::Config(
            "route_intent_inbound called with a non-intent message".to_string(),
        ));
    };

    // ADR-100 D12: a message composed inside one execution session's own
    // view already names its answer target — "who runs this" was settled by
    // where the human typed it. Skip the router queue entirely: no routing
    // LLM call, no routing-reaction row. The pin is only a hint,
    // so a session that is not loaded has its hint dropped and the turn takes
    // the ordinary route below (a stale id costs a hop, never the message).
    if !env.for_execution_session.trim().is_empty() {
        let pinned = env.for_execution_session.clone();
        if deliver_pinned_turn(session_manager, &pinned, &env).await {
            tracing::debug!(
                session_id = %pinned,
                chat_id = %env.source_chat_session_id,
                "pinned turn delivered straight to the named execution session"
            );
            return Ok(());
        }
        tracing::warn!(
            session_id = %pinned,
            chat_id = %env.source_chat_session_id,
            "pinned execution session is not loaded — routing this turn normally"
        );
        env.for_execution_session.clear();
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
            );
        }
        return Ok(());
    }

    // Router off: the deterministic route, unchanged (ADR-099).
    route_intent_legacy(
        session_manager,
        designated_session_id,
        env,
        node_token,
        false,
    )
    .await
}

/// Deliver a pinned turn (ADR-100 D12) straight to the execution session the
/// human composed it in, reporting whether that session took it.
///
/// The turn lands as [`SessionMessage::ChatMessage`] — the very variant the
/// execution surface's own write endpoint feeds — and not as an `Intent`.
/// That choice is what makes the pinned path capability-preserving: a per-turn
/// skill (`command`) and the ADR-046 attachment union are fields of
/// `ChatMessage` and exist nowhere on the Intent shape, so delivering a pinned
/// turn through the Intent path would silently drop both. `Intent` is the
/// agent-to-agent protocol; this turn is not agent-to-agent — it is the human
/// speaking to the task already on their screen.
///
/// The turn's `message_id` is the Chat row the Gateway wrote for it (the
/// client mints one id and both surfaces use it), which is what lets the
/// execution transcript, the chat thread and the frontend's optimistic bubble
/// settle on a single message instead of three.
///
/// Chat attachments are deliberately NOT pulled here. On this path the
/// execution session *is* the attachment's home: a `file_upload` blob already
/// sits in this agent's `files/` under the content-hash id the prompt hint
/// resolves, and a workspace reference reads straight off the node filesystem.
/// Pulling the Gateway's copy as well would hand the turn the same file twice.
///
/// `false` = the named session is not loaded in this agent. The caller then
/// drops the hint and routes normally; a pin can delay a turn, never lose one.
async fn deliver_pinned_turn(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    session_id: &str,
    env: &crate::agent::inbound::IntentEnvelope,
) -> bool {
    use crate::agent::session::SessionMessage;

    // Same ADR-046 parse the routed path uses — one parse, one meaning for a
    // reference whichever transport carried the turn.
    let attached_items = crate::agent::intent_turn::attached_items_from_params(&env.params);
    let content_parts = env
        .params
        .get("content_parts")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| {
                    serde_json::from_value::<acowork_core::providers::traits::ContentPart>(
                        p.clone(),
                    )
                    .ok()
                })
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<acowork_core::providers::traits::ContentPart>| !v.is_empty());
    let content = env
        .params
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if content.is_empty() && attached_items.is_none() && content_parts.is_none() {
        // Nothing to run. This mirrors the guard the SessionTask applies on
        // arrival, checked up here so a textless, attachment-less pinned send
        // is settled as done instead of waking a session only to be dropped
        // with a warning — and instead of falling through to the router, which
        // would spend a routing call on an empty turn. The Chat row the
        // Gateway already wrote stays the honest record of what was sent.
        return true;
    }

    // Per-turn skill: the client sends only the NAME and the Runtime owns the
    // instructions, resolved against the agent's loaded SkillRegistry here —
    // identical to the write path, so skill content is never trusted to the
    // client. An unknown command degrades to "no skill this turn".
    let command = env
        .params
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let active_skill = if command.is_empty() {
        None
    } else {
        let resolved = session_manager
            .lock()
            .await
            .resolve_skill_instructions(command);
        match &resolved {
            Some(instructions) => tracing::info!(
                session_id = %session_id,
                skill = %command,
                skill_len = instructions.len(),
                "pinned turn: resolved skill command → instructions"
            ),
            None => tracing::warn!(
                session_id = %session_id,
                command = %command,
                "pinned turn: command did not match any loaded skill, ignoring"
            ),
        }
        resolved
    };

    // ADR-100 D12: the pin may only name a session that belongs to the chat this
    // send came from. "Loaded in this agent" is not enough — one agent instance
    // serves many chats, and session ids travel on rows other people can read (a
    // group's `meta.agent_session_id` is visible to every member). Without this
    // check a person could inject text, a per-turn skill and node-local
    // attachment paths into somebody else's task, and that task's answer would be
    // written into a chat the asker has no row in — while the asker's own chat
    // kept the head of a turn that was never run there.
    //
    // Refusing returns `false`, which is the pin's existing "hint, not gate"
    // contract: the hint is dropped and the turn routes normally, so the sender
    // still reaches this agent — just not through a session that is not theirs.
    let (bound_chat, initiator) = session_manager
        .lock()
        .await
        .get_session(session_id)
        .and_then(|handle| {
            let conv = handle.conversation.as_ref()?;
            Some((conv.chat_session_id(), conv.initiator()))
        })
        .unwrap_or((None, None));
    let Some(bound_chat) = bound_chat.filter(|c| !c.is_empty()) else {
        return false;
    };
    if bound_chat != env.source_chat_session_id.trim() {
        tracing::warn!(
            session_id = %session_id,
            pinned_chat = %bound_chat,
            sender_chat = %env.source_chat_session_id,
            "pinned execution session belongs to another chat — dropping the pin and routing normally"
        );
        return false;
    }
    // ADR-076 §决策 4: the session's initiator is the only identity allowed to
    // write it, and the pin IS the execution surface's write path — a turn
    // never passes through `SessionMeta::is_writable_by`, which guards the HTTP
    // control plane. Without this check the single-initiator rule would live
    // only in the Desktop's disabled composer: every member of a group can read
    // the run's `agent_session_id` off the chat rows they hold, name it here,
    // and type into somebody else's task.
    //
    // Refusing returns `false`, the pin's existing "hint, not gate" contract:
    // the sender still reaches this agent, just in a session of its own.
    match initiator.as_deref().map(str::trim) {
        // Pre-accounts / local-mode rows carry no initiator — nobody to
        // exclude, the same rule `is_writable_by` applies to them.
        None | Some("") => {}
        Some(owner) if owner != env.from.trim() => {
            tracing::warn!(
                session_id = %session_id,
                pinned_initiator = %owner,
                sender = %env.from,
                "pinned execution session was initiated by somebody else — dropping the pin and routing normally"
            );
            return false;
        }
        Some(_) => {}
    }
    session_manager
        .lock()
        .await
        .send_to_session(
            session_id,
            SessionMessage::ChatMessage {
                content,
                message_id: env.message_id.clone(),
                active_skill,
                attached_items,
                content_parts,
                // ADR-100 follow-up: the pinned turn's Chat obligations come
                // from the envelope exactly like the Intent transport's — one
                // producer, so the row this turn answers, the human who can
                // answer a card, and the chat's human-presence fact can no
                // longer be dropped by the conversion that switches it to the
                // ChatMessage transport.
                surface: crate::agent::intent_turn::TurnSurface::from_envelope(env),
            },
        )
        .is_ok()
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
            let turn_result = tokio::spawn(async move {
                let outcome =
                    crate::agent::chat_router::runner::handle_intent(&sm, &cs, &envelope, &token)
                        .await;
                finish_chat_turn(&sm, outcome, &designated, envelope, &token)
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

/// The degraded route's one-shot brief for a fresh execution session.
///
/// `None` means "there is nothing to say": the router captured no excerpt, or
/// the turn had neither words nor files — the caller then leaves
/// `params.message` exactly as delivered. A turn that carried files but no
/// text still gets the excerpt: the router failing to decide may not surface
/// as an empty message in the execution session, because a human who attached
/// something instead of typing still asked for work.
fn degraded_brief(text: &str, has_attachments: bool, excerpt: Option<&str>) -> Option<String> {
    const HEADER: &str =
        "[recent chat — the routing assistant was unavailable; the conversation so far]";
    let excerpt = excerpt?;
    let text = text.trim();
    if text.is_empty() && !has_attachments {
        return None;
    }
    Some(if text.is_empty() {
        format!("{HEADER}\n{excerpt}")
    } else {
        format!("{text}\n\n{HEADER}\n{excerpt}")
    })
}

/// Deliver a queued turn according to what the router decided.
///
/// The router owns a turn it `Handled` — it either answered it or dispatched it,
/// and a failure it degraded on its own already left a visible notice. Every
/// other outcome is finished by [`route_intent_legacy`] on this task, so a
/// router that cannot decide costs the chat latency and never the message
/// (ADR-099 §5). A fallback must not re-enter the queue: it is the same
/// envelope, already claimed.
pub(crate) async fn finish_chat_turn(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    outcome: crate::agent::chat_router::runner::Outcome,
    designated_session_id: &str,
    mut envelope: Box<crate::agent::inbound::IntentEnvelope>,
    node_token: &str,
) -> crate::error::Result<()> {
    use crate::agent::chat_router::runner::Outcome;

    match outcome {
        Outcome::Handled => Ok(()),
        Outcome::PassThrough => {
            route_intent_legacy(
                session_manager,
                designated_session_id,
                envelope,
                node_token,
                false,
            )
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
            // The degraded route has no LLM-composed brief for a fresh
            // session. When the router HAD read the conversation before it
            // failed, hand that bounded mechanical tail along: it is what
            // the router saw, not what it concluded, so it re-briefs the
            // new session without re-introducing another task's noise.
            // `envelope` is an owned Box and this arm is its only consumer
            // — an in-place edit is free.
            //
            // An attachment-only turn has no text to prefix, and it must
            // still get the excerpt: "the router could not decide" may not
            // degrade into "the execution session received an empty message"
            // merely because the human attached instead of typing.
            //
            // Straight through the router's own reader rather than a second
            // reading of the raw param: the raw array and "did this turn carry
            // files?" disagree on a degenerate element (`[{}]`), and Block C,
            // the decision log and this brief must never disagree about it.
            let has_attachments =
                !crate::agent::chat_router::runner::turn_attachments(&envelope).is_empty();
            let text = envelope
                .params
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            if let Some(brief) = degraded_brief(&text, has_attachments, chat_excerpt.as_deref())
                && let Some(map) = envelope.params.as_object_mut()
            {
                map.insert("message".to_string(), serde_json::json!(brief));
            }
            // ADR-099 revision: a chat-sourced human message degraded by the
            // router gets a FRESH execution session, not the mapped one — the
            // mapping is not a semantic verdict and continuing an unrelated old
            // task on a guess is worse than starting a new one. PM tasks and
            // agent-agent intents keep the deterministic reuse (their
            // conversation_id / msg key IS the task identity).
            route_intent_legacy(
                session_manager,
                designated_session_id,
                envelope,
                node_token,
                true,
            )
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
    node_token: &str,
    fresh: bool,
) -> crate::error::Result<()> {
    use crate::agent::inbound::IntentEnvelope;
    use crate::agent::intent_turn::render_intent_message;
    use crate::agent::session::SessionMessage;
    use crate::agent::session::intent_routing::routing_key;

    let IntentEnvelope {
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
        // A pinned turn never reaches this function (ADR-100 D12 handles it
        // upstream); the name is dropped so the deterministic route stays
        // exactly what it was before the pin existed.
        for_execution_session: _,
    } = *env;

    let origin = acowork_core::intent::TurnOrigin::parse(&kind);
    let is_response = origin == acowork_core::intent::TurnOrigin::AgentResponse;
    // A PM dispatch's wake-up playbook is model-facing and ephemeral: it
    // bypasses the rendered message and history entirely, injected into
    // the turn's system block (see `ContextBuilder::set_task_brief`).
    // Origin-gated: a peer's params are never trusted with the system
    // block (`extract_assignment_brief`).
    let assignment_brief = crate::agent::intent_turn::extract_assignment_brief(&kind, &params);
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

    // Candidate session: the designated one for responses, registry otherwise.
    let mut target: Option<String> = None;
    if is_response && !designated_session_id.is_empty() {
        let guard = session_manager.lock().await;
        if guard.get_session(designated_session_id).is_some() {
            target = Some(designated_session_id.to_string());
        }
    }
    let target = match target {
        Some(t) => (t, false),
        None => {
            // The initiator's session-config suggestion (§4) is consumed on
            // whichever branch the routing takes: at creation here, or applied
            // to the session the message was routed into (ADR-099).
            let seed =
                crate::agent::session::intent_routing::SessionSeed::from_json(&session_config_json);
            let is_chat_human = origin.policy().router_eligible && !source_chat_session_id.is_empty();
            if fresh && is_chat_human {
                // Degraded router turn: fresh session, bound to the same
                // chat, mapping re-pointed — the same creation path the
                // router's own `dispatch(new)` uses.
                let mut guard = session_manager.lock().await;
                guard
                    .chat_dispatch(
                        &source_chat_session_id,
                        crate::agent::chat_router::tool::Target::New,
                        seed,
                        Some(from.as_str()),
                    )
                    .await?
            } else {
                let key = routing_key(
                    &kind,
                    &source_chat_session_id,
                    &from,
                    &conversation_id,
                    &message_id,
                );
                session_manager
                    .lock()
                    .await
                    .route_intent_session(
                        &key,
                        seed,
                        // ADR-099: a PM task dispatched from a group chat binds
                        // that session to the group, so follow-ups there can find
                        // it. Empty for agent-to-agent and cron intents.
                        Some(source_chat_session_id.as_str()),
                        // ADR-076 §决策 4: the session's initiator is the
                        // envelope's `from` — the human who sent the message,
                        // or the agent instance that started the work. Never
                        // the account this Runtime happens to be carrying.
                        Some(from.as_str()),
                    )
                    .await?
            }
        }
    };
    // §5.5.5 pending queue: which intents must be shown in the execution
    // side's pending list while that session is busy. A Chat-sourced turn that
    // owes its chat a write-back is exactly that set — no chat row is written
    // here (ADR-100 revision: dispatch/start milestones are runtime-only).
    let pointer = if !source_chat_session_id.is_empty()
        && crate::agent::intent_turn::turn_kind_semantics(&kind, &message_id).1
    {
        Some((
            message_id.clone(),
            source_chat_session_id.clone(),
            from.clone(),
        ))
    } else {
        None
    };
    // §5.5.5 pending queue: record BEFORE delivering, mirroring the router
    // path (runner.rs). Recording after the send raced with an idle session
    // that woke and started the turn at once — its empty removal ran first,
    // leaving the row we then pushed stuck in the queue forever. A busy
    // session is active now (row recorded, cleared at dequeue); an idle one
    // is not (record is a no-op — it starts immediately).
    if let Some((pending_message_id, _, pending_from)) = pointer.as_ref() {
        session_manager.lock().await.record_pending_intent(
            &target.0,
            crate::agent::session_state::PendingIntent {
                message_id: pending_message_id.clone(),
                from: pending_from.clone(),
                preview: crate::agent::session_state::pending_preview(
                    params.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                ),
                from_chat: true,
            },
        );
    }
    let opened_new = target.1;
    // ADR-100 follow-up: this turn's Chat obligations, produced from the same
    // envelope fields the route already read. One producer for every
    // transport: the loop's card writer, the human-awaiting gate and the
    // terminal echo all read this value instead of re-deriving it.
    let surface = crate::agent::intent_turn::TurnSurface::new(
        source_chat_session_id,
        message_id.clone(),
        &from,
        has_human,
    );
    let sent = session_manager.lock().await.send_to_session(
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
        },
    );
    // ADR-100 D11: the routing reaction row — the same conversational fact the
    // router path writes after a successful dispatch ("your message went to a
    // new task / continues task X"), here for every turn that owes its chat a
    // write-back: the router-off human turn and the PM dispatch alike. Written
    // only once the intent is actually delivered, best-effort: a lost row
    // costs the reaction, never the turn.
    if let Some((reaction_message_id, reaction_chat_id, reaction_from)) =
        pointer.as_ref().filter(|_| sent.is_ok())
    {
        let (instance_id, gateway_host) = {
            let guard = session_manager.lock().await;
            let core = guard.core();
            (
                core.config.agent_instance_id.clone(),
                core.config
                    .gateway_host
                    .clone()
                    .map(|h| h.trim().to_string())
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| "127.0.0.1".to_string()),
            )
        };
        if let Err(e) = crate::agent::intent_turn::append_chat_row(
            &instance_id,
            &gateway_host,
            node_token,
            reaction_chat_id,
            "system_notice",
            reaction_from,
            "",
            crate::agent::intent_turn::turn_routed_meta(reaction_message_id, &target.0, opened_new),
        )
        .await
        {
            tracing::warn!(
                chat_id = %reaction_chat_id,
                message_id = %reaction_message_id,
                error = %e,
                "routing reaction row failed — non-fatal"
            );
        }
    }
    sent
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
            let reason = match manager_guard.get_lifecycle_state(session_id, work_dir) {
                crate::agent::session::SessionLifecycleState::Closed => "session_closed",
                _ => "session_not_found",
            };
            tracing::warn!(
                session_id = %session_id,
                attempted_command = %attempted_command,
                reason = %reason,
                "session not Active: forwarding SessionNotOpened",
            );
            // Best-effort publish — never block on the broker.
            let publisher = lifecycle_publisher.clone();
            let sid = session_id.to_string();
            let cmd = attempted_command.to_string();
            let reason_owned = reason.to_string();
            tokio::spawn(async move {
                let _ = publisher
                    .publish_session_not_opened(&sid, &cmd, &reason_owned)
                    .await;
            });
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

    /// A router that fell back still owes the fresh execution session a
    /// brief. The turn that most needs it is the one with no text at all: the
    /// human attached a file and typed nothing, so "excerpt only" is the
    /// difference between a briefed session and an empty message.
    #[test]
    fn a_degraded_attachment_only_turn_still_carries_the_excerpt() {
        let brief = degraded_brief("", true, Some("U: (nothing)\nA: hi")).unwrap();
        assert!(brief.starts_with("[recent chat"));
        assert!(brief.ends_with("A: hi"));
        assert!(!brief.starts_with('\n'));

        // Text on the turn prefixes the excerpt; whitespace is not text.
        assert_eq!(
            degraded_brief("fix the build", false, Some("tail")).unwrap(),
            "fix the build\n\n[recent chat — the routing assistant was unavailable; the conversation so far]\ntail"
        );
        assert!(degraded_brief("  \n ", true, Some("tail")).is_some());

        // Nothing to say: no excerpt captured, or no excerpt and no files.
        assert!(degraded_brief("hello", false, None).is_none());
        assert!(degraded_brief("   ", false, Some("tail")).is_none());
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
            for_execution_session: String::new(),
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
            for_execution_session: String::new(),
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
            for_execution_session: String::new(),
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
            "node-token",
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
            finish_chat_turn(&sm, outcome, "", chat_envelope(chat, "hello"), "node-token")
                .await
                .expect("the deterministic route must deliver a declined turn");
        }
        assert_eq!(
            sessions(),
            2,
            "a degraded router turn costs latency, never the message"
        );
    }

    // ── ADR-100 D12: a turn pinned to one execution session ──────────────

    /// A human Chat message that also names the execution session it was
    /// composed in — the shape an execution view's composer sends.
    fn pinned_envelope(chat_id: &str, text: &str, exec_session: &str) -> Box<IntentEnvelope> {
        let mut env = chat_envelope(chat_id, text);
        env.for_execution_session = exec_session.to_string();
        env
    }

    /// Bring up one loaded execution session through the ordinary routing
    /// path and hand back its id — the target a pinned turn names.
    async fn spawn_session(
        sm: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
        key: &str,
    ) -> String {
        sm.lock()
            .await
            .route_intent_session(
                key,
                crate::agent::session::intent_routing::SessionSeed::from_json(""),
                None,
                None,
            )
            .await
            .expect("test session")
            .0
    }

    /// The pin is the whole point: no routing LLM call, no
    /// routing-reaction row. The human already knows which task this is —
    /// they typed it while looking at that task's own transcript.
    #[tokio::test]
    async fn a_pinned_turn_never_reaches_the_router_queue() {
        let dir = tempfile::TempDir::new().expect("temp work dir");
        let (sm, chat_sessions) = chat_router_test_pair(dir.path());
        let target = spawn_session(&sm, "chat:cs-pin").await;

        route_intent_inbound(
            &sm,
            &chat_sessions,
            "",
            InboundMessage::IntentMessage(pinned_envelope("cs-pin", "keep going", &target)),
            dir.path(),
            "node-token",
        )
        .await
        .expect("a pinned turn is delivered, not refused");

        assert!(
            !chat_sessions.chat_is_busy("cs-pin"),
            "a pinned turn is not a router turn — it must not claim a drain"
        );
        assert_eq!(
            chat_sessions.pending_turns("cs-pin"),
            0,
            "nor may it queue: skipping the routing call is what the pin buys"
        );
    }

    /// A pin is a hint, never a gate. A session that is not loaded — closed,
    /// or an id carried across a restart — must not cost the human their
    /// message: the hint is dropped and the turn routes the ordinary way.
    #[tokio::test]
    async fn a_stale_pin_falls_through_instead_of_losing_the_turn() {
        let dir = tempfile::TempDir::new().expect("temp work dir");
        let (sm, chat_sessions) = chat_router_test_pair(dir.path());

        route_intent_inbound(
            &sm,
            &chat_sessions,
            "",
            InboundMessage::IntentMessage(pinned_envelope(
                "cs-stale",
                "hello",
                "session-that-is-not-loaded",
            )),
            dir.path(),
            "node-token",
        )
        .await
        .expect("a stale pin still delivers");

        assert!(
            chat_sessions.chat_is_busy("cs-stale"),
            "the turn falls back to the router rather than vanishing with the pin"
        );
    }

    /// ADR-076 §决策 4: the pin may not carry a turn into a session somebody
    /// else initiated. Everything else about the pin passes here — the session
    /// is loaded and bound to the sender's own chat, because a group member
    /// reads its id off the chat rows they hold — so without the initiator
    /// check the single-initiator rule would live only in the Desktop's
    /// disabled composer.
    #[tokio::test]
    async fn a_pin_naming_another_members_session_does_not_take_that_turn() {
        let dir = tempfile::TempDir::new().expect("temp work dir");
        let (sm, _chat_sessions) = chat_router_test_pair(dir.path());
        let key = crate::agent::session::intent_routing::chat_key("cs-group", "u-alice");
        let alice_sid = sm
            .lock()
            .await
            .route_intent_session(&key, None, Some("cs-group"), Some("u-alice"))
            .await
            .expect("alice's run")
            .0;

        let mut alice = chat_envelope("cs-group", "keep going");
        alice.from = "u-alice".into();
        assert!(
            deliver_pinned_turn(&sm, &alice_sid, &alice).await,
            "the initiator's own pin is delivered to her run"
        );

        let mut bob = chat_envelope("cs-group", "do my bidding");
        bob.from = "u-bob".into();
        assert!(
            !deliver_pinned_turn(&sm, &alice_sid, &bob).await,
            "a fellow group member must not write into alice's run"
        );

        // An agent-initiated session has no human initiator, so no human pin
        // may enter it either.
        let agent_sid = sm
            .lock()
            .await
            .route_intent_session(
                &crate::agent::session::intent_routing::chat_key("cs-group", "agent-inst"),
                None,
                Some("cs-group"),
                Some("agent-inst"),
            )
            .await
            .expect("agent-initiated run")
            .0;
        assert!(!deliver_pinned_turn(&sm, &agent_sid, &alice).await);
    }

    /// A textless, attachment-less send is not a turn at all. It is settled
    /// before the target session is even looked up, so it cannot wake a
    /// session only to have the message dropped with a warning downstream.
    #[tokio::test]
    async fn a_pinned_turn_with_nothing_to_say_wakes_no_session() {
        let dir = tempfile::TempDir::new().expect("temp work dir");
        let (sm, _chat_sessions) = chat_router_test_pair(dir.path());
        let mut env = chat_envelope("cs-empty", "   ");
        env.for_execution_session = "session-that-was-never-loaded".into();

        assert!(
            deliver_pinned_turn(&sm, "session-that-was-never-loaded", &env).await,
            "nothing to run is a settled outcome, not a fall-through to routing"
        );
    }

    /// ADR-100 D12: a pin naming somebody else's session is not a delivery.
    /// The session exists and is loaded in this very agent — it is simply bound
    /// to a different chat — so the hint is dropped and the turn takes the
    /// ordinary route rather than running inside a foreign task.
    #[tokio::test]
    async fn a_pin_naming_another_chats_session_does_not_take_that_turn() {
        let dir = tempfile::TempDir::new().expect("temp work dir");
        let (sm, chat_sessions) = chat_router_test_pair(dir.path());
        // Bound to `chat:cs-victim` by its routing key, the way a task started
        // from that conversation is.
        let victim = spawn_session(&sm, "chat:cs-victim").await;

        route_intent_inbound(
            &sm,
            &chat_sessions,
            "",
            InboundMessage::IntentMessage(pinned_envelope("cs-attacker", "do my bidding", &victim)),
            dir.path(),
            "node-token",
        )
        .await
        .expect("a refused pin still delivers the turn");

        assert!(
            chat_sessions.chat_is_busy("cs-attacker"),
            "the turn falls back to the router in the sender's own chat"
        );
        assert!(
            !chat_sessions.chat_is_busy("cs-victim"),
            "and the victim's conversation is never touched"
        );
    }
}
