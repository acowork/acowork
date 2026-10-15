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

    let result = mqtt_only_loop(
        &session_manager,
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
    use crate::agent::inbound::InboundMessage;
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
            kind,
            in_reply_to,
            status,
        } => {
            let params: serde_json::Value =
                serde_json::from_str(&params_json).unwrap_or(serde_json::json!({}));
            Some((
                // Route to the sender-designated session when present
                // (design 27 §3.2 source_agent_session_id); otherwise the
                // empty id keeps the system-level path (cron).
                source_agent_session_id,
                InboundMessage::IntentMessage {
                    from,
                    action,
                    params,
                    message_id,
                    conversation_id,
                    source_chat_session_id,
                    repo,
                    kind,
                    in_reply_to,
                    status,
                },
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
                            lifecycle_publisher,
                            session_id,
                            msg,
                            &work_dir,
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
    lifecycle_publisher: &crate::mqtt::MqttChunkPublisher,
    session_id: String,
    msg: crate::agent::inbound::InboundMessage,
    work_dir: &std::path::Path,
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
                route_intent_inbound(session_manager, "", msg).await
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

        // ⑦ IntentMessage → receiver-side session routing (design 27 §3.2:
        // the wire addresses the intent context; which execution session
        // answers is the receiving agent's private decision).
        msg @ InboundMessage::IntentMessage { .. } => {
            route_intent_inbound(session_manager, &session_id, msg).await
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

        // ⑭ ChatMessage → SessionMessage::ChatMessage (from MQTT SendMessage)
        InboundMessage::ChatMessage {
            content,
            message_id,
            command,
            params_json,
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
            let mut skill_instructions: Option<String> = None;
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
                        skill_instructions = Some(instructions);
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
                        skill_instructions,
                        attached_items,
                        content_parts,
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
async fn route_intent_inbound(
    session_manager: &Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    designated_session_id: &str,
    msg: crate::agent::inbound::InboundMessage,
) -> crate::error::Result<()> {
    use crate::agent::inbound::InboundMessage;
    use crate::agent::intent_turn::render_intent_message;
    use crate::agent::session::intent_routing::routing_key;
    use crate::agent::session::SessionMessage;
    use crate::error::RuntimeError;

    let InboundMessage::IntentMessage {
        from,
        action,
        params,
        message_id,
        conversation_id,
        source_chat_session_id,
        repo,
        kind,
        in_reply_to,
        status,
    } = msg
    else {
        return Err(RuntimeError::Config(
            "route_intent_inbound called with a non-intent message".to_string(),
        ));
    };

    let is_response = kind == "response";
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

    // Candidate session: the designated one for responses, registry for
    // everything else.
    let mut target: Option<String> = None;
    if is_response && !designated_session_id.is_empty() {
        let guard = session_manager.lock().await;
        if guard.get_session(designated_session_id).is_some() {
            target = Some(designated_session_id.to_string());
        }
    }
    let target = match target {
        Some(t) => (t, None),
        None => {
            let key = routing_key(&source_chat_session_id, &conversation_id, &message_id);
            session_manager
                .lock()
                .await
                .route_intent_session(&key)
                .await?
        }
    };
    // A rotation (§2.1) prepends its pointer handoff to the routed intent —
    // one injection with the turn it belongs to, no extra plumbing.
    let content = match target.1 {
        Some(note) => format!("{}\n\n{}", note, content),
        None => content,
    };

    session_manager.lock().await.send_to_session(
        &target.0,
        SessionMessage::Intent {
            content,
            message_id,
            conversation_id,
            source_chat_session_id,
            from,
            action,
            repo,
            kind,
        },
    )
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
            kind: String::new(),
            in_reply_to: String::new(),
            status: String::new(),
        })
        .expect("Intent must map");
        assert_eq!(route, "", "cron intent has no target session — system-level");

        match msg {
            InboundMessage::IntentMessage {
                from,
                action,
                params,
                message_id,
                ..
            } => {
                assert_eq!(from, "cron:agent");
                assert_eq!(action, "hourly_check");
                assert_eq!(params["k"], 1);
                assert_eq!(message_id, "m-1");
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
            kind: String::new(),
            in_reply_to: String::new(),
            status: String::new(),
        })
        .expect("Intent must still map");

        match msg {
            InboundMessage::IntentMessage { params, .. } => assert!(params.is_object()),
            other => panic!("expected IntentMessage, got {other:?}"),
        }
    }
}