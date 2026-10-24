//! End-to-end global-search test for the **Runtime binary** (ADR-095, ADR-100).
//!
//! ## Why this file exists
//!
//! `retrieval_eval.rs` (same directory) proves the *fusion arithmetic* is sound,
//! but it does so against a deterministic hashed-token stand-in for the
//! embedding model, so its numbers cannot answer what ADR-095 §9 actually asks
//! (is 4:1 right for message text? is a cosine floor safe to enable?). Those
//! need the real model and the real HTTP surface. This file supplies both, and
//! it is also the only place the `/search` **wire contract** is asserted — the
//! shape the Desktop renders, not the arithmetic that produced it.
//!
//! ## Scope discipline (ADR-100)
//!
//! Runtime and the memory service are separate binaries, so this file covers the
//! Runtime half only: the conversation scope, plus the *absence* of the memory
//! scope. It does not link `acowork-memory` and must never start to — the memory
//! read path is measured by `core/acowork-memory/tests/*`.
//!
//! What that means for the retired scope: `/search` no longer answers `memory`.
//! The contract is an honest `{"status":"not_available"}` and **zero memory
//! hits**, which is exactly what Tier 1 asserts, because an empty result set
//! would be indistinguishable from "nothing matched" for the caller.
//!
//! ## Tiers
//!
//! * **Tier 1 — real HTTP, real SQLite, real FTS** (runs in CI). A live
//!   `RuntimeHttpServer` on an ephemeral port answers `GET /search` over a
//!   seeded conversation index. Lexical-only by construction: no
//!   `AgentCore` is injectable from an integration test, so `embed_query` takes
//!   its documented `None` fallback — and `search_meta.conversation.embedded`
//!   reports that on the wire rather than hiding it. Tier 1 also proves the
//!   `analyzer_mode` switch reaches the handler through the real config file.
//! * **Tier 2 — the production embedding model** (`#[ignore]`, needs the local
//!   embedding service). Drives `ConversationIndex::search_detailed` with the
//!   model the service currently has loaded, asked from `/health` instead of
//!   hardcoded, so a model swap is measured rather than silently missed. It
//!   asserts **no quality threshold** — ADR-095 D6 sets those from measured data
//!   and §9 leaves the numbers open for review; encoding a guessed number here
//!   would launder an undecided policy into a passing test. Structural
//!   invariants *are* asserted, because those are already decided.
//!
//! ## Shared kernel
//!
//! Metric math, dataset reading, the stand-in projection and the report shape
//! come from the dev-only `acowork_eval` crate. Nothing here redefines them: three
//! harnesses with one definition of nDCG is the difference between a comparison
//! and a translation exercise. Corpus and ground truth are the checked-in
//! `dev/eval/retrieval/` dataset, so Tier 2 and the variant harness measure the
//! same annotated material.

use std::collections::HashMap;
use std::sync::Arc;

use acowork_core::retrieval::{FusionPolicy, LexicalQuery};
use acowork_eval::{
    kept_cell, stand_in_embedding, CorpusRow, DatasetQuery, EvalDataset, EvalReport, QualityTally,
    VariantReport,
};
use acowork_runtime::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use acowork_runtime::conversation_index::ConversationIndex;
use acowork_runtime::embedding::remote::RemoteEmbeddingProvider;
use acowork_runtime::embedding::EmbeddingProvider;
use acowork_runtime::http::server::RuntimeHttpServer;
use acowork_runtime::search::analyzer::analyze;
use acowork_runtime::search::policy::SearchPolicyConfig;
use acowork_runtime::usecases::agent_config::AgentConfigService;
use acowork_runtime::usecases::agent_config_impl::RuntimeAgentConfigService;
use acowork_sqlite::SqliteStore;

const AGENT_ID: &str = "com.test.globalsearch";
const INSTANCE_ID: &str = "00000000-0000-4000-8000-000000000001";

/// The desktop's loopback embedding proxy. Deliberately probed, not assumed:
/// whatever model is loaded *is* the model under test.
const EMBED_BASE_URL: &str = "http://127.0.0.1:18080";

/// The one scope this binary answers for retrieval.
const SCOPE: &str = "conversation";
const K: usize = 5;
/// Tier 1 never runs the vector leg, but the store still needs a consistent
/// vector column width for the rows it indexes.
const TIER1_DIM: usize = 64;

// ---------------------------------------------------------------------------
// Dataset access
// ---------------------------------------------------------------------------

/// The `(session_id, message_index)` pair is both the dataset's ground-truth
/// pointer and the wire's locate key, so a hit resolves to an annotation without
/// matching on snippet text — the producer renders snippets and is free to
/// truncate them.
fn locate_key(session_id: &str, message_index: usize) -> String {
    format!("{session_id}#{message_index}")
}

/// Conversation-scope corpus rows with their original dataset index (which is
/// the key into `embeddings.json`).
fn corpus_rows(ds: &EvalDataset) -> Vec<(usize, &CorpusRow)> {
    ds.corpus
        .iter()
        .enumerate()
        .filter(|(_, r)| r.scope() == SCOPE)
        .collect()
}

/// Every locate key the seeded corpus contains — the ground set a wire hit must
/// fall inside.
fn seeded_keys(rows: &[(usize, &CorpusRow)]) -> Vec<String> {
    rows.iter()
        .map(|(_, r)| locate_key(&r.session_id, r.message_index))
        .collect()
}

/// Ground truth for one query, in this scope.
fn relevant_keys(q: &DatasetQuery) -> Vec<String> {
    q.annotations_for(SCOPE)
        .into_iter()
        .map(|a| {
            locate_key(
                a.session_id.as_deref().unwrap_or_default(),
                a.message_index.unwrap_or_default(),
            )
        })
        .collect()
}

/// `key -> content`, so a ranked hit can be shown with its text in the report.
fn content_by_key(rows: &[(usize, &CorpusRow)]) -> HashMap<String, String> {
    rows.iter()
        .map(|(_, r)| {
            (
                locate_key(&r.session_id, r.message_index),
                r.content.clone(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tier 1 — real HTTP server
// ---------------------------------------------------------------------------

/// A booted server plus the workspace it owns. The server handle is kept so the
/// listener cannot be dropped out from under a request.
struct Harness {
    port: u16,
    _server: RuntimeHttpServer,
    /// Held so the workspace (and its SQLite file) outlives every assertion.
    /// `TempDir`'s own cleanup is best-effort: on Windows the open SQLite
    /// handle makes the delete fail, and the OS recycles `%TEMP%` anyway.
    _dir: tempfile::TempDir,
    seeded: usize,
}

/// Boot a real `RuntimeHttpServer` over the dataset's conversation rows with the
/// given `analyzer_mode`, written the way production writes it
/// (`{work_dir}/config/agent_config.json`).
async fn tier1(analyzer_mode: &str) -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(work_dir.join("memory")).unwrap();
    std::fs::create_dir_all(work_dir.join("conversations")).unwrap();
    std::fs::create_dir_all(work_dir.join("config")).unwrap();

    let ds = EvalDataset::load_repo();
    let rows = corpus_rows(&ds);
    let store = Arc::new(
        SqliteStore::open(work_dir.join("memory").join("private.sqlite"), TIER1_DIM)
            .expect("open store"),
    );
    let index =
        Arc::new(ConversationIndex::from_store(store, &work_dir).expect("conversation index"));
    for (i, row) in &rows {
        // The stand-in projection keeps the seeded vectors deterministic and
        // decorrelated without a model. Tier 1 does not read them: with no
        // `AgentCore` bound, `/search` runs the lexical leg only.
        let v =
            stand_in_embedding(&row.content, TIER1_DIM).unwrap_or_else(|| vec![0.0f32; TIER1_DIM]);
        index
            .index_message(
                &row.session_id,
                row.message_index,
                &row.role,
                &row.content,
                &v,
            )
            .unwrap_or_else(|e| panic!("index corpus row {i}: {e}"));
    }

    std::fs::write(
        work_dir.join("config").join("agent_config.json"),
        serde_json::json!({ "search": { "analyzer_mode": analyzer_mode } }).to_string(),
    )
    .unwrap();

    let snapshots: SharedSessionSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
    let conversation_index: acowork_runtime::http::SharedConversationIndex =
        Arc::new(std::sync::RwLock::new(Some(index.clone())));
    let agent_config: Arc<tokio::sync::Mutex<Option<Arc<dyn AgentConfigService>>>> =
        Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
            RuntimeAgentConfigService::new(work_dir.clone()),
        ))));

    let server = RuntimeHttpServer::start_with_bind_port(
        0,
        work_dir.clone(),
        work_dir.clone(),
        AGENT_ID.to_string(),
        INSTANCE_ID.to_string(),
        snapshots,
        latest,
        // Every slot this harness does not exercise stays `None`, and the order
        // is positional: a slot shifted by one compiles and silently changes
        // meaning. These are the ones the search path reads.
        Arc::new(tokio::sync::Mutex::new(None)), // dispatch_tx
        Arc::new(std::sync::RwLock::new(Vec::new())), // degraded_reasons
        Arc::new(tokio::sync::Mutex::new(None)), // mqtt_client
        Arc::new(tokio::sync::Mutex::new(None)), // session_metadata
        conversation_index,
        Arc::new(tokio::sync::Mutex::new(None)), // workspace_query
        Arc::new(tokio::sync::Mutex::new(None)), // workspace_mutation
        Arc::new(tokio::sync::Mutex::new(None)), // git_query
        Arc::new(tokio::sync::Mutex::new(None)), // agent_tools
        agent_config,
        Arc::new(tokio::sync::Mutex::new(None)), // attachment
        Arc::new(tokio::sync::Mutex::new(None)), // session_config
        Arc::new(std::sync::RwLock::new(None)),  // rag_provider
        Arc::new(tokio::sync::Mutex::new(None)), // debug_service
        Arc::new(std::sync::RwLock::new(
            acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        )),
        Arc::new(tokio::sync::RwLock::new(None)), // session_manager_slot
        Arc::new(std::sync::RwLock::new(None)),   // agent_core
    )
    .await
    .expect("runtime http server should start");

    let seeded = index.message_count() as usize;
    Harness {
        port: server.port,
        _server: server,
        _dir: dir,
        seeded,
    }
}

/// One `/search` response as raw JSON: Tier 1 tests the *wire* contract, so the
/// body must not be decoded into a convenient struct first.
async fn search(port: u16, q: &str, scopes: &str) -> (u16, serde_json::Value) {
    let url = format!(
        "http://127.0.0.1:{port}/search?q={}&scopes={scopes}&limit=10",
        urlencoding(q)
    );
    let resp = reqwest::get(url)
        .await
        .expect("request should reach server");
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.expect("json body");
    (status, body)
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "%20".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The top-level `hits` array, in returned order.
///
/// `/search` flattens every scope into one list and reports per-scope status
/// under `scopes`; the Desktop renders from `hits`, so that is what the contract
/// has to be read off.
fn hits(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body.get("hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn hits_of_scope(body: &serde_json::Value, scope: &str) -> Vec<serde_json::Value> {
    hits(body)
        .into_iter()
        .filter(|h| h["scope"].as_str() == Some(scope))
        .collect()
}

/// `matched_by` / `fused_rank` for every hit that carries evidence.
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

/// Locate keys of the conversation hits, in returned order.
fn located(body: &serde_json::Value) -> Vec<String> {
    hits_of_scope(body, "conversation")
        .iter()
        .map(|h| {
            let p = &h["payload"];
            locate_key(
                p["session_id"].as_str().unwrap_or_default(),
                p["message_index"].as_u64().unwrap_or(u64::MAX) as usize,
            )
        })
        .collect()
}

#[tokio::test]
async fn tier1_wire_contract_and_retired_memory_scope() {
    let ds = EvalDataset::load_repo();
    let rows = corpus_rows(&ds);
    let h = tier1("off").await;
    assert_eq!(h.seeded, rows.len(), "corpus must be seeded exactly once");

    let (status, body) = search(h.port, "android icon build script", "conversation,memory").await;
    assert_eq!(status, 200, "GET /search must answer 200, got {status}");

    // The conversation scope answered rather than skipped or errored.
    let conv = &body["scopes"]["conversation"];
    assert_eq!(
        conv["status"].as_str(),
        Some("ok"),
        "conversation scope must report ok, got {conv}"
    );

    // ADR-100: this binary no longer holds memory nodes. The scope has to say so
    // — an empty-but-ok result would read to the Desktop as "nothing matched".
    let mem = &body["scopes"]["memory"];
    assert_eq!(
        mem["status"].as_str(),
        Some("not_available"),
        "memory scope must report not_available on the Runtime, got {mem}"
    );
    assert!(
        mem["message"]
            .as_str()
            .unwrap_or_default()
            .contains("/api/memory/search"),
        "the retired scope must name the service that answers it, got {mem}"
    );
    assert!(
        hits_of_scope(&body, "memory").is_empty(),
        "a Runtime that does not hold memory nodes must not return memory hits"
    );

    // Every returned row must be a row this index actually holds: a phantom
    // locate key renders as a clickable result that 404s on preview.
    let present = located(&body);
    assert!(
        !present.is_empty(),
        "an exact-term query must return conversation hits, got {body}"
    );
    let seeded = seeded_keys(&rows);
    for key in &present {
        assert!(
            seeded.contains(key),
            "wire hit {key} names no indexed message"
        );
    }

    // Evidence vocabulary is closed on the wire (ADR-095 D4).
    let (matched, ranks) = evidence_fields(&body);
    assert!(
        !matched.is_empty(),
        "conversation hits must carry matched_by once ADR-095 is wired"
    );
    for m in &matched {
        assert!(
            matches!(m.as_str(), "vector" | "text" | "both"),
            "matched_by must stay inside the wire vocabulary, got {m:?}"
        );
    }

    // `fused_rank` accompanies every evidenced hit, and the returned order *is*
    // the engine order — the scopes are concatenated, never re-sorted by score
    // (ADR-095 D4), so a monotone rank list is what proves the handler did not
    // sort on a value that is not comparable across sources.
    let present_ranks: Vec<u64> = ranks.iter().flatten().copied().collect();
    assert_eq!(
        present_ranks.len(),
        matched.len(),
        "every evidenced hit must carry fused_rank"
    );
    assert!(
        present_ranks.windows(2).all(|w| w[1] > w[0]),
        "hits must arrive in engine order, got {present_ranks:?}"
    );

    // Locate keys must survive alongside the evidence. ADR-095 added
    // matched_by/ranks to the payload; assigning that object as the *whole*
    // payload silently dropped the keys, the UI mapped the missing id to a
    // non-existent message, and every preview spun forever. The list still
    // rendered, which is why a list-only assertion would not have caught it.
    for h_row in hits_of_scope(&body, "conversation") {
        let payload = &h_row["payload"];
        assert!(
            payload["session_id"].as_str().is_some() && payload["message_index"].as_u64().is_some(),
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

    // Observability (ADR-095 D6): the report says how the query was processed.
    // `embedded: false` is the honest reading of this harness — no provider is
    // injectable here — and it is asserted so a future "vector leg silently
    // stopped running" cannot be mistaken for a retrieval regression.
    assert_eq!(body["search_meta"]["analyzer_mode"].as_str(), Some("off"));
    assert_eq!(
        body["search_meta"]["lexical_term_count"].as_u64(),
        Some(0),
        "analyzer_mode=off must not run analyzed terms"
    );
    assert_eq!(
        body["search_meta"]["conversation"]["embedded"].as_bool(),
        Some(false),
        "an integration harness with no AgentCore must report the lexical fallback"
    );
}

#[tokio::test]
async fn tier1_analyzer_mode_changes_the_lexical_plan_end_to_end() {
    // What the switch is observable through on the wire: `search_meta` reports
    // the mode and how many analyzed terms the lexical source actually ran.
    // Asserting that is stronger than asserting one particular result order,
    // which depends on corpus statistics rather than on the switch working.
    let probe = "which port does the gateway mqtt broker listen on";

    let off = tier1("off").await;
    let on = tier1("on").await;
    let shadow = tier1("shadow").await;

    let (_, off_probe) = search(off.port, probe, "conversation").await;
    let (_, on_probe) = search(on.port, probe, "conversation").await;
    let (_, shadow_probe) = search(shadow.port, probe, "conversation").await;

    let mode = |b: &serde_json::Value| {
        b["search_meta"]["analyzer_mode"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    let terms = |b: &serde_json::Value| b["search_meta"]["lexical_term_count"].as_u64();

    assert_eq!(
        mode(&off_probe),
        "off",
        "config must reach the handler through the real config service"
    );
    assert_eq!(mode(&on_probe), "on");
    assert_eq!(mode(&shadow_probe), "shadow");

    // `off` runs the raw query: no analyzed terms.
    assert_eq!(
        terms(&off_probe),
        Some(0),
        "analyzer_mode=off must not run analyzed terms (ADR-095 §7 rollback guarantee)"
    );
    // `on` must actually produce terms — otherwise "on" is indistinguishable
    // from "off" and the switch is decorative.
    let on_count = terms(&on_probe).unwrap_or(0);
    assert!(
        on_count > 0,
        "analyzer_mode=on must run analyzed terms, got {on_count} (body: {on_probe})"
    );
    // Stop-word filtering is the analyzer's job, so the analyzed plan must be
    // narrower than the raw whitespace split of the same sentence.
    let raw_terms = probe.split_whitespace().count();
    assert!(
        (on_count as usize) < raw_terms,
        "analyzed terms ({on_count}) should be fewer than raw whitespace tokens ({raw_terms}) once stop words are dropped"
    );

    // Shadow observes but never decides: identical results to `off`.
    assert_eq!(
        terms(&shadow_probe),
        Some(0),
        "shadow must keep answering with the raw query"
    );
    let (_, off_shadow) = search(shadow.port, "android icon build script", "conversation").await;
    let (_, off_off) = search(off.port, "android icon build script", "conversation").await;
    assert_eq!(
        located(&off_shadow),
        located(&off_off),
        "shadow mode must return identical results to off"
    );

    // Recall must not be lost on *labeled* relevance, for every query this scope
    // can answer. Measured against the raw result set instead, this would be the
    // wrong claim: the raw path OR-matches stop words, so its extra hits are
    // noise, not recall, and dropping them is the point of D2.
    let ds = EvalDataset::load_repo();
    for q in ds.queries.iter().filter(|q| !q.out_of_scope(SCOPE)) {
        let (_, off_body) = search(off.port, &q.query, "conversation").await;
        let (_, on_body) = search(on.port, &q.query, "conversation").await;
        let off_keys = located(&off_body);
        let on_keys = located(&on_body);
        let gt = relevant_keys(q);
        for key in &gt {
            if off_keys.contains(key) {
                assert!(
                    on_keys.contains(key),
                    "analyzer_mode=on lost labeled-relevant {key} for {:?} that the raw path found: off={off_keys:?} on={on_keys:?}",
                    q.query
                );
            }
        }
        // Fewer, better terms must not return *more* documents than the
        // stop-word-inflated raw path.
        assert!(
            on_keys.len() <= off_keys.len(),
            "analyzed terms should not widen the result set beyond the raw path for {:?}: off={} on={}",
            q.query,
            off_keys.len(),
            on_keys.len()
        );
        // And the switch must still be visible per query, including the CJK ones
        // where the analyzer emits n-grams instead of dropping words.
        assert!(
            terms(&on_body).unwrap_or(0) > 0,
            "analyzer_mode=on must produce a term plan for {:?} (language {})",
            q.query,
            q.lang
        );
    }
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
/// non-422 proves it passed validation (reaching the "agent core not bound"
/// path in this harness). No summary model is ever called.
#[tokio::test]
async fn tier3_summary_candidate_validation_over_http() {
    let h = tier1("off").await;
    let port = h.port;

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

// ---------------------------------------------------------------------------
// Tier 2 — production embedding model
// ---------------------------------------------------------------------------

struct LiveModel {
    provider: Arc<RemoteEmbeddingProvider>,
    model_id: String,
    dim: usize,
}

/// Probe the embedding service; `None` when it is not listening, which makes the
/// caller skip rather than fail. The model is a developer-machine dependency,
/// and a red suite without it would train everyone to ignore this file.
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
        Ok(e) if e.len() == dim => Some(LiveModel {
            provider,
            model_id,
            dim,
        }),
        _ => None,
    }
}

/// A conversation index seeded with the dataset's rows under the **real** model.
struct Live {
    index: Arc<ConversationIndex>,
    content: HashMap<String, String>,
    rows: usize,
    _dir: tempfile::TempDir,
}

async fn tier2_stack(live: &LiveModel) -> Live {
    // One directory per call: two stacks sharing a SQLite file would seed the
    // corpus twice into it and quietly double every metric.
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(work_dir.join("memory")).unwrap();
    std::fs::create_dir_all(work_dir.join("conversations")).unwrap();

    let ds = EvalDataset::load_repo();
    let rows = corpus_rows(&ds);
    let store = Arc::new(
        SqliteStore::open(work_dir.join("memory").join("private.sqlite"), live.dim)
            .expect("open store"),
    );
    let index =
        Arc::new(ConversationIndex::from_store(store, &work_dir).expect("conversation index"));

    let texts: Vec<&str> = rows.iter().map(|(_, r)| r.content.as_str()).collect();
    let vectors = live
        .provider
        .embed_batch(&texts)
        .await
        .expect("batch embed corpus");
    assert_eq!(vectors.len(), texts.len(), "one vector per document");
    for ((_, row), vector) in rows.iter().zip(&vectors) {
        assert_eq!(
            vector.len(),
            live.dim,
            "model returned a different width than advertised"
        );
        index
            .index_message(
                &row.session_id,
                row.message_index,
                &row.role,
                &row.content,
                vector,
            )
            .expect("index_message");
    }

    Live {
        index,
        content: content_by_key(&rows),
        rows: rows.len(),
        _dir: dir,
    }
}

/// One retrieval configuration under test. Named so the report reads as an
/// ablation, not a list of numbers: each pair below changes exactly one knob,
/// because a variant that moves two cannot attribute a loss to either.
struct Variant {
    name: &'static str,
    /// Feed analyzer-produced lexical terms (ADR-095 D2) instead of the raw query.
    analyzed: bool,
    policy: FusionPolicy,
    why: &'static str,
}

fn variants() -> Vec<Variant> {
    let shipped = SearchPolicyConfig::default().conversation_policy();
    let equal = || FusionPolicy {
        vector_weight: 1.0,
        text_weight: 1.0,
        min_cosine: None,
        pool_factor: 2,
    };
    vec![
        Variant {
            name: "baseline_raw_equal",
            analyzed: false,
            policy: equal(),
            why: "pre-ADR-095 reference: raw query, equal weights, no floor",
        },
        Variant {
            name: "shipped",
            analyzed: true,
            policy: shipped,
            why: "what runs today: analyzed terms, equal weights, 0.3 floor",
        },
        Variant {
            name: "raw_shipped_policy",
            analyzed: false,
            policy: shipped,
            why: "isolates the analyzer from the policy",
        },
        Variant {
            name: "analyzed_equal_no_floor",
            analyzed: true,
            policy: equal(),
            why: "isolates the cosine floor",
        },
        Variant {
            name: "analyzed_4to1_floor",
            analyzed: true,
            policy: FusionPolicy::CONVERSATION_4TO1,
            why: "the §9.3 candidate: memory's 4:1 ratio on message text",
        },
    ]
}

impl Live {
    fn run(&self, variant: &Variant, q: &DatasetQuery, embedding: &[f32]) -> (Vec<String>, f64) {
        let started = std::time::Instant::now();
        let terms: Vec<String> = if variant.analyzed {
            analyze(&q.query, &SearchPolicyConfig::default().analyzer_config()).lexical_terms
        } else {
            Vec::new()
        };
        let lexical = LexicalQuery {
            raw: &q.query,
            terms: &terms,
        };
        let hits = self
            .index
            .search_detailed(&lexical, Some(embedding), K, &variant.policy);
        let keys = hits
            .iter()
            .map(|h| locate_key(&h.session_id, h.message_index))
            .collect();
        (keys, started.elapsed().as_secs_f64() * 1000.0)
    }
}

/// Ignored by design: an evaluation, not a CI assertion. Run it with the local
/// embedding service up (see `dev/eval/retrieval/README.md`).
#[tokio::test]
#[ignore = "requires the local embedding service on 127.0.0.1:18080"]
async fn tier2_conversation_quality_against_production_model() {
    let Some(live) = live_model().await else {
        eprintln!("SKIP: no embedding service at {EMBED_BASE_URL}. Tier 2 measures the real model and has no substitute.");
        return;
    };
    eprintln!("live model: {} dim={}", live.model_id, live.dim);
    let stack = tier2_stack(&live).await;

    // Guard: a seeded-store collision would inflate every metric silently.
    assert_eq!(
        stack.index.message_count() as usize,
        stack.rows,
        "conversation corpus must be seeded exactly once"
    );

    let ds = EvalDataset::load_repo();
    let mut reports: Vec<VariantReport> = Vec::new();
    for variant in &variants() {
        let mut tally = QualityTally::new(K);
        let mut per_query = Vec::new();
        let mut skipped = 0usize;
        let mut latencies: Vec<f64> = Vec::new();
        // Every dataset query is offered to every variant, and the ones whose
        // ground truth lives in the memory scope are counted as skipped rather
        // than silently filtered by the caller — a reader has to be able to tell
        // a thin dataset from an engine that recalled nothing.
        for q in &ds.queries {
            if q.out_of_scope(SCOPE) {
                skipped += 1;
                continue;
            }
            let embedding = live
                .provider
                .embed(&q.query)
                .await
                .unwrap_or_else(|e| panic!("embed {}: {e}", q.id));
            let (keys, ms) = stack.run(variant, q, &embedding);
            latencies.push(ms);
            let analyzer_terms = if variant.analyzed {
                analyze(&q.query, &SearchPolicyConfig::default().analyzer_config())
                    .lexical_terms
                    .len()
            } else {
                0
            };
            let texts: Vec<String> = keys
                .iter()
                .filter_map(|k| stack.content.get(k).cloned())
                .collect();
            per_query.push(tally.record_query(
                q,
                &keys,
                &relevant_keys(q),
                &texts,
                Some(analyzer_terms),
            ));
        }
        reports.push(VariantReport {
            variant: variant.name.to_string(),
            metrics: tally.finish(),
            per_query,
            skipped_out_of_scope: skipped,
        });
        eprintln!(
            "{:<28} p50 {:.1}ms p95 {:.1}ms  ({})",
            variant.name,
            percentile(&latencies, 0.50),
            percentile(&latencies, 0.95),
            variant.why,
        );
    }

    let report = EvalReport {
        generated_by: "core/acowork-runtime/tests/global_search_e2e.rs".to_string(),
        scope: "conversation-live-model".to_string(),
        k: K,
        dataset: format!("{} queries", ds.queries.len()),
        corpus_rows: stack.rows,
        variants: reports,
        // Deliberately not `EvalReport::CAVEAT`: that one describes the
        // hashed-token stand-in. These numbers come from the real model, and the
        // limitation that travels with them is the corpus size.
        embedding_caveat: format!(
            "Embeddings are from the live model {} at {} dims — not a stand-in. The corpus is the {}-row \
             annotated conversation set: directional evidence for ADR-095 §9, not a release gate. \
             Queries with no conversation annotation are skipped by the shared tally, so false recall reads n/a here.",
            live.model_id,
            live.dim,
            stack.rows,
        ),
    };

    let body = per_query_section(&report);
    let (json_path, md_path) = report.write(&EvalDataset::dir(), &body);
    println!("{}\n{body}", report.render_summary());
    println!("\nwrote {} and {}", json_path.display(), md_path.display());

    // Structural invariants, asserted on the real model.
    let shipped = &variants()[1];
    assert_eq!(
        shipped.name, "shipped",
        "variant list order is load-bearing"
    );
    for q in ds.queries.iter().filter(|q| !q.out_of_scope(SCOPE)) {
        let embedding = live.provider.embed(&q.query).await.unwrap();
        let (keys, _) = stack.run(shipped, q, &embedding);
        // The lexical leg must never lose a literal, whatever the model does:
        // that is ADR-095 D6's exact-term retention claim, and it is the one
        // number here that does not depend on embedding quality.
        if !q.exact_terms.is_empty() {
            let texts: Vec<String> = keys
                .iter()
                .filter_map(|k| stack.content.get(k).cloned())
                .collect();
            for term in &q.exact_terms {
                assert!(
                    texts
                        .iter()
                        .any(|t| t.to_lowercase().contains(&term.to_lowercase())),
                    "exact term {term:?} for {} was not carried by any conversation hit: {keys:?}",
                    q.id
                );
            }
        }
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
    let stack = tier2_stack(&live).await;
    let ds = EvalDataset::load_repo();
    let policy = SearchPolicyConfig::default().conversation_policy();

    let mut rel: Vec<f64> = Vec::new();
    let mut irr: Vec<f64> = Vec::new();
    for q in ds.queries.iter().filter(|q| !q.out_of_scope(SCOPE)) {
        let gt = relevant_keys(q);
        if gt.is_empty() {
            continue;
        }
        let embedding = live.provider.embed(&q.query).await.unwrap();
        let terms =
            analyze(&q.query, &SearchPolicyConfig::default().analyzer_config()).lexical_terms;
        let lexical = LexicalQuery {
            raw: &q.query,
            terms: &terms,
        };
        // Deep pool so the tail is visible — that is where a floor would bite.
        for hit in stack
            .index
            .search_detailed(&lexical, Some(&embedding), 20, &policy)
        {
            // Only vector-sourced candidates report a cosine; a lexical-only hit
            // has none and is no evidence about the floor either way.
            let Some(c) = hit.cosine else { continue };
            let key = locate_key(&hit.session_id, hit.message_index);
            if gt.contains(&key) {
                rel.push(c);
            } else {
                irr.push(c);
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
            "min": round(s[0], 3),
            "p10": round(percentile(&s, 0.10), 3),
            "median": round(percentile(&s, 0.50), 3),
            "p90": round(percentile(&s, 0.90), 3),
            "max": round(s[s.len() - 1], 3),
        })
    };
    let out = serde_json::json!({
        "model": live.model_id,
        "scope": SCOPE,
        "policy": { "vector_weight": policy.vector_weight, "text_weight": policy.text_weight, "min_cosine": policy.min_cosine },
        "relevant_cosine": summary(&rel),
        "irrelevant_cosine": summary(&irr),
        "note": "Overlap between these two distributions is what a cosine floor would have to cut through. A floor above the relevant minimum rejects real hits.",
    });
    let dir = EvalDataset::dir().join("reports");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("conversation-live-cosine.json");
    std::fs::write(&path, serde_json::to_string_pretty(&out).unwrap()).expect("write cosine json");
    eprintln!(
        "cosine distribution: {}",
        serde_json::to_string(&out).unwrap()
    );
    eprintln!("report -> {}", path.display());
}

/// The scope-specific half of the markdown: the per-query table, plus the reason
/// each variant exists. Latency lives here and not in the shared renderer
/// because only a real-model run measures anything a latency number means —
/// the stand-in harnesses measure arithmetic, not a model.
fn per_query_section(r: &EvalReport) -> String {
    let mut s = String::from("\n## Per query\n\n");
    for v in &r.variants {
        s.push_str(&format!("### {}\n\n", v.variant));
        s.push_str("| id | lang | kind | relevant | retrieved | Recall@K | MRR | exact terms | analyzer terms |\n");
        s.push_str("|---|---|---|---|---|---|---|---|---|\n");
        for q in &v.per_query {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.3} | {:.3} | {} | {} |\n",
                q.id,
                q.lang,
                q.kind,
                q.relevant,
                q.retrieved,
                q.recall_at_k,
                q.mrr,
                kept_cell(q.exact_terms_kept),
                q.analyzer_terms
                    .map_or("n/a".to_string(), |n| n.to_string()),
            ));
        }
        s.push('\n');
    }
    s
}

fn round(x: f64, digits: u32) -> f64 {
    let f = 10f64.powi(digits as i32);
    (x * f).round() / f
}

/// Nearest-rank percentile on a copy. Local on purpose: one caller, and a
/// latency distribution is not a retrieval metric to be shared between scopes.
fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((v.len() - 1) as f64 * q).round() as usize;
    v[idx]
}
