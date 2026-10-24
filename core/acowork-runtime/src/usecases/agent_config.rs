//! Per-agent runtime configuration use case.
//!
//! ADR-040 follow-up: the `PUT /agents/{id}/config` HTTP handler used to
//! call `agent_config::*` directly and contained the entire
//! load-merge-save cycle, the field-dispatch loop, the
//! `RuntimeConfigOverrides` projection, and the per-field type
//! validation inline. This module extracts that work behind
//! [`AgentConfigService`] so the HTTP handler becomes a thin protocol
//! converter — consistent with [`crate::usecases::AgentToolsService`],
//! [`crate::usecases::WorkspaceMutationService`], and the rest of the
//! layer.
//!
//! ## Scope
//!
//! The trait covers **only the persistence of `agent_config.json`** —
//! the per-agent **runtime** config (temperature, context_window,
//! max_iterations, shell_approval_threshold, approval_timeout_secs,
//! max_output_tokens, max_sessions). Notably out of scope:
//!
//! - `PUT /agents/{id}/builtin-tools` — lives behind
//!   [`crate::usecases::AgentToolsService`] because `agent_tools.json`
//!   is a separate file with a different read-modify-write contract.
//! - MQTT `RuntimeConfigUpdate` — the parallel write path from the
//!   Gateway through `cli.rs`. It uses the same persistence functions
//!   but bypasses the HTTP layer entirely; it can be migrated to call
//!   this trait in a follow-up commit if we want a single audit point.
//!
//! ## Patches (triple-state wire semantics)
//!
//! The HTTP wire shape carries three states per field:
//!
//! | JSON token                      | Outer `Option` | Inner semantics         |
//! |---------------------------------|----------------|-------------------------|
//! | field absent                    | `None`         | leave on-disk alone     |
//! | `{"x": null}`                   | `Some(...)`    | explicit clear          |
//! | `{"x": 42}` / `{"x": "auto"}`   | `Some(...)`    | overwrite with `42`     |
//!
//! The wire layer collapses `null` and absent into the same `None`, so
//! we use [`FieldPatch`] (the `Set(T) | Clear` two-state enum) inside
//! the patch list and let the HTTP handler translate `null`-tokens to
//! `FieldPatch::Clear` at the protocol boundary. Type validation
//! happens in the impl (via the standard `serde_json::from_value`
//! round-trip) so wrong-typed values produce a `tracing::warn!` and
//! are dropped — matching the pre-refactor handler behaviour.
//!
//! ## Live-broadcast side effects
//!
//! [`AgentConfigService::put_config`] returns the new on-disk
//! [`AgentConfig`] **plus** a [`RuntimeConfigOverrides`] projection
//! for the HTTP handler to broadcast to active sessions. The MQTT
//! re-PUBLISH of the retained `acowork/agents/{id}/config` snapshot
//! also stays in the handler (it uses the [`crate::http::server::HttpState`]
//! MQTT slot, which is a protocol-layer concern). The trait only owns
//! persistence + projection; the handler owns the wire + broadcast.
//!
//! ## Late-bind wiring
//!
//! The implementation [`crate::usecases::RuntimeAgentConfigService`]
//! holds the `work_dir` resolved at boot (no async resource
//! dependencies), so it can be constructed immediately after the
//! workspace services in `session_init.rs` Phase B.

use async_trait::async_trait;
use serde::Serialize;

use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent_config::AgentConfig;

// ── Field-patch primitives ─────────────────────────────────────────────

/// A single per-field patch in a `PUT /agents/{id}/config` request.
///
/// Three-state wire semantics are encoded via [`FieldPatch`]:
///
/// - `FieldPatch::Set(value)` — overwrite the on-disk value with `value`
///   (after type-checking `value` against the field's `AgentConfig` type).
/// - `FieldPatch::Clear` — explicitly clear (JSON `null` on the wire).
///
/// The HTTP handler additionally tracks **field absence** as the
/// outer `Option` in [`PutAgentConfigBody::patches`]: a patch that is
/// simply not in the list means "leave on-disk alone".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFieldPatch {
    pub field: ConfigField,
    pub op: FieldPatch<serde_json::Value>,
}

/// Enumeration of every per-agent config field the Setup panel
/// exposes today. Adding a new field is a two-edit change: this enum
/// plus the dispatch loop in
/// [`crate::usecases::agent_config_impl::RuntimeAgentConfigService::put_config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigField {
    /// `AgentConfig::max_output_tokens` — `Option<u64>`.
    MaxOutputTokens,
    /// `AgentConfig::max_iterations` — `Option<u32>`.
    MaxIterations,
    /// `AgentConfig::max_sessions` — `Option<usize>`. The wire value
    /// is `u64` (JSON numbers don't carry size hints); the impl
    /// narrows via `usize::try_from`.
    MaxSessions,
    /// `AgentConfig::temperature` — `Option<f32>`.
    Temperature,
    /// `AgentConfig::context_window` — `Option<u64>`.
    ContextWindow,
    /// `AgentConfig::shell_approval_threshold` — `Option<String>`
    /// (`"low" | "medium" | "high" | "auto_approve"`; legacy `"never"` accepted).
    ShellApprovalThreshold,
    /// `AgentConfig::approval_timeout_secs` — `Option<u64>`.
    ApprovalTimeoutSecs,
    /// `AgentConfig::compression_ratio_threshold` — `Option<f64>`.
    /// ADR-061 compression ratio bar for levels 1-7 (0.90 default =
    /// "compress until at most 10% remains").
    CompressionRatioThreshold,
    // ADR-100: the distiller / forgetting knobs (`distiller_enabled`,
    // `distiller_models`, `distiller_interval_minutes`,
    // `distiller_accumulation_threshold`, `distiller_idle_minutes`,
    // `memory_forgetting_*`) moved into the memory service and are gone from
    // `AgentConfig` entirely — there is neither a read nor a write path. An
    // older client PUT carrying them is dropped by serde as an unknown field.
    /// `AgentConfig::session_language` — `Option<String>`.
    /// Per-agent override for the LLM session language hint baked into
    /// the identity-context system prompt (replaces
    /// `UserProfile.language`). `None` = follow the user's global
    /// language from `UserProfile`.
    SessionLanguage,
    /// `AgentConfig::intent_idle_rotate_hours` — `Option<u64>`
    /// (design 27 §4.1 T1 threshold, 1–72 hours, 0 disables).
    IntentIdleRotateHours,
    /// `AgentConfig::intent_handoff_max_tokens` — `Option<u64>`
    /// (design 27 §4.1 T2 handoff cap, 500–4,000 tokens).
    IntentHandoffMaxTokens,
    /// `AgentConfig::chat_router.enabled` — `Option<bool>` (ADR-099).
    /// `None` = enabled (the design default).
    ChatRouterEnabled,
    /// `AgentConfig::chat_router.model` — `Option<String>`. An empty
    /// string from the UI means "clear": fall back to the agent's
    /// compact-model chain.
    ChatRouterModel,
    /// `AgentConfig::chat_router.provider` — `Option<String>`. Empty
    /// string clears (whichever provider serves the model).
    ChatRouterProvider,
    /// `AgentConfig::chat_router.timeout_ms` — `Option<u64>`
    /// (clamped to 1,000–60,000 by [`ChatRouterConfig::timeout_ms`]).
    ChatRouterTimeoutMs,
    // ADR-094 note: `ConfigField::SkillsAutoLoad` was removed.
    // Skill mode (Progressive / Manual) is now driven entirely by
    // `agent_tools.json::load_skill.enabled` — see
    // `cli::resolve_skill_mode` for the priority chain. The user-facing
    // UI surface (the `load_skill` row in the built-in tools list)
    // already updates `agent_tools.json`, so no second field on
    // `AgentConfig` is needed.
}

impl ConfigField {
    /// Stable string key for logging and error messages. **Not** used
    /// on the wire (the wire is JSON with snake_case field names;
    /// see [`PutAgentConfigBody::from_request_fields`]).
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigField::MaxOutputTokens => "max_output_tokens",
            ConfigField::MaxIterations => "max_iterations",
            ConfigField::MaxSessions => "max_sessions",
            ConfigField::Temperature => "temperature",
            ConfigField::ContextWindow => "context_window",
            ConfigField::ShellApprovalThreshold => "shell_approval_threshold",
            ConfigField::ApprovalTimeoutSecs => "approval_timeout_secs",
            ConfigField::CompressionRatioThreshold => "compression_ratio_threshold",
            ConfigField::SessionLanguage => "session_language",
            ConfigField::IntentIdleRotateHours => "intent_idle_rotate_hours",
            ConfigField::IntentHandoffMaxTokens => "intent_handoff_max_tokens",
            ConfigField::ChatRouterEnabled => "chat_router_enabled",
            ConfigField::ChatRouterModel => "chat_router_model",
            ConfigField::ChatRouterProvider => "chat_router_provider",
            ConfigField::ChatRouterTimeoutMs => "chat_router_timeout_ms",
        }
    }
}

/// Two-state patch payload — the inner `T` is `serde_json::Value` in
/// the public trait body so the HTTP handler can carry its raw JSON
/// across the boundary without an extra round-trip through typed
/// fields. The impl applies `serde_json::from_value::<T>()` per field
/// (with `tracing::warn!` on type mismatch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldPatch<T> {
    /// Overwrite with `T`.
    Set(T),
    /// Explicit clear (JSON `null` on the wire).
    Clear,
}

impl<T> FieldPatch<T> {
    pub fn as_ref(&self) -> FieldPatch<&T> {
        match self {
            FieldPatch::Set(v) => FieldPatch::Set(v),
            FieldPatch::Clear => FieldPatch::Clear,
        }
    }
}

// ── Request / Response DTOs ─────────────────────────────────────────────

/// Request body for `PUT /agents/{id}/config` (use-case view).
///
/// A patch list rather than a struct-with-many-fields so the impl can
/// iterate uniformly. The HTTP handler converts its wire shape
/// (snake_case JSON object with optional fields) into this list via
/// [`PutAgentConfigBody::from_request_fields`].
#[derive(Debug, Clone, Default)]
pub struct PutAgentConfigBody {
    /// Per-field patches. Empty list means "no changes requested" —
    /// the impl still re-saves the (unchanged) config and returns the
    /// current on-disk state, so the caller can still observe the
    /// retained snapshot / broadcast.
    pub patches: Vec<ConfigFieldPatch>,
}

impl PutAgentConfigBody {
    /// Construct a body from the wire-shape fields.
    ///
    /// Each parameter is `Option<serde_json::Value>`:
    ///   - `None` → field absent on the wire (skip)
    ///   - `Some(Value::Null)` → `FieldPatch::Clear`
    ///   - `Some(v)` → `FieldPatch::Set(v)`
    ///
    /// `from_request_fields` exists so the HTTP handler can pass its
    /// raw wire fields directly without an intermediate struct —
    /// see the call site in `http/server.rs::put_agent_config`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_request_fields(
        max_output_tokens: Option<serde_json::Value>,
        max_iterations: Option<serde_json::Value>,
        max_sessions: Option<serde_json::Value>,
        temperature: Option<serde_json::Value>,
        context_window: Option<serde_json::Value>,
        shell_approval_threshold: Option<serde_json::Value>,
        approval_timeout_secs: Option<serde_json::Value>,
        compression_ratio_threshold: Option<serde_json::Value>,
        session_language: Option<serde_json::Value>,
        intent_idle_rotate_hours: Option<serde_json::Value>,
        intent_handoff_max_tokens: Option<serde_json::Value>,
        chat_router_enabled: Option<serde_json::Value>,
        chat_router_model: Option<serde_json::Value>,
        chat_router_provider: Option<serde_json::Value>,
        chat_router_timeout_ms: Option<serde_json::Value>,
    ) -> Self {
        let mut patches = Vec::new();
        if let Some(v) = max_output_tokens {
            patches.push(ConfigFieldPatch {
                field: ConfigField::MaxOutputTokens,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = max_iterations {
            patches.push(ConfigFieldPatch {
                field: ConfigField::MaxIterations,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = max_sessions {
            patches.push(ConfigFieldPatch {
                field: ConfigField::MaxSessions,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = temperature {
            patches.push(ConfigFieldPatch {
                field: ConfigField::Temperature,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = context_window {
            patches.push(ConfigFieldPatch {
                field: ConfigField::ContextWindow,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = shell_approval_threshold {
            patches.push(ConfigFieldPatch {
                field: ConfigField::ShellApprovalThreshold,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = approval_timeout_secs {
            patches.push(ConfigFieldPatch {
                field: ConfigField::ApprovalTimeoutSecs,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = compression_ratio_threshold {
            patches.push(ConfigFieldPatch {
                field: ConfigField::CompressionRatioThreshold,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = session_language {
            patches.push(ConfigFieldPatch {
                field: ConfigField::SessionLanguage,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = intent_idle_rotate_hours {
            patches.push(ConfigFieldPatch {
                field: ConfigField::IntentIdleRotateHours,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = intent_handoff_max_tokens {
            patches.push(ConfigFieldPatch {
                field: ConfigField::IntentHandoffMaxTokens,
                op: value_to_patch(&v),
            });
        }
        // ADR-099: the Chat Router block is nested on `AgentConfig`, but
        // the wire stays flat — one `chat_router_*` field per sub-field
        // so the desktop's per-field write-through debouncing composes
        // without read-modify-write races on the whole block.
        if let Some(v) = chat_router_enabled {
            patches.push(ConfigFieldPatch {
                field: ConfigField::ChatRouterEnabled,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = chat_router_model {
            patches.push(ConfigFieldPatch {
                field: ConfigField::ChatRouterModel,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = chat_router_provider {
            patches.push(ConfigFieldPatch {
                field: ConfigField::ChatRouterProvider,
                op: value_to_patch(&v),
            });
        }
        if let Some(v) = chat_router_timeout_ms {
            patches.push(ConfigFieldPatch {
                field: ConfigField::ChatRouterTimeoutMs,
                op: value_to_patch(&v),
            });
        }
        PutAgentConfigBody { patches }
    }
}

fn value_to_patch(v: &serde_json::Value) -> FieldPatch<serde_json::Value> {
    match v {
        serde_json::Value::Null => FieldPatch::Clear,
        other => FieldPatch::Set(other.clone()),
    }
}

/// Response for `GET /agents/{id}/config` (matches the existing wire
/// envelope — see also `http/server.rs::get_agent_config`).
#[derive(Debug, Clone, Serialize)]
pub struct GetAgentConfigResponse {
    pub agent_id: String,
    pub config: Option<AgentConfig>,
    pub manifest_path: std::path::PathBuf,
    pub work_dir: std::path::PathBuf,
}

/// Result for `PUT /agents/{id}/config`.
///
/// `config` is the new on-disk state (re-serialised by the impl).
/// `overrides` is the typed projection of all live-editable fields
/// the handler should broadcast to active sessions via the
/// `UserOp::UpdateRuntimeConfig` dispatch path; the impl builds this
/// in lockstep with the persistence so the two never drift.
#[derive(Debug, Clone)]
pub struct PutAgentConfigResult {
    pub agent_id: String,
    pub config: AgentConfig,
    pub overrides: RuntimeConfigOverrides,
    /// Serialized JSON of the persisted `AgentConfig` - used by the
    /// HTTP handler to re-PUBLISH the retained MQTT config snapshot
    /// without re-reading the file from disk.
    pub config_json: String,
}

// ── Error ──────────────────────────────────────────────────────────────

/// Errors that `AgentConfigService` operations can produce.
///
/// The HTTP layer maps each variant to a deterministic status code
/// (all currently map to 500).
#[derive(Debug, thiserror::Error)]
pub enum AgentConfigError {
    /// Failed to load, parse, or persist `agent_config.json`.
    #[error("failed to persist agent_config.json: {0}")]
    Persistence(String),
}

// ── Trait ──────────────────────────────────────────────────────────────

/// UseCase trait for `GET/PUT /agents/{id}/config` — the per-agent
/// runtime configuration endpoints that mutate `agent_config.json`.
///
/// See the module-level docs for the rationale, the explicit list of
/// covered endpoints, and the live-broadcast / MQTT re-PUBLISH
/// side-effects that intentionally stay in the HTTP handler.
#[async_trait]
pub trait AgentConfigService: Send + Sync {
    /// `GET /agents/{id}/config` — read the current `agent_config.json`
    /// (returns `None` when no file exists yet — fresh-install case).
    async fn get_config(&self, agent_id: &str) -> GetAgentConfigResponse;

    /// `PUT /agents/{id}/config` — apply the per-field patches and
    /// persist. Returns the new on-disk state plus a
    /// [`RuntimeConfigOverrides`] projection for the HTTP handler to
    /// broadcast to active sessions.
    ///
    /// Empty patch list is valid: the impl re-saves the current
    /// state (no-op write) so the caller can still observe the
    /// retained snapshot / broadcast side-effects. The MQTT
    /// re-PUBLISH in the handler is idempotent, so this is safe.
    async fn put_config(
        &self,
        agent_id: &str,
        body: PutAgentConfigBody,
    ) -> Result<PutAgentConfigResult, AgentConfigError>;
}
