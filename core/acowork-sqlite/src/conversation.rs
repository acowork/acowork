//! Conversation index storage on SQLite (ADR-082 §4 step 2, ADR-081).
//!
//! The grafeo implementation kept one `ConversationMessage` node per indexed
//! JSONL line and rebuilt its per-session watermark by scanning *every* node on
//! open (`nodes_by_label` + a `get_node` per id). The data model is unchanged —
//! same label, same props, embedding in the shared `vectors` table — but the
//! watermark becomes one indexed `MAX(message_index)` aggregate per session and
//! the boot-time node scan disappears, which is the point of the migration:
//! `open` no longer scales with the number of indexed messages.
//!
//! `props` holds `{session_id, message_index, role, content}` and the FTS blob
//! holds the truncated content, matching the grafeo node property set exactly so
//! an existing index can be re-read field for field.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use acowork_core::retrieval::{FusedHit, FusionPolicy, LexicalQuery, MatchedBy};

use crate::{Result, SqliteStore};

/// Label for every indexed conversation-message node (unchanged from grafeo).
pub const LABEL: &str = "ConversationMessage";

/// File name of the conversation index database.
///
/// Backend-specific on purpose: a SQLite file must never be handed to grafeo or
/// vice versa.
pub const STORE_FILE: &str = "conversation_index.sqlite";

/// Content is truncated to this many chars before embedding, bounding embedding
/// cost for very long messages (the stored snippet is the truncated form,
/// matching what was embedded).
pub const MAX_INDEX_CONTENT: usize = 4_000;

/// Cosine floor applied to the vector source of a hybrid conversation search
/// (ADR-082 D5). Kept as a named constant for callers that document the
/// behaviour; the *effective* value lives in
/// [`FusionPolicy::CONVERSATION_BASELINE`] so the policy and this note cannot
/// drift apart. Below the floor an "orthogonal" neighbour is noise, not a hit
/// — but the floor gates the vector source *only*, so a keyword hit whose
/// embedding is far still comes back (ADR-095 D3).
pub const DEFAULT_MIN_COSINE: f32 = 0.3;

// The named constant above is documentation; the constant below is behaviour.
// If they ever disagree, the conversation scope silently changes its gate.
const _: () = assert!(
    matches!(
        FusionPolicy::CONVERSATION_BASELINE.min_cosine,
        Some(v) if v == DEFAULT_MIN_COSINE
    ),
    "DEFAULT_MIN_COSINE and FusionPolicy::CONVERSATION_BASELINE.min_cosine drifted"
);

/// One ranked conversation hit.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationHit {
    pub session_id: String,
    /// JSONL line number — also the session message index.
    pub message_index: usize,
    pub role: String,
    pub content: String,
    /// RRF fused score (`Σ w / (60 + rank + 1)`). Larger = more relevant.
    /// Rank-position signal only; do not threshold on its absolute value.
    pub score: f64,
}

/// The stored shape of one indexed message.
#[derive(Debug, Serialize, Deserialize)]
struct MessageProps {
    session_id: String,
    message_index: i64,
    role: String,
    content: String,
}

/// The conversation vector index + per-session watermark.
///
/// Holds the [`SqliteStore`] behind an `Arc` so the memory provider, the
/// session-meta store and this index can share one connection (and one
/// write lock) when they live in the same file — ADR-082 §4 step 3's
/// "one `.sqlite` file per workspace".
pub struct ConversationStore {
    store: Arc<SqliteStore>,
    /// Per-session watermark: next JSONL line index to index.
    ///
    /// Not persisted: it is recovered from the indexed rows at `open`, which is
    /// what the grafeo version did too. A session whose lines are all filtered
    /// out by `is_indexable` therefore re-scans from 0 after a restart — wasted
    /// work, never a duplicate, since the filter is deterministic.
    watermark: Mutex<HashMap<String, usize>>,
}

impl ConversationStore {
    /// Open (or create) the conversation index at `path`, sized for
    /// `embedding_dim` (the live provider's dimension — hardcoding 384 made
    /// every vector write mismatch a 512-dim provider).
    pub fn open(path: impl AsRef<Path>, embedding_dim: usize) -> Result<Self> {
        let store = Arc::new(SqliteStore::open(path, embedding_dim)?);
        Self::from_store(store)
    }

    /// In-memory variant, for tests.
    pub fn open_in_memory(embedding_dim: usize) -> Result<Self> {
        let store = Arc::new(SqliteStore::open_in_memory(embedding_dim)?);
        Self::from_store(store)
    }

    /// Build the index on an already-open [`SqliteStore`], so memory, session
    /// meta and the conversation index share one connection and one file
    /// (ADR-082 §4 step 3).
    pub fn from_store(store: Arc<SqliteStore>) -> Result<Self> {
        let this = Self {
            store,
            watermark: Mutex::new(HashMap::new()),
        };
        this.recover_watermarks()?;
        Ok(this)
    }

    /// Rebuild `watermark` from the indexed rows and purge rows that cannot be
    /// searched (no identifying props) or that duplicate an earlier line of the
    /// same session. Called once from `open`.
    fn recover_watermarks(&self) -> Result<()> {
        let conn = self.store.lock();
        // Nodes without the identifying props are unsearchable garbage; a
        // duplicate (session_id, message_index) can only come from an earlier
        // crash between insert and watermark advance. Both are deleted by id so
        // the FTS entry for the row goes with them.
        let junk: Vec<i64> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM nodes WHERE label = ?1 AND ( \
                     json_extract(props, '$.session_id') IS NULL \
                     OR json_extract(props, '$.message_index') IS NULL \
                     OR id NOT IN ( \
                         SELECT MIN(id) FROM nodes WHERE label = ?1 \
                         GROUP BY json_extract(props, '$.session_id'), \
                                  json_extract(props, '$.message_index')))",
            )?;
            stmt.query_map([LABEL], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<_, _>>()?
        };

        let mut watermark: HashMap<String, usize> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT json_extract(props, '$.session_id'), \
                        MAX(json_extract(props, '$.message_index')) \
                 FROM nodes WHERE label = ?1 GROUP BY 1",
            )?;
            let rows = stmt.query_map([LABEL], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (session_id, max_line) = row?;
                watermark.insert(session_id, (max_line.max(0) as usize) + 1);
            }
        }
        drop(conn);

        let mut purged = 0usize;
        for id in junk {
            if self.store.delete_node(id as u64).unwrap_or(false) {
                purged += 1;
            }
        }
        let sessions = watermark.len();
        *self.watermark.lock().unwrap() = watermark;
        if purged > 0 {
            tracing::info!(sessions, purged, "conversation index: watermarks recovered");
        }
        Ok(())
    }

    /// Embedding dimension this index was opened with.
    pub fn embedding_dim(&self) -> usize {
        self.store.embedding_dim()
    }

    /// Record a new vector width, for an empty index.
    ///
    /// The store hands out the dimension it was created with and ignores the
    /// one a caller passes, so adopting a provider's width has to be explicit.
    /// Only meaningful while there is nothing stored: existing vectors keep the
    /// width they were written with.
    pub fn set_embedding_dim(&self, dim: usize) -> Result<()> {
        self.store.set_embedding_dim(dim)
    }

    /// Flush pending writes and release the file lock.
    pub fn close(&self) -> Result<()> {
        self.store.close()
    }

    /// Number of indexed messages.
    pub fn message_count(&self) -> Result<u64> {
        self.store.node_count_by_label(LABEL)
    }

    /// Number of sessions with at least one indexed message.
    pub fn session_count(&self) -> Result<usize> {
        Ok(self.watermark.lock().unwrap().len())
    }

    /// Every session with a known watermark (i.e. one present in the index).
    pub fn sessions(&self) -> Vec<String> {
        self.watermark.lock().unwrap().keys().cloned().collect()
    }

    /// Index one message and advance this session's watermark past it.
    pub fn index_message(
        &self,
        session_id: &str,
        message_index: usize,
        role: &str,
        content: &str,
        embedding: &[f32],
    ) -> Result<u64> {
        let truncated: String = content.chars().take(MAX_INDEX_CONTENT).collect();
        let props = serde_json::to_string(&MessageProps {
            session_id: session_id.to_string(),
            message_index: message_index as i64,
            role: role.to_string(),
            content: truncated.clone(),
        })?;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let id = self.store.insert_row(
            LABEL,
            "Active",
            &props,
            &truncated,
            &now,
            &now,
            Some(embedding),
        )?;
        self.mark_indexed(session_id, message_index + 1);
        Ok(id)
    }

    /// Next JSONL line to index for `session_id` (0 = not started).
    pub fn next_line(&self, session_id: &str) -> usize {
        self.watermark
            .lock()
            .unwrap()
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    /// Record that lines `< next_line` are indexed for `session_id`.
    pub fn mark_indexed(&self, session_id: &str, next_line: usize) {
        self.watermark
            .lock()
            .unwrap()
            .insert(session_id.to_string(), next_line);
    }

    /// Remove every indexed message of a (deleted) session.
    ///
    /// The watermark goes with it: a session that comes back gets re-indexed
    /// from the start of its (new) JSONL, which is what the grafeo version did.
    pub fn remove_session(&self, session_id: &str) -> Result<usize> {
        let ids = self.session_message_ids(session_id)?;
        let mut removed = 0usize;
        for id in ids {
            if self.store.delete_node(id)? {
                removed += 1;
            }
        }
        self.watermark.lock().unwrap().remove(session_id);
        Ok(removed)
    }

    /// Drop the whole index. The indexer re-runs from watermark 0 afterwards.
    pub fn rebuild(&self) -> Result<usize> {
        let ids: Vec<i64> = {
            let conn = self.store.lock();
            let mut stmt = conn.prepare("SELECT id FROM nodes WHERE label = ?1")?;
            stmt.query_map([LABEL], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        for id in &ids {
            self.store.delete_node(*id as u64)?;
        }
        self.watermark.lock().unwrap().clear();
        Ok(ids.len())
    }

    /// Rank indexed messages against `query_text`, optionally fused with a
    /// vector search (ADR-082 D5: per-source gating, rank fusion).
    ///
    /// A missing embedding degrades to text-only rather than failing: the
    /// embedding provider is a remote service and is not always reachable.
    ///
    /// Compatibility wrapper over [`Self::search_detailed`] with today's
    /// production policy ([`FusionPolicy::CONVERSATION_BASELINE`]).
    pub fn search(
        &self,
        query_text: &str,
        embedding: Option<&[f32]>,
        k: usize,
    ) -> Result<Vec<ConversationHit>> {
        Ok(self
            .search_detailed(
                &LexicalQuery::raw_only(query_text),
                embedding,
                k,
                &FusionPolicy::CONVERSATION_BASELINE,
            )?
            .into_iter()
            .map(|(hit, _)| hit)
            .collect())
    }

    /// Conversation retrieval with the per-source signals ADR-095 D4 requires.
    ///
    /// Shares the one fusion engine with the memory paths
    /// ([`SqliteStore::hybrid_search_detailed`]); only the default policy
    /// differs. `min_cosine` gates the **vector source only**, so an exact
    /// keyword hit whose embedding sits far away still comes back (D3) — that
    /// is why the existing 0.3 floor is kept rather than removed.
    ///
    /// `lexical` carries the raw query (what was embedded) separately from the
    /// analyzed terms (what FTS runs), so a full natural language sentence can
    /// drive cosine while precise terms drive the lexical source (D2).
    pub fn search_detailed(
        &self,
        lexical: &LexicalQuery<'_>,
        embedding: Option<&[f32]>,
        k: usize,
        policy: &FusionPolicy,
    ) -> Result<Vec<(ConversationHit, FusedHit)>> {
        let fused = match embedding {
            Some(emb) => self.store.hybrid_search_detailed(
                LABEL,
                lexical,
                Some(emb),
                k,
                &FusionPolicy {
                    min_cosine: policy.min_cosine,
                    ..*policy
                },
            )?,
            // No embedding: the vector source did not run, so every hit is
            // lexical and no cosine floor applies.
            None => self
                .store
                .text_search_by_query(LABEL, lexical, k)?
                .into_iter()
                .enumerate()
                .map(|(fused_rank, (id, score))| {
                    FusedHit {
                        id,
                        matched_by: MatchedBy::Text,
                        vector_rank: None,
                        text_rank: Some(fused_rank),
                        cosine: None,
                        fused_score: score,
                        fused_rank,
                    }
                })
                .collect(),
        };

        let mut out = Vec::with_capacity(fused.len());
        for f in fused {
            let Some(props) = self.store.load_props(f.id, LABEL)? else {
                continue;
            };
            let Ok(p) = serde_json::from_str::<MessageProps>(&props) else {
                continue;
            };
            out.push((
                ConversationHit {
                    session_id: p.session_id,
                    message_index: p.message_index.max(0) as usize,
                    role: p.role,
                    content: p.content,
                    score: f.fused_score,
                },
                f,
            ));
        }
        Ok(out)
    }

    /// Node ids of every indexed message belonging to `session_id`.
    fn session_message_ids(&self, session_id: &str) -> Result<Vec<u64>> {
        let conn = self.store.lock();
        let mut stmt = conn.prepare(
            "SELECT id FROM nodes WHERE label = ?1 \
             AND json_extract(props, '$.session_id') = ?2",
        )?;
        let ids = stmt
            .query_map(rusqlite::params![LABEL, session_id], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids.into_iter().map(|i| i as u64).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIM: usize = 4;

    fn emb(seed: f32) -> Vec<f32> {
        let mut v = vec![0.0; DIM];
        v[seed as usize % DIM] = 1.0;
        v
    }

    #[test]
    fn index_then_text_search_finds_cjk_substring() {
        let idx = ConversationStore::open_in_memory(DIM).unwrap();
        idx.index_message("s1", 0, "user", "帮我把网关的日志级别改成 debug", &emb(0.0))
            .unwrap();
        idx.index_message("s1", 1, "assistant", "已修改", &emb(1.0))
            .unwrap();
        // 4-char CJK substring: unicode61 would tokenize the whole run and
        // miss it — this is the ADR-082 §1.4 bug the trigram tokenizer fixes.
        let hits = idx.search("网关的日志", None, 10).unwrap();
        assert_eq!(hits.len(), 1, "trigram CJK substring must match: {hits:?}");
        assert_eq!(hits[0].session_id, "s1");
        assert_eq!(hits[0].message_index, 0);
    }

    #[test]
    fn short_query_falls_back_to_like() {
        let idx = ConversationStore::open_in_memory(DIM).unwrap();
        idx.index_message("s1", 0, "user", "ab cd ef", &emb(0.0))
            .unwrap();
        // <3 chars: trigram cannot index it, so LIKE carries the search.
        let hits = idx.search("ab", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn hybrid_search_returns_embedded_hit_and_carries_metadata() {
        let idx = ConversationStore::open_in_memory(DIM).unwrap();
        idx.index_message("s1", 0, "user", "deploy the gateway", &emb(0.0))
            .unwrap();
        idx.index_message("s1", 1, "assistant", "done", &emb(1.0))
            .unwrap();
        let hits = idx.search("gateway", Some(&emb(0.0)), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].role, "user");
        assert_eq!(hits[0].content, "deploy the gateway");
        // Score is the RRF fused value (`Σ w / (60 + rank + 1)`); for a
        // single-source rank-1 dual-source hit that's `2 / 61 ≈ 0.0328`.
        // Anything strictly positive is the real assertion.
        assert!(hits[0].score > 0.0, "score {:?}", hits[0].score);
    }

    /// ADR-082 D5 anchor, conversation flavour: a message the vector source
    /// rejects (cosine 0 < the 0.3 floor) must still be returned when it is the
    /// lexical match. If the floor were applied to the fused score instead of
    /// the vector source, conversation search would go blind to exact keyword
    /// hits every time the provider's embedding drifted.
    #[test]
    fn hybrid_search_keeps_lexical_hit_whose_embedding_is_far() {
        let idx = ConversationStore::open_in_memory(DIM).unwrap();
        idx.index_message("s1", 0, "user", "the zebra crossing", &emb(1.0))
            .unwrap();
        // Query embedding is orthogonal to the stored one.
        let hits = idx.search("zebra", Some(&emb(0.0)), 10).unwrap();
        assert_eq!(hits.len(), 1, "lexical hit dropped: {hits:?}");
        assert_eq!(hits[0].message_index, 0);
    }

    #[test]
    fn watermark_advances_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE_FILE);
        {
            let idx = ConversationStore::open(&path, DIM).unwrap();
            assert_eq!(idx.next_line("s1"), 0);
            idx.index_message("s1", 0, "user", "hello world", &emb(0.0))
                .unwrap();
            idx.index_message("s1", 1, "assistant", "hi there", &emb(1.0))
                .unwrap();
            assert_eq!(idx.next_line("s1"), 2);
            assert_eq!(idx.next_line("other"), 0);
            idx.close().unwrap();
        }
        // Reopening must not re-index: the watermark is derived from the rows.
        let idx = ConversationStore::open(&path, DIM).unwrap();
        assert_eq!(idx.next_line("s1"), 2);
        assert_eq!(idx.message_count().unwrap(), 2);
        assert_eq!(idx.session_count().unwrap(), 1);
        assert_eq!(idx.embedding_dim(), DIM);
    }

    #[test]
    fn reopen_purges_duplicate_lines_and_keeps_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE_FILE);
        let idx = ConversationStore::open(&path, DIM).unwrap();
        let first = idx
            .index_message("s1", 0, "user", "original line zero", &emb(0.0))
            .unwrap();
        // Simulate the crash-recovery case: a duplicate line slipped in.
        idx.index_message("s1", 0, "user", "duplicate line zero", &emb(1.0))
            .unwrap();
        assert_eq!(idx.message_count().unwrap(), 2);
        idx.close().unwrap();

        let idx = ConversationStore::open(&path, DIM).unwrap();
        assert_eq!(idx.message_count().unwrap(), 1, "duplicate must be purged");
        let hits = idx.search("line", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "original line zero");
        assert_eq!(idx.next_line("s1"), 1);
        // The surviving row is the one that was written first.
        assert!(idx.store.delete_node(first).is_ok());
    }

    #[test]
    fn remove_session_drops_only_that_session() {
        let idx = ConversationStore::open_in_memory(DIM).unwrap();
        idx.index_message("s1", 0, "user", "alpha message", &emb(0.0))
            .unwrap();
        idx.index_message("s2", 0, "user", "beta message", &emb(1.0))
            .unwrap();
        assert_eq!(idx.remove_session("s1").unwrap(), 1);
        assert_eq!(idx.message_count().unwrap(), 1);
        assert_eq!(idx.next_line("s1"), 0, "watermark goes with the session");
        let hits = idx.search("message", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s2");
    }

    #[test]
    fn rebuild_clears_rows_and_watermarks() {
        let idx = ConversationStore::open_in_memory(DIM).unwrap();
        idx.index_message("s1", 0, "user", "one two three", &emb(0.0))
            .unwrap();
        idx.index_message("s2", 3, "user", "four five six", &emb(1.0))
            .unwrap();
        assert_eq!(idx.rebuild().unwrap(), 2);
        assert_eq!(idx.message_count().unwrap(), 0);
        assert_eq!(idx.next_line("s1"), 0);
        assert_eq!(idx.next_line("s2"), 0);
        assert!(idx.search("one", None, 10).unwrap().is_empty());
    }

    #[test]
    fn content_is_truncated_and_the_embedding_dim_is_enforced() {
        let idx = ConversationStore::open_in_memory(DIM).unwrap();
        let long = "x".repeat(MAX_INDEX_CONTENT + 500);
        idx.index_message("s1", 0, "user", &long, &emb(0.0))
            .unwrap();
        let conn = idx.store.lock();
        let stored: String = conn
            .query_row(
                "SELECT json_extract(props, '$.content') FROM nodes WHERE label = ?1",
                [LABEL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored.chars().count(), MAX_INDEX_CONTENT);
    }
}
