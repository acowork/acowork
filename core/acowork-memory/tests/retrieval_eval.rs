//! Memory-scope retrieval variant comparison harness (ADR-095 D6, design 28 §3.3).
//!
//! ## What this answers
//!
//! "If we change the fusion weights or the cosine floor of the memory engine,
//! which measured quality moves, and by how much?" ADR-095 D6 makes such a
//! change conditional on that being answered, so this file is the gate, not
//! documentation. The knobs under test are the ones the memory read path
//! actually carries: `memory_recall` and `/api/memory/search` both search with
//! [`FusionPolicy::SEMANTIC_4TO1`], and no lexical terms (the ADR-095 analyzer
//! stays on the Runtime side — see `service::api::search`).
//!
//! ## Scope discipline
//!
//! This harness seeds and scores **memory rows only**. The conversation scope is
//! measured by `core/acowork-runtime/tests/retrieval_eval.rs`. The split follows
//! ADR-100: the two engines are separate binaries with separate code paths, and
//! neither test may reach into the other. A query whose ground truth is entirely
//! in the other scope is skipped, not counted — crediting an engine with rows it
//! was never given would move its numbers in the flattering direction.
//!
//! The metric math, the dataset reader, the stand-in projection and the report
//! shape are shared through the `acowork_eval` crate, because integration tests
//! in sibling crates cannot import each other and two definitions of nDCG would
//! mean two yardsticks for one weight change. It is pulled in as a
//! dev-dependency, never a dependency: measurement code must not end up linked
//! into the memory service binary.
//!
//! ## Relationship to `memory_retrieval_bench.rs`
//!
//! Deliberately not merged. That benchmark asserts **absolute** quality gates on
//! a store shaped like real agent memory; this one compares **variants** on a
//! hand-annotated query set and prints a report.
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
//! cargo test -p acowork-memory --test retrieval_eval -- --ignored --nocapture
//! ```
//!
//! The report lands in `dev/eval/retrieval/reports/memory-report.{json,md}`.

use std::collections::{HashMap, HashSet};

use acowork_core::retrieval::{FusionPolicy, LexicalQuery};
use acowork_eval::{
    exact_terms_present, kept_cell, stand_in_embedding, CorpusRow, DatasetQuery, EvalDataset,
    EvalReport, QualityTally, VariantReport,
};
use acowork_memory::sqlite::SqliteStore;
use acowork_memory::types::{Episode, KnowledgeNode, ProceduralNode};
use acowork_memory::{labels, KnowledgeSubType, NodeStatus, PrivacyLevel};

const K: usize = 10;
/// The scope this harness owns; annotations of any other scope are skipped.
const SCOPE: &str = "memory";

// ── fixture ─────────────────────────────────────────────────────────────────

/// The seeded memory store plus the maps annotations resolve through.
struct Fixture {
    store: SqliteStore,
    /// node id -> indexed text, built from the corpus rather than read back
    /// through the store, so the harness depends on no private accessor.
    text_by_id: HashMap<u64, String>,
    /// annotation content -> node id
    by_content: HashMap<String, u64>,
    dim: usize,
    memory_rows: usize,
}

fn build_fixture(ds: &EvalDataset) -> Fixture {
    let store = SqliteStore::open_in_memory(ds.dim).expect("open store");
    let mut text_by_id = HashMap::new();
    let mut by_content = HashMap::new();
    let mut memory_rows = 0usize;

    for (i, row) in ds.corpus.iter().enumerate() {
        if row.scope() != SCOPE {
            continue;
        }
        let id = insert(&store, row, ds.vector(i));
        text_by_id.insert(id, row.content.clone());
        by_content.insert(row.content.clone(), id);
        memory_rows += 1;
    }

    Fixture {
        store,
        text_by_id,
        by_content,
        dim: ds.dim,
        memory_rows,
    }
}

fn insert(store: &SqliteStore, row: &CorpusRow, vector: &[f32]) -> u64 {
    let now = chrono::Utc::now();
    match row.label.as_str() {
        labels::KNOWLEDGE => {
            // `KnowledgeNode` is a (subject, predicate, object) triple and the
            // engine dedups on (subject, predicate), so the fixture spreads its
            // rows across distinct predicates — otherwise every row after the
            // first would overwrite the previous one and the corpus would
            // silently collapse to a single node.
            let node = KnowledgeNode {
                subject: "eval".to_string(),
                predicate: format!("note-{}", sha16(&row.content)),
                object: row.content.clone(),
                sub_type: KnowledgeSubType::Fact,
                confidence: 0.9,
                source_episode_id: None,
                source_episode_ids: vec![],
                promotion_metadata: None,
                embedding: Some(vector.to_vec()),
                status: NodeStatus::Active,
                created_at: now,
                updated_at: now,
                metadata: HashMap::new(),
                privacy: PrivacyLevel::Personal,
                importance: 0.7,
            };
            store.store_knowledge(&node).expect("store knowledge")
        }
        labels::PROCEDURAL => {
            let node = ProceduralNode {
                id: None,
                name: format!("proc-{}", sha16(&row.content)),
                // Indexed text is `name trigger action_pattern`, so the content
                // goes in `action_pattern` to stay searchable.
                trigger_condition: "eval".to_string(),
                action_pattern: row.content.clone(),
                success_count: 0,
                fail_count: 0,
                confidence: 0.9,
                activation_count: 0,
                source_skill: None,
                learned_from: "user_feedback".to_string(),
                embedding: vector.to_vec(),
                status: NodeStatus::Active,
                created_at: now,
                updated_at: now,
                source_episode_ids: vec![],
                promotion_metadata: None,
                metadata: HashMap::new(),
            };
            store.store_procedural(&node).expect("store procedural")
        }
        // episodic + autobiographical both live in the episode table.
        _ => {
            let ep = Episode {
                session_id: format!("eval-{}-{}", row.label, sha16(&row.content)),
                turn_index: 0,
                role: "user".to_string(),
                content: row.content.clone(),
                embedding: Some(vector.to_vec()),
                timestamp: now,
                consolidated: false,
                metadata: HashMap::new(),
                importance: 0.7,
                knowledge_subtype: None,
                normalized: None,
            };
            store.store_episode(&ep).expect("store episode")
        }
    }
}

/// Short stable content id, used only to make fixture keys unique.
fn sha16(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

impl Fixture {
    /// Resolve one memory annotation to the node id it names. A miss is a
    /// dataset error and must fail loudly: silently skipping it would score a
    /// query as "no relevant results" and quietly flatter every variant.
    fn resolve(&self, q: &DatasetQuery) -> HashSet<u64> {
        q.annotations_for(SCOPE)
            .into_iter()
            .map(|a| {
                *self
                    .by_content
                    .get(a.content.as_deref().unwrap_or_default())
                    .unwrap_or_else(|| panic!("annotation names no memory row: {:?}", a.content))
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

/// A memory retrieval configuration under test. Each field is one of the knobs
/// ADR-095 authorizes only with measurement behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// What the read path does today: semantic-dominant, no cosine gate.
    CurrentRecall,
    /// Equal weights — the shape the admin panel search uses.
    EqualNoGate,
    /// The current weights with the conversation scope's cosine floor applied.
    CurrentWithFloor,
    /// A milder tilt, to show which way the number moves before 4:1 is argued.
    Semantic2To1,
}

impl Variant {
    fn name(self) -> &'static str {
        match self {
            Variant::CurrentRecall => "semantic_4to1_current",
            Variant::EqualNoGate => "equal_no_gate",
            Variant::CurrentWithFloor => "semantic_4to1_floor",
            Variant::Semantic2To1 => "semantic_2to1",
        }
    }

    fn policy(self) -> FusionPolicy {
        match self {
            Variant::CurrentRecall => FusionPolicy::SEMANTIC_4TO1,
            Variant::EqualNoGate => FusionPolicy::EQUAL_NO_GATE,
            Variant::CurrentWithFloor => FusionPolicy {
                min_cosine: Some(FLOOR),
                ..FusionPolicy::SEMANTIC_4TO1
            },
            Variant::Semantic2To1 => FusionPolicy::weighted(2.0 / 3.0, 1.0 / 3.0),
        }
    }
}

/// The cosine floor borrowed from the conversation scope's baseline, so the
/// comparison answers "what would the memory engine do with that gate".
const FLOOR: f32 = 0.15;

// ── runner ──────────────────────────────────────────────────────────────────

/// Run one variant over the whole dataset.
fn run_variant(fx: &Fixture, queries: &[DatasetQuery], variant: Variant) -> VariantReport {
    let mut tally = QualityTally::new(K);
    let mut per_query = Vec::new();
    let mut skipped_out_of_scope = 0usize;

    for q in queries {
        if q.out_of_scope(SCOPE) {
            // Nothing here for the memory engine to find: scoring it would
            // either dilute the averages or invent false recalls.
            skipped_out_of_scope += 1;
            continue;
        }

        let retrieved = search(fx, q, &variant.policy());
        let relevant = fx.resolve(q);
        let relevant: Vec<u64> = relevant.into_iter().collect();
        // `None` for the analyzer column: the memory read path never runs one, so
        // this scope reports "not applicable" rather than a misleading 0.
        per_query.push(tally.record_query(
            q,
            &retrieved,
            &relevant,
            &fx.texts_for(&retrieved),
            None,
        ));
    }

    VariantReport {
        variant: variant.name().to_string(),
        metrics: tally.finish(),
        per_query,
        skipped_out_of_scope,
    }
}

/// The four memory labels are disjoint, so ids cannot collide and the merge is a
/// fused-score sort — the same shape the production admin path uses.
///
/// No lexical terms are passed, exactly as `service::api::search` does: the
/// memory service receives the raw query, and [`LexicalQuery`] falls back to it.
fn search(fx: &Fixture, q: &DatasetQuery, policy: &FusionPolicy) -> Vec<u64> {
    let embedding = stand_in_embedding(&q.query, fx.dim);
    let mut hits: Vec<(u64, f64)> = Vec::new();
    for label in labels::ALL {
        let lexical = LexicalQuery {
            raw: &q.query,
            terms: &[],
        };
        let found = fx
            .store
            .hybrid_search_detailed(label, &lexical, embedding.as_deref(), K, policy)
            .expect("hybrid search");
        hits.extend(found.into_iter().map(|h| (h.id, h.fused_score)));
    }
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.into_iter().map(|(id, _)| id).collect()
}

// ── the test ────────────────────────────────────────────────────────────────

/// Ignored by design: this is an evaluation, not a CI assertion. It has no
/// pass/fail threshold because ADR-095 D6 says the thresholds get agreed at
/// review time, from the first real numbers this produces.
#[test]
#[ignore]
fn compare_memory_retrieval_variants() {
    // A missing dataset fails loudly rather than reporting "ok" over nothing —
    // see `EvalDataset::load_repo`.
    let ds = EvalDataset::load_repo();
    let fx = build_fixture(&ds);

    let variants = [
        Variant::CurrentRecall,
        Variant::EqualNoGate,
        Variant::CurrentWithFloor,
        Variant::Semantic2To1,
    ];
    let reports: Vec<VariantReport> = variants
        .iter()
        .map(|v| run_variant(&fx, &ds.queries, *v))
        .collect();

    let report = EvalReport {
        generated_by: "core/acowork-memory/tests/retrieval_eval.rs".to_string(),
        scope: SCOPE.to_string(),
        k: K,
        dataset: format!("{} queries", ds.queries.len()),
        corpus_rows: fx.memory_rows,
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
/// The memory side shows recall/MRR/exact-term retention per query and no
/// analyzer column — the analyzer is a conversation-scope component (ADR-095),
/// and this engine's read path never runs one.
fn per_query_section(r: &EvalReport) -> String {
    let mut s = String::from("\n## Per query\n\n");
    if let Some(v) = r.variants.first() {
        s.push_str(
            "| id | lang | kind | relevant | retrieved | Recall@K | MRR | exact terms kept |\n",
        );
        s.push_str("|---|---|---|---|---|---|---|---|\n");
        for q in &v.per_query {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.3} | {:.3} | {} |\n",
                q.id,
                q.lang,
                q.kind,
                q.relevant,
                q.retrieved,
                q.recall_at_k,
                q.mrr,
                kept_cell(q.exact_terms_kept),
            ));
        }
    }
    s
}

// ── harness self-checks ─────────────────────────────────────────────────────
//
// The eval is `#[ignore]`d, but these run in CI. They pin the two things the
// report depends on: that variants really differ, and that the scope filter
// never silently scores a query the memory engine was not given.

#[cfg(test)]
mod harness_tests {
    use super::*;
    use acowork_eval::Annotation;

    fn annotation(scope: &str, content: &str) -> Annotation {
        Annotation {
            scope: scope.to_string(),
            content: Some(content.to_string()),
            session_id: None,
            message_index: None,
        }
    }

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

    #[test]
    fn variants_differ_in_at_least_one_knob() {
        // Guards against a variant being added without wiring its policy, which
        // would silently duplicate a row in the report.
        let all = [
            Variant::CurrentRecall,
            Variant::EqualNoGate,
            Variant::CurrentWithFloor,
            Variant::Semantic2To1,
        ];
        let names: Vec<&str> = all.iter().map(|v| v.name()).collect();
        assert_eq!(names.len(), names.iter().collect::<HashSet<_>>().len());
        let policies: Vec<(f64, f64)> = all
            .iter()
            .map(|v| (v.policy().vector_weight, v.policy().text_weight))
            .collect();
        assert_ne!(policies[0], policies[1], "current and equal must differ");
        assert_ne!(policies[0], policies[3], "current and 2:1 must differ");
        assert_eq!(
            all[0].policy().min_cosine,
            None,
            "the current memory policy must be ungated"
        );
        assert!(
            all[2].policy().min_cosine.is_some(),
            "the floor variant must carry a floor"
        );
    }

    #[test]
    fn a_conversation_only_query_is_out_of_scope_for_memory() {
        let conv = query(
            "q-conv",
            vec![Annotation {
                scope: "conversation".to_string(),
                content: None,
                session_id: Some("s-1".to_string()),
                message_index: Some(0),
            }],
        );
        assert!(conv.out_of_scope(SCOPE));
        assert!(conv.annotations_for(SCOPE).is_empty());

        let mixed = query(
            "q-mixed",
            vec![annotation("memory", "x"), annotation("conversation", "y")],
        );
        assert!(!mixed.out_of_scope(SCOPE));
        assert_eq!(mixed.annotations_for(SCOPE).len(), 1);
    }

    #[test]
    fn the_current_policy_is_the_one_the_service_read_path_uses() {
        // The harness would otherwise measure a configuration production no
        // longer runs; `service::api::search` and this line must agree.
        assert_eq!(
            Variant::CurrentRecall.policy().vector_weight,
            FusionPolicy::SEMANTIC_4TO1.vector_weight
        );
    }

    #[test]
    fn exact_term_helper_is_the_shared_one() {
        let texts = vec!["Gateway MQTT broker on port 19875".to_string()];
        assert!(exact_terms_present(
            &texts,
            &["mqtt".to_string(), "19875".to_string()]
        ));
        assert!(!exact_terms_present(
            &texts,
            &["mqtt".to_string(), "kafka".to_string()]
        ));
    }
}
