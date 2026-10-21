//! Conversation history management (FIFO trimming + Sanitization + Emergency trim)
//!
//! Adapted from zeroclaw/src/agent/history.rs
//! ACowork deviation: uses acowork-core ChatMessage types; token estimation
//! uses char-based approximation instead of tiktoken.
//! SPDX-License-Identifier: MIT OR Apache-2.0
//!
//! ## Design note (2026-05-28)
//!
//! Programmatic folding strategies (Tool Result folding, content folding) have been
//! removed per [ADR-010](../../../../docs/adr/ADR-010-context-compression-simplification.md).
//! Context compression is a semantic understanding task — only an LLM can reliably
//! decide what to discard. Per ADR-061, the FIFO safety nets (trim_fifo,
//! emergency_trim) are deleted too: the ADR-061 v3 5-level plan (level 5 floor) covers every
//! within-budget case, and LLM failure is an explicit `ChunkEvent::Error` (§11.3).

use std::collections::HashSet;
use std::sync::Arc;

use acowork_core::protocol::ProtocolType;
#[cfg(test)]
use acowork_core::providers::traits::ChatRequest;
use acowork_core::providers::traits::{ChatMessage, MessageRole, Provider, ToolCall};

use crate::agent::compression_constants::SUMMARY_TOKEN_BUDGET;
use crate::error::RuntimeError;
use crate::token::counter::TokenCounter;

// ── ADR-061 v3 placeholder constants (revived from ADR-052 stubs) ─────
//
// `COMPRESSED_TOOL_PLACEHOLDER_PREFIX` was originally introduced for the
// `context_abandon` / `context_retrieve` tool pair (ADR-052). ADR-061 v2
// retired those tools and the placeholder machinery alongside them.
//
// ADR-061 v3 (§10.2 / §20) revives the placeholder machinery as an
// **internal** `HistoryManager` API (`abandon_tool_result` /
// `clear_round`) used by the 5-level compression plan to fold tool
// results in-place. The placeholder is **not** surfaced to the LLM as
// a retrievable artifact — `context_retrieve` remains deprecated — so
// the wording tells the LLM to re-invoke the tool if it needs the
// result back.

/// Stable prefix used by `abandon_tool_result` and `clear_round` to
/// mark a tool result that has been folded into the compaction summary.
///
/// Detection: a `Tool` message whose `content` starts with this prefix
/// is considered already-folded. Repeated folding is a no-op.
pub(crate) const COMPRESSED_TOOL_PLACEHOLDER_PREFIX: &str = "--- compressed: tool=";

/// Build the placeholder content for a folded tool result.
///
/// Format: `--- compressed: tool=<name> result reclaimed, re-invoke to re-fetch --- `
///
/// Output length is bounded (worst case ≈ 90 bytes for a 40-char tool
/// name), so calling this on an already-tiny tool result is still a
/// no-op-equivalent for token accounting.
pub(crate) fn make_compressed_placeholder(tool_name: &str) -> String {
    format!(
        "{}{} result reclaimed, re-invoke to re-fetch --- ",
        COMPRESSED_TOOL_PLACEHOLDER_PREFIX, tool_name
    )
}

/// Stable identifier string used by [`HistoryManager::replace_middle_with_summary`]
/// to mark the synthetic Assistant message that replaces the compacted middle.
/// Detected by `format_messages` to label these as "CompactionSummary"
/// (rather than "Assistant") in the summary prompt, so the LLM knows
/// it is reading a previous compaction output rather than a fresh turn.
pub(crate) const COMPACTION_SUMMARY_NAME: &str = "compaction_summary";

// ── ADR-061 v3: 5-level round-atomic compression types ─────────────────────

/// ADR-061: error from planning / applying a 5-level compression.
#[derive(Debug)]
pub(crate) enum CompressError {
    /// The plan failed its acceptance check at apply time (defensive;
    /// plan-time validation normally catches this first).
    InsufficientCompression { projected_ratio: f64 },
    /// Level 5 (the sole budget-only floor) still cannot fit the
    /// summary + retained skeleton within the budget (§19.5).
    UnrecoverableOverflow { projected: u64, budget: u64 },
}

impl std::fmt::Display for CompressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientCompression { projected_ratio } => write!(
                f,
                "compression plan failed acceptance check (projected ratio {:.1}%)",
                projected_ratio * 100.0
            ),
            Self::UnrecoverableOverflow { projected, budget } => write!(
                f,
                "level 5 (summary-only floor) cannot fit within budget: projected {} > budget {}",
                projected, budget
            ),
        }
    }
}

impl std::error::Error for CompressError {}

/// ADR-061: result of a successful 5-level compression apply.
#[derive(Debug, Clone)]
pub(crate) struct CompressionOutcome {
    pub level: u8,
    pub original_tokens: u64,
    pub new_tokens: u64,
    pub compression_ratio: f64,
    pub removed_messages: usize,
}

/// ADR-061: retention statistics per level — persisted inside the summary
/// marker's level-metadata block (§9) for post-hoc debugging.
#[derive(Debug, Clone)]
struct RetentionStats {
    user_messages: usize,
    assistant_messages: usize,
    tool_messages: usize,
    user_desc: String,
    assistant_desc: String,
    tool_desc: String,
}

/// ADR-061: a 5-level round-atomic compression plan (levels 1-5).
///
/// Captures exactly which messages survive at the chosen level (system +
/// user skeleton + selected assistant/tool tail) plus the token projection.
/// Created by [`HistoryManager::plan_compression`], consumed by
/// [`HistoryManager::apply_compression`].
#[derive(Debug)]
pub(crate) struct CompressionPlan {
    /// Selected level (1-5).
    pub level: u8,
    /// Retained messages in original order (excluding the new summary
    /// marker).
    retained: Vec<ChatMessage>,
    /// History token count at plan time.
    original_tokens: u64,
    /// Estimated tokens of `retained` (via `count_message`).
    retained_tokens: u64,
    /// Projected total = retained + summary marker (exact: the summary
    /// size is already known at plan time, §19.1).
    projected_tokens: u64,
    /// Retention stats for the level metadata block.
    stats: RetentionStats,
}

/// Build the summary marker content: level metadata block (§9) prepended
/// to the LLM summary. Metadata is runtime-generated (not LLM output) and
/// machine-parseable.
fn build_summary_marker(
    level: u8,
    original_tokens: u64,
    new_tokens: u64,
    ratio: f64,
    stats: &RetentionStats,
    summary: &str,
) -> String {
    format!(
        "[compressed: level={}]\n  user_messages: {} ({})\n  assistant_messages: {} ({})\n  tool_messages: {} ({})\n  tokens: {} -> {} (ratio {:.1}%)\n\n{}",
        level,
        stats.user_desc,
        stats.user_messages,
        stats.assistant_desc,
        stats.assistant_messages,
        stats.tool_desc,
        stats.tool_messages,
        original_tokens,
        new_tokens,
        ratio * 100.0,
        summary
    )
}

/// History manager for conversation
pub struct HistoryManager {
    /// Conversation messages.
    ///
    /// ADR-054: held behind `Arc` so debug context snapshots can retain a
    /// shallow reference (`Arc::clone`) to the exact history of their build
    /// iteration instead of deep-copying the whole conversation per iteration.
    /// All mutations go through `Arc::make_mut` (copy-on-write): when no
    /// snapshot holds the buffer (non-debug mode) mutations are in-place
    /// zero-copy; when snapshots share it, the first mutation after a
    /// snapshot clones once and subsequent mutations reuse that buffer.
    messages: Arc<Vec<ChatMessage>>,
    /// Maximum token budget for history
    max_tokens: u64,
    /// Current estimated token count for the conversation prompt.
    ///
    /// Initially tracks conversation history tokens only (via `count_message`).
    /// After each LLM call, [`calibrate_from_usage`] replaces this with the
    /// API-reported `prompt_tokens` (which includes system prompt), preventing
    /// cumulative estimation drift across turns.
    current_tokens: u64,
    /// LLM protocol type for image token estimation.
    /// Defaults to OpenAI; set via `set_protocol_type()` after construction.
    protocol_type: ProtocolType,
    /// Tiered token counter for unified token estimation.
    counter: TokenCounter,
    /// ADR-060 v2 §5.4: tool_call `id`s of rounds already spliced after a
    /// compaction marker by [`Self::inject_round_after_marker`].
    ///
    /// Used to enforce JSONL-side idempotency on consecutive compressions —
    /// if the next compression finds a round again here (because level 8
    /// already stripped our previous splice), we skip re-injecting and
    /// re-persisting it to avoid duplicate synthetic rows on disk.
    ///
    /// A *set*, not the former single `Option<String>`: skill-body
    /// preservation recalls one round per distinct skill, so a single
    /// compression can inject N rounds and each must be individually
    /// remembered. Widening it also fixes a latent hole in the single-slot
    /// version, where injecting round B silently forgot that round A had
    /// already been persisted.
    injected_round_call_ids: HashSet<String>,
    /// ADR-067: byte length of `serde_json::to_string(&self.messages)` —
    /// i.e. the exact JSON array serialization of the whole history.
    ///
    /// Maintained incrementally on the hot path (`append` / `extend` are
    /// O(1) per message) and recomputed on the rare structural operations
    /// (`load_restored`, `clear`, `truncate_to`).
    /// This lets the always-on context-usage observability path report
    /// the `messages` section byte size **without** re-serializing the
    /// entire history on every LLM call.
    messages_json_bytes: usize,
    /// Byte length of all `load_skill` Tool messages currently in history
    /// — i.e. the full `SKILL.md` bodies the LLM pulled on demand and that
    /// still occupy the context window.
    ///
    /// Same accounting contract as [`Self::messages_json_bytes`] (O(1)
    /// incremental on `append`/`extend`, full recompute at every
    /// structural-mutation site via [`Self::recompute_byte_counters`]).
    /// They MUST stay reconciled through the same single method: the two
    /// counters are recomputed from the same buffer, and having two
    /// separate recompute calls that can be independently forgotten is
    /// exactly the `clear_round` bug class (see the note on
    /// [`Self::recompute_byte_counters`]).
    ///
    /// Deliberately **not** deduplicated by skill name: loading the same
    /// skill twice puts two copies of the body in the prompt, so it costs
    /// twice. This counter answers "how many bytes of skill body are in
    /// context right now", not "how many skills were used".
    skills_loaded_bytes: usize,
}

// ── ADR-061 v3 §20.3: round-primitive report types ────────────────────

/// Report from [`HistoryManager::clear_round`].
// ADR-061 v3 §20.3 report contract: exercised by the unit suite;
// the compression-plan consumer is not wired in yet.
#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
pub(crate) struct ClearRoundReport {
    /// Tool call ids that were folded (idempotent calls contribute empty).
    pub cleared_tool_ids: Vec<String>,
    /// Total bytes reclaimed (sum of original content lengths).
    pub bytes_reclaimed: usize,
}

/// Report from [`HistoryManager::fix_round`].
// ADR-061 v3 §20.3 report contract: exercised by the unit suite;
// the compression-plan consumer is not wired in yet.
#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
pub(crate) struct FixReport {
    /// Same shape as `ClearRoundReport`.
    pub cleared: ClearRoundReport,
    /// Tool messages removed because their tool_call_id had no
    /// matching assistant.tool_calls entry.
    pub removed_orphan_tool_messages: usize,
    /// Tool-call ids removed from the assistant's tool_calls array
    /// because no matching Tool message existed.
    pub removed_orphan_tool_call_ids: Vec<String>,
}

/// Result of [`HistoryManager::recall_todo_round`].
// ADR-061 v3 §20.3 report contract: exercised by the unit suite;
// the compression-plan consumer is not wired in yet.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum RecallResult {
    Injected { todo_call_id: String },
    SkippedAlreadyInjected,
    SkippedAlreadyInTail,
    NoMarker,
    NoTodoRoundFound,
}

impl HistoryManager {
    /// Byte length of the empty JSON array `[]` — the baseline for
    /// [`Self::messages_json_bytes`] so the counter always equals
    /// `serde_json::to_string(&self.messages).len()` (even when empty).
    const EMPTY_JSON_ARRAY_BYTES: usize = 2;

    /// Create new history manager with token budget.
    pub fn new(max_tokens: u64) -> Self {
        Self {
            messages: Arc::new(Vec::new()),
            max_tokens,
            current_tokens: 0,
            protocol_type: ProtocolType::default(),
            counter: TokenCounter::new(),
            injected_round_call_ids: HashSet::new(),
            messages_json_bytes: Self::EMPTY_JSON_ARRAY_BYTES,
            skills_loaded_bytes: 0,
        }
    }

    /// Byte length of the JSON array serialization of the whole history
    /// (`serde_json::to_string(&self.messages)`). O(1) — maintained
    /// incrementally by [`Self::append`] / [`Self::extend`].
    ///
    /// ADR-067: the always-on context-usage path uses this instead of
    /// re-serializing the history on every LLM call.
    pub fn messages_json_bytes(&self) -> usize {
        self.messages_json_bytes
    }

    /// Byte length of the `load_skill` Tool messages currently in history
    /// (the `SKILL.md` bodies still occupying the context window). O(1) —
    /// maintained incrementally by [`Self::append`] / [`Self::extend`].
    ///
    /// Feeds the `skills_loaded` context-usage section, which the frontend
    /// folds into the *skills* category alongside the always-present
    /// Progressive-mode summary (`skill_instructions`).
    pub fn skills_loaded_bytes(&self) -> usize {
        self.skills_loaded_bytes
    }

    /// True when `msg` is the Tool result of a `load_skill` call — i.e. its
    /// `content` is a full `SKILL.md` body sitting in the context window.
    ///
    /// The `name` field is stamped by the agent loop when it appends tool
    /// results (`loop_.rs`, `ChatMessage { name: Some(tc.function.name…) }`),
    /// so this is a reliable discriminator and costs one string compare.
    fn is_skill_load_msg(msg: &ChatMessage) -> bool {
        matches!(msg.role, MessageRole::Tool)
            && msg.name.as_deref() == Some(crate::tools::builtin::load_skill::TOOL_NAME)
    }

    /// Recompute BOTH byte counters from scratch (O(n) scan, plus a full
    /// serialization for `messages_json_bytes`).
    ///
    /// Used by the rare structural mutations that don't go through
    /// [`Self::append`] / [`Self::extend`]. The two counters are folded into
    /// one method on purpose: every structural site must refresh both, and
    /// the `clear_round` precedent (folds Tool content, refreshes nothing)
    /// shows that separate recompute calls drift. If a future site only
    /// needs one of them, it still calls this.
    fn recompute_byte_counters(&mut self) {
        self.messages_json_bytes = serde_json::to_string(&*self.messages)
            .map(|s| s.len())
            .unwrap_or(Self::EMPTY_JSON_ARRAY_BYTES);
        self.skills_loaded_bytes = self
            .messages
            .iter()
            .filter(|m| Self::is_skill_load_msg(m))
            .map(Self::single_json_bytes)
            .sum();
    }

    /// Byte length of a single message's JSON serialization.
    fn single_json_bytes(msg: &ChatMessage) -> usize {
        serde_json::to_string(msg).map(|s| s.len()).unwrap_or(0)
    }

    /// Set the LLM protocol type for image token estimation.
    pub fn set_protocol_type(&mut self, pt: ProtocolType) {
        self.protocol_type = pt;
    }

    /// Get the current session chars/token ratio.
    /// Returns `None` if no calibration has occurred yet (callers fall back
    /// to [`crate::token::counter::DEFAULT_RATIO`]).
    pub fn model_ratio(&self) -> Option<f64> {
        self.counter.is_calibrated().then(|| self.counter.ratio())
    }

    /// Restore the token-counting "scene" from persisted session meta:
    /// the last API-counted input tokens (the authoritative anchor for the
    /// resumed history) and the last calibrated ratio. Missing values fall
    /// back to the default ratio / recomputed estimate — the JSONL replay
    /// in [`Self::load_restored`] already produced a sane estimate.
    ///
    /// Why the `last_input` anchor matters: `current_tokens` is, in a live
    /// session, the provider-counted `prompt_tokens` of the WHOLE prompt
    /// (history + system + tool schemas — see [`Self::calibrate_from_usage`]),
    /// and every trim/compaction threshold compares against it. The JSONL
    /// replay can only produce a messages-only estimate, which is strictly
    /// smaller than the real prompt, so seeding `current_tokens` from the
    /// persisted `last_input` is what keeps the very FIRST post-resume
    /// request measured on the same 口径 as a live session. Without it a
    /// resumed oversized session slips past the compaction threshold and
    /// the provider rejects the first request.
    pub fn restore_anchor(&mut self, last_input_tokens: Option<u64>, ratio: Option<f64>) {
        if let Some(ratio) = ratio {
            self.counter.set_ratio(ratio);
        }
        if let Some(tokens) = last_input_tokens {
            self.current_tokens = tokens;
            tracing::info!(
                tokens,
                ratio = self.counter.ratio(),
                "HistoryManager: anchored restored history to last API-counted input"
            );
        }
    }

    /// Dynamically update the max token budget for FIFO trimming.
    ///
    /// This should be called whenever the model changes (session creation,
    /// model switch), so that [`trim_fifo`] uses the correct
    /// [`ModelCapabilitiesInfo::effective_input_budget`] instead of
    /// the static config default.
    pub fn set_max_tokens(&mut self, max_tokens: u64) {
        tracing::info!(
            old = self.max_tokens,
            new = max_tokens,
            "HistoryManager max_tokens updated"
        );
        self.max_tokens = max_tokens;
    }

    /// Current effective history token budget. This is the model-calibrated
    /// value (via [`Self::set_max_tokens`]), NOT the static config default —
    /// callers that log or compare budgets must use this to stay consistent
    /// with the trim/compaction thresholds.
    pub fn max_tokens(&self) -> u64 {
        self.max_tokens
    }

    /// Get the current protocol type.
    pub fn protocol_type(&self) -> &ProtocolType {
        &self.protocol_type
    }

    /// Get reference to messages
    pub fn messages(&self) -> &[ChatMessage] {
        self.messages.as_slice()
    }

    /// Get an `Arc` clone of the messages (O(1), no copy).
    ///
    /// ADR-054: debug context snapshots hold this shallow reference so
    /// `messages_by_iteration` shares the underlying buffer instead of
    /// deep-copying per iteration. Mutations after this clone go through
    /// [`Arc::make_mut`], so the clone retains the exact history as of the
    /// snapshot's build iteration.
    pub fn messages_arc(&self) -> Arc<Vec<ChatMessage>> {
        Arc::clone(&self.messages)
    }

    /// Get mutable reference to messages (copy-on-write).
    ///
    /// When other `Arc` clones exist (e.g. a debug snapshot), this clones
    /// the buffer once; subsequent mutations reuse that buffer. When this
    /// is the sole owner, the buffer is mutated in place with zero copies.
    pub fn messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        Arc::make_mut(&mut self.messages)
    }

    /// Get current estimated token count
    pub fn token_count(&self) -> u64 {
        self.current_tokens
    }

    /// Calibrate the history token count from actual API usage feedback.
    ///
    /// LLM API responses include `usage.prompt_tokens` which is the authoritative
    /// token count for the entire prompt (system + history + tool definitions).
    /// This method:
    /// 1. Replaces our heuristic estimate with the API ground truth for budget tracking
    /// 2. Computes the chars/token ratio and feeds it back into TokenCounter
    ///
    /// ## Calibration formula
    ///
    /// ```text
    /// ratio = total_input_chars / prompt_tokens
    /// ```
    ///
    /// Both `total_input_chars` and `prompt_tokens` represent the same LLM request
    /// payload — they are **same-source** (分子分母同源), avoiding the calibration
    /// distortion that plagued previous versions.
    ///
    /// ## Safety
    ///
    /// When `prompt_tokens` is 0, the API response is considered unreliable
    /// (observed with some Anthropic-protocol providers like MiniMax that
    /// occasionally omit `message_start` usage fields). Calibration is skipped
    /// entirely to prevent corrupting the ratio with a bogus value.
    pub fn calibrate_from_usage(&mut self, prompt_tokens: u64, total_input_chars: usize) {
        if prompt_tokens == 0 {
            tracing::warn!(
                current_tokens = self.current_tokens,
                "Skipping calibration: API returned prompt_tokens=0 (unreliable usage data)"
            );
            return;
        }

        // Store API ground truth for budget tracking: `prompt_tokens` covers
        // the WHOLE request (history + system prompt + tool schemas) as the
        // provider counts it — this is the real anchor for the next turn's
        // projection. New messages appended afterwards are estimated as
        // `chars / ratio` on top of it.
        let prior = self.current_tokens;
        self.current_tokens = prompt_tokens;

        // Calibrate the session chars/token ratio from same-source data.
        // Both total_input_chars and prompt_tokens represent the same LLM
        // request, so the computed ratio is a precise measurement of the
        // model's chars/token. Direct replace (no EMA) — the live value has
        // bounded local error that only propagates to the next turn's new
        // input, never to already-counted history.
        if total_input_chars > 500 && prompt_tokens > 500 {
            let ratio = total_input_chars as f64 / prompt_tokens as f64;
            self.counter.set_ratio(ratio);
            tracing::info!(ratio, "Session token ratio calibrated from API usage");
        }

        tracing::debug!(
            prior,
            api = prompt_tokens,
            total_input_chars,
            delta = prompt_tokens as i64 - prior as i64,
            "History token count calibrated from API usage"
        );
    }

    /// Append a message to history
    pub fn append(&mut self, message: ChatMessage) {
        let tokens = self
            .counter
            .count_message(&message, Some(&self.protocol_type));
        self.current_tokens += tokens;
        let was_empty = self.messages.is_empty();
        let msg_bytes = Self::single_json_bytes(&message);
        let is_skill_load = Self::is_skill_load_msg(&message);
        self.messages_mut().push(message);
        // ADR-067: `[]` → `[a]` adds json(a); `[...,a]` adds json(a) + 1 comma.
        self.messages_json_bytes += if was_empty { msg_bytes } else { msg_bytes + 1 };
        if is_skill_load {
            self.skills_loaded_bytes += msg_bytes;
        }
    }

    /// Append multiple messages
    pub fn extend(&mut self, messages: Vec<ChatMessage>) {
        for msg in &messages {
            self.current_tokens += self.counter.count_message(msg, Some(&self.protocol_type));
        }
        let prev_len = self.messages.len();
        let extra_bytes: usize = messages.iter().map(Self::single_json_bytes).sum();
        let skill_bytes: usize = messages
            .iter()
            .filter(|m| Self::is_skill_load_msg(m))
            .map(Self::single_json_bytes)
            .sum();
        let separators = if prev_len == 0 {
            // `[]` → `[a,b]` adds (n-1) commas
            messages.len().saturating_sub(1)
        } else {
            // `[...,a,b]` adds one leading comma per appended message
            messages.len()
        };
        self.messages_mut().extend(messages);
        self.messages_json_bytes += extra_bytes + separators;
        self.skills_loaded_bytes += skill_bytes;
    }

    /// Bulk-load a pre-built message sequence from session resume.
    ///
    /// Replaces any existing messages and recomputes the token count once.
    /// Used by [`crate::agent::session::restorer`] to install the JSONL-derived
    /// history before the session starts processing new inbound messages.
    ///
    /// Unlike [`Self::append`], this is intended for trusted, already-sanitized
    /// input (the restorer guarantees tool_call/tool_result pairing and
    /// system/compaction-marker ordering invariants).
    ///
    /// `injected_round_call_ids` are the tool_call_ids of every *synthetic*
    /// round found in the JSONL (see [`RestoreOutcome::injected_round_call_ids`]).
    /// It restores the ADR-060 v2 §5.4 idempotency keys across process
    /// restarts: without them a compression after restart would re-inject
    /// rounds that a prior compaction already persisted, duplicating
    /// synthetic rows on disk.
    pub fn load_restored(
        &mut self,
        messages: Vec<ChatMessage>,
        injected_round_call_ids: HashSet<String>,
    ) {
        self.messages = Arc::new(messages);
        self.injected_round_call_ids = injected_round_call_ids;
        self.current_tokens = self
            .messages
            .iter()
            .map(|m| self.counter.count_message(m, Some(&self.protocol_type)))
            .sum();
        self.recompute_byte_counters();
        tracing::info!(
            count = self.messages.len(),
            tokens = self.current_tokens,
            injected_rounds = self.injected_round_call_ids.len(),
            "HistoryManager: loaded restored history"
        );
    }

    /// Clear all messages
    pub fn clear(&mut self) {
        self.messages = Arc::new(Vec::new());
        self.current_tokens = 0;
        self.messages_json_bytes = Self::EMPTY_JSON_ARRAY_BYTES;
        self.skills_loaded_bytes = 0;
    }

    /// Get message count
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Check if history is empty
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Truncate history to the specified number of messages.
    ///
    /// Keeps only the first `target_len` messages and recalculates
    /// the token count. Used by debug rewind to roll back history
    /// to a specific conversation snapshot.
    pub fn truncate_to(&mut self, target_len: usize) {
        if target_len >= self.messages.len() {
            return;
        }
        self.messages_mut().truncate(target_len);
        // Recalculate token count
        self.current_tokens = self
            .messages
            .iter()
            .map(|m| self.counter.count_message(m, Some(&self.protocol_type)))
            .sum();
        self.recompute_byte_counters();
        tracing::info!(
            target_len,
            new_token_count = self.current_tokens,
            "History truncated for debug rewind"
        );
    }

    /// Estimate total tokens for all messages (for pre-check)
    pub fn estimate_total_tokens(&self) -> u64 {
        self.current_tokens
    }

    /// Recompute `current_tokens` from scratch.
    ///
    /// Must be called after any in-place content mutation (e.g. summary
    /// replacement) since these mutate content in place but cannot
    /// update `current_tokens` under borrow rules. O(N) over messages
    /// with constant-time token estimation each.
    pub fn recalibrate_tokens(&mut self) {
        let pt = self.protocol_type.clone();
        let mut total = 0u64;
        for msg in self.messages.iter() {
            total = total.saturating_add(self.counter.count_message(msg, Some(&pt)));
        }
        self.current_tokens = total;
    }

    /// Sanitize message history to remove or fix corrupted entries.
    ///
    /// This prevents LLM 400 errors caused by invalid tool_call data when
    /// conversation history is replayed after an agent restart.
    ///
    /// Cleaning rules (applied in order):
    /// 1. Fix invalid tool_call arguments — replace non-JSON with `{}`
    /// 2. Remove orphaned tool result messages — no matching tool_call
    /// 3. Remove orphaned tool_calls — no matching tool result
    /// 4. Remove empty assistant messages — no content and no tool_calls
    /// 5. Remove non-first system messages — some LLM providers only allow
    ///    system role at the first position (e.g. MiniMax)
    ///
    /// This method is idempotent: calling it multiple times produces the same result.
    pub fn sanitize_messages(messages: &mut Vec<ChatMessage>) {
        // Step 1: Fix invalid tool_call arguments
        for msg in messages.iter_mut() {
            if let Some(ref mut tool_calls) = msg.tool_calls {
                for tc in tool_calls.iter_mut() {
                    if serde_json::from_str::<serde_json::Value>(&tc.function.arguments).is_err() {
                        tracing::warn!(
                            tool_call_id = %tc.id,
                            tool_name = %tc.function.name,
                            invalid_args = %tc.function.arguments,
                            "Sanitizing invalid tool_call arguments to empty object"
                        );
                        tc.function.arguments = "{}".to_string();
                    }
                }
            }
        }

        // Step 2: Collect valid tool_call_ids from assistant messages
        let valid_tool_call_ids: HashSet<String> = messages
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .flat_map(|tcs| tcs.iter().map(|tc| tc.id.clone()))
            .collect();

        // Step 3: Remove orphaned tool result messages
        messages.retain(|msg| {
            if msg.role == MessageRole::Tool
                && let Some(ref tcid) = msg.tool_call_id
                && !valid_tool_call_ids.contains(tcid)
            {
                tracing::warn!(
                    tool_call_id = %tcid,
                    "Removing orphaned tool result message"
                );
                return false;
            }
            true
        });

        // Step 4: Collect tool result IDs to find orphaned tool_calls
        let tool_result_ids: HashSet<String> = messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .filter_map(|m| m.tool_call_id.clone())
            .collect();

        // Remove tool_calls without corresponding tool results
        for msg in messages.iter_mut() {
            if let Some(ref mut tool_calls) = msg.tool_calls {
                let before = tool_calls.len();
                tool_calls.retain(|tc| {
                    if !tool_result_ids.contains(&tc.id) {
                        tracing::warn!(
                            tool_call_id = %tc.id,
                            tool_name = %tc.function.name,
                            "Removing tool_call without corresponding result"
                        );
                        return false;
                    }
                    true
                });
                // If all tool_calls were removed, clear the field
                if tool_calls.is_empty() && before > 0 {
                    msg.tool_calls = None;
                }
            }
        }

        // Step 5: Remove empty assistant messages (no content + no tool_calls)
        messages.retain(|msg| {
            if msg.role == MessageRole::Assistant {
                let has_content = !msg.content.is_empty();
                let has_tool_calls = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());
                if !has_content && !has_tool_calls {
                    tracing::warn!("Removing empty assistant message");
                    return false;
                }
            }
            true
        });

        // Step 6: Remove system messages that are not at position 0
        // Some LLM providers only allow system role at the first position.
        let before_len = messages.len();
        let mut first_system_seen = false;
        messages.retain(|m| {
            if matches!(m.role, MessageRole::System) {
                if !first_system_seen {
                    first_system_seen = true;
                    true
                } else {
                    tracing::warn!(
                        content_preview = %m.content.chars().take(80).collect::<String>(),
                        "sanitize: removing non-first system message"
                    );
                    false
                }
            } else {
                true
            }
        });
        if messages.len() < before_len {
            tracing::warn!(
                removed = before_len - messages.len(),
                "sanitize: removed non-first system messages"
            );
        }
    }

    // ── Compaction methods (ADR-011: 摘要即蒸馏) ─────────────────────

    /// Compact full conversation history into a natural-language summary
    /// via LLM. Used at 80% token usage threshold (context compaction).
    ///
    /// Formats all messages as text, wraps them in the COMPACT_PROMPT
    /// template, and sends to the configured Compact Model.
    /// Returns the plain-text summary (no JSON parsing).
    ///
    /// `identity_context` is the user's `UserProfile` formatted as text
    /// (see [`super::session::session_manager::format_user_profile_context`]).
    /// When `Some`, it is embedded into the system prompt so the LLM writes
    /// the summary in the user's preferred language. Pass `None` when the
    /// session has no user profile yet (default → English summary).
    ///
    /// Returns `(summary, usage)` per ADR-027 so callers can record raw
    /// Provider usage in [`crate::conversation::SessionTokens`].
    pub async fn compact_via_llm(
        &self,
        provider: &dyn Provider,
        model_name: &str,
        system_prompt: &str,
        identity_context: Option<&str>,
    ) -> std::result::Result<(String, acowork_core::providers::traits::UsageInfo), RuntimeError>
    {
        let messages_text = crate::episode_distill::format_messages(self.messages.as_slice());
        if messages_text.is_empty() {
            return Err(RuntimeError::Tool(
                "Cannot compact empty history".to_string(),
            ));
        }

        let prompt = crate::prompt::COMPACT_PROMPT.replace("{messages_text}", &messages_text);
        crate::episode_distill::compact_with_llm(
            &prompt,
            provider,
            model_name,
            SUMMARY_TOKEN_BUDGET as u32,
            identity_context,
            system_prompt,
        )
        .await
    }

    // ── ADR-061 v3 §20.3.1: tool-result folding primitives ─────────────
    //
    // PR1 revives `abandon_tool_result` from ADR-052 stubs. It is the
    // single-message primitive behind `clear_round` (PR2): replace one
    // `Tool` message's content with a placeholder, leaving role / id /
    // schema untouched.
    //
    // Invariants (pre & post):
    // - Only `role == Tool` messages with matching `tool_call_id` are
    //   modified. Assistant messages, other tool messages, system /
    //   user / summary messages are never touched.
    // - Already-folded messages (content starts with
    //   `COMPRESSED_TOOL_PLACEHOLDER_PREFIX`) are skipped — idempotent.
    // - `messages.len()` does not change. Schema (assistant.tool_calls[*].id
    //   ↔ tool.tool_call_id) is preserved.
    // - Caller is responsible for `recalibrate_tokens()` after a batch of
    //   folds; this method only mutates `content` bytes.
    //
    // Returns: number of tool messages whose content was actually
    // replaced (0 if not found or already folded).
    pub fn abandon_tool_result(&mut self, tool_call_id: &str) -> usize {
        let mut replaced = 0usize;
        // `self.messages` is `Arc<Vec<ChatMessage>>` (COW); use Arc::make_mut
        // so the mutation propagates to existing snapshots.
        let msgs = Arc::make_mut(&mut self.messages);
        for msg in msgs.iter_mut() {
            if !matches!(msg.role, MessageRole::Tool) {
                continue;
            }
            let Some(ref tcid) = msg.tool_call_id else {
                continue;
            };
            if tcid != tool_call_id {
                continue;
            }
            // Idempotency: skip already-folded messages.
            if msg.content.starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX) {
                continue;
            }
            let tool_name = msg.name.as_deref().unwrap_or("unknown");
            msg.content = make_compressed_placeholder(tool_name);
            replaced += 1;
        }
        replaced
    }

    // ── ADR-061 v3 §20.3.1: clear_round ─────────────────────────────────
    //
    // Atomic: fold every Tool message that belongs to the round anchored
    // at `assistant_idx`. Schema (assistant.tool_calls[*].id ↔ tool.tool_call_id)
    // is preserved because no message is removed — only `content` bytes
    // are replaced.
    //
    // Pre-condition: `assistant_idx < self.messages.len()` and
    // `self.messages[assistant_idx].role == MessageRole::Assistant`.
    // Violating the pre-condition is a bug (internal call site); we panic
    // loudly rather than no-op so the bug surfaces immediately.
    //
    // Returns: report describing which tool ids were folded and how many
    // bytes were reclaimed.

    // ADR-061 v3 §20.3 round primitive: implemented and unit-tested,
    // awaiting wiring into the 5-level compression plan (dead in
    // non-test builds until then).
    #[allow(dead_code)]
    pub(crate) fn clear_round(&mut self, assistant_idx: usize) -> ClearRoundReport {
        assert!(
            assistant_idx < self.messages.len(),
            "clear_round: assistant_idx out of bounds ({assistant_idx} / {})",
            self.messages.len()
        );
        assert!(
            matches!(self.messages[assistant_idx].role, MessageRole::Assistant),
            "clear_round: messages[{assistant_idx}] is not an Assistant"
        );

        let tool_call_ids: Vec<String> = self.messages[assistant_idx]
            .tool_calls
            .as_ref()
            .map(|tcs| tcs.iter().map(|tc| tc.id.clone()).collect())
            .unwrap_or_default();

        if tool_call_ids.is_empty() {
            // Round has no tool calls — nothing to fold. Still valid.
            return ClearRoundReport::default();
        }

        // First pass (immutable): gather original content lengths of messages
        // we'd fold, so the byte counter is accurate even after we mutate.
        let mut per_id_bytes: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for msg in self.messages.iter() {
            if !matches!(msg.role, MessageRole::Tool) {
                continue;
            }
            let Some(ref tcid) = msg.tool_call_id else {
                continue;
            };
            if !tool_call_ids.iter().any(|id| id == tcid) {
                continue;
            }
            if msg.content.starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX) {
                continue;
            }
            per_id_bytes
                .entry(tcid.clone())
                .and_modify(|n| *n += msg.content.len())
                .or_insert(msg.content.len());
        }

        // Second pass (mutable): actually fold. Use abandon_tool_result per id
        // so we get its idempotency + naming logic for free.
        let mut cleared = Vec::new();
        for id in &tool_call_ids {
            if self.abandon_tool_result(id) > 0 {
                cleared.push(id.clone());
            }
        }

        let bytes_reclaimed: usize = cleared.iter().filter_map(|id| per_id_bytes.get(id)).sum();

        // Folding replaced Tool `content` with short placeholders, so both
        // the token estimate and the byte counters computed from the old
        // bytes are now stale. `abandon_tool_result` documents this as the
        // caller's job ("Caller is responsible for `recalibrate_tokens()`
        // after a batch of folds") and `fix_round` — which wraps this
        // method — was doing it; this method returning without it meant the
        // `messages` / `skills_loaded` usage sections kept reporting bytes
        // that are no longer in the prompt.
        if !cleared.is_empty() {
            self.recalibrate_tokens();
            self.recompute_byte_counters();
        }

        ClearRoundReport {
            cleared_tool_ids: cleared,
            bytes_reclaimed,
        }
    }

    // ── ADR-061 v3 §20.3.3: fix_round ───────────────────────────────────
    //
    // Atomic: clear_round, then additionally sweep the round's slice
    // (assistant + trailing tool messages up to the next assistant) to
    // remove any orphan tool_calls or orphan tool_results that may have
    // leaked in from upstream (streaming truncation, malformed provider
    // responses, etc.).
    //
    // Used as defense-in-depth after `clear_round`. Normal compression
    // paths should not produce orphans, but `sanitize_messages` is the
    // last line; this round-scoped sweep is cheaper and more targeted.

    // ADR-061 v3 §20.3 round primitive: implemented and unit-tested,
    // awaiting wiring into the 5-level compression plan (dead in
    // non-test builds until then).
    #[allow(dead_code)]
    pub(crate) fn fix_round(&mut self, assistant_idx: usize) -> FixReport {
        assert!(
            assistant_idx < self.messages.len(),
            "fix_round: assistant_idx out of bounds"
        );
        assert!(
            matches!(self.messages[assistant_idx].role, MessageRole::Assistant),
            "fix_round: messages[{assistant_idx}] is not an Assistant"
        );

        // First: clear_round (fold every legitimate tool result).
        let cleared = self.clear_round(assistant_idx);

        // Second: sweep for orphans within the round's slice.
        // Round = messages[assistant_idx] plus everything up to (but
        // excluding) the next Assistant, OR the end of the buffer.
        let start = assistant_idx;
        let end = self.messages[start + 1..]
            .iter()
            .position(|m| matches!(m.role, MessageRole::Assistant))
            .map(|p| start + 1 + p)
            .unwrap_or(self.messages.len());

        // Collect ids the assistant currently claims (after clear_round,
        // these are the same ids it originally had — clear_round only
        // touched tool message contents, not the assistant's tool_calls).
        let claimed_ids: HashSet<String> = self.messages[assistant_idx]
            .tool_calls
            .as_ref()
            .map(|tcs| tcs.iter().map(|tc| tc.id.clone()).collect())
            .unwrap_or_default();

        // Pass A: find orphan tool messages (role=Tool, tool_call_id not in
        // claimed_ids). Mark for removal.
        let orphan_tool_indices: Vec<usize> = (start..end)
            .filter(|&i| {
                matches!(self.messages[i].role, MessageRole::Tool)
                    && self.messages[i]
                        .tool_call_id
                        .as_ref()
                        .map(|tcid| !claimed_ids.contains(tcid))
                        .unwrap_or(false)
            })
            .collect();

        // Pass B: find orphan tool_call ids (assistant claims id but no
        // matching Tool message exists in the round slice). Record ids
        // for in-place removal.
        let present_tool_ids: HashSet<String> = (start..end)
            .filter(|&i| matches!(self.messages[i].role, MessageRole::Tool))
            .filter_map(|i| self.messages[i].tool_call_id.clone())
            .collect();
        let orphan_call_ids: Vec<String> = claimed_ids
            .iter()
            .filter(|id| !present_tool_ids.contains(*id))
            .cloned()
            .collect();

        // Apply: remove orphan tool messages (reverse order to keep
        // indices stable) and prune orphan tool_call ids from the
        // assistant.
        let mut removed_orphan_tool_messages = 0usize;
        if !orphan_tool_indices.is_empty() {
            let msgs = Arc::make_mut(&mut self.messages);
            for &i in orphan_tool_indices.iter().rev() {
                msgs.remove(i);
                removed_orphan_tool_messages += 1;
            }
        }
        if !orphan_call_ids.is_empty() {
            let msgs = Arc::make_mut(&mut self.messages);
            if let Some(ref mut tcs) = msgs[assistant_idx].tool_calls {
                tcs.retain(|tc| !orphan_call_ids.contains(&tc.id));
                if tcs.is_empty() {
                    msgs[assistant_idx].tool_calls = None;
                }
            }
        }

        // Recalibrate once after the sweep.
        self.recalibrate_tokens();
        self.recompute_byte_counters();

        FixReport {
            cleared,
            removed_orphan_tool_messages,
            removed_orphan_tool_call_ids: orphan_call_ids,
        }
    }

    /// ADR-061 §8.2/§19.4: join the original user messages as the
    /// fallback `<user_intent>` when the compaction LLM omits the block.
    ///
    /// Compaction markers (`name == COMPACTION_SUMMARY_NAME`) are
    /// excluded — they are compression artifacts, not original user
    /// input, and must never masquerade as user intent (ADR-061 §19.4).
    pub fn user_intent_fallback_text(&self) -> String {
        self.messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User))
            .filter(|m| m.name.as_deref() != Some(COMPACTION_SUMMARY_NAME))
            .map(|m| m.content.trim())
            .filter(|c| !c.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Replace the middle section of history with a compaction summary.
    ///
    /// Keeps system messages at the start and the last `keep_last_rounds`
    /// conversational rounds at the end. The middle is replaced with a
    /// single `User`-role message carrying `name: "compaction_summary"` as
    /// a compaction marker for [`last_compaction_index`].
    ///
    /// NOTE: The marker is stored as `User` (not `Assistant`) so that
    /// providers which reject two consecutive `Assistant` messages in the
    /// request payload (e.g. glm-5.2 on Volcano Ark) do not 400 when the
    /// preserved tail's first round is also `Assistant{tool_calls}`.
    /// Consumers that need to recognize the marker must filter by
    /// `name == COMPACTION_SUMMARY_NAME`, not by role.
    ///
    /// Returns the number of messages removed.
    pub fn replace_middle_with_summary(&mut self, summary: &str, keep_last_rounds: usize) -> usize {
        // Count leading system messages
        let system_count = self
            .messages
            .iter()
            .take_while(|m| matches!(m.role, MessageRole::System))
            .count();

        // Find tail start: count User or Tool messages from the end.
        // Each "round" starts with a User message (human input) or a Tool
        // message (tool result that feeds the next assistant turn). Counting
        // both ensures correct round detection in tool-calling scenarios
        // where the only User messages are at the conversation start.
        let tail_start = {
            let mut round_count = 0usize;
            let mut idx = self.messages.len();
            for (i, msg) in self.messages.iter().enumerate().rev() {
                if matches!(msg.role, MessageRole::User | MessageRole::Tool) {
                    round_count += 1;
                    if round_count >= keep_last_rounds {
                        idx = i;
                        break;
                    }
                }
            }
            // Not enough rounds: keep everything after system messages
            if round_count < keep_last_rounds {
                system_count
            } else {
                // ── Fix: expand tail boundary to include Assistant messages
                // that own tool_calls referenced by Tool messages in the tail.
                // Without this, sanitize_messages removes orphaned Tool results
                // and the "kept" rounds become empty, defeating compaction.
                //
                // Collect tool_call_ids from Tool messages in [idx, end).
                let tail_tool_ids: HashSet<String> = self.messages[idx..]
                    .iter()
                    .filter(|m| m.role == MessageRole::Tool)
                    .filter_map(|m| m.tool_call_id.clone())
                    .collect();

                // Walk backward from idx-1 to expand tail_start to include
                // any Assistant whose tool_calls match tail_tool_ids.
                // Stop when hitting a User message (natural round boundary).
                let mut expanded = idx;
                if !tail_tool_ids.is_empty() {
                    for j in (system_count..idx).rev() {
                        match self.messages[j].role {
                            MessageRole::User => break,
                            MessageRole::Assistant | MessageRole::Tool => {
                                if let Some(ref tcs) = self.messages[j].tool_calls
                                    && tcs.iter().any(|tc| tail_tool_ids.contains(&tc.id))
                                {
                                    expanded = j;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                expanded
            }
        };

        if tail_start <= system_count {
            return 0; // Nothing to replace
        }

        let removed_count = tail_start - system_count;

        // Subtract tokens of removed messages
        for msg in &self.messages[system_count..tail_start] {
            let tokens = self.counter.count_message(msg, Some(&self.protocol_type));
            self.current_tokens = self.current_tokens.saturating_sub(tokens);
        }

        // Remove middle section
        self.messages_mut().drain(system_count..tail_start);

        // Insert compaction summary as a User message with marker.
        //
        // We use `User` (not `Assistant`) on purpose: a compaction summary
        // represents the conversation's prior content narrated as context, not
        // a prior assistant turn.  Crucially, this avoids producing two
        // consecutive `Assistant` messages in the request payload when the
        // tail's first preserved round is also `Assistant{tool_calls}`.  Some
        // providers (notably glm-5.2 on Volcano Ark) reject that exact pattern
        // with `400 InvalidParameter`, even though the OpenAI spec treats it
        // permissively.  The marker is still identified by
        // `name == COMPACTION_SUMMARY_NAME`, so consumers that filter by role
        // must check `name` instead.
        let summary_msg = ChatMessage {
            role: MessageRole::User,
            content: summary.to_string(),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        };
        let summary_tokens = self
            .counter
            .count_message(&summary_msg, Some(&self.protocol_type));
        self.messages_mut().insert(system_count, summary_msg);
        self.current_tokens += summary_tokens;
        self.recompute_byte_counters();

        tracing::debug!(
            removed = removed_count,
            inserted_tokens = summary_tokens,
            remaining_tokens = self.current_tokens,
            "Middle history replaced with compaction summary"
        );

        removed_count
    }

    /// Find the index of the last compaction summary message.
    ///
    /// Scans messages from the end, looking for any message with
    /// `name == COMPACTION_SUMMARY_NAME`. Returns `Some(index)` if found,
    /// `None` if no compaction has occurred in this session.
    ///
    /// Identification is by `name` only — the marker is a `User` message
    /// in memory (see [`Self::replace_middle_with_summary`]), not
    /// `Assistant`.
    ///
    /// Used at session close to determine the tail distillation start point:
    /// tail = `messages[last_compaction_index + 1 ..]`.
    pub fn last_compaction_index(&self) -> Option<usize> {
        self.messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, msg)| msg.name.as_deref() == Some(COMPACTION_SUMMARY_NAME))
            .map(|(i, _)| i)
    }

    // ── ADR-060 v2 §5.4: todo_write round preservation through compression ──

    /// Find the last `todo_write` Assistant+Tool round in history.
    ///
    /// Scans messages from the end backwards. Returns `(assistant, tool)`
    /// clones if a `todo_write` tool_call is followed by its tool result.
    /// Returns `None` if no `todo_write` has been invoked in this session.
    ///
    /// The returned pair is the canonical "current todo state" — Block C's
    /// old role is now played by these clones, which can survive compaction
    /// via [`Self::inject_round_after_marker`].
    pub fn find_last_todo_write_round(&self) -> Option<(ChatMessage, ChatMessage)> {
        // Reverse-scan for the most recent Assistant with a todo_write call.
        let assistant_idx = self.messages.iter().rposition(|m| {
            m.role == MessageRole::Assistant
                && m.tool_calls
                    .as_ref()
                    .is_some_and(|tcs| tcs.iter().any(|tc| tc.function.name == "todo_write"))
        })?;

        let assistant = self.messages[assistant_idx].clone();

        // Identify the todo_write call's id (most agents emit exactly one
        // todo_write per Assistant turn, but be defensive about the choice).
        let todo_call_id = assistant
            .tool_calls
            .as_ref()?
            .iter()
            .find(|tc| tc.function.name == "todo_write")?
            .id
            .clone();

        // Tool result may appear after Assistant (common) or before (some
        // providers reorder). Scan both directions from assistant_idx.
        let tool_idx = (assistant_idx + 1..self.messages.len())
            .chain(0..assistant_idx)
            .find(|&i| {
                self.messages[i].role == MessageRole::Tool
                    && self.messages[i].tool_call_id.as_deref() == Some(&todo_call_id)
            })?;

        Some((assistant, self.messages[tool_idx].clone()))
    }

    /// Reverse-scan variant that skips any todo_write round whose Assistant
    /// tool_call `id` is listed in `excluded_ids`. Used to enforce JSONL-side
    /// idempotency: if a prior compression already persisted this round, do
    /// not write a duplicate line.
    pub fn find_last_todo_write_round_excluding_injected(
        &self,
        excluded_ids: &HashSet<String>,
    ) -> Option<(ChatMessage, ChatMessage)> {
        // Reverse-scan for the most recent Assistant with a todo_write call
        // whose call id is NOT in `excluded_ids`.
        let assistant_idx = self.messages.iter().rposition(|m| {
            if m.role != MessageRole::Assistant {
                return false;
            }
            let Some(tcs) = m.tool_calls.as_ref() else {
                return false;
            };
            tcs.iter()
                .any(|tc| tc.function.name == "todo_write" && !excluded_ids.contains(&tc.id))
        })?;

        let assistant = self.messages[assistant_idx].clone();

        let todo_call_id = assistant
            .tool_calls
            .as_ref()?
            .iter()
            .find(|tc| tc.function.name == "todo_write")?
            .id
            .clone();

        let tool_idx = (assistant_idx + 1..self.messages.len())
            .chain(0..assistant_idx)
            .find(|&i| {
                self.messages[i].role == MessageRole::Tool
                    && self.messages[i].tool_call_id.as_deref() == Some(&todo_call_id)
            })?;

        Some((assistant, self.messages[tool_idx].clone()))
    }

    // ── ADR-094 follow-up: load_skill body preservation through compression ──

    /// Skill name behind a `load_skill` tool_call, if the arguments parse.
    ///
    /// The name is what makes recall **per-skill** rather than per-round: a
    /// second `load_skill("code-review")` supersedes the first copy, so
    /// keeping both would spend the budget twice on identical instructions.
    /// (Byte accounting deliberately has no such dedup —
    /// [`Self::skills_loaded_bytes`] reports what is physically in the
    /// prompt, which is two copies. Selection is about what is *worth*
    /// keeping; accounting is about what is *there*.)
    fn skill_name_of_call(tc: &ToolCall) -> Option<String> {
        if tc.function.name != crate::tools::builtin::load_skill::TOOL_NAME {
            return None;
        }
        serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
            .ok()?
            .get("name")?
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// The Tool message answering `call_id`, searching forward from
    /// `assistant_idx` then wrapping back (some providers reorder). Same
    /// bidirectional rule as [`Self::find_last_todo_write_round`].
    fn find_tool_result(&self, assistant_idx: usize, call_id: &str) -> Option<ChatMessage> {
        let idx = (assistant_idx + 1..self.messages.len())
            .chain(0..assistant_idx)
            .find(|&i| {
                self.messages[i].role == MessageRole::Tool
                    && self.messages[i].tool_call_id.as_deref() == Some(call_id)
            })?;
        Some(self.messages[idx].clone())
    }

    /// Every loaded skill's most recent round, newest-first, one per
    /// distinct skill name.
    ///
    /// Unlike [`Self::find_last_todo_write_round`] the returned Assistant is
    /// **synthesized to carry exactly the one `tool_call` whose result is
    /// retained**. That is not cosmetic: a single assistant turn can load
    /// several skills in parallel, and re-inserting the original Assistant
    /// once per skill would emit duplicate `tool_call` ids plus calls with no
    /// matching Tool message — which `sanitize_messages` then prunes,
    /// silently undoing the recall we just paid for.
    pub fn find_latest_skill_rounds(&self) -> Vec<(ChatMessage, ChatMessage)> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for (idx, msg) in self.messages.iter().enumerate().rev() {
            if msg.role != MessageRole::Assistant {
                continue;
            }
            let Some(tcs) = msg.tool_calls.as_ref() else {
                continue;
            };
            for tc in tcs {
                let Some(skill) = Self::skill_name_of_call(tc) else {
                    continue;
                };
                if !seen.insert(skill) {
                    continue;
                }
                let Some(tool) = self.find_tool_result(idx, &tc.id) else {
                    continue;
                };
                let mut assistant = msg.clone();
                if let Some(list) = assistant.tool_calls.as_mut() {
                    list.retain(|c| c.id == tc.id);
                }
                out.push((assistant, tool));
            }
        }
        out
    }

    /// Token cost of splicing one `(assistant, tool)` round back in.
    fn round_token_cost(&self, assistant: &ChatMessage, tool: &ChatMessage) -> u64 {
        self.counter
            .count_message(assistant, Some(&self.protocol_type))
            + self
                .counter
                .count_message(tool, Some(&self.protocol_type))
    }

    /// Choose which snapshotted skill rounds fit a recall budget.
    ///
    /// Split out from [`Self::find_latest_skill_rounds`] because the two
    /// halves run at different moments in the compaction: candidates must be
    /// snapshotted **before** `apply_compression` (which is what deletes
    /// them), while the budget is only knowable **after** it (the freed space
    /// is what we are spending). `candidates` is expected to come from
    /// [`Self::find_latest_skill_rounds`], already newest-first.
    ///
    /// Policy:
    /// 1. `budget_tokens == 0` → preserve nothing (the caller's explicit
    ///    "no room" signal).
    /// 2. Otherwise take rounds until the budget is exhausted, **but always
    ///    take at least one** — even when that single SKILL.md alone exceeds
    ///    the whole budget.
    ///
    /// Why the floor: a SKILL.md is the procedure the model is *mid-way
    /// through executing*, not a fact it might wish to recall. Amputating it
    /// breaks the workflow in progress, which is strictly worse than the
    /// temporary over-budget it causes; the next compaction re-evaluates
    /// against the headroom of the moment.
    ///
    /// Recall is monotone. A round dropped here is removed from `messages`
    /// outright, so [`Self::find_latest_skill_rounds`] can never surface it
    /// again and no later compression can resurrect it — the only way a skill
    /// comes back is the model calling `load_skill` again, which is also the
    /// evidence that it still needs it. That is what makes this safe to run on
    /// a *dynamic* budget: a shrinking budget prunes old skills once, and a
    /// later roomier budget cannot re-add them, so there is no inject/drop
    /// oscillation across consecutive compressions.
    pub fn apply_skill_recall_budget(
        &self,
        candidates: &[(ChatMessage, ChatMessage)],
        budget_tokens: u64,
    ) -> Vec<(ChatMessage, ChatMessage)> {
        if budget_tokens == 0 {
            return Vec::new();
        }
        let mut picked: Vec<(ChatMessage, ChatMessage)> = Vec::new();
        let mut spent = 0u64;
        for (assistant, tool) in candidates {
            let cost = self.round_token_cost(assistant, tool);
            if !picked.is_empty() && spent + cost > budget_tokens {
                break;
            }
            spent += cost;
            picked.push((assistant.clone(), tool.clone()));
        }
        picked
    }

    /// Convenience composition of find + budget, for callers that can do both
    /// in one step (and for tests). The live compaction path uses the two
    /// halves separately — see [`Self::apply_skill_recall_budget`].
    pub fn select_skill_rounds_to_preserve(
        &self,
        budget_tokens: u64,
    ) -> Vec<(ChatMessage, ChatMessage)> {
        let candidates = self.find_latest_skill_rounds();
        self.apply_skill_recall_budget(&candidates, budget_tokens)
    }

    /// Splice a preserved round right after the last compaction summary marker.
    ///
    /// Tool-agnostic by design: the caller decides which round to hand over
    /// ([`Self::find_last_todo_write_round`] for todo state,
    /// [`Self::select_skill_rounds_to_preserve`] for `load_skill` bodies).
    /// Both are "state the model still needs, stored as a real tool round in
    /// Block B", and ADR-060 v2 §5.4's splice is the shared mechanism.
    ///
    /// ADR-060 v2 §5.4: todos live in Block B as real `todo_write` tool
    /// results. After `apply_compression` replaces the middle with a summary,
    /// the round may have been removed from the retained tail. We splice it
    /// back right after the marker so the next LLM call sees the current
    /// todo state.
    ///
    /// Tool call_id collision with the retained tail is a hard skip (warning
    /// logged) — the LLM would otherwise see two Tool messages with the same
    /// id and `sanitize_messages` would drop the orphan.
    ///
    /// Token accounting: the injected pair's tokens are added to
    /// `current_tokens` via `recalibrate_tokens()`. If the result exceeds
    /// `max_tokens`, only a warning is emitted — the caller has already
    /// committed to the compaction summary (rolling back would lose it).
    /// Tokens may temporarily exceed budget until the next operation; this
    /// is acceptable because the alternative is dropping the todo state,
    /// which is more disruptive to ongoing work.
    ///
    /// Returns `(new_token_count, injected)`: the second element is `true`
    /// ONLY when this call actually spliced the round into memory (i.e. the
    /// `in_tail` collision check passed AND a marker exists). The caller
    /// uses it as the **persistence gate**: writing the synthetic rows to
    /// JSONL only when `injected == true` prevents duplicate `tool_call` /
    /// `tool_result` entries from accumulating on disk across consecutive
    /// compressions — without this gate, every compression would re-write
    /// the same synthetic round even though `in_tail` correctly skipped
    /// the in-memory splice.
    pub fn inject_round_after_marker(
        &mut self,
        assistant: ChatMessage,
        tool: ChatMessage,
    ) -> (u64, bool) {
        // ADR-060 v2 §5.4 idempotency:
        //
        //   The caller hands us the last todo_write round snapshotted BEFORE
        //   `apply_compression`. On the second consecutive compression, level
        //   8 strips BOTH the previous marker AND our previous splice, leaving
        //   history ≈ [sys, last_user, marker2]. The post-marker-tail check
        //   below would correctly find nothing → splice would succeed → JSONL
        //   accumulates duplicate synthetic rows.
        //
        //   The first time a round is injected, we record its tool_call_id in
        //   `injected_round_call_ids`. On every subsequent call we short-
        //   circuit: the same round was already injected in a previous
        //   compression, so we must not append it again. The trade-off is
        //   that on the second compression Block B has no todo_write tool
        //   result — but the JSONL still holds the synthetic row from the
        //   first injection, and the restorer rebuilds Block B from JSONL on
        //   resume. Within the same process the LLM continues without todo
        //   state for that turn, which is an accepted degradation: the
        //   alternative (re-inject on every compression) creates unbounded
        //   JSONL duplication that downstream consumers cannot tolerate.
        //
        //   The same gate serves skill-body recall. Because it is keyed on
        //   tool_call_id and a dropped round is removed from `self.messages`
        //   outright (so the finder can never surface it again), recall is
        //   monotonically shrinking: a skill compressed away stays away and
        //   is only restored when the model explicitly calls `load_skill`
        //   again. There is no inject/drop oscillation across compressions.
        if let Some(tcid) = tool.tool_call_id.as_deref()
            && self.injected_round_call_ids.contains(tcid)
        {
            tracing::debug!(
                tool_call_id = %tcid,
                "ADR-060 v2 §5.4: round already injected by prior \
                 compression — skip (JSONL holds the synthetic row for restart)"
            );
            return (self.current_tokens, false);
        }

        let Some(marker_idx) = self.last_compaction_index() else {
            // No compaction marker — caller should not invoke this; safe no-op.
            tracing::debug!(
                "ADR-060 v2: no compaction marker found — skipping todo_write injection"
            );
            return (self.current_tokens, false);
        };

        // Skip if the round is already in the retained tail (caller may have
        // re-fetched it, or `keep_last_rounds` preserved it). This preserves
        // cache hits and avoids a duplicate tool_call_id.
        //
        // NOTE: idempotency-vs-prior-compression is enforced at function
        // entry (above) so this block only handles the in-tail case where the
        // round survives both the splice and the snapshot.
        if let Some(tcid) = tool.tool_call_id.as_deref() {
            let tool_in_tail = self.messages[marker_idx + 1..]
                .iter()
                .any(|m| m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some(tcid));
            if tool_in_tail {
                tracing::debug!(
                    tool_call_id = %tcid,
                    "ADR-060 v2: todo_write Tool already in retained tail — skip injection"
                );
                return (self.current_tokens, false);
            }
            // Also: if the Assistant with a matching todo_write call_id is in
            // the tail, skip — defense against weird call orders where the
            // Tool is removed but Assistant survived (e.g., trimming
            // strategies that drop orphan Tools but keep their callers).
            let assistant_call_ids: Vec<&str> = assistant
                .tool_calls
                .as_ref()
                .map(|tcs| tcs.iter().map(|tc| tc.id.as_str()).collect())
                .unwrap_or_default();
            let assistant_in_tail = self.messages[marker_idx + 1..].iter().any(|m| {
                m.role == MessageRole::Assistant
                    && m.tool_calls.as_ref().is_some_and(|tcs| {
                        tcs.iter()
                            .any(|tc| assistant_call_ids.contains(&tc.id.as_str()))
                    })
            });
            if assistant_in_tail {
                tracing::debug!(
                    tool_call_id = %tcid,
                    "ADR-060 v2: todo_write Assistant already in retained tail — skip injection"
                );
                return (self.current_tokens, false);
            }
        }

        // Capture the tool_call id BEFORE we move `tool` into `messages`. We need
        // it below to update the idempotency key.
        let injected_tool_call_id = tool.tool_call_id.clone();

        // Insert (Assistant, Tool) immediately after the marker. Order
        // matters: the LLM expects Assistant{tool_calls} → Tool, mimicking
        // the natural round shape. Tool before Assistant would be invalid.
        let insert_at = marker_idx + 1;
        {
            let msgs = self.messages_mut();
            msgs.insert(insert_at, tool);
            msgs.insert(insert_at, assistant);
        }

        // Recompute token count for the new state. `recalibrate_tokens()`
        // walks all messages — the only side effect, but it's the source of
        // truth (avoids manual `count_message` arithmetic that could drift).
        self.recalibrate_tokens();
        self.recompute_byte_counters();

        if self.current_tokens > self.max_tokens {
            tracing::warn!(
                current_tokens = self.current_tokens,
                max_tokens = self.max_tokens,
                "ADR-060 v2: injected todo_write round pushed token count over budget \
                 (compaction summary already committed — log-only)"
            );
        }

        // ADR-060 v2 §5.4 idempotency: remember the call id we spliced so the
        // next compression recognizes this round and skips re-injection.
        if let Some(tcid) = injected_tool_call_id.as_deref() {
            self.injected_round_call_ids.insert(tcid.to_string());
        }

        (self.current_tokens, true)
    }

    // ── ADR-061 v3 §20.3.2: recall_todo_round ──────────────────────────
    //
    // v2 contract: find the last todo_write round, splice it after the
    // compaction marker. `inject_round_after_marker` already
    // implements this; `recall_todo_round` is the v3 facade that returns
    // a structured result instead of a `(tokens, injected_bool)` tuple,
    // so callers can branch on skip reasons cleanly.
    //
    // Returns:
    // - `Injected { todo_call_id }` — round was spliced after the marker
    // - `SkippedAlreadyInjected` — same round was injected by a prior compression
    // - `SkippedAlreadyInTail`     — todo round survived in the retained tail
    // - `NoMarker`                 — no compaction has happened (caller bug)
    // - `NoTodoRoundFound`         — history has no todo_write round yet
    // ADR-061 v3 §20.3 round primitive: implemented and unit-tested,
    // awaiting wiring into the 5-level compression plan (dead in
    // non-test builds until then).
    #[allow(dead_code)]
    pub(crate) fn recall_todo_round(&mut self) -> RecallResult {
        // Fast path: same round already injected by a prior compression.
        if let Some((asst, _)) = self.find_last_todo_write_round()
            && asst.tool_calls.as_ref().is_some_and(|tcs| {
                tcs.iter().any(|tc| self.injected_round_call_ids.contains(&tc.id))
            })
        {
            return RecallResult::SkippedAlreadyInjected;
        }

        // No marker means caller invoked us at the wrong time.
        if self.last_compaction_index().is_none() {
            return RecallResult::NoMarker;
        }

        // No todo_write round in history at all.
        let Some((assistant, tool)) =
            self.find_last_todo_write_round_excluding_injected(&HashSet::new())
        else {
            return RecallResult::NoTodoRoundFound;
        };

        let todo_call_id = assistant
            .tool_calls
            .as_ref()
            .and_then(|tcs| tcs.iter().find(|tc| tc.function.name == "todo_write"))
            .map(|tc| tc.id.clone())
            .unwrap_or_default();

        // Delegate to the existing injector. It handles the in-tail skip
        // and idempotency bookkeeping.
        let (_, injected) = self.inject_round_after_marker(assistant, tool);

        if injected {
            RecallResult::Injected { todo_call_id }
        } else {
            // Either the assistant or the tool was already in the tail.
            RecallResult::SkippedAlreadyInTail
        }
    }

    // ── ADR-061 v3 §20: 5-level round-atomic compression ──────────────

    /// ADR-061 v3 §19.1/§20.6 (PR3): select the 5-level degradation plan.
    ///
    /// `summary` is the LLM output (already parsed by `parse_compact_output`);
    /// its token size is known here, so the projection is exact — the plan
    /// walk happens **after** the summary (§19.1 先摘要后 plan).
    ///
    /// Selection rule (§19.2, renumbered for v3):
    /// - Levels 1-4: the **first** level satisfying
    ///   `ratio >= min_ratio && projected <= budget` wins
    ///   (stop at the first sufficient one — more aggressive levels are
    ///   never tried).
    /// - Level 5 (formerly level 8): sole fallback, **exempt** from the
    ///   ratio bar; only requires `projected <= budget` (T > budget
    ///   overflow lands here in one shot).
    ///
    /// `min_ratio` is the per-agent compression ratio threshold (default
    /// [`MIN_COMPRESSION_RATIO`] = 0.90, i.e. "compress until at most 10%
    /// remains", e.g. 200K → 20K).
    ///
    /// Errors with [`CompressError::UnrecoverableOverflow`] when level 5
    /// still cannot fit — the caller must not touch history (§19.5).
    pub(crate) fn plan_compression(
        &self,
        summary: &str,
        min_ratio: f64,
    ) -> std::result::Result<CompressionPlan, CompressError> {
        if self.current_tokens == 0 || self.max_tokens == 0 {
            return Err(CompressError::UnrecoverableOverflow {
                projected: self.current_tokens,
                budget: self.max_tokens,
            });
        }

        let msgs = self.messages.as_slice();
        let mut level5_projected = 0u64;
        for level in 1..=5u8 {
            let plan = self.build_level_plan(level, msgs, summary);
            let projected = plan.projected_tokens;
            if level < 5 {
                let ratio = 1.0 - (projected as f64 / self.current_tokens as f64);
                if ratio >= min_ratio && projected <= self.max_tokens {
                    return Ok(plan);
                }
            } else {
                level5_projected = projected;
                if projected <= self.max_tokens {
                    return Ok(plan);
                }
            }
        }

        Err(CompressError::UnrecoverableOverflow {
            projected: level5_projected,
            budget: self.max_tokens,
        })
    }
    /// Build the retention plan for a single level (pure function over the
    /// message snapshot; no mutation).
    ///
    /// ADR-061 v3 (§6.2 / §20): retention is decided per **round** — an
    /// assistant message together with the tool results that follow it is
    /// one atomic unit. A round is either kept whole (assistant + tools) or
    /// dropped whole into the summary middle; it is never half-kept. Among
    /// the kept rounds, the tool results of the last `P` tool-owning rounds
    /// stay verbatim while earlier kept rounds have their tool results
    /// folded into placeholders ([`make_compressed_placeholder`]). Because a
    /// kept assistant's `tool_calls[*].id` therefore always resolves to a
    /// kept `Tool` message, no dangling-id sweep is needed (this removes the
    /// v2 ghost-assistant bug, §20.1).
    ///
    /// Level table (v3 §6.2; renumbered from the former Lv4-Lv8):
    /// | level | assistant messages kept | tool results |
    /// |-------|-------------------------|--------------|
    /// | 1     | last 5                  | last 1 round verbatim, rest folded |
    /// | 2     | last 5                  | all folded |
    /// | 3     | last 3                  | all folded |
    /// | 4     | last 1                  | all folded |
    /// | 5     | none (summary only)     | —           |
    fn build_level_plan(&self, level: u8, msgs: &[ChatMessage], summary: &str) -> CompressionPlan {
        let system_count = msgs
            .iter()
            .take_while(|m| matches!(m.role, MessageRole::System))
            .count();

        // Level 5 (formerly level 8): only system + latest real user message
        // survive; all the rest goes to the summary. "Current user message" =
        // last non-marker User message (§19.1 note — compaction markers are
        // User-role too, so they must be excluded here).
        if level == 5 {
            let last_user = msgs.iter().rposition(|m| {
                matches!(m.role, MessageRole::User)
                    && m.name.as_deref() != Some(COMPACTION_SUMMARY_NAME)
            });
            let mut retained: Vec<ChatMessage> = msgs[..system_count].to_vec();
            let mut user_kept = 0usize;
            if let Some(ui) = last_user {
                retained.push(msgs[ui].clone());
                user_kept = 1;
            }
            let retained_tokens = self.count_slice_tokens(&retained);
            return CompressionPlan {
                level,
                retained,
                original_tokens: self.current_tokens,
                retained_tokens,
                projected_tokens: retained_tokens + self.estimate_marker_tokens(summary),
                stats: RetentionStats {
                    user_messages: user_kept,
                    assistant_messages: 0,
                    tool_messages: 0,
                    user_desc: if user_kept > 0 {
                        "last 1".to_string()
                    } else {
                        "none".to_string()
                    },
                    assistant_desc: "none".to_string(),
                    tool_desc: "none".to_string(),
                },
            };
        }

        // Levels 1-4: every user message (incl. prior compaction markers) is
        // preserved; the last K assistant messages are kept, and every kept
        // assistant's own tool results are kept too — verbatim for the last
        // P tool-owning rounds, folded into placeholders otherwise. Rounds
        // older than K are dropped whole (subsumed by the summary).
        let (assistant_keep, verbatim_rounds) = match level {
            // L1: last round's tool results stay verbatim; earlier kept
            // rounds are folded (was v2 Lv4).
            1 => (5, 1),
            // L2: same assistant window, every kept tool result folded.
            2 => (5, 0),
            // L3 / L4: tighter assistant windows, everything folded.
            3 => (3, 0),
            4 => (1, 0),
            _ => unreachable!("level 5 handled above"),
        };

        let n_msgs = msgs.len();
        // 1-based rank-from-the-end of every Assistant message; 0 otherwise.
        let assistant_positions: Vec<usize> = (0..n_msgs)
            .filter(|&i| matches!(msgs[i].role, MessageRole::Assistant))
            .collect();
        let n_assistants = assistant_positions.len();
        let mut assistant_rank = vec![0usize; n_msgs];
        for (j, &pos) in assistant_positions.iter().enumerate() {
            assistant_rank[pos] = n_assistants - j;
        }

        // Same ranking restricted to tool-owning assistants (an assistant
        // whose `tool_calls` is non-empty); decides which kept rounds keep
        // their tool results verbatim.
        let tool_owner_positions: Vec<usize> = (0..n_msgs)
            .filter(|&i| {
                matches!(msgs[i].role, MessageRole::Assistant)
                    && msgs[i]
                        .tool_calls
                        .as_ref()
                        .is_some_and(|tcs| !tcs.is_empty())
            })
            .collect();
        let n_tool_owners = tool_owner_positions.len();
        let mut tool_owner_rank = vec![0usize; n_msgs];
        for (j, &pos) in tool_owner_positions.iter().enumerate() {
            tool_owner_rank[pos] = n_tool_owners - j;
        }

        // Effective windows (a level may ask for more rounds than exist).
        let keep_min_rank = assistant_keep.min(n_assistants);
        let verbatim_min_rank = verbatim_rounds.min(n_tool_owners);

        // Round-atomic scanner. `mode` carries the decision of the assistant
        // message that owns the Tool messages currently being walked, so a
        // round is kept/dropped/folded as a unit.
        enum RoundMode {
            /// Before the first assistant in the buffer.
            Lead,
            /// Assistant round dropped whole into the summary.
            Drop,
            /// Assistant kept; its tool results stay verbatim.
            Verbatim,
            /// Assistant kept; its tool results are folded to placeholders.
            Fold,
        }

        let mut mode = RoundMode::Lead;
        let mut retained: Vec<ChatMessage> = Vec::with_capacity(n_msgs);
        let mut user_kept = 0usize;
        let mut assistant_kept = 0usize;
        let mut tool_kept = 0usize;
        for (i, msg) in msgs.iter().enumerate() {
            match msg.role {
                MessageRole::System => retained.push(msg.clone()),
                MessageRole::User => {
                    user_kept += 1;
                    retained.push(msg.clone());
                }
                MessageRole::Assistant => {
                    let rank = assistant_rank[i];
                    let kept = rank > 0 && rank <= keep_min_rank;
                    let owner_rank = tool_owner_rank[i];
                    mode = if !kept {
                        RoundMode::Drop
                    } else if owner_rank > 0 && owner_rank <= verbatim_min_rank {
                        RoundMode::Verbatim
                    } else if owner_rank > 0 {
                        RoundMode::Fold
                    } else {
                        // Text-only assistant: no tool results of its own, so
                        // nothing to fold.
                        RoundMode::Verbatim
                    };
                    if kept {
                        assistant_kept += 1;
                        retained.push(msg.clone());
                    }
                }
                MessageRole::Tool => match mode {
                    RoundMode::Lead | RoundMode::Drop => {}
                    RoundMode::Verbatim => {
                        tool_kept += 1;
                        retained.push(msg.clone());
                    }
                    RoundMode::Fold => {
                        tool_kept += 1;
                        let mut folded = msg.clone();
                        folded.content = make_compressed_placeholder(
                            folded.name.as_deref().unwrap_or("unknown"),
                        );
                        retained.push(folded);
                    }
                },
            }
        }

        let retained_tokens = self.count_slice_tokens(&retained);
        let assistant_desc = if n_assistants == 0 {
            "none".to_string()
        } else if keep_min_rank >= n_assistants {
            "all".to_string()
        } else {
            format!("last {}", assistant_keep)
        };
        let tool_desc = if n_tool_owners == 0 {
            "none".to_string()
        } else if verbatim_min_rank == 0 {
            "all folded to placeholders".to_string()
        } else if verbatim_min_rank >= n_tool_owners {
            "all verbatim".to_string()
        } else {
            format!("last {verbatim_min_rank} round(s) verbatim, rest folded")
        };
        let stats = RetentionStats {
            user_messages: user_kept,
            assistant_messages: assistant_kept,
            tool_messages: tool_kept,
            user_desc: "all".to_string(),
            assistant_desc,
            tool_desc,
        };
        CompressionPlan {
            level,
            retained,
            original_tokens: self.current_tokens,
            retained_tokens,
            projected_tokens: retained_tokens + self.estimate_marker_tokens(summary),
            stats,
        }
    }

    /// ADR-061 §19.2: apply the plan — rebuild history as
    /// `[system][summary marker][retained]`, with the level metadata block
    /// (§9) prepended to the summary inside the marker.
    ///
    /// The marker keeps `User` role + `name = "compaction_summary"` so the
    /// restorer / `last_compaction_index` contracts are unchanged, and no
    /// `Assistant → Assistant{tool_calls}` adjacency can appear (glm-5.2
    /// on Volcano Ark rejects that pattern).
    pub(crate) fn apply_compression(
        &mut self,
        plan: CompressionPlan,
        summary: &str,
        min_ratio: f64,
    ) -> std::result::Result<CompressionOutcome, CompressError> {
        let original_tokens = plan.original_tokens;
        let ratio = 1.0 - (plan.projected_tokens as f64 / original_tokens as f64);

        // Defensive re-check (plan-time validation already ran; this guards
        // against callers bypassing `plan_compression`). Level 8 is exempt
        // from the ratio bar (§19.2).
        if plan.projected_tokens > self.max_tokens || (plan.level < 5 && ratio < min_ratio) {
            return Err(CompressError::InsufficientCompression {
                projected_ratio: ratio,
            });
        }

        let marker = ChatMessage {
            role: MessageRole::User,
            content: build_summary_marker(
                plan.level,
                original_tokens,
                plan.projected_tokens,
                ratio,
                &plan.stats,
                summary,
            ),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        };

        // Marker sits right after the leading system block (middle-replace
        // semantics preserved; restorer anchors replay after it).
        let split = plan
            .retained
            .iter()
            .take_while(|m| matches!(m.role, MessageRole::System))
            .count();
        let mut new_msgs = Vec::with_capacity(plan.retained.len() + 1);
        new_msgs.extend_from_slice(&plan.retained[..split]);
        new_msgs.push(marker);
        new_msgs.extend_from_slice(&plan.retained[split..]);

        let removed_messages = self.messages.len().saturating_sub(new_msgs.len());
        *self.messages_mut() = new_msgs;
        self.recalibrate_tokens();
        self.recompute_byte_counters();

        tracing::info!(
            level = plan.level,
            removed_messages,
            original_tokens,
            retained_tokens = plan.retained_tokens,
            new_tokens = self.current_tokens,
            ratio = ?ratio,
            "ADR-061: 5-level compression plan applied"
        );

        Ok(CompressionOutcome {
            level: plan.level,
            original_tokens,
            new_tokens: self.current_tokens,
            compression_ratio: ratio,
            removed_messages,
        })
    }

    /// Sum `count_message` over a slice (kept messages at plan time).
    fn count_slice_tokens(&self, msgs: &[ChatMessage]) -> u64 {
        msgs.iter()
            .map(|m| self.counter.count_message(m, Some(&self.protocol_type)))
            .sum()
    }

    /// Estimate the to-be-inserted summary marker's tokens: precise summary
    /// text count plus a fixed overhead for the level metadata block (§9).
    fn estimate_marker_tokens(&self, summary: &str) -> u64 {
        crate::token::count_text(summary) as u64 + 64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::compression_constants::MIN_COMPRESSION_RATIO;

    fn make_message(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_append_and_count() {
        let mut hm = HistoryManager::new(1000);
        hm.append(make_message(MessageRole::User, "Hello world"));
        assert_eq!(hm.len(), 1);
        assert!(hm.token_count() > 0);
    }

    #[test]
    fn restore_recipe_keeps_full_history_above_80_percent_budget() {
        // Regression (session 20260921_155604_a4825c): cold-start restore
        // must NOT trim. The retired `fit_to_budget_lossless` dropped the
        // oldest complete rounds whenever the anchored API prompt exceeded
        // 80% of `max_tokens`, so a session whose meta said
        // `last_input = 167098` lost 90 messages and the status panel
        // reported `~109K` — a number the user never produced.
        //
        // `max_tokens = 208_000` puts 80% at 166_400, i.e. the anchor below
        // is over the old trigger line on purpose. `HistoryManager` now
        // exposes no restore-time trim at all.
        let mut hm = HistoryManager::new(208_000);
        hm.set_max_tokens(208_000);

        let msgs: Vec<ChatMessage> = (0..40)
            .map(|i| {
                make_message(
                    MessageRole::User,
                    &format!("message number {i} with enough padding text to count"),
                )
            })
            .collect();
        let count = msgs.len();
        hm.load_restored(msgs, std::collections::HashSet::new());
        hm.restore_anchor(Some(167_098), Some(3.5));

        assert_eq!(hm.len(), count, "restore must not drop any message");
        assert_eq!(
            hm.token_count(),
            167_098,
            "count is the persisted API prompt total, not a messages-only estimate"
        );
        assert!(
            hm.token_count() > hm.max_tokens() / 100 * 80,
            "precondition: over the old 80% trigger line"
        );
    }

    #[test]
    fn restore_anchor_restores_last_api_count_and_ratio() {
        // Regression: resumed sessions must re-anchor the token count to the
        // last API-counted prompt (which already includes system+tools) and
        // restore the calibrated ratio — otherwise the JSONL replay's
        // default-ratio estimate is strictly smaller than the real prompt
        // and the first post-resume request slips past the compaction
        // threshold.
        let mut hm = HistoryManager::new(1000);
        hm.append(make_message(
            MessageRole::User,
            "你好世界，这是一段比较长的中文历史内容",
        ));
        let replayed_estimate = hm.token_count();

        // The persisted meta says the last API count was much larger
        // (API tokenizer vs local default ratio).
        let api_last_input = replayed_estimate * 2;
        hm.restore_anchor(Some(api_last_input), Some(2.0));

        assert_eq!(
            hm.token_count(),
            api_last_input,
            "anchored to API ground truth"
        );
        assert_eq!(hm.model_ratio(), Some(2.0), "restored calibrated ratio");

        // A missing ratio falls back to the default (still anchored).
        let mut hm2 = HistoryManager::new(1000);
        hm2.restore_anchor(Some(42), None);
        assert_eq!(hm2.token_count(), 42);
        assert_eq!(hm2.model_ratio(), None);
    }

    #[test]
    fn messages_arc_is_shallow_and_copy_on_write() {
        let mut hm = HistoryManager::new(100_000);
        hm.append(ChatMessage::user("hello".to_string()));
        hm.append(ChatMessage::assistant("hi".to_string()));

        // Snapshot A: shallow Arc clone — O(1), shares the live buffer.
        let snap_a = hm.messages_arc();
        assert_eq!(snap_a.len(), 2);

        // Mutate after the clone: COW clones once; the snapshot retains the
        // exact build-time history while the live history advances.
        hm.append(ChatMessage::user("second turn".to_string()));
        assert_eq!(snap_a.len(), 2, "snapshot must retain build-time history");
        assert_eq!(hm.messages().len(), 3, "live history advances");

        // Snapshot B taken after the mutation sees the new state.
        let snap_b = hm.messages_arc();
        assert_eq!(snap_b.len(), 3);

        // Multiple snapshots without intervening mutation share one buffer.
        let snap_c = hm.messages_arc();
        assert!(
            Arc::ptr_eq(&snap_b, &snap_c),
            "no mutation between clones => same underlying buffer"
        );
        assert!(
            !Arc::ptr_eq(&snap_a, &snap_b),
            "mutation between clones => distinct buffers"
        );
    }

    #[test]
    fn messages_arc_survives_rewind_truncate() {
        let mut hm = HistoryManager::new(100_000);
        for i in 0..5 {
            hm.append(ChatMessage::user(format!("m{i}")));
        }
        let snap = hm.messages_arc();
        assert_eq!(snap.len(), 5);

        // Debug rewind truncates live history; the snapshot keeps the
        // pre-rewind buffer (lazy getSection(iteration, "messages") still
        // returns the full conversation as of that build).
        hm.truncate_to(3);
        assert_eq!(snap.len(), 5, "snapshot keeps pre-rewind history");
        assert_eq!(hm.messages().len(), 3, "live history truncated");
    }

    #[test]
    fn test_load_restored_replaces_and_recounts() {
        let mut hm = HistoryManager::new(10000);
        hm.append(make_message(MessageRole::User, "old data"));
        let before_tokens = hm.token_count();
        assert!(before_tokens > 0);

        let new_msgs = vec![
            make_message(MessageRole::System, "Sys"),
            make_message(MessageRole::User, "fresh"),
            make_message(MessageRole::Assistant, "fresh reply"),
        ];
        hm.load_restored(new_msgs, std::collections::HashSet::new());
        assert_eq!(hm.len(), 3);
        assert!(matches!(hm.messages()[0].role, MessageRole::System));
        // Token count must be recomputed (not stale "old data" + new).
        let recomputed = hm.token_count();
        assert!(recomputed > 0);
    }

    // ── sanitize_messages tests ─────────────────────────────────────────

    use acowork_core::providers::traits::{FunctionCall, ToolCall};

    fn make_tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn make_tool_result(tool_call_id: &str, content: &str) -> ChatMessage {
        ChatMessage::tool(tool_call_id, content)
    }

    #[test]
    fn test_sanitize_fixes_invalid_arguments() {
        let mut messages = vec![
            ChatMessage::assistant_with_tools(
                "",
                vec![
                    make_tool_call("tc_1", "read_file", "not valid json{{"),
                    make_tool_call("tc_2", "write_file", r#"{"path":"/tmp"}"#),
                ],
            ),
            make_tool_result("tc_1", "result 1"),
            make_tool_result("tc_2", "result 2"),
        ];

        HistoryManager::sanitize_messages(&mut messages);

        let assistant = &messages[0];
        let tool_calls = assistant.tool_calls.as_ref().unwrap();
        // Invalid arguments should be fixed to `{}`
        assert_eq!(tool_calls[0].function.arguments, "{}");
        // Valid arguments should be unchanged
        assert_eq!(tool_calls[1].function.arguments, r#"{"path":"/tmp"}"#);
    }

    #[test]
    fn test_sanitize_removes_orphaned_tool_result() {
        let mut messages = vec![
            ChatMessage::assistant_with_tools(
                "I'll help you",
                vec![make_tool_call("tc_1", "read_file", "{}")],
            ),
            make_tool_result("tc_1", "result 1"),
            make_tool_result("tc_orphan", "orphaned result"),
        ];

        HistoryManager::sanitize_messages(&mut messages);

        // Only tc_1's result should remain
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].tool_call_id, Some("tc_1".to_string()));
    }

    #[test]
    fn test_sanitize_removes_orphaned_tool_call() {
        let mut messages = vec![
            ChatMessage::assistant_with_tools(
                "",
                vec![
                    make_tool_call("tc_1", "read_file", "{}"),
                    make_tool_call("tc_2", "write_file", "{}"),
                ],
            ),
            make_tool_result("tc_1", "result 1"),
            // tc_2 has no result
        ];

        HistoryManager::sanitize_messages(&mut messages);

        let assistant = &messages[0];
        let tool_calls = assistant.tool_calls.as_ref().unwrap();
        // Only tc_1 should remain
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "tc_1");
    }

    #[test]
    fn test_sanitize_removes_empty_assistant_message() {
        let mut messages = vec![
            make_message(MessageRole::User, "Hello"),
            ChatMessage::assistant(""),
            make_message(MessageRole::User, "World"),
        ];

        HistoryManager::sanitize_messages(&mut messages);

        // Empty assistant message should be removed
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[1].role, MessageRole::User);
    }

    #[test]
    fn test_sanitize_preserves_order() {
        let mut messages = vec![
            make_message(MessageRole::System, "System"),
            make_message(MessageRole::User, "Hello"),
            ChatMessage::assistant_with_tools(
                "Let me check",
                vec![make_tool_call("tc_1", "search", "{}")],
            ),
            make_tool_result("tc_1", "Found it"),
            make_message(MessageRole::Assistant, "Here's the answer"),
        ];

        HistoryManager::sanitize_messages(&mut messages);

        // All messages should be preserved in order
        assert_eq!(messages.len(), 5);
        assert!(matches!(messages[0].role, MessageRole::System));
        assert!(matches!(messages[1].role, MessageRole::User));
        assert!(matches!(messages[2].role, MessageRole::Assistant));
        assert!(matches!(messages[3].role, MessageRole::Tool));
        assert!(matches!(messages[4].role, MessageRole::Assistant));
    }

    #[test]
    fn test_sanitize_is_idempotent() {
        let mut messages = vec![
            ChatMessage::assistant_with_tools(
                "",
                vec![make_tool_call("tc_1", "read_file", "not json")],
            ),
            make_tool_result("tc_1", "result 1"),
        ];

        HistoryManager::sanitize_messages(&mut messages);
        let first_result = messages.clone();

        HistoryManager::sanitize_messages(&mut messages);

        // Second call should produce same result
        assert_eq!(messages.len(), first_result.len());
        for (a, b) in messages.iter().zip(first_result.iter()) {
            assert_eq!(a.role, b.role);
            assert_eq!(a.content, b.content);
        }
    }

    #[test]
    fn test_sanitize_clears_tool_calls_when_all_orphaned() {
        let mut messages = vec![ChatMessage::assistant_with_tools(
            "Let me check",
            vec![
                make_tool_call("tc_1", "search", "{}"),
                make_tool_call("tc_2", "read", "{}"),
            ],
        )];
        // No tool results at all — both tool_calls should be removed

        HistoryManager::sanitize_messages(&mut messages);

        let assistant = &messages[0];
        // tool_calls should be cleared to None since all were orphaned
        assert!(assistant.tool_calls.is_none());
        // Content should be preserved since it's non-empty
        assert_eq!(assistant.content, "Let me check");
    }

    // ── ADR-061 v3 §20.3.1 / PR1: abandon_tool_result tests ─────────────
    //
    // PR1 revives `abandon_tool_result` as a single-message primitive.
    // PR2 will wrap it in `clear_round` / `fix_round`. These tests verify
    // the primitive's contract independently.

    /// Helper: build a Tool message with the given id, name, and content.
    fn make_tool_message(tool_call_id: &str, tool_name: &str, content: &str) -> ChatMessage {
        let mut m = ChatMessage::tool(tool_call_id, content);
        m.name = Some(tool_name.to_string());
        m
    }

    #[test]
    fn test_abandon_tool_result_replaces_matching_tool_content() {
        let mut hm = HistoryManager::new(10_000);
        // Assistant emits two tool_calls; both tool results present.
        hm.append(ChatMessage::assistant_with_tools(
            "checking",
            vec![
                make_tool_call("tc_alpha", "file_edit", "{}"),
                make_tool_call("tc_beta", "bash", "{}"),
            ],
        ));
        hm.append(make_tool_message(
            "tc_alpha",
            "file_edit",
            "Edited 42 bytes",
        ));
        hm.append(make_tool_message("tc_beta", "bash", "shell output here"));

        let replaced = hm.abandon_tool_result("tc_alpha");

        assert_eq!(replaced, 1, "exactly one tool message should be replaced");
        // tc_alpha was folded
        let folded = hm
            .messages()
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("tc_alpha"))
            .expect("tc_alpha tool message must still exist (schema preserved)");
        assert!(
            folded
                .content
                .starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX),
            "folded content must start with placeholder prefix, got: {:?}",
            folded.content
        );
        assert!(
            folded.content.contains("file_edit"),
            "placeholder must include the original tool name for re-invoke hints"
        );
        // tc_beta is untouched
        let kept = hm
            .messages()
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("tc_beta"))
            .expect("tc_beta tool message must still exist");
        assert_eq!(kept.content, "shell output here");
    }

    #[test]
    fn test_abandon_tool_result_returns_zero_when_id_unknown() {
        let mut hm = HistoryManager::new(10_000);
        hm.append(ChatMessage::assistant_with_tools(
            "checking",
            vec![make_tool_call("tc_alpha", "file_edit", "{}")],
        ));
        hm.append(make_tool_message(
            "tc_alpha",
            "file_edit",
            "Edited 42 bytes",
        ));

        let replaced = hm.abandon_tool_result("tc_does_not_exist");

        assert_eq!(replaced, 0, "unknown id must not match anything");
        // tc_alpha's content must be untouched
        let kept = hm
            .messages()
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("tc_alpha"))
            .expect("tc_alpha must still exist");
        assert_eq!(kept.content, "Edited 42 bytes");
    }

    #[test]
    fn test_abandon_tool_result_is_idempotent() {
        let mut hm = HistoryManager::new(10_000);
        hm.append(ChatMessage::assistant_with_tools(
            "checking",
            vec![make_tool_call("tc_alpha", "file_edit", "{}")],
        ));
        hm.append(make_tool_message(
            "tc_alpha",
            "file_edit",
            "Edited 42 bytes",
        ));

        assert_eq!(hm.abandon_tool_result("tc_alpha"), 1, "first fold replaces");
        assert_eq!(
            hm.abandon_tool_result("tc_alpha"),
            0,
            "second fold is no-op (idempotent)"
        );

        // Content must be exactly the placeholder, not "placeholder + original".
        let folded = hm
            .messages()
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("tc_alpha"))
            .expect("tc_alpha must still exist");
        assert!(
            folded
                .content
                .starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX),
            "content must be the single placeholder"
        );
    }

    #[test]
    fn test_abandon_tool_result_preserves_schema() {
        // The schema invariant: after folding, the assistant's tool_calls
        // still point at tool messages that exist (now folded, but present).
        let mut hm = HistoryManager::new(10_000);
        hm.append(ChatMessage::assistant_with_tools(
            "checking",
            vec![
                make_tool_call("tc_alpha", "file_edit", "{}"),
                make_tool_call("tc_beta", "bash", "{}"),
            ],
        ));
        hm.append(make_tool_message(
            "tc_alpha",
            "file_edit",
            "Edited 42 bytes",
        ));
        hm.append(make_tool_message("tc_beta", "bash", "shell output here"));

        let _ = hm.abandon_tool_result("tc_alpha");

        // Sanity-check schema: every id in assistant.tool_calls still
        // has a matching Tool message (sanitize_messages is happy).
        let mut sanitized: Vec<ChatMessage> = hm.messages().to_vec();
        HistoryManager::sanitize_messages(&mut sanitized);
        assert_eq!(
            sanitized.len(),
            hm.messages().len(),
            "sanitize must not drop anything — schema was preserved by folding"
        );
    }

    #[test]
    fn test_make_compressed_placeholder_format() {
        let p = make_compressed_placeholder("file_edit");
        assert!(p.starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX));
        assert!(
            p.contains("file_edit"),
            "tool name must appear in placeholder"
        );
        assert!(
            p.contains("re-invoke"),
            "placeholder must hint at re-invoking the tool (no retrieve path)"
        );
        assert!(
            !p.contains("context_retrieve"),
            "placeholder must NOT reference the deprecated context_retrieve tool"
        );
        // Length bound (worst case for a 64-char tool name ≈ 130 bytes).
        assert!(p.len() < 200, "placeholder must stay compact");
    }

    // ── ADR-061 v3 §20.3.1 / PR2: clear_round tests ─────────────────────

    #[test]
    fn test_clear_round_folds_all_tool_results_of_round() {
        let mut hm = HistoryManager::new(10_000);
        hm.append(ChatMessage::assistant_with_tools(
            "doing two things",
            vec![
                make_tool_call("tc_a", "file_edit", "{}"),
                make_tool_call("tc_b", "bash", "{}"),
            ],
        ));
        hm.append(make_tool_message(
            "tc_a",
            "file_edit",
            "long file content A",
        ));
        hm.append(make_tool_message("tc_b", "bash", "long shell output B"));

        let asst_idx = 0;
        let report = hm.clear_round(asst_idx);

        assert_eq!(report.cleared_tool_ids.len(), 2);
        assert!(
            report.bytes_reclaimed > 0,
            "should reclaim at least the original content bytes"
        );
        // Both tool messages must now be placeholders.
        for msg in hm.messages() {
            if matches!(msg.role, MessageRole::Tool) {
                assert!(
                    msg.content.starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX),
                    "tool message at idx {} must be folded, got: {:?}",
                    0,
                    msg.content
                );
            }
        }
    }

    #[test]
    fn test_clear_round_is_idempotent() {
        let mut hm = HistoryManager::new(10_000);
        hm.append(ChatMessage::assistant_with_tools(
            "doing",
            vec![make_tool_call("tc_a", "file_edit", "{}")],
        ));
        hm.append(make_tool_message("tc_a", "file_edit", "content"));

        let r1 = hm.clear_round(0);
        let r2 = hm.clear_round(0);
        assert_eq!(r1.cleared_tool_ids.len(), 1);
        assert_eq!(r2.cleared_tool_ids.len(), 0, "second call must be no-op");
        assert_eq!(r2.bytes_reclaimed, 0);
    }

    #[test]
    fn test_clear_round_no_op_when_round_has_no_tools() {
        let mut hm = HistoryManager::new(10_000);
        hm.append(make_message(MessageRole::Assistant, "just text"));
        hm.append(make_message(MessageRole::User, "next user"));

        let report = hm.clear_round(0);
        assert!(report.cleared_tool_ids.is_empty());
        assert_eq!(report.bytes_reclaimed, 0);
        // No messages should have been mutated.
        assert_eq!(hm.messages()[0].content, "just text");
        assert_eq!(hm.messages()[1].content, "next user");
    }

    #[test]
    fn test_clear_round_selective_only_target_round() {
        // Two rounds: round-A has tc_1, round-B has tc_2. clear_round(round-A)
        // must fold tc_1's tool message but leave tc_2's tool message alone.
        let mut hm = HistoryManager::new(10_000);
        hm.append(ChatMessage::assistant_with_tools(
            "round A",
            vec![make_tool_call("tc_1", "file_edit", "{}")],
        ));
        hm.append(make_tool_message("tc_1", "file_edit", "A content"));
        hm.append(make_message(MessageRole::User, "user 2"));
        hm.append(ChatMessage::assistant_with_tools(
            "round B",
            vec![make_tool_call("tc_2", "bash", "{}")],
        ));
        hm.append(make_tool_message("tc_2", "bash", "B content"));

        // round A is the first assistant (idx 0).
        let _ = hm.clear_round(0);

        let tc1 = hm
            .messages()
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("tc_1"))
            .unwrap();
        assert!(
            tc1.content.starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX),
            "round-A tool must be folded"
        );
        let tc2 = hm
            .messages()
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("tc_2"))
            .unwrap();
        assert_eq!(tc2.content, "B content", "round-B tool must be untouched");
    }

    #[test]
    fn test_clear_round_preserves_schema() {
        // After folding, sanitize_messages must not drop anything.
        let mut hm = HistoryManager::new(10_000);
        hm.append(ChatMessage::assistant_with_tools(
            "A",
            vec![
                make_tool_call("tc_1", "file_edit", "{}"),
                make_tool_call("tc_2", "bash", "{}"),
            ],
        ));
        hm.append(make_tool_message("tc_1", "file_edit", "alpha"));
        hm.append(make_tool_message("tc_2", "bash", "beta"));

        let _ = hm.clear_round(0);

        let mut sanitized = hm.messages().to_vec();
        HistoryManager::sanitize_messages(&mut sanitized);
        assert_eq!(sanitized.len(), hm.messages().len());
    }

    #[test]
    #[should_panic(expected = "is not an Assistant")]
    fn test_clear_round_panics_on_non_assistant_index() {
        let mut hm = HistoryManager::new(10_000);
        hm.append(make_message(MessageRole::User, "hello"));
        let _ = hm.clear_round(0); // must panic: idx 0 is User, not Assistant
    }

    // ── ADR-061 v3 §20.3.3 / PR2: fix_round tests ───────────────────────

    #[test]
    fn test_fix_round_clears_then_sweeps_orphans() {
        // Round at idx 1: assistant claims tc_alpha + tc_orphan. tc_alpha
        // has a tool message; tc_orphan does NOT (it's a leak from a
        // malformed streaming response). Plus there's a stray tool
        // message for tc_stray that no assistant claims.
        let mut hm = HistoryManager::new(10_000);
        hm.append(make_message(MessageRole::User, "Q"));
        hm.append(ChatMessage::assistant_with_tools(
            "A",
            vec![
                make_tool_call("tc_alpha", "file_edit", "{}"),
                make_tool_call("tc_orphan", "bash", "{}"),
            ],
        ));
        hm.append(make_tool_message("tc_alpha", "file_edit", "alpha content"));
        // tc_orphan has NO matching Tool message.
        hm.append(make_tool_message("tc_stray", "bash", "stray content")); // orphan Tool

        let report = hm.fix_round(1);

        // clear_round part: tc_alpha folded
        assert!(
            report
                .cleared
                .cleared_tool_ids
                .contains(&"tc_alpha".to_string())
        );
        // orphan sweep part: tc_stray removed, tc_orphan id removed from assistant
        assert_eq!(report.removed_orphan_tool_messages, 1);
        assert!(
            report
                .removed_orphan_tool_call_ids
                .contains(&"tc_orphan".to_string())
        );
    }

    #[test]
    fn test_fix_round_preserves_schema_and_idempotent() {
        let mut hm = HistoryManager::new(10_000);
        hm.append(ChatMessage::assistant_with_tools(
            "A",
            vec![make_tool_call("tc_a", "file_edit", "{}")],
        ));
        hm.append(make_tool_message("tc_a", "file_edit", "content"));

        let r1 = hm.fix_round(0);
        let r2 = hm.fix_round(0);
        // r1 did the work, r2 is a no-op
        assert_eq!(r1.cleared.bytes_reclaimed, "content".len());
        assert_eq!(r2.cleared.bytes_reclaimed, 0);
        // Schema invariant: sanitize doesn't drop anything after fix_round.
        let mut sanitized = hm.messages().to_vec();
        HistoryManager::sanitize_messages(&mut sanitized);
        assert_eq!(sanitized.len(), hm.messages().len());
    }

    #[test]
    fn test_fix_round_handles_clean_round_no_orphans() {
        // Round with no orphans: fix_round must produce empty orphan report.
        let mut hm = HistoryManager::new(10_000);
        hm.append(ChatMessage::assistant_with_tools(
            "A",
            vec![make_tool_call("tc_a", "file_edit", "{}")],
        ));
        hm.append(make_tool_message("tc_a", "file_edit", "content"));

        let report = hm.fix_round(0);
        assert_eq!(report.removed_orphan_tool_messages, 0);
        assert!(report.removed_orphan_tool_call_ids.is_empty());
        // clear_round still ran.
        assert!(
            report
                .cleared
                .cleared_tool_ids
                .contains(&"tc_a".to_string())
        );
    }

    // ── ADR-061 v3 §20.3.2 / PR2: recall_todo_round tests ───────────────

    #[test]
    fn test_recall_todo_round_no_marker() {
        let mut hm = HistoryManager::new(10_000);
        // No compaction marker — recall should return NoMarker.
        hm.append(make_todo_write_assistant("tc_t1"));
        hm.append(make_todo_write_tool("tc_t1"));
        let result = hm.recall_todo_round();
        assert!(matches!(result, RecallResult::NoMarker));
    }

    #[test]
    fn test_recall_todo_round_no_todo_round_returns_notfound() {
        let mut hm = HistoryManager::new(10_000);
        // Marker but no todo_write round.
        append_marker(&mut hm);
        hm.append(make_message(MessageRole::User, "post-marker user"));
        hm.append(make_message(
            MessageRole::Assistant,
            "post-marker assistant",
        ));
        let result = hm.recall_todo_round();
        assert!(matches!(result, RecallResult::NoTodoRoundFound));
    }

    #[test]
    fn test_recall_todo_round_skips_when_in_tail() {
        // Todo round is already in the retained tail (after marker).
        // Recall must detect and return SkippedAlreadyInTail.
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::System, "sys"));
        hm.append(make_message(MessageRole::User, "Q1"));
        append_marker(&mut hm);
        // Tail: a todo_write round right after the marker.
        hm.append(make_todo_write_assistant("tc_tail_todo"));
        hm.append(make_todo_write_tool("tc_tail_todo"));

        let result = hm.recall_todo_round();
        // The todo round is already present in tail; recall must not
        // duplicate it.
        assert!(
            matches!(
                result,
                RecallResult::SkippedAlreadyInTail | RecallResult::NoTodoRoundFound
            ),
            "expected skip or notfound, got {:?}",
            std::mem::discriminant(&result)
        );
    }

    #[test]
    fn test_recall_todo_round_idempotent_on_repeat_call() {
        // First recall after a fresh compression injects; second recall
        // (without further mutation) returns SkippedAlreadyInjected.
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::System, "sys"));
        hm.append(make_message(MessageRole::User, "Q1"));
        hm.append(make_todo_write_assistant("tc_todo_1"));
        hm.append(make_todo_write_tool("tc_todo_1"));
        // Marker mid-history.
        append_marker(&mut hm);
        hm.append(make_message(MessageRole::User, "post-marker user"));

        let r1 = hm.recall_todo_round();
        let r2 = hm.recall_todo_round();

        // r1 should inject; r2 should be skipped-already-injected.
        assert!(
            matches!(r1, RecallResult::Injected { .. }),
            "first call: {:?}",
            r1
        );
        assert!(
            matches!(r2, RecallResult::SkippedAlreadyInjected),
            "second call: {:?}",
            r2
        );
    }

    // ── replace_middle_with_summary tests ────────────────────────────────

    #[test]
    fn test_replace_middle_keeps_complete_tool_call_rounds() {
        // Scenario: 4 user messages, each followed by Assistant tc + Tool result.
        // With keep_last_rounds=3, Q4 should be complete, Q1 should be compacted.
        // The core fix ensures any Tool message kept in tail has its matching
        // Assistant preserved (no orphaned tool results that sanitize would remove).
        let mut hm = HistoryManager::new(100000);
        hm.append(make_message(MessageRole::System, "System prompt"));

        // Q1
        hm.append(make_message(MessageRole::User, "Question 1"));
        hm.append(ChatMessage::assistant_with_tools(
            "Searching",
            vec![make_tool_call("tc_1", "search", "{}")],
        ));
        hm.append(make_tool_result("tc_1", "Result for Q1"));
        hm.append(make_message(MessageRole::Assistant, "Answer 1"));

        // Q2
        hm.append(make_message(MessageRole::User, "Question 2"));
        hm.append(ChatMessage::assistant_with_tools(
            "Searching again",
            vec![make_tool_call("tc_2", "search", "{}")],
        ));
        hm.append(make_tool_result("tc_2", "Result for Q2"));
        hm.append(make_message(MessageRole::Assistant, "Answer 2"));

        // Q3
        hm.append(make_message(MessageRole::User, "Question 3"));
        hm.append(ChatMessage::assistant_with_tools(
            "Searching third",
            vec![make_tool_call("tc_3", "search", "{}")],
        ));
        hm.append(make_tool_result("tc_3", "Result for Q3"));
        hm.append(make_message(MessageRole::Assistant, "Answer 3"));

        // Q4
        hm.append(make_message(MessageRole::User, "Question 4"));
        hm.append(ChatMessage::assistant_with_tools(
            "Searching fourth",
            vec![make_tool_call("tc_4", "search", "{}")],
        ));
        hm.append(make_tool_result("tc_4", "Result for Q4"));
        hm.append(make_message(MessageRole::Assistant, "Answer 4"));

        let removed = hm.replace_middle_with_summary("Summary Q1", 3);
        assert!(removed > 0, "Should compact some messages");

        let messages = hm.messages();

        // Q1 (tc_1) should be compacted
        let has_tc1 = messages.iter().any(|m| {
            m.tool_calls
                .as_ref()
                .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == "tc_1"))
        });
        assert!(!has_tc1, "Q1 should be compacted");

        // Q4 must be complete (User + Assistant tc + Tool result)
        let has_tc4_call = messages.iter().any(|m| {
            m.tool_calls
                .as_ref()
                .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == "tc_4"))
        });
        assert!(has_tc4_call, "Q4 tool_call should be preserved");
        let has_tc4_result = messages
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("tc_4"));
        assert!(has_tc4_result, "Q4 tool result should be preserved");

        // Key assertion: sanitize should NOT remove any messages from the tail.
        // Before the fix, orphaned Tool results (preserved without their
        // Assistant) would be cleaned up here.
        let mut messages_clone = messages.to_vec();
        let len_before = messages_clone.len();
        HistoryManager::sanitize_messages(&mut messages_clone);
        assert_eq!(messages_clone.len(), len_before, "No orphans after fix");

        // All Tool messages still present after sanitize must have matching
        // Assistant with tool_calls.
        for msg in &messages_clone {
            if msg.role == MessageRole::Tool
                && let Some(ref tcid) = msg.tool_call_id
            {
                let has_call = messages_clone.iter().any(|m| {
                    m.tool_calls
                        .as_ref()
                        .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == *tcid))
                });
                assert!(has_call, "Tool result {tcid} has matching Assistant");
            }
        }
    }

    #[test]
    fn test_replace_middle_single_user_many_tools() {
        // Scenario: 1 user message followed by many tool-calling rounds.
        // With keep_last_rounds=2, tail should keep the last 2 complete
        // Assistant+Tool pairs (expanded from idx).
        let mut hm = HistoryManager::new(100000);
        hm.append(make_message(MessageRole::System, "System"));
        hm.append(make_message(MessageRole::User, "Complex task"));

        // 5 rounds of tool calls
        for i in 1..=5 {
            hm.append(ChatMessage::assistant_with_tools(
                format!("Round {i}"),
                vec![make_tool_call(&format!("tc_{i}"), "tool", "{}")],
            ));
            hm.append(make_tool_result(&format!("tc_{i}"), &format!("Result {i}")));
        }

        let removed = hm.replace_middle_with_summary("Summary of rounds 1-3", 2);
        assert!(removed > 0);

        let messages = hm.messages();

        // Should have: [System] [User compaction_summary] [Assistant Round 4] [Tool tc_4]
        //              [Assistant Round 5] [Tool tc_5]

        // Verify compaction summary exists (User role, marker by name)
        let has_summary = messages.iter().any(|m| {
            m.role == MessageRole::User && m.name.as_deref() == Some(COMPACTION_SUMMARY_NAME)
        });
        assert!(has_summary, "Compaction summary should be present");

        // Verify rounds 4 and 5 are complete (no orphans)
        for i in 4..=5 {
            let tc_id = format!("tc_{i}");
            let has_call = messages.iter().any(|m| {
                m.tool_calls
                    .as_ref()
                    .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == tc_id))
            });
            assert!(has_call, "Tool call {tc_id} should be preserved");

            let has_result = messages
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some(&tc_id));
            assert!(has_result, "Tool result {tc_id} should be preserved");
        }

        // Verify rounds 1-3 are NOT present (compacted)
        for i in 1..=3 {
            let tc_id = format!("tc_{i}");
            let has_call = messages.iter().any(|m| {
                m.tool_calls
                    .as_ref()
                    .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == tc_id))
            });
            assert!(!has_call, "Tool call {tc_id} should be compacted");
        }

        // sanitize should not remove anything
        let mut messages_clone = messages.to_vec();
        let len_before = messages_clone.len();
        HistoryManager::sanitize_messages(&mut messages_clone);
        assert_eq!(messages_clone.len(), len_before, "No orphans after fix");
    }

    // ─────────────────────────────────────────────────────────────────────
    // compact_via_llm language-aware system-prompt tests
    //
    // These verify that the user's identity_context (containing the
    // `Language: zh-CN` field) is embedded into the system message sent to
    // the compact model, so the LLM writes the summary in the user's
    // preferred language.
    // ─────────────────────────────────────────────────────────────────────

    use acowork_core::providers::traits::ChatResponse;
    use std::sync::{Arc, Mutex};

    /// Minimal Provider that captures the most recent `ChatRequest` and
    /// returns a canned summary. Used to assert what `compact_via_llm`
    /// actually sends to the LLM.
    struct CaptureProvider {
        captured: Arc<Mutex<Option<ChatRequest>>>,
        canned: String,
    }

    impl CaptureProvider {
        fn new(canned: impl Into<String>) -> Self {
            Self {
                captured: Arc::new(Mutex::new(None)),
                canned: canned.into(),
            }
        }
        fn last_request(&self) -> ChatRequest {
            self.captured
                .lock()
                .unwrap()
                .take()
                .expect("provider was never called")
        }
    }

    #[async_trait::async_trait]
    impl Provider for CaptureProvider {
        fn name(&self) -> &str {
            "capture"
        }

        async fn chat(&self, request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
            *self.captured.lock().unwrap() = Some(request);
            Ok(ChatResponse {
                content: self.canned.clone(),
                ..Default::default()
            })
        }

        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> acowork_core::error::Result<
            Box<
                dyn futures_core::Stream<Item = acowork_core::providers::traits::StreamEvent>
                    + Send,
            >,
        > {
            Err(acowork_core::error::AcoworkError::Provider(
                acowork_core::providers::traits::ProviderError::unknown(
                    "CaptureProvider does not support streaming".to_string(),
                ),
            ))
        }

        async fn chat_token_count(
            &self,
            _messages: &[ChatMessage],
        ) -> acowork_core::error::Result<u64> {
            Ok(0)
        }
    }

    fn build_history_with_messages() -> HistoryManager {
        let mut hm = HistoryManager::new(10_000);
        hm.append(make_message(MessageRole::User, "用户：你好"));
        hm.append(make_message(
            MessageRole::Assistant,
            "你好！有什么可以帮你的吗？",
        ));
        hm
    }

    #[tokio::test]
    async fn compact_via_llm_without_identity_keeps_system_prompt_unchanged() {
        let hm = build_history_with_messages();
        // Stub output must pass the compact_with_llm quality gate (≥20 chars).
        let provider =
            CaptureProvider::new("<summary>a valid compact model summary output here</summary>");

        let result = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                None,
            )
            .await;
        assert!(result.is_ok(), "compact_via_llm should succeed");

        let req = provider.last_request();
        // Two messages: system + user
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, MessageRole::System);
        assert_eq!(
            req.messages[0].content,
            crate::prompt::COMPACTION_SYSTEM_PROMPT,
            "with identity=None, system prompt must be the base prompt unchanged"
        );
        // User message keeps the unmodified COMPACT_PROMPT template — body
        // only, no summarization instructions (those live in
        // COMPACTION_SYSTEM_PROMPT now per the prompt.rs architecture note).
        assert!(req.messages[1].content.contains("<conversation>"));
        assert!(req.messages[1].content.contains("</conversation>"));
    }

    #[tokio::test]
    async fn compact_via_llm_with_identity_embeds_language_directive_into_system() {
        let hm = build_history_with_messages();
        // Stub output must pass the compact_with_llm quality gate (≥20 chars).
        let provider =
            CaptureProvider::new("<summary>a valid compact model summary output here</summary>");

        let identity =
            "- Display Name: 大鱼\n- Language: zh-CN\n- Timezone: Asia/Shanghai\n- City: 上海";

        let result = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                Some(identity),
            )
            .await;
        assert!(result.is_ok());

        let req = provider.last_request();
        assert_eq!(req.messages.len(), 2);
        let system = &req.messages[0].content;
        assert_eq!(
            system[0..crate::prompt::COMPACTION_SYSTEM_PROMPT.len()].to_string(),
            crate::prompt::COMPACTION_SYSTEM_PROMPT,
            "system prompt must start with the original base prompt"
        );
        // Identity text embedded verbatim — the LLM reads it directly
        assert!(
            system.contains(identity),
            "identity text must be embedded verbatim"
        );
        assert!(
            system.contains("Language"),
            "language directive must be present"
        );
        assert!(
            system.contains("preferred language"),
            "language directive must be present"
        );
        // COMPACT_PROMPT (user) is now body-only — summarization role lives
        // in COMPACTION_SYSTEM_PROMPT (system). Assert the split:
        // - system carries the summarization role/instructions
        // - user stays focused on the conversation body, free of role leakage
        assert!(
            req.messages[0].content.contains("summarizes conversations"),
            "system prompt must carry the summarization role/instructions"
        );
        assert!(
            !req.messages[1].content.contains("summarizes conversations"),
            "user message must stay focused on conversation body, not leak system role instructions"
        );
    }

    #[tokio::test]
    async fn compact_via_llm_with_empty_identity_keeps_system_prompt_unchanged() {
        let hm = build_history_with_messages();
        // Stub output must carry a valid <summary> block — the quality gate in
        // compact_with_llm rejects marker-less output (no raw-text fallback).
        let provider =
            CaptureProvider::new("<summary>a valid compact model summary here</summary>");

        let result = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                Some("   \n\t  "),
            )
            .await;
        assert!(result.is_ok());

        let req = provider.last_request();
        assert_eq!(
            req.messages[0].content,
            crate::prompt::COMPACTION_SYSTEM_PROMPT,
            "whitespace-only identity must not append the directive"
        );
    }

    // ── Quality gate rejection paths (P1: quality-over-nothing) ───────────

    #[tokio::test]
    async fn compact_via_llm_rejects_too_short_summary() {
        let hm = build_history_with_messages();
        // A placeholder summary (< MIN_SUMMARY_CHARS) must fail the quality
        // gate — the output is discarded, never stored.
        let provider = CaptureProvider::new("<summary>ok</summary>");
        let err = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeError::Summary(crate::episode_distill::SummaryError::LowQuality(_))
            ),
            "placeholder summary must fail the quality gate, got: {err}"
        );
    }

    #[tokio::test]
    async fn compact_via_llm_rejects_markerless_reasoning_dump() {
        let hm = build_history_with_messages();
        // Reasoning-model dump without the <summary> marker — the exact
        // pollution shape from the pasted-text incident. Must error, never
        // fall back to the raw text as a summary.
        let provider = CaptureProvider::new(
            "确认 disable 路径已清理 pending 槽位。\n开始实施。改两处：\n1. 删除 resolve_distill_model 调用\n2. 验证 fallback 链",
        );
        let err = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeError::Summary(crate::episode_distill::SummaryError::MissingBlock)
            ),
            "marker-less dump must fail the quality gate, got: {err}"
        );
    }

    #[tokio::test]
    async fn compact_via_llm_rejects_empty_content() {
        let hm = build_history_with_messages();
        // Empty content (model ignored thinking_mode=disabled and put its
        // output in reasoning_content) must surface as SummaryError::Empty.
        let provider = CaptureProvider::new("");
        let err = hm
            .compact_via_llm(
                &provider,
                "compact-model",
                crate::prompt::COMPACTION_SYSTEM_PROMPT,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeError::Summary(crate::episode_distill::SummaryError::Empty(_))
            ),
            "empty content must surface as SummaryError::Empty, got: {err}"
        );
    }

    // \u2500\u2500 ADR-061 v3: 5-level round-atomic plan/apply tests \u2500

    /// Build 7 full rounds: System + (User \u2192 Assistant{tool_calls} \u2192 Tool \u2192
    /// Assistant) \u00d7 7. Tool results carry extra text so level projections
    /// carry real token weight.
    fn build_7_round_history() -> HistoryManager {
        let mut hm = HistoryManager::new(1_000_000);
        hm.append(make_message(MessageRole::System, "System prompt"));
        for i in 1..=7 {
            hm.append(make_message(MessageRole::User, &format!("Question {i}")));
            hm.append(ChatMessage::assistant_with_tools(
                format!("Searching {i}"),
                vec![make_tool_call(&format!("tc_{i}"), "search", "{}")],
            ));
            hm.append(make_tool_result(
                &format!("tc_{i}"),
                &format!(
                    "Tool result for round {i} with a long payload so this \
                     message carries real token weight in every level projection"
                ),
            ));
            hm.append(make_message(MessageRole::Assistant, &format!("Answer {i}")));
        }
        hm
    }

    fn user_count(msgs: &[ChatMessage]) -> usize {
        msgs.iter()
            .filter(|m| matches!(m.role, MessageRole::User))
            .count()
    }

    fn has_tool_call(msgs: &[ChatMessage], id: &str) -> bool {
        msgs.iter().any(|m| {
            m.tool_calls
                .as_ref()
                .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == id))
        })
    }

    fn has_tool_result(msgs: &[ChatMessage], id: &str) -> bool {
        msgs.iter().any(|m| m.tool_call_id.as_deref() == Some(id))
    }

    /// Find the Tool message answering `tool_call_id`.
    fn tool_msg<'a>(msgs: &'a [ChatMessage], id: &str) -> Option<&'a ChatMessage> {
        msgs.iter()
            .find(|m| matches!(m.role, MessageRole::Tool) && m.tool_call_id.as_deref() == Some(id))
    }

    /// v3 schema invariants over a message list:
    /// 1. every `assistant.tool_calls[*].id` resolves to a Tool message in
    ///    the same buffer (no dangling / orphan id — v2's ghost bug);
    /// 2. no two Assistant messages are adjacent (dropping a round must not
    ///    fuse its neighbours into an illegal `Assistant \u2192 Assistant` pair).
    fn assert_schema_and_no_adjacency(msgs: &[ChatMessage], label: &str) {
        let tool_ids: Vec<&str> = msgs
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        for (i, m) in msgs.iter().enumerate() {
            if i > 0
                && matches!(msgs[i - 1].role, MessageRole::Assistant)
                && matches!(m.role, MessageRole::Assistant)
            {
                panic!("{label}: adjacent Assistant messages at index {i}");
            }
            if !matches!(m.role, MessageRole::Assistant) {
                continue;
            }
            let Some(tcs) = m.tool_calls.as_ref() else {
                continue;
            };
            for tc in tcs {
                assert!(
                    tool_ids.contains(&tc.id.as_str()),
                    "{label}: orphan tool_call id {} (assistant kept but Tool dropped)",
                    tc.id
                );
            }
        }
    }

    /// v3 ghost-bug regression (level 1): the non-final kept rounds are
    /// folded into placeholders — their assistant KEEPS `tool_calls` and the
    /// Tool message stays present (schema intact). v2 stripped the dangling
    /// tool_calls and silently degraded these assistants into text-only
    /// ghosts (deepseek-v4-flash incident, ADR-061 \u00a720.1).
    #[test]
    fn v3_level1_folds_non_final_rounds_keeps_schema() {
        let hm = build_7_round_history();
        let plan = hm.build_level_plan(1, hm.messages(), "Summary");

        // Rounds 1-5 are dropped whole (subsumed by the summary): neither
        // their tool_call nor their tool result survives.
        for id in ["tc_1", "tc_2", "tc_3", "tc_4", "tc_5"] {
            assert!(!has_tool_call(&plan.retained, id), "{id} call must be gone");
            assert!(
                !has_tool_result(&plan.retained, id),
                "{id} result must be gone"
            );
        }

        // tc_6: kept round, folded — assistant keeps its tool_call entry and
        // the Tool message is present as a placeholder.
        assert!(
            has_tool_call(&plan.retained, "tc_6"),
            "folded round's assistant must keep its tool_calls (no sweep)"
        );
        let tool6 = tool_msg(&plan.retained, "tc_6").expect("tc_6 Tool retained");
        assert!(
            tool6
                .content
                .starts_with(COMPRESSED_TOOL_PLACEHOLDER_PREFIX),
            "tc_6 Tool must be a placeholder"
        );

        // tc_7: final round stays verbatim.
        let tool7 = tool_msg(&plan.retained, "tc_7").expect("tc_7 Tool retained");
        assert!(
            tool7.content.contains("Tool result for round 7"),
            "final round's tool result must stay verbatim"
        );
        assert_schema_and_no_adjacency(&plan.retained, "L1 plan");
    }

    /// The 5-level table (ADR-061 v3 6.2): per level the fixture must
    /// keep exactly (users, assistants, tools) as follows, and every level
    /// must satisfy the round-atomic schema invariants.
    #[test]
    fn v3_level_table_round_atomic_counts() {
        let hm = build_7_round_history();
        // (level, users kept, assistants kept, tool messages kept)
        let table: [(u8, usize, usize, usize); 4] = [
            (1, 7, 5, 2), // 5 assistants; last tool round verbatim, tc_6 folded
            (2, 7, 5, 2), // same window; both kept tools folded
            (3, 7, 3, 1), // last 3 assistants; only tc_7 round kept, folded
            (4, 7, 1, 0), // last 1 assistant (text answer; no tools)
        ];
        for (level, users, assts, tools) in table {
            let plan = hm.build_level_plan(level, hm.messages(), "Summary");
            assert_eq!(user_count(&plan.retained), users, "L{level} users");
            assert_eq!(
                plan.stats.assistant_messages, assts,
                "L{level} assistant messages kept"
            );
            assert_eq!(
                plan.stats.tool_messages, tools,
                "L{level} tool messages kept"
            );
            assert_schema_and_no_adjacency(&plan.retained, &format!("L{level} plan"));
        }
    }

    #[test]
    fn v3_plan_projections_monotonic_and_first_fit() {
        let hm = build_7_round_history();
        let projections: Vec<u64> = (1..=5)
            .map(|l| {
                hm.build_level_plan(l, hm.messages(), "Summary")
                    .projected_tokens
            })
            .collect();
        // v3 invariant: more aggressive levels never retain more.
        for w in projections.windows(2) {
            assert!(
                w[0] >= w[1],
                "level projections must be monotonic non-increasing: {w:?}"
            );
        }
        assert!(
            projections[0] > projections[4],
            "level 5 must be strictly smaller"
        );

        // Relaxed 10% bar: level 1 is the first fit and wins.
        let plan = hm
            .plan_compression("Summary", 0.10)
            .expect("level 1 must fit");
        assert_eq!(
            plan.level, 1,
            "first sufficient level wins under a relaxed bar"
        );
    }

    #[test]
    fn v3_level5_exempt_from_ratio_keeps_last_non_marker_user() {
        // Conversation dominated by the system block: levels 1-4 drop nothing
        // (ratio ~0% < 10%) yet level 5 must still be accepted on the budget
        // check alone (19.2 ratio exemption).
        let mut hm = HistoryManager::new(1_000_000);
        hm.append(make_message(
            MessageRole::System,
            &"System prompt ".repeat(600),
        ));
        hm.append(ChatMessage {
            role: MessageRole::User,
            content: "Previous compaction summary".to_string(),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        });
        hm.append(make_message(MessageRole::User, "Question 1"));
        hm.append(make_message(MessageRole::Assistant, "Answer 1"));
        hm.append(make_message(MessageRole::User, "Question 2"));

        let p5 = hm
            .build_level_plan(5, hm.messages(), "Summary")
            .projected_tokens;
        hm.set_max_tokens(p5);
        let plan = hm
            .plan_compression("Summary", MIN_COMPRESSION_RATIO)
            .expect("level 5 must fit");
        assert_eq!(plan.level, 5, "only level 5 fits; ratio exemption required");

        // Level 5 retains system + the LAST non-marker User (the earlier
        // marker is User-role too and must be excluded, 19.1).
        assert_eq!(
            user_count(&plan.retained),
            1,
            "only the current user message"
        );
        assert_eq!(
            plan.retained.last().map(|m| m.content.as_str()),
            Some("Question 2"),
            "last real user message survives, marker does not"
        );
        assert_eq!(plan.stats.assistant_messages, 0);
        assert_eq!(plan.stats.tool_messages, 0);
    }

    #[test]
    fn v3_plan_unrecoverable_overflow_when_level5_exceeds_budget() {
        let mut hm = build_7_round_history();
        let p5 = hm
            .build_level_plan(5, hm.messages(), "Summary")
            .projected_tokens;
        hm.set_max_tokens(p5 - 1);
        let err = hm
            .plan_compression("Summary", MIN_COMPRESSION_RATIO)
            .unwrap_err();
        assert!(
            matches!(err, CompressError::UnrecoverableOverflow { .. }),
            "level 5 cannot fit => explicit failure, history untouched: {err}"
        );
        assert_eq!(
            hm.messages().len(),
            29,
            "planning must never mutate history"
        );
    }

    #[test]
    fn v3_apply_marker_contract_and_user_preservation() {
        let mut hm = build_7_round_history();
        let plan = hm
            .plan_compression("A solid summary of everything so far", 0.10)
            .expect("plan");
        assert_eq!(plan.level, 1);
        let _ = hm
            .apply_compression(plan, "A solid summary of everything so far", 0.10)
            .expect("apply");

        let msgs = hm.messages();
        let split = msgs
            .iter()
            .take_while(|m| matches!(m.role, MessageRole::System))
            .count();
        assert_eq!(
            msgs[split].name.as_deref(),
            Some(COMPACTION_SUMMARY_NAME),
            "marker sits right after the leading system block"
        );
        // 7 real users + the new marker (markers are User-role too).
        assert_eq!(
            user_count(msgs),
            8,
            "all user messages preserved plus marker"
        );
        assert_schema_and_no_adjacency(msgs, "post-apply L1");
    }

    #[test]
    fn v3_apply_rejects_insufficient_plan_without_touching_history() {
        let mut hm = build_7_round_history();
        let plan = hm.build_level_plan(1, hm.messages(), "Summary");
        let before = format!("{:?}", hm.messages());
        let err = hm
            .apply_compression(plan, "Summary", 1.0)
            .expect_err("L1 ratio < 1.0 => must be rejected");
        assert!(matches!(err, CompressError::InsufficientCompression { .. }));
        assert_eq!(
            format!("{:?}", hm.messages()),
            before,
            "history must be untouched on reject"
        );
    }

    /// E2E ghost-bug regression: applying ANY v3 level must produce a
    /// message list that survives `sanitize_messages` untouched — no orphan
    /// tool_call, no adjacent assistants, nothing for the sanitizer to strip.
    #[test]
    fn v3_apply_keeps_schema_intact_through_sanitize_all_levels() {
        for level in 1..=4u8 {
            let mut hm = build_7_round_history();
            let plan = hm.build_level_plan(level, hm.messages(), "Summary");
            let _ = hm
                .apply_compression(plan, "Summary", 0.0)
                .unwrap_or_else(|e| panic!("L{level} apply: {e}"));

            assert_schema_and_no_adjacency(hm.messages(), &format!("L{level} post-apply"));

            let mut sanitized = hm.messages().to_vec();
            let pre_count = sanitized.len();
            HistoryManager::sanitize_messages(&mut sanitized);
            let post_count = sanitized.len();
            assert_eq!(
                pre_count, post_count,
                "L{level}: sanitize must not drop any messages — v3 apply preserves schema"
            );
        }
    }

    #[test]
    fn v3_plan_default_ratio_falls_to_summary_floor() {
        // ADR-061 19.3: the default bar is 0.90 ("compress until at most
        // 10% remains"). The small 7-round fixture never removes 90% at
        // levels 1-4, so plan must fall through to level 5's ratio exemption.
        let hm = build_7_round_history();
        let plan = hm
            .plan_compression("Summary", MIN_COMPRESSION_RATIO)
            .expect("level 5 must still fit");
        assert_eq!(
            plan.level, 5,
            "under the default 0.90 bar every level 1-4 is skipped"
        );
    }

    #[test]
    fn test_build_summary_marker_metadata_format() {
        let stats = RetentionStats {
            user_messages: 7,
            assistant_messages: 5,
            tool_messages: 3,
            user_desc: "all".to_string(),
            assistant_desc: "last 5".to_string(),
            tool_desc: "within last 5 assistants".to_string(),
        };
        let marker = build_summary_marker(3, 1000, 400, 0.6, &stats, "body summary");
        assert!(marker.starts_with("[compressed: level=3]\n"));
        assert!(marker.contains("user_messages: all (7)"));
        assert!(marker.contains("assistant_messages: last 5 (5)"));
        assert!(marker.contains("tool_messages: within last 5 assistants (3)"));
        assert!(marker.contains("tokens: 1000 -> 400 (ratio 60.0%)"));
        assert!(marker.ends_with("body summary"));
    }

    // ── ADR-060 v2 §5.4: todo_write round preservation tests ───────────

    /// Build an Assistant message with a single todo_write tool_call.
    fn make_todo_write_assistant(tool_call_id: &str) -> ChatMessage {
        ChatMessage::assistant_with_tools(
            "",
            vec![make_tool_call(tool_call_id, "todo_write", "{\"todos\":[]}")],
        )
    }

    /// Build the canonical Tool result returned by AgentLoop for todo_write.
    fn make_todo_write_tool(tool_call_id: &str) -> ChatMessage {
        ChatMessage::tool(
            tool_call_id,
            "Todo list updated (1 items, merge=false):\n- [ ] task1",
        )
    }

    /// Insert a synthetic compaction summary marker into history.
    ///
    /// We bypass `apply_compression` to keep the unit tests focused on
    /// `inject_round_after_marker`'s contract rather than the
    /// compression pipeline. The marker contract is what matters: any
    /// `User` message with `name == COMPACTION_SUMMARY_NAME` will be
    /// detected by `last_compaction_index()`.
    fn append_marker(hm: &mut HistoryManager) {
        let marker = ChatMessage {
            role: MessageRole::User,
            content: "<summary>synthetic marker</summary>".to_string(),
            name: Some(COMPACTION_SUMMARY_NAME.to_string()),
            ..Default::default()
        };
        // Use append so token accounting stays correct.
        hm.append(marker);
    }

    #[test]
    fn find_returns_none_when_no_todo_write() {
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::User, "Hello"));
        hm.append(make_message(MessageRole::Assistant, "Hi"));
        assert!(
            hm.find_last_todo_write_round().is_none(),
            "no todo_write round in history"
        );
    }

    #[test]
    fn find_returns_pair_when_todo_write_present() {
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::User, "Build feature"));
        let assistant_orig = make_todo_write_assistant("tc_todo_1");
        hm.append(assistant_orig);
        let tool_orig = make_todo_write_tool("tc_todo_1");
        hm.append(tool_orig);
        hm.append(make_message(MessageRole::Assistant, "Done"));

        let (found_assistant, found_tool) =
            hm.find_last_todo_write_round().expect("must find round");

        // Canonical identifier: tool_call_id.
        let ass_id = &found_assistant.tool_calls.as_ref().unwrap()[0].id;
        assert_eq!(ass_id, "tc_todo_1");
        assert_eq!(found_tool.tool_call_id.as_deref(), Some("tc_todo_1"));
    }

    #[test]
    fn find_returns_latest_when_multiple_todo_writes() {
        let mut hm = HistoryManager::new(100_000);
        // First todo_write (older).
        hm.append(make_todo_write_assistant("tc_old"));
        hm.append(make_todo_write_tool("tc_old"));
        // Some other tool work in between.
        hm.append(ChatMessage::assistant_with_tools(
            "thinking",
            vec![make_tool_call("tc_other", "search", "{}")],
        ));
        hm.append(make_tool_result("tc_other", "result"));
        // Second todo_write (newer) — this is what we should find.
        hm.append(make_todo_write_assistant("tc_new"));
        hm.append(make_todo_write_tool("tc_new"));

        let (found_assistant, _) = hm.find_last_todo_write_round().unwrap();
        let ass_id = &found_assistant.tool_calls.as_ref().unwrap()[0].id;
        assert_eq!(ass_id, "tc_new", "must return LATEST todo_write");
    }

    #[test]
    fn inject_skips_when_round_already_in_tail() {
        // Scenario: compression kept the todo_write round in the retained
        // tail (via `keep_last_rounds` or low compression level). Injecting
        // the same round would duplicate the tool_call_id — `sanitize_messages`
        // would drop the orphan. Inject must detect and skip.
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::System, "sys"));
        hm.append(make_message(MessageRole::User, "Q1"));
        hm.append(make_message(MessageRole::Assistant, "A1"));
        // Simulate post-compression: marker inserted, retained tail starts here.
        append_marker(&mut hm);
        // Retained tail includes a complete todo_write round.
        hm.append(make_todo_write_assistant("tc_in_tail"));
        hm.append(make_todo_write_tool("tc_in_tail"));

        let len_before = hm.messages().len();
        let tokens_before = hm.token_count();

        // Inject the SAME round — should be a no-op (already in tail).
        let (assistant, tool) = hm.find_last_todo_write_round().unwrap();
        let (new_tokens, injected) = hm.inject_round_after_marker(assistant, tool);

        assert_eq!(
            hm.messages().len(),
            len_before,
            "no message inserted when round is already in retained tail"
        );
        assert_eq!(new_tokens, tokens_before, "token count unchanged on skip");
        assert!(
            !injected,
            "persistence gate must signal no-op when round is in retained tail"
        );
    }

    #[test]
    fn inject_splices_after_marker_when_round_removed() {
        // Scenario: compression removed the todo_write round from the
        // retained tail (e.g., level 8 emergency or aggressive keep_last).
        // Inject must splice the (Assistant, Tool) pair right after the
        // marker so the next LLM call sees the current todo state.
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::System, "sys"));
        append_marker(&mut hm);
        // Tail has other rounds but NOT the todo_write round.
        hm.append(make_message(MessageRole::User, "new Q"));
        hm.append(make_message(MessageRole::Assistant, "new A"));

        let len_before = hm.messages().len();

        // Build the round that was removed.
        let removed_assistant = make_todo_write_assistant("tc_removed");
        let removed_tool = make_todo_write_tool("tc_removed");

        let (_, injected) =
            hm.inject_round_after_marker(removed_assistant, removed_tool);
        assert!(injected, "splice path must signal injected=true");

        let msgs = hm.messages();
        assert_eq!(
            msgs.len(),
            len_before + 2,
            "two messages inserted (Assistant + Tool)"
        );

        // Marker position; expect Assistant+Tool immediately after.
        let marker_idx = msgs
            .iter()
            .position(|m| m.name.as_deref() == Some(COMPACTION_SUMMARY_NAME))
            .expect("marker present");
        let assistant_after = &msgs[marker_idx + 1];
        let tool_after = &msgs[marker_idx + 2];

        assert_eq!(assistant_after.role, MessageRole::Assistant);
        let ass_id = &assistant_after.tool_calls.as_ref().unwrap()[0].id;
        assert_eq!(
            ass_id, "tc_removed",
            "injected assistant right after marker"
        );
        assert_eq!(tool_after.role, MessageRole::Tool);
        assert_eq!(
            tool_after.tool_call_id.as_deref(),
            Some("tc_removed"),
            "injected tool right after assistant (round order preserved)"
        );

        // Sanity: the original tail (the last user+assistant pair) is still
        // at the end — injection happens right after the marker, BEFORE the
        // preserved tail.
        assert_eq!(
            msgs.last().unwrap().content,
            "new A",
            "original retained tail still at the end"
        );

        // The injected Tool must have a matching Assistant in messages — no
        // orphan for `sanitize_messages` to drop.
        let mut cloned = msgs.to_vec();
        let len_pre_sanitize = cloned.len();
        HistoryManager::sanitize_messages(&mut cloned);
        assert_eq!(
            cloned.len(),
            len_pre_sanitize,
            "no orphan Tool after injection (sanitize no-op)"
        );
    }

    #[test]
    fn inject_skips_on_tool_call_id_collision() {
        // Scenario: retained tail already has the same tool_call_id we'd
        // inject (rare but possible — e.g., duplicate id from a different
        // tool or pre-existing todo_write round). Inject must skip to avoid
        // two Tool messages with the same id, which `sanitize_messages`
        // would orphan-drop, losing the todo state.
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::System, "sys"));
        append_marker(&mut hm);
        // Tail already contains the tool_call_id we are about to inject.
        hm.append(make_todo_write_assistant("tc_collision"));
        hm.append(make_todo_write_tool("tc_collision"));

        let len_before = hm.messages().len();

        let (assistant, tool) = hm.find_last_todo_write_round().unwrap();
        let (new_tokens, injected) = hm.inject_round_after_marker(assistant, tool);

        assert_eq!(
            hm.messages().len(),
            len_before,
            "must skip on collision (no duplicate tool_call_id)"
        );
        assert_eq!(
            new_tokens,
            hm.token_count(),
            "tokens unchanged on collision skip"
        );
        assert!(
            !injected,
            "collision path must signal injected=false (no JSONL persistence)"
        );
    }

    #[test]
    fn inject_no_op_when_no_marker() {
        // Defensive: caller invokes inject but history has never been
        // compressed. The function must not crash and must not mutate.
        // (In practice this is unreachable — `apply_compression` always
        // inserts a marker when it succeeds — but the public contract
        // requires safe no-op behavior.)
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::User, "Q"));
        hm.append(make_todo_write_assistant("tc_only"));
        hm.append(make_todo_write_tool("tc_only"));

        let len_before = hm.messages().len();
        let (assistant, tool) = hm.find_last_todo_write_round().unwrap();
        let (new_tokens, injected) = hm.inject_round_after_marker(assistant, tool);

        assert_eq!(
            hm.messages().len(),
            len_before,
            "no-op when no compaction marker present"
        );
        assert_eq!(new_tokens, hm.token_count());
        assert!(!injected, "no-marker path must signal injected=false");
    }

    #[test]
    fn inject_updates_token_count() {
        // Verify `recalibrate_tokens()` is invoked and the new state
        // reflects the injected pair. Also exercises the "splice" path
        // (in_tail check fails, token accounting increases).
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::System, "sys"));
        append_marker(&mut hm);
        // Pre-existing tail with its own todo_write round (a different id).
        hm.append(make_todo_write_assistant("tc_orig"));
        hm.append(make_todo_write_tool("tc_orig"));
        hm.append(make_message(MessageRole::Assistant, "Done"));

        let tokens_before = hm.token_count();

        // Inject a round with a FRESH tool_call_id (not in retained tail).
        // Simulates "compression removed a previous todo_write round".
        let new_assistant = make_todo_write_assistant("tc_injected");
        let new_tool = make_todo_write_tool("tc_injected");
        let (returned_tokens, injected) =
            hm.inject_round_after_marker(new_assistant, new_tool);

        assert!(
            returned_tokens > tokens_before,
            "tokens must increase after injecting two messages (before={tokens_before}, after={returned_tokens})"
        );
        assert_eq!(
            returned_tokens,
            hm.token_count(),
            "returned count must match stored token_count after recalibrate"
        );
        assert!(
            injected,
            "fresh-id path must signal injected=true (gate opens for JSONL write)"
        );

        // The injected round must be present right after the marker.
        let msgs = hm.messages();
        let marker_idx = msgs
            .iter()
            .position(|m| m.name.as_deref() == Some(COMPACTION_SUMMARY_NAME))
            .unwrap();
        let ass_id = &msgs[marker_idx + 1].tool_calls.as_ref().unwrap()[0].id;
        assert_eq!(
            ass_id, "tc_injected",
            "injected round is at marker+1, marker+2"
        );
        assert_eq!(
            msgs[marker_idx + 2].tool_call_id.as_deref(),
            Some("tc_injected")
        );
    }

    /// ADR-060 v2 §5.4: consecutive compressions must not double-inject the
    /// same todo_write round. Level 8 strips the previously-spliced round on
    /// the next compression, but the JSONL already holds the synthetic rows;
    /// re-injecting would write duplicates. The manager remembers the most
    /// recent call id and short-circuits when the caller hands it the same id
    /// again.
    #[test]
    fn inject_is_idempotent_across_consecutive_compressions() {
        // Compress once: original todo_write round gets stripped, our splice
        // lands right after the marker.
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::System, "sys"));
        hm.append(make_todo_write_assistant("tc_original"));
        hm.append(make_todo_write_tool("tc_original"));
        append_marker(&mut hm);
        // After level 8 simulated, the original todo_write round is gone:
        // simulate by removing it from the buffer (the history.rs path
        // guarantees `messages[marker_idx + 1..]` is empty in that case).
        {
            let msgs = hm.messages_mut();
            // Find and drop the original todo_write pair.
            let mut remove_idxs: Vec<usize> = Vec::new();
            for (i, m) in msgs.iter().enumerate() {
                let is_orig_tool =
                    m.role == MessageRole::Tool && m.tool_call_id.as_deref() == Some("tc_original");
                let is_orig_assistant = m.role == MessageRole::Assistant
                    && m.tool_calls
                        .as_ref()
                        .is_some_and(|tcs| tcs.iter().any(|tc| tc.id == "tc_original"));
                if is_orig_tool || is_orig_assistant {
                    remove_idxs.push(i);
                }
            }
            for i in remove_idxs.iter().rev() {
                msgs.remove(*i);
            }
        }
        hm.recalibrate_tokens();
        let marker_idx = hm
            .messages()
            .iter()
            .position(|m| m.name.as_deref() == Some(COMPACTION_SUMMARY_NAME))
            .unwrap();
        assert_eq!(
            hm.messages()[marker_idx + 1..].len(),
            0,
            "marker is at the very tail (simulated level 8 strip)"
        );

        // First injection: succeeds.
        let new_assistant = make_todo_write_assistant("tc_injected");
        let new_tool = make_todo_write_tool("tc_injected");
        let (_tokens_1, injected_1) =
            hm.inject_round_after_marker(new_assistant, new_tool);
        assert!(injected_1, "first injection must succeed");
        let len_after_first = hm.messages().len();

        // Second consecutive compression: level 8 strips our splice again
        // (simulate by clearing the tail past the marker).
        {
            let msgs = hm.messages_mut();
            msgs.truncate(marker_idx + 1);
        }
        hm.recalibrate_tokens();
        assert_eq!(hm.messages()[marker_idx + 1..].len(), 0);

        // Re-attempt the injection with the SAME round (the round the caller
        // has snapshotted). Must be rejected by the idempotency gate —
        // otherwise we'd write duplicate synthetic rows to conversation.jsonl.
        let again_assistant = make_todo_write_assistant("tc_injected");
        let again_tool = make_todo_write_tool("tc_injected");
        let (_tokens_2, injected_2) =
            hm.inject_round_after_marker(again_assistant, again_tool);
        assert!(
            !injected_2,
            "second injection of the same round must be skipped (injected_round_call_ids gate)"
        );
        assert_eq!(
            hm.messages().len(),
            marker_idx + 1,
            "no messages appended on idempotency skip"
        );

        // A genuinely NEW round (different tool_call_id, e.g. user called
        // todo_write between the two compressions) must still inject — the
        // gate is per-call-id, not a global kill switch.
        let fresh_assistant = make_todo_write_assistant("tc_fresh");
        let fresh_tool = make_todo_write_tool("tc_fresh");
        let (_tokens_3, injected_3) =
            hm.inject_round_after_marker(fresh_assistant, fresh_tool);
        assert!(
            injected_3,
            "a NEW todo_write call (different id) must still inject — gate is per-id"
        );
        assert_eq!(
            hm.messages().len(),
            marker_idx + 3,
            "two new messages inserted (Assistant + Tool) for the fresh round"
        );
        assert_eq!(
            len_after_first,
            marker_idx + 3,
            "sanity: first splice length matches"
        );
    }

    // ── ADR-067: messages_json_bytes incremental counter ──────────────

    fn assert_json_bytes_matches(hm: &HistoryManager) {
        let expected = serde_json::to_string(&*hm.messages_arc())
            .map(|s| s.len())
            .unwrap_or(0);
        assert_eq!(
            hm.messages_json_bytes(),
            expected,
            "incremental counter must equal full serialization length"
        );
    }

    #[test]
    fn messages_json_bytes_tracks_append_and_extend() {
        let mut hm = HistoryManager::new(10_000);
        assert_eq!(
            hm.messages_json_bytes(),
            2,
            "empty history serializes as `[]`"
        );

        hm.append(make_message(MessageRole::User, "hello"));
        assert_json_bytes_matches(&hm);

        hm.append(make_message(MessageRole::Assistant, "hi there"));
        assert_json_bytes_matches(&hm);

        hm.extend(vec![
            make_message(MessageRole::User, "third"),
            make_message(MessageRole::Assistant, "fourth"),
        ]);
        assert_json_bytes_matches(&hm);
    }

    #[test]
    fn messages_json_bytes_tracks_clear_and_truncate() {
        let mut hm = HistoryManager::new(10_000);
        hm.append(make_message(MessageRole::User, "a"));
        hm.append(make_message(MessageRole::User, "b"));
        hm.append(make_message(MessageRole::User, "c"));
        assert_json_bytes_matches(&hm);

        hm.truncate_to(2);
        assert_eq!(hm.len(), 2);
        assert_json_bytes_matches(&hm);

        hm.clear();
        assert_eq!(
            hm.messages_json_bytes(),
            2,
            "cleared history serializes as `[]`"
        );
    }

    // ── skills_loaded_bytes: same accounting contract, skill bodies ───

    /// The counter must equal a fresh O(n) scan of the buffer, after every
    /// mutation. Mirrors [`assert_json_bytes_matches`]: the incremental
    /// hot-path accounting is only trustworthy if it is pinned against the
    /// recomputed ground truth at each step.
    fn assert_skills_bytes_matches(hm: &HistoryManager) {
        let expected: usize = hm
            .messages()
            .iter()
            .filter(|m| {
                matches!(m.role, MessageRole::Tool)
                    && m.name.as_deref()
                        == Some(crate::tools::builtin::load_skill::TOOL_NAME)
            })
            .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
            .sum();
        assert_eq!(
            hm.skills_loaded_bytes(),
            expected,
            "incremental skills_loaded_bytes must equal a full scan"
        );
    }

    /// A `load_skill` Tool result carrying a plausible SKILL.md body.
    fn make_skill_load_msg(tool_call_id: &str, skill: &str) -> ChatMessage {
        let body = format!(
            "{{\"name\":\"{skill}\",\"description\":\"does a thing\",\
             \"instructions\":\"## steps\\n1. do\\n2. thing\\n\"}}"
        );
        let mut m = ChatMessage::tool(tool_call_id, body);
        m.name = Some(crate::tools::builtin::load_skill::TOOL_NAME.to_string());
        m
    }

    fn make_skill_load_call(tool_call_id: &str, skill: &str) -> ChatMessage {
        ChatMessage::assistant_with_tools(
            "loading the skill",
            vec![make_tool_call(
                tool_call_id,
                crate::tools::builtin::load_skill::TOOL_NAME,
                &format!(r#"{{"name":"{skill}"}}"#),
            )],
        )
    }

    #[test]
    fn skills_loaded_counts_only_load_skill_tool_results() {
        let mut hm = HistoryManager::new(10_000);
        assert_eq!(hm.skills_loaded_bytes(), 0, "empty history has no skills");

        hm.append(make_message(MessageRole::User, "review this PR"));
        hm.append(make_skill_load_call("tc_sk1", "code-review"));
        hm.append(make_skill_load_msg("tc_sk1", "code-review"));
        let after_load = hm.skills_loaded_bytes();
        assert!(after_load > 0, "the load_skill result must be counted");
        assert_skills_bytes_matches(&hm);

        // A large non-skill tool result must not leak into the skills bucket.
        hm.append(ChatMessage::assistant_with_tools(
            "reading",
            vec![make_tool_call("tc_r", "file_read", "{}")],
        ));
        let mut read_result = ChatMessage::tool("tc_r", "x".repeat(4_000));
        read_result.name = Some("file_read".to_string());
        hm.append(read_result);
        assert_eq!(
            hm.skills_loaded_bytes(),
            after_load,
            "other tools' results must not count as skills"
        );
        assert_skills_bytes_matches(&hm);
    }

    #[test]
    fn skills_loaded_double_counts_a_repeated_load() {
        // Deliberate policy, not an oversight: loading the same skill twice
        // puts two copies of the body in the prompt, so it costs twice. The
        // metric answers "how many skill bytes are in context", not
        // "how many distinct skills were used".
        let mut hm = HistoryManager::new(10_000);
        hm.append(make_skill_load_call("tc_a", "code-review"));
        hm.append(make_skill_load_msg("tc_a", "code-review"));
        let once = hm.skills_loaded_bytes();

        hm.append(make_skill_load_call("tc_b", "code-review"));
        hm.append(make_skill_load_msg("tc_b", "code-review"));
        assert_eq!(
            hm.skills_loaded_bytes(),
            once * 2,
            "a second copy of the same skill is a second copy of tokens"
        );
        assert_skills_bytes_matches(&hm);
    }

    #[test]
    fn skills_loaded_shrinks_when_the_round_is_compacted_away() {
        // The self-correction property the whole design leans on: usage is
        // reconciled at every structural mutation, so a skill dropped by
        // compaction stops being reported without any extra bookkeeping.
        let mut hm = HistoryManager::new(100_000);
        for i in 1..=6 {
            hm.append(make_message(
                MessageRole::User,
                &format!("question {i} with enough words to carry weight"),
            ));
            hm.append(make_skill_load_call(&format!("tc_{i}"), "debugging"));
            hm.append(make_skill_load_msg(&format!("tc_{i}"), "debugging"));
            hm.append(make_message(MessageRole::Assistant, &format!("answer {i}")));
        }
        let before = hm.skills_loaded_bytes();
        assert!(before > 0);
        assert_skills_bytes_matches(&hm);

        let removed = hm.replace_middle_with_summary("Earlier rounds summarized.", 1);
        assert!(removed > 0, "test premise: something must be compacted");
        assert_skills_bytes_matches(&hm);
        assert!(
            hm.skills_loaded_bytes() < before,
            "compacted-away skill bodies must stop being reported ({} -> {})",
            before,
            hm.skills_loaded_bytes()
        );
    }

    #[test]
    fn skills_loaded_recomputed_on_restore_clear_and_truncate() {
        let mut hm = HistoryManager::new(10_000);
        let restored = vec![
            make_message(MessageRole::User, "hi"),
            make_skill_load_call("tc_r", "testing"),
            make_skill_load_msg("tc_r", "testing"),
        ];
        let expected = restored[2..]
            .iter()
            .map(|m| serde_json::to_string(m).unwrap().len())
            .sum::<usize>();

        // load_restored bypasses append/extend — the recompute site must catch it.
        hm.load_restored(restored, std::collections::HashSet::new());
        assert_eq!(hm.skills_loaded_bytes(), expected);
        assert_skills_bytes_matches(&hm);

        hm.truncate_to(2);
        assert_eq!(
            hm.skills_loaded_bytes(),
            0,
            "truncating away the tool result must drop the count"
        );
        assert_skills_bytes_matches(&hm);

        hm.append(make_skill_load_msg("tc_x", "testing"));
        assert!(hm.skills_loaded_bytes() > 0);
        hm.clear();
        assert_eq!(hm.skills_loaded_bytes(), 0, "clear resets both counters");
    }

    #[test]
    fn folding_a_skill_result_to_a_placeholder_releases_its_bytes() {
        // The `clear_round` / `abandon_tool_result` path replaces Tool
        // content with a short placeholder. The counters must follow, or the
        // skills bucket would report bodies that are no longer there.
        let mut hm = HistoryManager::new(10_000);
        hm.append(make_skill_load_call("tc_a", "code-review"));
        hm.append(make_skill_load_msg("tc_a", "code-review"));
        let before = hm.skills_loaded_bytes();
        assert!(before > 0);

        let idx = hm
            .messages()
            .iter()
            .position(|m| matches!(m.role, MessageRole::Assistant))
            .expect("assistant round present");
        hm.clear_round(idx);

        let after = hm.skills_loaded_bytes();
        assert!(
            after < before,
            "folded skill body must shrink the skills bucket ({before} -> {after})"
        );
        assert_skills_bytes_matches(&hm);
        assert_json_bytes_matches(&hm);
    }

    // ─── Module B: skill round recall ────────────────────────────────

    #[test]
    fn find_latest_skill_rounds_dedups_by_skill_newest_first() {
        let mut hm = HistoryManager::new(100_000);
        // architecture-design loaded twice (early + late) — only the latest
        // occurrence may be recalled; debugging loaded once.
        hm.append(make_skill_load_call("c_arch_1", "architecture-design"));
        hm.append(make_skill_load_msg("c_arch_1", "architecture-design"));
        hm.append(make_skill_load_call("c_dbg", "debugging"));
        hm.append(make_skill_load_msg("c_dbg", "debugging"));
        let mut late = make_skill_load_call("c_arch_2", "architecture-design");
        late.content = "LATE architecture body".to_string();
        hm.append(late);
        let mut late_res = make_skill_load_msg("c_arch_2", "architecture-design");
        late_res.content = "LATE architecture body".to_string();
        hm.append(late_res);

        let rounds = hm.find_latest_skill_rounds();
        let ids: Vec<String> = rounds
            .iter()
            .map(|(a, _)| a.tool_calls.as_ref().unwrap()[0].id.clone())
            .collect();
        assert_eq!(
            ids,
            vec!["c_arch_2".to_string(), "c_dbg".to_string()],
            "one round per distinct skill, newest load first; the superseded \
             early architecture load must not appear"
        );

        let (arch_assistant, arch_tool) = &rounds[0];
        assert_eq!(
            arch_assistant.tool_calls.as_ref().unwrap().len(),
            1,
            "the synthesized assistant must carry ONLY this round's call, so the \
             round is self-contained and its tool_call_ids stay unique"
        );
        assert!(
            !arch_tool.content.contains("does a thing"),
            "the superseded early body must not be recalled"
        );
        assert!(arch_tool.content.contains("LATE"));
    }

    #[test]
    fn find_latest_skill_rounds_splits_parallel_calls_into_separate_rounds() {
        let mut hm = HistoryManager::new(100_000);
        // One assistant turn loading two skills in parallel.
        hm.append(ChatMessage::assistant_with_tools(
            "loading two skills",
            vec![
                make_tool_call(
                    "c_x",
                    crate::tools::builtin::load_skill::TOOL_NAME,
                    r#"{"name":"skill-x"}"#,
                ),
                make_tool_call(
                    "c_y",
                    crate::tools::builtin::load_skill::TOOL_NAME,
                    r#"{"name":"skill-y"}"#,
                ),
            ],
        ));
        hm.append(make_skill_load_msg("c_x", "skill-x"));
        hm.append(make_skill_load_msg("c_y", "skill-y"));

        let rounds = hm.find_latest_skill_rounds();
        assert_eq!(
            rounds.len(),
            2,
            "parallel loads are two distinct skills and must become two rounds, \
             each with its own assistant turn"
        );
        for (assistant, _) in &rounds {
            assert_eq!(
                assistant.tool_calls.as_ref().unwrap().len(),
                1,
                "splitting must not duplicate the sibling call into both rounds"
            );
        }
    }

    #[test]
    fn skill_recall_budget_zero_selects_nothing() {
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_skill_load_call("c", "a"));
        hm.append(make_skill_load_msg("c", "a"));
        let candidates = hm.find_latest_skill_rounds();
        assert_eq!(candidates.len(), 1, "sanity: one candidate exists");
        assert!(
            hm.apply_skill_recall_budget(&candidates, 0).is_empty(),
            "budget 0 is the caller's explicit 'no room' signal"
        );
    }

    #[test]
    fn skill_recall_budget_floor_keeps_most_recent_even_when_over_budget() {
        let mut hm = HistoryManager::new(100_000);
        // newest = huge, older = tiny. A byte/token budget that only fits the
        // tiny one must STILL take the huge one: the floor is the most recent
        // load, not a cheaper older one.
        hm.append(make_skill_load_call("c_old", "older"));
        hm.append(make_skill_load_msg("c_old", "older"));
        hm.append(make_skill_load_call("c_new", "newest"));
        let mut big = make_skill_load_msg("c_new", "newest");
        big.content = "B".repeat(20_000);
        hm.append(big);

        let candidates = hm.find_latest_skill_rounds();
        assert_eq!(candidates.len(), 2);
        let older_cost = hm.round_token_cost(&candidates[1].0, &candidates[1].1);
        let newest_cost = hm.round_token_cost(&candidates[0].0, &candidates[0].1);
        assert!(
            newest_cost > older_cost,
            "test setup: newest round must be the expensive one"
        );

        let chosen = hm.apply_skill_recall_budget(&candidates, older_cost);
        assert_eq!(
            chosen.len(),
            1,
            "floor: exactly one round even though it alone exceeds the budget"
        );
        assert_eq!(
            chosen[0].0.tool_calls.as_ref().unwrap()[0].id,
            "c_new",
            "the floor must be the MOST RECENT load"
        );
    }

    #[test]
    fn skill_recall_budget_stops_at_first_round_that_does_not_fit() {
        let mut hm = HistoryManager::new(100_000);
        for i in 0..4 {
            hm.append(make_skill_load_call(&format!("c{i}"), &format!("s{i}")));
            hm.append(make_skill_load_msg(&format!("c{i}"), &format!("s{i}")));
        }
        let candidates = hm.find_latest_skill_rounds();
        assert_eq!(candidates.len(), 4);
        let one = hm.round_token_cost(&candidates[0].0, &candidates[0].1);
        let chosen = hm.apply_skill_recall_budget(&candidates, one * 2 + one / 2);
        assert_eq!(
            chosen.len(),
            2,
            "a budget of two-and-a-half rounds must buy two, never a partial round"
        );
    }

    #[test]
    fn skill_recall_reaccumulates_the_usage_counter_after_compaction() {
        // The scenario the user asked for: a load_skill body discarded by
        // compaction is spliced back after the marker, and the usage counter
        // tracks it both ways.
        let mut hm = HistoryManager::new(100_000);
        hm.append(make_message(MessageRole::User, "review my code please"));
        hm.append(make_skill_load_call("c_sk", "code-review"));
        let mut body = make_skill_load_msg("c_sk", "code-review");
        body.content = "FULL SKILL BODY ".repeat(50);
        hm.append(body);
        assert_skills_bytes_matches(&hm);
        let before = hm.skills_loaded_bytes();
        assert!(before > 0, "the loaded skill is counted in context");

        // Snapshot candidates BEFORE the structural change (that is exactly the
        // order the live compaction path must use).
        let candidates = hm.find_latest_skill_rounds();
        assert_eq!(candidates.len(), 1);

        // Compaction drops the middle: the skill round leaves the context.
        hm.truncate_to(1);
        assert_eq!(
            hm.skills_loaded_bytes(),
            0,
            "the loaded skill left the context with the discarded round"
        );
        assert_skills_bytes_matches(&hm);

        // ...and recall re-injects it right after the marker.
        append_marker(&mut hm);
        let (assistant, tool) = candidates[0].clone();
        let (_, injected) = hm.inject_round_after_marker(assistant, tool);
        assert!(injected, "the round is absent from the tail, so the splice must happen");
        assert_eq!(
            hm.skills_loaded_bytes(),
            before,
            "the body is back in context and must be counted again"
        );
        assert_skills_bytes_matches(&hm);
        assert_json_bytes_matches(&hm);
    }
}
