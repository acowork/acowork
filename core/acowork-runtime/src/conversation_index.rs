//! Conversation vector index (ADR-081 §4.2, P1-2).
//!
//! One `ConversationMessage` node per indexed JSONL line, in the workspace's
//! `memory/private.sqlite` — `session_id`, `message_index` (the JSONL line
//! number), `role`, `content`, embedding in the shared `vectors` table. The
//! rows are separable from the memory nodes by label, so the index can be
//! dropped and rebuilt from the JSONL history at any time (ADR-081
//! "索引目录独立，可删重建；降级关键词").
//!
//! The JSONL conversation log is append-only (compaction appends a
//! `kind="compaction"` marker — never truncates), so the JSONL line
//! number is a stable message index and a per-session watermark is all
//! the incremental state we need. [`ConversationIndexer`] tails the
//! files: cold-start scan on first run, then a periodic delta scan.
//! Only `user`/`assistant` messages are indexed — tool calls, thoughts,
//! system nudges and compaction summaries are dialogue noise for search.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use acowork_core::retrieval::{FusionPolicy, LexicalQuery, MatchedBy};
use acowork_sqlite::conversation::ConversationStore;

use crate::conversation::ConversationEntry;
use crate::error::Result;

/// One ranked conversation hit.
#[derive(Debug, Clone)]
pub struct ConversationHit {
    pub session_id: String,
    /// JSONL line number — also the session message index.
    pub message_index: usize,
    pub role: String,
    pub content: String,
    /// In-source RRF fused rank signal, **not** a similarity (design 28 §4.1).
    /// Only comparable against other conversation hits.
    pub score: f64,
    /// Which retrieval source(s) produced this hit (ADR-095 D4). `None` on the
    /// legacy path that cannot report it.
    pub matched_by: Option<MatchedBy>,
    /// 1-based rank in the vector pool, when the vector source contributed.
    pub vector_rank: Option<usize>,
    /// 1-based rank in the BM25 pool, when the lexical source contributed.
    pub text_rank: Option<usize>,
    /// Raw cosine of the vector hit, when the vector source contributed.
    /// Kept for evaluation and for the summary prompt's evidence wording; not a
    /// ranking key.
    pub cosine: Option<f64>,
    /// Position in this source's returned list, when the engine reports it.
    /// This is the key the UI renders by (ADR-095 D4: order comes from the
    /// engine, not from `score`). `None` on the legacy path, which has no
    /// fusion to rank.
    pub fused_rank: Option<usize>,
}

/// The conversation vector index + per-session watermark.
pub struct ConversationIndex {
    store: ConversationStore,
    /// `{work_dir}/conversations` — the JSONL + meta source of truth the
    /// indexer tails and `/search` reads session titles from.
    conversations_dir: PathBuf,
    /// True while the index is not yet caught up with the JSONL history.
    /// Surfaced by `/search` as the ADR-081 `indexing` flag.
    indexing: AtomicBool,
}

impl ConversationIndex {
    /// Build the index on an already-open [`acowork_sqlite::SqliteStore`], so
    /// memory, session meta and the conversation index share one `.sqlite`
    /// file (ADR-082 §4 step 3). `work_dir` still supplies the JSONL
    /// `conversations/` source dir the indexer tails.
    ///
    /// The vector width comes from the store, which remembers the dimension it
    /// was created with — see `SqliteStore::open`.
    pub fn from_store(
        store: std::sync::Arc<acowork_sqlite::SqliteStore>,
        work_dir: &Path,
    ) -> Result<Self> {
        let started = Instant::now();
        let embedding_dim = store.embedding_dim();
        let store = ConversationStore::from_store(store)
            .map_err(|e| crate::error::RuntimeError::Memory(e.to_string()))?;
        tracing::info!(
            work_dir = %work_dir.display(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            dim = embedding_dim,
            messages = store.message_count().unwrap_or(0),
            "conversation index store opened on the shared SQLite file"
        );
        Ok(Self {
            store,
            conversations_dir: work_dir.join("conversations"),
            indexing: AtomicBool::new(true),
        })
    }

    /// Embedding dimension this index was opened with.
    pub fn embedding_dim(&self) -> usize {
        self.store.embedding_dim()
    }

    /// Adopt a new vector width. Only for an empty index — see
    /// [`ConversationStore::set_embedding_dim`].
    pub fn set_embedding_dim(&self, dim: usize) {
        match self.store.set_embedding_dim(dim) {
            Ok(()) => tracing::info!(dim, "conversation index: dimension adopted"),
            Err(e) => {
                tracing::error!(dim, error = %e, "conversation index: failed to adopt dimension")
            }
        }
    }

    /// Number of indexed messages.
    pub fn message_count(&self) -> u64 {
        self.store.message_count().unwrap_or(0)
    }

    /// Every session present in the index (sweeper reconciliation).
    pub fn sessions(&self) -> Vec<String> {
        self.store.sessions()
    }

    /// `{work_dir}/conversations` — where session JSONL + `meta/*.json`
    /// live. `/search` reads session titles from here for display.
    pub fn conversations_dir(&self) -> &Path {
        &self.conversations_dir
    }

    /// True while the indexer is still catching up with the JSONL logs.
    pub fn is_indexing(&self) -> bool {
        self.indexing.load(Ordering::Relaxed)
    }

    /// Set by the indexer after each sweep.
    pub fn set_indexing(&self, indexing: bool) {
        self.indexing.store(indexing, Ordering::Relaxed);
    }

    /// Index one message and advance this session's watermark past it.
    pub fn index_message(
        &self,
        session_id: &str,
        message_index: usize,
        role: &str,
        content: &str,
        embedding: &[f32],
    ) -> Result<()> {
        self.store
            .index_message(session_id, message_index, role, content, embedding)
            .map_err(|e| crate::error::RuntimeError::Memory(e.to_string()))?;
        Ok(())
    }

    /// Next JSONL line to index for `session_id` (0 = not started).
    pub fn next_line(&self, session_id: &str) -> usize {
        self.store.next_line(session_id)
    }

    /// Record that lines `< next_line` are indexed for `session_id`.
    pub fn mark_indexed(&self, session_id: &str, next_line: usize) {
        self.store.mark_indexed(session_id, next_line);
    }

    /// Remove every indexed message of a (deleted) session.
    pub fn remove_session(&self, session_id: &str) {
        match self.store.remove_session(session_id) {
            Ok(removed) => {
                tracing::info!(session_id, removed, "conversation index: purged session")
            }
            Err(e) => tracing::warn!(session_id, error = %e, "conversation index: purge failed"),
        }
    }

    /// Reset the whole index: purge every message row and clear the
    /// per-session watermarks, then latch `indexing` so the next indexer
    /// sweep rebuilds from the JSONL history. The store file is
    /// re-derivable, so this is the self-heal path after corruption or an
    /// embedding-dimension change (ADR-081 "可删重建").
    pub fn rebuild(&self) {
        match self.store.rebuild() {
            Ok(purged) => {
                self.set_indexing(true);
                tracing::info!(purged, "conversation index: full rebuild scheduled");
            }
            Err(e) => tracing::warn!(error = %e, "conversation index: rebuild failed"),
        }
    }

    /// Hybrid (or BM25-only when `embedding` is `None`) search over indexed
    /// messages, ranked by descending score.
    ///
    /// Degrades to an empty result instead of propagating: `/search` is a read
    /// path and a broken index is not worth a 500.
    pub fn search(
        &self,
        query_text: &str,
        embedding: Option<&[f32]>,
        k: usize,
    ) -> Vec<ConversationHit> {
        match self.store.search(query_text, embedding, k) {
            Ok(hits) => hits
                .into_iter()
                .map(|h| ConversationHit {
                    session_id: h.session_id,
                    message_index: h.message_index,
                    role: h.role,
                    content: h.content,
                    score: h.score,
                    matched_by: None,
                    vector_rank: None,
                    text_rank: None,
                    cosine: None,
                    fused_rank: None,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "conversation index search failed");
                Vec::new()
            }
        }
    }

    /// [`Self::search`] plus the per-source evidence that global search needs
    /// for summary-candidate selection (ADR-095 D4, design 28 §4.2).
    ///
    /// `lexical_terms` drives the BM25 source; `query_text` is what the caller
    /// embedded. An empty term list falls back to `query_text` inside the
    /// engine, so an analyzer miss cannot cost recall (D2).
    pub fn search_detailed(
        &self,
        lexical: &LexicalQuery<'_>,
        embedding: Option<&[f32]>,
        k: usize,
        policy: &FusionPolicy,
    ) -> Vec<ConversationHit> {
        match self
            .store
            .search_detailed(lexical, embedding, k, policy)
        {
            Ok(hits) => hits
                .into_iter()
                .map(|(h, fused)| ConversationHit {
                    session_id: h.session_id,
                    message_index: h.message_index,
                    role: h.role,
                    content: h.content,
                    score: fused.fused_score,
                    matched_by: Some(fused.matched_by),
                    vector_rank: fused.vector_rank,
                    text_rank: fused.text_rank,
                    cosine: fused.cosine,
                    fused_rank: Some(fused.fused_rank),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "conversation index detailed search failed");
                Vec::new()
            }
        }
    }
}

fn is_indexable(entry: &ConversationEntry) -> bool {
    entry.kind.as_deref() != Some(crate::conversation::ENTRY_KIND_COMPACTION)
        && (entry.role == "user" || entry.role == "assistant")
        && !entry.content.is_empty()
}

/// Background tailer: cold-start scans all `{work_dir}/conversations/*.jsonl`
/// then periodically picks up appended lines, embedding via the agent's
/// live embedding provider (read from the shared `AgentCore` slot each
/// cycle — no hard coupling to provider lifecycle).
pub struct ConversationIndexer {
    index: std::sync::Arc<ConversationIndex>,
    conversations_dir: PathBuf,
    agent_core: crate::http::SharedAgentCore,
}

impl ConversationIndexer {
    pub fn new(
        index: std::sync::Arc<ConversationIndex>,
        work_dir: &Path,
        agent_core: crate::http::SharedAgentCore,
    ) -> Self {
        Self {
            index,
            conversations_dir: work_dir.join("conversations"),
            agent_core,
        }
    }

    /// Run the tail loop forever. Re-reads the provider each cycle so an
    /// embedding sidecar that (dis)connects later is picked up; while no
    /// provider is bound, watermarks stay put and the index simply lags.
    pub async fn run(self: std::sync::Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            self.sweep().await;
        }
    }

    async fn sweep(&self) {
        let provider: Option<std::sync::Arc<dyn crate::embedding::EmbeddingProvider>> = {
            let guard = self.agent_core.read().ok();
            guard
                .as_ref()
                .and_then(|g| g.as_ref())
                // The cell, not `.embedding_provider`: the published snapshot
                // is an immutable `Arc<AgentCore>` clone, so its field froze at
                // session start while the session went on to adopt a new model.
                // Reading the field is what made this sweep defer forever after
                // a model switch - provider_dim stayed on the old width while
                // the index had already been re-embedded at the new one.
                .and_then(|c| c.live_embedding_provider())
        };

        // Discover current JSONL sessions.
        let mut sessions: Vec<String> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.conversations_dir) {
            for e in rd.flatten() {
                if e.path().extension().is_some_and(|x| x == "jsonl")
                    && let Some(name) = e.file_name().to_str()
                    && let Some(sid) = name.strip_suffix(".jsonl")
                {
                    sessions.push(sid.to_string());
                }
            }
        }

        // Purge index entries whose JSONL file vanished (session deleted).
        let indexed: Vec<String> = self.index.sessions();
        for sid in &indexed {
            if !sessions.contains(sid) {
                self.index.remove_session(sid);
            }
        }

        let mut found_pending = false;
        // Dimension guard: the store keeps the dimension it was created with,
        // and `vector_search` ignores rows of another width, so writing a
        // provider's vectors into a store of a different width would land them
        // where nothing will ever look. Defer the sweep (watermarks stay put)
        // until the two agree.
        if let Some(provider) = provider.as_deref() {
            let dim = provider.dimension();
            let index_dim = self.index.embedding_dim();
            if dim != index_dim && self.index.message_count() == 0 {
                // An empty index's dimension is not a fact about stored data —
                // it is whatever the index was created with, often before the
                // provider had bound. Adopt the provider's width and index, so
                // a model swap (or a first binding) heals itself.
                tracing::info!(
                    provider_dim = dim,
                    index_dim,
                    "conversation index: empty index adopting the provider dimension"
                );
                self.index.set_embedding_dim(dim);
            }
            if dim != self.index.embedding_dim() {
                tracing::warn!(
                    provider_dim = dim,
                    index_dim = self.index.embedding_dim(),
                    "conversation index: provider dimension mismatch, deferring sweep"
                );
                self.index.set_indexing(true);
                return;
            }
        }
        for sid in &sessions {
            let path = self.conversations_dir.join(format!("{sid}.jsonl"));
            let total = crate::conversation::count_jsonl_lines(&path).unwrap_or(0);
            let next = self.index.next_line(sid);
            if next < total {
                found_pending = true;
            }
            if next >= total || provider.is_none() {
                continue;
            }
            self.index_session(sid, &path, next, total, provider.as_deref().unwrap())
                .await;
        }
        // Latch the flag: once a cycle finds nothing new, indexing is done.
        self.index.set_indexing(found_pending);
    }

    async fn index_session(
        &self,
        session_id: &str,
        path: &Path,
        mut next: usize,
        total: usize,
        provider: &dyn crate::embedding::EmbeddingProvider,
    ) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let lines: Vec<&str> = text.split('\n').collect();
        // Drop a trailing partial line (writer may be mid-append).
        let bound = if text.ends_with('\n') {
            lines.len()
        } else {
            lines.len().saturating_sub(1)
        };
        while next < bound.min(total) {
            let line = lines[next].trim();
            let entry: ConversationEntry = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(_) => {
                    // Non-JSON line (shouldn't happen): skip it, don't spin.
                    self.index.mark_indexed(session_id, next + 1);
                    next += 1;
                    continue;
                }
            };
            if !is_indexable(&entry) {
                self.index.mark_indexed(session_id, next + 1);
                next += 1;
                continue;
            }
            match provider.embed(&entry.content).await {
                Ok(emb) => {
                    if let Err(e) = self.index.index_message(
                        session_id,
                        next,
                        &entry.role,
                        &entry.content,
                        &emb,
                    ) {
                        tracing::warn!(session_id, line = next, error = %e, "conversation index: store write failed, skipping line");
                        self.index.mark_indexed(session_id, next + 1);
                    }
                }
                Err(e) => {
                    tracing::warn!(session_id, line = next, error = %e, "conversation index: embedding failed, skipping line");
                    self.index.mark_indexed(session_id, next + 1);
                }
            }
            next += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::ENTRY_KIND_COMPACTION;
    use acowork_memory::types::DEFAULT_EMBEDDING_DIM;

    /// The workspace's shared `memory/private.sqlite` — with ADR-082 §4 step 3
    /// the only place a conversation index lives.
    fn open_index(work_dir: &std::path::Path, dim: usize) -> ConversationIndex {
        let db = work_dir.join("memory").join("private.sqlite");
        let store = std::sync::Arc::new(
            acowork_sqlite::SqliteStore::open(&db, dim).expect("open shared sqlite store"),
        );
        ConversationIndex::from_store(store, work_dir).expect("index open")
    }

    fn open_tmp() -> (tempfile::TempDir, std::sync::Arc<ConversationIndex>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = std::sync::Arc::new(open_index(dir.path(), DEFAULT_EMBEDDING_DIM));
        (dir, index)
    }

    /// Deterministic unit-magnitude vector of the store's embedding dim.
    fn vec_dim(dim: usize, seed: u8) -> Vec<f32> {
        (0..dim)
            .map(|i| ((i as u8).wrapping_mul(seed) as f32) / 256.0)
            .collect()
    }

    fn entry(role: &str, content: &str, kind: Option<&str>) -> ConversationEntry {
        ConversationEntry {
            id: format!("{role}-{content}"),
            ts: "2025-01-01T00:00:00.000Z".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            metadata: None,
            kind: kind.map(String::from),
        }
    }

    #[test]
    fn is_indexable_filters_compaction_and_noise() {
        assert!(is_indexable(&entry("user", "hello", None)));
        assert!(is_indexable(&entry("assistant", "hi there", None)));
        // Dialogue noise is not indexed.
        assert!(!is_indexable(&entry("system", "nudge", None)));
        assert!(!is_indexable(&entry("thought", "internal", None)));
        assert!(!is_indexable(&entry("tool_result", "42", None)));
        // Compaction summaries are skipped even with a system role.
        assert!(!is_indexable(&entry(
            "system",
            "summary",
            Some(ENTRY_KIND_COMPACTION)
        )));
        // Empty content is skipped.
        assert!(!is_indexable(&entry("user", "", None)));
    }

    #[test]
    fn index_message_advances_watermark_and_is_searchable() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 3);

        index
            .index_message("s1", 0, "user", "the quick brown fox", &emb)
            .expect("index s1:0");
        index
            .index_message("s1", 1, "assistant", "jumps over the lazy dog", &emb)
            .expect("index s1:1");
        index
            .index_message("s2", 0, "user", "unrelated kitchen recipe", &emb)
            .expect("index s2:0");

        // Watermark advanced past every indexed line.
        assert_eq!(index.next_line("s1"), 2);
        assert_eq!(index.next_line("s2"), 1);

        // BM25 (no embedding) ranks the session that actually contains "fox".
        let hits = index.search("fox", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert_eq!(hits[0].message_index, 0);
        assert_eq!(hits[0].role, "user");
        assert_eq!(hits[0].content, "the quick brown fox");
        assert!(hits[0].score > 0.0);

        // "lazy dog" only matches s1:1.
        let hits = index.search("lazy dog", None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_index, 1);
    }

    #[test]
    fn search_with_embedding_uses_hybrid_vector_path() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 7);
        index
            .index_message("s1", 0, "assistant", "rust borrow checker rules", &emb)
            .expect("index");

        // Same-dimension query embedding drives the hybrid path without panic.
        let hits = index.search("borrow checker", Some(&emb), 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn remove_session_purges_nodes_and_watermark() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 1);
        index
            .index_message("s1", 0, "user", "alpha beta", &emb)
            .expect("s1");
        index
            .index_message("s2", 0, "user", "alpha gamma", &emb)
            .expect("s2");

        index.remove_session("s1");

        assert_eq!(
            index.next_line("s1"),
            0,
            "watermark reset for purged session"
        );
        let hits = index.search("alpha", None, 10);
        assert_eq!(
            hits.len(),
            1,
            "only the surviving session remains searchable"
        );
        assert_eq!(hits[0].session_id, "s2");
    }

    #[test]
    fn content_is_truncated_to_index_cap() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 2);
        let long: String = "banana ".repeat(acowork_sqlite::conversation::MAX_INDEX_CONTENT);
        index
            .index_message("s1", 0, "user", &long, &emb)
            .expect("index");
        let hits = index.search("banana", None, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].content.chars().count(),
            acowork_sqlite::conversation::MAX_INDEX_CONTENT
        );
    }

    /// A model swap leaves the index at its old width, where the indexer would
    /// defer every sweep forever. Emptying it and adopting the new width is the
    /// way out, and the new width has to survive a reopen — the store hands out
    /// the dimension it was created with and ignores whatever it is passed.
    #[test]
    fn an_emptied_index_adopts_a_new_dimension_and_keeps_it() {
        let (dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 7);
        index
            .index_message("s1", 0, "user", "alpha beta", &emb)
            .expect("index");
        assert_eq!(index.embedding_dim(), DEFAULT_EMBEDDING_DIM);

        index.rebuild();
        index.set_embedding_dim(512);
        assert_eq!(index.embedding_dim(), 512);
        drop(index);

        let reopened = open_index(dir.path(), DEFAULT_EMBEDDING_DIM);
        assert_eq!(
            reopened.embedding_dim(),
            512,
            "the store keeps the width it was given, not the one it is opened with"
        );
        assert_eq!(reopened.message_count(), 0);
    }

    #[test]
    fn rebuild_purges_nodes_resets_watermarks_and_latches_indexing() {
        let (_dir, index) = open_tmp();
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 4);
        index
            .index_message("s1", 0, "user", "alpha beta", &emb)
            .expect("s1");
        index
            .index_message("s2", 0, "user", "alpha gamma", &emb)
            .expect("s2");
        index.set_indexing(false);

        index.rebuild();

        assert_eq!(index.next_line("s1"), 0, "watermark reset");
        assert_eq!(index.next_line("s2"), 0, "watermark reset");
        assert!(index.is_indexing(), "indexing latched true until caught up");
        assert!(
            index.search("alpha", None, 10).is_empty(),
            "all nodes purged — the tailer re-indexes from JSONL"
        );
    }

    #[test]
    fn open_honors_non_default_embedding_dim() {
        // Regression: the index used to open at the hardcoded default (384),
        // so a 512-dim provider (bge-small-zh-v1.5) mismatched every write
        // and the indexer deferred every sweep — search always came back
        // empty. The width now comes from the memory store, which is sized by
        // the live embedding provider at boot.
        let dir = tempfile::tempdir().expect("tempdir");
        let index = open_index(dir.path(), 512);
        assert_eq!(index.embedding_dim(), 512);
        // And a 512-dim vector must be accepted (no dimension-mismatch panic).
        let emb = vec_dim(512, 9);
        index
            .index_message("s1", 0, "user", "dimension regression check", &emb)
            .expect("512-dim write accepted");
        assert_eq!(index.search("regression", None, 10).len(), 1);
    }

    #[test]
    fn reopen_recovers_watermark_without_duplicating() {
        // Regression: the watermark is in-memory but the nodes are on disk.
        // Reopening (a Runtime restart) used to reset every session to line 0,
        // so the tailer re-embedded the whole JSONL and wrote a second node
        // per message — duplicate rows, index growth on every restart.
        let dir = tempfile::tempdir().expect("tempdir");
        let emb = vec_dim(DEFAULT_EMBEDDING_DIM, 5);
        {
            let idx = open_index(dir.path(), DEFAULT_EMBEDDING_DIM);
            idx.index_message("s1", 0, "user", "unique alpha marker", &emb)
                .expect("i0");
            idx.index_message("s1", 1, "assistant", "unique beta marker", &emb)
                .expect("i1");
            assert_eq!(idx.next_line("s1"), 2);
        }

        let idx2 = open_index(dir.path(), DEFAULT_EMBEDDING_DIM);
        // Watermark restored from the persisted nodes, not reset to 0.
        assert_eq!(idx2.next_line("s1"), 2, "watermark recovered on reopen");
        assert_eq!(
            idx2.search("alpha", None, 10).len(),
            1,
            "no duplicate rows after reopen"
        );

        // Re-indexing the same lines again must not duplicate (a restart that
        // raced a sweep): overwrite-by-key is not available, so `recover`
        // purges the strays on the NEXT open — verify the dedup path directly.
        idx2.index_message("s1", 0, "user", "unique alpha marker", &emb)
            .expect("dup write");
        assert_eq!(
            idx2.search("alpha", None, 10).len(),
            2,
            "duplicate present before recovery"
        );
        // A restart drops the old handle before reopening — the shared
        // `.sqlite` file holds one live handle at a time.
        drop(idx2);
        let idx3 = open_index(dir.path(), DEFAULT_EMBEDDING_DIM);
        assert_eq!(
            idx3.search("alpha", None, 10).len(),
            1,
            "recovery purges duplicates"
        );
        assert_eq!(
            idx3.next_line("s1"),
            2,
            "watermark stays correct after purge"
        );
    }
}
