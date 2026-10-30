//! LLM-side effects of config changes (ADR-047 §3.3.2).
//!
//! Called by SessionTask at turn boundaries when the config version has
//! changed. This is the ONLY place that handles LLM-side reactions to
//! config changes.
//!
//! Design: compares the current snapshot with the previous one to
//! determine which fields changed, then applies only the necessary
//! LLM-side effects. This avoids unnecessary provider rebuilds when
//! only unrelated fields (e.g. title) changed.

use acowork_core::providers::traits::ReasoningEffort;

use crate::agent::context::ContextBuilder;
use crate::agent::loop_::AgentLoop;
use crate::agent::session_config::SessionConfigSnapshot;

/// Resolve the effective `reasoning_effort` for a session.
///
/// This is the **single source of truth** for the three-level priority
/// chain that decides what `reasoning_effort` value a session should
/// have. Previously every call site (session resume, model switch
/// `apply_llm_effects`, HTTP `GET /sessions/{sid}/config`, MQTT
/// `session_config` retained) reimplemented its own
/// `persisted → provider_default → supports_reasoning → None` chain,
/// which caused two user-visible bugs:
///
/// 1. Old sessions whose `meta.json` never persisted `reasoning_effort`
///    (e.g. the user never toggled it, or the model didn't support
///    reasoning at the time the session was created) would always
///    report `reasoning_effort: null` on HTTP GET and MQTT retained,
///    so the Desktop hid the reasoning-effort toggle button until the
///    user explicitly switched model once.
/// 2. Drift between the in-memory `ConversationSession` snapshot
///    (used by turn-boundary diff detection) and the externally
///    observable snapshot (HTTP/MQTT) created confusing "the
///    button appeared for one render, then disappeared" flicker.
///
/// Both bugs are fixed by funnelling all read and write paths through
/// this function.
///
/// # Priority chain
///
///  1. `persisted` — explicit per-session value (user-set, or
///     initialized from provider capabilities during
///     `create_or_resume_session` / `apply_llm_effects`).
///  2. `caps.default_reasoning_effort` — the provider's recommended
///     default for this model (e.g. `"medium"` for some Anthropic models).
///  3. `Auto` — the model is reasoning-capable per `supports_reasoning`
///     but has no explicit default. Ensures the UI shows the toggle.
///  4. `None` — model doesn't support thinking control. The UI hides the
///     toggle button.
///
/// # Why `Option<&str>` and `Option<&ModelCapabilitiesInfo>`?
///
/// Pure function — no interior mutability, no locks. Both call sites
/// (HTTP/MQTT read, in-memory init) compute `caps` from `AgentCore`
/// and `persisted` from `ConversationSession` separately, then ask this
/// function for the merged answer. This keeps `ConversationSession`
/// unaware of the capabilities registry and avoids cross-module
/// dependency cycles.
pub fn resolve_effective_reasoning_effort(
    caps: Option<&acowork_core::ModelCapabilitiesInfo>,
    persisted: Option<&str>,
) -> Option<ReasoningEffort> {
    // Level 1: persisted wins unconditionally. Even if the model later
    // drops support for reasoning, a user who explicitly chose an
    // effort should keep it (and the LLM call will then naturally fail
    // with a provider-side error if the model really can't honor it —
    // which is the right place for that error to surface).
    if let Some(persisted_str) = persisted
        && let Some(parsed) = ReasoningEffort::from_str_loose(persisted_str)
    {
        return Some(parsed);
    }

    // Level 2: provider-recommended default.
    if let Some(caps) = caps
        && let Some(default_str) = caps.default_reasoning_effort.as_deref()
        && let Some(parsed) = ReasoningEffort::from_str_loose(default_str)
    {
        return Some(parsed);
    }

    // Level 3: reasoning-capable model with no explicit default → Auto.
    // Without this fallback, every model that supports reasoning but
    // has no `default_reasoning_effort` configured would render the
    // toggle button hidden until the user switched models. The Rust
    // backend has always applied this fallback at session init time
    // (see `session_manager::create_or_resume_session`), but the
    // frontend never knew about it because HTTP GET read the raw
    // persisted value. Centralising the chain here means HTTP/MQTT
    // reads see the same effective value that the in-memory session
    // already has.
    if caps.and_then(|c| c.supports_reasoning).unwrap_or(false) {
        return Some(ReasoningEffort::Auto);
    }

    None
}

/// Apply LLM-side effects of a config change.
///
/// Called by SessionTask at turn boundaries when config version has changed.
/// This is the ONLY place that handles LLM-side reactions to config changes.
///
/// # Arguments
/// * `agent_loop` - The per-session AgentLoop (mutable, owns SessionState + AgentCore)
/// * `context_builder` - The per-session ContextBuilder (mutable)
/// * `snapshot` - Current config snapshot (post-change)
/// * `prev` - Previous config snapshot (pre-change, for diffing)
pub fn apply_llm_effects(
    agent_loop: &mut AgentLoop,
    context_builder: &mut ContextBuilder,
    snapshot: &SessionConfigSnapshot,
    prev: &SessionConfigSnapshot,
) {
    let model_changed = snapshot.model != prev.model;
    let provider_changed = snapshot.provider != prev.provider;
    let account_changed = snapshot.account_id != prev.account_id;

    // ── Model/Provider/Account change -> rebuild LLM Provider ──────
    if (model_changed || provider_changed || account_changed)
        && let Some(ref model) = snapshot.model
    {
        tracing::info!(
            model = %model,
            provider = ?snapshot.provider,
            "apply_llm_effects: model/provider changed, rebuilding LLM provider"
        );

        // Update in-memory SessionState
        agent_loop.session.set_model(model.clone());
        if let Some(ref provider_id) = snapshot.provider {
            agent_loop.session.set_provider(provider_id.clone());
            if let Some(ref account_id) = snapshot.account_id {
                agent_loop.session.set_account_id(account_id.clone());
            }

            // Rebuild the LLM Provider instance from the shared global cache.
            if let Some(new_provider) = agent_loop.session_core.build_provider_for(
                provider_id,
                Some(model.as_str()),
                snapshot.account_id.as_deref(),
                &crate::agent::session_core::ProviderBuildContext {
                    config: &agent_loop.core.config,
                    global_provider_list: &agent_loop.core.global_provider_list,
                    provider_key_vault: &agent_loop.core.provider_key_vault,
                    compat_cache: agent_loop.core.compat_cache.as_ref(),
                    responses_compat_cache: agent_loop.core.responses_compat_cache.as_ref(),
                },
            ) {
                agent_loop.update_provider(new_provider, model.clone(), Some(provider_id.clone()));
            } else {
                tracing::warn!(
                    provider_id = %provider_id,
                    "apply_llm_effects: provider not found in global cache, keeping current Provider instance"
                );
            }
        }

        // Update context builder for next iteration
        context_builder.set_override_model(model.clone());

        // Model switch: sync runtime LLM reasoning_effort state.
        //
        // The persisted `reasoning_effort` on `ConversationSession`
        // is already correct - it was resolved and set atomically by
        // `route_model_switch` via `set_reasoning_effort_raw` before
        // `apply_config` called `notify_config_change`.  Here we only
        // sync the runtime `SessionState` (used by the LLM call layer)
        // to match the already-correct persisted value.
        let runtime_effort = snapshot
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::from_str_loose);
        agent_loop.session.set_reasoning_effort(runtime_effort);
    }

    // ── ReasoningEffort change (without model switch) ──────────────
    if !model_changed
        && snapshot.reasoning_effort != prev.reasoning_effort
        && let Some(ref effort) = snapshot.reasoning_effort
    {
        let parsed = ReasoningEffort::from_str_loose(effort);
        tracing::info!(
            effort = %effort,
            parsed = ?parsed,
            "apply_llm_effects: reasoning_effort changed (no model switch)"
        );
        agent_loop.session.set_reasoning_effort(parsed);
    }

    // ── ContextWindow override change (ADR-074 §5.2) ───────────────
    //
    // The budget itself is resolved fresh on every build (never cached,
    // §3.2), so trim / compaction thresholds converge automatically on
    // the next build. What happens here is only the UI surface: re-emit
    // the session state so the context-usage total reflects the new
    // session-effective window right at the turn boundary (a running
    // loop has no idle `usage_recompute` callback to do this for it).
    if snapshot.context_window != prev.context_window {
        tracing::info!(
            old = ?prev.context_window,
            new = ?snapshot.context_window,
            "apply_llm_effects: context_window override changed, refreshing context usage"
        );
        agent_loop.emit_session_state();
    }
}
