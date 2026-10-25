//! `acowork-sqlite` — the Runtime-side sidecars that share the workspace
//! SQLite connection (ADR-082 §4 step 3).
//!
//! The storage engine itself ([`SqliteStore`], its schema, the
//! [`acowork_memory::MemoryProvider`] / [`acowork_memory::MemoryAdminService`]
//! implementations and the hybrid retrieval engine) moved to
//! `acowork_memory::sqlite` with ADR-101: the memory service binary is hosted by
//! that crate and has to link a concrete backend, and a backend crate cannot
//! depend on the crate whose traits it implements. This crate keeps the two
//! subsystems that are *not* memory and stay in the Runtime:
//!
//! * [`conversation::ConversationStore`] — the conversation vector index
//!   (ADR-081);
//! * [`SqliteSessionMetaStore`] — session metadata rows (ADR-082 §4 step 3).
//!
//! Both attach to an already-open `SqliteStore` so memory, session meta and the
//! conversation index keep living in one `memory/private.sqlite` file on one
//! connection. The type is re-exported below, so existing callers keep using
//! `acowork_sqlite::SqliteStore` unchanged.

pub mod conversation;
mod session_meta;

pub use acowork_memory::sqlite::{
    Error, Result, SqliteStore, SCHEMA_SQL, SCHEMA_VERSION, TRIGRAM_MIN, has_sqlite_magic,
    schema_version,
};
pub use conversation::{ConversationHit, ConversationStore};
pub use session_meta::SqliteSessionMetaStore;

