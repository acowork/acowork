//! Deterministic query analyzer (ADR-095 D2, design 28 §3.1).
//!
//! Pure function: no I/O, no global state, no model call. Same input and same
//! [`AnalyzerConfig`] always produce the same [`DualQuery`], which is what
//! makes the retrieval pipeline testable and the evaluation harness
//! reproducible — the reason ADR-095 rejected LLM query expansion as a
//! prerequisite rather than adopting it first.
//!
//! # The two query expressions
//!
//! A full natural-language sentence and a good BM25 query are different
//! objects, and forcing one string to serve both is the bug this module fixes:
//!
//! * `semantic_text` — the **raw** query, always. Embeddings are trained on
//!   sentences, so the whole utterance is the right input for cosine.
//! * `lexical_terms` — bounded terms for BM25/FTS. A whole sentence as one
//!   FTS phrase demands a verbatim occurrence, so a paraphrased ask returns
//!   nothing; terms OR-ed instead recover the lexical evidence.
//!
//! # What this module deliberately does NOT do
//!
//! * **No FTS escaping.** Terms are emitted bare. Quoting and operator
//!   neutralization live in `SqliteStore::text_search_terms`, the single
//!   escaping implementation (D2). Two escaping rules in two layers drift.
//! * **No Chinese segmentation in the default config.** `CjkStrategy::Keep`
//!   passes a CJK run through unchanged, which is byte-identical to the
//!   pre-ADR-095 behavior — zero regression. `CjkStrategy::Trigram` exists as
//!   an evaluation switch only: ADR-095 forbids treating an untested 3-gram OR
//!   query as a validated strategy (§9.2), so it is not the default and the
//!   Phase-0 harness decides whether it ever should be.

use acowork_core::retrieval::{MAX_TERM_CHARS, is_stopword};
use serde::{Deserialize, Serialize};

/// The dual query expression consumed by conversation/memory retrieval
/// (ADR-095 D2).
///
/// Producers differ — the deterministic analyzer, the `memory_recall` tool's
/// LLM-extracted keywords, or the raw-query fallback — but there is exactly
/// one consumer shape, which is what lets one engine serve all three paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DualQuery {
    /// Raw full user query. Always the embedding input.
    pub semantic_text: String,
    /// Deterministic lexical terms for BM25/FTS.
    ///
    /// **Empty means "no analyzed terms", not "search for nothing"**: the
    /// caller falls back to `semantic_text` at the storage layer. That
    /// distinction is the D2 no-recall-loss guarantee, so never "repair" an
    /// empty list into a term list here.
    pub lexical_terms: Vec<String>,
}

impl DualQuery {
    /// A query expression with no lexical analysis — the pre-ADR-095 single
    /// query form, kept so the `off` mode and every fallback path are
    /// expressible rather than special-cased at the call site.
    pub fn raw(semantic_text: impl Into<String>) -> Self {
        Self {
            semantic_text: semantic_text.into(),
            lexical_terms: Vec::new(),
        }
    }

    /// Whether lexical retrieval should use [`Self::lexical_terms`] rather than
    /// re-tokenizing the raw query.
    pub fn has_lexical_terms(&self) -> bool {
        !self.lexical_terms.is_empty()
    }

    /// The term list to hand the lexical executor, applying the D2 fallback.
    ///
    /// When analysis produced nothing usable this returns a **single-element
    /// list holding the raw query**, so the caller always has a term list to
    /// pass and an analyzer miss degrades to exactly the pre-ADR-095 behavior
    /// instead of to zero results. The executor tokenizes/escapes it — this
    /// module does not.
    pub fn lexical_terms_or_raw(&self) -> Vec<String> {
        if self.lexical_terms.is_empty() {
            vec![self.semantic_text.clone()]
        } else {
            self.lexical_terms.clone()
        }
    }
}

/// How to treat a contiguous CJK run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CjkStrategy {
    /// Pass the whole run as one term. Identical to the pre-ADR-095 behavior:
    /// zero regression, and the only mode approved for production until the
    /// evaluation set says otherwise.
    #[default]
    Keep,
    /// Expand runs into overlapping 3-grams. Evaluation switch only — it
    /// raises recall for partial-phrase asks and is not enabled by default
    /// because it is uncalibrated (ADR-095 §9.2).
    Trigram,
}

/// What the analyzer is allowed to influence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnalyzerMode {
    /// Analyzer never runs; retrieval uses the raw query only. The rollback
    /// switch (ADR-095 §7 Phase 1). The default, so that shipping this code
    /// changes nothing until an operator opts in.
    #[default]
    Off,
    /// Production keeps the raw-query path; the analyzed path runs
    /// **additionally** and its differences are logged for comparison.
    /// Nothing user-visible changes (design 28 §4.3).
    Shadow,
    /// Analyzed terms drive lexical retrieval.
    On,
}

impl AnalyzerMode {
    /// Stable wire name, matching the `search.analyzer_mode` config value and
    /// the `search_meta.analyzer_mode` field in `/search` responses.
    pub fn as_str(self) -> &'static str {
        match self {
            AnalyzerMode::Off => "off",
            AnalyzerMode::Shadow => "shadow",
            AnalyzerMode::On => "on",
        }
    }
}

/// Analyzer bounds. Every limit is a recall/precision trade stated out loud
/// rather than buried in a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalyzerConfig {
    /// Input longer than this many chars is truncated before analysis
    /// (ADR-095 §6 input bound). The *semantic* text is never truncated —
    /// only the term extraction, so a long question still embeds whole.
    pub max_input_chars: usize,
    /// Maximum terms emitted. Beyond this, later terms are dropped: an
    /// unbounded term list turns one FTS query into an OR explosion.
    pub max_terms: usize,
    /// Minimum term length. Aligned with the FTS5 `trigram` tokenizer floor —
    /// a shorter term cannot match, so emitting one is pure noise.
    pub min_term_chars: usize,
    /// Maximum single-term length (shared with write-time keyword governance).
    pub max_term_chars: usize,
    /// Drop [`acowork_core::retrieval::STOPWORDS`] entries.
    pub drop_stopwords: bool,
    /// CJK run handling.
    pub cjk: CjkStrategy,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            // ADR-095 §6 input bound.
            max_input_chars: 512,
            // Design 28 §3.1 v1 rule.
            max_terms: 16,
            // Matches `acowork_sqlite::TRIGRAM_MIN`.
            min_term_chars: 3,
            // Same constant the keyword sanitizer uses.
            max_term_chars: MAX_TERM_CHARS,
            drop_stopwords: true,
            cjk: CjkStrategy::default(),
        }
    }
}

/// Analyze `raw` into the dual query expression (ADR-095 D2).
///
/// Never fails and never returns an error to signal "no terms": an empty
/// `lexical_terms` is the documented miss, and the caller's fallback keeps the
/// original recall. That is why this returns a value instead of an `Option`.
pub fn analyze(raw: &str, cfg: &AnalyzerConfig) -> DualQuery {
    let semantic_text = raw.trim().to_string();
    if semantic_text.is_empty() {
        return DualQuery::raw(semantic_text);
    }

    // Bound the analysis input, not the semantic input: the embedding still
    // sees the whole utterance.
    let bounded: String = semantic_text.chars().take(cfg.max_input_chars).collect();

    let mut terms: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for run in split_runs(&bounded) {
        let candidates: Vec<String> = match run.kind {
            RunKind::Latin => run
                .text
                .split(|c: char| !is_word_char(c))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_lowercase())
                .collect(),
            RunKind::Cjk => match cfg.cjk {
                CjkStrategy::Keep => vec![run.text.to_string()],
                CjkStrategy::Trigram => ngrams(run.text, 3),
            },
        };

        for term in candidates {
            let chars = term.chars().count();
            if chars < cfg.min_term_chars || chars > cfg.max_term_chars {
                continue;
            }
            if cfg.drop_stopwords && is_stopword(&term) {
                continue;
            }
            if seen.insert(term.clone()) {
                terms.push(term);
                if terms.len() >= cfg.max_terms {
                    break;
                }
            }
        }
        if terms.len() >= cfg.max_terms {
            break;
        }
    }

    DualQuery {
        semantic_text,
        lexical_terms: terms,
    }
}

/// Character classes the splitter distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    /// ASCII/Latin script: split further into words on separators.
    Latin,
    /// Contiguous CJK: kept whole (`Keep`) or 3-gram expanded (`Trigram`).
    Cjk,
}

/// A maximal run of one script class.
struct Run<'a> {
    kind: RunKind,
    text: &'a str,
}

/// Split input into maximal Latin and CJK runs.
///
/// Whitespace and ASCII punctuation are **not** separators between runs of the
/// same class — they are handled inside [`RunKind::Latin`] splitting, so
/// "gateway route 配置 项" produces runs `latin("gateway route")`,
/// `cjk("配置")`, `latin("项")`... which is what keeps behavior aligned with
/// the FTS executor's whitespace tokenization (design 28 F6).
fn split_runs(input: &str) -> Vec<Run<'_>> {
    let mut runs: Vec<Run<'_>> = Vec::new();
    let bytes = input.as_bytes();
    let mut start = 0usize;
    let mut current: Option<RunKind> = None;

    // Track by char boundary: CJK detection needs the scalar value.
    for (i, ch) in input.char_indices() {
        let kind = if is_cjk(ch) {
            RunKind::Cjk
        } else if ch.is_whitespace() {
            // Whitespace closes the open run but does not start one.
            if let Some(k) = current.take() {
                push_run(&mut runs, input, start, i, k);
            }
            start = i + ch.len_utf8();
            continue;
        } else {
            RunKind::Latin
        };

        match current {
            Some(k) if k == kind => {}
            Some(k) => {
                push_run(&mut runs, input, start, i, k);
                current = Some(kind);
                start = i;
            }
            None => {
                current = Some(kind);
                start = i;
            }
        }
    }
    if let Some(k) = current.take() {
        push_run(&mut runs, input, start, bytes.len(), k);
    }
    runs
}

fn push_run<'a>(runs: &mut Vec<Run<'a>>, input: &'a str, from: usize, to: usize, kind: RunKind) {
    let text = &input[from..to];
    if !text.trim().is_empty() {
        runs.push(Run { kind, text });
    }
}

/// Keep letters/digits and the in-word connectors `-` / `_` (design 28 §3.1:
/// `hybrid_search` and `pool_factor` must not be split into fragments).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() && !is_cjk(c) || c == '-' || c == '_'
}

fn is_cjk(c: char) -> bool {
    // CJK Unified Ideographs + Ext A, Hiragana, Katakana, Hangul. Enough for
    // the query path; the storage layer's trigram tokenizer decides matching.
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
        || ('\u{3400}'..='\u{4DBF}').contains(&c)
        || ('\u{3040}'..='\u{30FF}').contains(&c)
        || ('\u{AC00}'..='\u{D7AF}').contains(&c)
}

/// Overlapping character n-grams, used only by [`CjkStrategy::Trigram`].
fn ngrams(text: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < n {
        // Shorter than one gram: emit the run itself rather than nothing, so
        // the executor's `LIKE` fallback can still match it.
        return if chars.is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        };
    }
    chars.windows(n).map(|w| w.iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AnalyzerConfig {
        AnalyzerConfig::default()
    }

    fn terms(raw: &str) -> Vec<String> {
        analyze(raw, &cfg()).lexical_terms
    }

    #[test]
    fn semantic_text_is_the_raw_query_verbatim() {
        // D2: the whole utterance drives embedding, so it must survive analysis
        // untouched (apart from trimming).
        let raw = "上次网关的 MQTT 端口配置是多少来着";
        let dq = analyze(raw, &cfg());
        assert_eq!(dq.semantic_text, raw);
    }

    #[test]
    fn english_splits_on_whitespace_and_lowercases() {
        assert_eq!(terms("Gateway ROUTE config"), ["gateway", "route", "config"]);
    }

    #[test]
    fn in_word_hyphen_and_underscore_are_kept() {
        // Splitting `hybrid_search` into "hybrid"+"search" would invent terms
        // the corpus never contains.
        assert_eq!(terms("hybrid_search full-text"), ["hybrid_search", "full-text"]);
    }

    #[test]
    fn punctuation_and_operators_are_stripped_not_emitted() {
        // Terms are bare; FTS escaping is the executor's job. An operator
        // leaking through as a *term* is still neutralized by phrase quoting.
        let t = terms("does it support AND/OR? (maybe)");
        assert!(t.iter().all(|x| !x.contains('(') && !x.contains('?')), "{t:?}");
        assert!(t.contains(&"support".to_string()));
    }

    #[test]
    fn terms_below_trigram_minimum_are_dropped() {
        // FTS5 trigram cannot match a 1-2 char pattern; emitting one is noise.
        // ("and" survives because it is 3 chars and is not in the deliberately
        // small shared stopword table — see acowork-core::retrieval::STOPWORDS.)
        assert_eq!(terms("go do it now"), ["now"]);
    }

    #[test]
    fn stopwords_are_dropped() {
        // "thing"/"stuff"/"data" are in the shared table; "gateway" is not.
        let t = terms("the gateway data thing");
        assert_eq!(t, ["gateway"], "stopwords dropped, content term kept: {t:?}");
    }

    #[test]
    fn all_stopword_query_yields_no_terms_and_falls_back() {
        // The D2 guarantee: an analyzer miss must degrade to the raw query,
        // never to zero recall.
        let dq = analyze("the user info", &cfg());
        assert!(!dq.has_lexical_terms());
        assert_eq!(dq.lexical_terms_or_raw(), ["the user info"]);
    }

    #[test]
    fn cjk_run_is_kept_whole_by_default() {
        // A run long enough for the trigram floor is passed through unchanged.
        assert_eq!(terms("网关路由配置"), ["网关路由配置"]);
    }

    #[test]
    fn keep_strategy_reproduces_the_old_executor_exactly() {
        // Zero-regression proof. The pre-ADR-095 path was
        // `split_whitespace().filter(chars >= 3)`, so a 2-character CJK run was
        // ALREADY dropped there — `Keep` dropping it is the same behavior, not
        // a new loss. Improving this is what `CjkStrategy::Trigram` is for, and
        // only after the evaluation set backs it (ADR-095 §9.2).
        assert_eq!(terms("网关 route 配置"), ["route"]);
    }

    #[test]
    fn cjk_trigram_strategy_is_opt_in_only() {
        let mut c = cfg();
        assert_eq!(c.cjk, CjkStrategy::Keep, "Trigram must never be the default");
        c.cjk = CjkStrategy::Trigram;
        let t = analyze("网关路由", &c).lexical_terms;
        assert_eq!(t, ["网关路", "关路由"]);
    }

    #[test]
    fn cjk_shorter_than_one_gram_yields_no_terms_and_falls_back() {
        let mut c = cfg();
        c.cjk = CjkStrategy::Trigram;
        // 2 chars cannot form a 3-gram, and emitting the raw 2-char run would
        // only be dropped by the min-length filter anyway. Empty terms ⇒ the
        // caller uses the raw query ⇒ the storage layer's `LIKE` fallback
        // handles it (which is exactly how a 2-char query works today).
        let dq = analyze("网关", &c);
        assert!(!dq.has_lexical_terms());
        assert_eq!(dq.lexical_terms_or_raw(), ["网关"]);
    }

    #[test]
    fn term_count_is_capped() {
        let raw = "alpha1 beta2 gamma3 delta4 epsilon5 zeta6 eta7 theta8 iota9 kappa1 lambda2 mu3 nu4 xi5 omicron6 pi7 rho8 sigma9";
        let t = terms(raw);
        assert_eq!(t.len(), cfg().max_terms, "cap must bind: {t:?}");
    }

    #[test]
    fn input_is_truncated_for_analysis_but_not_for_semantics() {
        let long = format!("{} tailterm", "filler ".repeat(300));
        let dq = analyze(&long, &cfg());
        assert_eq!(dq.semantic_text, long.trim());
        assert!(
            dq.lexical_terms.len() < 400,
            "analysis must be bounded: {}",
            dq.lexical_terms.len()
        );
    }

    #[test]
    fn long_terms_are_dropped() {
        // A whole clause is not a term; > MAX_TERM_CHARS is a sentence.
        let t = terms("this is definitely way over the maximum term length allowed here");
        assert!(t.iter().all(|x| x.chars().count() <= cfg().max_term_chars));
    }

    #[test]
    fn duplicates_are_removed_case_insensitively() {
        assert_eq!(terms("Gateway gateway GATEWAY"), ["gateway"]);
    }

    #[test]
    fn empty_and_whitespace_input_produce_no_terms() {
        assert!(terms("   ").is_empty());
        assert!(analyze("", &cfg()).semantic_text.is_empty());
    }

    #[test]
    fn mixed_script_keeps_both_evidences() {
        // The exact-entity case from ADR-095 §2.1: the product name must
        // survive as a lexical term even inside a Chinese sentence.
        let t = terms("为什么 acowork-gateway 启动失败");
        assert!(t.contains(&"acowork-gateway".to_string()), "{t:?}");
    }

    #[test]
    fn analyzer_is_deterministic() {
        let raw = "全局搜索 hybrid 融合 权重 4:1 配置";
        assert_eq!(analyze(raw, &cfg()), analyze(raw, &cfg()));
    }

    #[test]
    fn raw_constructor_expresses_the_off_path() {
        let dq = DualQuery::raw("网关 配置");
        assert!(!dq.has_lexical_terms());
        assert_eq!(dq.semantic_text, "网关 配置");
    }
}
