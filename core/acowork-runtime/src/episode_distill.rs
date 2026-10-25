//! Episode distillation & compaction — LLM-based semantic extraction from conversations.
//!
//! ## Compact Model role (ADR-011, revised)
//!
//! The compact model produces a natural-language summary used for two purposes:
//! - **Context compaction**: replaces the middle section of in-memory history
//!   when the 80% token threshold trips, so the next LLM call fits in budget.
//! - **Quality gate**: the summary must pass `is_low_quality` before it can
//!   replace history (raw tool output / reasoning dumps must not leak in).
//!
//! ## What the runtime does NOT do automatically (post-2026-09-10)
//!
//! Compaction is no longer a writer to Grafeo (was: `write_summary_to_provider`
//! called from `loop_context` after every successful compact). Session-close
//! tail distillation also no longer writes to Grafeo (`loop_session`).
//! Memory persistence is the agent's own decision through the memory service's
//! `memory_store` MCP tool (ADR-101); the Runtime does not write memories.
//! Rationale: keeping memory writes user-visible and agent-decided instead of
//! a silent system side-effect.
//!
//! ## Best-effort, non-blocking
//!
//! Compaction is best-effort: on quality-gate failure the history is left
//! untouched and the user-facing error surfaces (see ADR-061 §11.3).

use std::sync::OnceLock;

use regex::Regex;

use acowork_core::protocol::ModelCapabilitiesInfo;
use acowork_core::providers::traits::{ChatMessage, ChatRequest, MessageRole, Provider, UsageInfo};

// ADR-101: `DistilledEpisode` is a memory-service type; the Runtime does not
// name it any more (the re-export is gone with the acowork-memory dependency).

use crate::agent::loop_session::strip_think_block;
use crate::error::{Result, RuntimeError};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

// `DistilledEpisode` lives in `acowork-memory` (ADR-051 P2), which since ADR-101
// is the memory service — the Runtime no longer names it. `Triple` was removed
// in ADR-057 (triples-removed).

// ---------------------------------------------------------------------------
// Prompt templates
// ---------------------------------------------------------------------------

// Prompt moved to crate::prompt::COMPACT_PROMPT.

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parsed result from compact model output (summary-only).
///
/// ADR-057 (triples-removed): the `<triples>` block was dropped because
/// compact-model output quality was too low. The compact model now emits
/// only `<summary>` + (optional) `<user_intent>`. Knowledge-layer updates
/// flow through `memory_store` / procedural creation instead.
#[derive(Debug, Clone)]
pub struct CompactOutput {
    pub summary: String,
}

/// Parse the compact model's raw output into a [`CompactOutput`].
///
/// ADR-057 (triples-removed): the output only carries a `<summary>` block
/// (plus an optional `<user_intent>` used elsewhere). Any legacy `<triples>`
/// block the model still emits is deliberately ignored.
///
/// If the output does not contain a `<summary>` block, the entire text is
/// treated as the summary (backwards-compatible with pre-block format).
pub fn parse_compact_output(raw: &str) -> CompactOutput {
    let summary = extract_block(raw, "summary").unwrap_or_else(|| raw.trim().to_string());
    // Sanitize so role labels / tool echoes that the LLM failed to filter do
    // not land in the episodic memory (see `sanitize_summary_text`).
    let summary = sanitize_summary_text(&summary);
    // ADR-057 (triples-removed): `<triples>` block is ignored if the LLM
    // still emits one. We deliberately do NOT parse it, but surface it so
    // the model's non-compliance is observable in logs.
    if raw.contains("<triples>") {
        tracing::warn!(
            raw_preview = %raw.chars().take(200).collect::<String>(),
            "compact model emitted deprecated <triples> block; ignoring per ADR-057 (triples-removed)"
        );
    }
    CompactOutput { summary }
}

/// Quality/format gate error for LLM summary output (distillation & compaction).
///
/// Per the "LLM summaries must guarantee quality" principle, summary output
/// that fails this gate is **discarded** — it never lands in episodic memory
/// and never becomes the in-memory compaction marker:
///
/// - [`SummaryError::Empty`] / [`SummaryError::MissingBlock`] are
///   **retryable**: the model didn't follow the `<summary>` format, so a
///   different distillation target tier may do better.
/// - [`SummaryError::LowQuality`] is **not retryable**: the model produced a
///   structurally valid but unusable summary, and stepping down the fallback
///   chain only gets cheaper/weaker — the output is dropped
///   (quality-over-nothing).
#[derive(Debug, Clone)]
pub enum SummaryError {
    /// Model returned an empty response (or only thinking blocks).
    Empty(String),
    /// Output is missing the required `<summary>...</summary>` block.
    MissingBlock,
    /// Summary block exists but failed the heuristic quality gate.
    LowQuality(String),
}

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SummaryError::Empty(hint) => {
                write!(f, "compact model returned empty response{hint}")
            }
            SummaryError::MissingBlock => {
                write!(f, "compact model output is missing the <summary> block")
            }
            SummaryError::LowQuality(preview) => {
                write!(
                    f,
                    "compact model summary failed the quality gate: {preview:?}"
                )
            }
        }
    }
}

impl std::error::Error for SummaryError {}

impl SummaryError {
    /// Whether retrying with the next distillation target tier may help.
    ///
    /// `LowQuality` is a model-capability problem — the fallback chain only
    /// gets cheaper/weaker, so the output is discarded instead of retried.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, SummaryError::LowQuality(_))
    }
}

/// Minimum character count for an LLM summary to be considered substantive.
///
/// Shorter output is treated as the model stalling / refusing rather than a
/// real summary. Kept small so legitimate terse summaries are not misjudged.
const MIN_SUMMARY_CHARS: usize = 20;

/// Lazily-compiled regex matching `path/to/file.ext:NNN` leaks.
///
/// Reasoning models that ignore `thinking_mode=disabled` dump their chain of
/// thought into the content; a summary that still carries tool-output shapes
/// (file:line references) after sanitization is not clean prose.
fn file_line_leak_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"[A-Za-z0-9_\-./\\]+\.(?:rs|ts|tsx|js|jsx|py|go|java|kt|toml|json|ya?ml|sh|md|c|h|cpp|hpp):\s*\d+",
        )
        .expect("file-line leak regex is valid")
    })
}

/// Lazily-compiled regex matching raw tool-call/result echo tokens that
/// survived [`sanitize_summary_text`] (mid-line echoes, bracket variants).
fn tool_echo_leak_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|\s)\[(?:tool(?:\([^)]*\))?|tool_call|tool_result|thought)\]")
            .expect("tool echo leak regex is valid")
    })
}

/// Heuristic quality gate for a `<summary>` block (valid format, bad content).
///
/// Returns `true` when the summary shows clear signs of being unusable:
/// - too short to be a real summary (model stalling / refusing), or
/// - **two or more conversation-role label lines** (`[User]:`, `[Assistant]:`,
///   `[Tool(bash)]:`, ...) echoed verbatim — the model copied the raw dialog
///   into the summary instead of summarizing it (strong copy signal), or
/// - at least **two** contamination features: table-artifact lines,
///   `file:line` references, tool-call/result echoes, or a single role-label
///   line that survived sanitization.
///
/// A single contamination feature is tolerated (a summary may legitimately
/// mention one file path); two or more strongly indicate raw tool output or
/// reasoning dumps leaked into the summary.
fn is_low_quality(summary: &str) -> bool {
    let trimmed = summary.trim();
    if trimmed.chars().count() < MIN_SUMMARY_CHARS {
        return true;
    }
    let mut contamination_flags = 0u8;
    if trimmed
        .lines()
        .any(|l| l.trim_start().starts_with(['|', '│']))
    {
        contamination_flags += 1;
    }
    if file_line_leak_regex().is_match(trimmed) {
        contamination_flags += 1;
    }
    if tool_echo_leak_regex().is_match(trimmed) {
        contamination_flags += 1;
    }
    // Conversation-role label lines copied verbatim into the summary mean the
    // model echoed the raw dialog instead of summarizing it. Two or more
    // labelled lines are a strong copy signal → discard outright; a single
    // line is tolerated and only counts as one contamination feature.
    let role_label_echoes = trimmed
        .lines()
        .filter(|l| role_marker_prefix_regex().is_match(l) || tool_marker_line_regex().is_match(l))
        .count();
    if role_label_echoes >= 2 {
        return true;
    }
    if role_label_echoes == 1 {
        contamination_flags += 1;
    }
    contamination_flags >= 2
}

/// Validate raw compact-model output against the quality gate.
///
/// The gate is **strict**: a missing `<summary>` block is an error, not a
/// reason to fall back to the raw text — that fallback is exactly how
/// reasoning dumps landed in episodic memory. Callers either step down the
/// distillation target chain (retryable errors) or discard the output.
fn validate_summary_output(raw: &str) -> std::result::Result<(), SummaryError> {
    let Some(block) = extract_block(raw, "summary") else {
        return Err(SummaryError::MissingBlock);
    };
    if block.trim().is_empty() {
        return Err(SummaryError::Empty(
            " (response contained a <summary> block with no content)".to_string(),
        ));
    }
    if is_low_quality(&block) {
        let preview: String = block.trim().chars().take(200).collect();
        return Err(SummaryError::LowQuality(preview));
    }
    Ok(())
}

/// Strict variant of [`parse_compact_output`] for production write paths.
///
/// Unlike the backwards-compatible [`parse_compact_output`] (which treats a
/// missing `<summary>` block as "the whole output is the summary"), this
/// applies the quality gate first and **fails** when the block is missing or
/// unusable — the summary is discarded (quality-over-nothing), never
/// substituted with raw text.
pub fn parse_compact_output_strict(raw: &str) -> std::result::Result<CompactOutput, SummaryError> {
    validate_summary_output(raw)?;
    let summary = sanitize_summary_text(&extract_block(raw, "summary").expect("validated above"));
    // ADR-057 (triples-removed): `<triples>` block is ignored if the LLM
    // still emits one. We deliberately do NOT parse it.
    Ok(CompactOutput { summary })
}

/// ADR-061 §8.2/§8.3/§13.3: validated compaction summary output.
///
/// `summary` is the `<summary>` block content (empty when the tag is
/// missing — production paths gate the output through [`compact_with_llm`]
/// first, so the raw text is never substituted); `user_intent` is the
/// `<user_intent>` block content, falling back to the caller-supplied raw
/// user messages when the LLM omits the block. Both are sanitized so role
/// labels / tool echoes do not land in the marker text (see
/// [`sanitize_summary_text`]).
#[derive(Debug, Clone)]
pub struct ValidatedSummary {
    pub summary: String,
    pub user_intent: String,
}

/// Parse and validate the compact model's raw output (ADR-061 §8).
///
/// - `<summary>` missing → degrades to empty (production callers run the
///   output through the quality gate in [`compact_with_llm`] first, so a
///   missing block here means the caller is on a defensive path; the raw
///   text is NEVER substituted as the summary).
/// - `<user_intent>` missing → `fallback_user_intent` (the raw user
///   messages joined by the caller, compaction markers excluded) is used;
///   when that is also absent the block degrades to empty rather than
///   failing the compaction (the marker structure must always parse).
pub fn parse_and_validate_summary(
    raw: &str,
    fallback_user_intent: Option<&str>,
) -> ValidatedSummary {
    let summary = sanitize_summary_text(&extract_block(raw, "summary").unwrap_or_default());
    let user_intent = extract_block(raw, "user_intent")
        .map(|s| sanitize_summary_text(&s))
        .or_else(|| fallback_user_intent.map(str::to_string))
        .unwrap_or_default();
    ValidatedSummary {
        summary,
        user_intent,
    }
}

/// Extract the text content between `<tag>` and `</tag>` markers.
fn extract_block(text: &str, tag: &str) -> Option<String> {
    let start_marker = format!("<{}>", tag);
    let end_marker = format!("</{}>", tag);
    let start = text.find(&start_marker)? + start_marker.len();
    let end = text[start..].find(&end_marker)?;
    Some(text[start..start + end].trim().to_string())
}

/// Lazily-compiled regex for **tool-role marker lines**.
///
/// Matches a line that echoes a tool call / result back into the summary,
/// e.g. `[Tool(bash)]: ...`, `[Tool]:`, `[tool_call]: ...`, `[tool_result]: ...`.
/// Such lines are raw tool interleavings, never knowledge — the whole line is
/// dropped when sanitizing.
fn tool_marker_line_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*\[(?:tool(?:\([^\[\]]*\))?|thought|tool_call|tool_result)\]:")
            .expect("tool marker regex is valid")
    })
}

/// Lazily-compiled regex for **conversation-role markers** at line start.
///
/// Matches `[User]:`, `[Assistant]:`, `[System]:`, `[CompactionSummary]:` and
/// their lowercase JSONL variants (`[user]:`, `[assistant]:`, ...). The label
/// itself is stripped but any following text on the line is preserved.
fn role_marker_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^\s*\[(?:user|assistant|system|compaction_summary|compactionsummary)\]:\s*",
        )
        .expect("role marker regex is valid")
    })
}

/// Remove formatting artifacts from a compaction/distillation summary so the
/// text that lands in memory (Grafeo episodic) and in-memory context is clean
/// prose, never a verbatim echo of the conversation's role labels or tool
/// interleavings.
///
/// Two-stage cleanup:
/// 1. **Drop tool-echo lines entirely** — `[Tool(bash)]: <command>` etc. are
///    raw tool interleavings and are never knowledge worth remembering.
/// 2. **Strip conversation-role labels** (`[User]:`, `[Assistant]:`, ...) but
///    keep any following text on the line, so short-session raw-text fallback
///    (which stores the raw JSONL content as the summary) keeps the actual
///    dialogue without the labels.
///
/// The `[Tool result compressed...]` placeholder body is left untouched — it
/// is opaque tool *content* (handled by the prompt's acknowledgement rule),
/// not a role marker.
pub fn sanitize_summary_text(summary: &str) -> String {
    let tool_line = tool_marker_line_regex();
    let role_prefix = role_marker_prefix_regex();
    let mut out = Vec::with_capacity(summary.lines().count());
    for line in summary.lines() {
        if tool_line.is_match(line) {
            continue;
        }
        out.push(role_prefix.replace(line, "").to_string());
    }
    out.join("\n").trim().to_string()
}

// ---------------------------------------------------------------------------
// EpisodeDistiller
// ---------------------------------------------------------------------------

/// Distills conversation segments into semantic `DistilledEpisode` objects
/// using LLM-based extraction (natural-language output per ADR-011).
pub struct EpisodeDistiller;

impl EpisodeDistiller {
    /// Compact full conversation context into a natural-language summary.
    ///
    /// Used at 80% token usage threshold (context compaction) and for tail
    /// distillation at session close. The returned summary text is plain
    /// natural language — no JSON parsing needed.
    ///
    /// Returns the summary text (not a structured DistilledEpisode) so the
    /// caller can both write it to Grafeo and insert it into in-memory history.
    ///
    /// `identity_context` is the user's `UserProfile` formatted as text.
    /// When `Some`, it is embedded into the system prompt so the LLM writes
    /// the summary in the user's preferred language (see
    /// [`crate::prompt::build_compaction_system_prompt`]). Pass `None` when
    /// the session has no user profile yet (default → English summary).
    ///
    /// `compaction_prompt` is the agent-specific summarization directive from
    /// `prompts/summary.md` (see [`crate::package::prompt_builder::load_compaction_prompt`]).
    /// `None` falls back to the built-in [`crate::prompt::COMPACTION_SYSTEM_PROMPT`].
    ///
    /// Returns `(summary, usage)` per ADR-027 so callers can record raw
    /// Provider usage in [`crate::conversation::SessionTokens`].
    pub async fn compact_full_context(
        messages: &[ChatMessage],
        provider: &dyn Provider,
        model_name: &str,
        distill_max_tokens: u32,
        identity_context: Option<&str>,
        compaction_prompt: Option<&str>,
    ) -> Result<(String, UsageInfo)> {
        let messages_text = format_messages(messages);
        if messages_text.is_empty() {
            return Err(RuntimeError::Tool(
                "Cannot compact empty context".to_string(),
            ));
        }
        let prompt = crate::prompt::COMPACT_PROMPT.replace("{messages_text}", &messages_text);
        compact_with_llm(
            &prompt,
            provider,
            model_name,
            distill_max_tokens,
            identity_context,
            compaction_prompt.unwrap_or(crate::prompt::COMPACTION_SYSTEM_PROMPT),
        )
        .await
    }

    /// Compact a specific slice of in-memory messages (e.g. tail after last compaction).
    ///
    /// Same as `compact_full_context` but takes a slice reference for convenience
    /// when the caller already has the exact message range. `identity_context` is
    /// threaded through for the same language-aware reason as in
    /// [`Self::compact_full_context`], and `compaction_prompt` for the same
    /// per-agent summarization rules.
    ///
    /// Returns `(summary, usage)` per ADR-027 so callers can record raw
    /// Provider usage in [`crate::conversation::SessionTokens`].
    pub async fn compact_messages(
        messages: &[ChatMessage],
        provider: &dyn Provider,
        model_name: &str,
        distill_max_tokens: u32,
        identity_context: Option<&str>,
        compaction_prompt: Option<&str>,
    ) -> Result<(String, UsageInfo)> {
        Self::compact_full_context(
            messages,
            provider,
            model_name,
            distill_max_tokens,
            identity_context,
            compaction_prompt,
        )
        .await
    }

    /// Select the cheapest model from a list of `ModelCapabilitiesInfo`.
    ///
    /// Cost is estimated as `input_per_million + output_per_million`.
    /// Models without cost information are ranked last.
    /// Returns `None` if the list is empty.
    pub fn select_cheapest_model(
        models: &[ModelCapabilitiesInfo],
    ) -> Option<&ModelCapabilitiesInfo> {
        if models.is_empty() {
            return None;
        }

        models.iter().min_by(|a, b| {
            let cost_a = model_cost_score(a);
            let cost_b = model_cost_score(b);
            cost_a
                .partial_cmp(&cost_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute a cost score for a model (lower = cheaper).
///
/// Uses `input_per_million + output_per_million` as a simple heuristic.
/// Models without cost information get `f64::MAX` (ranked last).
pub fn model_cost_score(model: &ModelCapabilitiesInfo) -> f64 {
    match &model.cost {
        Some(cost) => {
            let input = cost.input_per_million.unwrap_or(0.0);
            let output = cost.output_per_million.unwrap_or(0.0);
            if input == 0.0 && output == 0.0 {
                // No meaningful cost data
                f64::MAX
            } else {
                input + output
            }
        }
        None => f64::MAX,
    }
}

use crate::agent::history::COMPACTION_SUMMARY_NAME;

/// Format a slice of `ChatMessage` into a human-readable text block for LLM-based
/// compaction & distillation.
///
/// ## Role label convention
///
/// | Actual role        | Detected condition                                          | Emitted label                                   |
/// |--------------------|-------------------------------------------------------------|-------------------------------------------------|
/// | `System`           | —                                                           | `[System]`                                      |
/// | any                | `name == Some("compaction_summary")`                        | `[CompactionSummary]`                           |
/// | `User`             | —                                                           | `[User]`                                        |
/// | `Assistant`        | —                                                           | `[Assistant]`                                   |
/// | `Tool`             | name set                                                    | `[Tool(name={name}, id={tool_call_id})]`        |
/// | `Tool`             | otherwise                                                   | `[Tool]`                                        |
///
/// Enhanced Tool labels give the compaction LLM enough visibility to write
/// a meaningful summary (ADR-032). Tool-result compression is retired, so
/// every Tool label carries name + id when available.
pub(crate) fn format_messages(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .map(|msg| {
            // Compaction summary marker takes precedence over the role
            // label — the marker lives at `User` role in memory (see
            // `HistoryManager::replace_middle_with_summary`), but we
            // still want the compaction LLM to recognize it as
            // previous-compaction output rather than fresh user input.
            if msg.name.as_deref() == Some(COMPACTION_SUMMARY_NAME) {
                return format!("[CompactionSummary]: {}", msg.content);
            }
            let role_label = match msg.role {
                MessageRole::System => "System".to_string(),
                MessageRole::User => "User".to_string(),
                MessageRole::Assistant => "Assistant".to_string(),
                MessageRole::Tool => match msg.name.as_deref() {
                    Some(name) => {
                        let tc_id = msg.tool_call_id.as_deref().unwrap_or("?");
                        format!("Tool(name={name}, id={tc_id})")
                    }
                    None => "Tool".to_string(),
                },
            };
            format!("[{}]: {}", role_label, msg.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Send a compaction prompt to the LLM and return the plain-text response.
///
/// Per [ADR-011], the LLM outputs natural language — no JSON parsing needed.
///
/// Core LLM-based compaction call shared by all distillation paths.
///
/// Builds the ChatRequest, sends it to the provider, strips thinking blocks
/// from the response, and returns the summary. `thinking_mode` is explicitly
/// set to `"disabled"` so reasoning models don't silo the output into
/// `reasoning_content`.
///
/// `system_prompt` is passed through to
/// [`crate::prompt::build_compaction_system_prompt`] together with
/// `identity_context` so the summary lands in the user's preferred language.
///
/// Returns `(summary, usage)` so the caller can record raw Provider usage
/// in the session's [`SessionTokens`] accumulator (ADR-027). `usage` is the
/// raw `UsageInfo` from the response, or `UsageInfo::default()` if the
/// Provider did not return one — callers must check `usage.prompt_tokens > 0`
/// before treating the values as a real cost record.
pub(crate) async fn compact_with_llm(
    prompt: &str,
    provider: &dyn Provider,
    model_name: &str,
    max_tokens: u32,
    identity_context: Option<&str>,
    system_prompt: &str,
) -> Result<(String, UsageInfo)> {
    let system_prompt =
        crate::prompt::build_compaction_system_prompt(system_prompt, identity_context);

    // Explicitly disable deep thinking for compaction/distillation.
    // Reasoning models may otherwise put the entire summary in
    // reasoning_content, leaving content empty and causing compaction
    // to fail silently.
    let request = ChatRequest {
        model: model_name.to_string(),
        messages: vec![
            ChatMessage {
                role: MessageRole::System,
                content: system_prompt,
                ..Default::default()
            },
            ChatMessage::user(prompt),
        ],
        temperature: None,
        max_tokens: Some(max_tokens),
        tools: None,
        reasoning_effort: None,
        thinking_mode: Some("disabled".to_string()),
    };

    let response = provider.chat(request).await.map_err(RuntimeError::Core)?;

    // ADR-027: capture raw Provider usage for session token accounting.
    // Providers that omit usage (e.g. some local mocks) yield
    // `UsageInfo::default()` — callers can detect this by `prompt_tokens == 0`.
    let usage = response.usage.unwrap_or_default();

    // Strip any thinking/reasoning blocks that may have leaked into content.
    // NEVER fall back to reasoning_content — it is the model's internal
    // monologue, not a useful summary.
    let raw_content = response.content.trim();
    let summary = strip_think_block(raw_content);
    if summary.is_empty() {
        let hint = if !raw_content.is_empty() {
            " (response contained only thinking/reasoning blocks that were stripped)"
        } else {
            " (content was empty — model may have ignored thinking_mode=disabled)"
        };
        return Err(RuntimeError::Summary(SummaryError::Empty(hint.to_string())));
    }
    // Quality gate: the output MUST carry a non-trivial <summary> block.
    // A missing block or low-quality content is an error — never fall back
    // to the raw text (that fallback is how reasoning dumps polluted
    // episodic memory). Retryable errors let the caller step down the
    // distillation target chain; LowQuality discards the output.
    validate_summary_output(&summary).map_err(RuntimeError::Summary)?;
    Ok((summary, usage))
}

/// Generate a session title from the first user message using the compact model.
///
/// Unlike [`compact_with_llm`], this is a minimal single-message call:
/// - No system prompt
/// - No identity block injection (language is already resolved into `{language}`)
/// - Lower `max_tokens` (title is ≤SESSION_TITLE_MAX_CHARS chars)
///
/// `prompt` should be [`crate::prompt::TITLE_PROMPT`] with `{language}` and
/// `{user_message}` already resolved by the caller.
///
/// Returns `(title, usage)` so the caller can record raw Provider usage in
/// the session's [`SessionTokens`] accumulator (ADR-027). `usage` is the
/// raw `UsageInfo` from the response, or `UsageInfo::default()` if absent.
pub async fn compact_session_title_with_llm(
    prompt: &str,
    provider: &dyn Provider,
    model_name: &str,
    max_tokens: u32,
) -> Result<(String, UsageInfo)> {
    // Explicitly disable deep thinking for title generation.
    // Reasoning models (DeepSeek, o-series) may otherwise put the entire
    // response in reasoning_content, leaving content empty — and
    // reasoning_content is the model's internal monologue, not a title.
    // If the model ignores this hint and still returns empty content,
    // the caller falls back to truncating the user message.
    let request = ChatRequest {
        model: model_name.to_string(),
        messages: vec![ChatMessage::user(prompt)],
        temperature: Some(0.3),
        max_tokens: Some(max_tokens),
        tools: None,
        reasoning_effort: None,
        thinking_mode: Some("disabled".to_string()),
    };

    let response = provider.chat(request).await.map_err(RuntimeError::Core)?;

    // ADR-027: capture raw Provider usage for session token accounting.
    let usage = response.usage.unwrap_or_default();

    // Never fall back to reasoning_content — it is the model's internal
    // monologue and unsuitable as a user-facing title.
    const MAX_RAW_LEN: usize = 500;
    let raw_content: String = response.content.chars().take(MAX_RAW_LEN).collect();
    let title = strip_think_block(&raw_content);
    if title.is_empty() {
        let hint = if !raw_content.trim().is_empty() {
            " (response contained only thinking/reasoning blocks that were stripped)"
        } else {
            " (content was empty — model may have ignored thinking_mode=disabled)"
        };
        return Err(RuntimeError::Tool(format!(
            "Title model returned empty response{hint}"
        )));
    }
    Ok((title, usage))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::providers::traits::ChatResponse;

    /// Minimal stub Provider that returns a canned `ChatResponse`.
    ///
    /// Used to verify that `compact_with_llm` / `compact_session_title_with_llm`
    /// correctly thread `UsageInfo` through their return tuple (ADR-027).
    struct StubProvider {
        content: String,
        usage: Option<UsageInfo>,
    }

    impl StubProvider {
        fn new(content: &str) -> Self {
            Self {
                content: content.to_string(),
                usage: None,
            }
        }
        fn with_usage(content: &str, usage: UsageInfo) -> Self {
            Self {
                content: content.to_string(),
                usage: Some(usage),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        async fn chat(
            &self,
            _request: ChatRequest,
        ) -> std::result::Result<ChatResponse, acowork_core::AcoworkError> {
            Ok(ChatResponse {
                content: self.content.clone(),
                usage: self.usage.clone(),
                ..Default::default()
            })
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> std::result::Result<
            Box<
                dyn futures_core::Stream<Item = acowork_core::providers::traits::StreamEvent>
                    + Send,
            >,
            acowork_core::AcoworkError,
        > {
            Err(acowork_core::AcoworkError::Unknown(
                "streaming not supported in stub".to_string(),
            ))
        }
        async fn chat_token_count(
            &self,
            _messages: &[acowork_core::providers::traits::ChatMessage],
        ) -> std::result::Result<u64, acowork_core::AcoworkError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn test_compact_with_llm_returns_usage_tuple_when_provider_supplies_it() {
        // ADR-027: Provider returning usage must thread through (String, UsageInfo).
        let provider = StubProvider::with_usage(
            "<summary>a concise summary of the conversation here</summary>",
            UsageInfo {
                prompt_tokens: 4_000,
                completion_tokens: 800,
                total_tokens: 4_800,
                ..Default::default()
            },
        );
        let result = compact_with_llm(
            "ignored",
            &provider,
            "model",
            1024,
            None,
            crate::prompt::COMPACTION_SYSTEM_PROMPT,
        )
        .await;
        let (summary, usage) = result.expect("compact_with_llm should succeed");
        assert!(
            summary.contains("a concise summary of the conversation"),
            "summary must be the raw marker-carrying output, got: {summary}"
        );
        assert_eq!(usage.prompt_tokens, 4_000);
        assert_eq!(usage.completion_tokens, 800);
        assert_eq!(usage.total_tokens, 4_800);
    }

    #[tokio::test]
    async fn test_compact_with_llm_returns_zero_usage_when_provider_omits_it() {
        // ADR-027 "宁可 miss 也不估计": when Provider returns no usage,
        // the second tuple element is UsageInfo::default() (all zeros).
        let provider =
            StubProvider::new("<summary>a concise summary of the conversation here</summary>");
        let result = compact_with_llm(
            "ignored",
            &provider,
            "model",
            1024,
            None,
            crate::prompt::COMPACTION_SYSTEM_PROMPT,
        )
        .await;
        let (_summary, usage) = result.expect("compact_with_llm should succeed");
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
    }

    #[tokio::test]
    async fn compact_messages_rejects_markerless_reasoning_dump() {
        // Tail-distillation entry point (used by close_session_inner) must
        // hit the same quality gate: a marker-less reasoning dump is an
        // error, never a summary (P1 — quality-over-nothing).
        let provider = StubProvider::new(
            "确认 disable 路径已清理 pending 槽位。\n开始实施。改两处：\n1. 删除 resolve_distill_model 调用\n2. 验证 fallback 链",
        );
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: "你好，请帮我分析这个问题".to_string(),
            ..Default::default()
        }];
        let err =
            EpisodeDistiller::compact_messages(&messages, &provider, "model", 1024, None, None)
                .await
                .unwrap_err();
        assert!(
            matches!(err, RuntimeError::Summary(SummaryError::MissingBlock)),
            "marker-less dump must fail the quality gate, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_compact_session_title_with_llm_returns_usage_tuple() {
        // Title generation also returns (title, usage) per ADR-027.
        let provider = StubProvider::with_usage(
            "Rust async ownership",
            UsageInfo {
                prompt_tokens: 200,
                completion_tokens: 10,
                total_tokens: 210,
                ..Default::default()
            },
        );
        let result = compact_session_title_with_llm("ignored", &provider, "model", 64).await;
        let (title, usage) = result.expect("title generation should succeed");
        assert_eq!(title, "Rust async ownership");
        assert_eq!(usage.prompt_tokens, 200);
        assert_eq!(usage.completion_tokens, 10);
    }

    #[test]
    fn test_select_cheapest_model_empty() {
        assert!(EpisodeDistiller::select_cheapest_model(&[]).is_none());
    }

    #[test]
    fn test_select_cheapest_model_single() {
        let models = vec![model_info("cheap-model", Some((0.5, 1.5)), 8192)];
        let selected = EpisodeDistiller::select_cheapest_model(&models);
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().name.as_deref(), Some("cheap-model"));
    }

    #[test]
    fn test_select_cheapest_model_multiple() {
        let models = vec![
            model_info("expensive-model", Some((5.0, 15.0)), 32768),
            model_info("cheap-model", Some((0.1, 0.2)), 8192),
            model_info("unknown-cost-model", None, 128000),
        ];
        let selected = EpisodeDistiller::select_cheapest_model(&models);
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().name.as_deref(), Some("cheap-model"));
    }

    #[test]
    fn test_format_messages_basic() {
        // Basic System / User / Assistant messages — no name metadata.
        let messages = vec![
            ChatMessage::system("System prompt"),
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ];
        let text = format_messages(&messages);
        assert!(text.starts_with("[System]: System prompt"));
        assert!(text.contains("[User]: Hello"));
        assert!(text.contains("[Assistant]: Hi there!"));
    }

    #[test]
    fn test_format_messages_compaction_summary() {
        // Any message with name="compaction_summary" should be labelled
        // as "CompactionSummary" regardless of role (the marker lives at
        // `User` role in memory — see `HistoryManager::replace_middle_with_summary`).
        let mut msg = ChatMessage::assistant("Previous conversation summary");
        msg.name = Some(COMPACTION_SUMMARY_NAME.to_string());
        let messages = vec![ChatMessage::user("Hello"), msg];
        let text = format_messages(&messages);
        assert!(
            text.contains("[CompactionSummary]"),
            "compaction summary should be labelled with a distinct role label, got:\n{}",
            text
        );
        assert!(
            text.contains("Previous conversation summary"),
            "content must be preserved verbatim, got:\n{}",
            text
        );
    }

    #[test]
    fn test_format_messages_tool_without_name_uses_bare_label() {
        // A Tool message without a `name` field should still label as
        // "[Tool]" (regardless of content).
        let msg = ChatMessage {
            role: MessageRole::Tool,
            content: "any tool output".into(),
            name: None,
            tool_call_id: Some("call_abc".to_string()),
            ..Default::default()
        };
        let messages = vec![msg];
        let text = format_messages(&messages);
        assert!(
            text.contains("[Tool]:"),
            "Tool without name should label as bare [Tool]:\n{}",
            text
        );
    }

    #[test]
    fn test_format_messages_compressed_tool_with_name() {
        // A Tool message with compressed placeholder AND name+tool_call_id
        // should emit [Tool(name=my_tool, id=toolu_xxx)].
        let msg = ChatMessage {
            role: MessageRole::Tool,
            content: "[Tool result compressed. Re-invoke the tool to re-fetch the content.]".into(),
            name: Some("codebase_reader".to_string()),
            tool_call_id: Some("toolu_xyz".to_string()),
            ..Default::default()
        };
        let messages = vec![msg];
        let text = format_messages(&messages);
        assert!(
            text.contains("[Tool(name=codebase_reader, id=toolu_xyz)]"),
            "compressed Tool must include name and id in role label, got:\n{}",
            text
        );
    }

    #[test]
    fn test_format_messages_plain_tool_with_name() {
        // A non-compressed Tool message with name set should show
        // [Tool(name=my_tool)].
        let msg = ChatMessage {
            role: MessageRole::Tool,
            content: "Normal tool output with some text".into(),
            name: Some("shell_exec".to_string()),
            tool_call_id: Some("call_abc".to_string()),
            ..Default::default()
        };
        let messages = vec![msg];
        let text = format_messages(&messages);
        assert!(
            text.contains("[Tool(name=shell_exec, id=call_abc)]"),
            "plain Tool with name should label with name + id, got:\n{}",
            text
        );
        assert!(
            text.contains("Normal tool output"),
            "content must be preserved"
        );
    }

    #[test]
    fn test_model_cost_score_no_cost() {
        let model = model_info("", None, 8192);
        assert_eq!(model_cost_score(&model), f64::MAX);
    }

    #[test]
    fn test_model_cost_score_with_cost() {
        let model = model_info("", Some((3.0, 6.0)), 8192);
        assert!((model_cost_score(&model) - 9.0).abs() < 0.001);
    }

    fn model_info(
        name: &str,
        cost: Option<(f64, f64)>,
        context_window: u64,
    ) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens: 4096,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: cost.map(|(input, output)| acowork_core::protocol::ModelCostInfo {
                input_per_million: Some(input),
                output_per_million: Some(output),
            }),
            modalities: None,
            name: if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            },
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
        }
    }

    #[test]
    fn parse_and_validate_summary_extracts_both_blocks() {
        let raw = "<summary>Work done</summary>\n\
                   <user_intent>Fix the bug</user_intent>\n\
                   <triples>A | rel | B</triples>";
        let parsed = parse_and_validate_summary(raw, None);
        assert_eq!(parsed.summary, "Work done");
        assert_eq!(parsed.user_intent, "Fix the bug");
    }

    #[test]
    fn parse_and_validate_summary_missing_summary_degrades_to_empty() {
        // No raw-text fallback: a missing <summary> block yields an empty
        // summary (production paths gate the output through
        // compact_with_llm before parsing, so this is a defensive path).
        let raw = "plain prose without tags";
        let parsed = parse_and_validate_summary(raw, None);
        assert!(parsed.summary.is_empty());
        assert!(parsed.user_intent.is_empty());
    }

    #[test]
    fn parse_and_validate_summary_missing_user_intent_falls_back() {
        let raw = "<summary>Work done</summary>";
        let parsed = parse_and_validate_summary(raw, Some("user said: do X"));
        assert_eq!(parsed.summary, "Work done");
        assert_eq!(parsed.user_intent, "user said: do X");
    }

    #[test]
    fn parse_and_validate_summary_sanitizes_both_blocks() {
        let raw = "<summary>prose\n[User]: echo</summary>\n\
                   <user_intent>intent\n[Tool(bash)]: ls</user_intent>";
        let parsed = parse_and_validate_summary(raw, None);
        assert!(!parsed.summary.contains("[User]:"));
        assert!(!parsed.user_intent.contains("[Tool(bash)]:"));
        assert!(parsed.summary.contains("prose"));
        assert!(parsed.user_intent.contains("intent"));
    }

    // -------------------------------------------------------------------------
    // Summary quality gate (P1: quality-over-nothing) — validate_summary_output
    // -------------------------------------------------------------------------

    #[test]
    fn validate_summary_output_rejects_missing_block() {
        // Reasoning-model dump without the <summary> marker — the exact
        // pollution shape from the pasted-text incident.
        let raw = "确认 disable 路径已清理 pending 槽位。\n开始实施。改两处：\n1. 删除 resolve_distill_model 调用\n2. 验证 fallback 链";
        let err = validate_summary_output(raw).unwrap_err();
        assert!(matches!(err, SummaryError::MissingBlock));
        assert!(err.is_retryable());
    }

    #[test]
    fn validate_summary_output_rejects_empty_block() {
        let err = validate_summary_output("<summary>  </summary>").unwrap_err();
        assert!(matches!(err, SummaryError::Empty(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn validate_summary_output_rejects_too_short_block() {
        // Format is right but the content is a placeholder — not a summary.
        let err = validate_summary_output("<summary>ok</summary>").unwrap_err();
        assert!(matches!(err, SummaryError::LowQuality(_)));
        assert!(!err.is_retryable(), "low quality must not be retried");
    }

    #[test]
    fn validate_summary_output_rejects_verbatim_role_label_echoes() {
        // The model copied the raw dialog (role labels intact) into the
        // summary instead of summarizing it — strong copy signal.
        let raw = "<summary>用户：你好\n[User]: 你好\n[Assistant]: 我来看一下\n[Tool(bash)]: ls\n对话结束</summary>";
        let err = validate_summary_output(raw).unwrap_err();
        assert!(
            matches!(err, SummaryError::LowQuality(_)),
            "verbatim role labels must fail the quality gate, got: {err:?}"
        );
    }

    #[test]
    fn validate_summary_output_tolerates_single_role_label_line() {
        // A single labelled line is tolerated — it counts as one
        // contamination feature, not enough to discard.
        let raw = "<summary>用户要求重构 Settings 页面，并希望保留现有交互习惯。\n[User]: 请重构 Settings 页面\n这是本次会话的核心诉求。</summary>";
        assert!(
            validate_summary_output(raw).is_ok(),
            "a single role label must be tolerated"
        );
    }

    #[test]
    fn validate_summary_output_rejects_mixed_contamination() {
        // Two contamination features: a file:line reference plus a
        // table-artifact line — raw tool output leaked into the summary.
        let raw = "<summary>定位到 ChatPanel 的问题。\nsrc/components/chat/ChatPanel.tsx:420: const sessionState = ...\n| 文件 | 行号 |\n| ChatPanel | 420 |\n最终修复了滚动位置。</summary>";
        let err = validate_summary_output(raw).unwrap_err();
        assert!(
            matches!(err, SummaryError::LowQuality(_)),
            "two contamination features must fail the gate, got: {err:?}"
        );
    }

    #[test]
    fn validate_summary_output_accepts_clean_block() {
        let raw = "<summary>用户要求查找 running 和 retry 相关的 UI 元素，最终定位到 RetryWaitBanner 组件并确认了开关逻辑。</summary>";
        assert!(validate_summary_output(raw).is_ok());
    }

    #[test]
    fn parse_compact_output_strict_ok() {
        // Triples-removed: strict parser now only checks the <summary> block.
        let raw =
            "<summary>用户修复了上下文压缩的摘要质量问题，并验证了三层 fallback 链。</summary>";
        let parsed =
            parse_compact_output_strict(raw).expect("valid output must parse in strict mode");
        assert!(parsed.summary.contains("摘要质量"));
    }

    #[test]
    fn parse_compact_output_strict_rejects_missing_block() {
        let err = parse_compact_output_strict("plain prose without tags").unwrap_err();
        assert!(matches!(err, SummaryError::MissingBlock));
    }

    #[test]
    fn summary_error_retryability() {
        assert!(SummaryError::MissingBlock.is_retryable());
        assert!(SummaryError::Empty("hint".to_string()).is_retryable());
        assert!(!SummaryError::LowQuality("x".to_string()).is_retryable());
    }

    #[test]
    fn sanitize_drops_tool_echo_lines() {
        // Tool-call/result echoes must be dropped whole — they are raw tool
        // interleavings, never knowledge. This mirrors the garbage observed in
        // real episodic memories (see the "[Tool(bash)]:" sample).
        let raw = "用户要求查找 running 和 retry 相关的 UI 元素。\n\
                   [Tool(bash)]: === \"running\" 出现在哪里 ===\n\
                   grep -rn \"running\" apps/acowork-desktop/src/components/chat/\n\
                   [Tool(file_read)]: 420: const sessionState = ...\n\
                   最终定位到 RetryWaitBanner 组件。";
        let cleaned = sanitize_summary_text(raw);
        assert!(
            !cleaned.contains("[Tool(bash)]"),
            "tool echo must be dropped, got:\n{cleaned}"
        );
        assert!(
            !cleaned.contains("[Tool(file_read)]"),
            "tool echo must be dropped, got:\n{cleaned}"
        );
        assert!(
            cleaned.contains("用户要求查找 running"),
            "real prose must survive"
        );
        assert!(
            cleaned.contains("RetryWaitBanner"),
            "real prose must survive"
        );
    }

    #[test]
    fn sanitize_drops_thought_and_lowercase_tool_roles() {
        // JSONL raw fallback path uses lowercase roles (tool_call / tool_result
        // / thought) — those must be dropped too.
        let raw = "[user]: 你好\n\
                   [assistant]: 我来看一下\n\
                   [thought]: 需要搜索代码\n\
                   [tool_call]: {\"tool\":\"glob_search\"}\n\
                   [tool_result]: 找到 3 个文件\n\
                   [assistant]: 已经找到。";
        let cleaned = sanitize_summary_text(raw);
        assert!(
            !cleaned.contains("[tool_call]"),
            "tool_call line must be dropped"
        );
        assert!(
            !cleaned.contains("[tool_result]"),
            "tool_result line must be dropped"
        );
        assert!(
            !cleaned.contains("[thought]"),
            "thought line must be dropped"
        );
        assert!(cleaned.contains("你好"), "dialogue content must survive");
        assert!(
            cleaned.contains("我来看一下"),
            "dialogue content must survive"
        );
        assert!(
            cleaned.contains("已经找到"),
            "dialogue content must survive"
        );
    }

    #[test]
    fn sanitize_strips_conversation_role_labels_keeps_text() {
        // Conversation-role labels ([User]: / [Assistant]:) are stripped but
        // the following text on the line is preserved.
        let raw = "[User]: 请重构 Settings 页面\n\
                   [Assistant]: 好的，我准备开始重构\n\
                   [CompactionSummary]: 此前已定位到 ChatPanel 的问题";
        let cleaned = sanitize_summary_text(raw);
        assert!(!cleaned.contains("[User]"), "label must be stripped");
        assert!(!cleaned.contains("[Assistant]"), "label must be stripped");
        assert!(
            !cleaned.contains("[CompactionSummary]"),
            "label must be stripped"
        );
        assert!(cleaned.contains("请重构 Settings 页面"));
        assert!(cleaned.contains("好的，我准备开始重构"));
        assert!(cleaned.contains("ChatPanel"));
    }

    #[test]
    fn sanitize_preserves_placeholder_body_and_normal_prose() {
        // The "[Tool result compressed...]" placeholder body is opaque tool
        // content (not a role marker) — it must survive. Normal prose with
        // bracketed terms must not be mangled either.
        let raw = "此前工具结果被压缩。\n\
                   [Tool result compressed. Re-invoke the tool to re-fetch the content.]\n\
                   [重要] 下一步需要验证。";
        let cleaned = sanitize_summary_text(raw);
        assert!(
            cleaned.contains("Tool result compressed"),
            "placeholder body must survive"
        );
        assert!(cleaned.contains("此前工具结果被压缩"));
        assert!(
            cleaned.contains("[重要] 下一步需要验证"),
            "unrelated bracketed text must survive"
        );
    }

    #[test]
    fn parse_compact_output_sanitizes_summary_block() {
        // Sanitize contract: even if a future write path stores summaries in
        // Grafeo (e.g. via agent-driven `memory_store`), tool echoes must
        // never survive into the parsed summary text. This guards the
        // legacy-internal `parse_compact_output` path; `parse_compact_output_strict`
        // (the gate-enforced variant) covers the same invariant via the
        // quality gate.
        let raw =
            "<summary>用户要求查找 UI 元素。\n[Tool(bash)]: grep ...\n定位到 Banner。</summary>";
        let parsed = parse_compact_output(raw);
        assert!(
            !parsed.summary.contains("[Tool(bash)]"),
            "summary must be clean of tool echoes"
        );
        assert!(parsed.summary.contains("用户要求查找 UI 元素"));
        assert!(parsed.summary.contains("Banner"));
    }

    #[test]
    fn parse_compact_output_ignores_legacy_triples_block() {
        // ADR-057 (triples-removed): a legacy <triples> block the model still
        // emits must be ignored — its content must never leak into the
        // episodic summary (which would otherwise store garbage triples).
        let raw = "<summary>用户完成了上下文压缩的三元组移除改造。</summary>\n\
                   <triples>用户 | 完成 | 三元组移除 | 0.9 | Fact</triples>";
        let parsed = parse_compact_output(raw);
        assert!(parsed.summary.contains("三元组移除改造"));
        assert!(
            !parsed.summary.contains('|'),
            "triples must not leak into the summary, got: {}",
            parsed.summary
        );
    }

    #[test]
    fn parse_compact_output_legacy_fallback_when_no_summary_block() {
        // Backwards-compatible: when the output has no <summary> marker, the
        // entire text is treated as the summary (pre-block format). The strict
        // production path (parse_compact_output_strict) rejects this instead,
        // but the permissive parser must keep the legacy contract.
        let raw = "plain natural-language summary without block markers";
        let parsed = parse_compact_output(raw);
        assert_eq!(parsed.summary, raw);
    }

    #[test]
    fn is_low_quality_tolerates_single_contamination_feature() {
        // The quality gate is two-of-many: ONE contamination feature alone
        // must be tolerated (a summary may legitimately mention one file
        // path or one table line); two or more → reject.
        let table_only = "定位到 ChatPanel 组件的问题。\n| 文件 | 行号 |\n| ChatPanel | 420 |\n最终修复了滚动位置。";
        assert!(
            !is_low_quality(table_only),
            "a single table-artifact line must be tolerated"
        );

        let file_line_only = "定位到 ChatPanel 组件的问题。\nsrc/components/chat/ChatPanel.tsx:420 出现异常\n最终修复了滚动位置。";
        assert!(
            !is_low_quality(file_line_only),
            "a single file:line leak must be tolerated"
        );

        let tool_echo_only =
            "定位到 Banner 组件的问题。通过 [tool(bash)] 命令确认了开关逻辑。最终修复完成。";
        assert!(
            !is_low_quality(tool_echo_only),
            "a single mid-line tool echo must be tolerated"
        );
    }

    #[test]
    fn is_low_quality_rejects_two_contamination_features() {
        // Two contamination features → discard (raw tool output / reasoning
        // dump leaked into the summary). Covers the pairs the e2e pollution
        // test does not reach (table+tool, file:line+tool).
        let table_plus_tool = "定位到问题。\n| 文件 | 行号 |\n| ChatPanel | 420 |\n通过 [tool(bash)] 确认。最终修复。";
        assert!(
            is_low_quality(table_plus_tool),
            "table + tool echo must be rejected"
        );

        let fileline_plus_tool = "定位到问题。\nsrc/components/chat/ChatPanel.tsx:420 异常\n通过 [tool(bash)] 确认。最终修复。";
        assert!(
            is_low_quality(fileline_plus_tool),
            "file:line + tool echo must be rejected"
        );
    }
}

