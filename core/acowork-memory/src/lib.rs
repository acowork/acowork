//! acowork-memory - MemoryProvider trait, shared memory types, and the SQLite
//! workspace store that implements them.
//!
//! This crate defines the MemoryProvider trait abstraction, the shared memory
//! types, the storage-agnostic logic built on top of them (the episodic
//! distiller and the retrieval manager), and the SQLite storage engine
//! ([`sqlite::SqliteStore`]) that backs them. It also hosts the standalone
//! memory **service** ([`service`], ADR-101) and the panel-facing wire format
//! ([`wire`]) that both the service and the Runtime endpoints it replaces speak.
//!
//! The store lives here, not in a separate backend crate, because the memory
//! service binary (ADR-101) is hosted by this crate and must link a concrete
//! backend: a backend crate cannot depend on the crate whose traits it
//! implements. `acowork-sqlite` keeps only the Runtime-side sidecars that share
//! the same connection (conversation index, session meta) and re-export the
//! store type.
//!
//! [`MemoryProvider`] (ADR-051) is the single storage abstraction: consolidation,
//! CRUD and lifecycle control all sit on it.
//!
//! Design ref: ADR-051, ADR-082, ADR-101, docs/05-memory.md §10

pub mod admin;
pub mod consolidation;
pub mod keyword;
pub mod manager;
pub mod provider;
pub mod quality;
pub mod service;
pub mod sqlite;
pub mod types;
pub mod wire;

// Re-exports: the standalone service (ADR-101) — config, process entry parts.
pub use service::{Cli, MemoryService, MemoryServiceConfig, MemoryState, ServiceError};

// Re-exports: the SQLite workspace store (ADR-082) and its error surface.
pub use sqlite::{
    Error as SqliteError, ImportCounts, Result as SqliteResult, SqliteStore, CONVERSATION_LABEL,
    MAX_EXECUTED_TERMS, RRF_K, SCHEMA_SQL, SCHEMA_VERSION, TRIGRAM_MIN, has_sqlite_magic,
};

// Re-exports: MemoryProvider trait (ADR-051 — the only storage abstraction)
pub use provider::MemoryProvider;

// Re-exports: MemoryAdminService trait (ADR-051 P4, admin/management operations)
pub use admin::{
    AdminListNodesOutput, AdminListNodesParams, AdminNodeDetail,
    AdminNodeRecord, AdminStats, MemoryAdminService, RebuildStats,
};

// Re-exports: MemoryManager + associated types (ADR-051 P2, moved from runtime)
pub use manager::{MemoryManager, MemoryManagerConfig, RetrievalResult, RetrievedMemory};

// Re-exports: consolidation types
pub use consolidation::{
    DefaultEpisodicDistiller, EpisodicDistiller,
    AutobioAspect, BehaviorPattern, ConflictAction, ConflictResolutionDetail, DistillerConfig,
    DistillerResult, EmbeddingFn,
    GeneralizationConfig, GeneralizationResult, LlmMessage, LlmResponse, MemoryStoreInput,
    MemoryStoreResult, OfflineConsolidationConfig, PatternCategory,
    PromotionDecision, PromotionEvaluation, PromotionKind, PromotionMetadata, SchedulerConfig,
    ConsolidationLlm,
};

// Re-exports: memory quality config (ADR-062 D2)
pub use quality::{ConsolidationQuality, DedupQuality, MemoryQualityConfig};

// Re-exports: core memory types
//
// Session-metadata types are deliberately not re-exported here (ADR-101): they
// live in `acowork_core::session_meta`, which both the Runtime and the SQLite
// store can name without pulling in this crate.
pub use types::{
    AutobioCategory, AutobiographicalNode, CollaborationSpan, ConflictSignal, ConflictType,
    ContextSource, DEFAULT_EMBEDDING_DIM, DecayScanResult, DistilledEpisode, Episode,
    EpisodicDecayConfig, KnowledgeNode, KnowledgeSubType, MemoryContext, MemoryNode, MemoryQuery,
    NodeStatus, PrivacyLevel, ProceduralNode, PurgeResult, ResultSource,
    SearchResult, StoreHealth, StoreStats,
};

// Label constants
pub use types::labels;
pub use types::{HintType, MemoryFilters, NodeTypeFilter};


