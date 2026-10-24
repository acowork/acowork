//! Shared retrieval-fusion contract (ADR-095 D3/D4).
//!
//! This module is the **only** place that defines the vocabulary of hybrid
//! retrieval. Three call paths consume it — `acowork-memory` (the
//! `memory_recall` tool), the global-search memory path
//! (`acowork-sqlite::admin`), and the global-search conversation path
//! (`acowork-sqlite::conversation`) — and each previously carried its own
//! idea of what a "score" meant. Putting the types here keeps those modules
//! siblings instead of letting `search` depend on `memory` or `memory`
//! depend on `sqlite` (both of which would form a dependency cycle through
//! `MemoryProvider`, whose trait lives in `acowork-memory` and whose
//! implementation lives in `acowork-sqlite`).
//!
//! ## Score domains — the reason this module exists
//!
//! A hybrid hit carries three *incommensurable* numbers. They must never be
//! conflated, which is exactly what the pre-ADR-095 code did by collapsing
//! them into one `score` field:
//!
//! | Signal | Domain | Comparable across | Meaning |
//! |---|---|---|---|
//! | `cosine` | `[-1.0, 1.0]` | queries, corpora | geometric similarity of two embeddings |
//! | BM25 / FTS | unbounded, corpus-statistics dependent | ranks only, never values | lexical relevance, drifts with corpus size |
//! | `fused_score` | `(0, Σw/(RRF_K+1)]` | **nothing** | rank position after fusion |
//!
//! `fused_score` is a rank signal, not a relevance measurement: its absolute
//! value says nothing about whether a hit answers the query, so it must not
//! be thresholded, displayed as a confidence, or compared across sources.
//! ADR-095 D4 forbids reusing it as a similarity — the global-search memory
//! path used to synthesise `1.0 - i*1e-4` and hand it to the UI as if it
//! were a score.

use serde::{Deserialize, Serialize};

/// The two views of one search query that hybrid retrieval needs (ADR-095 D2).
///
/// A hybrid query is not one string: the vector source wants the natural
/// language sentence, the lexical source wants bounded terms. Carrying both in
/// one type keeps the pairing explicit at every call site and — more
/// importantly — keeps the **fallback rule** in exactly one place: when the
/// analyzer produced nothing usable, the raw query drives the lexical source.
///
/// That rule is a correctness requirement, not a convenience. An analyzer miss
/// (all-English query under a CJK-only analyzer, empty result, over-long input)
/// must never cost recall; without a single owning place for it, every call
/// site re-implements "if terms are empty do X" and they drift.
///
/// The raw text is also what gets embedded, so it is the *semantic* side of the
/// pair too.
#[derive(Debug, Clone, Copy)]
pub struct LexicalQuery<'a> {
    /// Query text as the user/agent wrote it. Embedded for the vector source,
    /// and used for the lexical source when [`Self::terms`] is empty.
    pub raw: &'a str,
    /// Analyzer output: bounded, stop-word-filtered, **bare** terms (no FTS
    /// quoting or operators — escaping belongs to the executor, see
    /// `SqliteStore::text_search_terms`).
    pub terms: &'a [String],
}

impl<'a> LexicalQuery<'a> {
    /// A query with no analyzed terms: the raw text drives both sources.
    /// Used by the legacy single-string entry points.
    pub fn raw_only(raw: &'a str) -> Self {
        Self { raw, terms: &[] }
    }

    /// Resolve the pair into what the lexical source must actually run.
    ///
    /// This is the single owner of the fallback rule (ADR-095 D2): analyzed
    /// terms when there are any, otherwise the raw text, otherwise nothing.
    /// Call sites must not re-implement it.
    pub fn lexical_plan(&self) -> LexicalPlan<'a> {
        if !self.terms.is_empty() {
            return LexicalPlan::Terms(self.terms);
        }
        if self.raw.trim().is_empty() {
            // Neither side has content: skipping the lexical source is not a
            // recall loss, there is nothing to recall.
            return LexicalPlan::Skip;
        }
        LexicalPlan::Raw(self.raw)
    }
}

/// What the lexical source runs, as resolved by [`LexicalQuery::lexical_plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexicalPlan<'a> {
    /// Run these pre-analyzed terms verbatim.
    Terms(&'a [String]),
    /// No analyzed terms — run the raw text through the engine's own
    /// whitespace splitting (the legacy behaviour, kept as the fallback).
    Raw(&'a str),
    /// No usable lexical query at all — do not run the lexical source.
    Skip,
}

/// Which ranked source(s) produced a candidate (ADR-095 D4).
///
/// **Not** a relevance claim. `Both` means the id appeared in both bounded
/// candidate pools; it does not mean "twice as relevant".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// Only the vector (cosine) source ranked this id.
    Vector,
    /// Only the lexical (BM25/FTS) source ranked this id.
    Text,
    /// Both sources ranked this id within their candidate pools.
    Both,
}

impl MatchedBy {
    /// Wire label (`"vector"` / `"text"` / `"both"`), matching the
    /// `snake_case` serde representation so HTTP payloads and Rust callers
    /// cannot drift into two spellings of the same enum.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Text => "text",
            Self::Both => "both",
        }
    }

    /// Parse the wire label. Used to validate `matched_by` arriving from an
    /// untrusted client (the summary endpoint re-receives what the Desktop
    /// read out of `/search`) rather than silently accepting any string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "vector" => Some(Self::Vector),
            "text" => Some(Self::Text),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

/// One fused candidate with its per-source, in-source signals (ADR-095 D4).
///
/// Every field is a *within-source* signal. Ranks are 0-based positions
/// inside that source's own bounded candidate pool, so `vector_rank: Some(0)`
/// means "the closest embedding among the `pool_factor * k` vectors the
/// engine looked at" — not "the closest in the store".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FusedHit {
    /// Node id (memory node, or conversation message node).
    pub id: u64,
    /// Which source(s) surfaced this candidate.
    pub matched_by: MatchedBy,
    /// 0-based rank inside the vector pool. `None` = not a vector hit.
    pub vector_rank: Option<usize>,
    /// 0-based rank inside the lexical pool. `None` = not a lexical hit.
    pub text_rank: Option<usize>,
    /// Cosine similarity, retained **only** when the vector source actually
    /// ran. `None` for lexical-only hits and for text-fallback queries.
    ///
    /// This is the one field here that is a genuine similarity. It is kept
    /// for diagnostics and evaluation; ADR-095 §9.4 deliberately keeps it
    /// off the public UI payload until a threshold is calibrated.
    pub cosine: Option<f64>,
    /// Weighted RRF fused score. In-source rank signal only — never
    /// comparable across queries or scopes, and never a similarity.
    pub fused_score: f64,
    /// 0-based rank after fusion + dedup + truncation. This is the order the
    /// caller must present: recomputing a sort from `fused_score` elsewhere
    /// would re-derive the same order while implying that score is portable.
    pub fused_rank: usize,
}

/// Per-source retrieval signals for one hit, as the API payloads carry them.
///
/// This is the **shared** projection of [`FusedHit`]: memory hits (produced by
/// the memory service) and conversation hits (produced by the Runtime's
/// conversation index) both report the same shape, so it lives in `acowork-core`
/// rather than in either producer.
///
/// Ranks are **in-source** (position inside each candidate pool), not
/// comparable across sources; `fused_score` is the in-source RRF signal and
/// must not be presented as a similarity.
#[derive(Debug, Clone)]
pub struct RetrievalEvidence {
    pub matched_by: MatchedBy,
    pub vector_rank: Option<usize>,
    pub text_rank: Option<usize>,
    pub cosine: Option<f64>,
    pub fused_score: f64,
    /// `None` when the answering source has no fusion to rank (the legacy
    /// single-path search).
    pub fused_rank: Option<usize>,
}

impl RetrievalEvidence {
    /// The **whitelisted** client-facing projection of these signals.
    ///
    /// Lives on the type rather than at each call site on purpose: `cosine` and
    /// `fused_score` must not reach the list payload (design 28 §4.2 — a cosine
    /// in a UI payload gets rendered as a confidence percentage, and this one
    /// is not calibrated). Putting the projection here means adding a field to
    /// the struct does not silently publish it to Desktop.
    ///
    /// Absent signals are omitted, not emitted as `null`: a consumer reading
    /// `matched_by` then sees "unknown" instead of inventing a category.
    pub fn to_payload(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        self.write_into(&mut map);
        serde_json::Value::Object(map)
    }

    /// Merge the evidence keys into an existing scope payload.
    ///
    /// Evidence is **additive**: a hit is only actionable if the UI keeps its
    /// locate keys (`node_id`, `session_id`, ...). Building the evidence object
    /// and assigning it as the whole payload silently drops those, which is why
    /// this helper exists and why every scope uses it.
    pub fn merge_into(&self, payload: &mut serde_json::Map<String, serde_json::Value>) {
        self.write_into(payload);
    }

    fn write_into(&self, map: &mut serde_json::Map<String, serde_json::Value>) {
        map.insert(
            "matched_by".to_string(),
            serde_json::json!(self.matched_by),
        );
        if let Some(r) = self.vector_rank {
            map.insert("vector_rank".to_string(), serde_json::json!(r));
        }
        if let Some(r) = self.text_rank {
            map.insert("text_rank".to_string(), serde_json::json!(r));
        }
        if let Some(r) = self.fused_rank {
            map.insert("fused_rank".to_string(), serde_json::json!(r));
        }
    }
}

/// Per-source fusion knobs (ADR-095 D3).
///
/// The weights are rank-fusion weights, **not** probabilities or
/// similarities: `(0.8, 0.2)` means "a vector rank contributes 4× a lexical
/// rank to the fused sum", nothing more. Values are calibrated against the
/// Phase-0 evaluation set (design 28 §3.3); the constants below are the
/// ADR-specified starting points, not settled numbers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FusionPolicy {
    /// Weight of the vector source's RRF contribution.
    pub vector_weight: f64,
    /// Weight of the lexical source's RRF contribution.
    pub text_weight: f64,
    /// Cosine gate applied to the **vector source only**. `None` disables it.
    ///
    /// A lexical hit never passes through this gate (ADR-095 D3: lexical
    /// evidence is independent and must not be dropped because its embedding
    /// sits far away).
    pub min_cosine: Option<f32>,
    /// Per-source candidate pool = `pool_factor * k`, fused before truncating
    /// to `k`. Bounded so a threshold sweep cannot make the pool explode.
    pub pool_factor: usize,
}

impl FusionPolicy {
    /// Initial semantic-memory policy: vector:text = 4:1, mirroring the
    /// production `memory_recall` Semantic-hint weights (ADR-095 D3).
    ///
    /// `min_cosine: None` is deliberate — no cosine floor is applied to the
    /// memory path until the evaluation set proves a threshold helps
    /// (ADR-095 §2.2 / D6).
    pub const SEMANTIC_4TO1: Self = Self {
        vector_weight: 0.8,
        text_weight: 0.2,
        min_cosine: None,
        pool_factor: 2,
    };

    /// Conversation baseline: today's production behaviour — equal weights
    /// with the existing 0.3 cosine floor on the vector source.
    pub const CONVERSATION_BASELINE: Self = Self {
        vector_weight: 1.0,
        text_weight: 1.0,
        min_cosine: Some(0.3),
        pool_factor: 2,
    };

    /// Conversation candidate: 4:1 like semantic memory, floor retained.
    /// Only adopted if the evaluation set shows no loss of exact-term recall
    /// (ADR-095 §9.3).
    pub const CONVERSATION_4TO1: Self = Self {
        vector_weight: 0.8,
        text_weight: 0.2,
        min_cosine: Some(0.3),
        pool_factor: 2,
    };

    /// Equal weight, no gate — the legacy `hybrid_search_full` default, kept
    /// so the compat wrapper cannot change behaviour for untouched callers.
    pub const EQUAL_NO_GATE: Self = Self {
        vector_weight: 1.0,
        text_weight: 1.0,
        min_cosine: None,
        pool_factor: 2,
    };

    /// Build a policy from a `(vector_weight, text_weight)` pair, keeping the
    /// shared pool factor and no gate. Used by `acowork-memory`'s hint-type
    /// weights, which vary per hint but never vary the floor.
    pub fn weighted(vector_weight: f64, text_weight: f64) -> Self {
        Self {
            vector_weight,
            text_weight,
            min_cosine: None,
            pool_factor: 2,
        }
    }
}

impl Default for FusionPolicy {
    fn default() -> Self {
        Self::EQUAL_NO_GATE
    }
}

/// Maximum length of one lexical term, shared by write-time keyword
/// sanitization (`acowork-memory::keyword`) and query-time analysis
/// (`acowork-runtime::search::analyzer`).
///
/// The two producers are deliberately separate — one governs what enters the
/// BM25 index, the other governs what is asked of it — but they share this
/// limit because the governance intent is identical: a term longer than this
/// is a sentence, and a sentence in a keyword field pollutes BM25. It lives
/// here rather than in either consumer because `acowork-memory` and
/// `acowork-runtime::search` must not depend on each other (ADR-095 §5.3
/// dependency red line); `acowork-core` is the only cycle-free common point.
pub const MAX_TERM_CHARS: usize = 30;

/// Common no-op tokens that never help lexical retrieval.
///
/// Shared for the same reason as [`MAX_TERM_CHARS`]. Note this list is
/// tuned for *memory content* words, not for general NLP: it is short on
/// purpose, because dropping a term from a query loses recall, and an
/// uncalibrated stopword list is a silent recall bug.
pub const STOPWORDS: &[&str] = &[
    "the", "a", "user", "fact", "memory", "note", "info", "data", "thing", "item", "stuff",
    "kind", "type", "way", "something", "anything", "everything", "one", "two", "three", "yes",
    "no", "ok", "okay", "um", "uh", "hmm", "oh", "ah", "wow",
];

/// Whether `term` is a stopword. Case-insensitive by contract: callers pass
/// already-lowercased terms (both producers lowercase first).
pub fn is_stopword(term: &str) -> bool {
    STOPWORDS.contains(&term)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzed_terms_win_over_raw() {
        let terms: Vec<String> = vec!["rust".into(), "borrow".into()];
        let q = LexicalQuery { raw: "rust borrow checker", terms: &terms };
        assert_eq!(q.lexical_plan(), LexicalPlan::Terms(&terms));
    }

    #[test]
    fn empty_term_list_falls_back_to_raw_not_to_nothing() {
        // ADR-095 D2: an analyzer miss must not cost recall. This is the one
        // assertion that protects the whole fallback rule.
        let q = LexicalQuery::raw_only("how does tokio schedule tasks");
        assert_eq!(q.lexical_plan(), LexicalPlan::Raw("how does tokio schedule tasks"));
    }

    #[test]
    fn whitespace_only_raw_skips_the_lexical_source() {
        let q = LexicalQuery::raw_only("   ");
        assert_eq!(q.lexical_plan(), LexicalPlan::Skip);
    }

    #[test]
    fn empty_terms_with_non_empty_raw_still_runs_raw() {
        // `terms: &[]` and a real raw text is the legacy shape, not a miss.
        let q = LexicalQuery { raw: "cache", terms: &[] };
        assert_eq!(q.lexical_plan(), LexicalPlan::Raw("cache"));
    }

    #[test]
    fn matched_by_serializes_to_snake_case_wire_names() {
        // Desktop's DTO switch matches on these strings; renaming a variant
        // without renaming the wire value is a silent UI regression.
        assert_eq!(serde_json::to_string(&MatchedBy::Vector).unwrap(), "\"vector\"");
        assert_eq!(serde_json::to_string(&MatchedBy::Text).unwrap(), "\"text\"");
        assert_eq!(serde_json::to_string(&MatchedBy::Both).unwrap(), "\"both\"");
    }

    #[test]
    fn semantic_4to1_weights_are_vector_dominant_and_sum_to_one() {
        let p = FusionPolicy::SEMANTIC_4TO1;
        assert!(p.vector_weight > p.text_weight);
        assert!((p.vector_weight + p.text_weight - 1.0).abs() < 1e-9);
    }

    #[test]
    fn conversation_baseline_keeps_the_cosine_floor_memory_does_not_have() {
        // The floor is scope-specific: message text is short and noisy, memory
        // content is dense. Collapsing the two would silently change one of them.
        assert!(FusionPolicy::CONVERSATION_BASELINE.min_cosine.is_some());
        assert!(FusionPolicy::SEMANTIC_4TO1.min_cosine.is_none());
    }
}
