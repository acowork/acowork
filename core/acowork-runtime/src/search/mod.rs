//! Conversation/memory retrieval orchestration shared by `/search` and
//! `memory_recall` (ADR-095 D2, design 28 §3.1).
//!
//! Two responsibilities live here, and neither owns storage nor HTTP state:
//!
//! * [`analyzer`] — a deterministic, bounded query analyzer that turns one raw
//!   query into the dual query expression retrieval consumes.
//! * [`policy`] — loads the fusion knobs from agent configuration and produces
//!   the [`acowork_core::retrieval::FusionPolicy`] values the engine takes.
//!
//! # Why this is a Runtime module and not a `acowork-memory` one
//!
//! The analyzer serves conversation retrieval *and* memory retrieval. Putting
//! it in `acowork-memory` would make the conversation path depend on the
//! memory crate's domain types; putting it in `acowork-sqlite` would push a
//! language concern into the storage layer. `acowork-core::retrieval` holds
//! only the shared *contract* types (`FusedHit`, `FusionPolicy`,
//! `MatchedBy`) because `acowork-memory` and `acowork-sqlite` both need them;
//! the query-producing logic is Runtime-side orchestration, so it stays here.
//!
//! Dependency red line (design 28 §5.3, checked by `dev/ci.sh`): this module
//! MUST NOT import `acowork-memory`. When the memory path needs analyzed
//! terms, the Runtime orchestration layer passes them through the existing
//! `MemoryQuery` fields — a data flow, not a dependency edge.

pub mod analyzer;
pub mod policy;

pub use analyzer::{AnalyzerConfig, AnalyzerMode, CjkStrategy, DualQuery, analyze};
pub use policy::SearchPolicyConfig;
