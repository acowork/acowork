//! Context building (system prompt + history + memory + identity + skills)
//!
//! Builds the complete context for LLM requests following the priority order
//! defined in docs/03-agent-runtime.md §3.1.

use acowork_core::manifest::AgentManifest;
use acowork_core::protocol::ModelCapabilitiesInfo;
use acowork_core::providers::traits::{
    CacheControl, ChatMessage, ChatRequest, ContentPart, MessageRole, ReasoningEffort,
};
use std::sync::OnceLock;

use crate::agent::history::HistoryManager;
use crate::config::DEFAULT_TEMPERATURE;
use crate::token::counter::TokenCounter;

/// Context builder for LLM requests
pub struct ContextBuilder {
    /// System prompt from package
    system_prompt: String,
    /// Identity context (from Gateway injection)
    identity_context: Option<String>,
    /// Workspace context (self-formatted from agent_workspaces.json)
    workspace_context: Option<String>,
    /// Workspace prompt file content (CLAUDE.md / AGENTS.md) for injection
    workspace_prompt_file: Option<String>,
    /// Environment info override (for debug patching).
    /// When set, takes precedence over auto-detected platform info.
    /// Stored as the full formatted string (e.g. "## Environment\n- OS: ...\n- Shell: ...")
    environment_override: Option<String>,
    /// Tool definitions as JSON
    tool_definitions: Option<Vec<serde_json::Value>>,
    /// Model override (set by `model_switch` or session initialization;
    /// takes precedence over session default).
    override_model: Option<String>,
    /// Retrieved memory text shown in the `retrieved_memory` debug section.
    /// ADR-101 moved memory retrieval into the memory service (the Agent reads
    /// memory through the MCP `recall` tool, whose output is a tool result, not
    /// a context section), so nothing fills this slot any more — it only ever
    /// becomes non-empty through a DevMode patch.
    retrieved_memory: Option<String>,
    /// P3-4: Ambiguous conflict confirmation hint — when ≥ 3 pending
    /// ambiguous conflicts exist, this hint guides the Agent to naturally
    /// ask the user for disambiguation. Injected after retrieved memory.
    ambiguous_confirmation_hint: Option<String>,
    /// G9: Abstention guidance prompt — when retrieval returns nothing and
    /// abstention is enabled, this prompt tells the Agent to say "I'm not
    /// sure" rather than fabricate an answer. Injected after retrieved
    /// memory (same slot as `ambiguous_confirmation_hint`).
    abstention_prompt: Option<String>,
    /// ADR-094: agent-level Progressive-mode skills summary (compact
    /// name + description list). Session-lifetime: seeded when the session
    /// is created and replaced by `UpdateSkillInstructions` broadcasts.
    /// Only [`Self::set_skill_summary`] / [`Self::clear_skill_summary`]
    /// touch this — the per-turn skill-command path writes
    /// [`Self::active_skill`] instead.
    skill_summary: Option<String>,
    /// Per-turn skill instructions selected explicitly by the user (chat
    /// `command` resolved against the agent's `SkillRegistry`). Turn-scoped:
    /// cleared on any message without a command so it cannot leak into the
    /// next turn. Never touches [`Self::skill_summary`] — the two have
    /// different lifecycles and are therefore different fields.
    active_skill: Option<String>,
    /// Reasoning effort level for the LLM request.
    /// Resolved from ModelCapabilitiesInfo.default_reasoning_effort in
    /// `build_chat_request()` each iteration (supports mid-session model switch).
    /// Can also be overridden per-session via `set_reasoning_effort()`.
    reasoning_effort: Option<ReasoningEffort>,
    /// Anthropic thinking mode: "extended" or "adaptive".
    /// Resolved from ModelCapabilitiesInfo.thinking_mode in `build_chat_request()`.
    thinking_mode: Option<String>,
    /// LLM temperature override. `None` means fall through the per-agent chain:
    /// `agent_config.json.temperature` (Layer 1) → `manifest.llm.temperature` (Layer 2) → `DEFAULT_TEMPERATURE` (Layer 3).
    /// Set per-session via `set_temperature()` from AgentLoop before each build.
    temperature: Option<f32>,
    /// Reusable token counter for system prompt estimation.
    counter: TokenCounter,
}

impl ContextBuilder {
    /// Create a new context builder
    pub fn new(system_prompt: String) -> Self {
        Self {
            system_prompt,
            identity_context: None,
            workspace_context: None,
            workspace_prompt_file: None,
            environment_override: None,
            tool_definitions: None,
            override_model: None,
            retrieved_memory: None,
            ambiguous_confirmation_hint: None,
            abstention_prompt: None,
            skill_summary: None,
            active_skill: None,
            reasoning_effort: None,
            thinking_mode: None,
            temperature: None,
            counter: TokenCounter::new(),
        }
    }

    /// Set identity context (from Gateway)
    pub fn with_identity(mut self, identity: Option<String>) -> Self {
        self.identity_context = identity;
        self
    }

    /// Set workspace context (from Runtime self-formatting)
    pub fn with_workspace_context(mut self, workspace: Option<String>) -> Self {
        self.workspace_context = workspace;
        self
    }

    /// Set workspace prompt file content (CLAUDE.md / AGENTS.md)
    pub fn with_workspace_prompt_file(mut self, content: Option<String>) -> Self {
        self.workspace_prompt_file = content;
        self
    }

    /// Set tool definitions
    pub fn with_tools(mut self, tools: Vec<serde_json::Value>) -> Self {
        self.tool_definitions = Some(tools);
        self
    }

    /// Set the reasoning effort level for LLM requests.
    ///
    /// Called by `build_chat_request()` each iteration after resolving
    /// from the current model's capabilities (supports mid-session
    /// model switch). Can also be set directly for per-session overrides.
    pub fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.reasoning_effort = effort;
    }

    /// Set the Anthropic thinking mode ("extended" or "adaptive").
    pub fn set_thinking_mode(&mut self, mode: Option<String>) {
        self.thinking_mode = mode;
    }

    /// Set the LLM temperature override for this builder. `None` clears the
    /// override and falls back to [`DEFAULT_TEMPERATURE`] when building the
    /// ChatRequest. Called by AgentLoop each iteration from session/core state.
    pub fn set_temperature(&mut self, temperature: Option<f32>) {
        self.temperature = temperature;
    }

    /// Get the current reasoning effort level, if set.
    pub fn reasoning_effort(&self) -> Option<&ReasoningEffort> {
        self.reasoning_effort.as_ref()
    }

    /// Get the current temperature override, if set.
    ///
    /// `None` means the per-agent fallback chain applies at build time
    /// (`agent_config.temperature` → `manifest.llm.temperature` →
    /// [`DEFAULT_TEMPERATURE`]). ADR-054: exposed so the debug snapshot
    /// can report the request params the LLM actually received.
    pub fn temperature(&self) -> Option<f32> {
        self.temperature
    }

    /// Get the current Anthropic thinking mode ("extended" | "adaptive"), if set.
    ///
    /// ADR-054: exposed so the debug snapshot can report the request
    /// params the LLM actually received.
    pub fn thinking_mode(&self) -> Option<&str> {
        self.thinking_mode.as_deref()
    }

    /// Set model override (from `model_switch` or session initialization)
    pub fn with_override_model(mut self, model: String) -> Self {
        self.override_model = Some(model);
        self
    }

    /// Get the override model name, if set
    pub fn override_model(&self) -> Option<&str> {
        self.override_model.as_deref()
    }

    /// Update model override in-place (from model_switch message at runtime)
    pub fn set_override_model(&mut self, model: String) {
        let old = self.override_model.clone();
        tracing::info!(
            old_model = ?old,
            new_model = %model,
            "ContextBuilder model override updated via model_switch"
        );
        self.override_model = Some(model);
    }

    /// Update workspace context in-place (from WorkspaceConfigUpdate push or self-formatting)
    pub fn set_workspace_context(&mut self, context_text: String) {
        tracing::info!(
            context_len = context_text.len(),
            "ContextBuilder workspace context updated"
        );
        self.workspace_context = Some(context_text);
    }

    /// Update workspace prompt file content in-place (from workspace selection)
    pub fn set_workspace_prompt_file(&mut self, content: Option<String>) {
        if let Some(ref text) = content {
            tracing::info!(
                content_len = text.len(),
                "ContextBuilder workspace prompt file updated"
            );
        } else {
            tracing::info!("ContextBuilder workspace prompt file cleared");
        }
        self.workspace_prompt_file = content;
    }

    /// Set environment override (for debug patching).
    /// Takes precedence over auto-detected platform info in build().
    pub fn set_environment_override(&mut self, env_text: String) {
        tracing::info!(
            len = env_text.len(),
            "ContextBuilder environment override set via debug patch"
        );
        if env_text.is_empty() {
            self.environment_override = None;
        } else {
            self.environment_override = Some(env_text);
        }
    }

    /// Clear the environment override, reverting to auto-detection.
    pub fn clear_environment_override(&mut self) {
        self.environment_override = None;
    }

    /// Set the base system prompt (for debug patching).
    pub fn set_system_prompt(&mut self, prompt: String) {
        tracing::info!(
            old_len = self.system_prompt.len(),
            new_len = prompt.len(),
            "ContextBuilder system prompt updated via debug patch"
        );
        self.system_prompt = prompt;
    }

    /// Set tool definitions (for debug patching).
    pub fn set_tool_definitions(&mut self, tools: Vec<serde_json::Value>) {
        tracing::info!(
            tool_count = tools.len(),
            "ContextBuilder tool definitions updated via debug patch"
        );
        self.tool_definitions = Some(tools);
    }

    /// Set identity context in-place (for debug patching).
    pub fn set_identity_context(&mut self, identity: String) {
        if identity.is_empty() {
            tracing::info!(
                old_len = self.identity_context.as_ref().map(|s| s.len()).unwrap_or(0),
                "ContextBuilder identity context cleared"
            );
            self.identity_context = None;
        } else {
            tracing::info!(
                old_len = self.identity_context.as_ref().map(|s| s.len()).unwrap_or(0),
                new_len = identity.len(),
                "ContextBuilder identity context updated via debug patch"
            );
            self.identity_context = Some(identity);
        }
    }

    /// Set the agent-level Progressive-mode skills summary (ADR-094).
    ///
    /// Session-lifetime state: seeded at session creation, replaced by
    /// `UpdateSkillInstructions` broadcasts, overwritten by a debug patch.
    /// Empty string is treated as a clear signal, consistent with
    /// `set_environment_override()`.
    ///
    /// This does NOT touch [`Self::active_skill`] — a per-turn skill
    /// selection survives a summary reload, and clearing the summary
    /// (Manual-mode hot-toggle) does not silently drop a skill the user
    /// explicitly picked for the current turn.
    pub fn set_skill_summary(&mut self, summary: String) {
        if summary.is_empty() {
            self.clear_skill_summary();
        } else {
            tracing::info!(
                len = summary.len(),
                "ContextBuilder skill summary updated"
            );
            self.skill_summary = Some(summary);
        }
    }

    /// Clear the agent-level skills summary (Manual mode). Leaves any
    /// per-turn [`Self::active_skill`] alone.
    pub fn clear_skill_summary(&mut self) {
        if self.skill_summary.is_some() {
            tracing::debug!("ContextBuilder skill summary cleared");
            self.skill_summary = None;
        }
    }

    /// Set or clear the skill instructions explicitly selected for THIS
    /// turn via a chat `command`.
    ///
    /// `None` clears only the turn-scoped slot. The agent-level
    /// [`Self::skill_summary`] is deliberately untouched here: the two
    /// used to share one field, which meant the "don't leak a skill
    /// across turns" clear also destroyed the Progressive-mode summary
    /// seeded at session creation (a turn without a command is the normal
    /// case, so the summary vanished on the first message).
    pub fn set_active_skill(&mut self, instructions: Option<String>) {
        match instructions {
            Some(instructions) if !instructions.is_empty() => {
                tracing::info!(
                    len = instructions.len(),
                    "ContextBuilder active skill set (per-turn)"
                );
                self.active_skill = Some(instructions);
            }
            Some(_) | None => {
                if self.active_skill.is_some() {
                    tracing::debug!("ContextBuilder active skill cleared (per-turn)");
                }
                self.active_skill = None;
            }
        }
    }

    /// The composed text rendered into the `## Skill Instructions` section:
    /// the agent-level summary first, then any skill the user explicitly
    /// selected for this turn. `None` when neither is set.
    fn skill_section(&self) -> Option<String> {
        match (self.skill_summary.as_ref(), self.active_skill.as_ref()) {
            (Some(summary), Some(active)) => {
                Some(format!("{}\n\n### Selected skill\n\n{}", summary, active))
            }
            (Some(summary), None) => Some(summary.clone()),
            (None, Some(active)) => Some(active.clone()),
            (None, None) => None,
        }
    }

    /// Set retrieved memory text in-place (for debug patching).
    /// An empty string clears the section.
    pub fn set_retrieved_memory_patch(&mut self, memory_text: String) {
        if memory_text.is_empty() {
            tracing::debug!("ContextBuilder retrieved memory cleared via debug patch");
            self.retrieved_memory = None;
        } else {
            tracing::debug!(
                len = memory_text.len(),
                "ContextBuilder retrieved memory updated via debug patch"
            );
            self.retrieved_memory = Some(memory_text);
        }
    }

    /// Apply a debug PatchSet to the context builder.
    ///
    /// Only sections present in the patch are applied; sections that are
    /// not patched remain unchanged. Unknown section keys are rejected
    /// (ADR-054 §6 typo safety) and a `TypeMismatch` error is returned if
    /// a patch value's variant doesn't match the section's expected type.
    ///
    /// Per-section patch semantics (type validation, empty-string clearing,
    /// tool_definitions array check) live in [`resolve_patch`] — the SAME
    /// resolution path used by `handle_patch_context` for snapshot preview,
    /// so preview and apply-time behavior can never drift.
    pub fn apply_patches(
        &mut self,
        patches: &crate::debug::protocol::PatchSet,
    ) -> Result<(), crate::debug::protocol::PatchError> {
        for (key, value) in &patches.patches {
            match resolve_patch(key, value)? {
                ResolvedPatch::Text(content) => match key.as_str() {
                    "system_prompt" => self.set_system_prompt(content),
                    "workspace_context" => self.set_workspace_context(content),
                    "environment" => self.set_environment_override(content),
                    // Debug patches override the persistent (agent-level)
                    // slot; the turn-scoped active skill is runtime state
                    // the debugger does not address by section key.
                    "skill_instructions" => self.set_skill_summary(content),
                    "workspace_prompt_file" => self.set_workspace_prompt_file(Some(content)),
                    "ambiguous_confirmation_hint" => {
                        self.set_ambiguous_confirmation_hint(content);
                    }
                    _ => unreachable!("resolve_patch only yields Text for text sections"),
                },
                ResolvedPatch::Json(value) => match key.as_str() {
                    "retrieved_memory" => self.set_retrieved_memory_patch(value.to_string()),
                    "identity_context" => self.set_identity_context(value.to_string()),
                    _ => unreachable!("resolve_patch only yields Json for json sections"),
                },
                ResolvedPatch::ToolDefinitions(defs) => {
                    debug_assert_eq!(key, "tool_definitions");
                    self.set_tool_definitions(defs);
                }
                // Empty string clears the section — build() falls back:
                // environment → auto-detect; the three ADR-054 sections → omitted.
                ResolvedPatch::Clear => match key.as_str() {
                    "environment" => self.set_environment_override(String::new()),
                    "workspace_prompt_file" => self.set_workspace_prompt_file(None),
                    "ambiguous_confirmation_hint" => self.clear_ambiguous_confirmation_hint(),
                    _ => unreachable!("resolve_patch only yields Clear for clearable sections"),
                },
            }
        }
        Ok(())
    }

    /// P3-4: Set ambiguous conflict confirmation hint for injection into
    /// the system prompt. When ≥ 3 pending ambiguous conflicts exist,
    /// this hint guides the Agent to naturally ask the user about them.
    pub fn set_ambiguous_confirmation_hint(&mut self, hint: String) {
        self.ambiguous_confirmation_hint = Some(hint);
    }

    /// G9: Set the abstention guidance prompt for injection into the
    /// system prompt. When retrieval returns nothing and abstention is
    /// enabled, this prompt steers the Agent to abstain rather than guess.
    pub fn set_abstention_prompt(&mut self, prompt: String) {
        self.abstention_prompt = Some(prompt);
    }

    /// G9: Clear the abstention guidance prompt.
    pub fn clear_abstention_prompt(&mut self) {
        self.abstention_prompt = None;
    }

    /// G9: Get the abstention guidance prompt text, if set.
    pub fn abstention_prompt(&self) -> Option<&str> {
        self.abstention_prompt.as_deref()
    }

    /// Clear the ambiguous-confirmation hint.
    ///
    /// ADR-054: debug patch empty-string clearing semantics — an empty
    /// string patch removes the section so `build()` omits it entirely
    /// (consistent with `workspace_prompt_file`).
    pub fn clear_ambiguous_confirmation_hint(&mut self) {
        self.ambiguous_confirmation_hint = None;
    }

    /// Get the ambiguous-confirmation hint text, if set.
    ///
    /// ADR-054: exposed so the debug snapshot can surface this section
    /// (previously invisible — a common "why is the agent asking
    /// disambiguation questions" blind spot).
    pub fn ambiguous_confirmation_hint(&self) -> Option<&str> {
        self.ambiguous_confirmation_hint.as_deref()
    }

    // ── Section accessors for debug ContextSnapshot ──

    /// Get the base system prompt (before identity/memory/workspace injection).
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Get the identity context text, if set.
    pub fn identity_context(&self) -> Option<&str> {
        self.identity_context.as_deref()
    }

    /// Get the tool definitions as JSON values, if set.
    pub fn tool_definitions(&self) -> Option<&[serde_json::Value]> {
        self.tool_definitions.as_deref()
    }

    /// Get the retrieved memory text, if set.
    pub fn retrieved_memory(&self) -> Option<&str> {
        self.retrieved_memory.as_deref()
    }

    /// Get the workspace context text, if set.
    pub fn workspace_context(&self) -> Option<&str> {
        self.workspace_context.as_deref()
    }

    /// Get the workspace prompt file content (e.g. CLAUDE.md / AGENTS.md), if set.
    pub fn workspace_prompt_file(&self) -> Option<&str> {
        self.workspace_prompt_file.as_deref()
    }

    /// Get the environment override text, if set.
    pub fn environment_override(&self) -> Option<&str> {
        self.environment_override.as_deref()
    }

    /// The composed skills text injected under the
    /// "## Skill Instructions" section: agent-level Progressive summary
    /// plus this turn's explicitly selected skill, whichever are set.
    /// Owned because it may concatenate the two lifecycle slots.
    pub fn skill_instructions(&self) -> Option<String> {
        self.skill_section()
    }

    /// Build the complete ChatRequest for the LLM.
    ///
    /// ADR-060 v2 (with §5.5 erratum): output is reorganized into two
    /// cache-friendly blocks:
    /// - Block A: static system kernel (single SystemMessage + ephemeral breakpoint)
    /// - Block B: append-only conversation history — carries real
    ///   `todo_write` tool results, which are the canonical todo state
    ///   source (a full todo list embedded as a Block C tail snapshot
    ///   would shift every turn and never hit the cache).
    ///
    /// The current user message is carried by Block B's tail — `run_inner`
    /// appends it to history before calling `build`. There is NO separate
    /// "Block D" duplicate: a message already in Block B has already been
    /// sent to the LLM, so re-pushing it at the tail is a pure duplicate
    /// (extra tokens + makes the model see the same user turn twice). The
    /// only legitimate "copy from history to tail" case is compaction's
    /// `inject_round_after_marker`, which restores a round the
    /// summary replaced — that is a recovery, not a duplicate, and lives
    /// in `loop_context.rs::compact_history_if_needed`, independent of
    /// `build`.
    pub fn build(
        &self,
        manifest: &AgentManifest,
        history: &HistoryManager,
        gateway_capabilities: Option<&ModelCapabilitiesInfo>,
        max_output_tokens_limit: u64,
    ) -> ChatRequest {
        let mut messages = Vec::new();

        // ── Block A: static kernel (ADR-060 §5.2) ──
        // Byte-stable across iterations: no dynamic block (retrieved_memory,
        // abstention_prompt) is embedded here. Todo state lives in Block B's
        // history (real `todo_write` tool results); ambiguous_confirmation_hint
        // is intentionally not injected this round.
        let mut system_content = self.system_prompt.clone();

        // 2. Identity context (if available)
        if let Some(ref identity) = self.identity_context {
            system_content.push_str(&format!(
                "\n\n## User Identity\n{identity}\n\n\
                 Reply in the language specified by the Language field above."
            ));
        }

        // 2.2 Workspace context (if available, from Gateway push)
        if let Some(ref workspace) = self.workspace_context {
            system_content.push_str(&format!("\n\n{workspace}"));
        }

        // 2.5 Retrieved memory context from Grafeo (long-term memory)
        if let Some(ref memory) = self.retrieved_memory {
            system_content.push_str(&format!("\n\n## Relevant Memories\n{memory}"));
        }

        // 2.5.1 Abstention guidance (G9): injected only when retrieval
        // returned nothing and abstention was enabled. In the abstention
        // case `retrieved_memory` is empty, so this slot is otherwise
        // unused — Block A stays byte-stable on the normal path. Position
        // matches the historical `ambiguous_confirmation_hint` slot.
        if let Some(ref prompt) = self.abstention_prompt {
            system_content.push_str(&format!("\n\n## Memory Abstention Guidance\n{prompt}"));
        }

        // 2.6 Skill instructions: agent-level Progressive summary + this
        // turn's explicitly selected skill (whichever are set). Composed by
        // `skill_section` so the two lifecycle slots render as one section.
        if let Some(skills) = self.skill_section() {
            system_content.push_str(&format!("\n\n## Skill Instructions\n{skills}"));
        }

        // 3. Environment platform info
        // Debug override takes precedence over auto-detected platform info,
        // allowing the debugger to modify environment context without changing
        // the actual runtime environment.
        if let Some(ref env_override) = self.environment_override {
            system_content.push_str(&format!("\n\n{env_override}"));
        } else {
            system_content.push_str(&format!("\n\n{}", detect_environment_text()));
        }

        // 3.2 Workspace prompt file content (CLAUDE.md / AGENTS.md)
        // Injected at the end for maximum visibility.
        if let Some(ref prompt_file) = self.workspace_prompt_file {
            system_content.push_str(&format!("\n\n## Workspace Prompt File\n{prompt_file}"));
        }

        // ADR-060 v2: `todo_context` removed entirely (todos live in Block B
        // history via real `todo_write` tool results); ambiguous_confirmation_hint
        // is not injected this round (ADR-060 §5.2).

        // 3.5 Tool definitions are passed separately in ChatRequest

        // Block A carries the ephemeral cache breakpoint (ADR-060 §5.1/§5.6).
        let mut system_msg = ChatMessage::system(system_content);
        system_msg.cache_control = Some(CacheControl::Ephemeral);
        messages.push(system_msg);

        // Estimate system prompt tokens for observability
        let system_msg = messages.last().unwrap();
        let system_tokens = self.counter.count_message(system_msg, None);
        tracing::debug!(system_tokens, "System prompt token estimation");

        // 7. Conversation history — Block B (ADR-060 §5.3)
        // Filter out System messages from history — only the first system message
        // (created above) should exist. Some LLM providers (e.g. MiniMax) reject
        // system messages at non-first positions.
        messages.extend(
            history
                .messages()
                .iter()
                .filter(|m| !matches!(m.role, MessageRole::System))
                .cloned(),
        );

        // ADR-060 v2: Block C (dynamic todo snapshot at tail) was removed. The
        // full todo list (~hundreds of tokens) embedded as a tail snapshot
        // would shift position every turn as Block B grows, never hitting
        // the prompt cache. Todos now live in Block B (real `todo_write`
        // tool results in history) and survive compaction via the
        // `inject_round_after_marker` post-step.

        // ADR-060 §5.5 erratum: the former "Block D" (a caller-passed
        // duplicate of the current user message pushed at the tail) is
        // removed. The current user message is already at Block B's tail
        // (run_inner appends it to history before build), so the duplicate
        // added no cache value (the stable prefix is A+B, unaffected) and
        // cost tokens + made the model see the same turn twice.

        // 7.5 Sanitize messages before sending to LLM
        // This fixes corrupted tool_call data that would cause 400 errors
        HistoryManager::sanitize_messages(&mut messages);

        // 7.6 Filter image content_parts for non-vision models
        // When the model doesn't support image input (modalities known and
        // lacks "image"), strip ImageUrl parts from multimodal content to
        // prevent API 400 errors. This handles the case where a user switches
        // from a vision model to a text-only model mid-session — the session
        // history still contains base64 image data that would otherwise be
        // sent to a model that cannot process it.
        if let Some(caps) = gateway_capabilities {
            let supports_image = caps
                .modalities
                .as_ref()
                .map(|m| m.input.iter().any(|s| s == "image"))
                .unwrap_or(true); // true=don't filter when modalities unknown
            if !supports_image {
                for msg in &mut messages {
                    if let Some(ref mut parts) = msg.content_parts {
                        // Keep only Text parts, strip ImageUrl parts
                        parts.retain(|p| matches!(p, ContentPart::Text { .. }));
                        // If no text parts remain, set content_parts to None
                        // to avoid sending an empty array to the API.
                        if parts.is_empty() {
                            msg.content_parts = None;
                        }
                    }
                }
            }
        }

        // Determine the model to use.
        // Model comes from override_model (set by model_switch or session init).
        // When absent, model is empty — the LLM call will fail with a clear error.
        let model = self.override_model.clone().unwrap_or_default();

        // Auto-set max_tokens based on model capabilities with the following priority:
        // 1. manifest.llm.max_tokens (user explicit config, backward compatible)
        // 2. Gateway model_capabilities.max_output_tokens
        // 3. Warn + conservative default 4096
        let max_tokens = if let Some(explicit) = manifest.llm.max_tokens {
            tracing::info!(
                max_tokens = explicit,
                source = "manifest",
                "Using explicitly configured max_tokens"
            );
            Some(explicit)
        } else if let Some(caps) = gateway_capabilities {
            let raw = caps.max_output_tokens;

            if raw == 0 {
                // If max_output_tokens is 0, it means the value was not provided
                // (e.g. locally-discovered models without capability info).
                // Don't guess — omit max_tokens entirely and let the model
                // use its own default (typically the full context window).
                tracing::info!(
                    model = %model,
                    "max_output_tokens not configured, omitting max_tokens from request"
                );
                None
            } else {
                // Cap max_output_tokens: it should never exceed context_window.
                // models.dev data or user input may provide inflated values that
                // the actual API rejects (e.g. alibaba-cn proxy limits kimi-k2.6
                // max_tokens to 98304, but models.dev reports 384000).
                let context_window = caps.context_window;
                let recommended = if raw > context_window {
                    tracing::warn!(
                        model = %model,
                        raw_max_output_tokens = raw,
                        context_window = context_window,
                        "max_output_tokens exceeds context_window, capping"
                    );
                    context_window
                } else {
                    raw
                };
                // Hard cap: many provider APIs reject max_tokens above a certain limit.
                // This follows opencode's approach: Math.min(limit.output, 32000).
                // models.dev's limit.output can be inflated (e.g. 384000) but
                // actual API max_tokens parameter is usually capped much lower.
                // The limit is now configurable via Gateway config (max_output_tokens_limit).
                // Set to 0 to disable the limit.
                let hard_cap = if max_output_tokens_limit == 0 {
                    u64::MAX // No limit
                } else {
                    max_output_tokens_limit
                };
                let recommended = if recommended > hard_cap {
                    tracing::warn!(
                        model = %model,
                        requested = recommended,
                        cap = hard_cap,
                        "max_output_tokens exceeds hard cap, capping"
                    );
                    hard_cap
                } else {
                    recommended
                };
                let recommended = recommended.min(u32::MAX as u64) as u32;
                tracing::info!(
                    model = %model,
                    recommended_max_tokens = recommended,
                    source = "gateway",
                    "Auto-setting max_tokens from Gateway model capabilities"
                );
                Some(recommended)
            }
        } else {
            tracing::warn!(
                model = %model,
                "No model capabilities received from Gateway, using conservative default max_tokens=4096. Configure model capabilities in Desktop App settings."
            );
            Some(4096)
        };

        // Safety check: ensure max_tokens does not exceed context window capacity
        let max_tokens = max_tokens.map(|mt| {
            if let Some(caps) = gateway_capabilities {
                let context_window = caps.context_window;
                // Build a combined text from message content + tool_call arguments
                // for model-aware token counting via the unified API.
                let combined: String = messages.iter().fold(String::new(), |mut acc, m| {
                    acc.push_str(&m.content);
                    if let Some(ref tcs) = m.tool_calls {
                        for tc in tcs {
                            acc.push_str(&tc.function.name);
                            acc.push_str(&tc.function.arguments);
                        }
                    }
                    acc
                });
                // Safety margin: +10% overhead for role labels, formatting, and special tokens.
                let approx_msg_tokens =
                    (crate::token::count_text(&combined) as f64 * 1.1).ceil() as u64;
                if (approx_msg_tokens + mt as u64) > context_window {
                    let safe_max =
                        (context_window.saturating_sub(approx_msg_tokens)).max(256) as u32;
                    tracing::warn!(
                        model = %model,
                        requested_max_tokens = mt,
                        safe_max_tokens = safe_max,
                        approx_msg_tokens = approx_msg_tokens,
                        context_window = context_window,
                        "max_tokens would exceed context window, reducing to safe value"
                    );
                    safe_max
                } else {
                    mt
                }
            } else {
                // No gateway capabilities available — Runtime does not speculate.
                // Trust the max_tokens value already determined above.
                mt
            }
        });

        tracing::info!(
            model = %model,
            max_tokens = ?max_tokens,
            "Final max_tokens for ChatRequest"
        );
        ChatRequest {
            model,
            messages,
            temperature: Some(round_temperature(
                self.temperature.unwrap_or(DEFAULT_TEMPERATURE),
            )),
            max_tokens,
            tools: self.tool_definitions.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            thinking_mode: self.thinking_mode.clone(),
        }
    }
}

/// Widen an `f32` temperature to `f64` for the wire, dropping the binary
/// representation artifacts.
///
/// `ContextBuilder::temperature` is an `f32` (it comes from the `f32`
/// session/agent config), so a plain `as f64` cast exposes the float
/// expansion: `0.1_f32 as f64 == 0.10000000149011612`. Volcengine Ark
/// validates the *decimal places* of `temperature` and rejects anything
/// below 2 — `glm-5.3-flash` returns `400 InvalidParameter` for `0.123`
/// and for the `f32` artifact, while accepting `0.12`. Other models on the
/// very same endpoint (deepseek / doubao) accept the expansion, which is
/// why the bug only showed up on GLM.
///
/// ponytail: 2 decimals is exactly the resolution the Desktop already
/// selects and displays (`toFixed(2)`), so nothing expressible in the UI is
/// lost. If a UI ever offers finer steps, the provider-side limit must be
/// re-probed before widening this.
fn round_temperature(t: f32) -> f64 {
    (f64::from(t) * 100.0).round() / 100.0
}

/// Build a [`PatchError::TypeMismatch`] for a section patch whose value
/// variant doesn't match the section's expected type.
fn patch_type_mismatch(
    key: &str,
    expected: &'static str,
    value: &crate::debug::protocol::PatchValue,
) -> crate::debug::protocol::PatchError {
    crate::debug::protocol::PatchError::TypeMismatch {
        section: key.to_string(),
        expected,
        actual: value.variant_name(),
    }
}

/// Result of resolving a debug patch value against a section key.
///
/// ADR-054: the per-section patch semantics (type validation, empty-string
/// clearing, tool_definitions array check) live in ONE place — [`resolve_patch`]
/// — shared by `ContextBuilder::apply_patches` (actual application at build
/// time) and `handle_patch_context` (snapshot preview at RPC time), so the
/// two can never drift on edge cases (type mismatch, empty string, non-array
/// tool_definitions).
pub enum ResolvedPatch {
    /// String content for text-valued sections.
    Text(String),
    /// JSON value for `retrieved_memory` / `identity_context`.
    Json(serde_json::Value),
    /// Tool definitions — validated as a JSON array.
    ToolDefinitions(Vec<serde_json::Value>),
    /// Section cleared: `build()` falls back (environment → auto-detect;
    /// workspace_prompt_file / ambiguous_confirmation_hint → omitted).
    Clear,
}

/// Resolve a `(section key, patch value)` pair into the final content to
/// apply/store, or [`ResolvedPatch::Clear`] for empty-string clearing.
///
/// Validates the value variant against the section's expected type and
/// rejects mismatches with [`PatchError::TypeMismatch`] (ADR-054 §6 typo
/// safety). Unknown keys are rejected with [`PatchError::UnknownSection`].
pub fn resolve_patch(
    key: &str,
    value: &crate::debug::protocol::PatchValue,
) -> Result<ResolvedPatch, crate::debug::protocol::PatchError> {
    use crate::debug::protocol::{PatchError, PatchValue};

    match key {
        "system_prompt" | "workspace_context" | "skill_instructions" => match value {
            PatchValue::Text { value } => Ok(ResolvedPatch::Text(value.clone())),
            _ => Err(patch_type_mismatch(key, "text", value)),
        },
        // environment: empty string clears the override — build() falls
        // back to auto-detected platform info.
        "environment" => match value {
            PatchValue::Text { value } if value.is_empty() => Ok(ResolvedPatch::Clear),
            PatchValue::Text { value } => Ok(ResolvedPatch::Text(value.clone())),
            _ => Err(patch_type_mismatch(key, "text", value)),
        },
        "tool_definitions" => match value {
            PatchValue::Json { value } => match value.as_array() {
                Some(defs) => Ok(ResolvedPatch::ToolDefinitions(defs.clone())),
                None => Err(PatchError::TypeMismatch {
                    section: key.to_string(),
                    expected: "json array",
                    actual: "json non-array",
                }),
            },
            _ => Err(patch_type_mismatch(key, "json", value)),
        },
        "retrieved_memory" | "identity_context" => match value {
            PatchValue::Json { value } => Ok(ResolvedPatch::Json(value.clone())),
            _ => Err(patch_type_mismatch(key, "json", value)),
        },
        // ADR-054 step 3 sections: empty string clears (consistent clearing
        // semantics across all three — previously ambiguous_confirmation_hint
        // had none).
        "workspace_prompt_file" | "ambiguous_confirmation_hint" => match value {
            PatchValue::Text { value } if value.is_empty() => Ok(ResolvedPatch::Clear),
            PatchValue::Text { value } => Ok(ResolvedPatch::Text(value.clone())),
            _ => Err(patch_type_mismatch(key, "text", value)),
        },
        _ => Err(PatchError::UnknownSection(key.to_string())),
    }
}

/// Cached environment text — computed once per process (ADR-060 §6.4).
///
/// Environment info is static within a process lifetime; caching it makes
/// every `build()` call cheaper and guarantees byte stability across
/// iterations (a cache-friendly Block A requirement).
static CACHED_ENV_TEXT: OnceLock<String> = OnceLock::new();

/// Detect and format the environment info text that gets injected into
/// the system prompt. Used by debug snapshot capture and ContextBuilder::build().
///
/// ADR-060 §6.4: result is memoized in a process-global [`OnceLock`]; the
/// first call formats the text, subsequent calls return the cached `&str`.
pub fn detect_environment_text() -> &'static str {
    CACHED_ENV_TEXT.get_or_init(|| {
        let shell_info = crate::platform::detected_shell();
        let available_shells = crate::platform::detected_shells();
        let shell_tools_desc: Vec<String> = available_shells
            .iter()
            .map(|s| {
                let primary = if s.is_primary {
                    " (primary)"
                } else {
                    " (fallback)"
                };
                format!("{}{}", s.tool_name, primary)
            })
            .collect();
        format!(
            "## Environment\n- Operating System: {}\n- Architecture: {}\n- Shell: {}\n- Available Shell Tools: {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            shell_info.display_name,
            shell_tools_desc.join(", ")
        )
    })
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest() -> AgentManifest {
        AgentManifest::from_toml(
            r#"
            agent_id = "com.test.ctx"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#,
        )
        .unwrap()
    }

    #[test]
    fn test_context_builder_basic() {
        let manifest = test_manifest();
        let mut history = HistoryManager::new(10000);
        history.append(ChatMessage::user("Hello"));

        let builder = ContextBuilder::new("You are a helpful assistant.".to_string())
            .with_override_model("gpt-4".to_string());
        let request = builder.build(&manifest, &history, None, 32_768);

        assert_eq!(request.model, "gpt-4");
        assert_eq!(request.messages.len(), 2); // system + user
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert_eq!(request.messages[1].role, MessageRole::User);
        // ADR-060: Block A carries the ephemeral cache breakpoint.
        assert_eq!(
            request.messages[0].cache_control,
            Some(acowork_core::providers::traits::CacheControl::Ephemeral)
        );
    }

    /// Regression test: the built request's temperature must not carry the
    /// `f32`→`f64` expansion artifacts, and must stay within the 2 decimal
    /// places Volcengine Ark's `glm-5.3-flash` accepts (`0.123` → 400,
    /// `0.12` → 200).
    #[test]
    fn test_build_chat_request_rounds_temperature_for_wire() {
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);

        for (input, expected) in [
            (0.1_f32, "0.1"),
            (0.3, "0.3"),
            (0.7, "0.7"),
            (0.15, "0.15"),
            // Would be rejected verbatim; must go out as 0.12.
            (0.123, "0.12"),
        ] {
            let mut builder = ContextBuilder::new("You are a helper.".to_string());
            builder.set_temperature(Some(input));
            let request = builder.build(&manifest, &history, None, 32_768);
            let wire = serde_json::json!(request.temperature);
            assert_eq!(wire.to_string(), expected, "input {input}");
        }
    }

    #[test]
    fn test_context_builder_with_identity() {
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);

        let builder = ContextBuilder::new("You are a helper.".to_string())
            .with_identity(Some("Name: Alice, City: Shanghai".to_string()));

        let request = builder.build(&manifest, &history, None, 32_768);
        assert!(request.messages[0].content.contains("Alice"));
    }

    /// Sentinel test: when identity context contains a Language field, the
    /// built system prompt must include the language directive so the LLM
    /// replies in the user's preferred language. Guards against accidental
    /// removal during future refactors of `ContextBuilder::build()`.
    #[test]
    fn test_context_builder_injects_language_directive_when_identity_has_language() {
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);

        let identity =
            "- Display Name: Alice\n- Language: zh-CN\n- Timezone: Asia/Shanghai".to_string();
        let builder =
            ContextBuilder::new("You are a helper.".to_string()).with_identity(Some(identity));

        let request = builder.build(&manifest, &history, None, 32_768);
        let system = &request.messages[0].content;
        assert!(
            system.contains("Language field above"),
            "system prompt must instruct the LLM to follow the Language field; got:\n{system}"
        );
        assert!(
            system.contains("zh-CN"),
            "system prompt must contain the raw Language value so the directive is resolvable; got:\n{system}"
        );
    }

    // ── apply_patches / resolve_patch (ADR-054 single-source semantics) ──

    #[test]
    fn apply_patches_empty_string_clears_adr054_sections() {
        use crate::debug::protocol::{PatchSet, PatchValue};
        use std::collections::HashMap;

        let mut builder =
            ContextBuilder::new("base".to_string()).with_override_model("gpt-4".to_string());
        builder.set_ambiguous_confirmation_hint("hint".to_string());
        builder.set_workspace_prompt_file(Some("CLAUDE.md content".to_string()));

        // Empty string clears the ADR-054 step-3 sections consistently.
        // ADR-060 v2: `todo_context` removed — Block C is gone.
        let patches = PatchSet {
            patches: HashMap::from([
                (
                    "ambiguous_confirmation_hint".to_string(),
                    PatchValue::Text {
                        value: String::new(),
                    },
                ),
                (
                    "workspace_prompt_file".to_string(),
                    PatchValue::Text {
                        value: String::new(),
                    },
                ),
            ]),
        };
        builder.apply_patches(&patches).expect("clear must succeed");

        assert!(
            builder.ambiguous_confirmation_hint().is_none(),
            "ambiguous_confirmation_hint cleared"
        );
        assert!(
            builder.workspace_prompt_file().is_none(),
            "workspace_prompt_file cleared"
        );

        // build() must omit the cleared sections entirely.
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);
        let request = builder.build(&manifest, &history, None, 32_768);
        let system = &request.messages[0].content;
        assert!(
            !system.contains("Memory Conflicts Needing Confirmation"),
            "ambiguous hint omitted"
        );
        assert!(
            !system.contains("Workspace Prompt File"),
            "prompt file omitted"
        );
    }

    // ── ADR-060: Block A/B/C/D layout ──

    #[test]
    fn test_build_block_layout_a_b_current_in_b_tail() {
        // ADR-060 v2 + §5.5 erratum: Block C and Block D are both removed.
        // Output is A/B only; the current user message lives at Block B's
        // tail (run_inner appends it to history before build) and appears
        // EXACTLY ONCE — no tail duplicate.
        let manifest = test_manifest();
        let mut history = HistoryManager::new(10000);
        history.append(ChatMessage::user("First turn"));
        history.append(ChatMessage::assistant("First reply"));

        let builder =
            ContextBuilder::new("Kernel".to_string()).with_override_model("gpt-4".to_string());

        // Current user message is part of history (Block B), not a separate arg.
        let current = ChatMessage::user("Second turn");
        history.append(current.clone());
        let request = builder.build(&manifest, &history, None, 32_768);

        // [0] Block A (System + ephemeral breakpoint)
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert_eq!(
            request.messages[0].cache_control,
            Some(acowork_core::providers::traits::CacheControl::Ephemeral)
        );
        // Block A must NOT contain todo content (todo lives in Block B history).
        assert!(!request.messages[0].content.contains("Todo Task List"));
        // Block A must NOT contain the ambiguous hint section.
        assert!(!request.messages[0].content.contains("Memory Conflicts"));

        // [1..3] Block B: history turns (user + assistant + current user)
        assert_eq!(request.messages[1].role, MessageRole::User);
        assert_eq!(request.messages[1].content, "First turn");
        assert_eq!(request.messages[2].role, MessageRole::Assistant);
        assert_eq!(request.messages[2].content, "First reply");
        assert_eq!(request.messages[3].role, MessageRole::User);
        assert_eq!(request.messages[3].content, "Second turn");

        // The current user message appears exactly ONCE (Block B tail), not
        // duplicated as a trailing Block D.
        assert_eq!(request.messages.len(), 4);
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User && m.content == "Second turn")
                .count(),
            1,
            "current user message must appear exactly once (Block B tail, no Block D duplicate)"
        );
    }

    #[test]
    fn test_build_no_duplicate_in_tool_iteration() {
        use acowork_core::providers::traits::{FunctionCall, ToolCall};

        // Tool-loop iterations: history tail is a Tool message; the earlier
        // user turn appears once (in Block B), no tail duplicate.
        let manifest = test_manifest();
        let mut history = HistoryManager::new(10000);
        history.append(ChatMessage::user("Turn"));
        // A REAL tool_call on the assistant turn keeps the tool result
        // from being classified as orphaned by sanitize_messages.
        history.append(ChatMessage::assistant_with_tools(
            "",
            vec![ToolCall {
                id: "toolu_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "test_tool".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        ));
        history.append(ChatMessage::tool("toolu_1", "ok"));

        let builder =
            ContextBuilder::new("Kernel".to_string()).with_override_model("gpt-4".to_string());

        let request = builder.build(&manifest, &history, None, 32_768);
        // [0] A, [1..3] B — no D, no C (ADR-060 v2 + §5.5 erratum).
        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.messages[3].role, MessageRole::Tool);
        assert_eq!(request.messages[3].content, "ok");
        // The user turn appears exactly once (in Block B), no tail duplicate.
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User && m.content == "Turn")
                .count(),
            1
        );
    }

    // ── G9: Abstention guidance injection ──

    #[test]
    fn test_build_injects_abstention_prompt_when_set() {
        // G9: when abstention triggers (empty retrieval + enabled), the
        // prompt is injected into the system prompt (Block A) after the
        // retrieved-memory slot. On the normal path it is absent.
        let manifest = test_manifest();
        let history = HistoryManager::new(10000);

        // Normal path: no abstention prompt set → Block A unchanged.
        let builder =
            ContextBuilder::new("Kernel".to_string()).with_override_model("gpt-4".to_string());
        let request = builder.build(&manifest, &history, None, 32_768);
        assert!(
            !request.messages[0]
                .content
                .contains("Memory Abstention Guidance")
        );

        // Abstention path: prompt set → injected into Block A.
        let mut builder =
            ContextBuilder::new("Kernel".to_string()).with_override_model("gpt-4".to_string());
        builder
            .set_abstention_prompt("When you are not confident, say you're not sure.".to_string());
        let request = builder.build(&manifest, &history, None, 32_768);
        assert!(
            request.messages[0]
                .content
                .contains("## Memory Abstention Guidance"),
            "Block A must contain the abstention guidance section"
        );
        assert!(
            request.messages[0].content.contains("not confident"),
            "Block A must contain the abstention prompt text"
        );

        // clear_abstention_prompt removes it.
        let mut builder =
            ContextBuilder::new("Kernel".to_string()).with_override_model("gpt-4".to_string());
        builder.set_abstention_prompt("prompt".to_string());
        builder.clear_abstention_prompt();
        let request = builder.build(&manifest, &history, None, 32_768);
        assert!(
            !request.messages[0]
                .content
                .contains("Memory Abstention Guidance"),
            "abstention prompt must be cleared by clear_abstention_prompt"
        );
    }

    // ADR-060 v2: Block C removed — todo snapshot byte-stability test no longer
    // applies. Block A byte-stability is already covered by existing tests.

    #[test]
    fn apply_patches_rejects_type_mismatch_and_non_array_tools() {
        use crate::debug::protocol::{PatchError, PatchSet, PatchValue};
        use std::collections::HashMap;

        let mut builder = ContextBuilder::new("base".to_string());

        // JSON patch on a text section → TypeMismatch.
        let err = builder
            .apply_patches(&PatchSet {
                patches: HashMap::from([(
                    "system_prompt".to_string(),
                    PatchValue::Json {
                        value: serde_json::json!({ "not": "text" }),
                    },
                )]),
            })
            .expect_err("json patch for text section must error");
        assert!(matches!(
            err,
            PatchError::TypeMismatch { section, .. } if section == "system_prompt"
        ));

        // Non-array JSON on tool_definitions → TypeMismatch.
        let err = builder
            .apply_patches(&PatchSet {
                patches: HashMap::from([(
                    "tool_definitions".to_string(),
                    PatchValue::Json {
                        value: serde_json::json!({ "not": "an array" }),
                    },
                )]),
            })
            .expect_err("non-array tool_definitions must error");
        assert!(matches!(
            err,
            PatchError::TypeMismatch { section, .. } if section == "tool_definitions"
        ));
    }

    #[test]
    fn apply_patches_empty_environment_falls_back_to_detect() {
        use crate::debug::protocol::{PatchSet, PatchValue};
        use std::collections::HashMap;

        let mut builder = ContextBuilder::new("base".to_string());
        builder.set_environment_override("custom env".to_string());
        assert!(builder.environment_override().is_some());

        builder
            .apply_patches(&PatchSet {
                patches: HashMap::from([(
                    "environment".to_string(),
                    PatchValue::Text {
                        value: String::new(),
                    },
                )]),
            })
            .expect("clear environment must succeed");
        assert!(
            builder.environment_override().is_none(),
            "empty string clears the override — build() falls back to auto-detect"
        );
    }

    /// Helper: build a simple ModelCapabilitiesInfo for testing.
    fn test_caps(context_window: u64, max_output_tokens: u64) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
        }
    }

    #[test]
    fn test_build_context_usage_from_persisted_matches_compute() {
        let caps = test_caps(128_000, 16_384);
        let max_output_limit = 32_768u64;

        // Via compute_context_usage with real UsageInfo
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: 45_000,
            completion_tokens: 1_200,
            total_tokens: 46_200,
            ..Default::default()
        };
        let fresh = compute_context_usage(&caps, &usage, max_output_limit, None);

        // Via build_context_usage_from_persisted with same numbers (no cumulative)
        let persisted = build_context_usage_from_persisted(
            &caps,
            45_000,
            1_200,
            max_output_limit,
            None,
            None,
            None,
            None,
        );

        assert_eq!(fresh.context_window, persisted.context_window);
        assert_eq!(fresh.input_tokens, persisted.input_tokens);
        assert_eq!(fresh.output_tokens, persisted.output_tokens);
        assert_eq!(fresh.total_tokens, persisted.total_tokens);
        assert_eq!(fresh.max_input_tokens, persisted.max_input_tokens);
        assert_eq!(fresh.usable_context, persisted.usable_context);
        assert_eq!(fresh.usage_percent, persisted.usage_percent);
        // Without cumulative tokens supplied, the new fields stay None.
        assert_eq!(persisted.total_input_tokens, None);
        assert_eq!(persisted.total_output_tokens, None);
    }

    #[test]
    fn test_build_context_usage_from_persisted_zero_tokens() {
        // New session with no token data yet → should produce 0 input/output
        let caps = test_caps(200_000, 8_192);
        let info = build_context_usage_from_persisted(&caps, 0, 0, 32_768, None, None, None, None);
        assert_eq!(info.input_tokens, 0);
        assert_eq!(info.output_tokens, 0);
        assert_eq!(info.total_tokens, 0);
        assert_eq!(info.usage_percent, 0);
        assert_eq!(info.total_input_tokens, None);
        assert_eq!(info.total_output_tokens, None);
    }

    #[test]
    fn test_build_context_usage_from_persisted_populates_cumulative_totals() {
        // When SessionTokens is supplied, the resulting ContextUsageInfo must
        // carry the cumulative total_input_tokens / total_output_tokens.
        // This is the path used by session_task.rs on resume to give the
        // frontend both per-turn (last) and cumulative (total) figures.
        use crate::conversation::SessionTokens;

        let caps = test_caps(128_000, 16_384);
        let cumulative = SessionTokens {
            last_input: 45_000,
            last_output: 1_200,
            total_input: 250_000,
            total_output: 7_500,
            // ADR-066: cache fields default to 0 (this test focuses
            // on the in/out cumulative code path).
            ..Default::default()
        };

        let info = build_context_usage_from_persisted(
            &caps,
            cumulative.last_input,
            cumulative.last_output,
            32_768,
            None,
            Some(&cumulative),
            Some(9),
            None,
        );

        // Per-turn fields populated from last_input/last_output scalars.
        assert_eq!(info.input_tokens, 45_000);
        assert_eq!(info.output_tokens, 1_200);
        assert_eq!(info.total_tokens, 46_200);

        // Cumulative fields populated from SessionTokens.total_*.
        assert_eq!(info.total_input_tokens, Some(250_000));
        assert_eq!(info.total_output_tokens, Some(7_500));

        // Cumulative totals must be distinct from per-turn values (the
        // whole point of having both sets of fields).
        assert_ne!(
            info.total_input_tokens.unwrap(),
            info.input_tokens,
            "cumulative total_input_tokens must NOT equal per-turn input_tokens",
        );
        assert!(info.total_input_tokens.unwrap() > info.input_tokens);
        // Persisted per-session iteration count carried through.
        assert_eq!(info.iteration, Some(9));
    }

    #[test]
    fn test_context_usage_with_override_caps_window() {
        // Model has 200K window. User overrides to 100K TOTAL.
        let caps = test_caps(200_000, 16_384);
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: 60_000,
            completion_tokens: 5_000,
            total_tokens: 65_000,
            ..Default::default()
        };
        let info = compute_context_usage(&caps, &usage, 32_768, Some(100_000));
        // Display window stays the user-configured TOTAL (100K).
        assert_eq!(info.context_window, 100_000);
        // Usable input (compaction denominator only) = cap − output reserve
        // inside the cap = 83_616.
        assert_eq!(info.usable_context, 83_616);
        // UI percent = total (input + output) / window TOTAL = 65 000/100 000 = 65.
        // (Decoupled from usable_context — see compute_context_usage doc.)
        assert_eq!(info.usage_percent, 65);
    }

    #[test]
    fn test_context_usage_with_zero_override_uses_model_window() {
        // Defensive path only: the resolution chain never emits Some(0)
        // (ADR-074 §6 — 0 invalid → skip layer), but the low-level function
        // still treats Some(0) like None → model's full window.
        let caps = test_caps(128_000, 8_192);
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: 50_000,
            completion_tokens: 2_000,
            total_tokens: 52_000,
            ..Default::default()
        };
        let info = compute_context_usage(&caps, &usage, 32_768, Some(0));
        assert_eq!(info.context_window, 128_000);
    }

    #[test]
    fn test_context_usage_none_override_uses_model_window() {
        let caps = test_caps(128_000, 8_192);
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: 30_000,
            completion_tokens: 1_000,
            total_tokens: 31_000,
            ..Default::default()
        };
        let info = compute_context_usage(&caps, &usage, 32_768, None);
        assert_eq!(info.context_window, 128_000);
    }

    // ── compute_section_sizes (ADR-067) ────────────────────────────────
    //
    // The input-box context-usage popup aggregates byte sizes from these
    // sections to render the 5-category percentage breakdown (system /
    // tools / messages / connectors / skills).  These tests pin the
    // section ordering and sizes so the always-on observability path
    // matches what `DebugObserverImpl::on_context_built` used to emit
    // when DevMode was enabled.

    fn lookup(sections: &[acowork_core::protocol::ContextUsageSection], key: &str) -> u64 {
        sections
            .iter()
            .find(|s| s.key == key)
            .map(|s| s.size_bytes)
            .unwrap_or_else(|| panic!("section {key} missing; got: {:?}", sections))
    }

    #[test]
    fn compute_section_sizes_emits_mandatory_sections_on_empty_builder() {
        let history = HistoryManager::new(10_000);
        let builder = ContextBuilder::new("Base prompt".to_string());
        let mcp_tools: Vec<serde_json::Value> = vec![];

        let sections = compute_section_sizes(&builder, &history, &mcp_tools, "gpt-4");

        assert_eq!(
            lookup(&sections, "system_prompt"),
            "Base prompt".len() as u64
        );
        assert!(
            sections.iter().any(|s| s.key == "environment"),
            "environment section must always be present"
        );
        assert!(lookup(&sections, "messages") >= 2);
        for optional in [
            "identity_context",
            "workspace_context",
            "retrieved_memory",
            "ambiguous_confirmation_hint",
            "abstention_prompt",
            "skill_instructions",
            "workspace_prompt_file",
            "tool_definitions",
        ] {
            assert!(
                !sections.iter().any(|s| s.key == optional),
                "{optional} must not appear when its field is unset"
            );
        }
    }

    #[test]
    fn compute_section_sizes_separates_builtin_and_mcp_tools() {
        let mut history = HistoryManager::new(10_000);
        history.append(ChatMessage::user("hi"));

        let identity = "Name: Alice";
        let workspace = "Working dir: /tmp";
        let retrieved = "Memory: previous chat about X";
        let skills = "Skill: do_thing — read README first";
        let hint = "Confirm ambiguous file path";
        let abstention = "Refuse if blocked";
        let prompt_file = "# AGENTS.md\nbe terse";

        let builtin_tool = serde_json::json!({
            "type": "function",
            "function": { "name": "read_file" }
        });
        let mcp_tool = serde_json::json!({
            "type": "function",
            "function": { "name": "mcp_github_list_issues" }
        });

        let mut builder = ContextBuilder::new("Base prompt".to_string())
            .with_identity(Some(identity.into()))
            .with_workspace_context(Some(workspace.into()))
            .with_workspace_prompt_file(Some(prompt_file.into()))
            .with_tools(vec![builtin_tool.clone()]);
        builder.set_retrieved_memory_patch(retrieved.into());
        builder.set_skill_summary(skills.into());
        builder.set_ambiguous_confirmation_hint(hint.into());
        builder.set_abstention_prompt(abstention.into());

        let mcp_tools = vec![mcp_tool.clone()];
        let sections = compute_section_sizes(&builder, &history, &mcp_tools, "gpt-4");

        assert_eq!(
            lookup(&sections, "system_prompt"),
            "Base prompt".len() as u64
        );
        assert_eq!(lookup(&sections, "identity_context"), identity.len() as u64);
        assert_eq!(
            lookup(&sections, "workspace_context"),
            workspace.len() as u64
        );
        assert_eq!(
            lookup(&sections, "retrieved_memory"),
            retrieved.len() as u64
        );
        assert_eq!(
            lookup(&sections, "ambiguous_confirmation_hint"),
            hint.len() as u64
        );
        assert_eq!(
            lookup(&sections, "abstention_prompt"),
            abstention.len() as u64
        );
        assert_eq!(lookup(&sections, "skill_instructions"), skills.len() as u64);
        assert_eq!(
            lookup(&sections, "workspace_prompt_file"),
            prompt_file.len() as u64
        );

        assert_eq!(
            lookup(&sections, "tool_definitions"),
            serde_json::to_string(&vec![builtin_tool]).unwrap().len() as u64
        );
        assert_eq!(
            lookup(&sections, "connector_tool_definitions"),
            serde_json::to_string(&vec![mcp_tool]).unwrap().len() as u64
        );

        assert!(lookup(&sections, "messages") > 0);
    }

    #[test]
    fn compute_section_sizes_reports_skill_bodies_separately_from_the_summary() {
        // The attribution gap this section exists to close: a `load_skill`
        // result is a Tool message, so before `skills_loaded` its bytes were
        // billed to *messages* and the popover's *skills* category stayed
        // flat no matter how much SKILL.md the model pulled in.
        use acowork_core::providers::traits::ChatMessage;
        let mut history = HistoryManager::new(10_000);
        history.append(ChatMessage::user("review this PR"));

        let mut load = ChatMessage::tool("tc_1", "{\"instructions\":\"step 1\nstep 2\"}");
        load.name = Some("load_skill".to_string());
        let load_json_len = serde_json::to_string(&load).unwrap().len() as u64;
        history.append(load);

        let mut read = ChatMessage::tool("tc_2", "x".repeat(900));
        read.name = Some("file_read".to_string());
        history.append(read);

        let mut builder = ContextBuilder::new("Kernel".to_string());
        // Progressive summary present at the same time — the two keys must
        // stay separate and both non-zero.
        builder.set_skill_summary("## Available Skills\n- **code-review**: review".to_string());
        let mcp_tools: Vec<serde_json::Value> = vec![];

        let sections = compute_section_sizes(&builder, &history, &mcp_tools, "gpt-4");

        assert_eq!(
            lookup(&sections, "skills_loaded"),
            load_json_len,
            "only the load_skill result counts, not the file_read one"
        );
        assert!(lookup(&sections, "skill_instructions") > 0);
        assert!(
            lookup(&sections, "messages") > load_json_len,
            "the body is still billed to messages too — skills_loaded is an \
             attribution view, not a re-partition"
        );
    }

    #[test]
    fn compute_section_sizes_omits_skills_loaded_when_nothing_was_pulled() {
        // Matches the convention of every other optional section: absent,
        // not zero, so the frontend's skills bucket doesn't grow a phantom
        // sub-row in the common case.
        let mut history = HistoryManager::new(10_000);
        history.append(ChatMessage::user("hi"));
        let builder = ContextBuilder::new("Kernel".to_string());
        let mcp_tools: Vec<serde_json::Value> = vec![];

        let sections = compute_section_sizes(&builder, &history, &mcp_tools, "gpt-4");
        assert!(
            !sections.iter().any(|s| s.key == "skills_loaded"),
            "no load_skill in history must emit no skills_loaded section"
        );
    }

    // ── ADR-094: skills summary vs per-turn skill are separate slots ──
    //
    // They used to share one `ContextBuilder` field, so the per-turn
    // "don't leak a skill across turns" clear also destroyed the
    // Progressive-mode summary seeded at session creation. A turn without
    // a selected skill is the NORMAL case, so the summary vanished on the
    // first message and the LLM never saw the skill list.

    #[test]
    fn per_turn_clear_keeps_agent_skill_summary() {
        let mut builder = ContextBuilder::new("Base prompt".to_string());
        builder.set_skill_summary("## Available Skills\n- **ponytail**: lazy dev".into());

        // A plain chat message (no skill command) clears only the turn slot.
        builder.set_active_skill(None);

        let skills = builder.skill_instructions().expect("summary survives a turn without a skill");
        assert!(skills.contains("ponytail"), "summary text lost: {skills:?}");
    }

    #[test]
    fn per_turn_skill_composes_with_summary_then_leaves_it_intact() {
        let mut builder = ContextBuilder::new("Base prompt".to_string());
        builder.set_skill_summary("SUMMARY".into());
        builder.set_active_skill(Some("SKILL BODY".into()));

        let both = builder.skill_instructions().unwrap();
        assert!(both.contains("SUMMARY"), "summary missing while a skill is active");
        assert!(both.contains("SKILL BODY"), "active skill missing");

        // Next turn selects nothing: the body goes, the summary stays.
        builder.set_active_skill(None);
        assert_eq!(builder.skill_instructions().as_deref(), Some("SUMMARY"));
    }

    #[test]
    fn clearing_summary_leaves_active_skill_and_vice_versa() {
        let mut builder = ContextBuilder::new("Base prompt".to_string());
        builder.set_skill_summary("SUMMARY".into());
        builder.set_active_skill(Some("SKILL BODY".into()));

        // Manual-mode hot toggle must not silently drop a skill the user
        // explicitly picked for this turn.
        builder.clear_skill_summary();
        assert_eq!(builder.skill_instructions().as_deref(), Some("SKILL BODY"));
    }

    #[test]
    fn empty_active_skill_clears_the_turn_slot_only() {
        let mut builder = ContextBuilder::new("Base prompt".to_string());
        builder.set_skill_summary("SUMMARY".into());
        builder.set_active_skill(Some(String::new()));
        assert_eq!(builder.skill_instructions().as_deref(), Some("SUMMARY"));
    }

    #[test]
    fn no_skill_state_emits_no_skill_section() {
        let builder = ContextBuilder::new("Base prompt".to_string());
        assert!(builder.skill_instructions().is_none());
    }

    // ── regression t-83afab47 ─────────────────────────────────────────
    //
    // The live `context_usage` push path has TWO independent builders:
    //
    //   - `compute_context_usage`         (fresh UsageInfo, all cumulative
    //                                       fields left at None)
    //   - `build_context_usage_from_persisted` (resume / session_state,
    //                                       session totals patched via
    //                                       `patch_session_totals`)
    //
    // Every caller must follow up with `patch_agent_totals` so the four
    // `agent_total_*` fields land on the wire. Without that, the
    // frontend's `mergeContextUsage` wipes them on every `session_state`
    // re-emit and the Agent Status panel flickers between the live LLM
    // value and a stale `agentStore` fallback.

    #[test]
    fn patch_agent_totals_fills_all_four_fields_on_fresh_compute() {
        let caps = test_caps(200_000, 16_384);
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: 12_345,
            completion_tokens: 678,
            ..Default::default()
        };
        // `compute_context_usage` is the path the config-change /
        // post-compaction pushers use; it never fills agent_total_*.
        let mut info = compute_context_usage(&caps, &usage, 32_768, None);
        assert_eq!(info.agent_total_input_tokens, None);
        assert_eq!(info.agent_total_output_tokens, None);
        assert_eq!(info.agent_total_cache_read_tokens, None);
        assert_eq!(info.agent_total_cache_write_tokens, None);

        patch_agent_totals(&mut info, (1_000, 200, 300, 50));

        assert_eq!(info.agent_total_input_tokens, Some(1_000));
        assert_eq!(info.agent_total_output_tokens, Some(200));
        assert_eq!(info.agent_total_cache_read_tokens, Some(300));
        assert_eq!(info.agent_total_cache_write_tokens, Some(50));
    }

    #[test]
    fn patch_agent_totals_does_not_touch_session_totals() {
        let caps = test_caps(200_000, 16_384);
        let mut info = build_context_usage_from_persisted(
            &caps,
            10_000,
            500,
            32_768,
            None,
            None,
            None,
            None,
        );
        // Pretend the session has accumulated something via a
        // SessionTokens snapshot.
        let persisted = crate::conversation::SessionTokens {
            last_input: 10_000,
            last_output: 500,
            total_input: 25_000,
            total_output: 2_300,
            last_cache_read: 0,
            last_cache_write: 0,
            total_cache_read: 0,
            total_cache_write: 0,
        };
        patch_session_totals(&mut info, &persisted);
        patch_agent_totals(&mut info, (123_456, 7_890, 11_111, 222));

        // Session totals preserved.
        assert_eq!(info.total_input_tokens, Some(25_000));
        assert_eq!(info.total_output_tokens, Some(2_300));
        assert_eq!(info.total_cache_read_tokens, Some(0));
        assert_eq!(info.total_cache_write_tokens, Some(0));
        // Agent totals filled in.
        assert_eq!(info.agent_total_input_tokens, Some(123_456));
        assert_eq!(info.agent_total_output_tokens, Some(7_890));
        assert_eq!(info.agent_total_cache_read_tokens, Some(11_111));
        assert_eq!(info.agent_total_cache_write_tokens, Some(222));
    }
}

/// Compute the total character count of a ChatRequest for token ratio calibration.
///
/// Counts ALL text that the LLM will receive in the prompt:
/// - Message content + name + tool_call function name/arguments
/// - Tool definitions JSON (see [ADR about including tool_defs])
///
/// This total is used with API `prompt_tokens` to calibrate the chars/token ratio:
/// ```text
/// ratio = count_chat_request_chars(request) / prompt_tokens
/// ```
pub fn count_chat_request_chars(request: &ChatRequest) -> usize {
    // 1. Count message text
    let msg_chars: usize = request
        .messages
        .iter()
        .map(|m| {
            let mut chars = m.content.len();
            if let Some(ref name) = m.name {
                chars += name.len();
            }
            if let Some(ref tool_calls) = m.tool_calls {
                for tc in tool_calls {
                    chars += tc.function.name.len();
                    chars += tc.function.arguments.len();
                }
            }
            chars
        })
        .sum();

    // 2. Count tool definition JSON text
    let tool_chars: usize = request
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0);

    msg_chars + tool_chars
}

// ── compute_section_sizes (ADR-067) ──────────────────────────────────
//
// This helper is the single source of truth for the "section byte sizes"
// payload that drives the input-box context-usage popup (5-category
// percentage breakdown: system / tools / messages / connectors / skills).
//
// ADR-067 separated it from the DevMode-only debug snapshot path so the
// frontend always has a populated breakdown regardless of whether the
// Debug Panel is open. Both consumers — the always-on
// `process_llm_response_usage` push and the DevMode
// `DebugObserverImpl::on_context_built` snapshot — must call
// `compute_section_sizes` rather than rolling their own ordering. The
// helper returns `Vec<ContextUsageSection>` (one entry per populated
// section) — see `acowork_core::protocol::ContextUsageSection` for the
// wire contract (the frontend `contextUsageBreakdown.ts` groups by
// `.key`).
//
// The `messages` byte size is read from `HistoryManager::messages_json_bytes()`
// (O(1) incremental counter) — the always-on path deliberately avoids
// re-serializing the full history on every LLM call.

/// Build the section-byte-sizes list for the current context.
///
/// Returns an empty `Vec` when the builder has no fields set — the
/// frontend treats empty as "no composition data yet" and falls back
/// to the 5 zero-percent rows.
pub fn compute_section_sizes(
    builder: &ContextBuilder,
    history: &HistoryManager,
    mcp_tools: &[serde_json::Value],
    _model: &str,
) -> Vec<acowork_core::protocol::ContextUsageSection> {
    use acowork_core::protocol::ContextUsageSection;

    let mut out: Vec<ContextUsageSection> = Vec::with_capacity(11);

    // 1. Base system prompt — always present (required by `ContextBuilder::new`).
    out.push(ContextUsageSection {
        key: "system_prompt".to_string(),
        size_bytes: builder.system_prompt().len() as u64,
    });

    // 2. Identity context
    if let Some(identity) = builder.identity_context() {
        out.push(ContextUsageSection {
            key: "identity_context".to_string(),
            size_bytes: identity.len() as u64,
        });
    }

    // 2.2 Workspace context
    if let Some(ws) = builder.workspace_context() {
        out.push(ContextUsageSection {
            key: "workspace_context".to_string(),
            size_bytes: ws.len() as u64,
        });
    }

    // 2.5 Retrieved memory
    if let Some(mem) = builder.retrieved_memory() {
        out.push(ContextUsageSection {
            key: "retrieved_memory".to_string(),
            size_bytes: mem.len() as u64,
        });
    }

    // 2.5b Ambiguous-confirmation hint (P3-4)
    if let Some(hint) = builder.ambiguous_confirmation_hint() {
        out.push(ContextUsageSection {
            key: "ambiguous_confirmation_hint".to_string(),
            size_bytes: hint.len() as u64,
        });
    }

    // 2.5c Abstention guidance (G9)
    if let Some(prompt) = builder.abstention_prompt() {
        out.push(ContextUsageSection {
            key: "abstention_prompt".to_string(),
            size_bytes: prompt.len() as u64,
        });
    }

    // 2.6 Skill instructions
    if let Some(skills) = builder.skill_instructions() {
        out.push(ContextUsageSection {
            key: "skill_instructions".to_string(),
            size_bytes: skills.len() as u64,
        });
    }

    // 2.6b Skill bodies pulled in by the `load_skill` tool this session.
    //
    // Separate key rather than folded into `skill_instructions`: the two
    // have different lifecycles (system-prompt section vs. Tool messages in
    // history) and the Debug panel can only render a section whose content
    // it can re-materialize as one string. The frontend groups both keys
    // into the *skills* category (`contextUsageBreakdown.ts`
    // SKILL_SECTION_KEYS), so the user-visible number is the sum.
    //
    // O(1): reads the incremental counter maintained by `HistoryManager`.
    // This function runs on every LLM request, so it MUST NOT scan history
    // — see the accounting contract on
    // [`HistoryManager::skills_loaded_bytes`].
    let skills_loaded = history.skills_loaded_bytes() as u64;
    if skills_loaded > 0 {
        out.push(ContextUsageSection {
            key: "skills_loaded".to_string(),
            size_bytes: skills_loaded,
        });
    }

    // 3. Environment (override wins, else auto-detect). Memoized
    // string in detect_environment_text so the `.len()` is O(1).
    let env_size = builder
        .environment_override()
        .map(|s| s.len() as u64)
        .unwrap_or_else(|| detect_environment_text().len() as u64);
    out.push(ContextUsageSection {
        key: "environment".to_string(),
        size_bytes: env_size,
    });

    // 3.2 Workspace prompt file (CLAUDE.md / AGENTS.md)
    if let Some(prompt_file) = builder.workspace_prompt_file() {
        out.push(ContextUsageSection {
            key: "workspace_prompt_file".to_string(),
            size_bytes: prompt_file.len() as u64,
        });
    }

    // 3.5 Tool definitions (passed separately in ChatRequest.tools).
    // Keep MCP definitions separate so the usage popup can distinguish
    // connector-provided tools from Runtime built-ins.
    if let Some(defs) = builder.tool_definitions().filter(|defs| !defs.is_empty()) {
        let size = serde_json::to_string(defs)
            .map(|s| s.len())
            .unwrap_or(0) as u64;
        out.push(ContextUsageSection {
            key: "tool_definitions".to_string(),
            size_bytes: size,
        });
    }
    if !mcp_tools.is_empty() {
        let size = serde_json::to_string(mcp_tools)
            .map(|s| s.len())
            .unwrap_or(0) as u64;
        out.push(ContextUsageSection {
            key: "connector_tool_definitions".to_string(),
            size_bytes: size,
        });
    }

    // 3.6 Messages — ADR-054 step 4.
    //
    // ADR-067: byte size comes from `HistoryManager::messages_json_bytes()`
    // — an O(1) incrementally-maintained counter — instead of re-serializing
    // the whole history on every LLM call. The value is exactly
    // `serde_json::to_string(&history.messages()).len()`.
    out.push(ContextUsageSection {
        key: "messages".to_string(),
        size_bytes: history.messages_json_bytes() as u64,
    });

    out
}

/// Compute context usage info from model capabilities and API usage response.
///
/// Usable input context is derived from
/// [`ModelCapabilitiesInfo::input_budget_with_cap`]: when a cap is set it is
/// treated as a **total** window and the output reserve is subtracted inside
/// it (250K cap + 32K reserve → 218K input); without a cap the provider's own
/// `max_input_tokens` is authoritative, else `window − reserve`.
///
/// `context_window_cap` is the per-agent context window override (ADR-026):
/// - `None` — not set, use model's full `context_window`.
/// - `Some(0)` — defensive only (ADR-074 §6 abolished the "0 = no limit"
///   sentinel; the resolution chain never emits it) — treated like `None`.
/// - `Some(n)` where `n > 0` — cap the effective window at `min(n, model_window)`.
///
/// `context_window` in the output reflects the effective (capped) window for
/// display and stays the **user-configured total** (e.g. 250K).
/// `usable_context` is the runtime input budget (218K) used by compaction
/// thresholds — NOT the UI denominator.
///
/// The UI-facing `usage_percent` is deliberately decoupled from the
/// compaction threshold (2026-09-06 review): the frontend reads it as
/// "used / total window" (including input AND output of the reported turn),
/// while automatic compaction fires on PROJECTED input vs `usable_context`.
/// So the denominator here is `context_window` (total), not the input
/// budget — the two never move together.
pub fn compute_context_usage(
    caps: &ModelCapabilitiesInfo,
    usage: &acowork_core::providers::traits::UsageInfo,
    max_output_tokens_limit: u64,
    context_window_cap: Option<u64>,
) -> acowork_core::protocol::ContextUsageInfo {
    let model_window = caps.context_window;
    let effective_window = match context_window_cap {
        Some(0) | None => model_window,
        Some(cap) => cap.min(model_window),
    };
    let effective_usable = caps.input_budget_with_cap(context_window_cap, max_output_tokens_limit);
    let total = usage.prompt_tokens + usage.completion_tokens;
    // UI percent = total (input + output) / window total. The compaction
    // line lives on the PROJECTED input vs usable_context and is NOT the
    // number displayed here.
    let percent = if effective_window > 0 {
        ((total as f64 / effective_window as f64) * 100.0).min(100.0) as u8
    } else {
        0
    };
    acowork_core::protocol::ContextUsageInfo {
        context_window: effective_window,
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
        total_tokens: total,
        max_input_tokens: caps.max_input_tokens,
        usable_context: effective_usable,
        usage_percent: percent,
        // compute_context_usage only sees per-turn UsageInfo. Cumulative
        // session totals (total_input_tokens / total_output_tokens) must
        // be patched by the caller after consulting SessionTokens. They
        // default to None here so per-turn callers (e.g. loop_context.rs
        // fallback path) don't accidentally surface a stale cumulative
        // figure from a previous round.
        total_input_tokens: None,
        total_output_tokens: None,
        // ADR-028: agent-scoped cumulative tokens (patched by caller).
        agent_total_input_tokens: None,
        agent_total_output_tokens: None,
        // ADR-066: per-turn cache breakdown — set here so callers
        // computing from a fresh `UsageInfo` see the right numbers.
        // Cumulative session / agent cache totals remain `None`
        // (patched by the caller, mirroring the in/out policy above).
        cache_read_tokens: Some(usage.cache_read_tokens),
        cache_write_tokens: Some(usage.cache_write_tokens),
        total_cache_read_tokens: None,
        total_cache_write_tokens: None,
        agent_total_cache_read_tokens: None,
        agent_total_cache_write_tokens: None,
        // ADR-067: see compute_section_sizes; populated by callers with a ContextBuilder
        sections: None,
        // LLM-call iteration count is patched by the caller
        // (process_llm_response_usage) before push, so this builder
        // doesn't need to know the counter.
        iteration: None,
        // Session speed / latency averages are session-level, never
        // derivable from a single `UsageInfo` — same caller-patch
        // policy as the cumulative counters above.
        avg_tps: None,
        avg_ttft_ms: None,
    }
}

/// Build a [`ContextUsageInfo`] from persisted last-token counts.
///
/// This produces the same structure as [`compute_context_usage`] but from
/// the raw `last_input_tokens` / `last_output_tokens` values stored in
/// JSONL metadata.  Per-turn cache/reasoning breakdown fields are sourced
/// from the `last_*` halves of `SessionTokens` (the last LLM call's
/// provider-reported figures), and cumulative fields from the `total_*`
/// halves — see [`patch_session_totals`] for the centralised patcher.
///
/// Callers must supply the *current* [`ModelCapabilitiesInfo`] because
/// window-derived fields (`context_window`, `usable_context`, `usage_percent`)
/// are model-dependent and become stale if the user switched models between
/// sessions.
///
/// `context_window_cap` mirrors the same parameter on [`compute_context_usage`].
///
/// `cumulative_tokens` optionally carries the full [`crate::conversation::SessionTokens`]
/// so the cumulative `total_input_tokens` / `total_output_tokens` /
/// `total_cache_read_tokens` / `total_cache_write_tokens` fields can be
/// populated on the resulting [`ContextUsageInfo`]. Pass `None` when only the
/// last-turn snapshot is available (e.g. unit tests exercising the legacy
/// scalar path).
///
/// `perf` does the same for `avg_tps` / `avg_ttft_ms` — the session speed
/// and first-token-latency means. Project convention for a pure builder
/// whose parameter list mirrors the persisted meta's field groups.
#[allow(clippy::too_many_arguments)]
pub fn build_context_usage_from_persisted(
    caps: &ModelCapabilitiesInfo,
    last_input_tokens: u64,
    last_output_tokens: u64,
    max_output_tokens_limit: u64,
    context_window_cap: Option<u64>,
    cumulative_tokens: Option<&crate::conversation::SessionTokens>,
    llm_call_counter: Option<u32>,
    perf: Option<&crate::conversation::SessionPerf>,
) -> acowork_core::protocol::ContextUsageInfo {
    let mut info = {
        let usage = acowork_core::providers::traits::UsageInfo {
            prompt_tokens: last_input_tokens,
            completion_tokens: last_output_tokens,
            total_tokens: last_input_tokens + last_output_tokens,
            ..Default::default()
        };
        compute_context_usage(caps, &usage, max_output_tokens_limit, context_window_cap)
    };
    if let Some(t) = cumulative_tokens {
        // ADR-066: patch from the persisted SessionTokens snapshot —
        // both per-turn (last) and cumulative (total) views.  Per-turn
        // cache is included even though the resume path doesn't have
        // a fresh UsageInfo, because the last LLM call's cache figures
        // were already recorded into `last_cache_read` /
        // `last_cache_write` and are a meaningful per-turn indicator
        // after a restart.
        info.cache_read_tokens = Some(t.last_cache_read);
        info.cache_write_tokens = Some(t.last_cache_write);
        patch_session_totals(&mut info, t);
    }
    // Per-session lifetime LLM-call count ("Iterations" display),
    // persisted in `SessionMeta.llm_call_counter`. Mirrors the live
    // `ContextUsage` push so the retained `session_state` snapshot and
    // the `fetchSessionState` pull carry the same count as the push.
    info.iteration = llm_call_counter;
    // Same for the session speed / latency averages — persisted in
    // `SessionMeta.perf` so a resumed session shows the figures its live
    // panel had.
    patch_perf(&mut info, perf);
    info
}

/// Orchestrate the full "persisted SessionTokens → wire-ready
/// ContextUsageInfo" build for the three non-LLM push sites that
/// used to duplicate the same 12-line template by hand:
///
///   - `SessionManager::build_initial_session_state` (writes the
///     snapshot only — caller serialises to JSON for the retained
///     session_state payload)
///   - `SessionManager::usage_recompute` (writes the snapshot AND
///     broadcasts a `ChunkEvent::ContextUsage`)
///   - `SessionTask` resume-context initial push (broadcasts a
///     `ChunkEvent::ContextUsage` so the resumed agent lands in the
///     UI with non-empty counters before the first LLM call)
///
/// All three previously called, in order:
///   1. `AgentCore::get_model_capabilities(model_name)`
///   2. `AgentCore::max_output_tokens_limit_for_model(model_name)`
///   3. `session_config::resolve_effective_context_window(...)`
///   4. `build_context_usage_from_persisted(...)`
///   5. `patch_agent_totals(&mut ctx, core.agent_token_totals())`
///
/// with `model_name = conv.model().unwrap_or("unknown")` and the
/// `last_input` / `last_output` straight from `persisted`. Steps 1–3
/// are pure caps resolution; steps 4–5 already have their own helpers,
/// so this function is the single entry point that ties them together.
///
/// Returns `None` when:
///   - the conversation has no persisted `SessionTokens` yet
///     (`conv.tokens()` is None — fresh session pre-first-LLM)
///   - the model's capabilities are not registered (legacy Runtime
///     without models.dev data)
///
/// Both are legitimate "skip the push" cases — the next live
/// `ChunkEvent::ContextUsage` will fill the gap once the first LLM
/// call completes, so callers MUST treat `None` as "no-op" rather
/// than as an error.
///
/// `emit_session_state` (the status-transition path) deliberately does
/// NOT use this helper: it needs the `live_tokens` override (uses
/// `history.token_count()` over `persisted.last_input` to avoid showing
/// a stale PRE-restart value after cold-start resume, incident
/// 2026-09-06) and the cached `sections` merge for the input-box
/// breakdown popover. Folding them in here would force 3 `Option` flags
/// onto the signature for one caller — net negative readability.
pub(crate) fn build_persisted_ctx_usage(
    core: &crate::agent::agent_core::AgentCore,
    conv: &crate::conversation::ConversationSession,
) -> Option<acowork_core::protocol::ContextUsageInfo> {
    let persisted = conv.tokens()?;
    let model_name = conv.model().unwrap_or_else(|| "unknown".to_string());
    let caps = core.get_model_capabilities(&model_name)?;
    let max_output = core.max_output_tokens_limit_for_model(&model_name);
    let resolved = crate::agent::session_config::resolve_effective_context_window(
        conv.context_window(),
        core.context_window_override,
        core.manifest_context_window,
        Some(&caps),
    );
    let mut ctx = build_context_usage_from_persisted(
        &caps,
        persisted.last_input,
        persisted.last_output,
        max_output,
        Some(resolved),
        Some(&persisted),
        conv.llm_call_counter(),
        conv.perf().as_ref(),
    );
    patch_agent_totals(&mut ctx, core.agent_token_totals());
    Some(ctx)
}

/// Patch session-level cumulative fields on a [`ContextUsageInfo`].
///
/// ADR-066 §2 commits to a strict split between per-turn (filled by
/// [`compute_context_usage`] from a fresh `UsageInfo`) and cumulative
/// (filled here from a [`SessionTokens`] snapshot).  All three call
/// sites that build a `ContextUsageInfo` for push delivery now go
/// through this helper, which guarantees:
///
///   - `total_input_tokens` / `total_output_tokens` come from
///     `tokens.total_input` / `tokens.total_output` (saturating sums).
///   - `total_cache_read_tokens` / `total_cache_write_tokens` come
///     from the matching cumulative cache fields.
///
/// Centralising the patch prevents the original ADR-066 P0 bug
/// (`total_cache_read_tokens` left at `None` because the caller
/// forgot to copy it from `SessionTokens` after `accumulate_llm_usage`)
/// from recurring in any of the three push sites in `loop_context.rs`.
pub fn patch_session_totals(
    info: &mut acowork_core::protocol::ContextUsageInfo,
    tokens: &crate::conversation::SessionTokens,
) {
    info.total_input_tokens = Some(tokens.total_input);
    info.total_output_tokens = Some(tokens.total_output);
    info.total_cache_read_tokens = Some(tokens.total_cache_read);
    info.total_cache_write_tokens = Some(tokens.total_cache_write);
}

/// Patch the session-level speed / latency averages onto a
/// [`ContextUsageInfo`].
///
/// Same centralised-patcher policy as [`patch_session_totals`]: the five
/// `ContextUsageInfo` push sites (one per LLM response, plus the
/// resume / compaction / config-change pushes) must not each decide how to
/// source the averages, or a resumed session starts showing "—" for a
/// session whose live panel had a number.
pub fn patch_perf(
    info: &mut acowork_core::protocol::ContextUsageInfo,
    perf: Option<&crate::conversation::SessionPerf>,
) {
    let Some(perf) = perf else {
        return;
    };
    info.avg_tps = perf.avg_tps();
    info.avg_ttft_ms = perf.avg_ttft_ms();
}

/// Patch agent-scoped cumulative fields on a [`ContextUsageInfo`].
///
/// Mirror of [`patch_session_totals`] for the ADR-028 / ADR-066 agent
/// counters that live on `AgentCore` (AtomicU64), not on the per-session
/// `SessionTokens`. Every call site that builds a `ContextUsageInfo`
/// from a non-LLM path (`emit_session_state`, resume snapshot, initial
/// session-state push, etc.) MUST invoke this after constructing the
/// per-turn + session-cumulative figures — otherwise the resulting
/// push drops the four `agent_total_*` fields, the frontend's
/// `mergeContextUsage` wipes them to `None`, and the Agent Status
/// panel's "累计输入 Token" row flickers between the live value
/// (from the LLM-call `ContextUsage` push) and the stale fallback
/// (`agentStore.agents[id].agentTokenTotals`, refreshed only on
/// session-list fetch) every time `session_state` is re-emitted.
///
/// The four-tuple mirrors `AgentCore::agent_token_totals()`:
/// `(input, output, cache_read, cache_write)`.
pub fn patch_agent_totals(
    info: &mut acowork_core::protocol::ContextUsageInfo,
    agent_totals: (u64, u64, u64, u64),
) {
    info.agent_total_input_tokens = Some(agent_totals.0);
    info.agent_total_output_tokens = Some(agent_totals.1);
    info.agent_total_cache_read_tokens = Some(agent_totals.2);
    info.agent_total_cache_write_tokens = Some(agent_totals.3);
}

