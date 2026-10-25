//! SQLite workspace store (ADR-082 D1) — the concrete backend behind the
//! [`crate::MemoryProvider`] / [`crate::MemoryAdminService`] traits.
//!
//! Replaces grafeo-engine as the on-disk store: one `nodes` row per memory
//! node, embeddings in a `vectors` table (f32 little-endian BLOB), and one FTS5
//! `trigram` index per memory label. `open` is O(1) — there is no WAL to replay
//! and no BM25 index to rebuild — so startup no longer scales with store size.
//!
//! # Ownership
//!
//! The type lives here (rather than in the former `acowork-sqlite` crate)
//! because the memory service binary is the primary owner of this storage
//! engine, and a backend crate cannot be linked *into* the trait crate that
//! declares the traits it implements (ADR-101). `acowork-sqlite` now keeps only
//! the two Runtime-side sidecars that share this connection — the conversation
//! index (`ConversationStore`) and session meta (`SqliteSessionMetaStore`) —
//! and re-exports [`SqliteStore`] for its callers. [`SqliteStore::lock`] and the
//! generic row helpers are therefore `pub`: they are the cross-crate seam those
//! sidecars need, not a stable public API for other consumers.
//!
//! # Storage contract
//!
//! `props` is the `serde_json` serialization of the node structs ([`Episode`],
//! [`KnowledgeNode`], [`ProceduralNode`], [`AutobiographicalNode`]) with exactly
//! two fields projected out:
//!
//! * `id` → the `nodes.id` primary key;
//! * `embedding` → the `vectors` table.
//!
//! Every other field round-trips verbatim, so a node read back is
//! field-for-field equal to the one written. The expected key set of `props` is
//! asserted per node type in [`tests`] — adding a field to a node struct
//! without wiring it up fails the suite. Missing `id` / `embedding` keys
//! deserialize to `None` / empty (serde `Option` and `#[serde(default)]`), so
//! the projection never breaks a read.

mod admin;
pub mod legacy_import;
mod provider;
mod retrieval;
mod schema;
#[cfg(test)]
mod tests;

pub use legacy_import::{ImportCounts, has_sqlite_magic};

pub use retrieval::RRF_K;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension, ToSql, params};

use acowork_core::retrieval::{LexicalPlan, LexicalQuery};
use crate::quality::MemoryQualityConfig;
use crate::{
    AutobioCategory, AutobiographicalNode, Episode, KnowledgeNode, NodeStatus, PromotionMetadata,
    ProceduralNode,
};
pub use crate::labels;

pub use schema::{SCHEMA_SQL, SCHEMA_VERSION};

/// Node label of the conversation index (ADR-081).
///
/// Declared here because [`schema::fts_table`] and
/// [`SqliteStore::purge_conversation_state`] both have to recognise it, and
/// they live in this crate; `acowork-sqlite::conversation::LABEL` is an alias
/// of this constant, so the two crates can never drift apart.
pub const CONVERSATION_LABEL: &str = "ConversationMessage";

/// Minimum pattern length the FTS5 `trigram` tokenizer can match. Terms
/// shorter than this are dropped by [`SqliteStore::text_search_terms`] (and
/// by the Runtime query analyzer, which uses the same floor so it never
/// emits a term the executor will discard).
pub const TRIGRAM_MIN: usize = 3;

/// Defensive ceiling on the number of terms executed in one FTS query. The
/// analyzer caps lexical terms lower than this; this exists only so a
/// hand-built term list cannot produce an unbounded `MATCH` expression.
pub const MAX_EXECUTED_TERMS: usize = 32;

/// Convenience alias for callers that want the active number without a
/// `schema::` import path.
pub fn schema_version() -> i64 {
    SCHEMA_VERSION
}

/// Error type for the SQLite backend.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A `props` value could not be (de)serialized.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Filesystem failure while creating the database directory.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A storage invariant was violated (e.g. updating a node without an id).
    #[error("{0}")]
    Memory(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Backend errors surface to the Runtime as memory errors, so `?` works inside
/// the [`crate::MemoryProvider`] implementation.
impl From<Error> for acowork_core::error::AcoworkError {
    fn from(e: Error) -> Self {
        Self::Memory(e.to_string())
    }
}

/// Cosine-similarity threshold above which two knowledge nodes with the same
/// `(subject, predicate)` are treated as the same fact (mirrors the grafeo
/// store's semantic dedup).
///
/// Never-configurable default; [`MemoryProvider::apply_quality_config`] may
/// override it with `DedupQuality::knowledge_threshold` (ADR-062 D2).
const KNOWLEDGE_DEDUP_SIMILARITY: f64 = 0.95;

/// SQLite-backed memory store.
///
/// # Thread safety
///
/// A single [`Connection`] is guarded by a [`Mutex`]; the ADR-082 workload
/// premise is one agent per database with zero query concurrency, so serializing
/// access is both sufficient and simpler than a pool. `Connection` is `Send`, so
/// `SqliteStore` is `Send + Sync`.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    /// Current vector dimension. Atomic because embedding migration rewrites
    /// it in place; persisted in `meta` so a reopen reports the real stored
    /// dimension rather than the caller's guess.
    embedding_dim: AtomicUsize,
    /// Agent memory-quality knobs (ADR-062 D2). Defaults reproduce the
    /// pre-configuration behaviour until `apply_quality_config` is called.
    quality: RwLock<MemoryQualityConfig>,
}

// Static assertion: the store must be shareable across runtime tasks.
const _: () = {
    const fn assert_sync<T: Sync>() {}
    assert_sync::<SqliteStore>();
};

impl SqliteStore {
    /// Open (or create) a SQLite store at `path` and apply the schema.
    ///
    /// `embedding_dim` is the expected vector length; embeddings of a different
    /// length are skipped by [`vector_search`](Self::vector_search) rather than
    /// silently corrupting scores.
    pub fn open(path: impl AsRef<Path>, embedding_dim: usize) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn, embedding_dim)
    }

    /// Open an in-memory store (tests, ephemeral runs).
    pub fn open_in_memory(embedding_dim: usize) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, embedding_dim)
    }

    /// Open (or create) the store at `path` **without recording an
    /// embedding dimension**.
    ///
    /// For subsystems that share the workspace `.sqlite` file but never
    /// touch `vectors` — session meta (ADR-082 §4 step 3), the conversation
    /// index. Opening such a file before the memory backend does must not
    /// mint an `embedding_dim`: the memory backend is the owner of that
    /// value, and a guessed dimension recorded here would be adopted by the
    /// later `open` (see [`Self::open`]) and silently break vector search.
    /// Reads the stored dimension when present, otherwise falls back to
    /// [`crate::types::DEFAULT_EMBEDDING_DIM`]; never writes it.
    pub fn open_dim_agnostic(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA_SQL)?;
        schema::apply_migrations(&mut conn)?;
        let stored: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'embedding_dim'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let embedding_dim = stored
            .as_deref()
            .and_then(|v| v.parse().ok())
            .unwrap_or(crate::types::DEFAULT_EMBEDDING_DIM);
        Ok(Self {
            conn: Mutex::new(conn),
            embedding_dim: AtomicUsize::new(embedding_dim),
            quality: RwLock::new(MemoryQualityConfig::default()),
        })
    }

    fn from_connection(mut conn: Connection, embedding_dim: usize) -> Result<Self> {
        conn.execute_batch(SCHEMA_SQL)?;
        schema::apply_migrations(&mut conn)?;
        // The database remembers its own dimension: a caller that opens an
        // existing store with a stale dimension must not be believed.
        let stored: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'embedding_dim'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let embedding_dim = match stored.as_deref().and_then(|v| v.parse().ok()) {
            Some(stored) => stored,
            None => {
                conn.execute(
                    "INSERT OR REPLACE INTO meta(key, value) VALUES ('embedding_dim', ?1)",
                    params![embedding_dim.to_string()],
                )?;
                embedding_dim
            }
        };
        Ok(Self {
            conn: Mutex::new(conn),
            embedding_dim: AtomicUsize::new(embedding_dim),
            quality: RwLock::new(MemoryQualityConfig::default()),
        })
    }

    /// Checkpoint the write-ahead log and shrink it.
    ///
    /// Persistent WAL mode keeps durability on every commit regardless; this
    /// only stops the `-wal` file from sitting at its high-water mark
    /// (ADR-082 §1.2). Best-effort: an in-memory database has no WAL and the
    /// resulting error is ignored.
    pub fn close(&self) -> Result<()> {
        let conn = self.lock();
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        Ok(())
    }

    /// The embedding dimension this store was opened with.
    pub fn embedding_dim(&self) -> usize {
        self.embedding_dim.load(Ordering::Relaxed)
    }

    /// Record a new vector dimension (embedding migration) and persist it.
    pub fn set_embedding_dim(&self, dim: usize) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('embedding_dim', ?1)",
            params![dim.to_string()],
        )?;
        drop(conn);
        self.embedding_dim.store(dim, Ordering::Relaxed);
        Ok(())
    }

    // ── Generic node operations ──────────────────────────────────────────

    /// Delete a node by id: its FTS row is removed and its vector cascades via
    /// the foreign key. Returns `false` if the node did not exist.
    pub fn delete_node(&self, id: u64) -> Result<bool> {
        let conn = self.lock();
        let label: Option<String> = conn
            .query_row(
                "SELECT label FROM nodes WHERE id = ?1",
                params![id as i64],
                |r| r.get(0),
            )
            .optional()?;
        let Some(label) = label else {
            return Ok(false);
        };
        if let Some(fts) = schema::fts_table(&label) {
            conn.execute(
                &format!("DELETE FROM {fts} WHERE rowid = ?1"),
                params![id as i64],
            )?;
        }
        let deleted = conn.execute("DELETE FROM nodes WHERE id = ?1", params![id as i64])?;
        Ok(deleted > 0)
    }

    /// Drop every conversation-related row from this store.
    ///
    /// Called by the package-clone `Full` mode after copying the source
    /// agent's `memory/private.sqlite`: a clone starts a new dialogue, not
    /// a continuation, so the index rows (`ConversationMessage` nodes + their
    /// vectors + `fts_conversation`) and the `sessions` / `fts_sessions`
    /// meta rows would otherwise dangle — pointing at JSONL files the clone
    /// deliberately did not copy.
    ///
    /// Memory nodes (`Episodic`, `Knowledge`, `Procedural`,
    /// `Autobiographical`) are kept; the user spent the afternoon curating
    /// them and a clone should not force them to redo it. Returns the number
    /// of session-meta rows removed (the conversation-index row count is
    /// derivable but not separately reported).
    pub fn purge_conversation_state(&self) -> Result<usize> {
        let conn = self.lock();
        // Conversation index: drop every `ConversationMessage` node in one
        // statement. The FK from `vectors.node_id` cascades; `fts_conversation`
        // is the contentless companion and has no row-level FK to `nodes`,
        // so it must be cleared explicitly.
        let conv = conn.execute(
            "DELETE FROM nodes WHERE label = ?1",
            params![CONVERSATION_LABEL],
        )?;
        conn.execute("DELETE FROM fts_conversation", [])?;
        // Session meta lives alongside memory per ADR-082 §4 step 3. A
        // clone that keeps the memory file would otherwise surface ghost
        // sessions whose JSONL has not been copied.
        let sessions = conn.execute("DELETE FROM sessions", [])?;
        conn.execute("DELETE FROM fts_sessions", [])?;
        tracing::info!(
            conversation_nodes = conv,
            session_rows = sessions,
            "purged conversation state from cloned memory store"
        );
        Ok(sessions)
    }

    /// Total number of memory nodes.
    pub fn node_count(&self) -> Result<u64> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Number of nodes carrying the given label.
    pub fn node_count_by_label(&self, label: &str) -> Result<u64> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE label = ?1",
            params![label],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Number of nodes that have a stored embedding.
    /// Memory nodes that hold a vector, counted over the same label set as
    /// `AdminStats::total_nodes`.
    ///
    /// Counted `vectors` alone, this also returned every conversation-message
    /// row - which the panel's `total_nodes` deliberately does not count - so
    /// `nodes_with_embedding < total_nodes` was false on any store with chat
    /// history no matter how many of its memory nodes had lost their vectors.
    /// The two sides of that comparison must be the same set of nodes.
    pub fn count_nodes_with_embedding(&self) -> Result<u64> {
        let conn = self.lock();
        let placeholders: Vec<String> =
            (1..=labels::ALL.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT COUNT(*) FROM vectors v JOIN nodes n ON n.id = v.node_id              WHERE n.label IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let args: Vec<&dyn rusqlite::types::ToSql> =
            labels::ALL.iter().map(|l| l as &dyn rusqlite::types::ToSql).collect();
        let n: i64 = stmt.query_row(args.as_slice(), |r| r.get(0))?;
        Ok(n as u64)
    }

    // ── Episodic layer ───────────────────────────────────────────────────

    /// Insert an episode and return its assigned id.
    pub fn store_episode(&self, episode: &Episode) -> Result<u64> {
        let ts = ts_text(episode.timestamp);
        self.insert_row(
            labels::EPISODIC,
            "Active",
            &encode(episode)?,
            &episode.content,
            &ts,
            &ts,
            episode.embedding.as_deref(),
        )
    }

    /// Load an episode by id, or `None` if it is missing or not an episode.
    pub fn get_episode(&self, id: u64) -> Result<Option<Episode>> {
        let Some(props) = self.load_props(id, labels::EPISODIC)? else {
            return Ok(None);
        };
        let mut episode: Episode = serde_json::from_str(&props)?;
        episode.embedding = self.load_embedding(id)?;
        Ok(Some(episode))
    }

    /// Episodes for one session, newest first.
    pub fn search_episodes_by_session(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let conn = self.lock();
        let ids = query_ids(
            &conn,
            "SELECT id FROM nodes \
             WHERE label = ?1 AND json_extract(props, '$.session_id') = ?2 \
             ORDER BY created_at DESC LIMIT ?3",
            &[&labels::EPISODIC, &session_id, &(limit as i64)],
        )?;
        drop(conn);
        self.load_episodes(ids)
    }

    /// All episodes across sessions, newest first.
    pub fn list_all_episodes(&self, limit: usize) -> Result<Vec<Episode>> {
        self.episodes_where(
            "SELECT id FROM nodes WHERE label = ?1 ORDER BY created_at DESC LIMIT ?2",
            &[&(limit as i64)],
        )
    }

    /// Episodes not yet promoted to the semantic layer, oldest first.
    pub fn get_unconsolidated_episodes(&self, limit: usize) -> Result<Vec<Episode>> {
        self.episodes_where(
            "SELECT id FROM nodes WHERE label = ?1 AND json_extract(props, '$.consolidated') = 0 AND json_extract(props, '$.metadata.distiller_skip') IS NULL ORDER BY created_at ASC LIMIT ?2",
            &[&(limit as i64)],
        )
    }

    /// Count of episodes still awaiting consolidation.
    pub fn count_unconsolidated_episodes(&self) -> Result<usize> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE label = ?1 \
             AND json_extract(props, '$.consolidated') = 0 \
             AND json_extract(props, '$.knowledge_subtype') IS NOT NULL",
            params![labels::EPISODIC],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Flag an episode as consolidated.
    pub fn mark_episode_consolidated(&self, id: u64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE nodes SET props = json_set(props, '$.consolidated', json('true')) \
             WHERE id = ?1 AND label = ?2",
            params![id as i64, labels::EPISODIC],
        )?;
        Ok(())
    }

    /// Delete consolidated episodes older than `retention_days`. Returns the
    /// number removed.
    pub fn cleanup_old_episodes(&self, retention_days: u32) -> Result<usize> {
        let cutoff = ts_text(Utc::now() - TimeDelta::days(i64::from(retention_days)));
        let conn = self.lock();
        let ids = query_ids(
            &conn,
            "SELECT id FROM nodes \
             WHERE label = ?1 AND json_extract(props, '$.consolidated') = 1 AND created_at < ?2",
            &[&labels::EPISODIC, &cutoff],
        )?;
        drop(conn);
        let mut removed = 0usize;
        for id in ids {
            if self.delete_node(id as u64)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Episodes matching `query` by text relevance (FTS5 BM25, or `LIKE` for
    /// queries shorter than one trigram).
    pub fn search_episodes_by_keyword(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(Episode, f64)>> {
        let hits = self.text_search(labels::EPISODIC, query, limit)?;
        let mut out = Vec::with_capacity(hits.len());
        for (id, score) in hits {
            if let Some(episode) = self.get_episode(id)? {
                out.push((episode, score));
            }
        }
        Ok(out)
    }

    /// Episodes ranked by cosine similarity to `query`, most similar first.
    pub fn search_episodes_by_embedding(
        &self,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<(Episode, f64)>> {
        let hits = self.vector_search(labels::EPISODIC, query, limit)?;
        let mut out = Vec::with_capacity(hits.len());
        for (id, score) in hits {
            if let Some(episode) = self.get_episode(id)? {
                out.push((episode, score));
            }
        }
        Ok(out)
    }

    // ── Semantic layer: Knowledge ────────────────────────────────────────

    /// Store a knowledge node, deduplicating by `(subject, predicate)` (see
    /// [`KNOWLEDGE_DEDUP_SIMILARITY`]). Returns the affected node id.
    pub fn store_knowledge(&self, node: &KnowledgeNode) -> Result<u64> {
        if let Some((id, existing)) =
            self.find_knowledge_by_subject(&node.subject, &node.predicate)?
        {
            let merge = match (&node.embedding, &existing.embedding) {
                (Some(new), Some(old)) => cosine_similarity(new, old) > self.dedup_threshold(),
                // Missing embeddings on either side: fall back to the exact
                // (subject, predicate) match (conservative dedup).
                _ => true,
            };
            if merge {
                // Provenance is a union, not an overwrite. The caller is about
                // to mark `node`'s episodes consolidated; dropping their ids
                // here would leave those episodes citing no node and the node
                // unciteable, which is exactly the audit trail a rollback needs.
                let mut source_episode_ids = existing.source_episode_ids.clone();
                for id in &node.source_episode_ids {
                    if !source_episode_ids.contains(id) {
                        source_episode_ids.push(*id);
                    }
                }
                let promotion_metadata =
                    union_promotion_metadata(existing.promotion_metadata.as_ref(), node.promotion_metadata.as_ref(), &source_episode_ids);
                let merged = KnowledgeNode {
                    object: node.object.clone(),
                    confidence: node.confidence.max(existing.confidence),
                    importance: existing.importance.max(node.importance),
                    updated_at: Utc::now(),
                    embedding: node.embedding.clone().or(existing.embedding.clone()),
                    source_episode_id: node.source_episode_id.or(existing.source_episode_id),
                    source_episode_ids,
                    promotion_metadata,
                    ..existing
                };
                self.update_knowledge(id, &merged)?;
                return Ok(id);
            }
        }

        self.insert_row(
            labels::KNOWLEDGE,
            node.status.as_str(),
            &encode(node)?,
            &knowledge_content(node),
            &ts_text(node.created_at),
            &ts_text(node.updated_at),
            node.embedding.as_deref(),
        )
    }

    /// Effective knowledge-dedup cosine threshold.
    fn dedup_threshold(&self) -> f64 {
        self.quality
            .read()
            .map(|q| f64::from(q.dedup.knowledge_threshold))
            .unwrap_or(KNOWLEDGE_DEDUP_SIMILARITY)
    }

    /// Overwrite an existing knowledge node.
    ///
    /// [`KnowledgeNode`] carries no id, so the target is passed explicitly.
    pub fn update_knowledge(&self, id: u64, node: &KnowledgeNode) -> Result<()> {
        self.update_row(
            id,
            labels::KNOWLEDGE,
            node.status.as_str(),
            &encode(node)?,
            &knowledge_content(node),
            &ts_text(node.created_at),
            &ts_text(node.updated_at),
            node.embedding.as_deref(),
        )
        .map(|_| ())
    }

    /// Load a knowledge node by id.
    pub fn get_knowledge(&self, id: u64) -> Result<Option<KnowledgeNode>> {
        let Some(props) = self.load_props(id, labels::KNOWLEDGE)? else {
            return Ok(None);
        };
        let mut node: KnowledgeNode = serde_json::from_str(&props)?;
        node.embedding = self.load_embedding(id)?;
        Ok(Some(node))
    }

    /// Find the knowledge node with an exact `(subject, predicate)` match,
    /// returning its id alongside the node.
    pub fn find_knowledge_by_subject(
        &self,
        subject: &str,
        predicate: &str,
    ) -> Result<Option<(u64, KnowledgeNode)>> {
        let conn = self.lock();
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM nodes WHERE label = ?1 \
                 AND json_extract(props, '$.subject') = ?2 \
                 AND json_extract(props, '$.predicate') = ?3 LIMIT 1",
                params![labels::KNOWLEDGE, subject, predicate],
                |r| r.get(0),
            )
            .optional()?;
        drop(conn);
        match id {
            Some(id) => Ok(self.get_knowledge(id as u64)?.map(|node| (id as u64, node))),
            None => Ok(None),
        }
    }

    // ── Semantic layer: Procedural ───────────────────────────────────────

    /// Store a procedural node (updates when [`ProceduralNode::id`] is set).
    pub fn store_procedural(&self, node: &ProceduralNode) -> Result<u64> {
        if let Some(id) = node.id {
            self.update_procedural(node)?;
            return Ok(id);
        }
        self.insert_row(
            labels::PROCEDURAL,
            node.status.as_str(),
            &encode(node)?,
            &procedural_content(node),
            &ts_text(node.created_at),
            &ts_text(node.updated_at),
            non_empty(&node.embedding),
        )
    }

    /// Overwrite an existing procedural node (requires [`ProceduralNode::id`]).
    pub fn update_procedural(&self, node: &ProceduralNode) -> Result<()> {
        let id = require_id(node.id, "procedural")?;
        self.update_row(
            id,
            labels::PROCEDURAL,
            node.status.as_str(),
            &encode(node)?,
            &procedural_content(node),
            &ts_text(node.created_at),
            &ts_text(node.updated_at),
            non_empty(&node.embedding),
        )
        .map(|_| ())
    }

    /// Load a procedural node by id.
    pub fn get_procedural(&self, id: u64) -> Result<Option<ProceduralNode>> {
        let Some(props) = self.load_props(id, labels::PROCEDURAL)? else {
            return Ok(None);
        };
        let mut node: ProceduralNode = serde_json::from_str(&props)?;
        node.id = Some(id);
        node.embedding = self.load_embedding(id)?.unwrap_or_default();
        Ok(Some(node))
    }

    /// Procedural nodes whose `trigger_condition` contains `trigger`
    /// (case-insensitive).
    pub fn find_procedural_by_trigger(
        &self,
        trigger: &str,
        limit: usize,
    ) -> Result<Vec<ProceduralNode>> {
        let pattern = format!("%{}%", escape_like(&trigger.to_lowercase()));
        let conn = self.lock();
        let ids = query_ids(
            &conn,
            "SELECT id FROM nodes WHERE label = ?1 \
             AND lower(json_extract(props, '$.trigger_condition')) LIKE ?2 ESCAPE '\\' LIMIT ?3",
            &[&labels::PROCEDURAL, &pattern, &(limit as i64)],
        )?;
        drop(conn);
        self.load_procedural(ids)
    }

    /// Every procedural node (used by dedup scans).
    pub fn get_all_procedural_nodes(&self) -> Result<Vec<ProceduralNode>> {
        let conn = self.lock();
        let ids = query_ids(
            &conn,
            "SELECT id FROM nodes WHERE label = ?1",
            &[&labels::PROCEDURAL],
        )?;
        drop(conn);
        self.load_procedural(ids)
    }

    // ── Semantic layer: Autobiographical ─────────────────────────────────

    /// Store an autobiographical node.
    ///
    /// `status` is forced to [`NodeStatus::Active`]: self-knowledge never
    /// participates in decay.
    pub fn store_autobiographical(&self, node: &AutobiographicalNode) -> Result<u64> {
        if let Some(id) = node.id {
            self.update_autobiographical(node)?;
            return Ok(id);
        }
        let mut node = node.clone();
        node.status = NodeStatus::Active;
        self.insert_row(
            labels::AUTOBIOGRAPHICAL,
            node.status.as_str(),
            &encode(&node)?,
            &autobiographical_content(&node),
            &ts_text(node.created_at),
            &ts_text(node.updated_at),
            node.embedding.as_deref(),
        )
    }

    /// Overwrite an existing autobiographical node (requires `node.id`).
    ///
    /// An explicit [`NodeStatus::Dormant`] is respected (History compression);
    /// any other status is coerced back to `Active`.
    pub fn update_autobiographical(&self, node: &AutobiographicalNode) -> Result<()> {
        let id = require_id(node.id, "autobiographical")?;
        let mut node = node.clone();
        if node.status != NodeStatus::Dormant {
            node.status = NodeStatus::Active;
        }
        node.updated_at = Utc::now();
        self.update_row(
            id,
            labels::AUTOBIOGRAPHICAL,
            node.status.as_str(),
            &encode(&node)?,
            &autobiographical_content(&node),
            &ts_text(node.created_at),
            &ts_text(node.updated_at),
            node.embedding.as_deref(),
        )
        .map(|_| ())
    }

    /// Load an autobiographical node by id.
    pub fn get_autobiographical(&self, id: u64) -> Result<Option<AutobiographicalNode>> {
        let Some(props) = self.load_props(id, labels::AUTOBIOGRAPHICAL)? else {
            return Ok(None);
        };
        let mut node: AutobiographicalNode = serde_json::from_str(&props)?;
        node.id = Some(id);
        node.embedding = self.load_embedding(id)?;
        Ok(Some(node))
    }

    /// Find the autobiographical node with the given `key`.
    pub fn find_autobiographical_by_key(&self, key: &str) -> Result<Option<AutobiographicalNode>> {
        let conn = self.lock();
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM nodes WHERE label = ?1 \
                 AND json_extract(props, '$.key') = ?2 LIMIT 1",
                params![labels::AUTOBIOGRAPHICAL, key],
                |r| r.get(0),
            )
            .optional()?;
        drop(conn);
        match id {
            Some(id) => self.get_autobiographical(id as u64),
            None => Ok(None),
        }
    }

    /// All autobiographical nodes in a category.
    pub fn find_autobiographical_by_category(
        &self,
        category: AutobioCategory,
    ) -> Result<Vec<AutobiographicalNode>> {
        let conn = self.lock();
        let ids = query_ids(
            &conn,
            "SELECT id FROM nodes WHERE label = ?1 \
             AND json_extract(props, '$.category') = ?2",
            &[&labels::AUTOBIOGRAPHICAL, &category.as_str()],
        )?;
        drop(conn);
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(node) = self.get_autobiographical(id as u64)? {
                out.push(node);
            }
        }
        Ok(out)
    }

    // ── Retrieval primitives (ADR-082 D2/D3) ─────────────────────────────

    /// Exact cosine top-`k` over the label's stored vectors (brute force).
    ///
    /// `ponytail:` reads every BLOB per call instead of caching in memory; the
    /// ADR-082 D2 ceiling is ~100k vectors (≈200 MB resident, single-digit ms).
    /// When the cache lands, only this function changes — f16 → mmap → ANN all
    /// keep the same BLOB format and signature. Vectors whose length differs
    /// from `query` (a provider/model switch) are skipped, not mis-scored.
    pub fn vector_search(&self, label: &str, query: &[f32], k: usize) -> Result<Vec<(u64, f64)>> {
        if query.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let query_norm = l2_norm(query);
        if query_norm == 0.0 {
            return Ok(Vec::new());
        }

        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT vectors.node_id, vectors.embedding FROM vectors \
             JOIN nodes ON nodes.id = vectors.node_id WHERE nodes.label = ?1",
        )?;
        let rows = stmt.query_map(params![label], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;

        let mut scored: Vec<(u64, f64)> = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            let embedding = blob_to_embedding(&blob);
            if embedding.len() != query.len() {
                tracing::debug!(node_id = id, "vector dimension mismatch; skipping");
                continue;
            }
            let denom = query_norm * l2_norm(&embedding);
            if denom == 0.0 {
                continue;
            }
            scored.push((id as u64, dot(query, &embedding) / denom));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        Ok(scored)
    }

    /// Text relevance ranking for `label` (ADR-082 D3).
    ///
    /// Whitespace tokenization only — the term encoding, the trigram-minimum
    /// rule and the `LIKE` fallback all live in
    /// [`Self::text_search_terms`], which this delegates to. Callers that have
    /// already analyzed a query into terms (the Runtime query analyzer, ADR-095
    /// D2) call that function directly instead of re-joining terms into a
    /// string here.
    pub fn text_search(&self, label: &str, query: &str, k: usize) -> Result<Vec<(u64, f64)>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let terms: Vec<String> = query.split_whitespace().map(str::to_string).collect();
        self.text_search_terms(label, &terms, k)
    }

    /// Lexical retrieval driven by a [`LexicalQuery`] — the analyzer's terms
    /// when it produced any, otherwise the raw text.
    ///
    /// This exists so the fallback rule is applied by the executor rather than
    /// re-derived at each call site (ADR-095 D2).
    pub fn text_search_by_query(
        &self,
        label: &str,
        lexical: &LexicalQuery<'_>,
        k: usize,
    ) -> Result<Vec<(u64, f64)>> {
        match lexical.lexical_plan() {
            LexicalPlan::Skip => Ok(Vec::new()),
            LexicalPlan::Terms(terms) => self.text_search_terms(label, terms, k),
            LexicalPlan::Raw(raw) => self.text_search(label, raw, k),
        }
    }

    /// Lexical retrieval over **pre-tokenized, bounded terms** — the only place
    /// in the codebase that quotes/escapes terms for FTS5 (ADR-095 D2: one
    /// escaping implementation, so the analyzer and the executor can never
    /// drift into two different quoting rules).
    ///
    /// Terms are OR-ed as quoted FTS5 phrases. Quoting is what makes this safe:
    /// an operator inside a term (`AND`, `*`, `(`, `-`) is data, not syntax.
    ///
    /// Two deliberate degradations, both inherited from the pre-ADR-095
    /// `text_search` behavior and kept because they bound cost rather than
    /// expand it:
    ///
    /// * terms shorter than the `trigram` tokenizer's minimum are dropped,
    ///   because FTS5 cannot match a pattern shorter than three characters;
    /// * if *every* term is dropped that way (or the list is empty after the
    ///   defensive truncation), the search falls back to a single unscored
    ///   `LIKE` on the first term instead of returning nothing — an empty
    ///   `MATCH ''` silently yields zero rows, which would turn a short query
    ///   into a false "no results".
    ///
    /// `k` bounds the result count; the term list is defensively truncated to
    /// [`MAX_EXECUTED_TERMS`] so an over-long analyzed query cannot amplify
    /// into an over-long `MATCH` expression.
    pub fn text_search_terms(
        &self,
        label: &str,
        terms: &[String],
        k: usize,
    ) -> Result<Vec<(u64, f64)>> {
        let Some(fts) = schema::fts_table(label) else {
            return Ok(Vec::new());
        };
        if k == 0 {
            return Ok(Vec::new());
        }

        let kept: Vec<String> = terms
            .iter()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .take(MAX_EXECUTED_TERMS)
            .map(str::to_string)
            .collect();
        if kept.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.lock();
        let mut hits = Vec::new();

        // Trigram tokenizer: a pattern shorter than three characters can never
        // match, so those terms are dropped (and, if all of them are short,
        // handled by the LIKE fallback below).
        let phrases: Vec<String> = kept
            .iter()
            .filter(|term| term.chars().count() >= TRIGRAM_MIN)
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect();

        if phrases.is_empty() {
            // Every term is shorter than the trigram minimum: FTS5 can match
            // none of them, and an empty `MATCH ''` returns zero rows. Fall
            // back to an OR of `LIKE` patterns so a short query still yields
            // lexical evidence instead of a false "no results" (ADR-095 D2,
            // "空词项退化"). Scores are unscored (`0.0`) — the fusion layer
            // reads this list for its rank order only.
            let patterns: Vec<String> = kept
                .iter()
                .map(|term| format!("%{}%", escape_like(term)))
                .collect();
            let clauses = vec!["content LIKE ? ESCAPE '\\'"; patterns.len()].join(" OR ");
            let sql = format!(
                "SELECT rowid FROM {fts} WHERE {clauses} LIMIT ?{}",
                patterns.len() + 1
            );
            let limit = k as i64;
            let mut args: Vec<&dyn rusqlite::ToSql> =
                patterns.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            args.push(&limit);
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(args.as_slice(), |r| r.get::<_, i64>(0))?;
            for row in rows {
                hits.push((row? as u64, 0.0));
            }
            return Ok(hits);
        }

        // OR the phrases: the trigram tokenizer matches contiguous text, so a
        // single phrase of the whole query would demand the query appear
        // verbatim and a multi-word ask ("网关 route 配置") would return almost
        // nothing. BM25 ranks the union.
        //
        // ponytail: no shingle expansion, so a Chinese query written as one
        // long run still needs its three-character windows to appear
        // contiguously. Upgrade path: split CJK runs into 3-grams in the
        // analyzer (`CjkStrategy::Trigram`) if partial-phrase recall turns out
        // to matter — measured first, per ADR-095 §9.2.
        let phrase = phrases.join(" OR ");
        let mut stmt = conn.prepare(&format!(
            "SELECT rowid, -bm25({fts}) FROM {fts} WHERE {fts} MATCH ?1 \
             ORDER BY bm25({fts}) LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![phrase, k as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        })?;
        for row in rows {
            let (id, score) = row?;
            hits.push((id as u64, score));
        }
        Ok(hits)
    }

    // ── Row access shared with the `acowork-sqlite` sidecars ────────────

    /// Lock the single connection.
    ///
    /// `pub` only because the conversation-index and session-meta sidecars in
    /// `acowork-sqlite` share this connection (ADR-082 §4 step 3) and issue
    /// their own statements against it; treat it as crate-internal elsewhere.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Insert a `nodes` row plus optional vector and FTS entries. Returns the id.
    ///
    /// Generic enough for non-memory labels (the conversation index uses it,
    /// hence `pub` — see [`Self::lock`] for why that seam is public).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_row(
        &self,
        label: &str,
        status: &str,
        props: &str,
        content: &str,
        created_at: &str,
        updated_at: &str,
        embedding: Option<&[f32]>,
    ) -> Result<u64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO nodes(label, status, props, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![label, status, props, created_at, updated_at],
        )?;
        let id = conn.last_insert_rowid();
        if let Some(embedding) = embedding {
            conn.execute(
                "INSERT INTO vectors(node_id, dim, embedding) VALUES (?1, ?2, ?3)",
                params![id, embedding.len() as i64, embedding_to_blob(embedding)],
            )?;
        }
        if let Some(fts) = schema::fts_table(label) {
            // rowid == node id, so deletion and MATCH both address the node id.
            conn.execute(
                &format!("INSERT INTO {fts}(rowid, content, node_id) VALUES (?1, ?2, ?1)"),
                params![id, content],
            )?;
        }
        Ok(id as u64)
    }

    /// Overwrite a `nodes` row and refresh its derived vector / FTS entries.
    ///
    /// A `None` embedding leaves any existing vector untouched (mirrors the
    /// grafeo store's prop-merge update path).
    #[allow(clippy::too_many_arguments)]
    pub fn update_row(
        &self,
        id: u64,
        label: &str,
        status: &str,
        props: &str,
        content: &str,
        created_at: &str,
        updated_at: &str,
        embedding: Option<&[f32]>,
    ) -> Result<bool> {
        let raw_id = id as i64;
        let conn = self.lock();
        let updated = conn.execute(
            "UPDATE nodes SET status = ?2, props = ?3, created_at = ?4, updated_at = ?5 \
             WHERE id = ?1 AND label = ?6",
            params![raw_id, status, props, created_at, updated_at, label],
        )?;
        if updated == 0 {
            return Ok(false);
        }
        if let Some(embedding) = embedding {
            conn.execute(
                "INSERT INTO vectors(node_id, dim, embedding) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(node_id) DO UPDATE SET dim = excluded.dim, embedding = excluded.embedding",
                params![raw_id, embedding.len() as i64, embedding_to_blob(embedding)],
            )?;
        }
        if let Some(fts) = schema::fts_table(label) {
            conn.execute(
                &format!("DELETE FROM {fts} WHERE rowid = ?1"),
                params![raw_id],
            )?;
            conn.execute(
                &format!("INSERT INTO {fts}(rowid, content, node_id) VALUES (?1, ?2, ?1)"),
                params![raw_id, content],
            )?;
        }
        Ok(true)
    }

    pub fn load_props(&self, id: u64, label: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT props FROM nodes WHERE id = ?1 AND label = ?2",
            params![id as i64, label],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn load_embedding(&self, id: u64) -> Result<Option<Vec<f32>>> {
        let conn = self.lock();
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT embedding FROM vectors WHERE node_id = ?1",
                params![id as i64],
                |r| r.get(0),
            )
            .optional()?;
        Ok(blob.map(|b| blob_to_embedding(&b)))
    }

    /// Run an episode id query and materialize the rows.
    fn episodes_where(&self, sql: &str, extra: &[&dyn ToSql]) -> Result<Vec<Episode>> {
        let mut params: Vec<&dyn ToSql> = vec![&labels::EPISODIC];
        params.extend_from_slice(extra);
        let conn = self.lock();
        let ids = query_ids(&conn, sql, &params)?;
        drop(conn);
        self.load_episodes(ids)
    }

    fn load_episodes(&self, ids: Vec<i64>) -> Result<Vec<Episode>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(episode) = self.get_episode(id as u64)? {
                out.push(episode);
            }
        }
        Ok(out)
    }

    fn load_procedural(&self, ids: Vec<i64>) -> Result<Vec<ProceduralNode>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(node) = self.get_procedural(id as u64)? {
                out.push(node);
            }
        }
        Ok(out)
    }
}

// ── Free helpers ─────────────────────────────────────────────────────────

/// Serialize a node to its `props` JSON, projecting `id` and `embedding` out
/// (they live in the `nodes.id` column and the `vectors` table).
///
/// A missing `id` / `embedding` key deserializes back to `None` / empty, so the
/// projection is lossless: callers must pass the same embedding to
/// [`SqliteStore::insert_row`] / [`SqliteStore::update_row`] that the node
/// carries, so `props` and `vectors` cannot disagree.
fn encode<T: serde::Serialize>(node: &T) -> Result<String> {
    let mut value = serde_json::to_value(node)?;
    if let Some(map) = value.as_object_mut() {
        map.remove("id");
        map.remove("embedding");
    }
    Ok(serde_json::to_string(&value)?)
}

fn require_id(id: Option<u64>, kind: &str) -> Result<u64> {
    id.ok_or_else(|| Error::Memory(format!("cannot update {kind} node without an ID")))
}

/// Empty vector means "no embedding" in the memory contract (ADR-057 §5.1).
fn non_empty(embedding: &[f32]) -> Option<&[f32]> {
    if embedding.is_empty() {
        None
    } else {
        Some(embedding)
    }
}

/// Fold two promotion records together after a dedup merge.
///
/// The surviving record is the newer one — it describes the statement that is
/// actually in the row now — but the evidence list is the union the caller
/// computed, since both episodes really did back this statement.
fn union_promotion_metadata(
    existing: Option<&PromotionMetadata>,
    incoming: Option<&PromotionMetadata>,
    source_episode_ids: &[u64],
) -> Option<PromotionMetadata> {
    let sides: Vec<&PromotionMetadata> = existing.into_iter().chain(incoming).collect();
    let newest = sides.iter().max_by_key(|m| m.promoted_at)?;
    Some(PromotionMetadata {
        evidence_episode_ids: source_episode_ids.to_vec(),
        promoted_at: newest.promoted_at,
        promoted_by: newest.promoted_by.clone(),
        evidence_span_days: sides.iter().map(|m| m.evidence_span_days).max().unwrap_or(0),
        llm_judge_confidence: sides
            .iter()
            .map(|m| m.llm_judge_confidence)
            .fold(0.0, f32::max),
        llm_judge_reasoning: newest.llm_judge_reasoning.clone(),
    })
}

/// FTS content for a knowledge node — matches `KnowledgeNode::to_properties`.
fn knowledge_content(node: &KnowledgeNode) -> String {
    format!("{} {} {}", node.subject, node.predicate, node.object)
}

/// FTS content for a procedural node — matches `ProceduralNode::to_properties`.
fn procedural_content(node: &ProceduralNode) -> String {
    format!(
        "{} {} {}",
        node.name, node.trigger_condition, node.action_pattern
    )
}

/// FTS content for an autobiographical node — matches its `to_properties`.
fn autobiographical_content(node: &AutobiographicalNode) -> String {
    format!("{}: {}", node.key, node.value)
}

/// Fixed-width RFC3339 (microsecond precision, UTC `Z`). Lexicographic order
/// equals chronological order, which is what the `created_at` sorts rely on.
fn ts_text(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(embedding.len() * 4);
    for value in embedding {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

// `clippy::chunks_exact_to_as_chunks` prefers `slice::as_chunks`, but that is
// a rustc 1.99 API. The fleet is heterogeneous — the gateway builds on 1.99
// while remote nodes and desktop machines are on 1.96 — so the workspace MSRV
// stays at 1.95 and we keep `chunks_exact` here rather than force every machine
// to upgrade rustc for one helper. Revisit once the fleet MSRV reaches 1.99.
//
// `unknown_lints` rides along because the asymmetry cuts both ways: on the
// 1.96 end of the fleet the lint name itself does not exist, and `cargo
// clippy -- -D warnings` (dev/ci.sh) would fail the build on the attribute
// rather than on the code it suppresses.
#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum()
}

fn l2_norm(v: &[f32]) -> f64 {
    v.iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let denom = l2_norm(a) * l2_norm(b);
    if denom == 0.0 { 0.0 } else { dot(a, b) / denom }
}

/// Collect the first column of every row as `i64`.
fn query_ids(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Escape `%`, `_` and `\` for a `LIKE ... ESCAPE '\'` pattern.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod blob_tests {
    use super::*;

    #[test]
    fn embedding_blob_round_trips() {
        let v = vec![1.5f32, -2.25, 0.0, 9.81];
        assert_eq!(blob_to_embedding(&embedding_to_blob(&v)), v);
    }

    #[test]
    fn blob_ignores_trailing_partial_f32() {
        // 3 whole f32s + 2 stray bytes: as_chunks drops the 2-byte tail,
        // matching the old chunks_exact(4) semantics.
        let mut blob = embedding_to_blob(&[1.0, 2.0, 3.0]);
        blob.extend_from_slice(&[0xff, 0xff]);
        assert_eq!(blob_to_embedding(&blob), vec![1.0, 2.0, 3.0]);
    }
}

