//! End-to-end verification of the ADR-095 global-search retrieval path.
//!
//! ## Why this file exists
//!
//! The unit tests added with ADR-095 prove each layer behaves in isolation, and
//! `retrieval_eval.rs` proves the *fusion arithmetic* is sound — but it does so
//! with a deterministic hashed-token stand-in for the embedding model, so its
//! numbers cannot answer the questions ADR-095 §9 actually asks (is 4:1 right?
//! is a cosine floor safe to enable?). Those are only answerable against the
//! real model.
//!
//! Two tiers, because no single tier covers both things that matter:
//!
//! * **Tier 1 — real HTTP, real SQLite, real FTS.** A live `RuntimeHttpServer`
//!   on an ephemeral port answers `GET /search` against a real store. Verifies
//!   the *wire contract* (payload shape, `matched_by` vocabulary, rank ordering,
//!   no raw-score leakage) and that the `analyzer_mode` config switch changes
//!   behavior end to end. Lexical-only by construction: `AgentCore`'s embedding
//!   provider is `pub(crate)`, so an integration test cannot inject one and
//!   `/search` takes its documented `embed_query` fallback. That is not a hole
//!   for the analyzer's own coverage — the analyzer emits a *lexical* plan, so
//!   the lexical source is precisely where it acts.
//! * **Tier 2 — the production embedding model.** Drives the real retrieval
//!   stack (`MemoryAdminAdapter::semantic_search_detailed` +
//!   `ConversationIndex::search_detailed` over one real `SqliteStore`) with
//!   whichever model the local embedding service currently has loaded, asked
//!   from `/health` rather than hardcoded, so a model swap is measured instead
//!   of silently missed. This produces the metrics §9 needs. It **skips rather
//!   than fails** when no embedding service is listening: the model is a
//!   developer-machine dependency, and a red suite without it would train
//!   everyone to ignore this file.
//!
//! ## What this file deliberately does not do
//!
//! It asserts no quality *threshold*. ADR-095 D6 requires the gate to be set
//! from measured data and §9 leaves the number open for review; encoding a
//! guessed number here would launder an undecided policy into a passing test.
//! Tier 2 prints and writes the comparison for a human to decide. The
//! structural invariants (Tier 1) *are* asserted — those are already decided.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acowork_core::retrieval::{FusionPolicy, LexicalQuery};
use acowork_memory::types::{KnowledgeNode, KnowledgeSubType, NodeStatus, PrivacyLevel};
use acowork_runtime::conversation_index::ConversationIndex;
use acowork_runtime::embedding::remote::RemoteEmbeddingProvider;
use acowork_runtime::embedding::{EmbeddingProvider, SharedEmbedProvider};
use acowork_runtime::http::server::RuntimeHttpServer;
use acowork_runtime::http::{SharedConversationIndex, SharedMemoryStore};
use acowork_runtime::search::analyzer;
use acowork_runtime::usecases::agent_config::AgentConfigService;
use acowork_runtime::usecases::agent_config_impl::RuntimeAgentConfigService;
use acowork_runtime::usecases::memory_query::{MemoryQueryService, SemanticMemoryQuery};
use acowork_runtime::usecases::memory_query_impl::MemoryAdminAdapter;
use chrono::Utc;

const AGENT_ID: &str = "com.test.globalsearch";
const INSTANCE_ID: &str = "00000000-0000-4000-8000-000000000001";

/// The desktop's loopback embedding proxy. Deliberately probed, not assumed:
/// whatever model is loaded *is* the model under test.
const EMBED_BASE_URL: &str = "http://127.0.0.1:18080";

const K: usize = 5;

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// One indexed document with a stable tag, so queries can label relevance
/// without depending on store-assigned ids (which vary with seeding order).
/// Content is the join key back to the tag, so every content string must be
/// unique across both corpora.
#[derive(Clone)]
struct Doc {
    tag: &'static str,
    content: &'static str,
}

fn memory_corpus() -> Vec<Doc> {
    vec![
        Doc { tag: "M-GW-1", content: "The Gateway listens on HTTP port 19876 and MQTT port 19875 on the loopback interface." },
        Doc { tag: "M-GW-2", content: "Gateway 进程负责包管理、生命周期管理和反向代理,它是唯一的常驻进程。" },
        Doc { tag: "M-GW-3", content: "Agent-private data must be read through Runtime HTTP, never by direct filesystem access from the Gateway." },
        Doc { tag: "M-NODE-1", content: "acowork-node is a per-machine daemon that spawns and reaps Runtime processes and owns the LSP server registry." },
        Doc { tag: "M-NODE-2", content: "Node Agent 通过 MQTT 控制平面 acowork/nodes/# 注册自己,并暴露反向代理端口 19900。" },
        Doc { tag: "M-MEM-1", content: "Hybrid retrieval fuses BM25 and vector ranks with weighted RRF, k=60." },
        Doc { tag: "M-MEM-2", content: "记忆检索的 cosine 门控只作用于向量路,词法命中不会因为向量距离远而被丢弃。" },
        Doc { tag: "M-MEM-3", content: "RRF encodes rank position only and is not comparable across queries, so never threshold on its absolute value." },
        Doc { tag: "M-SQL-1", content: "The SQLite backend stores memory nodes, session metadata and the conversation index in one file per agent workspace." },
        Doc { tag: "M-SQL-2", content: "private.sqlite 位于 workspace/memory 目录,对话索引和记忆共用同一个文件。" },
        Doc { tag: "M-ADR-1", content: "ADR-095 changes how global search retrieves conversation and memory results and how it selects summary candidates." },
        Doc { tag: "M-ADR-2", content: "ADR-093 defines the bounded summary candidate budget for global search." },
        Doc { tag: "M-ADR-3", content: "ADR-082 established per-source gating and rank fusion for memory recall." },
        Doc { tag: "M-DESKTOP-1", content: "The desktop app is presentation only: no business logic and no state persistence." },
        Doc { tag: "M-DESKTOP-2", content: "Tauri dev server for mobile runs on port 19877." },
        Doc { tag: "M-EMBED-1", content: "The embedding model is downloaded on first use into the gateway data directory." },
        Doc { tag: "M-EMBED-2", content: "bge-m3 produces 1024-dimensional L2-normalized embeddings and is served by acowork-embed over OpenAI-compatible HTTP." },
        Doc { tag: "M-UNREL-1", content: "Pasta water should be salted generously and the starch never rinsed off before adding sauce." },
        Doc { tag: "M-UNREL-2", content: "周一的例行站会安排在上午十点,由轮值主持。" },
        Doc { tag: "M-UNREL-3", content: "The espresso machine descaling cycle runs every sixty hours of operation." },
    ]
}

/// Conversation-side corpus: message-shaped text, shorter and chattier than
/// durable claims — which is exactly why ADR-095 §9 item 3 asks whether memory
/// weights transfer here.
fn conversation_corpus() -> Vec<Doc> {
    vec![
        Doc { tag: "C-GW-1", content: "we should move the port to 19876 so it does not clash with the old gateway" },
        Doc { tag: "C-GW-2", content: "gateway 那边 19875 是 MQTT,不要拿去当 HTTP 端口用" },
        Doc { tag: "C-GW-3", content: "the reverse proxy in gateway forwards to the runtime localhost http port" },
        Doc { tag: "C-NODE-1", content: "node agent kills orphaned runtime processes on startup, that was the flaky restart bug" },
        Doc { tag: "C-NODE-2", content: "lsp registry 现在在 node 这边,relay 只是读注册表做 probe,已经无状态了" },
        Doc { tag: "C-MEM-1", content: "the fusion is rrf over two ranked lists, weights are per source not probabilities" },
        Doc { tag: "C-MEM-2", content: "别用 cosine 阈值去砍词法命中,那样精确实体就搜不到了" },
        Doc { tag: "C-SQL-1", content: "sqlite file per workspace holds memory plus session meta plus conversation rows" },
        Doc { tag: "C-SQL-2", content: "private.sqlite 里 embedding_dim 记的是 1024,不是代码默认值" },
        Doc { tag: "C-ADR-1", content: "ADR-095 之后 global search 的 memory 和 conversation 走同一个混合检索引擎" },
        Doc { tag: "C-ADR-2", content: "summary candidates are a bounded post-process, not everything stuffed at the model" },
        Doc { tag: "C-EMBED-1", content: "embed service serves bge-m3 on 18080 with an OpenAI compatible embeddings endpoint" },
        Doc { tag: "C-EMBED-2", content: "first run downloads the model into the gateway data dir, that is why cold start is slow" },
        Doc { tag: "C-DESKTOP-1", content: "desktop must not persist state, it re-reads everything from gateway on boot" },
        Doc { tag: "C-UNREL-1", content: "lunch at the place on the third floor is fine but the queue is long" },
        Doc { tag: "C-UNREL-2", content: "我把会议室订在下午两点,密码在日历邀请里" },
        Doc { tag: "C-UNREL-3", content: "the office wifi guest network rotates its password every quarter" },
    ]
}

/// A query with a labeled relevant set. An empty `expect` means "nothing here
/// is relevant" — the false-recall probe.
struct Query {
    id: &'static str,
    text: &'static str,
    expect: &'static [&'static str],
    intent: &'static str,
}

fn queries() -> Vec<Query> {
    vec![
        Query { id: "q-exact-entity", text: "bge-m3", expect: &["M-EMBED-2", "C-EMBED-1"], intent: "exact_entity" },
        Query { id: "q-exact-id", text: "ADR-095", expect: &["M-ADR-1", "C-ADR-1"], intent: "exact_entity" },
        Query { id: "q-port", text: "19876", expect: &["M-GW-1", "C-GW-1"], intent: "exact_entity" },
        Query { id: "q-sqlite-file", text: "private.sqlite", expect: &["M-SQL-2", "C-SQL-2"], intent: "exact_entity" },
        Query { id: "q-zh-keyword", text: "词法命中 向量 阈值", expect: &["M-MEM-2", "C-MEM-2"], intent: "short_keyword_zh" },
        Query { id: "q-en-keyword", text: "gateway reverse proxy port", expect: &["M-GW-1", "M-GW-2", "C-GW-3"], intent: "short_keyword_en" },
        Query { id: "q-full-sentence", text: "How does the system combine the two ranked sources into one result order?", expect: &["M-MEM-1", "M-MEM-3", "C-MEM-1"], intent: "full_sentence" },
        Query { id: "q-semantics-only", text: "why is the first launch of the desktop application so slow", expect: &["M-EMBED-1", "C-EMBED-2"], intent: "semantics_only" },
        Query { id: "q-mixed", text: "node agent 的 LSP registry 在哪里", expect: &["M-NODE-1", "M-NODE-2", "C-NODE-2"], intent: "mixed_lang" },
        Query { id: "q-no-relevant", text: "how do I make a sourdough starter", expect: &[], intent: "no_relevant" },
        Query { id: "q-no-relevant-2", text: "最佳旅游路线 欧洲 暑假", expect: &[], intent: "no_relevant" },
    ]
}

/// Content → tag, so ranked results can be resolved back to labels.
fn tag_index() -> HashMap<String, String> {
    let mut m = HashMap::new();
    for d in memory_corpus().into_iter().chain(conversation_corpus()) {
        m.insert(d.content.to_string(), d.tag.to_string());
        // Memory snippets are rendered from the stored triple by
        // `provider::render_content`, so the wire carries "user knows <content>"
        // rather than the raw content. Both must resolve to the same tag.
        m.insert(format!("user knows {}", d.content), d.tag.to_string());
    }
    // Uniqueness is on the raw content (each doc's own text), not on the map
    // size, which also holds the rendered-triple alias above.
    let raw: HashSet<&str> = memory_corpus()
        .iter()
        .chain(conversation_corpus().iter())
        .map(|d| d.content)
        .collect();
    assert_eq!(
        raw.len(),
        memory_corpus().len() + conversation_corpus().len(),
        "corpus content must be unique, or tag resolution is ambiguous"
    );
    m
}

// ---------------------------------------------------------------------------
// Metrics (strict definitions)
// ---------------------------------------------------------------------------

/// Precision@k divides by `k`, never by the number returned, so a retriever
/// cannot inflate it by returning less. The archived m4/m5 benchmarks divided by
/// `min(k, returned.len())` and are not comparable — see
/// `memory_retrieval_bench.rs`, which fixed this.
fn recall_at_k(returned: &[String], expect: &HashSet<String>, k: usize) -> f64 {
    let hits = returned.iter().take(k).filter(|t| expect.contains(*t)).count();
    hits as f64 / expect.len() as f64
}

fn precision_at_k(returned: &[String], expect: &HashSet<String>, k: usize) -> f64 {
    let hits = returned.iter().take(k).filter(|t| expect.contains(*t)).count();
    hits as f64 / k as f64
}

fn reciprocal_rank(returned: &[String], expect: &HashSet<String>) -> f64 {
    match returned.iter().position(|t| expect.contains(t)) {
        Some(pos) => 1.0 / (pos as f64 + 1.0),
        None => 0.0,
    }
}

fn ndcg_at_k(returned: &[String], expect: &HashSet<String>, k: usize) -> f64 {
    // Canonical discount: position `i` (0-based) is ranked `i + 1`, so the
    // divisor is log2(i + 2). Using log2(i + 1) divides by zero at the first
    // rank, which produced inf/inf = NaN and made every nDCG read as absent.
    let gain = |rel: bool, rank: usize| -> f64 {
        if rel {
            1.0 / (rank as f64 + 2.0).log2()
        } else {
            0.0
        }
    };
    let dcg: f64 = returned
        .iter()
        .take(k)
        .enumerate()
        .map(|(rank, tag)| gain(expect.contains(tag), rank))
        .sum();
    let idcg: f64 = (0..expect.len()).map(|rank| gain(true, rank)).sum();
    if idcg == 0.0 {
        f64::NAN
    } else {
        dcg / idcg
    }
}

/// Average ignoring NAN, so an undefined-per-query metric is excluded rather
/// than silently scored as a miss.
fn mean(values: &[f64]) -> f64 {
    let usable: Vec<f64> = values.iter().copied().filter(|v| !v.is_nan()).collect();
    if usable.is_empty() {
        return f64::NAN;
    }
    usable.iter().sum::<f64>() / usable.len() as f64
}

fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((v.len() - 1) as f64 * q).round() as usize;
    v[idx]
}

// ---------------------------------------------------------------------------
// Shared stack
// ---------------------------------------------------------------------------

/// A real store plus the two services that read it.
struct Stack {
    work_dir: PathBuf,
    store: Arc<acowork_sqlite::SqliteStore>,
    index: Arc<ConversationIndex>,
    memory: Arc<MemoryAdminAdapter>,
    tags: HashMap<String, String>,
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.work_dir);
    }
}

fn open_stack(work_dir: &Path, dim: usize) -> (Arc<acowork_sqlite::SqliteStore>, Arc<ConversationIndex>) {
    std::fs::create_dir_all(work_dir.join("memory")).unwrap();
    std::fs::create_dir_all(work_dir.join("conversations")).unwrap();
    std::fs::create_dir_all(work_dir.join("config")).unwrap();
    let db = work_dir.join("memory").join("private.sqlite");
    let store = Arc::new(acowork_sqlite::SqliteStore::open(&db, dim).expect("open store"));
    let index = Arc::new(ConversationIndex::from_store(store.clone(), work_dir).expect("conversation index"));
    (store, index)
}

fn knowledge_node(content: &str, embedding: Vec<f32>) -> KnowledgeNode {
    KnowledgeNode {
        subject: "user".to_string(),
        // Non-empty on purpose: `render_content` only reconstructs a Knowledge
        // snippet from a complete triple, and every live Knowledge row carries a
        // predicate (checked against 195 rows in 11 real stores). Seeding an
        // empty one produced blank snippets in this harness and made it look
        // like the product had a rendering bug.
        predicate: "knows".to_string(),
        object: content.to_string(),
        sub_type: KnowledgeSubType::Fact,
        confidence: 0.9,
        source_episode_id: None,
        source_episode_ids: Vec::new(),
        promotion_metadata: None,
        embedding: Some(embedding),
        status: NodeStatus::Active,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        metadata: HashMap::new(),
        privacy: PrivacyLevel::Public,
        importance: 0.7,
    }
}

fn memory_slot(store: Arc<acowork_sqlite::SqliteStore>) -> SharedMemoryStore {
    Arc::new(std::sync::RwLock::new(Some(
        store as Arc<dyn acowork_memory::admin::MemoryAdminService>
    )))
}

fn embed_slot(provider: Arc<dyn EmbeddingProvider>) -> SharedEmbedProvider {
    Arc::new(std::sync::RwLock::new(Some(provider)))
}

/// One retrieval configuration under test. Named so the report reads as an
/// ablation, not a list of numbers.
struct Variant {
    name: &'static str,
    /// Feed analyzer-produced lexical terms (ADR-095 D2) instead of the raw query.
    analyzed: bool,
    memory_policy: FusionPolicy,
    conversation_policy: FusionPolicy,
}

/// The pre-ADR-095 configuration: raw query driving both sources, equal weights,
/// no cosine floor. The reference every new variant is compared against.
fn baseline_variants() -> Vec<Variant> {
    let equal = || FusionPolicy { vector_weight: 1.0, text_weight: 1.0, min_cosine: None, pool_factor: 2 };
    vec![
        Variant {
            name: "baseline_raw_equal",
            analyzed: false,
            memory_policy: equal(),
            conversation_policy: equal(),
        },
        Variant {
            name: "adr095_analyzed_4to1",
            analyzed: true,
            memory_policy: FusionPolicy::SEMANTIC_4TO1,
            conversation_policy: FusionPolicy::CONVERSATION_BASELINE,
        },
        // Isolates the analyzer from the weights: the pair above changes both.
        Variant {
            name: "analyzed_equal_weights",
            analyzed: true,
            memory_policy: equal(),
            conversation_policy: equal(),
        },
        Variant {
            name: "raw_4to1",
            analyzed: false,
            memory_policy: FusionPolicy::SEMANTIC_4TO1,
            conversation_policy: FusionPolicy::CONVERSATION_BASELINE,
        },
    ]
}

/// Ranked output of one query through one variant, resolved to corpus tags.
struct Run {
    tags: Vec<String>,
    /// `matched_by` per returned item, same order as `tags`.
    matched: Vec<String>,
    latency_ms: f64,
}

impl Stack {
    async fn run_with_embedding(
        &self,
        variant: &Variant,
        q: &str,
        embedding: Option<Vec<f32>>,
        k: usize,
    ) -> Result<Run, String> {
        let started = std::time::Instant::now();
        let analyzed: Vec<String> = if variant.analyzed {
            analyzer::analyze(q, &analyzer::AnalyzerConfig::default()).lexical_terms
        } else {
            Vec::new()
        };

        let mem_query = SemanticMemoryQuery {
            query_text: q.to_string(),
            mode: "hybrid".to_string(),
            limit: k,
            embedding: embedding.clone(),
            lexical_terms: analyzed.clone(),
        };
        let mem_hits = self
            .memory
            .semantic_search_detailed(&mem_query, &variant.memory_policy)
            .await
            .map_err(|e| format!("memory scope failed: {e}"))?;

        let lexical = LexicalQuery { raw: q, terms: &analyzed };
        let conv_hits = self
            .index
            .search_detailed(&lexical, embedding.as_deref(), k, &variant.conversation_policy);

        let mut tags = Vec::new();
        let mut matched = Vec::new();
        for scored in &mem_hits {
            tags.push(self.tag_of(&scored.node.content));
            matched.push(match scored.evidence.as_ref() {
                Some(e) => e.matched_by.as_str().to_string(),
                None => "none".to_string(),
            });
        }
        for hit in &conv_hits {
            tags.push(self.tag_of(&hit.content));
            matched.push(match hit.matched_by {
                Some(m) => m.as_str().to_string(),
                None => "none".to_string(),
            });
        }
        Ok(Run {
            tags,
            matched,
            latency_ms: started.elapsed().as_secs_f64() * 1000.0,
        })
    }

    fn tag_of(&self, content: &str) -> String {
        self.tags
            .get(content)
            .cloned()
            .unwrap_or_else(|| format!("?{content}"))
    }
}

// ---------------------------------------------------------------------------
// Tier 2 — production embedding model
// ---------------------------------------------------------------------------

struct LiveModel {
    provider: Arc<RemoteEmbeddingProvider>,
    model_id: String,
    dim: usize,
}

/// Probe the embedding service; `None` when it is not listening, which makes the
/// caller skip rather than fail.
async fn live_model() -> Option<LiveModel> {
    let v: serde_json::Value = reqwest::get(format!("{EMBED_BASE_URL}/health"))
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    if v.get("status")?.as_str()? != "ready" {
        return None;
    }
    let model = v.get("model")?;
    let model_id = model.get("id")?.as_str()?.to_string();
    let dim = model.get("dimension")?.as_u64()? as usize;
    if dim == 0 {
        return None;
    }
    let provider = Arc::new(RemoteEmbeddingProvider::with_config(
        &format!("{EMBED_BASE_URL}/v1"),
        None,
        &model_id,
        dim,
    ));
    // Trust the advertised width only after producing a vector of it.
    match provider.embed("ping").await {
        Ok(e) if e.len() == dim => Some(LiveModel { provider, model_id, dim }),
        _ => None,
    }
}

/// `tag` must differ per test: two stacks sharing a directory would seed the
/// corpus twice into one SQLite file and quietly double every metric.
async fn tier2_stack(live: &LiveModel, tag: &str) -> Stack {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let work_dir = std::env::temp_dir().join(format!(
        "acowork-e2e-gs-live-{}-{tag}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&work_dir);
    let (store, index) = open_stack(&work_dir, live.dim);

    let mem = memory_corpus();
    let conv = conversation_corpus();
    let texts: Vec<&str> = mem.iter().chain(conv.iter()).map(|d| d.content).collect();
    let vectors = live
        .provider
        .embed_batch(&texts)
        .await
        .expect("batch embed corpus");
    assert_eq!(vectors.len(), texts.len(), "one vector per document");

    let mut it = vectors.into_iter();
    for doc in &mem {
        acowork_memory::MemoryProvider::store_knowledge(
            store.as_ref(),
            &knowledge_node(doc.content, it.next().unwrap()),
        )
        .expect("store_knowledge");
    }
    for (i, doc) in conv.iter().enumerate() {
        // Spread across sessions so summary-candidate session diversity is
        // representable.
        let session = ["sess-alpha", "sess-beta", "sess-gamma"][i % 3];
        index
            .index_message(session, i, "user", doc.content, &it.next().unwrap())
            .expect("index_message");
    }

    let embed = embed_slot(live.provider.clone() as Arc<dyn EmbeddingProvider>);
    let memory = Arc::new(MemoryAdminAdapter::new(memory_slot(store.clone()), embed.clone()));
    Stack {
        work_dir,
        store,
        index,
        memory,
        tags: tag_index(),
    }
}

#[tokio::test]
#[ignore = "requires the local embedding service on 127.0.0.1:18080 (see dev/eval/retrieval/README.md)"]
async fn tier2_quality_against_production_model() {
    let Some(live) = live_model().await else {
        eprintln!("SKIP: no embedding service at {EMBED_BASE_URL}. Tier 2 measures the real model and has no substitute.");
        return;
    };
    eprintln!("live model: {} dim={}", live.model_id, live.dim);
    let stack = tier2_stack(&live, "quality").await;

    let variants = baseline_variants();
    let qs = queries();
    let mut rows: Vec<serde_json::Value> = Vec::new();

    for variant in &variants {
        let (mut rec, mut prec, mut mrr, mut ndcg, mut lat) =
            (vec![], vec![], vec![], vec![], vec![]);
        let mut false_hits = 0usize;
        let mut false_total = 0usize;
        let mut exact_hit = 0usize;
        let mut exact_total = 0usize;
        let mut per_query: Vec<serde_json::Value> = Vec::new();

        for q in &qs {
            let embedding = live
                .provider
                .embed(q.text)
                .await
                .unwrap_or_else(|e| panic!("embed {}: {e}", q.id));
            let run = variant
                .name;
            let run = stack
                .run_with_embedding(variant, q.text, Some(embedding), K)
                .await
                .unwrap_or_else(|e| panic!("{run} / {}: {e}", q.id));
            let expect: HashSet<String> = q.expect.iter().map(|s| s.to_string()).collect();
            lat.push(run.latency_ms);

            if expect.is_empty() {
                false_total += 1;
                if !run.tags.is_empty() {
                    false_hits += 1;
                }
                continue;
            }
            rec.push(recall_at_k(&run.tags, &expect, K));
            prec.push(precision_at_k(&run.tags, &expect, K));
            mrr.push(reciprocal_rank(&run.tags, &expect));
            ndcg.push(ndcg_at_k(&run.tags, &expect, K));
            if q.intent == "exact_entity" {
                exact_total += 1;
                if run.tags.iter().take(K).any(|t| expect.contains(t)) {
                    exact_hit += 1;
                }
            }
            per_query.push(serde_json::json!({
                "query": q.id,
                "intent": q.intent,
                "returned": run.tags,
                "matched_by": run.matched,
            }));
        }

        rows.push(serde_json::json!({
            "variant": variant.name,
            "recall_at_k": (mean(&rec) * 1000.0).round() / 1000.0,
            "precision_at_k": (mean(&prec) * 1000.0).round() / 1000.0,
            "mrr": (mean(&mrr) * 1000.0).round() / 1000.0,
            "ndcg_at_k": (mean(&ndcg) * 1000.0).round() / 1000.0,
            "false_recall_hits": false_hits,
            "no_relevant_queries": false_total,
            "exact_term_retention": format!("{exact_hit}/{exact_total}"),
            "p50_ms": (percentile(&lat, 0.50) * 10.0).round() / 10.0,
            "p95_ms": (percentile(&lat, 0.95) * 10.0).round() / 10.0,
            "per_query": per_query,
        }));
    }

    // Guard: a seeded-store collision would inflate every metric silently. Fail
    // here instead of reporting numbers that are not from this corpus.
    let mem_nodes = stack
        .store
        .node_count_by_label(acowork_memory::types::labels::KNOWLEDGE)
        .unwrap_or(u64::MAX);
    assert_eq!(
        mem_nodes as usize,
        memory_corpus().len(),
        "memory corpus must be seeded exactly once"
    );
    assert_eq!(
        stack.index.message_count() as usize,
        conversation_corpus().len(),
        "conversation corpus must be seeded once"
    );

    let report = serde_json::json!({
        "model": live.model_id,
        "embedding_dim": live.dim,
        "k": K,
        "memory_docs": memory_corpus().len(),
        "conversation_docs": conversation_corpus().len(),
        "queries": queries().len(),
        "precision_definition": "hits / k (strict); not comparable to the archived m4/m5 benchmarks",
        "caveat": "Corpus is synthetic-but-realistic and small (37 documents). Directional evidence for ADR-095 §9, not a release gate.",
        "variants": rows,
    });
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/eval/retrieval/reports");
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("e2e_live_model.json"), serde_json::to_string_pretty(&report).unwrap())
        .expect("write report");

    for r in &rows {
        eprintln!(
            "{:<24} R@{} {:.3}  P@{} {:.3}  MRR {:.3}  nDCG {:.3}  false {} /{}  exact {}  p50 {}ms p95 {}ms",
            r["variant"].as_str().unwrap(),
            K,
            r["recall_at_k"].as_f64().unwrap_or_default(),
            K,
            r["precision_at_k"].as_f64().unwrap_or_default(),
            r["mrr"].as_f64().unwrap_or_default(),
            r["ndcg_at_k"].as_f64().unwrap_or_default(),
            r["false_recall_hits"].as_u64().unwrap_or_default(),
            r["no_relevant_queries"].as_u64().unwrap_or_default(),
            r["exact_term_retention"].as_str().unwrap_or("n/a"),
            r["p50_ms"].as_f64().unwrap_or_default(),
            r["p95_ms"].as_f64().unwrap_or_default(),
        );
    }
    eprintln!("report -> dev/eval/retrieval/reports/e2e_live_model.json");

    // Structural invariants, asserted on the real model.
    let v = &variants[1];
    for q in queries().iter().filter(|q| q.intent == "exact_entity") {
        let embedding = live.provider.embed(q.text).await.unwrap();
        let run = stack.run_with_embedding(v, q.text, Some(embedding), K).await.unwrap();
        assert!(
            run.tags.iter().any(|t| q.expect.contains(&t.as_str())),
            "exact entity {:?} must be retrievable with the real model, got {:?}",
            q.text,
            run.tags
        );
        assert!(
            run.matched.iter().all(|m| m == "vector" || m == "text" || m == "both"),
            "every hit must carry a matched_by vocabulary value, got {:?}",
            run.matched
        );
    }
}

/// The cosine distribution the §9 threshold question needs: how far do relevant
/// and irrelevant candidates actually sit under the production model? Printed,
/// not asserted — the number is what a human has to decide with.
#[tokio::test]
#[ignore = "requires the local embedding service on 127.0.0.1:18080"]
async fn tier2_cosine_distribution_for_threshold_decision() {
    let Some(live) = live_model().await else {
        eprintln!("SKIP: no embedding service at {EMBED_BASE_URL}");
        return;
    };
    let stack = tier2_stack(&live, "cosine").await;

    let relevant_tags: HashSet<String> = queries()
        .iter()
        .flat_map(|q| q.expect.iter().map(|s| s.to_string()))
        .collect();

    let mut rel: Vec<f64> = Vec::new();
    let mut irr: Vec<f64> = Vec::new();
    for q in queries().iter().filter(|q| !q.expect.is_empty()) {
        let embedding = live.provider.embed(q.text).await.unwrap();
        // Deep pool so the tail is visible, which is where a floor would bite.
        let lexical = LexicalQuery { raw: q.text, terms: &[] };
        let hits = stack.index.search_detailed(
            &lexical,
            Some(&embedding),
            20,
            &FusionPolicy::CONVERSATION_BASELINE,
        );
        for hit in hits {
            // Only vector-sourced candidates report a cosine; a lexical-only hit
            // has none and is not evidence about the floor either way.
            if let Some(c) = hit.cosine {
                if relevant_tags.contains(&stack.tag_of(&hit.content)) {
                    rel.push(c);
                } else {
                    irr.push(c);
                }
            }
        }
    }

    let summary = |v: &[f64]| -> serde_json::Value {
        if v.is_empty() {
            return serde_json::Value::Null;
        }
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        serde_json::json!({
            "n": s.len(),
            "min": (s[0] * 1000.0).round() / 1000.0,
            "p10": (percentile(&s, 0.10) * 1000.0).round() / 1000.0,
            "median": (percentile(&s, 0.50) * 1000.0).round() / 1000.0,
            "p90": (percentile(&s, 0.90) * 1000.0).round() / 1000.0,
            "max": (s[s.len() - 1] * 1000.0).round() / 1000.0,
        })
    };
    let out = serde_json::json!({
        "model": live.model_id,
        "relevant_cosine": summary(&rel),
        "irrelevant_cosine": summary(&irr),
        "note": "Overlap between these two distributions is what a cosine floor would have to cut through. A floor above the relevant minimum rejects real hits.",
    });
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dev/eval/retrieval/reports");
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("e2e_cosine_distribution.json"), serde_json::to_string_pretty(&out).unwrap())
        .unwrap();
    eprintln!("cosine distribution: {}", serde_json::to_string(&out).unwrap());
}

// ---------------------------------------------------------------------------
// Tier 1 — real HTTP server
// ---------------------------------------------------------------------------

/// Stub embedder for Tier 1: the store needs *some* vector column width, and the
/// HTTP path never calls it (no `AgentCore` is injectable), so the vectors exist
/// only to keep the schema self-consistent.
struct FixedEmbedding(usize);

#[async_trait::async_trait]
impl EmbeddingProvider for FixedEmbedding {
    fn name(&self) -> &str {
        "fixed-test-embedding"
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, acowork_core::EmbeddingError> {
        let mut v = vec![0.0f32; self.0];
        for (i, b) in text.as_bytes().iter().enumerate() {
            v[i % self.0] += (*b as f32) / 1000.0;
        }
        Ok(v)
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, acowork_core::EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }

    fn dimension(&self) -> usize {
        self.0
    }

    async fn is_available(&self) -> bool {
        true
    }
}

const TIER1_DIM: usize = 64;

/// Boot a real `RuntimeHttpServer` over a seeded store and write the agent's
/// `search` policy, returning the port.
async fn tier1_server(variant_analyzer_mode: &str) -> (RuntimeHttpServer, Stack) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let work_dir = std::env::temp_dir().join(format!(
        "acowork-e2e-gs-http-{}-{variant_analyzer_mode}-{seq}",
        std::process::id(),
    ));
    let _ = std::fs::remove_dir_all(&work_dir);
    let (store, index) = open_stack(&work_dir, TIER1_DIM);

    let fixed = Arc::new(FixedEmbedding(TIER1_DIM));
    for (i, doc) in memory_corpus().iter().enumerate() {
        let mut v = fixed.embed(doc.content).await.unwrap();
        // Decorrelate otherwise-similar fixed vectors so the vector source is
        // not a constant; Tier 1 does not measure it.
        v[i % TIER1_DIM] += 1.0;
        acowork_memory::MemoryProvider::store_knowledge(store.as_ref(), &knowledge_node(doc.content, v))
            .expect("store_knowledge");
    }
    for (i, doc) in conversation_corpus().iter().enumerate() {
        index
            .index_message("sess-one", i, "user", doc.content, &fixed.embed(doc.content).await.unwrap())
            .expect("index_message");
    }

    let embed = embed_slot(fixed.clone() as Arc<dyn EmbeddingProvider>);
    let memory = Arc::new(MemoryAdminAdapter::new(memory_slot(store.clone()), embed.clone()));

    // The real config service reads work_dir/config/agent_config.json, so the
    // analyzer switch is exercised through the same file path production uses.
    std::fs::write(
        work_dir.join("config").join("agent_config.json"),
        serde_json::json!({ "search": { "analyzer_mode": variant_analyzer_mode } }).to_string(),
    )
    .unwrap();

    let stack = Stack {
        work_dir: work_dir.clone(),
        store,
        index: index.clone(),
        memory,
        tags: tag_index(),
    };

    let conv_slot: SharedConversationIndex = Arc::new(std::sync::RwLock::new(Some(index)));
    let memory_query: Arc<
        tokio::sync::Mutex<Option<Arc<dyn MemoryQueryService>>>,
    > = Arc::new(tokio::sync::Mutex::new(Some(stack.memory.clone() as Arc<dyn MemoryQueryService>)));
    let agent_config: Arc<
        tokio::sync::Mutex<Option<Arc<dyn AgentConfigService>>>,
    > = Arc::new(tokio::sync::Mutex::new(Some(Arc::new(RuntimeAgentConfigService::new(
        work_dir.clone(),
    )))));

    let none_snapshots: acowork_runtime::agent::session_state::SharedSessionSnapshots =
        Arc::new(std::sync::RwLock::new(HashMap::new()));
    let server = RuntimeHttpServer::start_with_bind_port(
        0,
        work_dir.clone(),
        work_dir.clone(),
        AGENT_ID.to_string(),
        INSTANCE_ID.to_string(),
        none_snapshots,
        Arc::new(std::sync::RwLock::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(std::sync::RwLock::new(Vec::new())),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        memory_query,
        conv_slot,
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        agent_config,
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(std::sync::RwLock::new(None)),
        Arc::new(std::sync::RwLock::new(None)),
        Arc::new(tokio::sync::Mutex::new(None)),
        Arc::new(std::sync::RwLock::new(
            acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        )),
        Arc::new(tokio::sync::RwLock::new(None)),
        Arc::new(std::sync::RwLock::new(None)),
    )
    .await
    .expect("runtime http server should start");

    (server, stack)
}

/// One `/search` response, kept as raw JSON: Tier 1 tests the *wire* contract,
/// so it must not be decoded into a convenient struct first.
async fn search(port: u16, q: &str, scopes: &str) -> (u16, serde_json::Value) {
    let url = format!(
        "http://127.0.0.1:{port}/search?q={}&scopes={scopes}&limit=10",
        urlencoding(q)
    );
    let resp = reqwest::get(url).await.expect("request should reach server");
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.expect("json body");
    (status, body)
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "%20".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The top-level `hits` array, in returned order.
///
/// `/search` flattens every scope into one list and reports per-scope status
/// separately under `scopes` — the Desktop renders from `hits`, so that is what
/// the contract has to be read off.
fn hits(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body.get("hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// `matched_by` / `fused_rank` for every hit that carries them.
fn evidence_fields(body: &serde_json::Value) -> (Vec<String>, Vec<Option<u64>>) {
    let mut matched = Vec::new();
    let mut ranks = Vec::new();
    for hit in hits(body) {
        let payload = hit.get("payload").cloned().unwrap_or_default();
        if let Some(m) = payload.get("matched_by").and_then(|v| v.as_str()) {
            matched.push(m.to_string());
            ranks.push(payload.get("fused_rank").and_then(|v| v.as_u64()));
        }
    }
    (matched, ranks)
}

#[tokio::test]
async fn tier1_wire_contract_over_real_http() {
    let (server, _stack) = tier1_server("off").await;
    let port = server.port;

    let (status, body) = search(port, "bge-m3", "memory,conversation").await;
    assert_eq!(status, 200, "GET /search must answer 200, got {status}");

    // Both scopes answered (not skipped, not errored).
    for scope in ["memory", "conversation"] {
        let s = body["scopes"][scope].clone();
        assert!(
            s["status"].as_str() == Some("ok"),
            "{scope} scope must report ok, got {s:?}"
        );
    }

    let (matched, ranks) = evidence_fields(&body);
    assert!(
        !matched.is_empty(),
        "memory/conversation hits must carry matched_by once ADR-095 is wired"
    );
    for m in &matched {
        assert!(
            matches!(m.as_str(), "vector" | "text" | "both"),
            "matched_by must stay inside the wire vocabulary, got {m:?}"
        );
    }

    // fused_rank is present wherever evidence is present: the Desktop renders in
    // server order now that score is a real (small, tie-prone) RRF value.
    let present: Vec<u64> = ranks.iter().flatten().copied().collect();
    assert_eq!(present.len(), matched.len(), "every evidenced hit must carry fused_rank");

    // Locate metadata must survive alongside the evidence. ADR-095 added
    // matched_by/ranks to the memory payload; assigning that object as the
    // *whole* payload silently dropped node_id, the Desktop mapped the missing
    // id to 0, and every memory preview spun forever on a fetch of node 0. The
    // list still rendered because it reads `snippet`, which is why a
    // list-only assertion would not have caught this.
    let all_hits = hits(&body);
    let memory_hits: Vec<&serde_json::Value> = all_hits
        .iter()
        .filter(|h| h["scope"].as_str() == Some("memory"))
        .collect();
    assert!(
        !memory_hits.is_empty(),
        "memory scope must return hits for an exact-entity query"
    );
    for h in &memory_hits {
        let payload = &h["payload"];
        assert!(
            payload.get("node_id").and_then(|v| v.as_u64()).is_some(),
            "memory payload must carry node_id for the preview to resolve, got {payload}"
        );
        assert!(
            payload.get("node_id").and_then(|v| v.as_u64()) != Some(0),
            "node_id 0 is not addressable; a zero here means the producer lost the field"
        );
        assert!(
            payload.get("matched_by").is_some(),
            "evidence must be present alongside locate keys, got {payload}"
        );
    }

    // Same contract for conversation: the UI jumps to a message by
    // session_id + message_index.
    for h in all_hits
        .iter()
        .filter(|h| h["scope"].as_str() == Some("conversation"))
    {
        let payload = &h["payload"];
        assert!(
            payload.get("session_id").and_then(|v| v.as_str()).is_some()
                && payload.get("message_index").is_some(),
            "conversation payload must keep its locate keys, got {payload}"
        );
    }

    // Privacy contract (ADR-095 §6): no raw similarity on the wire.
    let raw = serde_json::to_string(&body).unwrap();
    for forbidden in ["\"cosine\"", "\"bm25\"", "\"raw_score\""] {
        assert!(
            !raw.contains(forbidden),
            "payload must not expose {forbidden} — an uncalibrated score must not reach the UI"
        );
    }
}

#[tokio::test]
async fn tier1_analyzer_mode_changes_the_lexical_plan_end_to_end() {
    // What the switch is observable through on the wire: `search_meta` reports
    // the mode and how many analyzed terms the lexical source actually ran.
    // Asserting that is stronger than asserting a particular result order, which
    // depends on corpus statistics rather than on the switch working.
    let q = "how do I find the gateway port 19876";

    let (off_server, _off_stack) = tier1_server("off").await;
    let (on_server, _on_stack) = tier1_server("on").await;
    let (shadow_server, _shadow_stack) = tier1_server("shadow").await;

    let (_, off_body) = search(off_server.port, q, "memory,conversation").await;
    let (_, on_body) = search(on_server.port, q, "memory,conversation").await;
    let (_, shadow_body) = search(shadow_server.port, q, "memory,conversation").await;

    let term_count = |b: &serde_json::Value| -> Option<u64> {
        b["search_meta"]["lexical_term_count"].as_u64()
    };
    let mode = |b: &serde_json::Value| -> String {
        b["search_meta"]["analyzer_mode"].as_str().unwrap_or("").to_string()
    };

    assert_eq!(mode(&off_body), "off", "config must reach the handler through the real config service");
    assert_eq!(mode(&on_body), "on");
    assert_eq!(mode(&shadow_body), "shadow");

    // `off` runs the raw query: no analyzed terms.
    assert_eq!(
        term_count(&off_body),
        Some(0),
        "analyzer_mode=off must not run analyzed terms (ADR-095 §7 rollback guarantee)"
    );
    // `on` must actually produce terms — otherwise "on" is indistinguishable
    // from "off" and the switch is decorative.
    let on_count = term_count(&on_body).unwrap_or(0);
    assert!(
        on_count > 0,
        "analyzer_mode=on must run analyzed terms, got {on_count} (body: {on_body})"
    );
    // Stop-word filtering is the analyzer's job, so the analyzed plan must be
    // narrower than the raw whitespace split of the same sentence.
    let raw_terms = q.split_whitespace().count();
    assert!(
        (on_count as usize) < raw_terms,
        "analyzed terms ({on_count}) should be fewer than raw whitespace tokens ({raw_terms}) once stop words are dropped"
    );

    // Shadow observes but never decides: identical results to `off`.
    assert_eq!(
        term_count(&shadow_body),
        Some(0),
        "shadow must keep answering with the raw query"
    );
    assert_eq!(
        evidence_tags(&shadow_body),
        evidence_tags(&off_body),
        "shadow mode must return identical results to off"
    );

    // Recall must not be lost on *labeled* relevance. Measured against the raw
    // result set instead, this assertion would be wrong: the raw path OR-matches
    // stop words ("the", "and"), so it returns documents like M-SQL-1 ("The
    // SQLite backend stores ... in one file per agent workspace") for a
    // "gateway port 19876" query. Those extra hits are noise, not recall, and
    // dropping them is the point of D2.
    let labeled = ["M-GW-1", "C-GW-1", "C-GW-3"];
    let off_tags = evidence_tags(&off_body);
    let on_tags = evidence_tags(&on_body);
    for tag in labeled {
        if off_tags.contains(&tag.to_string()) {
            assert!(
                on_tags.contains(&tag.to_string()),
                "analyzer_mode=on lost labeled-relevant {tag} that the raw path found for {q:?}: {on_tags:?}"
            );
        }
    }
    // And the analyzed plan should be at least as tight: fewer, better terms
    // must not return *more* documents than the stop-word-inflated raw path.
    assert!(
        on_tags.len() <= off_tags.len(),
        "analyzed terms should not widen the result set beyond the raw path: off={} on={}",
        off_tags.len(),
        on_tags.len()
    );
}

/// Corpus tags for every memory/conversation hit, in returned order.
fn evidence_tags(body: &serde_json::Value) -> Vec<String> {
    let tags = tag_index();
    hits(body)
        .iter()
        .filter(|h| {
            matches!(h["scope"].as_str(), Some("memory") | Some("conversation"))
        })
        .map(|h| {
            let snippet = h["snippet"].as_str().unwrap_or("");
            tags.get(snippet).cloned().unwrap_or_else(|| format!("?{snippet}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tier 3 — summary endpoint trust boundary
// ---------------------------------------------------------------------------

/// `POST /search/summary` with a raw JSON body.
async fn summary(port: u16, body: serde_json::Value) -> (u16, String) {
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/search/summary"))
        .json(&body)
        .send()
        .await
        .expect("summary request should reach the server");
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

fn candidate(id: &str, scope: &str, matched_by: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "scope": scope,
        "title": "t",
        "snippet": "s",
        "matched_by": matched_by,
    })
}

/// The validator runs before any provider is resolved, so every case here is
/// free: a 422 proves the request was rejected at the trust boundary, and any
/// non-422 proves it passed validation (reaching the "no LLM bound" path in
/// this harness). No summary model is ever called.
#[tokio::test]
async fn tier3_summary_candidate_validation_over_http() {
    let (server, _stack) = tier1_server("off").await;
    let port = server.port;

    // Evidence categories ADR-095 D4 puts on the wire must be accepted.
    for good in ["vector", "text", "both"] {
        let (status, body) = summary(
            port,
            serde_json::json!({
                "query": "gateway port",
                "candidates": [candidate("c1", "conversation", Some(good))],
            }),
        )
        .await;
        assert_ne!(
            status, 422,
            "matched_by={good:?} is a valid evidence category, rejected: {body}"
        );
    }

    // A candidate carrying a made-up evidence category must be rejected, not
    // coerced: an unknown category in the prompt would read as evidence.
    for bad in ["relevance", "high", "VECTOR", "", "both "] {
        let (status, _) = summary(
            port,
            serde_json::json!({
                "query": "gateway port",
                "candidates": [candidate("c1", "conversation", Some(bad))],
            }),
        )
        .await;
        assert_eq!(
            status, 422,
            "matched_by={bad:?} is not in the vocabulary and must be rejected"
        );
    }

    // Absent matched_by stays legal — non-fused sources have no evidence to claim.
    let (status, body) = summary(
        port,
        serde_json::json!({
            "query": "gateway port",
            "candidates": [candidate("f1", "file", None)],
        }),
    )
    .await;
    assert_ne!(status, 422, "matched_by omitted must be accepted: {body}");

    // The offending value must not be reflected back (it is client-controlled
    // and one refactor away from the prompt).
    let (status, reflected) = summary(
        port,
        serde_json::json!({
            "query": "gateway port",
            "candidates": [candidate("c1", "conversation", Some("REFLECT_ME_NOT"))],
        }),
    )
    .await;
    assert_eq!(status, 422);
    assert!(
        !reflected.contains("REFLECT_ME_NOT"),
        "422 body must not echo client input, got: {reflected}"
    );

    // Budget is a ceiling, and 0 candidates is a client bug: the Desktop must
    // not call at all, and the server must not summarize nothing into something.
    let (status, _) = summary(
        port,
        serde_json::json!({ "query": "gateway port", "candidates": [] }),
    )
    .await;
    assert_eq!(status, 422, "empty candidate list must be rejected");

    let (status, _) = summary(
        port,
        serde_json::json!({
            "query": "gateway port",
            "candidates": (1..=31).map(|i| candidate(&format!("c{i}"), "memory", Some("text"))).collect::<Vec<_>>(),
        }),
    )
    .await;
    assert_eq!(status, 422, "31 candidates exceeds the 30 budget");

    // Duplicate ids would make citations ambiguous.
    let (status, _) = summary(
        port,
        serde_json::json!({
            "query": "gateway port",
            "candidates": [candidate("dup", "memory", Some("text")), candidate("dup", "conversation", Some("text"))],
        }),
    )
    .await;
    assert_eq!(status, 422, "duplicate candidate id must be rejected");
}
