//! Service layer — business logic abstracted behind traits (ADR-040 style).
//!
//! Layering:
//! - `document.rs` — `DocumentService` trait: read, create, update (with
//!   version concurrency), rename, move, delete. The single audit point
//!   for version-number semantics (design §4 PUT / §5.4).
//! - `document_impl.rs` — concrete implementation that owns a `LibraryStore`
//!   and a clock (`Fn() -> DateTime<Utc>`) for testability.
//! - `directory.rs` / `directory_impl.rs` — `DirectoryService` for
//!   create / list / rename / delete on directory nodes.
//! - `acl.rs` — `AclService`: node-level view/edit permissions with
//!   closest-ancestor inheritance and creator/admin management (design §9).
//! - `request.rs` / `request_impl.rs` — `RequestService` PR-style review
//!   flow: submit (base_version check), list, approve (merge + version+1),
//!   reject, TTL expiry (design §5).
//! - `trash.rs` / `trash_impl.rs` — `TrashService` recycle-bin listing,
//!   restore, and 30-day lazy purge.
//! - `search.rs` / `search_impl.rs` — `SearchService` cross-directory
//!   keyword scan (no global index, design §3).
//!
//! API-layer modules in `crate::api::*` depend on these traits only — never
//! on `crate::store::*` or `crate::types` directly. This lets the on-the-
//! wire schema evolve independently of the on-disk schema, and keeps
//! version / lock semantics in one auditable place.

pub mod acl;
pub mod directory;
pub mod directory_impl;
pub mod document;
pub mod document_impl;
pub mod request;
pub mod request_impl;
pub mod search;
pub mod search_impl;
pub mod trash;
pub mod trash_impl;

pub use acl::{
    AclService, AclTarget, ActorCtx, Effective, LOCAL_HUMAN_ACTOR, PermissionsView,
};
pub use directory::{CreateDirectoryInput, DirectoryService};
pub use directory_impl::LibraryDirectoryService;
pub use document::{CreateDocumentInput, DocumentService, UpdateDocumentInput};
pub use document_impl::LibraryDocumentService;
pub use request::{ApproveOutcome, RequestService, SubmitRequestInput};
pub use request_impl::LibraryRequestService;
pub use search::SearchService;
pub use search_impl::LibrarySearchService;
pub use trash::TrashService;
pub use trash_impl::LibraryTrashService;
