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
        ConfigField::DistillerEnabled => {
            cfg.distiller_enabled = patch_typed::<bool>(field, op);
        }
        ConfigField::DistillerModels => {
            cfg.distiller_models =
                patch_typed::<Vec<acowork_core::protocol::CompactModelRef>>(field, op);
        }
        ConfigField::DistillerIntervalMinutes => {
            cfg.distiller_interval_minutes = patch_typed::<u64>(field, op);
        }
        ConfigField::DistillerAccumulationThreshold => {
            cfg.distiller_accumulation_threshold =
                patch_typed::<u64>(field, op).map(|v| v as usize);
        }
        ConfigField::DistillerIdleMinutes => {
            cfg.distiller_idle_minutes = patch_typed::<u64>(field, op);
        }
        ConfigField::MemoryForgettingEnabled => {
            cfg.memory_forgetting_enabled = patch_typed::<bool>(field, op);
        }
        ConfigField::MemoryForgettingHalfLifeDays => {
            cfg.memory_forgetting_half_life_days = patch_typed::<u64>(field, op);
        }
        ConfigField::MemoryForgettingDormantThreshold => {
            cfg.memory_forgetting_dormant_threshold = patch_typed::<f32>(field, op);
        }
        ConfigField::MemoryForgettingArchiveDays => {
            cfg.memory_forgetting_archive_days = patch_typed::<u64>(field, op);
        }
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
// Tests — ADR-071 D4 distiller field patches
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

    /// ADR-071 D4: the enabled switch accepts booleans, refuses wrong-typed
    /// JSON (collapses to leave-on-disk), and Clear nulls it.
    #[test]
    fn test_distiller_enabled_patch() {
        let mut cfg = AgentConfig::default();
        apply(
            &mut cfg,
            &[patch(
                ConfigField::DistillerEnabled,
                serde_json::json!(true),
            )],
        );
        assert_eq!(cfg.distiller_enabled, Some(true));

        // Wrong-typed value: the shared dispatch collapses it to `None`
        // (pre-existing semantics — `patch_typed` returns None on parse
        // failure and `apply_field_patch` assigns unconditionally, so the
        // field is CLEARED, not preserved; documented in review #33).
        apply(
            &mut cfg,
            &[patch(
                ConfigField::DistillerEnabled,
                serde_json::json!("yes"),
            )],
        );
        assert_eq!(cfg.distiller_enabled, None, "bad type clears the field");

        apply(
            &mut cfg,
            &[patch(
                ConfigField::DistillerEnabled,
                serde_json::json!(false),
            )],
        );
        assert_eq!(cfg.distiller_enabled, Some(false));

        // Explicit clear -> None.
        apply(&mut cfg, &[clear(ConfigField::DistillerEnabled)]);
        assert_eq!(cfg.distiller_enabled, None);
    }

    /// ADR-071 D4/D5 (list form): the models patch accepts an array of
    /// `{provider_id, model_id}` refs, refuses a plain string, and Clear
    /// nulls it.
    #[test]
    fn test_distiller_models_patch() {
        let mut cfg = AgentConfig::default();
        apply(
            &mut cfg,
            &[patch(
                ConfigField::DistillerModels,
                serde_json::json!([
                    {"provider_id": "openai", "model_id": "gpt-4o-mini"},
                    {"provider_id": "ollama", "model_id": "qwen2.5:0.5b"},
                ]),
            )],
        );
        let models = cfg.distiller_models.as_ref().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].provider_id, "openai");
        assert_eq!(models[1].model_id, "qwen2.5:0.5b");

        // Wrong-typed value: clears the field (see test_distiller_enabled_patch
        // for the pre-existing dispatch semantics).
        apply(
            &mut cfg,
            &[patch(
                ConfigField::DistillerModels,
                serde_json::json!("oops"),
            )],
        );
        assert_eq!(cfg.distiller_models, None, "bad shape clears the field");

        // Explicit clear.
        apply(&mut cfg, &[clear(ConfigField::DistillerModels)]);
        assert_eq!(cfg.distiller_models, None);
    }

    /// ADR-071 D1/D3: the three trigger fields accept u64 numbers (accumulation
    /// narrows to usize), refuse strings, and Clear nulls them.
    #[test]
    fn test_distiller_numeric_patches() {
        let mut cfg = AgentConfig::default();
        apply(
            &mut cfg,
            &[
                patch(ConfigField::DistillerIntervalMinutes, serde_json::json!(45)),
                patch(
                    ConfigField::DistillerAccumulationThreshold,
                    serde_json::json!(80),
                ),
                patch(ConfigField::DistillerIdleMinutes, serde_json::json!(20)),
            ],
        );
        assert_eq!(cfg.distiller_interval_minutes, Some(45));
        assert_eq!(cfg.distiller_accumulation_threshold, Some(80usize));
        assert_eq!(cfg.distiller_idle_minutes, Some(20));

        // Wrong-typed values: clear the fields (see test_distiller_enabled_patch
        // for the pre-existing dispatch semantics).
        apply(
            &mut cfg,
            &[
                patch(
                    ConfigField::DistillerIntervalMinutes,
                    serde_json::json!("fast"),
                ),
                patch(
                    ConfigField::DistillerAccumulationThreshold,
                    serde_json::json!("many"),
                ),
                patch(ConfigField::DistillerIdleMinutes, serde_json::json!(12.5)),
            ],
        );
        assert_eq!(cfg.distiller_interval_minutes, None);
        assert_eq!(cfg.distiller_accumulation_threshold, None);
        assert_eq!(cfg.distiller_idle_minutes, None);

        // Explicit clears.
        apply(
            &mut cfg,
            &[
                clear(ConfigField::DistillerIntervalMinutes),
                clear(ConfigField::DistillerAccumulationThreshold),
                clear(ConfigField::DistillerIdleMinutes),
            ],
        );
        assert_eq!(cfg.distiller_interval_minutes, None);
        assert_eq!(cfg.distiller_accumulation_threshold, None);
        assert_eq!(cfg.distiller_idle_minutes, None);
    }

    /// ADR-071 D4: `from_request_fields` maps absent/null/value onto
    /// skip/Clear/Set for the five distiller wire fields.
    #[test]
    fn test_distiller_wire_to_patch_translation() {
        let body = PutAgentConfigBody::from_request_fields(
            None, // max_output_tokens absent -> skip
            None, // max_iterations absent -> skip
            None, // max_sessions absent -> skip
            None, // temperature absent -> skip
            None, // context_window absent -> skip
            None, // shell_approval_threshold absent -> skip
            None, // approval_timeout_secs absent -> skip
            None, // compression_ratio_threshold absent -> skip
            Some(serde_json::json!(true)), // distiller_enabled
            None, // distiller_model (legacy) absent
            Some(serde_json::json!([
                {"provider_id": "p", "model_id": "m"}
            ])), // distiller_models
            Some(serde_json::json!(30)),   // distiller_interval_minutes
            Some(serde_json::json!(null)), // distiller_accumulation_threshold -> Clear
            None,                          // distiller_idle_minutes absent -> skip
            None,                          // memory_forgetting_enabled absent -> skip
            None,                          // memory_forgetting_half_life_days absent -> skip
            None,                          // memory_forgetting_dormant_threshold absent -> skip
            None,                          // memory_forgetting_archive_days absent -> skip
            None,                          // session_language absent -> skip
            None,                          // chat_router_enabled absent -> skip
            None,                          // chat_router_model absent -> skip
            None,                          // chat_router_provider absent -> skip
            None,                          // chat_router_timeout_ms absent -> skip
        );

        let fields: Vec<(ConfigField, &FieldPatch<serde_json::Value>)> =
            body.patches.iter().map(|p| (p.field, &p.op)).collect();
        assert_eq!(
            fields.len(),
            4,
            "five present/clear + one absent -> four patches"
        );
        assert!(fields.contains(&(
            ConfigField::DistillerEnabled,
            &FieldPatch::Set(serde_json::json!(true))
        )));
        assert!(fields.contains(&(
            ConfigField::DistillerModels,
            &FieldPatch::Set(serde_json::json!([{"provider_id":"p","model_id":"m"}]))
        )));
        assert!(fields.contains(&(
            ConfigField::DistillerIntervalMinutes,
            &FieldPatch::Set(serde_json::json!(30))
        )));
        assert!(fields.contains(&(
            ConfigField::DistillerAccumulationThreshold,
            &FieldPatch::Clear
        )));
        assert!(
            !fields
                .iter()
                .any(|(f, _)| *f == ConfigField::DistillerIdleMinutes)
        );
    }

    /// Memory forgetting: `from_request_fields` maps absent/null/value
    /// onto skip/Clear/Set for the four forgetting wire fields.
    #[test]
    fn test_forgetting_wire_to_patch_translation() {
        let body = PutAgentConfigBody::from_request_fields(
            None,                          // max_output_tokens absent -> skip
            None,                          // max_iterations absent -> skip
            None,                          // max_sessions absent -> skip
            None,                          // temperature absent -> skip
            None,                          // context_window absent -> skip
            None,                          // shell_approval_threshold absent -> skip
            None,                          // approval_timeout_secs absent -> skip
            None,                          // compression_ratio_threshold absent -> skip
            None,                          // distiller_enabled absent
            None,                          // distiller_model absent
            None,                          // distiller_models absent
            None,                          // distiller_interval_minutes absent
            None,                          // distiller_accumulation_threshold absent
            None,                          // distiller_idle_minutes absent
            Some(serde_json::json!(true)), // memory_forgetting_enabled
            Some(serde_json::json!(180)),  // memory_forgetting_half_life_days
            Some(serde_json::json!(null)), // memory_forgetting_dormant_threshold -> Clear
            None,                          // memory_forgetting_archive_days absent -> skip
            None,                          // session_language absent -> skip
            None,                          // chat_router_enabled absent -> skip
            None,                          // chat_router_model absent -> skip
            None,                          // chat_router_provider absent -> skip
            None,                          // chat_router_timeout_ms absent -> skip
        );

        let fields: Vec<(ConfigField, &FieldPatch<serde_json::Value>)> =
            body.patches.iter().map(|p| (p.field, &p.op)).collect();
        assert_eq!(
            fields.len(),
            3,
            "two present/clear + one absent -> three patches"
        );
        assert!(fields.contains(&(
            ConfigField::MemoryForgettingEnabled,
            &FieldPatch::Set(serde_json::json!(true))
        )));
        assert!(fields.contains(&(
            ConfigField::MemoryForgettingHalfLifeDays,
            &FieldPatch::Set(serde_json::json!(180))
        )));
        assert!(fields.contains(&(
            ConfigField::MemoryForgettingDormantThreshold,
            &FieldPatch::Clear
        )));
        assert!(
            !fields
                .iter()
                .any(|(f, _)| *f == ConfigField::MemoryForgettingArchiveDays)
        );
    }

    /// ADR-099: the flat `chat_router_*` wire fields translate onto
    /// sub-field patches (present -> Set, null -> Clear, absent -> skip)
    /// so the Desktop's per-field write-through composes without a
    /// read-modify-write race on the nested block. The distiller /
    /// forgetting wire tests already pin the absent->skip path for these
    /// four args; this test pins the present/null mapping to the right
    /// `ConfigField`.
    #[test]
    fn test_chat_router_wire_to_patch_translation() {
        let body = PutAgentConfigBody::from_request_fields(
            None, None, None, None, None, None, None, None, // 8 core knobs
            None, None, None, None, None, None, // 6 distiller
            None, None, None, None, // 4 memory-forgetting
            None, // session_language
            Some(serde_json::json!(true)),   // chat_router_enabled -> Set
            Some(serde_json::json!("fast")), // chat_router_model -> Set
            Some(serde_json::json!(null)),   // chat_router_provider -> Clear
            None,                            // chat_router_timeout_ms absent -> skip
        );

        let fields: Vec<(ConfigField, &FieldPatch<serde_json::Value>)> =
            body.patches.iter().map(|p| (p.field, &p.op)).collect();
        assert_eq!(fields.len(), 3, "two present + one clear, absent skipped");
        assert!(fields.contains(&(
            ConfigField::ChatRouterEnabled,
            &FieldPatch::Set(serde_json::json!(true))
        )));
        assert!(fields.contains(&(
            ConfigField::ChatRouterModel,
            &FieldPatch::Set(serde_json::json!("fast"))
        )));
        assert!(fields.contains(&(ConfigField::ChatRouterProvider, &FieldPatch::Clear)));
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
    }
}
