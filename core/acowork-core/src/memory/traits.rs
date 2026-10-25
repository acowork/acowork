//! Shared memory value types.
//!
//! Only the wire-stable node types live here, because several crates have to
//! name them (`acowork-gateway` intent privacy, `acowork-memory`,
//! `acowork-runtime`) without depending on the storage layer. The storage
//! abstraction itself is `acowork_memory::MemoryProvider` (ADR-051); the old
//! `MemoryStore` trait stub that used to sit in this module had no
//! implementation left and was removed with ADR-101.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Memory node with metadata
///
/// ⚠️ NOTE: The `zone` field is defined but NOT currently used in Phase 1-3.
/// Zone functionality is deferred to Phase 4+. Currently all nodes belong to
/// the `default` zone. See docs/05-memory.md §8.2 for details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNode {
    pub id: String,
    pub content: String,
    pub metadata: Value,
    /// Business scenario zone (e.g., "work", "personal", "system").
    /// ⚠️ UNUSED in Phase 1-3. Reserved for Phase 4+.
    pub zone: String,
    pub privacy_level: PrivacyLevel,
}

/// Privacy level for memory nodes
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum PrivacyLevel {
    /// Shareable across agents (e.g. the user's name) — kept when a package is
    /// packed and shared.
    Public,
    /// Private to this agent (e.g. preferred coding style) — stripped when a
    /// package is shared (the conservative default).
    #[default]
    Personal,
    /// Sensitive information — stripped when a package is shared.
    Sensitive,
}

