//! Phase C: spawn subsystems.
//!
//! Covers the latter part of Step 9:
//!   - Spawn chunk_relay task first so the chunk channel is draining
//!   - DevMode: start Debug Protocol server if --dev-mode
//!   - Sync agent_mcp.json catalog from Gateway hello
//!   - Spawn MCP auto-connect background task
//!   - Send workspace config snapshot to Gateway

use crate::config::RuntimeConfig;
use crate::conversation::{ConfigChange, ConversationSession, StateChange};
use crate::error::Result;
use crate::http::SharedAgentCore;
use crate::startup::context::{AgentBootContext, SessionBootContext};
use acowork_core::mqtt_proto::StreamLine;

/// Re-export the wire enum under the runtime alias to avoid a name
/// collision with [`crate::agent::LlmAvailability`] (the runtime-side
/// view) in this module.
use crate::agent::llm_availability::WireAvailability;

/// Future that resolves when the LLM availability watch channel fires.
///
/// When there is no registry (standalone mode without
/// `available_cache`) the branch pends forever, keeping the
/// availability fan-out disabled.
async fn llm_changed(
    rx: &mut Option<tokio::sync::watch::Receiver<crate::agent::LlmAvailability>>,
) -> bool {
    match rx.as_mut() {
        Some(r) => r.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Resources produced by Phase C, needed by Phase D.
pub(crate) struct SubsystemHandles {
    /// chunk_relay task join handle (Gateway mode only).
    pub chunk_relay: Option<tokio::task::JoinHandle<()>>,
    /// MCP startup result receiver (Gateway mode only).
    pub mcp_startup_rx:
        Option<tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>>,
    /// Runtime MCP channel used by run_gateway_loop.
    pub mcp_runtime_tx: tokio::sync::mpsc::Sender<crate::tools::mcp_manager::McpConnectResult>,
    pub mcp_runtime_rx: tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
}

/// Phase C: spawn background subsystems (Gateway mode).
///
/// After this phase the agent is functionally ready:
/// - chunk_relay is running and draining the chunk channel
/// - MCP auto-connect is progressing in the background
pub(crate) async fn phase_c_spawn_subsystems(
    ctx: &mut AgentBootContext,
    session_ctx: &mut SessionBootContext,
    config: &RuntimeConfig,
) -> Result<SubsystemHandles> {
    let _span = tracing::info_span!("startup_phase_c").entered();

    let work_dir_path = std::path::Path::new(&config.work_dir);

    // ── LLM availability registry ───────────────────────────────────
    // Polls SharedAvailableCache for bootstrap phase + AvailableProviders
    // transitions and re-publishes every active session's config snapshot
    // when the value changes. Created only when the MQTT path is wired
    // (the chunk_relay task below is the only consumer).
    let llm_availability = ctx.available_cache.as_ref().map(|cache| {
        std::sync::Arc::new(
            crate::agent::llm_availability::LlmAvailabilityRegistry::new(cache.clone()),
        )
    });

    // ── Spawn chunk relay task first ─────────────────────────────────
    // This must run before the session-ready signal reaches the Desktop
    // (ADR-085 `SESSIONS_READY`; historically the `AgentReady` message)
    // so the chunk channel is already being drained when the Gateway
    // loop starts and the first chat turn streams.
    //
    // ADR-033: MQTT chunk relay takes priority over gRPC when MQTT is
    // available. All chunk events are published to the MQTT broker as
    // `DataEnvelope { payload: SessionMessage }` protobuf on session
    // message topics. Desktop subscribes directly to the broker.
    //
    // The relay also listens for LLM availability transitions: when the
    // Registry fires, every active session's `SessionConfig` snapshot is
    // re-published with the current `llm_availability` tag.
    let agent_id_for_relay = ctx.agent_id.clone();
    let chunk_relay = if ctx.chunk_rx.is_some() {
        if let Some(ref mqtt_client) = ctx.mqtt_client {
            // MQTT chunk relay — publish all events via MQTT broker.
            let chunk_rx = ctx.chunk_rx.take().unwrap();
            let chunk_publisher = crate::mqtt::MqttChunkPublisher::from_runtime_client(mqtt_client);
            let session_manager_arc = std::sync::Arc::clone(&session_ctx.session_manager);
            let llm_rx = llm_availability.as_ref().map(|r| r.subscribe());
            let llm_registry_arc = llm_availability.clone();
            Some(tokio::spawn(async move {
                tracing::info!("MQTT chunk relay started");
                let mut chunk_rx = chunk_rx;
                // `biased` keeps chunk events strictly ahead of LLM
                // availability re-publishes so streaming output is never
                // queued behind a per-session config fan-out. When
                // llm_rx is `None` (no available_cache, e.g. standalone
                // mode), the branch is permanently disabled.
                let mut llm_rx = llm_rx;
                loop {
                    tokio::select! {
                        biased;
                        evt = chunk_rx.recv() => {
                            match evt {
                                Some(mut session_event) => {
                                    // Stamp the authoritative availability level on
                                    // every SessionConfigChanged publish, not just the
                                    // transition fan-out below.
                                    // `build_session_config_snapshot` defaults the field
                                    // to Unspecified; this single choke point keeps all
                                    // per-session config re-publishes level-correct
                                    // without threading the registry through
                                    // ConversationSession or the config-change relay.
                                    if let crate::agent::loop_::ChunkEvent::SessionConfigChanged { config } =
                                        &mut session_event.event
                                        && let Some(reg) = llm_registry_arc.as_ref()
                                    {
                                        let stamped = reg.current().as_i32();
                                        tracing::info!(
                                            target: "llm_avail_diag",
                                            session_id = %session_event.session_id,
                                            stamped_value = stamped,
                                            before = config.llm_availability,
                                            "DIAG: stamping llm_availability"
                                        );
                                        config.llm_availability = stamped;
                                    } else if let crate::agent::loop_::ChunkEvent::SessionConfigChanged { config: _ } =
                                        &mut session_event.event
                                    {
                                        tracing::info!(
                                            target: "llm_avail_diag",
                                            session_id = %session_event.session_id,
                                            registry_available = false,
                                            "DIAG: NOT stamping llm_availability — registry is None"
                                        );
                                    }
                                    relay_chunk_event_mqtt(
                                        &chunk_publisher,
                                        &agent_id_for_relay,
                                        &session_event.session_id,
                                        session_event.event,
                                    )
                                    .await;
                                }
                                None => break,
                            }
                        }
                        changed = llm_changed(&mut llm_rx) => {
                            if !changed {
                                continue;
                            }
                            // Borrow the latest value, mark it seen.
                            let next = match llm_rx.as_mut() {
                                Some(rx) => *rx.borrow_and_update(),
                                None => continue,
                            };
                            // Fan out: snapshot every active session
                            // (lock is held only for the HashMap iteration)
                            // and re-publish each config with the new tag.
                            let snapshot = {
                                let guard = session_manager_arc.lock().await;
                                guard.snapshot_active_conversations()
                            };
                            let wire: WireAvailability = next.as_wire();
                            for (sid, conv) in snapshot {
                                // Build the fresh snapshot with the new
                                // llm_availability tag. The reasoning_effort
                                // already stored on the ConversationSession
                                // is reused — the per-config-change relay
                                // (spawn_config_change_relay) is the only
                                // path that re-resolves effective effort,
                                // because model/provider changes are the
                                // trigger that needs re-resolution.
                                let cfg = conv.build_session_config_snapshot(wire);
                                chunk_publisher.publish_session_config(&sid, &cfg).await;
                            }
                        }
                    }
                }
                tracing::debug!("MQTT chunk relay task ended");
            }))
        } else {
            None
        }
    } else {
        None
    };

    // Spawn the registry poller now that the relay is ready to consume.
    // The poller task holds an Arc<Self>, so no external handle is
    // required — it terminates with the runtime process.
    if let Some(reg) = llm_availability.as_ref() {
        let _poller = std::sync::Arc::clone(reg).spawn_poller();
    }

    // ── DevMode: register Debug HTTP routes + spawn events publisher ──
    if config.dev_mode {
        let debug_port = config.debug_port as u32;
        tracing::info!(
            debug_port = debug_port,
            "DevMode enabled at startup — registering Debug Protocol routes and MQTT events publisher"
        );
        // ADR-048 follow-up: route the startup wiring through the
        // shared helper so the runtime `/api/debug/enable` route uses
        // exactly the same code path. The helper is idempotent — when
        // the slot is already filled (e.g. a previous enable call
        // flipped DevMode on before startup finished) it returns
        // AlreadyEnabled without re-running the wiring.
        let outcome = crate::startup::debug_enable::enable_debug_mode_and_fill_slot(
            &ctx.debug_service_slot,
            &ctx.mqtt_client_slot,
            &ctx.session_manager_slot,
            debug_port,
        )
        .await;
        tracing::info!(?outcome, debug_port, "Phase C DevMode activation outcome");

        // ADR-063 §3.7.5 L2 reload: when DevMode is enabled at startup,
        // re-read every `prompts/<file>.md` from disk into the
        // canonical `AgentCore`'s `Arc<RwLock<Option<String>>>` fields
        // so the next LLM call uses the on-disk directives even if
        // Phase B loaded stale content (e.g. operator edited a file
        // between Phase A scan and Phase B injection). This is a
        // **single** reload — the Debug panel's PUT-then-RELOAD flow
        // is the live-editing path; this is the startup-seal path.
        if let Some(debug_svc) = ctx.debug_service_slot.lock().await.as_ref() {
            match debug_svc.reload_prompts().await {
                Ok(()) => tracing::info!(
                    "ADR-063 Phase C reload_prompts: 9 LLM prompt overrides refreshed from disk"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    "ADR-063 Phase C reload_prompts failed; runtime continues with the Phase A loaded content"
                ),
            }
        }
    }

    // ADR-040: gRPC hello_config MCP catalog sync removed.
    // MCP servers are loaded from the on-disk agent_mcp.json config.

    // ── MCP auto-connect at startup (background, non-blocking) ───────
    let mcp_startup_rx: Option<
        tokio::sync::mpsc::Receiver<crate::tools::mcp_manager::McpConnectResult>,
    > = {
        let mcp_configs = crate::agent_config::load_active_mcp_configs(work_dir_path);
        if !mcp_configs.is_empty() {
            let (tx, rx) =
                tokio::sync::mpsc::channel::<crate::tools::mcp_manager::McpConnectResult>(1);
            tracing::info!(
                mcp_count = mcp_configs.len(),
                "Auto-connecting to persisted MCP servers at startup (background)"
            );
            // Owned copy so the spawned task does not borrow `config`
            // (which is only valid for this function body).
            let reload_work_dir = work_dir_path.to_path_buf();
            // ADR-076: `--mqtt-password` already carries the node_token
            // (the Node passes the same value for MQTT CONNECT auth), so
            // reuse it as the machine credential that lets pm / doc MCP
            // pass the Gateway's `auth_middleware`. `None` standalone.
            let node_token = config.mqtt_password.clone();
            tokio::spawn(async move {
                // ADR-069: startup MCP auto-connect goes through the
                // reconcile+filter path — reconcile agent_mcp_tools.json
                // against the live tools/list, then expose only enabled
                // tools.
                let (registry, wrappers, specs, failures) =
                    crate::tools::mcp_manager::connect_mcp_with_reconcile_and_filter(
                        &reload_work_dir,
                        &mcp_configs,
                        node_token.as_deref(),
                    )
                    .await;
                let _ = tx.send((registry, wrappers, specs, failures)).await;
            });
            Some(rx)
        } else {
            None
        }
    };

    // ── ADR-058: start workspace FS watchers ────────────────────────
    // Reconcile the watcher set against the shared WorkspaceResolver:
    // every user-configured workspace (agent_workspaces.json entries)
    // gets a watcher task that publishes fs-changed events over MQTT.
    // Runs after the MQTT client slot is populated (Phase A) so events
    // are publishable immediately; task lifecycle is owned by the set.
    //
    // The watcher scan is **spawned onto its own background task** instead
    // of running inline. The blocking piece is `PollWatcher::watch(Recur‐
    // siveMode::Recursive)`, which walks the entire root tree on init — a
    // large project (`target/`, `node_modules/`, `core/target/`, ...) can
    // take 6–8 seconds. We don't want Phase C → Phase D → `ready=true`
    // publication to wait on that scan; `ready=true` is now published at
    // the end of Phase A (see `agent_init.rs::phase_a_init_agent`) once
    // the HTTP server is up, so the Desktop can start hitting
    // `/workspaces`, `/workspaces/tree`, etc. immediately while watchers
    // come online. Late-arriving watcher events are safe: any Desktop
    // reconnect (or `WorkspaceFsEvents::fullSync`) triggers a full tree
    // re-sync from the authoritative HTTP endpoint, so events missed
    // during the watcher-warm-up window are recovered.
    {
        let agent_id = ctx.agent_id.clone();
        let resolver = ctx
            .workspace_resolver
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let watcher_set = ctx.workspace_watcher_set.clone();
        tokio::spawn(async move {
            // Lock + scan on the background task. PollWatcher::watch blocks
            // the worker thread but not the runtime scheduler.
            let mut watchers = watcher_set.lock().await;
            watchers.sync_from_resolver(&resolver);
            tracing::info!(
                agent_id = %agent_id,
                watcher_count = watchers.watcher_count(),
                "Workspace FS watchers started (ADR-058, async)"
            );
        });
    }

    let (mcp_runtime_tx, mcp_runtime_rx) =
        tokio::sync::mpsc::channel::<crate::tools::mcp_manager::McpConnectResult>(1);

    Ok(SubsystemHandles {
        chunk_relay,
        mcp_startup_rx,
        mcp_runtime_tx,
        mcp_runtime_rx,
    })
}

/// MQTT-based chunk relay — publishes each ChunkEvent to the broker.
///
/// All events are encoded as `DataEnvelope { payload: SessionMessage }`
/// protobuf and published to `acowork/agents/{id}/sessions/{sid}/messages/{event_type}`.
/// Desktop subscribes to these topics directly on the broker.
///
/// **Ordering invariant**: each publish is awaited before the next
/// ChunkEvent is consumed. This makes the relay's call order the same as
/// the broker's enqueue order, eliminating the previous `tokio::spawn` race
/// between events of asymmetric payload size (notably
/// `session_state_changed(idle)` racing the last `record_complete`).
async fn relay_chunk_event_mqtt(
    publisher: &crate::mqtt::MqttChunkPublisher,
    _agent_id: &str,
    sid: &str,
    event: crate::agent::loop_::ChunkEvent,
) {
    use crate::agent::loop_::ChunkEvent;

    match event {
        ChunkEvent::ContextUsage(ctx_info) => {
            // Publish the full ContextUsageInfo so the StatusBar can render
            // total_tokens / context_window / usage_percent, not just the
            // last-turn input/output tokens.
            publisher.publish_context_usage(sid, &ctx_info).await;
        }

        ChunkEvent::CompactingStarted => {
            publisher.publish_compacting(sid, true).await;
        }

        ChunkEvent::CompactingEnded => {
            publisher.publish_compacting(sid, false).await;
        }

        ChunkEvent::CompactionCancelled { reason } => {
            publisher.publish_compaction_cancelled(sid, reason).await;
        }

        ChunkEvent::IterationLimitPaused {
            iteration,
            max_iterations,
            message,
        } => {
            publisher
                .publish_iteration_limit_paused(sid, iteration, max_iterations, message)
                .await;
        }

        ChunkEvent::ToolApprovalNeeded {
            request_id,
            tool_name,
            action,
            risk_level,
            reason,
            tool_call_id,
            approval_timeout_secs,
        } => {
            publisher
                .publish_tool_approval_needed(crate::mqtt::client::ToolApprovalNeededEvent {
                    session_id: sid,
                    request_id: &request_id,
                    tool_name: &tool_name,
                    action: &action,
                    risk_level: &risk_level,
                    reason: &reason,
                    tool_call_id: &tool_call_id,
                    approval_timeout_secs,
                })
                .await;
        }

        ChunkEvent::Done { message_id, .. } => {
            publisher.publish_done(sid, &message_id).await;
        }

        ChunkEvent::Error {
            user_message,
            detail: _,
            error_type: _,
            message_id,
        } => {
            publisher
                .publish_error(sid, &message_id, &user_message)
                .await;
        }

        ChunkEvent::Stopped { .. } => {
            // Use empty message_id for stopped — the event itself is the signal.
            publisher.publish_stopped(sid, "").await;
        }

        ChunkEvent::SessionStateChanged { state } => {
            publisher.publish_session_state(sid, &state).await;
        }

        ChunkEvent::TodoListUpdated { todos } => {
            let todos_json = serde_json::to_string(&todos).unwrap_or_default();
            publisher.publish_todo_updated(sid, &todos_json).await;
        }

        ChunkEvent::NewDataAvailable {
            session_id,
            interval_ms,
            title,
        } => {
            publisher
                .publish_new_data_available(&session_id, interval_ms as u32, title.as_deref())
                .await;
        }

        ChunkEvent::StreamDelta {
            session_id,
            lines,
            seq,
        } => {
            // ADR-035 M2: lines is Vec<(role, message_id, content)>.
            //
            // `seq` was assigned by `SessionCore::next_seq` at the emit
            // site and shipped through the chunk event unchanged. The
            // chunk_relay loop is single-threaded, so the seq here is
            // guaranteed to match the order in which this frame should
            // reach the Desktop — that is what makes the backend the
            // single source of truth for ordering, and lets the Desktop's
            // `insertBySeq` re-derive the right position even on rare
            // reorder.
            let lines_proto: Vec<StreamLine> = lines
                .into_iter()
                .map(|(role, message_id, content)| StreamLine {
                    role,
                    message_id,
                    line_no: 0,
                    content,
                })
                .collect();
            publisher
                .publish_stream_delta(&session_id, &lines_proto, seq)
                .await;
        }

        // ADR-035 C1: publish record_complete at QoS 1 (authoritative
        // terminal event). tool_result content is truncated to first 5
        // lines inside publish_record_complete (D9.2).
        //
        // `seq` travels with the ChunkEvent exactly as it was assigned at
        // the emit site (`SessionCore::next_seq`) — see the StreamDelta
        // branch above for the full ordering rationale.
        ChunkEvent::RecordComplete {
            session_id,
            role,
            message_id,
            content,
            tool_name,
            tool_call_id,
            is_error,
            seq,
        } => {
            publisher
                .publish_record_complete(
                    &session_id,
                    &role,
                    &message_id,
                    &content,
                    &tool_name,
                    &tool_call_id,
                    is_error,
                    seq,
                )
                .await;
        }

        ChunkEvent::AskQuestion {
            request_id,
            question,
            options,
            title,
            timeout_seconds,
        } => {
            let qjson = serde_json::json!({
                "request_id": request_id,
                "question": question,
                "options": options,
                "title": title,
                "timeout_seconds": timeout_seconds,
            });
            let question_json = qjson.to_string();
            publisher
                .publish_ask_question(sid, &request_id, &question_json)
                .await;
        }

        ChunkEvent::ClearRetainedEvent { event_type } => {
            publisher.clear_retained_event(sid, &event_type).await;
        }

        ChunkEvent::ToolProgress {
            session_id,
            tool_call_id,
            elapsed_ms,
            timeout_ms,
        } => {
            publisher
                .publish_tool_progress(&session_id, &tool_call_id, elapsed_ms, timeout_ms)
                .await;
        }

        // ADR-XXX: per-session persisted metadata changed. Published
        // with Retained=true so a (re)connecting Desktop immediately
        // receives the current state via the broker's retained store.
        ChunkEvent::SessionConfigChanged { config } => {
            tracing::info!(
                sid = %sid,
                title = %config.title,
                model_id = %config.model_id,
                provider_id = %config.provider_id,
                workspace_id = %config.workspace_id,
                "Publishing session_config (retained)"
            );
            publisher.publish_session_config(sid, &config).await;
        }

        ChunkEvent::LoopDetectedPaused { message, .. } => {
            publisher.publish_loop_detected_paused(sid, &message).await;
        }

        // ADR-106 revision (2026-11-03): the round doorbell. QoS 1 and NOT
        // retained — a re-subscribing Desktop must not be told to re-read a
        // round that ended hours ago; the interval backstop plus the retained
        // `sessions/{sid}/state` snapshot cover a missed frame.
        ChunkEvent::RoundState {
            round_id,
            chat_session_id,
        } => {
            publisher
                .publish_round_state(sid, &round_id, &chat_session_id)
                .await;
        }
    }
}

/// Minimum interval between MQTT publishes for state changes (tokens,
/// Spawn the per-session config-change relay (ADR-043).
///
/// Config changes (title, model, provider, workspace_id, reasoning_effort,
/// temperature) are low-frequency user actions. The relay publishes
/// immediately with no throttle.
pub(crate) fn spawn_config_change_relay(
    mut config_rx: tokio::sync::mpsc::UnboundedReceiver<ConfigChange>,
    chunk_tx: tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>,
    conv: ConversationSession,
    session_id: String,
    // Late-bind slot for AgentCore. Used to run
    // resolve_effective_reasoning_effort before publishing so MQTT
    // subscribers see the same effective value as HTTP GET.
    // See agent::session_config::llm_effects for the rationale.
    core_slot: SharedAgentCore,
) {
    tokio::spawn(async move {
        use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};

        while let Some(change) = config_rx.recv().await {
            // Drop sentinel: empty session_id means the ConversationSession
            // is being dropped. Fall back to the relay's own clone.
            // llm_availability is left as Unspecified here — the dedicated
            // availability watcher in the chunk_relay task publishes the
            // authoritative value independently, so per-config-change
            // re-publishes don't risk carrying a stale tag.
            let mut config =
                if change.snapshot.session_id.is_empty() && change.snapshot.agent_id.is_empty() {
                    conv.build_session_config_snapshot(
                        acowork_core::mqtt_proto::LlmAvailability::Unspecified,
                    )
                } else {
                    change.snapshot
                };

            // Resolve effective reasoning_effort before publish. The
            // raw value may be empty if the session was resumed from
            // a meta.json that never persisted reasoning_effort.
            // Funnel through the shared resolver so MQTT subscribers
            // see the same effective value as HTTP GET.
            if config.reasoning_effort.is_empty()
                && let Ok(slot) = core_slot.read()
                && let Some(core) = slot.as_ref()
            {
                let caps = core.get_model_capabilities(&config.model_id);
                if let Some(effort) =
                    crate::agent::session_config::llm_effects::resolve_effective_reasoning_effort(
                        caps.as_ref(),
                        None,
                    )
                {
                    config.reasoning_effort = effort.to_string();
                }
            }

            tracing::info!(
                session_id = %session_id,
                model_id = %config.model_id,
                provider_id = %config.provider_id,
                workspace_id = %config.workspace_id,
                "config relay: sending SessionConfigChanged to chunk channel"
            );

            let event = SessionChunkEvent {
                session_id: session_id.clone(),
                event: ChunkEvent::SessionConfigChanged { config },
            };
            if chunk_tx.try_send(event).is_err() {
                tracing::debug!(
                    session_id = %session_id,
                    "config relay: chunk channel full/closed, dropping notification"
                );
            }
        }
        tracing::debug!(
            session_id = %session_id,
            "config relay: config_rx closed, exiting"
        );
    });
}

/// Spawn the per-session state-change relay (ADR-043).
///
/// Coalescing behaviour (LOG-001 / PERF-001): a single LLM turn emits ~9
/// `StateChange` snapshots (status flip + one `message_count` increment per
/// appended message + context_usage + token updates). Publishing every one
/// floods the chunk channel and forces the Desktop to re-render 9x per turn.
/// We hold the newest snapshot and publish **at most once per coalescing
/// window** (500 ms). The retained SessionState topic
/// (`sessions/{sid}/state`) overwrites the previous value on the broker, so
/// even a disconnected Desktop receives the latest snapshot on reconnect.
pub(crate) fn spawn_state_change_relay(
    mut state_rx: tokio::sync::mpsc::UnboundedReceiver<StateChange>,
    chunk_tx: tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>,
    conv: ConversationSession,
    session_id: String,
) {
    tokio::spawn(async move {
        use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};
        use std::time::{Duration, Instant};

        const COALESCE_WINDOW: Duration = Duration::from_millis(500);

        // Hold the ConversationSession alive for as long as the relay runs.
        // The caller hands us a clone; if we never bind it, the clone is
        // dropped as soon as this function returns, which triggers
        // `ConversationSession::drop` → sends a Drop sentinel on
        // `state_change_tx` → this relay interprets it as "session destroyed"
        // and exits immediately (ADR-043 sentinel semantics). Binding it here
        // keeps the channel sender alive so the sentinel only arrives when the
        // SessionManager truly destroys the session.
        let _alive = conv;

        /// Publish a single snapshot to the chunk channel (best-effort).
        fn publish(
            chunk_tx: &tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>,
            session_id: &str,
            state: acowork_core::mqtt_proto::SessionState,
        ) {
            let event = SessionChunkEvent {
                session_id: session_id.to_string(),
                event: ChunkEvent::SessionStateChanged { state },
            };
            if chunk_tx.try_send(event).is_err() {
                tracing::debug!(
                    session_id = %session_id,
                    "state relay: chunk channel full/closed, dropping notification"
                );
            }
        }

        /// Drop sentinel: empty session_id means the ConversationSession is
        /// being destroyed — flush any pending snapshot, then exit.
        fn is_drop_sentinel(change: &StateChange) -> bool {
            change.snapshot.session_id.is_empty() && change.snapshot.agent_id.is_empty()
        }

        let mut pending: Option<acowork_core::mqtt_proto::SessionState> = None;
        loop {
            // 1) Wait for the first change of a burst.
            match state_rx.recv().await {
                Some(change) => {
                    if is_drop_sentinel(&change) {
                        if let Some(s) = pending.take() {
                            publish(&chunk_tx, &session_id, s);
                        }
                        break;
                    }
                    pending = Some(change.snapshot);
                }
                None => {
                    if let Some(s) = pending.take() {
                        publish(&chunk_tx, &session_id, s);
                    }
                    break;
                }
            }

            // 2) Coalesce the rest of the window — keep only the newest.
            let deadline = Instant::now() + COALESCE_WINDOW;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, state_rx.recv()).await {
                    Ok(Some(change)) => {
                        if is_drop_sentinel(&change) {
                            if let Some(s) = pending.take() {
                                publish(&chunk_tx, &session_id, s);
                            }
                            break;
                        }
                        pending = Some(change.snapshot);
                    }
                    Ok(None) => {
                        if let Some(s) = pending.take() {
                            publish(&chunk_tx, &session_id, s);
                        }
                        break;
                    }
                    Err(_) => break, // window elapsed
                }
            }

            // 3) Publish the freshest snapshot of this burst.
            if let Some(s) = pending.take() {
                tracing::debug!(
                    session_id = %session_id,
                    message_count = s.message_count,
                    "state relay: publishing coalesced SessionStateChanged"
                );
                publish(&chunk_tx, &session_id, s);
            }
        }
        tracing::debug!(
            session_id = %session_id,
            "state relay: state_rx closed, exiting"
        );
    });
}
