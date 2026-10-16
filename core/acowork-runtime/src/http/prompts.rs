//! ADR-063 — Package-level LLM prompt override HTTP routes.
//!
//! Four handlers expose the 5 overridable prompts (see
//! [`crate::package::prompt_builder::OVERRIDABLE_PROMPTS`]) plus the
//! required `system.md` dialog section to the Debug panel and any
//! external tooling:
//!
//! **ADR-068 note**: The three grafeo-specific overrides
//! (`extraction`, `conflict-classification`, `generalization`) were
//! removed in this revision because ADR-068 rewrites the LLM→memory
//! boundary (LLM writes only Episode nodes via `memory_store`; the
//! distillation path is owned by the offline `EpisodicDistiller`
//! pipeline). Its internal constants (`EXTRACTION_SYSTEM_PROMPT` /
//! `CONFLICT_CLASSIFICATION_PROMPT` / `GENERALIZATION_PROMPT`) stay
//! private to the distiller module in `acowork-memory`, and they are not
//! exposed via the package `prompts/` overlay.
//!
//! | Method | Path                                | Handler         |
//! |--------|-------------------------------------|-----------------|
//! | GET    | `/agents/{id}/prompts`              | [`list_prompts`] |
//! | GET    | `/agents/{id}/prompts/{name}`       | [`get_prompt`]   |
//! | PUT    | `/agents/{id}/prompts/{name}`       | [`put_prompt`]   |
//! | POST   | `/agents/{id}/prompts/reload`       | [`post_reload_prompts`] |
//!
//! ## Path `id` semantics
//!
//! Each handler validates `Path(id) == state.instance_id` (same cross-process
//! guard as `/agents/{id}/config`, see ADR-034). A mismatch returns 404
//! rather than silently writing to the wrong runtime — see `put_prompt`.
//!
//! ## Path `{name}` semantics
//!
//! `{name}` is the **file basename without `.md`** (e.g. `summary`,
//! `compact-template`). Resolution to the actual file on disk is done by
//! [`resolve_prompt_entry`] which performs a strict whitelist match
//! against [`OVERRIDABLE_PROMPTS`]. Any `name` that doesn't resolve —
//! including names containing `/`, `\`, `..`, or any case variant — is
//! rejected with 404. This is the security guard referenced by the
//! `test_is_overridable_prompt_rejects_unknown_filenames` test in
//! `prompt_builder.rs`.
//!
//! ## PUT semantics
//!
//! PUT writes UTF-8 text to `<package_dir>/prompts/<file>.md`. The file
//! is **not** automatically reloaded into the live `AgentCore` Arc —
//! the Debug panel / caller is expected to follow up with
//! `POST /agents/{id}/prompts/reload` (ADR-063 §3.7.6) so the L2
//! reload path documented in ADR-063 §3.7.5 fires. This matches
//! the existing `/workspaces/file` semantics: the disk write is the
//! source of truth; reload is a separate intentional step.
//!
//! ## Reload semantics
//!
//! `POST /agents/{id}/prompts/reload` reads all 7 canonical
//! `prompts/<file>.md` override files from `package_dir` and writes them
//! back into the matching `Arc<RwLock<Option<String>>>` slot on the live
//! `AgentCore` via [`reload_prompts_into_core`]. This is a
//! **package-level** operation — it does not require DevMode to be
//! enabled (that was the old behaviour when the route sat under
//! `/api/debug/prompts/reload`, and is what caused the "刷新 503"
//! bug — see ADR-063 §3.7.7 for the corrected placement).
//!
//! Size cap: 1 MiB per prompt (well above the longest built-in
//! `EXTRACTION_SYSTEM_PROMPT`, which is ~3 KiB — generous headroom for
//! extensions without enabling accidental binary uploads).

use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::http::server::HttpState;

/// Hard cap for a single prompt write. 1 MiB is well above any built-in
/// constant (longest is `EXTRACTION_SYSTEM_PROMPT` ~3 KiB) but small
/// enough to prevent accidental binary upload or DoS via giant PUTs.
const MAX_PROMPT_BYTES: usize = 1024 * 1024;

// ── Wire types ────────────────────────────────────────────────────────

/// Per-prompt metadata. Shared by [`list_prompts`] and [`get_prompt`].
#[derive(Debug, Clone, Serialize)]
pub struct PromptMeta {
    /// Basename without `.md` — the same string the URL `{name}` takes.
    /// The Debug panel uses this as the canonical UI key.
    pub name: &'static str,
    /// Relative path inside the package (`prompts/<file>.md`).
    pub file: &'static str,
    /// Short, user-facing description of what this prompt is used for and
    /// in which call path. Rendered in the Debug panel list under the
    /// prompt name so operators can recognise the slot at a glance.
    pub purpose: &'static str,
    /// True iff the package declares this file (Phase A read it; the
    /// live `AgentCore` field is `Some(_)`).
    pub overridden: bool,
    /// True iff this prompt is mandatory for the agent to function —
    /// currently only `system.md` (the main dialog identity section).
    /// The Debug panel renders a "required" badge and offers a create
    /// flow when a required prompt is missing.
    pub required: bool,
    /// File size on disk in bytes; 0 if `overridden = false`.
    pub size_bytes: u64,
    /// The built-in default text that takes effect when `overridden = false`.
    /// Mirrors the value of the Rust constant named in the corresponding
    /// `crate::prompt::*` / downstream grafeo/memory module — see
    /// ADR-063 §3.2. Held verbatim (not just the constant name) so the
    /// Debug panel can show the user what the LLM is currently using as a
    /// reference comment in the editor when no override exists.
    pub fallback_constant: &'static str,
}

/// `GET /agents/{id}/prompts` response envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ListPromptsResponse {
    pub agent_id: String,
    pub prompts: Vec<PromptMeta>,
}

/// `GET /agents/{id}/prompts/{name}` response envelope.
#[derive(Debug, Clone, Serialize)]
pub struct GetPromptResponse {
    pub agent_id: String,
    #[serde(flatten)]
    pub meta: PromptMeta,
    /// UTF-8 content; `None` when the package does not declare an
    /// override (the built-in `fallback_constant` is in effect).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// `PUT /agents/{id}/prompts/{name}` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct PutPromptRequest {
    pub content: String,
}

/// `PUT /agents/{id}/prompts/{name}` response envelope.
#[derive(Debug, Clone, Serialize)]
pub struct PutPromptResponse {
    pub agent_id: String,
    pub name: &'static str,
    pub file: &'static str,
    pub size_bytes: u64,
    pub accepted: bool,
    /// Reminder for the Debug panel / caller: disk write succeeded but
    /// the live `AgentCore` Arc was NOT updated. Follow up with
    /// `POST /api/agents/{id}/debug/prompts/reload` to apply (R7).
    pub reload_required: bool,
}

// ── Internal resolution ────────────────────────────────────────────────

/// Canonical mapping from URL `{name}` (basename without `.md`) to the
/// on-disk file. Built lazily on first access — same OnceLock pattern as
/// `prompt_builder::overridable_filenames`.
#[derive(Debug, Clone, Copy)]
struct PromptEntry {
    name: &'static str,
    file: &'static str,
    purpose: &'static str,
    fallback_constant: &'static str,
    /// True iff this prompt is mandatory (currently only `system.md`).
    required: bool,
}

const PROMPT_ENTRIES: &[PromptEntry] = &[
    PromptEntry {
        name: "system",
        file: "system.md",
        purpose: "Agent identity definition — the main dialog system prompt section (required).",
        fallback_constant: crate::prompt::PROMPT_BUILDER_FALLBACK,
        required: true,
    },
    PromptEntry {
        name: "summary",
        file: "summary.md",
        purpose: "Summarize the conversation history during context compaction (ADR-053).",
        fallback_constant: crate::prompt::COMPACTION_SYSTEM_PROMPT,
        required: false,
    },
    PromptEntry {
        name: "compact-template",
        file: "compact-template.md",
        purpose: "Template wrapping the conversation body before the compaction LLM call (must keep {messages_text}).",
        fallback_constant: crate::prompt::COMPACT_PROMPT,
        required: false,
    },
    PromptEntry {
        name: "title",
        file: "title.md",
        purpose: "Generate a session title (max 60 chars) from the first user message.",
        fallback_constant: crate::prompt::TITLE_PROMPT,
        required: false,
    },
    PromptEntry {
        name: "abstention",
        file: "abstention.md",
        purpose: "Refuse to answer when memory confidence is too low (memory RAG).",
        // Mirrors `acowork-memory::manager::DEFAULT_ABSTENTION_PROMPT`.
        fallback_constant: "When you are not confident about the information from memory, respond with 'I'm not sure about this' rather than guessing.",
        required: false,
    },
    PromptEntry {
        name: "distiller-merge",
        file: "distiller-merge.md",
        purpose: "Offline memory distiller — decide whether one new statement merges into, supersedes, or stands apart from its nearest existing nodes (ADR-071).",
        // The real fallback is `acowork_memory::consolidation::distiller::MERGE_SYSTEM_PROMPT`
        // (private const); keep a reference note rather than a copy that can
        // drift (see module docs: long built-ins are not mirrored).
        fallback_constant: "(built-in MERGE_SYSTEM_PROMPT — see acowork-memory/src/consolidation/distiller.rs)",
        required: false,
    },
];

fn lookup_entry(name: &str) -> Option<PromptEntry> {
    PROMPT_ENTRIES.iter().copied().find(|e| e.name == name)
}

fn resolve_prompt_path(
    package_dir: &std::path::Path,
    name: &str,
) -> Option<(PromptEntry, PathBuf)> {
    let entry = lookup_entry(name)?;
    // Defense in depth: even though `lookup_entry` only matches the
    // canonical names, reject any name with path separators to keep
    // the PUT path strictly inside `prompts/`.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    let path = package_dir.join("prompts").join(entry.file);
    Some((entry, path))
}

fn read_prompt_bytes(path: &std::path::Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => Ok(Some(s.trim_end_matches('\n').to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn build_meta(entry: PromptEntry, path: &std::path::Path) -> PromptMeta {
    let (overridden, size_bytes) = match std::fs::metadata(path) {
        Ok(md) => (true, md.len()),
        Err(_) => (false, 0),
    };
    PromptMeta {
        name: entry.name,
        file: entry.file,
        purpose: entry.purpose,
        overridden,
        required: entry.required,
        size_bytes,
        fallback_constant: entry.fallback_constant,
    }
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /agents/{id}/prompts` — list every overridable prompt with its
/// current `overridden` / `size_bytes` status. `id` mismatch returns an
/// empty list (404-equivalent for the listing endpoint — see ADR-034
/// "tolerate misconfigured Gateway" pattern).
async fn list_prompts(State(state): State<HttpState>, Path(id): Path<String>) -> Response {
    if !state.instance_matches(&id) {
        return (
            StatusCode::NOT_FOUND,
            Json(ListPromptsResponse {
                agent_id: state.agent_id.clone(),
                prompts: Vec::new(),
            }),
        )
            .into_response();
    }
    let prompts: Vec<PromptMeta> = PROMPT_ENTRIES
        .iter()
        .map(|entry| {
            let (_, path) = match resolve_prompt_path(&state.package_dir, entry.name) {
                Some(pair) => pair,
                None => unreachable!("canonical entries always resolve"),
            };
            build_meta(*entry, &path)
        })
        .collect();
    (
        StatusCode::OK,
        Json(ListPromptsResponse {
            agent_id: state.agent_id.clone(),
            prompts,
        }),
    )
        .into_response()
}

/// `GET /agents/{id}/prompts/{name}` — return content + metadata for a
/// single prompt. `content` is `None` when the package does not declare
/// the override (the Debug panel then shows the built-in
/// `fallback_constant` description instead).
async fn get_prompt(
    State(state): State<HttpState>,
    Path((id, name)): Path<(String, String)>,
) -> Response {
    if !state.instance_matches(&id) {
        return err_response(
            StatusCode::NOT_FOUND,
            "instance_id_mismatch",
            format!(
                "path '{}' is not this runtime's instance id '{}'",
                id, state.instance_id
            ),
        );
    }
    let (entry, path) = match resolve_prompt_path(&state.package_dir, &name) {
        Some(pair) => pair,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "unknown_prompt",
                format!(
                    "unknown prompt name '{}'; expected one of: {}",
                    name,
                    PROMPT_ENTRIES
                        .iter()
                        .map(|e| e.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
    };
    let meta = build_meta(entry, &path);
    let content = match read_prompt_bytes(&path) {
        Ok(c) => c,
        Err(e) => {
            return err_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "io_error",
                format!("failed to read {}: {}", meta.file, e),
            );
        }
    };
    (
        StatusCode::OK,
        Json(GetPromptResponse {
            agent_id: state.agent_id.clone(),
            meta,
            content,
        }),
    )
        .into_response()
}

/// `PUT /agents/{id}/prompts/{name}` — write `content` to the canonical
/// file under `<package_dir>/prompts/`. Does NOT update the live
/// `AgentCore` Arc — that requires the L2 reload endpoint (R7).
async fn put_prompt(
    State(state): State<HttpState>,
    Path((id, name)): Path<(String, String)>,
    Json(req): Json<PutPromptRequest>,
) -> Response {
    if !state.instance_matches(&id) {
        return err_response(
            StatusCode::NOT_FOUND,
            "instance_id_mismatch",
            format!(
                "path '{}' is not this runtime's instance id '{}'",
                id, state.instance_id
            ),
        );
    }
    let (entry, path) = match resolve_prompt_path(&state.package_dir, &name) {
        Some(pair) => pair,
        None => {
            return err_response(
                StatusCode::NOT_FOUND,
                "unknown_prompt",
                format!("unknown prompt name '{}'", name),
            );
        }
    };
    let bytes = req.content.len();
    if bytes > MAX_PROMPT_BYTES {
        return err_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!(
                "prompt size {} exceeds {} bytes (1 MiB cap)",
                bytes, MAX_PROMPT_BYTES
            ),
        );
    }
    if bytes == 0 || req.content.trim().is_empty() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "empty_content",
            "refusing to write an empty / whitespace-only prompt — use a delete endpoint or fix the file on disk".to_string(),
        );
    }
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "io_error",
            format!("failed to create prompts dir {}: {}", parent.display(), e),
        );
    }
    // Write atomically: write to `<path>.tmp`, then rename. Prevents a
    // half-written file if the process dies mid-write (an LLM in the
    // middle of reading the previous content would otherwise see
    // truncated text). Same discipline as the shell-risk-rules writer.
    let tmp_path = path.with_extension("md.tmp");
    if let Err(e) = std::fs::write(&tmp_path, req.content.as_bytes()) {
        return err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "io_error",
            format!("failed to write tmp {}: {}", tmp_path.display(), e),
        );
    }
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        let _ = std::fs::remove_file(&tmp_path);
        return err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "io_error",
            format!(
                "failed to rename {} -> {}: {}",
                tmp_path.display(),
                path.display(),
                e
            ),
        );
    }
    tracing::info!(
        agent_id = %state.agent_id,
        name = %entry.name,
        file = %entry.file,
        bytes,
        "ADR-063: prompt PUT succeeded; live AgentCore reload is a separate step (R7)"
    );
    (
        StatusCode::OK,
        Json(PutPromptResponse {
            agent_id: state.agent_id.clone(),
            name: entry.name,
            file: entry.file,
            size_bytes: bytes as u64,
            accepted: true,
            reload_required: true,
        }),
    )
        .into_response()
}

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

// ── Reload handler ────────────────────────────────────────────────────

/// `POST /agents/{id}/prompts/reload` — ADR-063 §3.7.6 L2 reload.
///
/// Pushes the on-disk `prompts/<file>.md` content into the live
/// `AgentCore` Arc so every running session that holds a clone sees the
/// new value (see ADR-063 §3.7.5). Same handler as the Debug panel
/// "刷新" button (UI label: `刷新`, see ADR-063 §3.7.7).
///
/// Replaces the old `POST /api/debug/prompts/reload` (ADR-048 §D8
/// historical placement) which 503'd outside DevMode because it
/// routed through `DebugService::reload_prompts` and the
/// `debug_service_slot` is only populated when DevMode is active.
/// Reload is a **package-level** operation, not a debug-session one —
/// the new placement under `/agents/{id}/prompts/reload` matches
/// the rest of the prompts family and works unconditionally.
///
/// Status codes:
/// - `200 OK` — reload succeeded (7 prompts reloaded).
/// - `404 Not Found` — `{id}` does not match `state.instance_id` (same guard
///   as every other handler in this module — see ADR-034 "tolerate
///   misconfigured Gateway" pattern).
/// - `503 Service Unavailable` — `AgentCore` slot is still empty (Phase B
///   has not yet constructed the core, or this is a pre-Phase-B test
///   harness).
/// - `422 Unprocessable Entity` — dispatch table drift (a future
///   `OVERRIDABLE_PROMPTS` entry with no matching `AgentCore` field).
///   Surfaces loudly rather than silently dropping a prompt; treat as a
///   bug.
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
    /// Always equals `OVERRIDABLE_PROMPTS.len()` (= 7 as of ADR-071).
    /// Returned so the Debug panel can render a "8 / 8 已重载" hint
    /// without re-fetching the list.
    pub reloaded_count: usize,
    /// True iff the main-dialog system prompt (system.md + all prompt
    /// sections) was also rebuilt and pushed to live sessions. False when
    /// the rebuild failed (e.g. unreadable manifest) — the 7 overrides
    /// still applied, but system.md changes need a restart.
    pub system_prompt_reloaded: bool,
}

// ── Router builder ────────────────────────────────────────────────────

/// Mount the four prompt routes on the Runtime's HTTP server.
///
/// Routes registered:
///   - `GET    /agents/{id}/prompts`
///   - `GET    /agents/{id}/prompts/{name}`
///   - `PUT    /agents/{id}/prompts/{name}`
///   - `POST   /agents/{id}/prompts/reload`
pub(crate) fn prompts_routes() -> Router<HttpState> {
    Router::new()
        .route("/agents/{id}/prompts", get(list_prompts))
        .route(
            "/agents/{id}/prompts/{name}",
            get(get_prompt).put(put_prompt),
        )
        .route("/agents/{id}/prompts/reload", post(post_reload_prompts))
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::prompt_builder::OVERRIDABLE_PROMPTS;

    #[test]
    fn test_lookup_entry_finds_all_entries() {
        // Cross-check against `OVERRIDABLE_PROMPTS`: every canonical
        // override filename must resolve through `lookup_entry` by its
        // `.md`-stripped basename, plus the required `system.md` section.
        for (file, _desc) in OVERRIDABLE_PROMPTS {
            let name = file.trim_end_matches(".md");
            assert!(
                lookup_entry(name).is_some(),
                "lookup_entry must find `{name}` (file `{file}`)"
            );
        }
        assert!(
            lookup_entry("system").is_some(),
            "lookup_entry must find the required `system` section"
        );
    }

    #[test]
    fn test_system_entry_is_required() {
        // The only `required` entry is `system.md` — the main dialog
        // identity section. Every other prompt is an optional override.
        let system = lookup_entry("system").expect("system entry must exist");
        assert!(system.required, "system.md must be marked required");
        assert_eq!(system.file, "system.md");
        for entry in PROMPT_ENTRIES {
            if entry.name != "system" {
                assert!(
                    !entry.required,
                    "`{}` must not be required — only system.md is mandatory",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn test_resolve_prompt_path_rejects_path_traversal() {
        let dir = std::env::temp_dir();
        assert!(resolve_prompt_path(&dir, "../etc/passwd").is_none());
        assert!(resolve_prompt_path(&dir, "sub/dir").is_none());
        assert!(resolve_prompt_path(&dir, "sub\\dir").is_none());
        assert!(resolve_prompt_path(&dir, "..").is_none());
    }

    #[test]
    fn test_resolve_prompt_path_rejects_unknown_names() {
        let dir = std::env::temp_dir();
        assert!(resolve_prompt_path(&dir, "unknown").is_none());
        assert!(resolve_prompt_path(&dir, "").is_none());
        // Case variant — only exact basename match is allowed.
        assert!(resolve_prompt_path(&dir, "Summary").is_none());
        assert!(resolve_prompt_path(&dir, "System").is_none());
    }

    #[test]
    fn test_max_prompt_bytes_constant_is_1mib() {
        assert_eq!(MAX_PROMPT_BYTES, 1024 * 1024);
    }

    #[test]
    fn test_prompt_entries_match_overridable_prompts() {
        // The (file, fallback_constant) pair must agree across the two
        // tables — otherwise the Debug panel would advertise one
        // fallback while the runtime resolves another.
        use std::collections::HashMap;
        let mut by_file: HashMap<&str, &str> = HashMap::new();
        for entry in PROMPT_ENTRIES {
            by_file.insert(entry.file, entry.fallback_constant);
        }
        for (file, _desc) in OVERRIDABLE_PROMPTS {
            assert!(
                by_file.contains_key(file),
                "PROMPT_ENTRIES missing entry for file `{file}`"
            );
        }
        // PROMPT_ENTRIES = 7 overridable overrides + the required
        // `system.md` dialog section (which is deliberately NOT in
        // OVERRIDABLE_PROMPTS — it is a normal dialog section, not a
        // task-instruction override).
        assert_eq!(
            by_file.len(),
            OVERRIDABLE_PROMPTS.len() + 1,
            "PROMPT_ENTRIES must be exactly OVERRIDABLE_PROMPTS + system.md"
        );
        assert!(
            by_file.contains_key("system.md"),
            "PROMPT_ENTRIES must include the required system.md section"
        );
    }

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
        assert!(summary.contains("greeting"), "summary must list the skill name");
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
