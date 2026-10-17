//! `GET /search` — single-agent aggregate search (ADR-081 §4.1).
//!
//! Aggregates the agent's memory / file / git scopes in-process by
//! locking the existing usecase slots and running each requested scope
//! in parallel (`tokio::join!`). The conversation scope (P1-2, vector
//! conversation index) is not built yet — it reports
//! `{"status":"not_indexed"}` and yields no hits.
//!
//! The Gateway reverse-proxies `/api/agents/{id}/search` to this route
//! (ADR-009: agent-private data stays inside the Runtime; the Gateway
//! never touches the filesystem and holds no search business logic —
//! ADR-064/070).

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::http::server::HttpState;
use crate::usecases::git_query::GitLogParams;
use acowork_core::retrieval::LexicalQuery;

use crate::search::analyzer::AnalyzerMode;
use crate::usecases::memory_query::{RetrievalEvidence, SemanticMemoryQuery};
use crate::usecases::workspace_query::SearchFilesParams;

/// Query parameters for `GET /search` (ADR-081 §4.1).
#[derive(Debug, Deserialize)]
pub struct GlobalSearchParams {
    #[serde(default)]
    pub q: String,
    /// Comma-separated scope names: `conversation,memory,file,git`.
    #[serde(default)]
    pub scopes: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Semantic mode for the memory scope: `vector` | `hybrid` |
    /// `keyword`. Empty = `hybrid`.
    #[serde(default)]
    pub mode: String,
    /// Workspace id for the file / git scopes. When omitted those scopes
    /// are skipped (reported as `{"status":"skipped"}`) — the Desktop
    /// file/git tabs use their dedicated endpoints anyway.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// ADR-081: `all` accepted for future cross-agent search; a
    /// single-agent Runtime has no cross-agent scope, so it is ignored.
    #[serde(default)]
    pub agent: Option<String>,
}

fn default_limit() -> usize {
    20
}

/// One ranked hit in the aggregate response.
#[derive(Debug, serde::Serialize)]
pub struct SearchHit {
    pub scope: String,
    pub title: String,
    pub snippet: String,
    /// In-source ranking signal. Semantics depend on the scope:
    /// - `memory`, `conversation`: the real RRF fused score
    ///   (`Σ w / (60 + rank + 1)`) from the shared engine. It is a
    ///   **rank-position signal, not a similarity**: not comparable across
    ///   queries, not comparable to another scope's values, and it can repeat
    ///   for two hits (ADR-095 D4). Consumers must render the returned order,
    ///   never re-sort by this number.
    /// - `git`, `file`: synthetic `1.0 - i * 1e-4` — these scopes have no
    ///   fusion engine, so the value only encodes their own order.
    pub score: f64,
    /// Scope-specific payload (see each scope builder).
    pub payload: Value,
}

/// Response body for `GET /search`.
#[derive(Debug, serde::Serialize)]
pub struct GlobalSearchResponse {
    pub hits: Vec<SearchHit>,
    /// Per-scope metadata: `{"memory": {"status","count"}, ...}`, plus a
    /// `search_meta` block describing how the query was processed (analyzer
    /// mode, term count, embedding availability) — the observability half of
    /// ADR-095 D2/D6: without it, a retrieval regression is invisible.
    pub scopes: Value,
    /// True while the (P1-2) conversation index is still being built.
    pub indexing: bool,
}

/// Embed `q` with the agent's live embedding provider (ADR-081 memory
/// scope). Returns `None` when no provider is bound or embedding fails —
/// the memory scope then falls back to BM25 text search.
pub(crate) async fn embed_query(state: &HttpState, q: &str) -> Option<Vec<f32>> {
    let provider: Option<Arc<dyn crate::embedding::EmbeddingProvider>> = {
        let guard = state.agent_core.read().ok();
        guard
            .as_ref()
            .and_then(|g| g.as_ref())
            .and_then(|c| c.embedding_provider.clone())
    };
    match provider {
        Some(provider) => match provider.embed(q).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(error = %e, "global search: embedding failed, falling back to BM25");
                None
            }
        },
        None => None,
    }
}

/// The agent's retrieval policy for this request (ADR-095 D6).
///
/// Read from `agent_config.json` per request rather than cached, because the
/// config is user-editable at runtime (`PUT /agents/{id}/config`) and a stale
/// policy would make the `analyzer_mode` kill switch unreliable. A missing
/// service or unreadable config falls back to `Default`, which is
/// `analyzer_mode = off` — the pre-ADR-095 behavior, so a config failure can
/// never *enable* an unvalidated path.
async fn resolve_search_policy(state: &HttpState) -> crate::search::policy::SearchPolicyConfig {
    let svc = {
        let guard = state.agent_config.lock().await;
        guard.clone()
    };
    let Some(svc) = svc else {
        return Default::default();
    };
    let resp = svc.get_config(&state.agent_id).await;
    // Absent config section ⇒ `Default`, which is `analyzer_mode = off`.
    resp.config.and_then(|c| c.search).unwrap_or_default()
}

/// Set/order difference between two ranked id lists.
///
/// Ids and counts only — never content. Shadow logging must not become a
/// channel for copying user data into the logs (ADR-095 §6), which is why this
/// takes `u64` ids rather than hits.
#[derive(Debug, Default, PartialEq, Eq)]
struct RetrievalSetDiff {
    common: usize,
    only_production: usize,
    only_shadow: usize,
    /// True when the shared ids appear in a different relative order, i.e. the
    /// analyzer changed *ranking* and not merely the result set.
    order_changed: bool,
}

fn diff_retrieval_sets(first: &[u64], second: &[u64]) -> RetrievalSetDiff {
    use std::collections::HashSet;
    let s2: HashSet<u64> = second.iter().copied().collect();
    let s1: HashSet<u64> = first.iter().copied().collect();
    let common: Vec<u64> = first.iter().copied().filter(|id| s2.contains(id)).collect();
    // Compare the shared ids' relative order, projected out of each list.
    let order_in = |list: &[u64]| -> Vec<u64> {
        list.iter().copied().filter(|id| s1.contains(id) && s2.contains(id)).collect()
    };
    RetrievalSetDiff {
        common: common.len(),
        only_production: first.iter().filter(|id| !s2.contains(id)).count(),
        only_shadow: second.iter().filter(|id| !s1.contains(id)).count(),
        order_changed: order_in(first) != order_in(second),
    }
}

/// Build `git`-scope hits with CommitPicker-identical semantics
/// (subject/author/short_hash/hash case-insensitive substring), applied
/// server-side as the ADR-081 sanctioned pre-filter. Pseudo-score keeps
/// newest-first order from `git log`.
fn git_scope_hits(commits: Vec<crate::usecases::git_query::GitCommitDto>, q: &str, limit: usize) -> Vec<SearchHit> {
    let lower = q.to_lowercase();
    commits
        .into_iter()
        .filter(|c| {
            c.subject.to_lowercase().contains(&lower)
                || c.author.to_lowercase().contains(&lower)
                || c.short_hash.contains(q)
                || c.hash.contains(q)
        })
        .take(limit)
        .enumerate()
        .map(|(i, c)| SearchHit {
            scope: "git".to_string(),
            title: c.subject.clone(),
            snippet: format!("{} · {}", c.author, c.date),
            score: 1.0 - (i as f64) * 1e-4,
            payload: json!({
                "hash": c.hash,
                "short_hash": c.short_hash,
                "author": c.author,
                "date": c.date,
            }),
        })
        .collect()
}

/// Map `ConversationIndex::search` hits to aggregate `SearchHit`s.
/// Each hit becomes one row; the displayed title is the session title
/// from the session-meta store (ADR-082 §4 step 3: SQLite) when one
/// exists, falling back to the raw session id. The payload always carries
/// the raw `session_id` + `#message_index` for locate (ADR-081 §4.2).
fn conversation_hits_to_search_hits(
    hits: Vec<crate::conversation_index::ConversationHit>,
    titles: &std::collections::HashMap<String, Option<String>>,
) -> Vec<SearchHit> {
    hits.into_iter()
        .map(|h| {
            let title = titles
                .get(&h.session_id)
                .and_then(|t| t.clone())
                .unwrap_or_else(|| h.session_id.clone());
            // Locate keys first, then the retrieval evidence merged in. A
            // conversation hit is only useful if the UI can jump to it, so the
            // payload must keep its shape even when signals are absent.
            let mut payload = json!({
                "session_id": h.session_id,
                "message_index": h.message_index,
                "role": h.role,
            });
            let evidence = h.matched_by.map(|m| RetrievalEvidence {
                matched_by: m,
                vector_rank: h.vector_rank,
                text_rank: h.text_rank,
                cosine: h.cosine,
                fused_score: h.score,
                fused_rank: h.fused_rank,
            });
            if let (Some(map), Some(ev)) = (payload.as_object_mut(), evidence.as_ref()) {
                ev.merge_into(map);
            }
            SearchHit {
                scope: "conversation".to_string(),
                title,
                snippet: h.content.clone(),
                score: h.score,
                payload,
            }
        })
        .collect()
}

/// Resolve `session_id → title` from the session-meta store (ADR-082 §4
/// step 3: SQLite; blocking reads off the async runtime). Missing rows map
/// to `None` — the caller falls back to the raw id.
async fn resolve_session_titles(
    conversations_dir: &std::path::Path,
    session_ids: &[String],
) -> std::collections::HashMap<String, Option<String>> {
    let mut titles = std::collections::HashMap::with_capacity(session_ids.len());
    for sid in session_ids {
        let dir = conversations_dir.to_path_buf();
        let sid_key = sid.clone();
        let title = tokio::task::spawn_blocking(move || {
            crate::conversation::read_session_meta(&dir, &sid_key)
                .ok()
                .and_then(|m| m.title)
        })
        .await
        .unwrap_or(None);
        titles.insert(sid.clone(), title);
    }
    titles
}

/// `GET /search` handler.
pub(crate) async fn global_search(
    State(state): State<HttpState>,
    Query(params): Query<GlobalSearchParams>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let q = params.q.trim().to_string();
    let limit = params.limit.clamp(1, 50);
    let scopes: Vec<String> = params
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let empty = || {
        Json(json!({
            "hits": [],
            "scopes": {},
            "indexing": false,
        }))
    };
    if q.is_empty() || scopes.is_empty() {
        return Ok(empty());
    }

    let want_memory = scopes.iter().any(|s| s == "memory");
    let want_file = scopes.iter().any(|s| s == "file");
    let want_git = scopes.iter().any(|s| s == "git");
    let want_conversation = scopes.iter().any(|s| s == "conversation");

    // Clone service handles out of their slots so the guards are dropped
    // before the parallel await points below (also makes the futures Send).
    let memory_svc = if want_memory {
        state.memory_query.lock().await.clone()
    } else {
        None
    };
    let file_svc = if want_file {
        state.workspace_query.lock().await.clone()
    } else {
        None
    };
    let git_svc = if want_git {
        state.git_query.lock().await.clone()
    } else {
        None
    };
    let conversation_index = if want_conversation {
        state.conversation_index.read().ok().and_then(|g| g.clone())
    } else {
        None
    };
    // ── Query analysis (ADR-095 D2) ─────────────────────────────────
    // One analyzer pass serves every scope: the same raw text is embedded for
    // the vector source and the same analyzed terms drive FTS. Doing this per
    // scope would let memory and conversation disagree about what the user
    // asked for.
    let policy = resolve_search_policy(&state).await;
    let memory_policy = policy.memory_policy();
    let conversation_policy = policy.conversation_policy();
    let analyzer_mode = policy.analyzer_mode();
    let analyzer_cfg = policy.analyzer_config();
    let dual = crate::search::analyzer::analyze(&q, &analyzer_cfg);

    // `shadow` keeps production on the old single-query path while still
    // measuring what the analyzer *would* have produced, so a regression is
    // visible before it is live (§9 rollout). The difference is logged as
    // counts and ids only — never query text or content (privacy, §6).
    let lexical_terms: &[String] = match analyzer_mode {
        AnalyzerMode::On => &dual.lexical_terms,
        AnalyzerMode::Off | AnalyzerMode::Shadow => &[],
    };
    if analyzer_mode == AnalyzerMode::Shadow {
        tracing::debug!(
            analyzer_terms = dual.lexical_terms.len(),
            raw_len = q.chars().count(),
            "global search: shadow analyzer ran; production still uses the raw query"
        );
    }

    let analyzer_meta = json!({
        "analyzer_mode": analyzer_mode.as_str(),
        "lexical_term_count": lexical_terms.len(),
    });

    let qq = q.clone();
    let mode = params.mode.clone();
    let lexical_memory = lexical_terms.to_vec();
    // `HttpState` is `Clone` and every field is an `Arc`/handle, so each
    // `async move` future takes its own copy rather than borrowing across the
    // join (the futures run concurrently and cannot share a borrow).
    let state_mem = state.clone();
    // Shadow runs the analyzed path *additionally*; production still uses raw.
    let dual_memory_terms = dual.lexical_terms.clone();
    let analyzer_mode_m = analyzer_mode;
    let memory_fut = async move {
        let state = &state_mem;
        let Some(svc) = memory_svc else {
            return (json!({"status": "unavailable"}), vec![], false);
        };
        let embedding = embed_query(state, &qq).await;
        let embedded = embedding.is_some();
        let semantic_mode = if mode.is_empty() { "hybrid" } else { mode.as_str() };
        let query = SemanticMemoryQuery {
            query_text: qq,
            mode: semantic_mode.to_string(),
            limit,
            embedding,
            lexical_terms: lexical_memory,
        };
        match svc.semantic_search_detailed(&query, &memory_policy).await {
            Ok(nodes) => {
                // Collected before `nodes` is consumed by the map below.
                let production_node_ids: Vec<u64> = nodes.iter().map(|s| s.node.node_id).collect();
                let hits: Vec<SearchHit> = nodes
                    .into_iter()
                    .map(|scored| {
                        let node = scored.node;
                        let ev = scored.evidence;
                        // Locate keys first, then evidence merged in — the same
                        // shape the conversation scope builds. Assigning the
                        // evidence object as the whole payload dropped
                        // `node_id`, and the preview panel then fetched node 0
                        // (GlobalSearchDialog maps a missing node_id to 0).
                        let mut payload = json!({
                            "node_id": node.node_id,
                            "node_type": node.node_type,
                            "sub_type": node.sub_type,
                        });
                        if let (Some(map), Some(e)) = (payload.as_object_mut(), ev.as_ref()) {
                            e.merge_into(map);
                        }
                        SearchHit {
                            scope: "memory".to_string(),
                            title: node.node_type.clone(),
                            snippet: node.content.clone(),
                            // Real fused score from the shared engine. The
                            // synthetic 1.0-i*1e-4 this replaces was an order
                            // index wearing a float's clothes (D4).
                            score: ev.as_ref().map_or(0.0, |e| e.fused_score),
                            payload,
                        }
                    })
                    .collect();
                let count = hits.len();
                // Shadow rollout (design 28 §4.2): production answered with the
                // raw query, so re-run the *analyzed* path here and log how the
                // two differ. This is the only way to decide whether `on` is
                // safe, and it must be measured on live traffic rather than the
                // eval fixture.
                if analyzer_mode_m == AnalyzerMode::Shadow {
                    let shadow_query = SemanticMemoryQuery {
                        query_text: query.query_text.clone(),
                        mode: query.mode.clone(),
                        limit: query.limit,
                        embedding: query.embedding.clone(),
                        lexical_terms: dual_memory_terms.clone(),
                    };
                    if let Ok(shadow) = svc.semantic_search_detailed(&shadow_query, &memory_policy).await {
                        let production_ids: Vec<u64> = production_node_ids;
                        let shadow_ids: Vec<u64> = shadow.iter().map(|s| s.node.node_id).collect();
                        let diff = diff_retrieval_sets(&production_ids, &shadow_ids);
                        tracing::info!(
                            scope = "memory",
                            production = production_ids.len(),
                            shadow = shadow_ids.len(),
                            only_production = diff.only_production,
                            only_shadow = diff.only_shadow,
                            common = diff.common,
                            order_changed = diff.order_changed,
                            "global search: shadow retrieval comparison"
                        );
                    }
                }
                (json!({"status": "ok", "count": count}), hits, embedded)
            }
            Err(e) => (json!({"status": "error", "message": e.to_string()}), vec![], embedded),
        }
    };

    let qq = q.clone();
    let workspace_id = params.workspace_id.clone();
    let file_fut = async {
        let Some(svc) = file_svc else {
            return (json!({"status": "unavailable"}), vec![]);
        };
        let Some(wid) = workspace_id.as_deref() else {
            return (json!({"status": "skipped", "message": "workspace_id required"}), vec![]);
        };
        let search_params = SearchFilesParams {
            q: Some(qq),
            workspace_id: Some(wid.to_string()),
            include: None,
            max_results: Some(limit),
            case_sensitive: false,
            whole_word: false,
        };
        match svc.search_files(&search_params).await {
            Ok(resp) => {
                let hits: Vec<SearchHit> = resp
                    .matches
                    .into_iter()
                    .enumerate()
                    .map(|(i, m)| SearchHit {
                        scope: "file".to_string(),
                        title: m.file.clone(),
                        snippet: m.text.clone(),
                        score: 1.0 - (i as f64) * 1e-4,
                        payload: json!({
                            "file": m.file,
                            "line": m.line,
                            "column": m.column,
                        }),
                    })
                    .collect();
                (
                    json!({"status": "ok", "count": resp.total_matches, "truncated": resp.truncated}),
                    hits,
                )
            }
            Err(e) => (json!({"status": "error", "message": e.to_string()}), vec![]),
        }
    };

    let qq = q.clone();
    let workspace_id = params.workspace_id.clone();
    let git_fut = async {
        let Some(svc) = git_svc else {
            return (json!({"status": "unavailable"}), vec![]);
        };
        let Some(wid) = workspace_id.as_deref() else {
            return (json!({"status": "skipped", "message": "workspace_id required"}), vec![]);
        };
        let log_params = GitLogParams {
            workspace_id: Some(wid.to_string()),
            path: None,
            limit: Some(100),
            skip: None,
        };
        match svc.log(&log_params).await {
            Ok(resp) => {
                let hits = git_scope_hits(resp.commits, &qq, limit);
                (json!({"status": "ok", "count": hits.len()}), hits)
            }
            Err(e) => (json!({"status": "error", "message": e.to_string()}), vec![]),
        }
    };

    let qq = q.clone();
    let lexical_conversation = lexical_terms.to_vec();
    let state_conv = state.clone();
    let conversation_index_c = conversation_index.clone();
    let conversation_fut = async move {
        let state = &state_conv;
        let Some(index) = conversation_index_c.as_ref() else {
            return (json!({"status": "not_indexed"}), vec![], false);
        };
        let embedding = embed_query(state, &qq).await;
        let embedded = embedding.is_some();
        let lexical = LexicalQuery {
            raw: &qq,
            terms: &lexical_conversation,
        };
        let hits = index.search_detailed(&lexical, embedding.as_deref(), limit, &conversation_policy);
        // Session titles for display (dedup by session; meta reads off-runtime).
        let mut session_ids: Vec<String> = Vec::new();
        for h in &hits {
            if !session_ids.contains(&h.session_id) {
                session_ids.push(h.session_id.clone());
            }
        }
        let titles = resolve_session_titles(index.conversations_dir(), &session_ids).await;
        let hits = conversation_hits_to_search_hits(hits, &titles);
        (json!({"status": "ok", "count": hits.len()}), hits, embedded)
    };

    let (
        (memory_meta, memory_hits, memory_embedded),
        (file_meta, file_hits),
        (git_meta, git_hits),
        (conversation_meta, conversation_hits, conversation_embedded),
    ) = tokio::join!(memory_fut, file_fut, git_fut, conversation_fut);

    // Presentation order, deliberately **not** a score sort (ADR-095 D4).
    //
    // A fused score is an in-source rank signal: comparing a memory hit's
    // 0.016 against a file hit's synthetic 0.9999 would rank two unrelated
    // number systems against each other and silently bury the scopes that
    // report honest numbers. Each scope therefore keeps its own engine order
    // (`fused_rank`), and the scopes are concatenated in a fixed priority.
    // Consumers render this order as-is.
    let mut hits: Vec<SearchHit> = Vec::new();
    hits.extend(memory_hits);
    hits.extend(conversation_hits);
    hits.extend(file_hits);
    hits.extend(git_hits);

    let indexing = conversation_index
        .as_ref()
        .map(|i| i.is_indexing())
        .unwrap_or(false);

    let mut scopes_map = Map::new();
    if want_conversation {
        scopes_map.insert("conversation".to_string(), conversation_meta);
    }
    if want_memory {
        scopes_map.insert("memory".to_string(), memory_meta);
    }
    if want_file {
        scopes_map.insert("file".to_string(), file_meta);
    }
    if want_git {
        scopes_map.insert("git".to_string(), git_meta);
    }

    // `embedding_available` per scope: a vector source that never ran is the
    // single most common cause of "search got worse" reports, and it is
    // otherwise indistinguishable from a source that ran and found nothing.
    let search_meta = json!({
        "analyzer_mode": analyzer_meta["analyzer_mode"],
        "lexical_term_count": analyzer_meta["lexical_term_count"],
        "memory": { "embedded": memory_embedded },
        "conversation": { "embedded": conversation_embedded },
    });

    Ok(Json(json!({
        "hits": hits,
        "scopes": scopes_map,
        "indexing": indexing,
        "search_meta": search_meta,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_index::ConversationHit;

    use crate::usecases::git_query::GitCommitDto;

    use super::{conversation_hits_to_search_hits, git_scope_hits};

    /// A conversation hit from the legacy single-path `search()`: no fusion, so
    /// no per-source signals at all.
    fn legacy_hit(
        session_id: &str,
        message_index: usize,
        role: &str,
        content: &str,
        score: f64,
    ) -> ConversationHit {
        ConversationHit {
            session_id: session_id.to_string(),
            message_index,
            role: role.to_string(),
            content: content.to_string(),
            score,
            matched_by: None,
            vector_rank: None,
            text_rank: None,
            cosine: None,
            fused_rank: None,
        }
    }

    /// A hit straight out of `search_detailed`, carrying its evidence.
    fn detailed_hit(
        session_id: &str,
        message_index: usize,
        score: f64,
        matched_by: acowork_core::retrieval::MatchedBy,
        vector_rank: Option<usize>,
        text_rank: Option<usize>,
        fused_rank: usize,
    ) -> ConversationHit {
        ConversationHit {
            session_id: session_id.to_string(),
            message_index,
            role: "user".to_string(),
            content: "x".to_string(),
            score,
            matched_by: Some(matched_by),
            vector_rank,
            text_rank,
            cosine: vector_rank.map(|_| 0.42),
            fused_rank: Some(fused_rank),
        }
    }

    fn commit(subject: &str, author: &str, short: &str, hash: &str) -> GitCommitDto {
        GitCommitDto {
            hash: hash.to_string(),
            short_hash: short.to_string(),
            author: author.to_string(),
            date: "2025-01-01T00:00:00Z".to_string(),
            subject: subject.to_string(),
        }
    }

    #[test]
    fn git_scope_matches_any_of_four_fields_case_insensitively() {
        let commits = vec![
            commit("feat: add global search", "Alice", "a1b2c3", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            commit("fix: dark mode toggle", "Bob", "d4e5f6", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            commit("refactor: pm search", "Carol", "g7h8i9", "cccccccccccccccccccccccccccccccccccccccc"),
        ];
        // subject substring (case-insensitive)
        let hits = git_scope_hits(commits.clone(), "GLOBAL SEARCH", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload["short_hash"], "a1b2c3");
        // author substring
        assert_eq!(git_scope_hits(commits.clone(), "carol", 10).len(), 1);
        // short-hash substring
        assert_eq!(git_scope_hits(commits.clone(), "b2c", 10).len(), 1);
        // hash substring
        assert_eq!(git_scope_hits(commits.clone(), "bbbb", 10).len(), 1);
        // no match
        assert!(git_scope_hits(commits, "zebra", 10).is_empty());
    }

    #[test]
    fn git_scope_respects_limit() {
        let commits: Vec<GitCommitDto> = (0..5)
            .map(|i| commit(&format!("commit {i}"), "Alice", &format!("h{i}"), &format!("f{i}")))
            .collect();
        let hits = git_scope_hits(commits, "commit", 3);
        assert_eq!(hits.len(), 3);
        // Newest first preserved (commit 0 first in the seeded order).
        assert_eq!(hits[0].title, "commit 0");
    }

    #[test]
    fn shadow_diff_reports_the_added_and_dropped_ids() {
        let production = vec![1u64, 2, 3];
        let shadow = vec![1u64, 3, 4];
        let diff = diff_retrieval_sets(&production, &shadow);
        assert_eq!(diff.common, 2);
        assert_eq!(diff.only_production, 1); // id 2
        assert_eq!(diff.only_shadow, 1); // id 4
        // Shared ids keep the same relative order (1 before 3) in both.
        assert!(!diff.order_changed);
    }

    #[test]
    fn shadow_diff_detects_reordering_of_the_shared_ids() {
        // Same set, different ranking: the analyzer changed *order*, which is the
        // subtler shadow signal — a set-only comparison would report no change.
        let production = vec![1u64, 2, 3];
        let shadow = vec![3, 2, 1];
        let diff = diff_retrieval_sets(&production, &shadow);
        assert_eq!(diff.common, 3);
        assert_eq!(diff.only_production, 0);
        assert_eq!(diff.only_shadow, 0);
        assert!(diff.order_changed);
    }

    #[test]
    fn shadow_diff_of_identical_lists_reports_no_change() {
        let a = vec![5u64, 6, 7];
        let diff = diff_retrieval_sets(&a, &a);
        assert_eq!(diff.only_production, 0);
        assert_eq!(diff.only_shadow, 0);
        assert!(!diff.order_changed);
        assert_eq!(diff.common, 3);
    }

    #[test]
    fn shadow_diff_handles_an_empty_side() {
        // The analyzed path yielding nothing is the fallback case; it must read as
        // "production had N, shadow had 0", not as an error.
        let diff = diff_retrieval_sets(&[1, 2, 3], &[]);
        assert_eq!(diff.only_production, 3);
        assert_eq!(diff.only_shadow, 0);
        assert_eq!(diff.common, 0);
        assert!(!diff.order_changed);
    }

    #[test]
    fn conversation_hits_carry_session_locate_payload() {
        let mut titles = std::collections::HashMap::new();
        titles.insert("s-abc".to_string(), Some("rust borrow deep-dive".to_string()));
        let hits = conversation_hits_to_search_hits(
            vec![
                legacy_hit("s-abc", 42, "assistant", "borrow checker rules", 0.91),
                legacy_hit("s-abc", 41, "user", "what is lifetime elision", 0.8),
                legacy_hit("s-unknown", 3, "user", "no meta file", 0.5),
            ],
            &titles,
        );
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].scope, "conversation");
        // Session title wins for display; raw id stays in the payload.
        assert_eq!(hits[0].title, "rust borrow deep-dive");
        assert_eq!(hits[0].snippet, "borrow checker rules");
        assert_eq!(hits[0].score, 0.91);
        assert_eq!(hits[0].payload["session_id"], "s-abc");
        assert_eq!(hits[0].payload["message_index"], 42);
        assert_eq!(hits[0].payload["role"], "assistant");
        assert_eq!(hits[1].payload["message_index"], 41);
        // Missing meta falls back to the raw session id.
        assert_eq!(hits[2].title, "s-unknown");
        // Legacy path: no evidence keys at all — not `null`, not a guess.
        assert!(hits[0].payload.get("matched_by").is_none());
        assert!(hits[0].payload.get("fused_rank").is_none());
    }

    #[test]
    fn conversation_hits_carry_retrieval_evidence_in_returned_order() {
        use acowork_core::retrieval::MatchedBy;
        let mut titles = std::collections::HashMap::new();
        titles.insert("s-1".to_string(), Some("first".to_string()));
        let hits = conversation_hits_to_search_hits(
            vec![
                detailed_hit(
                    "s-1",
                    7,
                    0.0163,
                    MatchedBy::Both,
                    Some(2),
                    Some(1),
                    0,
                ),
                detailed_hit("s-1", 9, 0.0161, MatchedBy::Vector, Some(5), None, 1),
            ],
            &titles,
        );
        // matched_by is the snake_case wire name the Desktop DTO reads.
        assert_eq!(hits[0].payload["matched_by"], "both");
        assert_eq!(hits[0].payload["vector_rank"], 2);
        assert_eq!(hits[0].payload["text_rank"], 1);
        assert_eq!(hits[0].payload["fused_rank"], 0);
        // A source-only hit omits the rank it does not have.
        assert_eq!(hits[1].payload["matched_by"], "vector");
        assert!(hits[1].payload.get("text_rank").is_none());
    }

    #[test]
    fn retrieval_payload_never_leaks_cosine_or_raw_score() {
        use acowork_core::retrieval::MatchedBy;
        // Design 28 §4.2 keeps cosine out of the list payload: an uncalibrated
        // cosine rendered as a confidence percentage is a false claim. This
        // asserts the whitelist rather than the field list, so adding a field
        // to RetrievalEvidence cannot publish it by accident.
        let ev = RetrievalEvidence {
            matched_by: MatchedBy::Vector,
            vector_rank: Some(3),
            text_rank: None,
            cosine: Some(0.87),
            fused_score: 0.0123,
            fused_rank: Some(1),
        };
        let payload = ev.to_payload();
        assert_eq!(payload["matched_by"], "vector");
        assert_eq!(payload["fused_rank"], 1);
        assert!(payload.get("cosine").is_none());
        assert!(payload.get("fused_score").is_none());
    }
}
