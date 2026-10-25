//! Conversation-scope retrieval variant comparison harness (ADR-095 D6, design 28 §3.3).
//!
//! ## What this answers
//!
//! "If we change the query analyzer, the fusion weights, or the cosine floor,
//! which measured quality moves, and by how much?" ADR-095 D6 makes a Phase-2
//! weight change conditional on that being answered, so this file is the gate,
//! not documentation. The analyzer knob lives here and nowhere else: it is a
//! Runtime component (`crate::search::analyzer`), and the memory service
//! deliberately runs with `analyzer_mode = off`.
//!
//! ## Scope discipline
//!
//! This harness seeds and scores **conversation rows only**. The memory scope is
//! measured by `core/acowork-memory/tests/retrieval_eval.rs`. The split follows
//! ADR-101: the two engines are separate binaries, and neither test may reach
//! into the other — a fused-scope harness would have to link both, which is how
//! this file used to end up naming memory types. A query whose ground truth is
//! entirely in the other scope is skipped, not counted.
//!
//! The metric math, the dataset reader, the stand-in projection and the report
//! shape are shared through the `acowork_eval` crate, because integration tests
//! in sibling crates cannot import each other and two definitions of nDCG would
//! mean two yardsticks for one weight change. It is pulled in as a
//! dev-dependency, never a dependency: measurement code must not end up linked
//! into the runtime binary.
//!
//! ## Relationship to `memory_retrieval_bench.rs`
//!
//! Deliberately not merged. That benchmark (in `acowork-memory`) asserts
//! **absolute** quality gates on a store shaped like real agent memory; this one
//! compares **variants** on a hand-annotated query set and prints a report.
//!
//! ## Determinism
//!
//! Embeddings come from `dev/eval/retrieval/embeddings.json` and are never
//! fetched from a live model, so two runs of one variant are byte-comparable.
//! The checked-in file is a hashed-token projection: a **stand-in** that makes
//! lexical/semantic agreement reproducible, but is not a calibrated embedding
//! space. Regenerate it with the production model before a weight change is
//! justified by these numbers.
//!
//! ## Running
//!
//! ```bash
//! cd core
//! cargo test -p acowork-runtime --test retrieval_eval -- --ignored --nocapture
//! ```
//!
//! The report lands in `dev/eval/retrieval/reports/conversation-report.{json,md}`.

use std::collections::{HashMap, HashSet};

use acowork_core::retrieval::{FusionPolicy, LexicalQuery};
use acowork_eval::{
    exact_terms_present, stand_in_embedding, DatasetQuery, EvalDataset, EvalReport, QualityTally,
    VariantReport,
};
use acowork_runtime::search::analyzer::{analyze, AnalyzerConfig};

const K: usize = 10;
/// The scope this harness owns; annotations of any other scope are skipped.
const SCOPE: &str = "conversation";

// ── fixture ─────────────────────────────────────────────────────────────────

/// The seeded conversation index plus the maps annotations resolve through.
///
/// `SqliteStore` here is only the shared connection the index attaches to
/// (ADR-082 — one `memory/private.sqlite`, one connection); no memory row is
/// ever written or read by this harness.
struct Fixture {
    conv: acowork_sqlite::ConversationStore,
    /// row id -> indexed text. Built from the corpus rather than read back
    /// through the store, so the harness depends on no private accessor.
    text_by_id: HashMap<u64, String>,
    /// annotation key -> row id
    by_key: HashMap<(String, usize), u64>,
    dim: usize,
    conv_rows: usize,
}

fn build_fixture(ds: &EvalDataset) -> Fixture {
    let store = std::sync::Arc::new(
        acowork_sqlite::SqliteStore::open_in_memory(ds.dim).expect("open store"),
    );
    let conv = acowork_sqlite::ConversationStore::from_store(store).expect("conv store");

    let mut text_by_id = HashMap::new();
    let mut by_key = HashMap::new();
    let mut conv_rows = 0usize;

    for (i, row) in ds.corpus.iter().enumerate() {
        if row.scope() != SCOPE {
            continue;
        }
        let id = conv
            .index_message(
                &row.session_id,
                row.message_index,
                &row.role,
                &row.content,
                ds.vector(i),
            )
            .expect("index message");
        text_by_id.insert(id, row.content.clone());
        by_key.insert((row.session_id.clone(), row.message_index), id);
        conv_rows += 1;
    }

    Fixture {
        conv,
        text_by_id,
        by_key,
        dim: ds.dim,
        conv_rows,
    }
}

impl Fixture {
    /// Resolve a query's in-scope annotations to row ids. A miss is a dataset
    /// error and must fail loudly: silently skipping it would score a query as
    /// "no relevant results" and quietly flatter every variant.
    fn resolve(&self, q: &DatasetQuery) -> Vec<u64> {
        q.annotations_for(SCOPE)
            .into_iter()
            .map(|a| {
                *self
                    .by_key
                    .get(&(
                        a.session_id.clone().unwrap_or_default(),
                        a.message_index.unwrap_or_default(),
                    ))
                    .unwrap_or_else(|| panic!("annotation names no conversation row: {:?}", a))
            })
            .collect()
    }

    fn texts_for(&self, ids: &[u64]) -> Vec<String> {
        ids.iter()
            .filter_map(|id| self.text_by_id.get(id).cloned())
            .collect()
    }
}

// ── variants ────────────────────────────────────────────────────────────────

/// A retrieval configuration under test. Each field is one of the knobs ADR-095
/// authorizes only with measurement behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// One query text drives both sources, equal weight.
    Baseline,
    /// Analyzer terms drive FTS, raw text drives cosine, equal weight.
    DualEqual,
    /// The same dual query at `SEMANTIC_4TO1` — the ADR's initial memory policy.
    DualSemantic4To1,
    /// Dual query at 4:1 with the conversation cosine floor.
    DualSemantic4To1WithFloor,
}

impl Variant {
    fn name(self) -> &'static str {
        match self {
            Variant::Baseline => "baseline",
            Variant::DualEqual => "dual_equal",
            Variant::DualSemantic4To1 => "dual_4to1",
            Variant::DualSemantic4To1WithFloor => "dual_4to1_floor",
        }
    }

    fn uses_analyzer(self) -> bool {
        !matches!(self, Variant::Baseline)
    }

    fn policy(self) -> FusionPolicy {
        match self {
            Variant::Baseline | Variant::DualEqual => FusionPolicy::EQUAL_NO_GATE,
            Variant::DualSemantic4To1 => FusionPolicy::SEMANTIC_4TO1,
            Variant::DualSemantic4To1WithFloor => FusionPolicy::CONVERSATION_BASELINE,
        }
    }
}

// ── runner ──────────────────────────────────────────────────────────────────

/// Run one variant over the whole dataset.
fn run_variant(
    fx: &Fixture,
    queries: &[DatasetQuery],
    variant: Variant,
    analyzer_cfg: &AnalyzerConfig,
) -> VariantReport {
    let mut tally = QualityTally::new(K);
    let mut per_query = Vec::new();
    let mut skipped_out_of_scope = 0usize;

    for q in queries {
        if q.out_of_scope(SCOPE) {
            // Nothing here for the conversation index to find: scoring it would
            // either dilute the averages or invent false recalls.
            skipped_out_of_scope += 1;
            continue;
        }

        let dual = analyze(&q.query, analyzer_cfg);
        let terms: &[String] = if variant.uses_analyzer() {
            &dual.lexical_terms
        } else {
            &[]
        };
        let lexical = LexicalQuery {
            raw: &q.query,
            terms,
        };
        let hits = fx
            .conv
            .search_detailed(
                &lexical,
                stand_in_embedding(&q.query, fx.dim).as_deref(),
                K,
                &variant.policy(),
            )
            .expect("conversation search");
        let retrieved: Vec<u64> = hits.iter().map(|(_, fused)| fused.id).collect();

        let relevant = fx.resolve(q);
        // The report row and the aggregate come from one call: the analyzer term
        // count is this scope's extra column, `None` on the memory side.
        per_query.push(tally.record_query(
            q,
            &retrieved,
            &relevant,
            &fx.texts_for(&retrieved),
            Some(terms.len()),
        ));
    }

    VariantReport {
        variant: variant.name().to_string(),
        metrics: tally.finish(),
        per_query,
        skipped_out_of_scope,
    }
}

// ── the test ────────────────────────────────────────────────────────────────

/// Ignored by design: this is an evaluation, not a CI assertion. It has no
/// pass/fail threshold because ADR-095 D6 says the thresholds get agreed at
/// review time, from the first real numbers this produces.
#[test]
#[ignore]
fn compare_retrieval_variants() {
    // A missing dataset fails loudly rather than reporting "ok" over nothing —
    // see `EvalDataset::load_repo`.
    let ds = EvalDataset::load_repo();
    let fx = build_fixture(&ds);
    let analyzer_cfg =
        acowork_runtime::search::policy::SearchPolicyConfig::default().analyzer_config();

    let variants = [
        Variant::Baseline,
        Variant::DualEqual,
        Variant::DualSemantic4To1,
        Variant::DualSemantic4To1WithFloor,
    ];
    let reports: Vec<VariantReport> = variants
        .iter()
        .map(|v| run_variant(&fx, &ds.queries, *v, &analyzer_cfg))
        .collect();

    let report = EvalReport {
        generated_by: "core/acowork-runtime/tests/retrieval_eval.rs".to_string(),
        scope: SCOPE.to_string(),
        k: K,
        dataset: format!("{} queries", ds.queries.len()),
        corpus_rows: fx.conv_rows,
        variants: reports,
        embedding_caveat: EvalReport::CAVEAT.to_string(),
    };

    let body = per_query_section(&report);
    let (json_path, md_path) = report.write(&EvalDataset::dir(), &body);
    println!("{}\n{body}", report.render_summary());
    println!("\nwrote {} and {}", json_path.display(), md_path.display());
}

/// The scope-specific half of the markdown: the per-query table.
///
/// Term counts are read from an analyzer variant on purpose — the baseline is
/// *defined* to have zero terms, so printing it would report "0 terms" for every
/// query and hide the one signal this column exists for.
fn per_query_section(r: &EvalReport) -> String {
    let mut s = String::from("\n## Per query\n\n");
    if let Some(v) = r
        .variants
        .iter()
        .find(|v| v.variant != "baseline")
        .or_else(|| r.variants.first())
    {
        s.push_str("| id | lang | kind | relevant | retrieved | analyzer terms |\n");
        s.push_str("|---|---|---|---|---|---|\n");
        for q in &v.per_query {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                q.id,
                q.lang,
                q.kind,
                q.relevant,
                q.retrieved,
                q.analyzer_terms
                    .map_or("n/a".to_string(), |n| n.to_string()),
            ));
        }
    }
    s
}

// ── harness self-checks ─────────────────────────────────────────────────────
//
// The eval is `#[ignore]`d, but these run in CI. A metric definition that is
// wrong produces numbers nobody can trust, and the D6 gate rests on them — so
// what is checked here is the harness: that the variants really differ, that
// scope filtering never scores a query this engine was not given, and that the
// shared kernel is the one the memory-side harness uses too.

#[cfg(test)]
mod harness_tests {
    use super::*;
    use acowork_eval::{recall_at_k, Annotation};

    fn query(id: &str, relevant: Vec<Annotation>) -> DatasetQuery {
        DatasetQuery {
            id: id.to_string(),
            lang: "en".to_string(),
            kind: "exact_entity".to_string(),
            query: id.to_string(),
            relevant,
            exact_terms: vec![],
        }
    }

    fn conv_annotation(session: &str, index: usize) -> Annotation {
        Annotation {
            scope: "conversation".to_string(),
            content: None,
            session_id: Some(session.to_string()),
            message_index: Some(index),
        }
    }

    #[test]
    fn variants_differ_in_at_least_one_knob() {
        // Guards against a variant being added without wiring its policy, which
        // would silently duplicate a row in the report.
        let all = [
            Variant::Baseline,
            Variant::DualEqual,
            Variant::DualSemantic4To1,
            Variant::DualSemantic4To1WithFloor,
        ];
        let names: Vec<&str> = all.iter().map(|v| v.name()).collect();
        assert_eq!(names.len(), names.iter().collect::<HashSet<_>>().len());
        let policies: Vec<(f64, f64)> = all
            .iter()
            .map(|v| (v.policy().vector_weight, v.policy().text_weight))
            .collect();
        assert_ne!(policies[0], policies[2], "baseline and 4:1 must differ");
        assert!(
            all.iter()
                .map(|v| v.uses_analyzer())
                .collect::<Vec<_>>()
                .windows(2)
                .any(|w| w[0] != w[1]),
            "some variant must exercise the analyzer"
        );
    }

    #[test]
    fn a_memory_only_query_is_out_of_scope_for_conversation() {
        let mem = query(
            "q-mem",
            vec![Annotation {
                scope: "memory".to_string(),
                content: Some("x".to_string()),
                session_id: None,
                message_index: None,
            }],
        );
        assert!(mem.out_of_scope(SCOPE));
        assert!(mem.annotations_for(SCOPE).is_empty());

        let mixed = query(
            "q-mixed",
            vec![conv_annotation("s-1", 0), mem.relevant[0].clone()],
        );
        assert!(!mixed.out_of_scope(SCOPE));
        assert_eq!(mixed.annotations_for(SCOPE).len(), 1);
    }

    #[test]
    fn metrics_come_from_the_shared_kernel() {
        // The number the memory-side harness reports has to mean the same thing.
        // Denominator `K`, never `min(K, returned)`: dividing by the returned
        // count is the flaw the archived benchmarks had — it let a one-item
        // retriever look perfect.
        assert!((recall_at_k(&[1, 2, 5], &[1, 2, 3, 4], K) - 0.5).abs() < 1e-12);
        let mut t = QualityTally::new(K);
        t.record(&[1], &[1]);
        assert!(t.finish().ndcg_at_k > 0.0);
    }

    #[test]
    fn empty_relevant_set_is_not_averaged_into_the_variant() {
        // `no_relevant` queries are measured by false-recall, not by these.
        let mut t = QualityTally::new(K);
        let junk = t.record(&[1, 2], &[]);
        assert!(!junk.scored);
        assert_eq!(t.finish().queries, 0);
        assert_eq!(t.finish().false_recall_rate, Some(1.0));
    }

    #[test]
    fn exact_term_retention_is_case_insensitive_and_requires_all_terms() {
        let texts = vec!["Gateway MQTT broker on port 19875".to_string()];
        assert!(exact_terms_present(
            &texts,
            &["mqtt".to_string(), "19875".to_string()]
        ));
        assert!(!exact_terms_present(
            &texts,
            &["mqtt".to_string(), "kafka".to_string()]
        ));
        // An unannotated query must not claim retention it did not measure.
        assert!(!exact_terms_present(&texts, &[]));
    }
}

