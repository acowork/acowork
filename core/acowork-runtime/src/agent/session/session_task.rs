//! SessionTask: independent execution actor for a single session.
//!
//! Each `SessionTask` runs in its own tokio task, processing messages
//! from an inbound channel. It owns an `AgentLoop` instance for the
//! session's lifetime, ensuring per-session isolation of history,
//! budget, and loop detection while sharing provider/tools via Arc.

use std::sync::Arc;

use acowork_core::providers::traits::ChatMessage;
use acowork_core::tools::traits::Tool;
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::agent::agent_core::AgentCore;
use crate::agent::context::ContextBuilder;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_::{AgentLoop, ChunkEvent, SessionChunkEvent};
use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent::session_core::SessionCore;
use crate::agent::session_state::SessionState;
use crate::cancellation::CancelHandle;
use crate::debug::DebugHandles;
use crate::debug::DebugObserverImpl;
use crate::usecases::attachment::on_disk_name;

/// A Chat attachment the Runtime pulled over node-auth HTTP from the
/// Gateway (design 27 §4.2.1). Bytes never ride the MQTT envelope — the
/// Intent carries ids only; this is what a successful pull yields.
#[derive(Clone, Debug)]
pub struct PulledAttachment {
    pub filename: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Messages that can be sent to a SessionTask.
#[derive(Clone)]
pub enum SessionMessage {
    /// User chat message to process
    ChatMessage {
        content: String,
        message_id: String,
        /// Per-turn skill instructions the user explicitly selected via a chat
        /// `command` (resolved against the agent's `SkillRegistry`). Turn-scoped:
        /// `None` means "no skill selected this turn" and clears only this slot —
        /// it does NOT touch the agent-level Progressive summary, which lives on
        /// `SessionManagerConfig.skill_summary` and is seeded separately.
        active_skill: Option<String>,
        /// ADR-046: unified array of attached items replacing the prior
        /// `documents` + `attached_context` fields. Each item is a
        /// discriminated union (file_upload / image_upload / attached_file
        /// / attached_selection / attached_folder) carrying the full
        /// metadata needed for JSONL persistence and prompt-side hints.
        /// `None` / empty means no attachments.
        attached_items: Option<Vec<acowork_core::protocol::AttachedItem>>,
        /// Optional multimodal content parts (e.g. text + image_url).
        /// When present, the agent loop constructs a ChatMessage::user_multimodal()
        /// instead of ChatMessage::user(), enabling image inputs to flow to the LLM.
        content_parts: Option<Vec<acowork_core::providers::traits::ContentPart>>,
    },
    /// Continue execution after tool result or iteration pause
    ContinueExecution,
    /// Apply runtime config overrides from Gateway
    UpdateRuntimeConfig(RuntimeConfigOverrides),
    /// Update workspace context text
    UpdateWorkspaceContext { context_text: String },
    /// Update MCP tools on AgentCore (hot-push when MCP servers connect/disconnect).
    /// Refreshes `AgentCore.all_tools` so LLM injection and debug snapshot capture
    /// pick up the latest MCP tool list.
    UpdateMcpTools {
        mcp_tools: Option<Vec<Arc<dyn Tool>>>,
    },
    /// ADR-029: Update builtin tools enabled flags on AgentCore (hot-push
    /// when Gateway pushes RuntimeConfigUpdate.builtin_tools_enabled).
    /// Refreshes `AgentCore.all_tools` so LLM injection picks up
    /// toggled tools.
    UpdateBuiltinTools {
        entries: Vec<crate::agent_config::AgentToolEntry>,
    },
    /// ADR-063 §3.7.6: replace the main-dialog system prompt in this
    /// session's `ContextBuilder`. Produced by
    /// `SessionManager::apply_system_prompt` (triggered by the L2 reload
    /// endpoint after it rebuilds the prompt from `prompts/*.md`,
    /// including the required `system.md`). The next `build_chat_request`
    /// uses the new value — no agent restart needed.
    UpdateSystemPrompt { system_prompt: String },
    /// ADR-094: replace the Progressive-mode skills summary in this
    /// session's `ContextBuilder`. `None` clears the section
    /// (Manual mode hot-toggle). Mirrors `UpdateSystemPrompt` /
    /// `UpdateIdentityContext` shape and lifecycle — agent-scoped
    /// template + per-session broadcast via `SessionManager`.
    UpdateSkillInstructions { instructions: Option<String> },
    /// ADR-030 C3: Sidecar came online (LSP relay became ready, embed model
    /// switched, ...) and we want to surface a new builtin tool to this
    /// session. Carries a pre-decorated `BuiltinToolEntry` so each session
    /// gets its own security-wrapped clone (path guard + rate limiter).
    ///
    /// Replaces any existing entry with the same `name()` so hot-reload
    /// of the LSP relay endpoint is idempotent.
    AddDynamicBuiltinTool {
        entry: crate::agent::agent_core::BuiltinToolEntry,
    },
    /// ADR-030 C3: Sidecar went away (LSP relay died, embed sidecar
    /// restart, ...). Removing the named builtin tool from this session's
    /// dispatch list. `name` should match `BuiltinToolEntry::name()`.
    RemoveDynamicBuiltinTool { name: String },
    /// Update the workspace directory path for tool execution.
    /// Carries the fully-resolved absolute path from SessionManager.
    SetWorkDir { path: String },
    /// Update the workspace prompt file content (CLAUDE.md / AGENTS.md).
    /// Content is None when no prompt file is configured.
    SetWorkspacePromptFile { content: Option<String> },
    /// Update identity context from Gateway UserProfileUpdate push
    UpdateIdentityContext { identity_context: Option<String> },
    /// Global provider list was updated (Gateway pushed new model capabilities).
    /// Sessions should emit an updated status to the frontend so the UI
    /// reflects the latest available models and providers.
    ProviderListUpdated,
    /// Stop signal to stop the current agent loop iteration
    Stop { reason: String },
    /// Enable debug mode at runtime (after Gateway pushes EnableDebugMode).
    /// Carries the DebugController, event sender, and notify handles so the
    /// SessionTask can inject them into its AgentCore and start emitting
    /// debug events without a process restart.
    EnableDebugMode(DebugHandles),
    /// Disable debug mode at runtime (after Gateway pushes DisableDebugMode).
    /// The SessionTask calls `AgentCore::clear_debug_mode()` so subsequent
    /// agent_loop iterations stop emitting debug events. No payload, because
    /// the per-session observer is replaced with the production no-op slot;
    /// the SessionManager already cleared its `debug_controllers` and
    /// `debug_event_senders` maps on the way out.
    DisableDebugMode,
    /// Close the session gracefully: trigger distillation and free resources.
    /// JSONL history is preserved (use Delete to also remove the file).
    Close,
    /// ADR-032 C4c: User-initiated compression action (from frontend buttons).
    /// Carries the specific action type to execute.
    CompressAction(crate::agent::loop_::CompressionAction),
    /// Update the embedding provider at runtime (hot-push from Gateway
    /// via SidecarEndpointUpdate(Embed, non-empty endpoint)).
    /// The session rebuilds its ONNX embedding provider with the new
    /// endpoint/model/dimension.
    ///
    /// ADR-s1: when `embed_provider_id` is `Some`, the endpoint/model
    /// above refer to a cloud OpenAI-compatible embedding service
    /// (volcengine / dashscope / siliconflow / …) instead of the local
    /// ONNX sidecar. The session constructs a
    /// `RemoteEmbeddingProvider` and authenticates with `embed_api_key`.
    /// When `embed_provider_id` is `None`, behaviour is unchanged —
    /// the endpoint is the local ONNX service.
    UpdateEmbedConfig {
        embed_endpoint: String,
        embed_model_id: String,
        embed_dimension: usize,
        /// Active cloud provider ID (e.g. "volcengine"). `None` keeps
        /// the local ONNX path.
        embed_provider_id: Option<String>,
        /// Decrypted API key for `embed_provider_id`. Empty / `None`
        /// for the local ONNX path.
        embed_api_key: Option<String>,
    },
    /// Disable the embedding provider (hot-push from Gateway via
    /// SidecarEndpointUpdate(Embed, empty endpoint)). The session clears
    /// its ONNX embedding provider so embedding-dependent operations
    /// degrade gracefully until the sidecar comes back (ADR-030 ISSUE-2).
    DisableEmbedConfig,
    /// Inject a system notification into the conversation history.
    /// Used to surface MCP connection failures and other async events
    /// to the LLM context so the Agent can self-heal.
    SystemNotification { content: String },
    /// Design 27 §6/§7: an Intent envelope delivered to this execution
    /// session as a *turn trigger* (the session was idle, or the turn must
    /// run with responder-role semantics). `kind == "request"` puts the
    /// turn in responder role: tool trimming (R1), no human-awaiting
    /// pauses (R3), and a system `response` write-back at turn end (§7.1).
    /// `kind == "response"` renders the initiator-side answer turn.
    Intent {
        /// Rendered user-visible message text (shared renderer,
        /// `intent_turn::render_intent_message`).
        content: String,
        /// Stable envelope id — also the write-back path coordinate for
        /// requests (`POST /api/intents/{request_id}/response`).
        message_id: String,
        conversation_id: String,
        source_chat_session_id: String,
        /// Sender instance (write-back `target`).
        from: String,
        action: String,
        repo: String,
        kind: String,
        has_human: Option<bool>,
        /// Chat attachment ids from a human Chat message. Runtime pulls from Gateway HTTP.
        chat_attachment_ids: Vec<String>,
        /// Design 27 §2.1 / §4.1: when this intent opened a *rotated*
        /// successor session, the handoff note describing the session it
        /// replaced. Persisted as a standalone `system` JSONL row (transcript
        /// copy) and folded into the model-facing `user_message`; `content`
        /// itself stays the user's verbatim text so the auto-title and the
        /// persisted `user` entry stay clean (ADR-046).
        handoff_note: Option<String>,
    },
    // ADR-035 Phase 3: EnableNotify/DisableNotify removed — push drives all
    // streaming, the front/back suppression mechanism is gone.
}

impl std::fmt::Debug for SessionMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionMessage::ChatMessage {
                content,
                message_id,
                active_skill,
                attached_items,
                content_parts,
            } => f
                .debug_struct("ChatMessage")
                .field("content", &content.chars().take(64).collect::<String>())
                .field("message_id", message_id)
                .field("has_skill", &active_skill.is_some())
                .field(
                    "attached_count",
                    &attached_items.as_ref().map(|c| c.len()).unwrap_or(0),
                )
                .field("has_content_parts", &content_parts.is_some())
                .finish(),
            SessionMessage::ContinueExecution => f.debug_tuple("ContinueExecution").finish(),
            SessionMessage::UpdateRuntimeConfig(overrides) => f
                .debug_struct("UpdateRuntimeConfig")
                .field("max_output_tokens", &overrides.max_output_tokens)
                .field("max_iterations", &overrides.max_iterations)
                .field("temperature", &overrides.temperature)
                .field("context_window", &overrides.context_window)
                .field(
                    "has_system_prompt",
                    &overrides.system_prompt_override.is_some(),
                )
                .field(
                    "shell_approval_threshold",
                    &overrides.shell_approval_threshold,
                )
                .field("approval_timeout_secs", &overrides.approval_timeout_secs)
                .finish(),
            SessionMessage::UpdateWorkspaceContext { context_text } => f
                .debug_struct("UpdateWorkspaceContext")
                .field("len", &context_text.len())
                .finish(),
            SessionMessage::UpdateMcpTools { mcp_tools } => f
                .debug_struct("UpdateMcpTools")
                .field(
                    "mcp_tool_count",
                    &mcp_tools.as_ref().map(|t| t.len()).unwrap_or(0),
                )
                .finish(),
            SessionMessage::UpdateBuiltinTools { entries } => f
                .debug_struct("UpdateBuiltinTools")
                .field("entry_count", &entries.len())
                .field(
                    "enabled_count",
                    &entries.iter().filter(|e| e.enabled).count(),
                )
                .finish(),
            SessionMessage::UpdateSystemPrompt { system_prompt } => f
                .debug_struct("UpdateSystemPrompt")
                .field("prompt_len", &system_prompt.len())
                .finish(),
            SessionMessage::UpdateSkillInstructions { instructions } => f
                .debug_struct("UpdateSkillInstructions")
                .field("has_skills", &instructions.is_some())
                .finish(),
            SessionMessage::AddDynamicBuiltinTool { entry } => f
                .debug_struct("AddDynamicBuiltinTool")
                .field("name", &entry.name())
                .field("enabled", &entry.enabled)
                .finish(),
            SessionMessage::RemoveDynamicBuiltinTool { name } => f
                .debug_struct("RemoveDynamicBuiltinTool")
                .field("name", name)
                .finish(),
            SessionMessage::SetWorkDir { path } => {
                f.debug_struct("SetWorkDir").field("path", path).finish()
            }
            SessionMessage::SetWorkspacePromptFile { content } => f
                .debug_struct("SetWorkspacePromptFile")
                .field("has_content", &content.is_some())
                .field("content_len", &content.as_ref().map(|c| c.len()))
                .finish(),
            SessionMessage::UpdateIdentityContext { identity_context } => f
                .debug_struct("UpdateIdentityContext")
                .field("has_identity", &identity_context.is_some())
                .finish(),
            SessionMessage::ProviderListUpdated => f.debug_struct("ProviderListUpdated").finish(),
            SessionMessage::Stop { reason } => {
                f.debug_struct("Stop").field("reason", reason).finish()
            }
            SessionMessage::EnableDebugMode(_) => f.debug_tuple("EnableDebugMode").finish(),
            SessionMessage::DisableDebugMode => f.debug_tuple("DisableDebugMode").finish(),
            SessionMessage::Close => f.debug_tuple("Close").finish(),
            SessionMessage::CompressAction(action) => f
                .debug_tuple("CompressAction")
                .field(&format!("{:?}", action))
                .finish(),
            SessionMessage::UpdateEmbedConfig {
                embed_endpoint,
                embed_model_id,
                embed_dimension,
                embed_provider_id,
                embed_api_key,
            } => f
                .debug_struct("UpdateEmbedConfig")
                .field("embed_endpoint", embed_endpoint)
                .field("embed_model_id", embed_model_id)
                .field("embed_dimension", embed_dimension)
                .field("embed_provider_id", embed_provider_id)
                .field(
                    "embed_api_key_redacted",
                    &embed_api_key.as_ref().map(|_| "***"),
                )
                .finish(),
            SessionMessage::DisableEmbedConfig => f.debug_tuple("DisableEmbedConfig").finish(),
            SessionMessage::SystemNotification { content } => f
                .debug_struct("SystemNotification")
                .field("len", &content.len())
                .finish(),
            SessionMessage::Intent {
                message_id,
                kind,
                from,
                action,
                ..
            } => f
                .debug_struct("Intent")
                .field("message_id", message_id)
                .field("kind", kind)
                .field("from", from)
                .field("action", action)
                .finish(),
        }
    }
}

/// Independent execution actor for a single session.
///
/// Each `SessionTask` runs as a separate tokio task, processing
/// `SessionMessage`s from its inbound channel. It owns an `AgentLoop`
/// built from a cloned `AgentCore` plus its own `SessionState`,
/// ensuring full per-session isolation.
pub(crate) struct SessionTask {
    /// The session's AgentLoop, pre-constructed so that external callers
    /// can obtain its `InboundMessage` sender at session-creation time.
    agent_loop: AgentLoop,
    /// Clone of the AgentLoop's inbound sender, kept here purely as a
    /// fallback so that legacy `SessionMessage::ContinueExecution` /
    /// `SessionMessage::Stop` messages (if anyone still sends them)
    /// can be forwarded. The primary, deadlock-safe path is via
    /// `SessionHandle::send_inbound`.
    agent_inbound_tx: mpsc::Sender<InboundMessage>,
    /// Inbound message receiver (SessionMessage-level, not InboundMessage)
    inbound_rx: mpsc::Receiver<SessionMessage>,
    /// System prompt for context building
    system_prompt: String,
    /// ADR-021: Single chunk sender for control events.
    chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
    /// Unique session identifier (used for logging and chunk tagging)
    session_id: String,
    /// Identity context string injected by Gateway
    identity_context: Option<String>,
    /// LLM protocol type (for image token estimation)
    protocol_type: acowork_core::protocol::ProtocolType,
    /// Design 27 §2.1: shared intent routing registry. An intent `request`
    /// turn stamps its rotation bookkeeping (T1 idle clock + T2 compaction
    /// marker) here at turn end, after the response write-back path runs.
    intent_map: Arc<std::sync::Mutex<super::intent_routing::IntentSessionMap>>,
    /// ADR-094: agent-level Progressive-mode skills summary, seeded from
    /// `SessionManagerConfig.skill_summary` at session creation. Applied to
    /// the new session's `ContextBuilder` in `run()` via
    /// `set_skill_summary` (parallel to how `system_prompt` and
    /// `identity_context` are applied). Hot reloads arrive via
    /// `SessionMessage::UpdateSkillInstructions`.
    ///
    /// Named `skill_summary` (not `skill_instructions`) because it is a
    /// different lifecycle from the per-turn `ChatMessage.active_skill`:
    /// this one lives as long as the session.
    skill_summary: Option<String>,
}

// ADR-046: pre-extraction helpers (`extract_document_text`,
// `build_attached_context_blocks`) were removed. Attachment records are
// now persisted as standalone system JSONL entries via
// `AgentLoop::write_attached_items` and rendered by the frontend from
// the JSONL metadata. The Runtime no longer reads uploaded file bytes
// or assembles complex prompt prefixes — see ADR-046 §2.4.

/// ADR-047: apply deferred LLM-side effects when the session config
/// version has advanced since the last poll.
///
/// Called (a) at the top of the SessionTask main loop — the original
/// turn-boundary site — and (b) immediately before processing any
/// inbound message that can drive an agent-loop run (ChatMessage,
/// ContinueExecution, debug replay). Without (b), a config change
/// (model_switch / reasoning_effort / workspace_switch) landing while
/// the task is blocked in `select!` was only applied AFTER the next
/// run finished — every LLM call of that run still used the stale
/// provider ("switched model but usage still hits the old provider").
fn apply_pending_config_effects(
    agent_loop: &mut AgentLoop,
    context_builder: &mut ContextBuilder,
    session_id: &str,
    last_config_version: &mut u64,
    last_config_snapshot: &mut crate::agent::session_config::SessionConfigSnapshot,
) {
    if let Some(conv) = agent_loop.session.conversation.as_ref() {
        let current_version = conv.config_version();
        if current_version != *last_config_version {
            let snapshot = conv.config_snapshot();
            tracing::info!(
                session_id = %session_id,
                old_version = *last_config_version,
                new_version = current_version,
                "SessionTask: config version changed, applying LLM-side effects"
            );
            crate::agent::session_config::llm_effects::apply_llm_effects(
                agent_loop,
                context_builder,
                &snapshot,
                last_config_snapshot,
            );
            *last_config_snapshot = snapshot;
            *last_config_version = current_version;
        }
    }
}

/// ADR-083: surface a manual-compaction error to the frontend as an error
/// chunk — UNLESS it is a cancellation.
///
/// A user cancel or deadline expiry leaves history untouched and is already
/// reported through [`ChunkEvent::CompactionCancelled`]; emitting the generic
/// `Error` bubble on top of it would double-report and wrongly read as a
/// failure. `label` only affects the log line.
async fn report_manual_compaction_error(
    chunk_tx: Option<&mpsc::Sender<SessionChunkEvent>>,
    session_id: &str,
    label: &str,
    e: &crate::error::RuntimeError,
) {
    if matches!(e, crate::error::RuntimeError::CompactionCancelled(_)) {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "Manual compaction cancelled (no error chunk: CompactionCancelled already emitted)"
        );
        return;
    }
    tracing::error!(
        session_id = %session_id,
        error = %e,
        label = label,
        "Manual compaction failed"
    );
    if let Some(tx) = chunk_tx {
        let (user_message, detail, error_type) = e.error_info();
        let event = SessionChunkEvent {
            session_id: session_id.to_string(),
            event: ChunkEvent::Error {
                user_message,
                detail,
                error_type,
                message_id: String::new(),
            },
        };
        if tx.send(event).await.is_err() {
            tracing::warn!(
                session_id = %session_id,
                label = label,
                "Failed to send Error chunk event (manual compaction)"
            );
        }
    }
}

impl SessionTask {
    /// Create a new SessionTask with the given shared core, session state,
    /// message receiver, system prompt, and optional chunk channel.
    ///
    /// Returns the task together with the `AgentLoop`'s `InboundMessage`
    /// sender. Callers (SessionManager) must stash that sender in
    /// `SessionHandle` so that out-of-band signals (Continue/Interrupt)
    /// can be delivered directly to the AgentLoop without going through
    /// the SessionTask's main loop — which would otherwise deadlock
    /// whenever the AgentLoop is awaiting a pause-resume signal.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        core: Arc<AgentCore>,
        session: SessionState,
        inbound_rx: mpsc::Receiver<SessionMessage>,
        system_prompt: String,
        // ADR-094: agent-level Progressive-mode skills summary - see struct
        // field docs. Mirrors `system_prompt` / `identity_context` placement
        // in the parameter list.
        skill_summary: Option<String>,
        chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
        session_id: String,
        identity_context: Option<String>,
        protocol_type: acowork_core::protocol::ProtocolType,
        mcp_tools: Option<Vec<Arc<dyn Tool>>>,
        // ADR-030 C3: Dynamic builtin tools registered via SidecarEndpointUpdate
        // before this session was created. Injected into the cloned AgentCore's
        // `builtin_tools` so the session starts with them (ISSUE-1 fix).
        dynamic_builtin_tools: Vec<crate::agent::agent_core::BuiltinToolEntry>,
        runtime_debug: Option<DebugHandles>,
        pending_debug_handles: Arc<tokio::sync::Mutex<Option<DebugHandles>>>,
        // Accumulated runtime config overrides from Gateway pushes.
        // Applied directly to AgentCore during session init so the session
        // starts with correct values (not patched via message replay).
        runtime_overrides: RuntimeConfigOverrides,
        // Resolved workspace directory for tool execution. Shared with SessionManager.
        current_work_dir: Arc<std::sync::RwLock<Option<String>>>,
        // Per-session committed_lines counter, shared with the writer thread.
        committed_lines: Arc<std::sync::atomic::AtomicUsize>,
        // Shared streaming lines map, cloned into SessionCore.
        streaming_lines: crate::conversation::StreamingStateMap,
        // Design 27 §2.1: shared intent routing registry (rotation bookkeeping).
        intent_map: Arc<std::sync::Mutex<super::intent_routing::IntentSessionMap>>,
    ) -> (Self, mpsc::Sender<InboundMessage>) {
        // Create per-session SessionCore from the shared AgentCore template.
        let notify_interval_ms = core.config.data_flow.notify_interval_ms;
        let session_core = SessionCore::new(
            session_id.clone(),
            chunk_tx.clone(),
            committed_lines,
            notify_interval_ms,
            current_work_dir,
            streaming_lines,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );

        // Build per-session AgentCore clone from the shared template.
        let mut core_mut = (*core).clone();
        // Inject dynamic builtin tools (from SidecarEndpointUpdate pushes that
        // arrived before this session was created). Same-name tools in the
        // template are replaced; new tools are appended.
        for entry in dynamic_builtin_tools {
            let name = entry.name();
            if let Some(existing) = core_mut.builtin_tools.iter().position(|e| e.name() == name) {
                core_mut.builtin_tools[existing] = entry;
            } else {
                core_mut.builtin_tools.push(entry);
            }
        }
        // Set MCP tools and rebuild
        core_mut.mcp_tools = mcp_tools;
        core_mut.rebuild_all_tools();

        // Inject pending debug handles
        core_mut.set_debug_pending_injection(pending_debug_handles);

        // Inject runtime debug handles
        if let Some(handles) = runtime_debug {
            let observer = DebugObserverImpl::new(handles);
            core_mut.set_debug_mode(observer);
        }

        // Rebuild LLM Provider for this session
        if let Some(ref provider_id) = session.provider
            && let Some(new_provider) = session_core.build_provider_for(
                provider_id,
                session.account_id.as_deref(),
                &core_mut.config,
                &core_mut.global_provider_list,
                &core_mut.provider_key_vault,
                core_mut.compat_cache.as_ref(),
            )
        {
            let model = session.model.clone().unwrap_or_default();
            core_mut.update_provider(new_provider, model);
        } else {
            let raw = core_mut.provider.clone();
            let retry_config =
                crate::providers::reliable::RetryConfig::from(&core_mut.config.timeouts.retry);
            let mut reliable = crate::providers::reliable::ReliableProvider::new(raw, retry_config);
            if let Some(status) = &session_core.retry_session_status
                && let Some(handle) = &session_core.retry_wait_handle
                && let Some(tx) = &session_core.chunk_tx
                && let Some(sid) = &session_core.session_id
            {
                reliable = reliable.with_retry_ux(
                    crate::providers::reliable::RetryWaitHandle {
                        state: handle.state.clone(),
                        skip_notify: handle.skip_notify.clone(),
                    },
                    status.clone(),
                    tx.clone(),
                    sid.clone(),
                );
            }
            let model = session.model.clone().unwrap_or_default();
            core_mut.update_provider(Arc::new(reliable), model);
        }

        // Apply accumulated runtime config overrides
        core_mut.apply_runtime_config(&runtime_overrides);

        // Sync temperature override
        let mut session = session;
        if core_mut.temperature_override.is_some() {
            session.set_temperature(core_mut.temperature_override);
        }

        let (agent_loop, agent_inbound_tx) =
            AgentLoop::from_core_and_session(core_mut, session_core, session);

        let task = Self {
            agent_loop,
            agent_inbound_tx: agent_inbound_tx.clone(),
            inbound_rx,
            system_prompt,
            skill_summary,
            chunk_tx,
            session_id,
            identity_context,
            protocol_type,
            intent_map,
        };
        (task, agent_inbound_tx)
    }

    /// Set the status watch sender (ADR-014).
    /// Called by SessionManager after creating the SessionTask, before spawning.
    pub(crate) fn set_status_tx(
        &mut self,
        tx: tokio::sync::watch::Sender<crate::agent::session_state::SessionStatus>,
    ) {
        self.agent_loop.session_core.status_tx = Some(tx);
    }

    /// Return the per-session urgent_stop Notify so SessionManager can
    /// route fire_urgent_stop() to only the target session.
    /// Returns None in standalone mode (where urgent_stop is not initialized).
    pub(crate) fn urgent_stop_notify(&self) -> Option<Arc<Notify>> {
        self.agent_loop.session_core.urgent_stop.clone()
    }

    /// Return the `Arc` slot handle to the session's per-request
    /// [`CancelHandle`] so [`crate::agent::session::session_manager::SessionManager`]
    /// can route external cancellation signals (MQTT `StopGeneration`,
    /// debug `Stop`, CLI cancel) to the **current** generation of the
    /// handle on every dispatch.
    ///
    /// Unlike [`urgent_stop_notify`](Self::urgent_stop_notify) this always
    /// returns a clonable handle — the slot is unconditionally allocated
    /// in [`crate::agent::session_core::SessionCore::new`].
    ///
    /// ADR-044 §4.5: the field exposed here is an `Arc<parking_lot::Mutex<CancelHandle>>`,
    /// not a plain `CancelHandle`, because external sources must always
    /// observe the *current* request's generation. Storing a plain clone
    /// would freeze the generation to whatever was in the slot at
    /// registration time (the bug this design eliminates — see
    /// `SessionCore::begin_new_request`).
    pub(crate) fn cancel_handle_arc(&self) -> Arc<parking_lot::Mutex<CancelHandle>> {
        self.agent_loop.session_core.cancel_handle_arc()
    }

    /// ADR-083: return the `Arc` slot to the session's per-compaction
    /// [`CancelHandle`] so [`crate::agent::session::session_manager::SessionManager`]
    /// can route the `compress_type = 3` cancel to the *current* compaction
    /// on every dispatch. See [`SessionCore::compaction_cancel_handle_arc`].
    pub(crate) fn compaction_cancel_handle_arc(&self) -> Arc<parking_lot::Mutex<CancelHandle>> {
        self.agent_loop.session_core.compaction_cancel_handle_arc()
    }

    /// Run the session task, processing messages until Stop or channel close.
    pub async fn run(self) {
        let Self {
            mut agent_loop,
            agent_inbound_tx,
            session_id,
            chunk_tx,
            mut inbound_rx,
            system_prompt,
            identity_context,
            protocol_type,
            intent_map,
            skill_summary,
        } = self;

        // Build ContextBuilder with identity. Tool definitions are NOT
        // seeded from a pre-baked snapshot — we derive them live from
        // `core.builtin_tools` so hot-reloads (e.g. MCP install/uninstall
        // toggles) propagate to NEW sessions the same way they propagate
        // to live sessions. Pre-baked snapshots were the root cause of
        // the bug where the LLM still saw stale tool definitions after a
        // runtime config change (see commit history and
        // `rebuild_context_tool_definitions`).
        let mut context_builder =
            ContextBuilder::new(system_prompt.clone()).with_identity(identity_context.clone());

        // ADR-094: seed the agent-level Progressive-mode skills summary.
        // `None` means Manual mode, so the section simply stays unset.
        // This slot is session-lifetime: the per-turn skill-command path
        // writes `active_skill`, never this field, so a turn without a
        // selected skill cannot wipe the summary (see
        // `ContextBuilder::set_active_skill`).
        if let Some(ref skills) = skill_summary {
            context_builder.set_skill_summary(skills.clone());
        }

        // Mirror the identity onto SessionState so compaction paths
        // (loop_context / loop_session) can inject the user's preferred
        // language into the compact model's system prompt.
        agent_loop
            .session
            .set_identity_context(identity_context.clone());

        // ADR-012: Apply per-session model from SessionState.
        // For new sessions, model is set from provider_config during creation.
        // For resumed sessions, model is restored from JSONL metadata.
        if let Some(ref model) = agent_loop.session.model {
            context_builder = context_builder.with_override_model(model.clone());
        }

        // Set protocol type for image token estimation in HistoryManager.
        agent_loop
            .session
            .history_mut()
            .set_protocol_type(protocol_type.clone());

        // Seed ContextBuilder.tool_definitions from the freshly built
        // per-session `core.builtin_tools`. Done here so the very first
        // `build_chat_request` reflects the current runtime config
        // (already applied during `SessionTask::new` via
        // `core_mut.apply_runtime_config`). Mirror of the hot-reload
        // path inside the `UpdateRuntimeConfig` handler — single
        // derivation logic, single source of truth (`core.builtin_tools`).
        rebuild_context_tool_definitions(&agent_loop.core.builtin_tools, &mut context_builder);

        // Emit initial session state so the snapshot is populated
        // before the frontend's first fetchSessionState pull request.
        // Without this, the snapshot stays with default values until the first
        // status transition, causing the frontend to see null for
        // reasoning_effort and hide the thinking level control.
        agent_loop.emit_session_state();

        // Emit initial context-usage indicator for resumed sessions so the
        // frontend can show input/output token counts without waiting for
        // the first LLM round. Only fires when persisted SessionTokens
        // exist and model capabilities are available.
        //
        // See `context::build_persisted_ctx_usage` for the
        // caps-resolution + cumulative-patch sequence this used to
        // spell out by hand (regression t-83afab47 refactor). `None`
        // (no persisted tokens / no caps) is a legitimate skip — the
        // first live `ChunkEvent::ContextUsage` will fill the gap.
        if let Some(ref conv) = agent_loop.session.conversation
            && let Some(ctx) = crate::agent::context::build_persisted_ctx_usage(&agent_loop.core, conv)
            && let Some(ref tx) = chunk_tx
        {
            let _ = tx
                .send(SessionChunkEvent {
                    session_id: session_id.clone(),
                    event: ChunkEvent::ContextUsage(ctx),
                })
                .await;
        }

        // Saved user message for debug resume re-execution.
        // When the user presses resume after the agent loop has exited
        // (e.g. after rewind was issued post-completion), SessionTask
        // replays the agent loop with this saved message.
        let mut last_user_message: Option<(String, String)> = None;

        // ADR-047: config version polling state.
        //
        // SessionTask polls the config version at turn boundaries.
        // If config was mutated during the previous inference turn
        // (via SessionManager -> ConversationSession::apply_config),
        // the version will have changed and we apply deferred LLM-side
        // effects before processing the next message.
        //
        // Bug B fix: initialise `last_config_version = 0` and
        // `last_config_snapshot = Default` (instead of reading the
        // current version) so the FIRST iteration of the main loop
        // always takes the "version changed" branch and calls
        // `apply_llm_effects` once at startup. Without this, when the
        // session was created with `provider = None` (cache empty at
        // phase_b during onboarding) and Desktop's later
        // `model_switch` -> `apply_config` already advanced
        // `config_version` to e.g. 3, the loop's first poll sees
        // `current_version == last_config_version` and never applies
        // the LLM-side effects — the agent loop fires its first
        // `chat_message` against the stale `noop` provider and
        // surfaces "No LLM provider configured" to the user. The
        // second chat succeeds because the failed iteration re-enters
        // the loop, observes the version drift, and finally rebuilds
        // the LLM provider. Cost of the always-on initial
        // apply: one extra `build_provider_for` per SessionTask
        // startup (idempotent, ms-scale).
        let mut last_config_version: u64 = 0;
        let mut last_config_snapshot =
            crate::agent::session_config::SessionConfigSnapshot::default();

        loop {
            // ── ADR-047: Check if config was mutated during previous turn ──
            // Applied both here (turn boundary after a run) and before every
            // inbound message (see the `apply_pending_config_effects` call
            // before `match msg`), so a config change landing while the task
            // waits in `select!` is picked up BEFORE the next run starts —
            // not after it finishes.
            apply_pending_config_effects(
                &mut agent_loop,
                &mut context_builder,
                &session_id,
                &mut last_config_version,
                &mut last_config_snapshot,
            );

            // Use tokio::select! to await inbound messages, rewind
            // notifications, and resume notifications — all sourced
            // from the debug observer slot (ADR-013).
            let msg = if let Some(rewind) = agent_loop.core.debug_observer.rewind_notify().cloned()
            {
                let resume = agent_loop
                    .core
                    .debug_observer
                    .resume_notify()
                    .cloned()
                    .expect("resume_notify must be set when rewind_notify is set");
                tokio::select! {
                    msg = inbound_rx.recv() => msg,
                    _ = rewind.notified() => {
                        // Apply rewind via the observer
                        agent_loop.core.debug_observer.apply_rewind(
                            &session_id,
                            &mut agent_loop.session.history,
                        ).await;
                        continue;
                    }
                    _ = resume.notified() => {
                        // Resume or Step pressed while agent loop is not running.
                        let can_continue = if let Some(ctrl) = agent_loop.core.debug_observer.debug_ctrl() {
                            let guard = ctrl.lock().await;
                            matches!(
                                guard.state,
                                crate::debug::controller::DebugState::Running
                                    | crate::debug::controller::DebugState::Stepping
                            )
                        } else {
                            false
                        };
                        if can_continue
                            && let Some((ref content, ref msg_id)) = last_user_message
                        {
                                tracing::info!(
                                    session_id = %session_id,
                                    "Debug: resume/step notify — restarting agent loop"
                                );
                                // ADR-047: apply any config changes that
                                // landed while the run was paused before
                                // replaying (same rationale as the
                                // ChatMessage path).
                                apply_pending_config_effects(
                                    &mut agent_loop,
                                    &mut context_builder,
                                    &session_id,
                                    &mut last_config_version,
                                    &mut last_config_snapshot,
                                );
                                // Apply rewind/patches before run
                                agent_loop.core.debug_observer.apply_rewind_and_patches(
                                    &session_id,
                                    &mut agent_loop.session.history,
                                    &mut context_builder,
                                ).await;
                                // Use replay() to avoid appending a duplicate user message
                                // to history (the original is already there).
                                match agent_loop.replay(content, &mut context_builder, None).await {
                                    Ok(response) => {
                                        tracing::info!(
                                            session_id = %session_id,
                                            response_len = response.len(),
                                            "SessionTask processed chat message (replay)"
                                        );
                                        if let Some(ref tx) = chunk_tx {
                                            let event = SessionChunkEvent {
                                                session_id: session_id.clone(),
                                                event: ChunkEvent::Done {
                                                    content: response,
                                                    message_id: msg_id.clone(),
                                                },
                                            };
                                            if tx.send(event).await.is_err() {
                                                tracing::warn!(
                                                    session_id = %session_id,
                                                    "Failed to send Done chunk event (replay)"
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            session_id = %session_id,
                                            error = %e,
                                            "SessionTask agent loop error (replay)"
                                        );
                                        if let Some(ref tx) = chunk_tx {
                                            let (user_message, detail, error_type) = e.error_info();
                                            let event = SessionChunkEvent {
                                                session_id: session_id.clone(),
                                                event: ChunkEvent::Error {
                                                    user_message,
                                                    detail,
                                                    error_type,
                                                    message_id: msg_id.clone(),
                                                },
                                            };
                                            if tx.send(event).await.is_err() {
                                                tracing::warn!(
                                                    session_id = %session_id,
                                                    "Failed to send Error chunk event (replay)"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        continue;
                    }
                }
            } else {
                inbound_rx.recv().await
            };

            // ADR-047: Re-check the config version before handling ANY
            // inbound message. A model_switch / reasoning_effort /
            // workspace_switch that landed while this task was blocked in
            // `select!` must be applied before the next run (ChatMessage /
            // ContinueExecution / debug replay) — otherwise the run still
            // executes against the stale LLM provider.
            apply_pending_config_effects(
                &mut agent_loop,
                &mut context_builder,
                &session_id,
                &mut last_config_version,
                &mut last_config_snapshot,
            );

            // Note: msg is now Option<SessionMessage> directly (no
            // Ok/Err wrapper from the old timeout pattern).
            match msg {
                Some(SessionMessage::ChatMessage {
                    content,
                    message_id,
                    active_skill,
                    attached_items,
                    content_parts,
                }) => {
                    let has_attached = attached_items.as_ref().is_some_and(|a| !a.is_empty());
                    let has_content_parts = content_parts.as_ref().is_some_and(|p| !p.is_empty());
                    if content.trim().is_empty() && !has_attached && !has_content_parts {
                        tracing::warn!(
                            session_id = %session_id,
                            "SessionTask received empty chat message, ignoring"
                        );
                        continue;
                    }

                    // Save the user message so it can be replayed if
                    // resume is pressed after the agent loop exits
                    // (e.g. after a rewind issued post-completion).
                    last_user_message = Some((content.clone(), message_id.clone()));

                    // ADR-046: Attachment records are persisted inside
                    // `agent_loop.run()` (loop_.rs) AFTER the user message, so
                    // their timestamps are slightly later than the user entry.
                    // This ensures the frontend's `foldMessages` (which looks
                    // for attachment entries *after* the user message by
                    // timestamp) can fold them into a single
                    // `user_with_attachments` block.

                    // Save the raw user message before any prompt assembly.
                    // Used by AgentLoop for session title generation and
                    // other metadata extraction that needs clean user input.
                    let raw_user_message = content.clone();

                    // ADR-046: Build a *minimal* prompt-side hint for
                    // attached workspace files / selections AND user-uploaded
                    // documents. The full attachment metadata is already in
                    // the JSONL as separate system entries (written above),
                    // so the LLM has the authoritative record. The prompt
                    // hint exists only to point the LLM at the right tool
                    // call when the user asks about an attached file by name.
                    //
                    // No doc_reader pre-extraction (the LLM calls it
                    // itself), no human-readable file list (frontend
                    // renders that from the JSONL entries), no "The
                    // following workspace files..." preamble.
                    //
                    // The hint is built by the pure helper
                    // [`build_attachment_hint`] (kept module-level so the
                    // tests below can pin the wire shape). Image uploads
                    // (AttachedItem::ImageUpload) are NOT hinted here —
                    // they flow into the multimodal `ContentPart::ImageUrl`
                    // path below, so the LLM sees the picture directly.
                    // Folders (AttachedFolder) are also intentionally
                    // unhinted — the LLM has no tool to walk a directory
                    // tree in one shot.
                    let agent_home = Some(agent_loop.core.config.work_dir.clone());
                    let enriched_content = build_attachment_hint(
                        &content,
                        attached_items.as_deref(),
                        agent_home.as_deref(),
                    );

                    // Per-turn skill selection (from the chat `command`) goes
                    // on the turn-scoped slot. One unconditional call:
                    // `Some` sets it, `None` clears it so a skill chosen last
                    // turn cannot leak into this one. The agent-level
                    // Progressive summary is a different field and is not
                    // touched here — see `ContextBuilder::set_active_skill`.
                    //
                    // Injected via ContextBuilder rather than prepended to the
                    // user message so it shows up in the debug panel's system
                    // prompt section.
                    if active_skill.is_some() {
                        tracing::info!(
                            session_id = %session_id,
                            skill_len = active_skill.as_deref().map(str::len).unwrap_or(0),
                            "Applying per-turn skill instructions to ContextBuilder"
                        );
                    }
                    context_builder.set_active_skill(active_skill);

                    // ── Debug mode: apply rewind/patches before running agent loop ──
                    agent_loop
                        .core
                        .debug_observer
                        .apply_rewind_and_patches(
                            &session_id,
                            &mut agent_loop.session.history,
                            &mut context_builder,
                        )
                        .await;

                    // ── Debug mode: auto-resume if paused/stopped ──
                    // When the user sends a chat message while the debug controller
                    // is Paused, the agent loop is blocking in await_step_or_continue()
                    // on rewind_notify (polled every 100ms).  Switch to Running so the
                    // next poll sees the new state and continues processing.
                    //
                    // We do NOT match Stepping here — stepping is a deliberate mode
                    // where each phase step auto-pauses (on_phase_step_done → Paused).
                    // Overriding Stepping to Running would defeat the user's intent to
                    // single-step through the agent's reasoning.
                    //
                    // We do NOT call resume_notify.notify_one() — the paused agent loop
                    // waits on rewind_notify (polling), not resume_notify.  Calling
                    // notify_one() here would leak a permit to the next iteration of
                    // the SessionTask loop, causing an unwanted replay of the last
                    // user message via the resume.notified() branch.
                    if let Some(ctrl) = agent_loop.core.debug_observer.debug_ctrl() {
                        let mut guard = ctrl.lock().await;
                        match guard.state {
                            crate::debug::controller::DebugState::Paused
                            | crate::debug::controller::DebugState::Stopped => {
                                let old_state = guard.state;
                                guard.state = crate::debug::controller::DebugState::Running;
                                let iteration = guard.iteration;
                                drop(guard);
                                tracing::info!(
                                    session_id = %session_id,
                                    old_state = ?old_state,
                                    "Debug: auto-resuming on chat_message"
                                );
                                // Notify the debug frontend so it updates the UI
                                if let Some(event_tx) =
                                    agent_loop.core.debug_observer.debug_event_tx()
                                {
                                    let _ = event_tx.send(
                                        crate::debug::DebugEvent::ExecutionStateChanged {
                                            new_state:
                                                crate::debug::controller::DebugState::Running,
                                            iteration,
                                        },
                                    );
                                }
                            }
                            _ => {}
                        }
                    }

                    // Check for bypass-injected debug handles before each agent
                    // loop run (safety net for idle sessions).
                    agent_loop.core.debug_observer.check_pending_injection();

                    // ── ADR-046: synthesise image content parts from attached_items ──
                    //
                    // Pre-ADR-046, the frontend inlined base64 image payloads in
                    // `params.content_parts`. ADR-046 moved attachment metadata
                    // into `attached_items` (the `image_upload` variant carries
                    // document_id / format / dimensions only — never raw bytes).
                    //
                    // The Runtime now reads each image blob from the
                    // `AttachmentService` slot populated in Phase B and emits
                    // `ContentPart::ImageUrl` entries; any legacy `content_parts`
                    // sent by the frontend are preserved (text first) and the
                    // derived images are appended. If `attached_items` is
                    // None/empty, `content_parts` is forwarded unchanged — the
                    // legacy pre-046 path stays bit-compatible.
                    //
                    // Read errors are not silently dropped — they short-circuit
                    // the chat turn with the same `ChunkEvent::Error` shape used
                    // by `agent_loop.run` failures below, so the frontend sees a
                    // consistent error surface regardless of which stage failed.
                    let final_content_parts: Option<
                        Vec<acowork_core::providers::traits::ContentPart>,
                    > = match attached_items.as_deref() {
                        Some(items) if !items.is_empty() => {
                            match crate::agent::attachment_to_image::derive_image_parts(
                                agent_loop.core.attachment_service(),
                                items,
                            )
                            .await
                            {
                                Ok(derived) => {
                                    crate::agent::attachment_to_image::merge_content_parts(
                                        content_parts,
                                        derived,
                                    )
                                }
                                Err(e) => {
                                    tracing::error!(
                                        session_id = %session_id,
                                        error = %e,
                                        "Failed to derive image content_parts from attached_items"
                                    );
                                    if let Some(ref tx) = chunk_tx {
                                        let (user_message, detail, error_type) = e.error_info();
                                        let event = SessionChunkEvent {
                                            session_id: session_id.clone(),
                                            event: ChunkEvent::Error {
                                                user_message,
                                                detail,
                                                error_type,
                                                message_id: message_id.clone(),
                                            },
                                        };
                                        if tx.send(event).await.is_err() {
                                            tracing::warn!(
                                                session_id = %session_id,
                                                "Failed to send Error chunk event (derive failure)"
                                            );
                                        }
                                    }
                                    continue;
                                }
                            }
                        }
                        _ => content_parts,
                    };

                    match agent_loop
                        .run(
                            &enriched_content,
                            &mut context_builder,
                            final_content_parts,
                            Some(message_id.clone()),
                            Some(raw_user_message.as_str()),
                            attached_items.as_deref(),
                        )
                        .await
                    {
                        Ok(response) => {
                            tracing::info!(
                                session_id = %session_id,
                                response_len = response.len(),
                                "SessionTask processed chat message"
                            );
                            if let Some(ref tx) = chunk_tx {
                                let event = SessionChunkEvent {
                                    session_id: session_id.clone(),
                                    event: ChunkEvent::Done {
                                        content: response,
                                        message_id,
                                    },
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Failed to send Done chunk event"
                                    );
                                }
                            }
                            // ADR-021: Notify frontend that new conversation data
                            // is available for polling.  This is the canonical
                            // notification point — by the time run() returns, ALL
                            // response data (text, tool_calls, assistant messages,
                            // reasoning) has been persisted to JSONL regardless of
                            // whether the LLM produced text-only, tool calls, or
                            // mixed output.  The per-chunk notifications during
                            // streaming (loop_llm.rs) serve real-time streaming UX;
                            // this one guarantees a signal after full persistence.
                            agent_loop.session_core.notify_new_data_available();
                        }
                        Err(e) => {
                            tracing::error!(
                                session_id = %session_id,
                                error = %e,
                                "SessionTask agent loop error"
                            );
                            if let Some(ref tx) = chunk_tx {
                                let (user_message, detail, error_type) = e.error_info();
                                let event = SessionChunkEvent {
                                    session_id: session_id.clone(),
                                    event: ChunkEvent::Error {
                                        user_message,
                                        detail,
                                        error_type,
                                        message_id,
                                    },
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Failed to send Error chunk event"
                                    );
                                }
                            }
                        }
                    }
                }
                Some(SessionMessage::Intent {
                    content,
                    message_id,
                    conversation_id,
                    source_chat_session_id,
                    from,
                    action,
                    repo: _,
                    kind,
                    has_human,
                        chat_attachment_ids,
                    handoff_note,
                }) => {
                    // Design 27 §7.1 / plan D-C/C1: the inbound kind
                    // decides two orthogonal things — the responder role
                    // (`request` only: hide intent_send / ask_user_question,
                    // refuse human-awaiting pauses) and the write-back the
                    // turn owes (a `request`, or a human `user_message`
                    // routed from a Chat session per §5.4.4). A `response`
                    // is an ordinary turn for the initiator (the answer
                    // arrives as user input; the model decides any
                    // follow-up with a fresh request).
                    let (responder, write_back) =
                        crate::agent::intent_turn::turn_kind_semantics(&kind, &message_id);
                    let turn_ctx = || {
                        crate::agent::intent_turn::IntentTurnContext::new(
                            &message_id,
                            &conversation_id,
                            &source_chat_session_id,
                            &from,
                            &action,
                        )
                    };
                    agent_loop.pending_intent = if responder { Some(turn_ctx()) } else { None };
                    agent_loop.chat_writeback = if write_back { Some(turn_ctx()) } else { None };
                    // §5.5.5 (plan 3.3): the turn-started pointer row,
                    // written before the turn runs so a refresh mid-turn
                    // recovers "a round is in flight". Deterministic
                    // message id — a redelivered trigger dedups at the
                    // store, never a second row. Awaited rather than
                    // spawned so the pointer precedes the write-back in
                    // seq order; a failure is logged, never fatal (the
                    // pointer is a credential for the surface, not for
                    // the conversation).
                    if write_back
                        && !source_chat_session_id.is_empty()
                        && let Err(e) = crate::agent::intent_turn::append_chat_row(
                            &agent_loop.core.config.agent_instance_id,
                            &agent_loop
                                .core
                                .config
                                .gateway_host
                                .clone()
                                .unwrap_or_else(|| "127.0.0.1".to_string()),
                            &agent_loop
                                .core
                                .config
                                .mqtt_password
                                .clone()
                                .unwrap_or_default(),
                            &source_chat_session_id,
                            "system_notice",
                            &from,
                            "",
                            serde_json::json!({
                                "message_id": format!("ts:{message_id}"),
                                "notice_type": "turn_started",
                                "in_reply_to": message_id,
                                "agent_session_id": session_id,
                            }),
                        )
                        .await
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            error = %e,
                            "turn_started pointer row failed — turn proceeds without it"
                        );
                    }
                    // §5.5.4 answerability for this turn (plan 2b.3); reset
                    // with the turn like the two fields above.
                    agent_loop.chat_has_human = has_human;
                    // A rotation (§2.1) hands off with a note describing the
                    // session it replaced. Two deliveries: (a) persist it as a
                    // standalone `system` JSONL row for the transcript/UI, and
                    // (b) fold it into the model-facing message so the live
                    // turn sees the handoff — `ContextBuilder::build` drops
                    // System-role history, so the note must ride on the user
                    // turn (a cold restart re-feeds it via the restorer's
                    // `rotation_handoff` → User replay). `content` stays the
                    // user's clean text for the auto-title and the JSONL `user`
                    // entry (ADR-046).
                    let mut llm_message = match handoff_note.as_deref() {
                        Some(note) => {
                            agent_loop.record_intent_handoff(note, &message_id);
                            format!("{note}\n\n{content}")
                        }
                        None => content.clone(),
                    };
                    // Design 27 §4.2.1 chat attachments: the Intent carries
                    // ids only — the bytes are pulled over node-auth HTTP
                    // through the Gateway (never the MQTT envelope) and
                    // materialized into the agent-home `files/` dir through
                    // the SAME `AttachmentService` the execution surface
                    // uses, so the hint path / `doc_reader` resolve in
                    // lock-step. A failed pull degrades that ONE attachment
                    // to "invisible this turn" (the durable chat row still
                    // records it); the turn must still run.
                    let mut intent_content_parts: Option<
                        Vec<acowork_core::providers::traits::ContentPart>,
                    > = None;
                    let intent_attached_items: Option<Vec<acowork_core::protocol::AttachedItem>> =
                        if chat_attachment_ids.is_empty() {
                            None
                        } else if source_chat_session_id.is_empty() {
                            tracing::warn!(
                                session_id = %session_id,
                                message_id = %message_id,
                                "chat attachments carry ids but the Intent has no chat session — skipped"
                            );
                            None
                        } else {
                            let gateway_host = agent_loop
                                .core
                                .config
                                .gateway_host
                                .clone()
                                .unwrap_or_else(|| "127.0.0.1".to_string());
                            let node_token = agent_loop
                                .core
                                .config
                                .mqtt_password
                                .clone()
                                .unwrap_or_default();
                            let instance = agent_loop.core.config.agent_instance_id.clone();
                            let mut pulled = Vec::new();
                            for aid in &chat_attachment_ids {
                                match crate::agent::intent_turn::pull_chat_attachment(
                                    &gateway_host,
                                    &node_token,
                                    &instance,
                                    &source_chat_session_id,
                                    aid,
                                    &message_id,
                                )
                                .await
                                {
                                    Ok((filename, mime, bytes)) => pulled.push(PulledAttachment {
                                        filename,
                                        mime,
                                        bytes,
                                    }),
                                    Err(e) => tracing::warn!(
                                        session_id = %session_id,
                                        attachment_id = %aid,
                                        error = %e,
                                        "chat attachment pull failed, turn continues without it"
                                    ),
                                }
                            }
                            let items = materialize_pulled_attachments(
                                agent_loop.core.attachment_service(),
                                &session_id,
                                pulled,
                            )
                            .await;
                            if items.is_empty() {
                                None
                            } else {
                                llm_message = build_attachment_hint(
                                    &llm_message,
                                    Some(&items),
                                    Some(agent_loop.core.config.work_dir.as_str()),
                                );
                                // Images flow to the LLM multimodally, same
                                // as the execution surface; a derive failure
                                // only drops the image parts (the file hint
                                // above already stands).
                                match crate::agent::attachment_to_image::derive_image_parts(
                                    agent_loop.core.attachment_service(),
                                    &items,
                                )
                                .await
                                {
                                    Ok(derived) => {
                                        intent_content_parts =
                                            crate::agent::attachment_to_image::merge_content_parts(
                                                None, derived,
                                            )
                                    }
                                    Err(e) => tracing::warn!(
                                        session_id = %session_id,
                                        error = %e,
                                        "chat attachment image parts unavailable, documents only"
                                    ),
                                }
                                Some(items)
                            }
                        };
                    last_user_message = Some((content.clone(), message_id.clone()));
                    let result = agent_loop
                        .run(
                            &llm_message,
                            &mut context_builder,
                            intent_content_parts,
                            Some(message_id.clone()),
                            Some(content.as_str()),
                            intent_attached_items.as_deref(),
                        )
                        .await;
                    // Take both fields back regardless of outcome — the
                    // turn is over and the next one must not inherit them.
                    // The write-back context (D-C/C1) is what the turn-end
                    // posting and the routing registry stamp read.
                    agent_loop.pending_intent = None;
                    agent_loop.chat_has_human = None;
                    let ctx = agent_loop.chat_writeback.take();
                    // Design 27 §2.1: stamp the routing registry for the next
                    // arrival — reset the T1 idle clock and carry the T2
                    // compaction marker if this turn compacted. Bookkeeping
                    // only; the rotation itself is taken by the next
                    // `route_intent_session` (i.e. after this write-back).
                    if let Some(ref c) = ctx {
                        let key = super::intent_routing::routing_key(
                            &c.source_chat_session_id,
                            &c.conversation_id,
                            &c.request_id,
                        );
                        let compacted = agent_loop.session.is_compacted;
                        if let Ok(mut map) = intent_map.lock() {
                            let _ = map.note_turn_end(&key, compacted);
                        }
                    }
                    match result {
                        Ok(response) => {
                            if let Some(ref tx) = chunk_tx {
                                let event = SessionChunkEvent {
                                    session_id: session_id.clone(),
                                    event: ChunkEvent::Done {
                                        content: response.clone(),
                                        message_id: message_id.clone(),
                                    },
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Failed to send Done chunk event (intent turn)"
                                    );
                                }
                            }
                            agent_loop.session_core.notify_new_data_available();
                            if let Some(ctx) = ctx {
                                // System write-back (§7.1) — the model had
                                // no tool that could produce this.
                                let body =
                                    crate::agent::intent_turn::truncate_response_body(&response, 64 * 1024);
                                if let Err(e) = crate::agent::intent_turn::post_intent_response(
                                    &agent_loop.core.config.agent_instance_id,
                                    &agent_loop
                                        .core
                                        .config
                                        .gateway_host
                                        .clone()
                                        .unwrap_or_else(|| "127.0.0.1".to_string()),
                                    &agent_loop
                                        .core
                                        .config
                                        .mqtt_password
                                        .clone()
                                        .unwrap_or_default(),
                                    &session_id,
                                    &ctx,
                                    &body,
                                )
                                .await
                                {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        request_id = %ctx.request_id,
                                        error = %e,
                                        "Intent response write-back failed (turn already ended; Gateway mailbox keeps the fact)"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                session_id = %session_id,
                                error = %e,
                                "SessionTask agent loop error (intent turn)"
                            );
                            let (user_message, detail, error_type) = e.error_info();
                            if let Some(ref tx) = chunk_tx {
                                let event = SessionChunkEvent {
                                    session_id: session_id.clone(),
                                    event: ChunkEvent::Error {
                                        user_message: user_message.clone(),
                                        detail,
                                        error_type,
                                        message_id: message_id.clone(),
                                    },
                                };
                                if tx.send(event).await.is_err() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Failed to send Error chunk event (intent turn)"
                                    );
                                }
                            }
                            if let Some(mut ctx) = ctx {
                                // §7.1 failed row: user-facing summary only,
                                // no internal detail / stack on the wire.
                                ctx.outcome = "failed".to_string();
                                let _ = crate::agent::intent_turn::post_intent_response(
                                    &agent_loop.core.config.agent_instance_id,
                                    &agent_loop
                                        .core
                                        .config
                                        .gateway_host
                                        .clone()
                                        .unwrap_or_else(|| "127.0.0.1".to_string()),
                                    &agent_loop
                                        .core
                                        .config
                                        .mqtt_password
                                        .clone()
                                        .unwrap_or_default(),
                                    &session_id,
                                    &ctx,
                                    &user_message,
                                )
                                .await;
                            }
                        }
                    }
                }
                Some(SessionMessage::ContinueExecution) => {
                    tracing::debug!(
                        session_id = %session_id,
                        "SessionTask: ContinueExecution received"
                    );
                    // 429 retry UX: if the ReliableProvider is currently in a
                    // long retry wait, wake it immediately via skip_notify.
                    if let Some(ref handle) = agent_loop.session_core.retry_wait_handle {
                        handle.skip_notify.notify_one();
                        tracing::info!(
                            session_id = %session_id,
                            "Skip retry wait triggered via ContinueExecution"
                        );
                    }
                    let _ = agent_inbound_tx
                        .send(crate::agent::inbound::InboundMessage::ContinueExecution {
                            // ADR-034: populate session_id for Phase 2 routing.
                            session_id: session_id.clone(),
                            reason: "user_requested".to_string(),
                        })
                        .await;
                }
                Some(SessionMessage::UpdateRuntimeConfig(overrides)) => {
                    tracing::info!(
                        session_id = %session_id,
                        max_output_tokens = ?overrides.max_output_tokens,
                        max_iterations = ?overrides.max_iterations,
                        temperature = ?overrides.temperature,
                        context_window = ?overrides.context_window,
                        "SessionTask: applying runtime config overrides"
                    );
                    // No tool_definitions change originates from
                    // runtime-config pushes anymore — the tool
                    // compression surface was retired.
                    agent_loop.apply_runtime_config(&overrides);

                    // Push updated state to frontend immediately so the
                    // ResultsPanel temperature display reflects the new value
                    // without waiting for the next LLM iteration.
                    agent_loop.emit_session_state();
                }
                Some(SessionMessage::UpdateWorkspaceContext { context_text }) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: updating workspace context"
                    );
                    context_builder.set_workspace_context(context_text);
                }
                Some(SessionMessage::UpdateMcpTools { mcp_tools }) => {
                    tracing::info!(
                        session_id = %session_id,
                        mcp_tool_count = mcp_tools.as_ref().map(|t| t.len()).unwrap_or(0),
                        "SessionTask: updating MCP tools on AgentCore"
                    );
                    agent_loop.core.mcp_tools = mcp_tools;
                    agent_loop.core.rebuild_all_tools();
                }
                Some(SessionMessage::UpdateBuiltinTools { entries }) => {
                    let update_count = entries.len();
                    let enabled_count = entries.iter().filter(|e| e.enabled).count();
                    tracing::info!(
                        session_id = %session_id,
                        update_count,
                        enabled_count,
                        "SessionTask: updating builtin tools on AgentCore"
                    );
                    // Atomic mutation: rebuild agent_tools dispatch list + LLM
                    // tool_definitions in one shot. `apply_builtin_tools_update`
                    // re-resolves through the shared policy, so no manual
                    // override is needed here.
                    apply_builtin_tools_update(&mut agent_loop.core, &mut context_builder, entries);
                }
                Some(SessionMessage::UpdateSystemPrompt { system_prompt }) => {
                    tracing::info!(
                        session_id = %session_id,
                        prompt_len = system_prompt.len(),
                        "SessionTask: updating main-dialog system prompt (ADR-063 hot-reload)"
                    );
                    // Replace the main-dialog system prompt in the
                    // ContextBuilder. The next `build_chat_request` picks
                    // up the new value — no agent restart needed.
                    context_builder.set_system_prompt(system_prompt);
                }
                // ── ADR-094: Progressive-mode skills summary hot-reload ──
                //
                // Mirrors the `UpdateSystemPrompt` handler above: replace (or
                // clear) the agent-level `skill_summary` slot so the next
                // `build_chat_request` picks up the new value. `None` clears
                // the summary (Manual mode hot-toggle) and deliberately leaves
                // any per-turn `active_skill` alone — that slot is owned by the
                // user-message branch above, not by config reloads.
                Some(SessionMessage::UpdateSkillInstructions { instructions }) => {
                    tracing::info!(
                        session_id = %session_id,
                        has_skills = instructions.is_some(),
                        "SessionTask: updating skill instructions (ADR-094)"
                    );
                    match instructions {
                        Some(skills) => context_builder.set_skill_summary(skills),
                        None => context_builder.clear_skill_summary(),
                    }
                }
                // ── ADR-030 C3: dynamic builtin tool add/remove ──────
                //
                // A SidecarEndpointUpdate just told us a sidecar came
                // online (LSP relay ready, embed model restart, ...).
                // `entry` is already wrapped with security decorators by
                // the SessionManager, so the session just appends /
                // replaces it in its own `builtin_tools` and rebuilds
                // the dispatch list.
                Some(SessionMessage::AddDynamicBuiltinTool { entry }) => {
                    let tool_name = entry.name().to_string();
                    let replaced = if let Some(existing) = agent_loop
                        .core
                        .builtin_tools
                        .iter_mut()
                        .find(|e| e.name() == tool_name)
                    {
                        *existing = entry;
                        true
                    } else {
                        agent_loop.core.builtin_tools.push(entry);
                        false
                    };
                    // Rebuild dispatch list + LLM tool_definitions in one
                    // place (single source of truth for the reroute).
                    refresh_builtin_tools_dependents(&mut agent_loop.core, &mut context_builder);
                    tracing::info!(
                        session_id = %session_id,
                        tool = %tool_name,
                        replaced,
                        "SessionTask: dynamic builtin tool added/updated"
                    );
                }
                Some(SessionMessage::RemoveDynamicBuiltinTool { name }) => {
                    let before = agent_loop.core.builtin_tools.len();
                    agent_loop.core.builtin_tools.retain(|e| e.name() != name);
                    let removed = agent_loop.core.builtin_tools.len() < before;
                    if removed {
                        refresh_builtin_tools_dependents(
                            &mut agent_loop.core,
                            &mut context_builder,
                        );
                    }
                    tracing::info!(
                        session_id = %session_id,
                        tool = %name,
                        removed,
                        "SessionTask: dynamic builtin tool removed"
                    );
                }
                Some(SessionMessage::SetWorkDir { path }) => {
                    tracing::debug!(
                        session_id = %session_id,
                        path = %path,
                        "SessionTask: SetWorkDir received (SessionCore already updated synchronously by SessionManager)"
                    );
                    // SessionCore.current_work_dir is already set via the shared
                    // Arc<RwLock> by SessionManager — no further action needed.
                }
                Some(SessionMessage::SetWorkspacePromptFile { content }) => {
                    tracing::info!(
                        session_id = %session_id,
                        has_content = content.is_some(),
                        "SessionTask: updating workspace prompt file content"
                    );
                    context_builder.set_workspace_prompt_file(content);
                }
                Some(SessionMessage::UpdateIdentityContext { identity_context }) => {
                    tracing::info!(
                        session_id = %session_id,
                        has_context = identity_context.is_some(),
                        "SessionTask: updating identity context"
                    );
                    let next = identity_context.unwrap_or_default();
                    context_builder.set_identity_context(next.clone());
                    // Keep SessionState in sync so compaction sees the latest value.
                    agent_loop.session.set_identity_context(if next.is_empty() {
                        None
                    } else {
                        Some(next)
                    });
                }
                Some(SessionMessage::ProviderListUpdated) => {
                    // The shared global_provider_list on AgentCore is already updated
                    // (written by SessionManager before broadcasting this message).
                    // Just emit session state so the frontend sees the latest info.
                    // Reasoning_effort is NOT reset here — it was correctly initialized
                    // at session creation in build_initial_session_state, and the user
                    // may have explicitly overridden it via ReasoningEffort messages.
                    agent_loop.emit_session_state();
                }
                Some(SessionMessage::Stop { reason }) => {
                    tracing::info!(
                        session_id = %session_id,
                        reason = %reason,
                        "SessionTask: forwarding stop signal"
                    );
                    let _ = agent_inbound_tx
                        .send(crate::agent::inbound::InboundMessage::Stop { reason })
                        .await;
                }
                Some(SessionMessage::EnableDebugMode(handles)) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: injecting debug mode into existing session"
                    );
                    // Create a DevMode observer from the handles and inject it
                    // into AgentCore (ADR-013: Observer Pipeline).
                    let observer = DebugObserverImpl::new(handles);
                    agent_loop.core.set_debug_mode(observer);
                }
                Some(SessionMessage::DisableDebugMode) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: clearing debug mode from existing session"
                    );
                    // Symmetric counterpart to EnableDebugMode above.
                    // The SessionManager already cleared its controller/sender
                    // maps and `runtime_debug_handles`; we only need to drop
                    // the observer on AgentCore so future agent_loop iterations
                    // emit no debug events.
                    agent_loop.core.clear_debug_mode();
                }
                Some(SessionMessage::Close) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: Close received, shutting down"
                    );
                    break;
                }
                Some(SessionMessage::CompressAction(action)) => {
                    tracing::info!(
                        session_id = %session_id,
                        action = ?action,
                        "SessionTask: manual compress action triggered"
                    );
                    match action {
                        crate::agent::loop_::CompressionAction::CompressSummary => {
                            let model_name =
                                agent_loop.session.model().unwrap_or("default").to_string();
                            if let Err(e) = agent_loop
                                .compact_history_if_needed(&model_name, true)
                                .await
                            {
                                report_manual_compaction_error(
                                    chunk_tx.as_ref(),
                                    &session_id,
                                    "compress_action",
                                    &e,
                                )
                                .await;
                            }
                        }
                    }
                }
                Some(SessionMessage::UpdateEmbedConfig {
                    embed_endpoint,
                    embed_model_id,
                    embed_dimension,
                    embed_provider_id,
                    embed_api_key,
                }) => {
                    let cloud_provider_label =
                        embed_provider_id.as_deref().unwrap_or("<local-onnx>");
                    tracing::info!(
                        session_id = %session_id,
                        endpoint = %embed_endpoint,
                        model_id = %embed_model_id,
                        dimension = embed_dimension,
                        cloud_provider = cloud_provider_label,
                        has_api_key = embed_api_key.is_some(),
                        "SessionTask: updating embedding provider"
                    );

                    // Check if dimension migration is needed.
                    // When the memory store already has data with a different
                    // dimension, we must re-embed all nodes and rebuild the
                    // HNSW indexes before switching to the new provider.
                    //
                    // ADR-051 P4: Uses `memory_admin` (dyn MemoryAdminService)
                    // instead of concrete `grafeo_store`.
                    let needs_migration = agent_loop
                        .core
                        .memory_admin
                        .as_ref()
                        .map(|admin| admin.embedding_dim() != embed_dimension)
                        .unwrap_or(false);

                    // S1-5c: cloud embedding provider needs the API key to
                    // authenticate. For local ONNX, the key is unused and we
                    // pass None to keep the original call shape.
                    let api_key_ref = embed_api_key.as_deref();

                    if needs_migration && let Some(ref admin) = agent_loop.core.memory_admin {
                        let admin = admin.clone();
                        let old_dim = admin.embedding_dim();
                        tracing::info!(
                            old_dim,
                            new_dim = embed_dimension,
                            "Embedding dimension changed, starting migration"
                        );

                        // Build a temporary provider for the migration
                        // re-embedding. We use the new provider directly
                        // (not the fallback chain) to ensure consistent
                        // embeddings during migration.
                        let migration_provider =
                            crate::embedding::remote::RemoteEmbeddingProvider::with_config_and_timeouts(
                                &embed_endpoint,
                                api_key_ref,
                                &embed_model_id,
                                embed_dimension,
                                &agent_loop.core.config.timeouts,
                            );
                        let migration_provider = std::sync::Arc::new(migration_provider)
                            as std::sync::Arc<dyn crate::embedding::EmbeddingProvider>;

                        // `MemoryAdminService::migrate_embedding_dimension` is a
                        // sync trait method, so re-embedding means driving the
                        // async provider with `block_on`. That is only legal on a
                        // thread that is not already polling a future. This code
                        // used to call `Handle::current().block_on(...)` directly
                        // here, on the session task's own async thread, and panicked
                        // with "Cannot start a runtime from within a runtime" -
                        // which promoted the session to Errored and the agent
                        // stopped answering. Run the whole migration on a blocking
                        // thread, the same pattern
                        // `MemoryQueryUseCase::rebuild_embeddings_with_progress`
                        // already uses for the identical job.
                        let migration = tokio::task::spawn_blocking(move || {
                            let handle = tokio::runtime::Handle::current();
                            let provider_for_fn = migration_provider.clone();
                            let embed_fn = move |text: &str| -> Option<Vec<f32>> {
                                let text_owned = text.to_string();
                                match handle.block_on(provider_for_fn.embed(&text_owned)) {
                                    Ok(vec) => Some(vec),
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            "Re-embedding failed during migration, skipping node"
                                        );
                                        None
                                    }
                                }
                            };
                            admin.migrate_embedding_dimension(&embed_fn, embed_dimension)
                        })
                        .await;

                        match migration {
                            Ok(Ok(stats)) => {
                                tracing::info!(
                                    rebuilt = stats.rebuilt,
                                    skipped = stats.skipped_no_embedding + stats.skipped_no_content,
                                    errors = stats.errors,
                                    "Embedding migration complete"
                                );
                            }
                            Ok(Err(e)) => {
                                tracing::error!(
                                    error = %e,
                                    "Embedding migration failed, vector search may be broken"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "Embedding migration task died, vector search may be broken"
                                );
                            }
                        }
                    }

                    // Build the new embedding provider (local ONNX or cloud).
                    // `RemoteEmbeddingProvider::with_config_and_timeouts` already
                    // supports any OpenAI-compatible `/v1/embeddings` endpoint
                    // with optional API key — for local ONNX, the api_key is
                    // None and the sidecar runs without auth.
                    let new_onnx_provider =
                        crate::embedding::remote::RemoteEmbeddingProvider::with_config_and_timeouts(
                            &embed_endpoint,
                            api_key_ref,
                            &embed_model_id,
                            embed_dimension,
                            &agent_loop.core.config.timeouts,
                        );
                    // Wrap as FallbackEmbeddingProvider with ONNX as primary,
                    // keeping the existing provider chain as fallback (if available).
                    // Lock the dimension so that fallback providers with a
                    // different dimension are automatically filtered out.
                    let new_emb: Arc<dyn crate::embedding::EmbeddingProvider> =
                        if let Some(ref old_provider) = agent_loop.core.embedding_provider {
                            Arc::new(
                                crate::embedding::FallbackEmbeddingProvider::with_providers(
                                    vec![
                                // ONNX local base timeout: 5s. The effective
                                // timeout is scaled by in-flight concurrency
                                // (ProviderEntry::scaled_timeout_ms) so queued
                                // attempts on the serialized ONNX session are
                                // not killed before they get to run.
                                (Box::new(new_onnx_provider), 5000),
                                (
                                    Box::new(
                                        crate::embedding::ArcDelegateEmbeddingProvider::from_arc(
                                            old_provider.clone(),
                                        ),
                                    ),
                                    5000,
                                ),
                            ],
                                    crate::embedding::EmbeddingConfig::default(),
                                )
                                .with_locked_dimension(embed_dimension),
                            )
                        } else {
                            Arc::new(
                                crate::embedding::FallbackEmbeddingProvider::with_providers(
                                    vec![(Box::new(new_onnx_provider), 5000)],
                                    crate::embedding::EmbeddingConfig::default(),
                                )
                                .with_locked_dimension(embed_dimension),
                            )
                        };
                    agent_loop.core.update_embedding_provider(new_emb);
                }
                Some(SessionMessage::DisableEmbedConfig) => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: disabling embedding provider (embed sidecar unavailable)"
                    );
                    agent_loop.core.clear_embedding_provider();
                }
                Some(SessionMessage::SystemNotification { content }) => {
                    // Only inject into sessions that have already started a conversation.
                    // Prevents MCP connection failure notifications from appearing before
                    // the user has sent their first message.
                    if agent_loop.session.history_mut().is_empty() {
                        tracing::debug!(
                            session_id = %session_id,
                            "SessionTask: skipping system notification — no conversation history yet"
                        );
                        continue;
                    }
                    tracing::info!(
                        session_id = %session_id,
                        content_len = content.len(),
                        "SessionTask: injecting system notification into history"
                    );
                    agent_loop
                        .session
                        .history_mut()
                        .append(ChatMessage::user(format!(
                            "[System Notification] {content}"
                        )));
                }
                // ADR-035 Phase 3: EnableNotify/DisableNotify removed.
                None => {
                    tracing::info!(
                        session_id = %session_id,
                        "SessionTask: inbound channel closed, shutting down"
                    );
                    break;
                }
            }
        }

        // Graceful shutdown: attempt to close session with distillation
        if let Err(e) = agent_loop.close_session_with_distillation().await {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "SessionTask: failed to close session with distillation (non-fatal)"
            );
        }
    }
}

/// Rebuild the LLM-visible tool definitions from `builtin_tools` (enabled
/// only) and update the `context_builder` so the next LLM call sees the
/// current tool set.
///
/// **Why this is needed:** `UpdateBuiltinTools`, `AddDynamicBuiltinTool`,
/// and `RemoveDynamicBuiltinTool` all modify `agent_loop.core.builtin_tools`
/// and call `rebuild_all_tools()` to refresh the dispatch list, but they did
/// NOT update the `tool_definitions` stored in `ContextBuilder`. This caused
/// a mismatch between what the LLM *sees* (stale tool_definitions) and what
/// the runtime can *dispatch* (fresh all_tools):
///
/// - Tool enabled at runtime → LLM never sees it → never calls it
/// - Tool disabled at runtime → LLM still sees it → calls it → "Unknown tool"
///
/// See also: `loop_context.rs` MCP injection — MCP tools are injected
/// separately at the `build_chat_request` level, not in tool_definitions.
fn rebuild_context_tool_definitions(
    builtin_tools: &[crate::agent::agent_core::BuiltinToolEntry],
    context_builder: &mut ContextBuilder,
) {
    let new_tool_definitions: Vec<serde_json::Value> = builtin_tools
        .iter()
        .filter(|e| e.enabled)
        .map(|e| {
            let spec = e.tool.spec();
            serde_json::to_value(&spec).unwrap_or_default()
        })
        .collect();
    tracing::info!(
        old_count = context_builder
            .tool_definitions()
            .map(|t| t.len())
            .unwrap_or(0),
        new_count = new_tool_definitions.len(),
        "Rebuilding context builder tool definitions from builtin_tools"
    );
    context_builder.set_tool_definitions(new_tool_definitions);
}

/// Atomic builtin-tools enabled mutation - one of the two call sites
/// for [`AgentCore::apply_builtin_enabled_entries`] (the other being
/// the `SessionManager` template sync).
///
/// Routes the per-session write through the shared policy so the
/// platform-protected filter is never re-implemented here. After the
/// flag rewrite, refreshes both dependents in lock-step so the
/// dispatch list (`all_tools`) and the LLM's `tool_definitions`
/// (`ContextBuilder`) cannot diverge - the invariant the original
/// `UpdateBuiltinTools` hot path broke.
///
/// `entries` carries the *desired* (name, enabled) state for every
/// registered builtin tool. The wrapped `tool` impl behind each
/// registered slot is preserved by the shared policy; only the
/// `enabled` flag is rewritten, so security decorators set during
/// `ToolRegistry::activate` survive.
fn apply_builtin_tools_update(
    core: &mut AgentCore,
    context_builder: &mut ContextBuilder,
    entries: Vec<crate::agent_config::AgentToolEntry>,
) {
    // Re-resolve through the shared policy (the empty patch drops
    // unregistered slots, so this stays correct even if the incoming
    // `entries` came from a write path that didn't itself consult the
    // same policy).
    core.apply_builtin_enabled_entries(&entries);

    // Refresh both dependents in lock-step - the dispatch list for
    // the runtime, and the LLM-visible spec list for the LLM.
    refresh_builtin_tools_dependents(core, context_builder);
}

/// Rebuild `all_tools` (dispatch list) and `ContextBuilder` tool_definitions
/// (LLM-visible spec list) from the current `core.builtin_tools`.
///
/// Used by callers that mutate `builtin_tools` directly (e.g. dynamic
/// sidecar add/remove) and by [`apply_builtin_tools_update`]. Callers
/// MUST NOT call `rebuild_all_tools` / `rebuild_context_tool_definitions`
/// separately — that would re-introduce the duplicated flow that this
/// function deprecates.
fn refresh_builtin_tools_dependents(core: &mut AgentCore, context_builder: &mut ContextBuilder) {
    core.rebuild_all_tools();
    rebuild_context_tool_definitions(&core.builtin_tools, context_builder);
}

// ---------------------------------------------------------------------------
// ADR-046 §2.4 — prompt-side attachment hint builder
// ---------------------------------------------------------------------------
//
// Pure helper, lifted out of the ChatMessage handler so unit tests can pin
// the exact wire shape the LLM sees. Returns the *enriched* user-message
// content (i.e. original text + a small bracketed "use `read_file` /
// `doc_reader`" hint when at least one hintable item is present), or the
// original content unchanged when nothing is hintable.
//
// Hint contract (ADR-046 §2.4 + this revision):
//
//   workspace file   → - file: `<abs_path>`
//   workspace sel    → - file: `<abs_path>` (L10-L25)  (range collapsed when same line)
//   file_upload      → - file: `<filename>` (id=<doc_id>, format=<fmt>, path=<abs_path>)
//   image_upload     → (no hint — flows via multimodal ContentPart::ImageUrl)
//   attached_folder  → (no hint — no single-tool way to enumerate a folder)
//
// The on-disk path for `file_upload` mirrors the layout chosen by
// `attachment::on_disk_name` (the single source of truth shared with
// `RuntimeAttachmentService::write_blob_atomic`):
// `<work_dir>/files/<sanitized_stem>_<document_id>.<safe_ext>`.
// `doc_reader` resolves blobs by absolute path and dispatches on the
// file extension — so the `<ext>` suffix is the only contract surface
// between the upload pipeline and the read tool. Keeping the call here
// in lock-step with `on_disk_name` is what guarantees the path string
// the LLM receives matches the actual file on disk.
/// Derive the on-disk `format` (lowercase extension, no dot) for a
/// Gateway-forwarded chat attachment. Prefers the filename extension — the
/// same source the execution surface's multipart upload feeds the store —
/// and falls back to a MIME subtype when the name carries no dot. Unknown
/// values are fine: `safe_extension` collapses anything off the whitelist to
/// `bin`, exactly as a direct execution-view upload would.
fn forwarded_format(filename: &str, mime: &str) -> String {
    if let Some(ext) = filename.rsplit('.').next().filter(|e| !e.is_empty() && *e != filename) {
        return ext.to_ascii_lowercase();
    }
    if let Some(sub) = mime.split('/').nth(1) {
        let sub = sub.split(';').next().unwrap_or(sub);
        if !sub.is_empty() {
            return sub.to_ascii_lowercase();
        }
    }
    "bin".to_string()
}

/// Materialize HTTP-pulled Chat attachments (design 27 §4.2.1) into the
/// agent-home `files/` dir and fold them into `attached_items`.
///
/// Each blob is re-uploaded through the SAME [`AttachmentService`] the
/// execution surface uses, so the content-hash `document_id`, the on-disk
/// name (`on_disk_name`), and the LLM hint / `doc_reader` path all stay in
/// lock-step — a chat attachment and an execution-view upload of the same
/// bytes land in exactly the same place. A missing service slot or a store
/// rejection degrades that ONE blob to "invisible this turn" (a warning,
/// not an error): the durable chat row already records it, and the turn
/// must still run.
async fn materialize_pulled_attachments(
    svc: Option<&Arc<dyn crate::usecases::AttachmentService>>,
    session_id: &str,
    pulled: Vec<PulledAttachment>,
) -> Vec<acowork_core::protocol::AttachedItem> {
    let Some(svc) = svc else {
        tracing::warn!(
            session_id = %session_id,
            blob_count = pulled.len(),
            "chat attachments: AttachmentService slot not ready, pull skipped"
        );
        return Vec::new();
    };
    let mut out = Vec::new();
    for blob in pulled {
        let is_image = blob.mime.starts_with("image/");
        let format = forwarded_format(&blob.filename, &blob.mime);
        let params = crate::usecases::attachment::UploadFileParams {
            filename: blob.filename.clone(),
            format,
            bytes: blob.bytes,
            width: None,
            height: None,
        };
        match svc.upload_file(params).await {
            Ok(resp) => {
                out.push(if is_image {
                    acowork_core::protocol::AttachedItem::ImageUpload {
                        document_id: resp.document_id,
                        filename: resp.filename,
                        format: resp.format,
                        size_bytes: resp.size_bytes,
                        width: None,
                        height: None,
                        client_id: None,
                    }
                } else {
                    acowork_core::protocol::AttachedItem::FileUpload {
                        document_id: resp.document_id,
                        filename: resp.filename,
                        format: resp.format,
                        size_bytes: resp.size_bytes,
                        client_id: None,
                    }
                });
            }
            Err(e) => tracing::warn!(
                session_id = %session_id,
                filename = %blob.filename,
                error = %e,
                "chat attachment store failed, turn continues without it"
            ),
        }
    }
    out
}

fn build_attachment_hint(
    user_content: &str,
    items: Option<&[acowork_core::protocol::AttachedItem]>,
    work_dir: Option<&str>,
) -> String {
    let mut enriched = user_content.to_string();
    let Some(items) = items else {
        return enriched;
    };
    if items.is_empty() {
        return enriched;
    }

    let hint_lines: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            acowork_core::protocol::AttachedItem::AttachedFile { abs_path, .. } => {
                Some(format!("- file: `{}`", abs_path))
            }
            acowork_core::protocol::AttachedItem::AttachedSelection {
                abs_path,
                start_line,
                end_line,
                ..
            } => {
                let line_info = if start_line != end_line {
                    format!(" (L{}-L{})", start_line, end_line)
                } else {
                    format!(" (L{})", start_line)
                };
                Some(format!("- file: `{}`{}", abs_path, line_info))
            }
            acowork_core::protocol::AttachedItem::FileUpload {
                document_id,
                filename,
                format,
                ..
            } => {
                // The on-disk path is built by `attachment::on_disk_name`
                // — the same helper `write_blob_atomic` uses — so the
                // absolute path handed to the LLM always matches the
                // actual file. Without `work_dir` we still emit the hint
                // but mark the path as `<work_dir>/...` so the LLM knows
                // it can be resolved once `current_work_dir` is set by
                // the SessionManager.
                let on_disk = on_disk_name(document_id, format, filename);
                let path_str = match work_dir {
                    Some(wd) => format!("{}/files/{}", wd.trim_end_matches('/'), on_disk),
                    None => format!("<work_dir>/files/{}", on_disk),
                };
                Some(format!(
                    "- file: `{}` (id={}, format={}, path={})",
                    filename, document_id, format, path_str
                ))
            }
            // Image upload flows through the multimodal
            // ContentPart::ImageUrl path above — no prompt hint needed.
            // Folder has no single-tool enumeration in our toolchain.
            acowork_core::protocol::AttachedItem::ImageUpload { .. }
            | acowork_core::protocol::AttachedItem::AttachedFolder { .. } => None,
        })
        .collect();

    if !hint_lines.is_empty() {
        let sep = if enriched.is_empty() {
            String::new()
        } else {
            format!("{}\n\n", enriched)
        };
        enriched = format!(
            "{}[Attached workspace files & uploads — use `read_file` / `doc_reader` on demand]\n{}",
            sep,
            hint_lines.join("\n")
        );
    }
    enriched
}

#[cfg(test)]
mod tests {
    // ADR-046: replaced the old `UploadedDocumentEntry` tests with
    // direct `AttachedItem` wire-format verification. The runtime no
    // longer uses a separate `UploadedDocumentEntry` struct.

    /// Round-trips the wire schema for a file upload — the frontend sends
    /// camelCase, the runtime reads the `AttachedItem` discriminated union.
    #[test]
    fn test_attached_item_file_upload_roundtrip() {
        let item = acowork_core::protocol::AttachedItem::FileUpload {
            document_id: "0123456789ab-3".to_string(),
            filename: "report.pdf".to_string(),
            format: "pdf".to_string(),
            size_bytes: 12345,
            client_id: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"documentId\":"));
        assert!(json.contains("\"type\":\"file_upload\""));
        assert!(!json.contains("\"width\":"));

        let parsed: acowork_core::protocol::AttachedItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, item);
    }

    /// Image upload round-trips optional width/height when present.
    #[test]
    fn test_attached_item_image_upload_with_dimensions() {
        let item = acowork_core::protocol::AttachedItem::ImageUpload {
            document_id: "doc".to_string(),
            filename: "screen.png".to_string(),
            format: "png".to_string(),
            size_bytes: 987654,
            width: Some(1920),
            height: Some(1080),
            client_id: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"type\":\"image_upload\""));
        assert!(json.contains("\"width\":1920"));
        assert!(json.contains("\"height\":1080"));

        let parsed: acowork_core::protocol::AttachedItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, item);
    }

    /// CLI-style clients may omit width/height; the field is genuinely
    /// optional and serializes as absent (not null).
    #[test]
    fn test_attached_item_image_upload_omits_dimensions() {
        let item = acowork_core::protocol::AttachedItem::ImageUpload {
            document_id: "doc".to_string(),
            filename: "x.png".to_string(),
            format: "png".to_string(),
            size_bytes: 1,
            width: None,
            height: None,
            client_id: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(!json.contains("width"));
        assert!(!json.contains("height"));
    }

    // -----------------------------------------------------------------
    // ADR-046 §2.4 — `build_attachment_hint` regression suite
    //
    // Pre-revision: `session_task.rs` ChatMessage handler built the
    // hint inline with a `.filter_map(...)` that returned `None` for
    // every `AttachedItem::FileUpload`. Result: docx/pptx/xlsx/pdf
    // uploads landed in `<work_dir>/files/<id>.bin` (broken whitelist)
    // AND were invisible to the LLM (no hint ⇒ doc_reader never
    // invoked). These tests pin both halves of the fix.
    // -----------------------------------------------------------------

    use super::build_attachment_hint;

    /// No attachments → user content returned verbatim (no hint block).
    #[test]
    fn test_build_attachment_hint_no_items_returns_verbatim() {
        let s = build_attachment_hint("hello", None, Some("/work"));
        assert_eq!(s, "hello");
    }

    /// Empty vec → same as None (frontend always sends an empty array
    /// rather than omitting the field for a turn with no attachments).
    #[test]
    fn test_build_attachment_hint_empty_vec_returns_verbatim() {
        let s = build_attachment_hint("hello", Some(&[]), Some("/work"));
        assert_eq!(s, "hello");
    }

    /// Workspace `AttachedFile` keeps the pre-046 hint shape exactly
    /// — no `(id=, format=, path=)` clutter, just the absolute path.
    /// This protects the chip-renderer / read_file call sites that
    /// were already wired up before this revision.
    #[test]
    fn test_build_attachment_hint_workspace_file_shape_unchanged() {
        let items = vec![acowork_core::protocol::AttachedItem::AttachedFile {
            abs_path: "/work/src/main.rs".to_string(),
            name: "main.rs".to_string(),
            client_id: None,
        }];
        let s = build_attachment_hint("review this", Some(&items), Some("/work"));
        assert!(s.contains("- file: `/work/src/main.rs`"));
        assert!(s.contains("[Attached workspace files & uploads"));
        // The original user text is preserved verbatim — no [Attached
        // context:] prefix is allowed per ADR-046 §2.1.
        assert!(s.starts_with("review this\n\n"));
    }

    /// `AttachedSelection` with same start/end line → single line
    /// marker `(L10)`. Different lines → range `(L10-L25)`.
    #[test]
    fn test_build_attachment_hint_selection_range() {
        let items = vec![acowork_core::protocol::AttachedItem::AttachedSelection {
            abs_path: "/work/lib.rs".to_string(),
            name: "lib.rs".to_string(),
            start_line: 10,
            end_line: 25,
            client_id: None,
        }];
        let s = build_attachment_hint("x", Some(&items), Some("/work"));
        assert!(s.contains("- file: `/work/lib.rs` (L10-L25)"), "got: {s}");

        let items = vec![acowork_core::protocol::AttachedItem::AttachedSelection {
            abs_path: "/work/lib.rs".to_string(),
            name: "lib.rs".to_string(),
            start_line: 7,
            end_line: 7,
            client_id: None,
        }];
        let s = build_attachment_hint("x", Some(&items), Some("/work"));
        assert!(s.contains("- file: `/work/lib.rs` (L7)"), "got: {s}");
    }

    /// REGRESSION: `FileUpload` MUST show up in the hint block with
    /// `(id, format, path)` triple — pre-revision the filter_map
    /// returned `None` for this variant, leaving docx/pptx/xlsx/pdf
    /// uploads invisible to the LLM (no doc_reader call possible).
    #[test]
    fn test_build_attachment_hint_file_upload_emits_path_metadata() {
        let items = vec![acowork_core::protocol::AttachedItem::FileUpload {
            document_id: "0123456789ab-3".to_string(),
            filename: "report.pdf".to_string(),
            format: "pdf".to_string(),
            size_bytes: 12345,
            client_id: None,
        }];
        let s = build_attachment_hint("summarise this contract", Some(&items), Some("/work"));
        assert!(s.starts_with("summarise this contract\n\n"));
        assert!(
            s.contains("- file: `report.pdf` (id=0123456789ab-3, format=pdf, path=/work/files/report_0123456789ab-3.pdf)"),
            "got: {s}"
        );
        // The on-disk suffix must come from the format, NOT the old
        // hardcoded `.bin` (the safe_extension whitelist regression).
        assert!(
            s.contains(".pdf)"),
            "docx should land with real extension, got: {s}"
        );
        assert!(
            !s.contains(".bin"),
            "must not regress to .bin fallback, got: {s}"
        );
    }

    /// Office docs all emit their real extension in the hint path —
    /// this is what `doc_reader` keys on. Without real extensions
    /// the LLM would invoke doc_reader and get an "Unsupported
    /// document format" error back.
    #[test]
    fn test_build_attachment_hint_office_docs_emit_real_extensions() {
        for (fmt, ext) in [
            ("pdf", "pdf"),
            ("docx", "docx"),
            ("pptx", "pptx"),
            ("xlsx", "xlsx"),
        ] {
            let items = vec![acowork_core::protocol::AttachedItem::FileUpload {
                document_id: "0123456789ab-3".to_string(),
                filename: format!("report.{fmt}"),
                format: fmt.to_string(),
                size_bytes: 100,
                client_id: None,
            }];
            let s = build_attachment_hint("x", Some(&items), Some("/work"));
            assert!(
                s.contains(&format!("path=/work/files/report_0123456789ab-3.{ext}")),
                "format={fmt} must yield .{ext} suffix in hint, got: {s}"
            );
        }
    }

    /// Without `work_dir` the path segment becomes the literal
    /// `<work_dir>/files/<id>.<ext>` so the LLM at least knows the
    /// shape — better than a fabricated absolute path that points
    /// nowhere. (CLI / detached sessions may legitimately have no
    /// `current_work_dir` set.)
    #[test]
    fn test_build_attachment_hint_falls_back_to_placeholder_path() {
        let items = vec![acowork_core::protocol::AttachedItem::FileUpload {
            document_id: "abc123-0".to_string(),
            filename: "report.docx".to_string(),
            format: "docx".to_string(),
            size_bytes: 1,
            client_id: None,
        }];
        let s = build_attachment_hint("x", Some(&items), None);
        assert!(
            s.contains("path=<work_dir>/files/report_abc123-0.docx"),
            "without work_dir hint must use placeholder, got: {s}"
        );
    }

    /// Mixed attachments preserve order and the prompt hint contains
    /// every hintable variant — workspace file first, file_upload
    /// second — while image_upload (multimodal) is silently skipped.
    #[test]
    fn test_build_attachment_hint_mixed_items_in_order() {
        let items = vec![
            acowork_core::protocol::AttachedItem::AttachedFile {
                abs_path: "/work/src/lib.rs".to_string(),
                name: "lib.rs".to_string(),
                client_id: None,
            },
            acowork_core::protocol::AttachedItem::FileUpload {
                document_id: "0123456789ab-3".to_string(),
                filename: "report.docx".to_string(),
                format: "docx".to_string(),
                size_bytes: 1,
                client_id: None,
            },
            acowork_core::protocol::AttachedItem::ImageUpload {
                document_id: "img1".to_string(),
                filename: "screen.png".to_string(),
                format: "png".to_string(),
                size_bytes: 1,
                width: Some(800),
                height: Some(600),
                client_id: None,
            },
        ];
        let s = build_attachment_hint("look at these", Some(&items), Some("/work"));
        let ws_idx = s.find("/work/src/lib.rs").expect("workspace hint present");
        let doc_idx = s.find("report.docx").expect("file upload hint present");
        assert!(
            ws_idx < doc_idx,
            "workspace hint must come before upload hint, got: {s}"
        );
        assert!(
            !s.contains("screen.png"),
            "image_upload must not appear in hint (multimodal path)"
        );
    }

    /// `AttachedFolder` (Add to Chat directory attach) gets no hint
    /// — there is no single tool call the LLM can make to enumerate
    /// a folder. The JSONL record still shows the user attached it.
    #[test]
    fn test_build_attachment_hint_folder_is_silent() {
        let items = vec![acowork_core::protocol::AttachedItem::AttachedFolder {
            abs_path: "/work/src".to_string(),
            name: "src".to_string(),
            client_id: None,
        }];
        let s = build_attachment_hint("x", Some(&items), Some("/work"));
        // With only a folder (no hintable items) we return verbatim
        // — the user text must NOT be polluted with an empty hint block.
        assert_eq!(s, "x");
    }

    // ── Design 27 §4.2.1 chat-attachment forwarding ──────────────────
    use super::{forwarded_format, materialize_pulled_attachments, PulledAttachment};

    #[test]
    fn forwarded_format_prefers_filename_extension() {
        assert_eq!(forwarded_format("report.PDF", "application/pdf"), "pdf");
        assert_eq!(forwarded_format("shot.png", "image/png"), "png");
    }

    #[test]
    fn forwarded_format_falls_back_to_mime_subtype() {
        // A name with no extension derives `format` from the MIME subtype.
        assert_eq!(forwarded_format("noext", "image/jpeg"), "jpeg");
    }

    #[test]
    fn forwarded_format_defaults_to_bin_when_nothing_usable() {
        assert_eq!(forwarded_format("", ""), "bin");
    }

    #[tokio::test]
    async fn materialize_without_service_yields_nothing() {
        // The Phase-B slot not ready degrades gracefully: the attachments
        // are invisible this turn (a warning, not an error), the turn runs.
        let pulled = vec![PulledAttachment {
            filename: "pic.png".into(),
            mime: "image/png".into(),
            bytes: vec![1, 2, 3],
        }];
        let out = materialize_pulled_attachments(None, "s1", pulled).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn materialize_uploads_pulled_bytes_into_attached_items() {
        use crate::usecases::{AttachmentService, RuntimeAttachmentService};
        let dir = tempfile::tempdir().unwrap();
        let svc = RuntimeAttachmentService::new(dir.path().to_path_buf());
        let arc: std::sync::Arc<dyn AttachmentService> = std::sync::Arc::new(svc);
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let pulled = vec![
            PulledAttachment {
                filename: "pic.png".into(),
                mime: "image/png".into(),
                bytes: png,
            },
            PulledAttachment {
                filename: "note.txt".into(),
                mime: "text/plain".into(),
                bytes: b"hi".to_vec(),
            },
        ];
        let out = materialize_pulled_attachments(Some(&arc), "s1", pulled).await;
        assert_eq!(out.len(), 2);
        // image mime → ImageUpload, everything else → FileUpload.
        assert!(matches!(
            &out[0],
            acowork_core::protocol::AttachedItem::ImageUpload { filename, .. } if filename == "pic.png"
        ));
        assert!(matches!(
            &out[1],
            acowork_core::protocol::AttachedItem::FileUpload { filename, .. } if filename == "note.txt"
        ));
        // The blobs landed in the agent-home `files/` dir.
        let files_dir = dir.path().join("files");
        assert!(std::fs::read_dir(&files_dir).unwrap().count() >= 2);
    }
}
