//! RuntimeAgentConfigService — implements [`AgentConfigService`].
//!
//! Owns the load-merge-save cycle against `agent_config.json`. The
//! HTTP handler stays a thin protocol converter that:
//!   1. decodes the wire shape into [`PutAgentConfigBody`],
//!   2. hands it to [`RuntimeAgentConfigService::put_config`],
//!   3. re-PUBLISHes the retained MQTT snapshot using the returned
//!      [`AgentConfig`],
//!   4. broadcasts the returned [`RuntimeConfigOverrides`] to active
//!      sessions via the existing dispatch channel.
//!
//! The trait impl is the **single audit point** for `agent_config.json`
//! persistence on the HTTP path: field dispatch, type-checked
//! `serde_json::from_value` per field, and the post-persist
//! `RuntimeConfigOverrides` projection. Wire shape → patch list
//! translation lives in [`PutAgentConfigBody::from_request_fields`]
//! so the handler doesn't repeat it.
//!
//! ## State
//!
//! Holds the agent `work_dir` resolved at boot (no async resource
//! dependencies), so the service can be constructed immediately after
//! the workspace services in `session_init.rs` Phase B and doesn't
//! require a late-bind slot's typical "wait for Phase B" setup.
//!
//! ## Type validation
//!
//! Every field is decoded via `serde_json::from_value::<T>()` where
//! `T` is the concrete `AgentConfig` field type. A wrong-typed JSON
//! value (e.g. `{"temperature": "hot"}`) collapses to `None` (i.e.
//! "leave on-disk alone") and emits a `tracing::warn!` — matching the
//! pre-refactor handler behaviour so the migration is invisible to
//! the desktop.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent_config::{self, AgentConfig};
use crate::usecases::agent_config::{
    AgentConfigError, AgentConfigService, ConfigField, ConfigFieldPatch, FieldPatch,
    GetAgentConfigResponse, PutAgentConfigBody, PutAgentConfigResult,
};

/// Concrete [`AgentConfigService`] backed by the
/// `workspace/config/agent_config.json` file.
pub struct RuntimeAgentConfigService {
    work_dir: PathBuf,
}

impl RuntimeAgentConfigService {
    pub fn new(work_dir: PathBuf) -> Self {
        Self { work_dir }
    }
}

#[async_trait]
impl AgentConfigService for RuntimeAgentConfigService {
    async fn get_config(&self, agent_id: &str) -> GetAgentConfigResponse {
        let config = agent_config::load_agent_config(&self.work_dir)
            .ok()
            .flatten();
        GetAgentConfigResponse {
            agent_id: agent_id.to_string(),
            config,
            manifest_path: self.work_dir.join("manifest.toml"),
            work_dir: self.work_dir.clone(),
        }
    }

    async fn put_config(
        &self,
        agent_id: &str,
        body: PutAgentConfigBody,
    ) -> Result<PutAgentConfigResult, AgentConfigError> {
        // 1. Load current (or default for fresh-install).
        let mut cfg: AgentConfig = agent_config::load_agent_config(&self.work_dir)
            .map_err(|e| AgentConfigError::Persistence(e.to_string()))?
            .unwrap_or_default();

        // 2. Apply patches. Same dispatch loop the pre-refactor
        //    handler used, lifted here so the persistence + type
        //    validation has exactly one canonical home.
        for ConfigFieldPatch { field, op } in &body.patches {
            apply_field_patch(&mut cfg, *field, op);
        }

        // 3. Persist (atomic write-tmp-rename).
        agent_config::save_agent_config(&self.work_dir, &cfg)
            .map_err(|e| AgentConfigError::Persistence(e.to_string()))?;

        tracing::info!(
            agent_id,
            patch_count = body.patches.len(),
            "RuntimeAgentConfigService::put_config: agent_config.json persisted"
        );

        // 4. Project the new on-disk state to a RuntimeConfigOverrides
        //    so the HTTP handler can broadcast to active sessions.
        //    Using the existing `From<&AgentConfig>` impl keeps the
        //    projection schema-locked to the live-broadcast path.
        let overrides = RuntimeConfigOverrides::from(&cfg);

        // Serialize the persisted config so the HTTP handler can
        // re-PUBLISH the retained MQTT snapshot without re-reading
        // the file from disk (ADR-040: handler must not touch fs).
        let config_json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());

        Ok(PutAgentConfigResult {
            agent_id: agent_id.to_string(),
            config: cfg,
            overrides,
            config_json,
        })
    }
}

// ── Field dispatch ─────────────────────────────────────────────────────

/// Apply one [`ConfigFieldPatch`] to an in-memory [`AgentConfig`].
/// Mirrors the pre-refactor handler's dispatch loop so the migration
/// is invisible to the desktop (same per-field type checks, same
/// `tracing::warn!` on type mismatch).
fn apply_field_patch(
    cfg: &mut AgentConfig,
    field: ConfigField,
    op: &FieldPatch<serde_json::Value>,
) {
    match field {
        ConfigField::MaxOutputTokens => {
            cfg.max_output_tokens = patch_typed::<u64>(field, op);
        }
        ConfigField::MaxIterations => {
            cfg.max_iterations = patch_typed::<u32>(field, op);
        }
        ConfigField::MaxSessions => {
            cfg.max_sessions = patch_typed::<u64>(field, op).map(|v| v as usize);
        }
        ConfigField::Temperature => {
            cfg.temperature = patch_typed::<f32>(field, op);
        }
        ConfigField::ContextWindow => {
            cfg.context_window = patch_typed::<u64>(field, op);
        }
        ConfigField::ShellApprovalThreshold => {
            cfg.shell_approval_threshold = patch_typed::<String>(field, op);
        }
        ConfigField::ApprovalTimeoutSecs => {
            cfg.approval_timeout_secs = patch_typed::<u64>(field, op);
        }
        ConfigField::CompressionRatioThreshold => {
            cfg.compression_ratio_threshold = patch_typed::<f64>(field, op);
        }
        // ADR-101: no distiller / forgetting arms — those knobs belong to the
        // memory service and `AgentConfig` no longer writes them.
        ConfigField::SessionLanguage => {
            cfg.session_language = patch_typed::<String>(field, op);
        }
        // ADR-099: sub-field patches against the nested `chat_router`
        // block. The block is created on first write; an all-`None`
        // block serializes as `{}` and `chat_router_config()` reads it
        // back as the design defaults, so clearing is lossless.
        ConfigField::ChatRouterEnabled => {
            chat_router(cfg).enabled = patch_typed::<bool>(field, op);
        }
        ConfigField::ChatRouterModel => {
            // Empty string = explicit clear (the Setup panel's "auto"
            // option): the wire collapses JSON-null into "absent", so
            // `""` is the only value that can express "fall back to
            // the compact-model chain".
            chat_router(cfg).model = patch_typed::<String>(field, op)
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty());
        }
        ConfigField::ChatRouterProvider => {
            chat_router(cfg).provider = patch_typed::<String>(field, op)
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty());
        }
        ConfigField::ChatRouterTimeoutMs => {
            chat_router(cfg).timeout_ms = patch_typed::<u64>(field, op);
        }
        ConfigField::ChatRouterHistoryMaxTokens => {
            chat_router(cfg).history_max_tokens = patch_typed::<usize>(field, op);
        }
    }
}

/// Get-or-create the nested `chat_router` block for a sub-field patch.
fn chat_router(
    cfg: &mut AgentConfig,
) -> &mut crate::agent_config::ChatRouterConfig {
    cfg.chat_router.get_or_insert_with(Default::default)
}

/// Type-erase a `FieldPatch<serde_json::Value>` into the
/// persistence-loop-friendly `Option<T>` shape (`Set(v)` -> `Some(T)`,
/// `Clear` -> `None`). A wrong-typed JSON value (e.g. `Set("foo")` for
/// a `u64` field) collapses to `None` and emits a `tracing::warn!`,
/// matching the pre-refactor handler so the two write paths can never
/// disagree about what landed on disk.
fn patch_typed<T>(field: ConfigField, patch: &FieldPatch<serde_json::Value>) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    match patch {
        FieldPatch::Clear => None,
        FieldPatch::Set(v) => match serde_json::from_value::<T>(v.clone()) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(
                    field = field.as_str(),
                    value = ?v,
                    error = %e,
                    "AgentConfigService::put_config: type mismatch — leaving on-disk value"
                );
                None
            }
        },
    }
}

// ============================================================================
// Tests — ADR-099 chat_router field patches and wire translation
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecases::agent_config::FieldPatch;

    fn patch(field: ConfigField, value: serde_json::Value) -> ConfigFieldPatch {
        ConfigFieldPatch {
            field,
            op: FieldPatch::Set(value),
        }
    }

    fn clear(field: ConfigField) -> ConfigFieldPatch {
        ConfigFieldPatch {
            field,
            op: FieldPatch::Clear,
        }
    }

    fn apply(cfg: &mut AgentConfig, patches: &[ConfigFieldPatch]) {
        for ConfigFieldPatch { field, op } in patches {
            apply_field_patch(cfg, *field, op);
        }
    }

    /// ADR-099: the flat `chat_router_*` wire fields translate onto
    /// sub-field patches (present -> Set, null -> Clear, absent -> skip)
    /// so the Desktop's per-field write-through composes without a
    /// read-modify-write race on the nested block. This test pins the
    /// present/null mapping to the right `ConfigField`; the absent->skip
    /// path is covered by every other call in this module passing `None`.
    #[test]
    fn test_chat_router_wire_to_patch_translation() {
        let body = PutAgentConfigBody::from_request_fields(
            None, None, None, None, None, None, None, None, // 8 core knobs
            None, // session_language
            Some(serde_json::json!(true)),   // chat_router_enabled -> Set
            Some(serde_json::json!("fast")), // chat_router_model -> Set
            Some(serde_json::json!(null)),   // chat_router_provider -> Clear
            None,                            // chat_router_timeout_ms absent -> skip
            Some(serde_json::json!(12_000)), // chat_router_history_max_tokens -> Set
        );

        let fields: Vec<(ConfigField, &FieldPatch<serde_json::Value>)> =
            body.patches.iter().map(|p| (p.field, &p.op)).collect();
        assert_eq!(fields.len(), 4, "three present + one clear, absent skipped");
        assert!(fields.contains(&(
            ConfigField::ChatRouterEnabled,
            &FieldPatch::Set(serde_json::json!(true))
        )));
        assert!(fields.contains(&(
            ConfigField::ChatRouterModel,
            &FieldPatch::Set(serde_json::json!("fast"))
        )));
        assert!(fields.contains(&(ConfigField::ChatRouterProvider, &FieldPatch::Clear)));
        assert!(fields.contains(&(
            ConfigField::ChatRouterHistoryMaxTokens,
            &FieldPatch::Set(serde_json::json!(12_000))
        )));
        assert!(
            !fields
                .iter()
                .any(|(f, _)| *f == ConfigField::ChatRouterTimeoutMs)
        );
    }

    /// ADR-099: `chat_router_*` sub-field patches create the nested block,
    /// set one sub-field at a time, treat an empty string as "clear" (the
    /// Setup panel's "auto" option), and never disturb sibling fields.
    #[test]
    fn test_chat_router_subfield_patches() {
        let mut cfg = AgentConfig::default();
        assert!(cfg.chat_router.is_none());

        apply(
            &mut cfg,
            &[patch(
                ConfigField::ChatRouterModel,
                serde_json::json!("fast-model"),
            )],
        );
        let router = cfg.chat_router.as_ref().expect("block created");
        assert_eq!(router.model.as_deref(), Some("fast-model"));
        assert!(router.enabled.is_none(), "sibling fields untouched");

        apply(
            &mut cfg,
            &[patch(
                ConfigField::ChatRouterEnabled,
                serde_json::json!(false),
            )],
        );
        assert_eq!(cfg.chat_router.as_ref().unwrap().enabled, Some(false));

        // Empty string clears the model back to the compact chain while
        // the kill switch stays where the user left it.
        apply(
            &mut cfg,
            &[patch(ConfigField::ChatRouterModel, serde_json::json!(""))],
        );
        let router = cfg.chat_router.as_ref().unwrap();
        assert_eq!(router.model, None);
        assert_eq!(router.enabled, Some(false));

        apply(
            &mut cfg,
            &[patch(ConfigField::ChatRouterTimeoutMs, serde_json::json!(15000))],
        );
        assert_eq!(cfg.chat_router.as_ref().unwrap().timeout_ms, Some(15000));

        // The history token budget is a plain number, so it only has
        // set / clear — no empty-string spelling of "default".
        apply(
            &mut cfg,
            &[patch(
                ConfigField::ChatRouterHistoryMaxTokens,
                serde_json::json!(12000),
            )],
        );
        let router = cfg.chat_router.as_ref().unwrap();
        assert_eq!(router.history_max_tokens, Some(12000));
        assert_eq!(router.history_max_tokens(), 12000);
        assert_eq!(router.timeout_ms, Some(15000), "sibling preserved");

        // Provider round-trips the same way the model does: a value
        // sets it, an empty string clears it back to "whichever provider
        // serves the model", and neither touches the other sub-fields.
        apply(
            &mut cfg,
            &[
                patch(
                    ConfigField::ChatRouterProvider,
                    serde_json::json!("openai"),
                ),
                patch(ConfigField::ChatRouterTimeoutMs, serde_json::json!(20000)),
            ],
        );
        let router = cfg.chat_router.as_ref().unwrap();
        assert_eq!(router.provider.as_deref(), Some("openai"));
        assert_eq!(router.timeout_ms, Some(20000));
        assert_eq!(router.model, None, "still cleared from the prior step");

        apply(
            &mut cfg,
            &[patch(ConfigField::ChatRouterProvider, serde_json::json!(""))],
        );
        assert_eq!(cfg.chat_router.as_ref().unwrap().provider, None);

        // `FieldPatch::Clear` (wire `null`) nulls a sub-field exactly like the
        // empty string does, and still leaves every sibling alone.
        apply(&mut cfg, &[clear(ConfigField::ChatRouterTimeoutMs)]);
        let router = cfg.chat_router.as_ref().unwrap();
        assert_eq!(router.timeout_ms, None, "Clear nulls the target");
        assert_eq!(router.enabled, Some(false), "sibling preserved");

        // Clearing the budget falls back to the design default rather than
        // to "no history": a router turn with zero context routes worse than
        // one paying the default price.
        apply(&mut cfg, &[clear(ConfigField::ChatRouterHistoryMaxTokens)]);
        let router = cfg.chat_router.as_ref().unwrap();
        assert_eq!(router.history_max_tokens, None);
        assert_eq!(
            router.history_max_tokens(),
            crate::agent_config::ChatRouterConfig::DEFAULT_HISTORY_MAX_TOKENS
        );
    }
}


