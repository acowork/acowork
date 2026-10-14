//! MemoryAdminService trait - administrative interface for memory engines.
//!
//! This trait provides management/admin operations used by HTTP admin
//! endpoints (`/memory/nodes`, `/memory/stats`, `/memory/nodes/{nid}`)
//! and embedding-dimension migration. It is intentionally separate from
//! [`crate::MemoryProvider`] (which serves the agent loop) to follow
//! Interface Segregation:
//!
//! - `MemoryProvider`: retrieve, inject, record, consolidation lifecycle
//!   — used by `loop_memory.rs`, `MemoryManager`, tools.
//! - `MemoryAdminService`: list/get/create/update/delete nodes, stats,
//!   consolidation trigger, embedding migration — used by HTTP admin
//!   endpoints and session bootstrap.
//!
//! A concrete engine (e.g., `GrafeoStore`) typically implements both
//! traits. A remote/test provider may implement only `MemoryProvider`
//! and skip `MemoryAdminService` (admin endpoints will report
//! "unavailable").
//!
//! Design ref: ADR-051 §4.4.1

use std::collections::HashMap;

use acowork_core::error::Result;

// ── Admin types (engine-independent) ──────────────────────────────────

/// One row in a memory list response.
///
/// Mirrors the wire-format contract consumed by the Desktop Memory panel.
/// Fields are engine-agnostic: no grafeo types leak through.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminNodeRecord {
    pub node_id: u64,
    pub node_type: String,
    /// Secondary classification inside the storage layer.
    ///
    /// For `Knowledge` nodes: `Fact` | `Preference` | `Relation` | `Procedure`.
    /// For `Autobiographical` nodes: `Identity` | `Capability` | `Limitation`
    /// | `Preference` | `History` | `Relationship`. `None` for `Episodic`
    /// and `Procedural` nodes (which have no sub-classification).
    ///
    /// Drives the memory panel's per-`Autobiographical` sub-filter and the
    /// Knowledge column-grouping in the desktop UI. Serialised with
    /// `skip_serializing_if = "Option::is_none"` so older clients see no
    /// change in the JSON wire format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_type: Option<String>,
    pub content: String,
    /// Raw `confidence` property on the node (0.0 when the node type has no
    /// such property — e.g. Episodic). Passed through verbatim, never derived.
    pub confidence: f64,
    /// Raw `importance` property on the node (0.0 when the node type has no
    /// such property — e.g. Procedural/Autobiographical). Passed through
    /// verbatim, never derived.
    pub importance: f64,
    pub decay_score: f64,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: u32,
    pub status: String,
}

/// Parameters for [`MemoryAdminService::list_nodes`].
#[derive(Debug, Clone, Default)]
pub struct AdminListNodesParams {
    /// 1-based page number.
    pub page: u32,
    /// Page size (clamped to 1..=100 by the implementation).
    pub size: u32,
    /// Filter by node type ("Episodic" / "Knowledge" / "Procedural" /
    /// "Autobiographical"). Empty string = no filter.
    pub node_type: String,
    /// Filter by sub-classification within the chosen `node_type`:
    ///
    /// - `Knowledge`: `Fact` | `Preference` | `Relation` | `Procedure`
    /// - `Autobiographical`: `Identity` | `Capability` | `Limitation`
    ///   | `Preference` | `History` | `Relationship`
    ///
    /// Empty string = no filter. Only meaningful when `node_type` is set
    /// to `Knowledge` or `Autobiographical`; for other types the filter
    /// is ignored because those labels have no sub-classification.
    ///
    /// This is the lever the memory panel uses to drill into, e.g.,
    /// "Autobiographical → Limitation" without fetching the whole label.
    pub sub_type: String,
    /// Case-insensitive substring filter applied to the rendered content.
    /// Empty string = no filter.
    pub keyword: String,
    /// Time-range bucket. Supported values: "1h", "1d", "7d", "30d",
    /// "all", "". Empty string = no filter.
    pub time_range: String,
}

/// Result of a list-nodes query.
#[derive(Debug, Clone)]
pub struct AdminListNodesOutput {
    pub total: u64,
    pub page: u32,
    pub size: u32,
    pub nodes: Vec<AdminNodeRecord>,
    /// When `Some`, the unfiltered scan was rejected because the database
    /// exceeds the implementation's safety limit.
    pub rejected_unfiltered: Option<u64>,
}

/// Result of a single-node GET query.
///
/// `found == false` with `message == "Memory store not available"`
/// indicates the engine has not been initialised yet.
#[derive(Debug, Clone)]
pub struct AdminNodeDetail {
    pub node_id: u64,
    pub found: bool,
    pub node_type: String,
    /// Secondary classification (see [`AdminNodeRecord::sub_type`]).
    pub sub_type: Option<String>,
    pub content: String,
    /// Raw `confidence` property on the node (0.0 when the node type has no
    /// such property — e.g. Episodic). Passed through verbatim, never derived.
    pub confidence: f64,
    /// Raw `importance` property on the node (0.0 when the node type has no
    /// such property — e.g. Procedural/Autobiographical). Passed through
    /// verbatim, never derived.
    pub importance: f64,
    pub decay_score: f64,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: u32,
    pub status: String,
    /// All node properties, serialised as JSON values. Empty when
    /// `found == false`.
    pub properties: HashMap<String, serde_json::Value>,
    pub message: String,
}

/// Detailed memory statistics for the admin UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdminStats {
    pub total_nodes: u64,
    pub storage_bytes: u64,
    pub by_type: HashMap<String, u64>,
    pub by_status: HashMap<String, u64>,
    pub index_health: String,
    /// Embedding dimension stored in the engine's vector indexes.
    pub stored_dim: u64,
    /// Number of nodes that have an embedding vector.
    pub nodes_with_embedding: u64,
    /// Vectors stored at a width other than the engine's recorded embedding
    /// dimension.
    ///
    /// This is the signal a model swap can leave behind: the recorded
    /// dimension is updated while some rows still hold the previous width, and
    /// vector search skips foreign-width rows silently. Those rows are not
    /// missing - they are invisible - so no count of *missing* embeddings can
    /// report them, and a comparison of declared dimensions cannot either.
    pub vectors_of_other_dim: u64,
    /// On-disk schema version of the underlying store.
    ///
    /// For SQLite-backed implementations this is `PRAGMA user_version`, a
    /// 32-bit integer the engine bumps when the schema layout changes. The
    /// desktop Memory panel surfaces this on the overview strip so an
    /// operator can tell at a glance which schema an agent's store is on
    /// (and spot drift across machines). Engines without a comparable
    /// schema-version knob (e.g. Grafeo, which is being phased out) report
    /// 0.
    pub schema_version: u64,
}


/// Which rows a rebuild re-embeds.
///
/// The distinction matters because the two failure modes have different
/// repair scopes:
///
/// - A model swap changes the width *and* the meaning of every vector, so
///   nothing but a full re-embed can heal the store — vectors from two
///   models are not comparable even when they share a width.
/// - A half-applied swap (or a write that raced one) leaves a handful of
///   rows at a foreign width, or none at all, while the store's declared
///   dimension already matches the active model. `vector_search` skips
///   foreign-width rows silently, so those rows — and only those — are
///   invisible to retrieval; re-embedding the rest is pure cost.
///
/// Wire format: `snake_case` strings (`"all"`, `"repair"`). Absent on the
/// wire reads as `All`, which is what a client predating this field asked
/// for anyway.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RebuildScope {
    /// Every row in the store, regardless of its current vector.
    #[default]
    All,
    /// Only rows whose vector is missing or stored at a width other than
    /// the target dimension — exactly the rows retrieval cannot see.
    Repair,
}

/// Statistics returned by embedding dimension migration.
///
/// Mirrors `GrafeoStore::RebuildStats` but without engine-specific types.
#[derive(Debug, Clone, Default)]
pub struct RebuildStats {
    pub total_scanned: u64,
    pub rebuilt: u64,
    pub skipped_no_embedding: u64,
    pub skipped_no_content: u64,
    pub errors: u64,
}

// ── Trait definition ──────────────────────────────────────────────────

/// Administrative interface for memory storage engines.
///
/// All methods are synchronous because they perform in-process graph
/// operations without I/O. Implementations that need async (e.g., a
/// remote HTTP backend) can bridge internally.
///
/// Design ref: ADR-051 §4.4.1
pub trait MemoryAdminService: Send + Sync {
    // ── Node CRUD ─────────────────────────────────────────────────────

    /// List memory nodes with pagination, filtering, and search.
    fn list_nodes(&self, params: &AdminListNodesParams) -> AdminListNodesOutput;

    /// Semantic (vector/hybrid) search over memory nodes (ADR-081 P1-1).
    ///
    /// `mode` ∈ `{"vector", "hybrid"}`; when `embedding` is `None` the
    /// engine falls back to text search (embedding model not ready).
    /// Returns up to `limit` records ranked by descending relevance score.
    fn semantic_search(
        &self,
        query_text: &str,
        embedding: Option<&[f32]>,
        mode: &str,
        limit: usize,
    ) -> Vec<AdminNodeRecord>;

    /// Get a single node's full detail by numeric ID.
    fn get_node(&self, node_id: u64) -> AdminNodeDetail;

    /// Create a new memory node with the given label and property map.
    ///
    /// Returns the new `node_id`.
    fn create_node(
        &self,
        label: &str,
        properties: &HashMap<String, serde_json::Value>,
    ) -> Result<u64>;

    /// Update (merge) properties on an existing node.
    ///
    /// Returns `Err` if the node does not exist (so the HTTP layer can
    /// map it to a 404).
    fn update_node(
        &self,
        node_id: u64,
        properties: &HashMap<String, serde_json::Value>,
    ) -> Result<()>;

    /// Delete a memory node by ID. Returns `true` if found and deleted.
    fn delete_node(&self, node_id: u64) -> bool;

    // ── Statistics ────────────────────────────────────────────────────

    /// Collect detailed memory statistics for the admin UI.
    fn get_stats(&self) -> AdminStats;

    // ── Embedding migration ──────────────────────────────────────────

    /// Get the embedding dimension stored in the engine's vector indexes.
    fn embedding_dim(&self) -> usize;

    /// Count nodes that have an embedding vector.
    fn count_nodes_with_embedding(&self) -> u64;

    /// Re-embed all nodes with a new embedding function and dimension.
    ///
    /// `embed_fn` is a synchronous closure that takes a text string and
    /// returns an embedding vector of length `new_dim`. Callers are
    /// responsible for bridging async providers.
    fn migrate_embedding_dimension(
        &self,
        embed_fn: &(dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync),
        new_dim: usize,
    ) -> Result<RebuildStats>;

    /// Like [`Self::migrate_embedding_dimension`], but reports progress.
    ///
    /// `progress` is called with `(processed, total)` as nodes are
    /// re-embedded (from a blocking thread). Default implementation just
    /// delegates to the non-progress variant; engines that can report
    /// progress (Grafeo) override it.
    fn migrate_embedding_dimension_with_progress(
        &self,
        embed_fn: &(dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync),
        new_dim: usize,
        progress: Option<&dyn Fn(u64, u64)>,
    ) -> Result<RebuildStats> {
        let _ = progress;
        self.migrate_embedding_dimension(embed_fn, new_dim)
    }

    /// Like [`Self::migrate_embedding_dimension_with_progress`], but limited
    /// to the rows named by `scope`.
    ///
    /// The default implementation ignores the scope and re-embeds everything
    /// — correct for every scope, just not cheap. Engines that can select
    /// rows by stored width (SQLite) override it so a repair of a handful of
    /// foreign-width rows costs a handful of embed calls, not a full scan.
    fn migrate_embedding_dimension_scoped(
        &self,
        embed_fn: &(dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync),
        new_dim: usize,
        scope: RebuildScope,
        progress: Option<&dyn Fn(u64, u64)>,
    ) -> Result<RebuildStats> {
        let _ = scope;
        self.migrate_embedding_dimension_with_progress(embed_fn, new_dim, progress)
    }

    /// How many rows [`Self::migrate_embedding_dimension_scoped`] would
    /// re-embed for `scope` against a target width of `dim`.
    ///
    /// The desktop Memory panel shows this as the banner's promise — "this
    /// rebuild will process N rows" — so the number a user reads before
    /// clicking and the `total` the progress bar reports afterwards come
    /// from one definition instead of two, and cannot disagree. Engines
    /// that can select rows by stored width (SQLite) count with the exact
    /// same predicate the scoped migration selects with.
    ///
    /// The default reports the node total for every scope: that is what
    /// the default scoped migration above actually does (it ignores the
    /// scope and re-embeds everything), so the promise stays honest even
    /// on engines without width-aware selection.
    fn rebuild_scope_count(&self, scope: RebuildScope, dim: usize) -> u64 {
        let _ = (scope, dim);
        self.get_stats().total_nodes
    }
}
