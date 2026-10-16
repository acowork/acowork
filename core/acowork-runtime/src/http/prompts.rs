//! Runtime hot-reload endpoint for package prompts and skills.
//!
//! Prompt and skill files are accessed through the restricted package
//! workspace. This module retains only the explicit reload operation.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Serialize;

use crate::http::server::HttpState;

/// JSON error envelope for the reload handler.
fn err_response(status: StatusCode, code: &str, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": code,
            "message": message,
        })),
    )
        .into_response()
}

async fn post_reload_prompts(State(state): State<HttpState>, Path(id): Path<String>) -> Response {
    if !state.instance_matches(&id) {
        return err_response(
            StatusCode::NOT_FOUND,
            "instance_id_mismatch",
            format!(
                "path `{id}` is not this runtime's instance id `{}`",
                state.instance_id
            ),
        );
    }

    // Clone the canonical `Arc<AgentCore>` out of the late-bind slot.
    // The handler holds the slot lock for at most a clone() — the
    // per-field writes go through the inner `Arc<RwLock<Option<String>>>`
    // handles inside AgentCore, so we never hold both locks at once.
    let core_arc = {
        let slot = state.agent_core.read().unwrap();
        slot.clone()
    };
    let Some(core_arc) = core_arc else {
        return err_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_core_not_ready",
            "AgentCore slot is empty — Phase B has not yet constructed the core".to_string(),
        );
    };

    if let Err(msg) =
        crate::package::prompt_builder::reload_prompts_into_core(&state.package_dir, &core_arc)
    {
        return err_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reload_dispatch_error",
            msg,
        );
    }

    // Reload the executable skill definitions as an immutable snapshot.
    // The SessionManager atomically swaps it so existing and future sessions
    // resolve skill commands against the same current package state.
    match crate::skills::parser::SkillRegistry::load_from_dir(&state.package_dir.join("skills")) {
        Ok(registry) => {
            crate::http::server::dispatch_agent_level_config(
                &state,
                "skill_registry_reload",
                crate::agent::inbound::InboundMessage::UpdateSkillRegistry { registry },
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(agent_id = %state.agent_id, %error, "skill registry reload failed")
        }
    }

    // ADR-063 §3.7.6: after the 7 overrides, also rebuild the main-dialog
    // system prompt (system.md + all prompt sections) and push it to live
    // sessions — best-effort, see `rebuild_and_dispatch_system_prompt`.
    let system_prompt_reloaded = rebuild_and_dispatch_system_prompt(&state).await;

    (
        StatusCode::OK,
        Json(ReloadPromptsResponse {
            agent_id: state.agent_id.clone(),
            reloaded_count: crate::package::prompt_builder::OVERRIDABLE_PROMPTS.len(),
            system_prompt_reloaded,
        }),
    )
        .into_response()
}

/// Rebuild the main-dialog system prompt from `prompts/*.md` (including
/// the required `system.md`) and push it to every live session.
///
/// ADR-063 §3.7.6 hot-reload: `system.md` is a normal dialog section, NOT
/// one of the 7 `OVERRIDABLE_PROMPTS`, so `reload_prompts_into_core` never
/// touches it. To make edits take effect without an agent restart we
/// re-assemble the full prompt here (same inputs as Phase A:
/// `build_system_prompt_with_mode` + the resolved skill mode) and dispatch
/// it through the agent-level pipeline (`dispatch_agent_level_config` →
/// `SessionManager::apply_system_prompt` → per-session
/// `ContextBuilder::set_system_prompt`).
///
/// Best-effort: a failure (e.g. unreadable manifest) is logged and
/// reported via `system_prompt_reloaded = false` — the 7 overrides above
/// still apply, and the on-disk file remains authoritative for the next
/// boot.
/// ADR-094: rebuild the Progressive-mode skills summary and dispatch
/// it through the agent-level pipeline (`dispatch_agent_level_config` →
/// `SessionManager::apply_skill_instructions` → per-session
/// `ContextBuilder::set_skill_summary`).
///
/// Mirrors the "compute → template sync + broadcast" one-pipeline
/// pattern used by `apply_builtin_tools_enabled` /
/// `apply_system_prompt` / `apply_runtime_config_override` — used by
/// the L2 reload endpoint (`POST /agents/{id}/prompts/reload`) and
/// by `broadcast_builtin_tools_update` (live-coupled to the
/// `load_skill` built-in tool toggle).
///
/// `true` = dispatched; `false` = skipped (manifest or prompt build
/// failed, on-disk file remains authoritative for next boot).
pub(crate) async fn rebuild_and_dispatch_skill_instructions(state: &HttpState) -> bool {
    match crate::package::prompt_builder::compute_skill_instructions_payload(
        &state.package_dir,
        &state.work_dir,
    ) {
        Ok(summary) => {
            crate::http::server::dispatch_agent_level_config(
                state,
                "skill_instructions_reload",
                crate::agent::inbound::InboundMessage::UpdateSkillInstructions {
                    instructions: summary,
                },
            )
            .await;
            true
        }
        Err(e) => {
            tracing::warn!(
                agent_id = %state.agent_id,
                error = %e,
                "skill instructions hot-reload skipped (on-disk file is authoritative for next boot)"
            );
            false
        }
    }
}

/// Rebuild the main-dialog system prompt from `prompts/*.md` (including
/// the required `system.md`) and push it to every live session.
///
/// ADR-063 §3.7.6 hot-reload: `system.md` is a normal dialog section, NOT
/// one of the 7 `OVERRIDABLE_PROMPTS`, so `reload_prompts_into_core` never
/// touches it. To make edits take effect without an agent restart we
/// re-assemble the full prompt here (same inputs as Phase A:
/// `build_system_prompt_with_mode` + the resolved skill mode) and dispatch
/// it through the agent-level pipeline (`dispatch_agent_level_config` →
/// `SessionManager::apply_system_prompt` → per-session
/// `ContextBuilder::set_system_prompt`).
///
/// Skills-summary dispatch is delegated to
/// [`rebuild_and_dispatch_skill_instructions`] so the L2-reload flow and
/// the `PUT /agents/{id}/builtin-tools` `load_skill` toggle run on the
/// same compute path (no two implementations of the same wiring).
///
/// Best-effort: a failure (e.g. unreadable manifest) is logged and
/// reported via `system_prompt_reloaded = false` — the 7 overrides above
/// still apply, and the on-disk file remains authoritative for the next
/// boot.
async fn rebuild_and_dispatch_system_prompt(state: &HttpState) -> bool {
    let manifest = match crate::package::loader::load_manifest(&state.package_dir) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                agent_id = %state.agent_id,
                error = %e,
                "system prompt hot-reload skipped: failed to load manifest (on-disk file is authoritative for next boot)"
            );
            return false;
        }
    };
    let skill_mode = crate::cli::resolve_skill_mode(&manifest, &state.work_dir.to_string_lossy());
    let prompt_parts = match crate::package::prompt_builder::build_system_prompt_with_mode(
        &state.package_dir,
        skill_mode,
    ) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                agent_id = %state.agent_id,
                error = %e,
                "system prompt hot-reload skipped: failed to rebuild prompt (on-disk file is authoritative for next boot)"
            );
            return false;
        }
    };
    let system_prompt = prompt_parts.base.clone();
    crate::http::server::dispatch_agent_level_config(
        state,
        "system_prompt_reload",
        crate::agent::inbound::InboundMessage::UpdateSystemPrompt { system_prompt },
    )
    .await;
    // ADR-094: Progressive-mode skills summary is a SEPARATE
    // `ContextBuilder` section. Delegated to the shared helper so the
    // L2-reload flow and the per-agent `load_skill` toggle
    // (`PUT /agents/{id}/builtin-tools`) run on the same dispatch
    // pipeline.
    rebuild_and_dispatch_skill_instructions(state).await
}

/// `POST /agents/{id}/prompts/reload` response envelope.
#[derive(Debug, Clone, Serialize)]
struct ReloadPromptsResponse {
    pub agent_id: String,
    /// Number of prompt overrides reloaded into the live AgentCore.
    pub reloaded_count: usize,
    /// True iff the main-dialog system prompt (system.md + all prompt
    /// sections) was also rebuilt and pushed to live sessions. False when
    /// the rebuild failed (e.g. unreadable manifest) — the 7 overrides
    /// still applied, but system.md changes need a restart.
    pub system_prompt_reloaded: bool,
}

/// File content is served by workspace routes; only reload remains here.
pub(crate) fn prompts_routes() -> Router<HttpState> {
    Router::new().route("/agents/{id}/prompts/reload", post(post_reload_prompts))
}

#[cfg(test)]
mod tests {
    // ── ADR-094: load_skill toggle hot-reload contract ──────────────
    //
    // Pins that `compute_skill_instructions_payload` honours the
    // `agent_tools.json::load_skill.enabled` override priority (the
    // Desktop Tools panel `load_skill` Switch). Without this
    // pin, a future refactor could silently drop the override and
    // regress the bug fixed by `rebuild_and_dispatch_skill_instructions`.
    //
    // Three cells:
    // 1. `load_skill.enabled = true` (Progressive) → Some(summary)
    // 2. `load_skill.enabled = false` (Manual) → None
    // 3. `load_skill` entry absent in `agent_tools.json` → fall
    //    through to manifest default (no `[skills]` section → Manual
    //    → None)
    #[test]
    fn test_compute_skill_instructions_payload_priority() {
        use crate::package::prompt_builder::compute_skill_instructions_payload;
        use std::fs;

        // Shared package: `manifest.toml` + `prompts/system.md` +
        // `skills/greeting/SKILL.md`. All per-test workdirs share this
        // package via a single temp dir; the workdir controls the
        // per-agent `agent_tools.json::load_skill.enabled` override.
        let package_dir = std::env::temp_dir().join("acowork-test-prompts-cip");
        let _ = fs::remove_dir_all(&package_dir);
        fs::create_dir_all(package_dir.join("prompts")).unwrap();
        fs::create_dir_all(package_dir.join("skills").join("greeting")).unwrap();
        // `load_manifest` requires manifest.toml at the package root —
        // minimal valid shape (matches the synthetic manifests in
        // `cli::resolve_skill_mode_tests`).
        fs::write(
            package_dir.join("manifest.toml"),
            "agent_id = \"com.test.cip\"\n\
             name = \"cip-test\"\n\
             version = \"0.0.1\"\n\
             author = \"test\"\n\
             description = \"synthetic manifest for compute_skill_instructions_payload tests\"\n\
             runtime_version = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            package_dir.join("prompts").join("system.md"),
            "You are a test assistant.",
        )
        .unwrap();
        fs::write(
            package_dir.join("skills").join("greeting").join("SKILL.md"),
            "---\nname: greeting\ndescription: Greet users warmly\ntriggers:\n  - hello\n---\n\n# Greeting Skill\n\nBe friendly.\n",
        )
        .unwrap();

        // Cell 1: `load_skill.enabled = true` → Some(summary)
        let wd1 = std::env::temp_dir().join("acowork-test-prompts-cip-wd1");
        let _ = fs::remove_dir_all(&wd1);
        fs::create_dir_all(wd1.join("config")).unwrap();
        fs::write(
            wd1.join("config").join("agent_tools.json"),
            r#"{"tools":[{"name":"load_skill","enabled":true}]}"#,
        )
        .unwrap();
        let summary = compute_skill_instructions_payload(&package_dir, &wd1)
            .expect("cell 1: compute must succeed when skills/ is populated");
        let summary = summary.expect("cell 1: Progressive + load_skill=true must yield Some");
        assert!(
            summary.contains("greeting"),
            "summary must list the skill name"
        );
        assert!(
            summary.contains("Greet users warmly"),
            "summary must include the skill description"
        );

        // Cell 2: `load_skill.enabled = false` → None (Manual mode clears the section)
        let wd2 = std::env::temp_dir().join("acowork-test-prompts-cip-wd2");
        let _ = fs::remove_dir_all(&wd2);
        fs::create_dir_all(wd2.join("config")).unwrap();
        fs::write(
            wd2.join("config").join("agent_tools.json"),
            r#"{"tools":[{"name":"load_skill","enabled":false}]}"#,
        )
        .unwrap();
        let summary = compute_skill_instructions_payload(&package_dir, &wd2)
            .expect("cell 2: compute must succeed in Manual mode");
        assert!(
            summary.is_none(),
            "cell 2: Manual mode must produce None (clears the section)"
        );

        // Cell 3: `load_skill` entry absent in `agent_tools.json` →
        // resolve_skill_mode falls through to manifest default. The
        // synthetic package has no [skills] section, so the manifest
        // default is Manual → None.
        let wd3 = std::env::temp_dir().join("acowork-test-prompts-cip-wd3");
        let _ = fs::remove_dir_all(&wd3);
        fs::create_dir_all(wd3.join("config")).unwrap();
        fs::write(
            wd3.join("config").join("agent_tools.json"),
            r#"{"tools":[{"name":"memory_recall","enabled":true}]}"#,
        )
        .unwrap();
        let summary = compute_skill_instructions_payload(&package_dir, &wd3)
            .expect("cell 3: compute must succeed with no load_skill entry");
        assert!(
            summary.is_none(),
            "cell 3: manifest default (no [skills] section, no load_skill entry) must yield None"
        );
    }
}
