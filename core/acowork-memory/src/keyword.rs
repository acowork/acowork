//! Deterministic write-time keyword quality gate (ADR-062 §6.2.1, M5 step 2a).
//!
//! Keywords flowing into memory come from the LLM via the `memory_store`
//! tool parameters — unfiltered LLM output would otherwise be folded into
//! the BM25 index verbatim (Plan Y, ADR-062 §6.2), carrying the same
//! LLM-anchoring / garbage-in risk the ADR-062 §1 flags for
//! `confidence` / `importance`.
//!
//! This module is a **pure, always-on** sanitizer (independent of the
//! `quality.keyword_index` toggle): it keeps `metadata["keywords"]` itself
//! clean, so the write-time fold into the BM25 `object` field (step 2b)
//! never amplifies garbage.
//!
//! Rules (in application order, ADR-062 §6.2.1):
//!   1. trim + length filter: `0 < chars ≤ 30` (anti whole-sentence pollution)
//!   2. lowercase (matches BM25 tokenizer casing)
//!   3. must contain at least one alphabetic char (ASCII alpha or CJK;
//!      filters pure digits / pure punctuation)
//!   4. de-duplicate (case-insensitive, after lowercase)
//!   5. stopword filter (~30 common no-op tokens)
//!   6. cap: at most 8 keywords per node (keep the first 8)
//!
//! Call sites (idempotent, defensive at both boundaries):
//!   - [`crate::manager`]'s tool-facing path (`memory_store` in runtime) —
//!     the LLM boundary
//!   - the storage backend (`acowork-sqlite`) — defensive fallback at the
//!     persistence boundary

/// Maximum number of characters a single keyword may have (rule 1).
///
/// Aliased from [`acowork_core::retrieval::MAX_TERM_CHARS`] rather than
/// restated: the query-time analyzer (ADR-095 D2) enforces the same limit on
/// the other side of the index, and two numbers that must agree but are
/// written twice eventually stop agreeing.
pub const MAX_KEYWORD_CHARS: usize = acowork_core::retrieval::MAX_TERM_CHARS;

/// Maximum number of keywords kept per node (rule 6).
pub const MAX_KEYWORDS_PER_NODE: usize = 8;


/// Per-rule drop counters for observability (`memory_write_keyword_gate`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SanitizeStats {
    /// Keywords before sanitization.
    pub input_count: usize,
    /// Keywords after sanitization.
    pub output_count: usize,
    /// Dropped by rule 1 (empty after trim / longer than 30 chars).
    pub dropped_empty_or_too_long: usize,
    /// Dropped by rule 3 (no alphabetic / CJK character).
    pub dropped_no_alpha_cjk: usize,
    /// Dropped by rule 4 (case-insensitive duplicate).
    pub dropped_duplicate: usize,
    /// Dropped by rule 5 (stopword).
    pub dropped_stopword: usize,
    /// Dropped by rule 6 (over the 8-keyword cap).
    pub dropped_over_cap: usize,
}

impl SanitizeStats {
    /// Total number of dropped keywords across all rules.
    pub fn total_dropped(&self) -> usize {
        self.dropped_empty_or_too_long
            + self.dropped_no_alpha_cjk
            + self.dropped_duplicate
            + self.dropped_stopword
            + self.dropped_over_cap
    }
}

/// Sanitize an LLM-provided keyword list (see module docs for the rules).
///
/// Pure function — no I/O, no global state. Idempotent: sanitizing an
/// already-clean list is a no-op.
pub fn sanitize(input: Vec<String>) -> Vec<String> {
    sanitize_with_stats(input).0
}

/// Sanitize and return per-rule drop statistics for structured observability.
pub fn sanitize_with_stats(input: Vec<String>) -> (Vec<String>, SanitizeStats) {
    let mut stats = SanitizeStats {
        input_count: input.len(),
        ..SanitizeStats::default()
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(input.len());

    for raw in input {
        // Rule 1: trim + length filter (chars, CJK-friendly).
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() || trimmed.chars().count() > MAX_KEYWORD_CHARS {
            stats.dropped_empty_or_too_long += 1;
            continue;
        }

        // Rule 2: lowercase (also makes rules 3-5 case-insensitive).
        let lowered = trimmed.to_lowercase();

        // Rule 3: must contain at least one alphabetic char (ASCII alpha or
        // CJK; `char::is_alphabetic` covers both plus other scripts — the
        // intent is to filter pure digits / pure punctuation).
        if !lowered.chars().any(|c| c.is_alphabetic()) {
            stats.dropped_no_alpha_cjk += 1;
            continue;
        }

        // Rule 4: de-duplicate (case-insensitive by virtue of rule 2).
        if !seen.insert(lowered.clone()) {
            stats.dropped_duplicate += 1;
            continue;
        }

        // Rule 5: stopword filter (shared table: acowork_core::retrieval::STOPWORDS).
        if acowork_core::retrieval::is_stopword(&lowered) {
            stats.dropped_stopword += 1;
            continue;
        }

        out.push(lowered);
    }

    // Rule 6: cap at 8 keywords per node (keep the first 8).
    if out.len() > MAX_KEYWORDS_PER_NODE {
        stats.dropped_over_cap = out.len() - MAX_KEYWORDS_PER_NODE;
        out.truncate(MAX_KEYWORDS_PER_NODE);
    }

    stats.output_count = out.len();
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(input: &[&str]) -> Vec<String> {
        sanitize(input.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn keeps_clean_lowercase_tokens() {
        assert_eq!(kept(&["shanghai", "location", "home"]), vec![
            "shanghai".to_string(),
            "location".to_string(),
            "home".to_string(),
        ]);
    }

    #[test]
    fn keeps_cjk_and_mixed_tokens() {
        assert_eq!(kept(&["上海", "beijing-2024"]), vec![
            "上海".to_string(),
            "beijing-2024".to_string(),
        ]);
    }

    #[test]
    fn drops_empty_and_whitespace_only() {
        // NB: "ok" is itself a stopword — use a neutral token here so the
        // empty/whitespace rule is what's exercised.
        let (out, stats) = sanitize_with_stats(vec!["".into(), "   ".into(), "beijing".into()]);
        assert_eq!(out, vec!["beijing".to_string()]);
        assert_eq!(stats.dropped_empty_or_too_long, 2);
    }

    #[test]
    fn drops_overlong_keywords() {
        let long = "a".repeat(31);
        let (out, stats) = sanitize_with_stats(vec![long, "short".into()]);
        assert_eq!(out, vec!["short".to_string()]);
        assert_eq!(stats.dropped_empty_or_too_long, 1);
    }

    #[test]
    fn boundary_len_30_is_kept() {
        let ok = "k".repeat(30);
        assert_eq!(kept(&[&ok]), vec![ok]);
    }

    #[test]
    fn lowercases_mixed_case() {
        assert_eq!(kept(&["Shanghai", "LOCATION"]), vec![
            "shanghai".to_string(),
            "location".to_string(),
        ]);
    }

    #[test]
    fn drops_pure_digits_and_punctuation() {
        let (out, stats) = sanitize_with_stats(vec![
            "12345".into(),
            "!!!".into(),
            "……".into(),
            "v2".into(),
        ]);
        assert_eq!(out, vec!["v2".to_string()]);
        assert_eq!(stats.dropped_no_alpha_cjk, 3);
    }

    #[test]
    fn dedups_case_insensitively() {
        let (out, stats) = sanitize_with_stats(vec![
            "Shanghai".into(),
            "shanghai".into(),
            "SHANGHAI".into(),
            "beijing".into(),
        ]);
        assert_eq!(out, vec!["shanghai".to_string(), "beijing".to_string()]);
        assert_eq!(stats.dropped_duplicate, 2);
    }

    #[test]
    fn drops_stopwords() {
        let (out, stats) = sanitize_with_stats(vec![
            "user".into(),
            "memory".into(),
            "the".into(),
            "shanghai".into(),
        ]);
        assert_eq!(out, vec!["shanghai".to_string()]);
        assert_eq!(stats.dropped_stopword, 3);
    }

    #[test]
    fn caps_at_eight_keywords() {
        let input: Vec<String> = (0..10).map(|i| format!("kw{i}")).collect();
        let (out, stats) = sanitize_with_stats(input);
        assert_eq!(out.len(), MAX_KEYWORDS_PER_NODE);
        assert_eq!(stats.dropped_over_cap, 2);
        assert_eq!(out, (0..8).map(|i| format!("kw{i}")).collect::<Vec<_>>());
    }

    #[test]
    fn cap_counts_only_after_other_rules() {
        // 10 inputs, 2 dropped earlier (stopword + duplicate) → 8 kept, no
        // over-cap drop.
        let (out, stats) = sanitize_with_stats(vec![
            "user".into(),          // stopword
            "kw1".into(),
            "kw1".into(),           // duplicate
            "kw2".into(),
            "kw3".into(),
            "kw4".into(),
            "kw5".into(),
            "kw6".into(),
            "kw7".into(),
            "kw8".into(),
        ]);
        assert_eq!(out.len(), 8);
        assert_eq!(stats.dropped_over_cap, 0);
        assert_eq!(stats.dropped_stopword, 1);
        assert_eq!(stats.dropped_duplicate, 1);
    }

    #[test]
    fn stats_totals_are_consistent() {
        let (out, stats) = sanitize_with_stats(vec![
            "".into(),
            "a_really_long_keyword_that_exceeds_thirty_characters!!".into(),
            "123".into(),
            "shanghai".into(),
            "shanghai".into(),
            "the".into(),
            "beijing".into(),
        ]);
        assert_eq!(stats.input_count, 7);
        assert_eq!(stats.output_count, out.len());
        assert_eq!(
            stats.total_dropped(),
            7 - out.len(),
            "every dropped keyword is attributed to exactly one rule"
        );
    }

    #[test]
    fn sanitize_is_idempotent() {
        let once = kept(&["Shanghai", "user", "LOCATION"]);
        let twice = sanitize(once.clone());
        assert_eq!(once, twice);
    }

    #[test]
    fn empty_input_is_noop() {
        let (out, stats) = sanitize_with_stats(vec![]);
        assert!(out.is_empty());
        assert_eq!(stats.input_count, 0);
        assert_eq!(stats.total_dropped(), 0);
    }
}
