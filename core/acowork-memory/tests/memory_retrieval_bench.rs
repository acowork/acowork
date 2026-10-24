//! Realistic-store retrieval benchmark with absolute quality gates.
//!
//! ## Why this file exists
//!
//! The two archived benchmarks (`memory_m4_bench.rs`, `memory_m5_bench.rs`)
//! are before/after comparisons for specific ADR-062 decisions. They are sound
//! for that purpose and are left alone. Neither can answer "is retrieval good",
//! for two reasons this file fixes:
//!
//! 1. **Their corpus has exactly one relevant node per query.** Under the
//!    standard P@k definition (denominator = k) the Precision@5 ceiling is
//!    therefore 1/5 = 0.20 *forever*, however good the retriever is. The
//!    archived report's 0.80 was only reachable because `precision_at_k`
//!    divided by `min(k, returned.len())` - a system returning a single
//!    correct item scored 1.0. That is now fixed, so the old number is not
//!    reproducible and should not be quoted.
//! 2. **They assert nothing absolute.** The precision gate is
//!    `after.p5 >= before.p5`, a self-consistency check. A benchmark that
//!    passes at any level is a number nobody is obliged to read.
//!
//! ## What "realistic" means here
//!
//! A store shaped like an agent's actual memory, built from the transcripts of
//! two live Ponytail instances and the real knowledge nodes in their stores:
//! several durable claims per topic, so a query has a *set* of relevant nodes
//! rather than a needle in a haystack; near-duplicate restatements of the same
//! fact, which is what a store accumulates across sessions; topically adjacent
//! distractors that share vocabulary but answer a different question (the cases
//! that actually put junk into context); superseded restatements sitting in
//! Dormant; and unrelated filler, so filling the slots is a real decision
//! rather than a freebie.
//!
//! ## Three modes, because one number cannot be both reproducible and complete
//!
//! * **lexical** - BM25 alone, the fallback the runtime takes with no embedding.
//!   Deterministic on any machine, so this is the mode carrying the primary gates.
//! * **hybrid + live model** - the production path, fused over HTTP with
//!   whichever model the embedding service currently has loaded (asked from its
//!   `/health`, not hardcoded, so a model swap is measured instead of missed).
//!   Skips rather than fails where no model service is listening.
//! * **hybrid + identity-hash vector** - `procedural_embedding_fallback`, a
//!   semantically null vector. Reported, and asserted never to beat the lexical
//!   tier: an arbitrary second source must not out-vote a correct first one.
//!
//! ## The finding this file records
//!
//! The variable that dominates the vector tier is the encoder's language match,
//! not the fusion weight. Measured on this corpus of English sediment:
//!
//! | vector source           | Recall@5 | MRR   | nDCG@5 |
//! |-------------------------|----------|-------|--------|
//! | none (BM25 alone)       | 0.729    | 0.903 | 0.740  |
//! | bge-small-zh-v1.5 (512) | 0.688    | 0.774 | -      |
//! | identity-hash fallback  | 0.493    | 0.549 | 0.411  |
//! | bge-m3 (1024)           | 0.854    | 0.896 | 0.801  |
//!
//! The zh encoder did subtract from BM25: sweeping the fusion weight gave MRR
//! 0.33 at vector 0.8 (`Semantic`, the manager's default hint), 0.59 at 0.5
//! (`Factual`), 0.70 at 0.3 and 0.90 with no vector at all, because its
//! similarities collapse into a 0.44-0.69 band on short English sentences - the
//! anisotropy `manager::retrieve` warns about - and RRF let that noise out-vote
//! the lexical tier's correct rank-1. Switching to the multilingual model
//! removes the collapse: the vector tier now adds 12.5 points of Recall@5 over
//! BM25 alone while giving back 0.007 of MRR.
//!
//! Two things survive the swap. A semantically null vector still loses to BM25
//! by a wide margin, which is the assertion that keeps the fallback honest. And
//! MRR is still marginally better with no vector source at all, so the vector
//! tier earns its place by finding answers BM25 misses, not by improving the
//! ones BM25 already ranks first.
//!
//! Run with `-- --nocapture` to see the per-query report and all three modes.

use std::collections::HashMap;
use std::sync::Arc;

use acowork_core::EmbeddingProvider;
use acowork_eval::{EvalQuery, evaluate_retrieval_quality};
use acowork_memory::{
    KnowledgeNode, KnowledgeSubType, MemoryManager, MemoryManagerConfig, MemoryProvider,
    MemoryQuery, NodeStatus, PrivacyLevel,
};
use chrono::Utc;

/// Deterministic hash embedding - deliberately semantically null, see header.
struct NullEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for NullEmbedding {
    fn name(&self) -> &str {
        "null-semantic-bench"
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, acowork_core::EmbeddingError> {
        Ok(acowork_memory::manager::procedural_embedding_fallback(text))
    }

    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, acowork_core::EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }

    fn dimension(&self) -> usize {
        384
    }

    async fn is_available(&self) -> bool {
        true
    }
}

/// `(id, content, importance, dormant)`.
///
/// `dormant` models a superseded restatement: the store keeps it for audit,
/// but it must never enter context (ADR-062 D1).
type Row = (&'static str, &'static str, f32, bool);

const CORPUS: &[Row] = &[
    // ---- topic: tauri release build --------------------------------------
    ("T1", "The Tauri release build runs npm run tauri build, which triggers core:build:release first", 0.9, false),
    ("T2", "Tauri bundling on Windows needs the WiX toolset installed for the MSI installer", 0.85, false),
    ("T3", "The tauri.conf.json beforeDevCommand starts the Vite dev server on port 1420", 0.8, false),
    ("T4", "Release bundles for the desktop app are written to target/release/bundle", 0.8, false),
    ("T5", "Building the desktop app requires eight core binaries to exist before tauri build", 0.85, false),
    // ---- topic: flaky cargo tests ----------------------------------------
    ("F1", "todo_write_roundtrip in acowork-runtime is flaky and passes when run alone", 0.8, false),
    ("F2", "mqtt_e2e_full fails under parallel test execution but is green single-threaded", 0.8, false),
    ("F3", "Flaky tests in this repo are shared-state races between concurrently running tokio runtimes", 0.75, false),
    ("F4", "cargo test output is truncated in the middle, so filter with grep before reading", 0.7, false),
    // ---- topic: memory consolidation -------------------------------------
    ("C1", "Memory consolidation projects an episode into a semantic node when nothing similar exists", 0.9, false),
    ("C2", "The distiller calls the model only to decide merge, no_merge or contradicts between candidates", 0.9, false),
    ("C3", "min_importance is the single volume knob for the semantic layer, defaulting to project everything", 0.85, false),
    ("C4", "Episodes deferred for want of a model stay unconsolidated and are reconsidered every run", 0.8, false),
    ("C5", "Episode::statement() is the only place the normalized-to-content fallback lives", 0.85, false),
    // ---- topic: user working style ---------------------------------------
    ("P1", "The user wants decisions made and carried through without being asked to confirm each step", 0.9, false),
    ("P2", "The user prefers a plan document before implementation starts on a refactor", 0.8, false),
    ("P3", "The user reads diffs by asking what changed and why, not by reading every line", 0.75, false),
    ("P4", "The user writes and expects replies in Chinese on this project", 0.8, false),
    ("P5", "The user dislikes being handed a menu of options when the direction is already clear", 0.85, false),
    // ---- topic: windows / shell environment ------------------------------
    ("W1", "Git Bash /tmp maps to the AppData Temp folder, not to a literal /tmp", 0.85, false),
    ("W2", "PowerShell 7 is available as a fallback when Git Bash is missing from the machine", 0.7, false),
    ("W3", "Windows absolute paths must use the C:/ form inside bash commands on this machine", 0.8, false),
    // ---- topic: sqlite storage layer -------------------------------------
    ("S1", "The memory store keeps embeddings in a separate vectors table keyed by node_id", 0.85, false),
    ("S2", "Forgetting archives a node into purge_log before deleting it, so it stays recoverable", 0.9, false),
    ("S3", "The decay scan selects only rows labelled Episodic, never semantic nodes", 0.9, false),
    ("S4", "Production stores record embedding_dim 512 in the meta table rather than the code default", 0.8, false),
    // ---- topic: commit / branch conventions -------------------------------
    ("B1", "Rust code comments in this repo are English even though design docs are bilingual", 0.85, false),
    ("B2", "The repo forbids committing Chinese prose into source files", 0.8, false),
    ("B3", "Work lands on feature/multi-user and each milestone is committed separately", 0.75, false),
    ("B4", "Commit messages explain why the change was made, not which files were touched", 0.8, false),
    // ---- restatements: the same fact stored again by a later session -----
    //
    // A real store is not one node per fact. Consolidation folds restatements
    // inside a run, but across runs a differently-worded episode that the model
    // answers `no_merge` to becomes its own node, and an LLM writing a memory
    // statement names its own topic ("reply language", "recovery", "512
    // dimensions"). Omitting those would flatter the retriever: it is the
    // near-duplicate density of a live store that makes recall hard, and also
    // what makes it work.
    ("P6", "Reply language for this user is Chinese (zh-CN); English is for code and commits only", 0.8, false),
    ("S5", "Forgotten episodes are archived to purge_log rather than erased, so recovery is possible", 0.9, false),
    ("S6", "Embedding vectors in the memory store are 512 dimensions, not the code default of 384", 0.8, false),
    ("W4", "In Git Bash the temp directory resolves to AppData Local Temp, not to a real /tmp mount", 0.85, false),
    ("B5", "The current working branch for this project is feature/multi-user", 0.75, false),
    ("C6", "Nothing is sedimented while the consolidation switch is off, regardless of backlog size", 0.85, false),
    // ---- vocabulary-sharing distractors (relevant to nothing) -------------
    ("X1", "The Tauri system tray icon is rendered from icons/tray-icon.png", 0.7, false),
    ("X2", "The desktop app shows a build indicator while the release bundle is compiling", 0.6, false),
    ("X3", "Cargo clippy runs with -D warnings across the workspace in dev/ci.sh", 0.7, false),
    ("X4", "The user editor theme is a dark colour scheme for the terminal, not the IDE", 0.6, false),
    ("X5", "The memory panel is a collapsible card in the right sidebar", 0.6, false),
    ("X6", "SQLite WAL mode is enabled for the conversation index", 0.6, false),
    ("X7", "The gateway listens on HTTP port 19876 and MQTT on 19875", 0.65, false),
    ("X8", "Release notes are generated from commit subjects on the develop branch", 0.55, false),
    // ---- superseded restatements (Dormant; must never reach context) -----
    ("D1", "The distiller uses clustering then a judge to promote memories, superseded", 0.2, true),
    ("D2", "The user wanted to be asked before each step, superseded by later instruction", 0.2, true),
    ("D3", "Forgetting deletes episodes immediately with no grace period, superseded", 0.2, true),
    // ---- unrelated filler -------------------------------------------------
    ("U1", "The doc service exposes a REST API under /v1/docs", 0.5, false),
    ("U2", "Package signing uses ed25519 keys stored in the vault", 0.5, false),
    ("U3", "The embedding model is downloaded on first use into the gateway data directory", 0.5, false),
    ("U4", "LSP relaying bridges the desktop client to external language servers", 0.5, false),
    ("U5", "The node agent supervises runtime processes over MQTT on the control plane topic", 0.5, false),
    ("U6", "Multi-user accounts were split into the acowork-user service", 0.5, false),
];

const DORMANT_IDS: &[&str] = &["D1", "D2", "D3"];

// ============================================================================
// Queries + ground truth
// ============================================================================

/// `(query, relevant ids)`.
///
/// Relevance rule, applied to content before any retrieval ran: **a node is
/// relevant if acting on its claim would inform or change the answer.** The
/// `X*` distractors deliberately share topic vocabulary but describe a
/// different thing (a tray icon is not the release build; clippy is not a
/// flaky test), so they stay out of the key even though they are the hardest
/// misses.
/// One question per intent, with the nodes that *answer* it.
///
/// Relevance here means "this statement resolves the question", not "this
/// statement is topically nearby". Marking adjacent nodes as relevant is what
/// made the earlier cut of this benchmark unpassable: it pushed the Precision@5
/// ceiling below the gate before a single document was retrieved, and it let a
/// retriever score well by surfacing the wrong fact about the right subject.
/// A node that is superseded (`D*`) or unrelated filler (`X*`, `U*`) is never
/// relevant, which is the whole point of having them in the corpus.
const QUERIES: &[(&str, &[&str])] = &[
    ("how do I produce the windows installer for the desktop app", &["T1", "T2", "T4", "T5"]),
    ("the vite dev server port for tauri dev", &["T3"]),
    ("these two tests keep failing in the full run but pass on their own", &["F1", "F2", "F3"]),
    ("why does consolidation produce no memories when the switch is on", &["C1", "C3", "C4", "C6"]),
    ("how do I stop the semantic layer filling up", &["C3", "C1"]),
    ("should I ask before making the next decision", &["P1", "P5"]),
    ("which language should I reply in", &["P4", "P6"]),
    ("where does the temp directory actually point in git bash", &["W1", "W4"]),
    ("can a forgotten memory be recovered", &["S2", "S5"]),
    ("what dimension are the stored vectors", &["S4", "S6"]),
    ("is it fine to write the commit message in chinese", &["B2", "B1"]),
    ("which branch does this work land on", &["B3", "B5"]),
];


// ============================================================================
// Harness
// ============================================================================

/// The hint every benchmark query is run under, and therefore the text/vector
/// balance the RRF fusion uses (see `manager::hint_weights`).
///
/// `Factual` (0.5 text / 0.5 vector) is the semantically correct hint for these
/// queries - each asks for one stored fact - and it is *not* the best-scoring
/// one. Sweeping the weight on this corpus gives MRR 0.33 at vector 0.8
/// (`Semantic`, the manager default), 0.59 at 0.5, 0.70 at 0.3 and 0.90 with no
/// vector source at all. Picking 0.3 here would flatter the benchmark and hide
/// that finding.
const BENCH_HINT: acowork_memory::types::HintType = acowork_memory::types::HintType::Factual;

/// Which vector source `MemoryManager` sees for this run.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Mode {
    /// No query embedding at all: the manager runs BM25 alone. This is the
    /// degraded path the runtime takes when no embedding is available.
    Lexical,
    /// A deterministic identity-hash vector. Its ranking is arbitrary, so the
    /// run measures how RRF fusion reacts to a *semantically null* second
    /// source - a property of the fusion, not of retrieval quality.
    NullVector,
    /// The live embedding service: whichever model the desktop app currently
    /// has loaded, at the dimension it reports. This is the path production
    /// actually runs, and the only one whose score describes real retrieval
    /// quality.
    LiveVector,
}

/// Live-mode request timeout. Long enough for a cold model to answer one
/// probe, short enough that an absent service does not stall the run.
const LIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The service the desktop app keeps running. Unreachable on a machine with no
/// model downloaded, which is why the live run skips rather than fails.
const EMBED_BASE_URL: &str = "http://127.0.0.1:18080";

type Live = acowork_memory::service::embedding::HttpEmbeddingClient;

/// Which model is actually loaded, asked of the service itself.
///
/// Not a constant: switching the embedding model is precisely the event this
/// benchmark exists to measure, and a hardcoded id would keep scoring the old
/// model after the swap. `/health` reports the loaded model and its dimension.
async fn loaded_model() -> Option<(String, usize)> {
    let v: serde_json::Value = reqwest::get(format!("{EMBED_BASE_URL}/health"))
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    if v.get("status")?.as_str()? != "ready" {
        return None;
    }
    Some((
        v["model"]["id"].as_str()?.to_string(),
        v["model"]["dimension"].as_u64()? as usize,
    ))
}

struct LiveService {
    provider: Arc<Live>,
    model: String,
    dim: usize,
}

async fn live_embedder() -> Option<LiveService> {
    let (model, dim) = loaded_model().await?;
    let provider =
        Live::new(&format!("{EMBED_BASE_URL}/v1"), None, &model, dim, LIVE_TIMEOUT).ok()?;
    // One short probe: an absent or cold service must not turn into a wall of
    // connection errors across every corpus document.
    match provider.embed("ping").await {
        Ok(v) if v.len() == dim => Some(LiveService {
            provider: Arc::new(provider),
            model,
            dim,
        }),
        _ => None,
    }
}

struct Bench {
    store: Arc<acowork_memory::sqlite::SqliteStore>,
    ids: HashMap<&'static str, u64>,
    mode: Mode,
    live: Option<LiveService>,
}

impl Bench {
    async fn seed(mode: Mode) -> Self {
        let live = if mode == Mode::LiveVector {
            live_embedder().await
        } else {
            None
        };
        let dim = match &live {
            Some(svc) => svc.dim,
            None => acowork_memory::types::DEFAULT_EMBEDDING_DIM,
        };
        let store = Arc::new(
            acowork_memory::sqlite::SqliteStore::open_in_memory(dim).expect("in-memory store"),
        );
        let mut ids = HashMap::new();
        for (key, content, importance, dormant) in CORPUS {
            let embedding = match &live {
                Some(svc) => svc.provider.embed(content).await.expect("embed corpus node"),
                None => NullEmbedding.embed(content).await.expect("embed"),
            };
            let node = KnowledgeNode {
                subject: "user".to_string(),
                predicate: String::new(),
                object: content.to_string(),
                sub_type: KnowledgeSubType::Fact,
                confidence: 0.85,
                source_episode_id: None,
                source_episode_ids: Vec::new(),
                promotion_metadata: None,
                embedding: Some(embedding),
                status: if *dormant {
                    NodeStatus::Dormant
                } else {
                    NodeStatus::Active
                },
                created_at: Utc::now(),
                updated_at: Utc::now(),
                metadata: HashMap::new(),
                privacy: PrivacyLevel::Personal,
                importance: *importance,
            };
            let id = MemoryProvider::store_knowledge(store.as_ref(), &node).expect("seed node");
            ids.insert(*key, id);
        }
        Self {
            store,
            ids,
            mode,
            live,
        }
    }

    async fn retrieve(&self, query_text: &str) -> Vec<u64> {
        let manager = MemoryManager::new(MemoryManagerConfig::default());
        let mut q = MemoryQuery::new(query_text.to_string());
        q.limit = 10;
        // Every question in QUERIES asks for one specific stored fact, so this
        // is a `Factual` recall (0.5 text / 0.5 vector). The `Semantic` default
        // leans 0.8 on the vector source, which is the right trade for loose
        // association and the wrong one here.
        q.hint_type = BENCH_HINT;
        // The manager auto-embeds the query from whatever provider it is handed;
        // `Lexical` is the one mode where it is handed nothing.
        let embedding: Option<&dyn EmbeddingProvider> = match self.mode {
            Mode::Lexical => None,
            Mode::NullVector => Some(&NullEmbedding),
            Mode::LiveVector => self
                .live
                .as_ref()
                .map(|svc| svc.provider.as_ref() as &dyn EmbeddingProvider),
        };
        manager
            .retrieve(&*self.store, &mut q, embedding)
            .await
            .expect("retrieve")
            .memories
            .iter()
            .map(|m| m.node_id)
            .collect()
    }
}

// ============================================================================
// Gates
// ============================================================================

/// Gates are stated on Recall@5, MRR and nDCG@5 - never on Precision@5.
///
/// Precision@5 has a hard ceiling of `min(|relevant|, 5) / 5` per query. This
/// corpus answers each question with one to four statements - that is what makes
/// it realistic, a user asks one thing rather than five - so the mean Precision@5
/// ceiling across the twelve queries is 22/60 = 0.367. A Precision@5 gate above
/// that measures the size of the ground truth, not the retriever. The archived
/// ADR-062 report could show 0.80 only because its denominator was
/// `min(k, returned.len())`; see `acowork_eval::ndcg_at_k`.
const MIN_RECALL_AT_5: f32 = 0.70;
const MIN_MRR: f32 = 0.85;
const MIN_NDCG_AT_5: f32 = 0.70;

/// Regression gates for the live-model hybrid run, set just under what the
/// configured model measures (Recall@5 0.854, MRR 0.896 with bge-m3).
///
/// They were 0.60 / 0.50 when the configured model was bge-small-zh-v1.5, which
/// scored below BM25 alone; raising them to sit above the lexical run's
/// Recall@5 (0.729) is what makes the gate say something - the vector tier must
/// now earn its place, and a swap to an encoder that does not earn it fails
/// here rather than in someone's context window.
const MIN_LIVE_RECALL_AT_5: f32 = 0.75;
const MIN_LIVE_MRR: f32 = 0.80;

/// One benchmark run: retrieve every query, score it, keep what the report needs.
struct Run {
    p5: f32,
    r5: f32,
    p10: f32,
    r10: f32,
    mrr: f32,
    ndcg5: f32,
    dormant_in_context: usize,
    total_returned: usize,
    per_query: Vec<Vec<u64>>,
}

async fn score(bench: &Bench) -> Run {
    let eval: Vec<EvalQuery> = QUERIES
        .iter()
        .map(|(text, keys)| EvalQuery {
            query: text.to_string(),
            relevant_ids: keys
                .iter()
                .map(|k| *bench.ids.get(k).expect("ground truth key in corpus"))
                .collect(),
        })
        .collect();

    let mut per_query = Vec::with_capacity(QUERIES.len());
    for (text, _) in QUERIES {
        per_query.push(bench.retrieve(text).await);
    }
    let dormant: Vec<u64> = DORMANT_IDS.iter().map(|k| bench.ids[*k]).collect();
    let dormant_in_context: usize = per_query
        .iter()
        .map(|ids| ids.iter().filter(|i| dormant.contains(i)).count())
        .sum();
    let total_returned: usize = per_query.iter().map(|v| v.len()).sum();
    let metrics = evaluate_retrieval_quality(&eval, &per_query, &[5, 10]);
    let at = |v: &[(usize, f64)], k: usize| v.iter().find(|(i, _)| *i == k).map(|(_, x)| (*x) as f32).unwrap_or(0.0);
    Run {
        p5: at(&metrics.precision_at_k, 5),
        r5: at(&metrics.recall_at_k, 5),
        p10: at(&metrics.precision_at_k, 10),
        r10: at(&metrics.recall_at_k, 10),
        ndcg5: at(&metrics.ndcg_at_k, 5),
        mrr: metrics.mrr as f32,
        dormant_in_context,
        total_returned,
        per_query,
    }
}

fn report(bench: &Bench, mode: &str, run: &Run) {
    println!("\n-- {mode} --");
    println!(
        "avg returned      : {:.1} per query",
        run.total_returned as f32 / QUERIES.len() as f32
    );
    println!("Recall@5          : {:.4}", run.r5);
    println!("MRR               : {:.4}", run.mrr);
    println!("nDCG@5            : {:.4}", run.ndcg5);
    println!("Precision@5       : {:.4}  (ceiling 0.367 - see the gates note)", run.p5);
    println!("Recall@10         : {:.4}", run.r10);
    println!("Precision@10      : {:.4}", run.p10);
    println!("dormant in context: {}", run.dormant_in_context);

    for ((text, keys), ids) in QUERIES.iter().zip(&run.per_query) {
        let truth: Vec<u64> = keys.iter().map(|k| bench.ids[*k]).collect();
        let first = ids.iter().position(|i| truth.contains(i)).map(|p| p + 1);
        let hit5 = ids[..5.min(ids.len())]
            .iter()
            .filter(|i| truth.contains(i))
            .count();
        println!(
            "  {:<56} truth={} hit@5={} first_rank={:?}",
            text.chars().take(56).collect::<String>(),
            truth.len(),
            hit5,
            first
        );
    }
}

fn banner() {
    println!("===== realistic-store retrieval benchmark =====");
    println!(
        "corpus : {} nodes ({} dormant; distractors and filler taken from the live store)",
        CORPUS.len(),
        DORMANT_IDS.len()
    );
    println!("queries: {} (each with the statements that ANSWER it, not merely the ones nearby)", QUERIES.len());
}

// ============================================================================
// Tests
// ============================================================================

/// The production path: BM25 fused with the real sentence encoder.
///
/// Skips (does not fail) when no embedding service is listening, because the
/// model is downloaded on first use and a bare CI machine has no reason to
/// carry it.
#[tokio::test]
async fn hybrid_with_the_live_embedding_model_does_not_regress() {
    let bench = Bench::seed(Mode::LiveVector).await;
    if bench.live.is_none() {
        println!(
            "SKIP: no embedding service at {EMBED_BASE_URL} - start acowork-embed \
             (or open the desktop app) to run the gated benchmark."
        );
        return;
    }
    assert_eq!(bench.ids.len(), CORPUS.len(), "every corpus row got an id");
    banner();
    let run = score(&bench).await;
    let label = format!(
        "hybrid (BM25 + {}, {}-dim, RRF fused)",
        bench.live.as_ref().unwrap().model,
        bench.live.as_ref().unwrap().dim
    );
    report(&bench, &label, &run);

    assert_eq!(
        run.dormant_in_context, 0,
        "ADR-062 D1: a superseded (Dormant) node entered context"
    );
    assert!(
        run.r5 >= MIN_LIVE_RECALL_AT_5,
        "Recall@5 {:.4} below gate {MIN_LIVE_RECALL_AT_5}",
        run.r5
    );
    assert!(
        run.mrr >= MIN_LIVE_MRR,
        "MRR {:.4} below gate {MIN_LIVE_MRR}",
        run.mrr
    );
}

/// The deterministic tier, carrying the primary gates: BM25 alone, plus what
/// fusing a semantically null vector does to it.
#[tokio::test]
async fn retrieval_fallback_and_null_vector_are_measured() {
    let bench = Bench::seed(Mode::Lexical).await;
    banner();

    let lexical = score(&bench).await;
    report(&bench, "lexical only (BM25, the no-embedding fallback)", &lexical);
    assert_eq!(
        lexical.dormant_in_context, 0,
        "ADR-062 D1: a superseded (Dormant) node entered context"
    );
    assert!(
        lexical.r5 >= MIN_RECALL_AT_5,
        "fallback Recall@5 {:.4} below gate {MIN_RECALL_AT_5}",
        lexical.r5
    );
    assert!(
        lexical.mrr >= MIN_MRR,
        "MRR {:.4} below gate {MIN_MRR}",
        lexical.mrr
    );
    assert!(
        lexical.ndcg5 >= MIN_NDCG_AT_5,
        "nDCG@5 {:.4} below gate {MIN_NDCG_AT_5}",
        lexical.ndcg5
    );

    let null = score(&Bench::seed(Mode::NullVector).await).await;
    report(&bench, "hybrid (BM25 + identity-hash vector, RRF fused)", &null);
    assert_eq!(null.dormant_in_context, 0, "ADR-062 D1 holds in every mode");
    assert!(
        null.mrr <= lexical.mrr + 1e-6,
        "fusing an arbitrary vector source must not out-rank having no vector \
         source at all: null {:.4} vs lexical {:.4}",
        null.mrr,
        lexical.mrr
    );
}
