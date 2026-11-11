//! acowork-doc — online document library service.
//!
//! Provides CRUD over a shared document tree, optimistic-version concurrency,
//! a PR-style update review flow (Agent submits, human approves), recycle bin,
//! and cross-directory keyword search. Exposed via:
//!
//! - **REST API** (axum) — reverse-proxied by the Gateway (`/api/doc/*` public,
//!   internal routes without the `/api` prefix)
//! - **MCP tools** (HTTP server) — Agents call `doc_*` tools (D3)
//!
//! ## Storage model: directory tree + per-dir `library.json`
//!
//! The filesystem is the source of truth: directories = folders, documents =
//! `.md` files, file name (sans suffix) = title. Each directory (including
//! the root) owns a `library.json` that only manages its own `files[]`
//! metadata and `dirs[]` lightweight child references; there is no global
//! tree index (design §3).
//!
//! ## Cross-service integration
//!
//! | Caller | Protocol | Entry |
//! |--------|----------|-------|
//! | Gateway HTTP API | REST over axum (reverse proxy) | `acowork-gateway::http::doc_proxy` |
//! | Agent (local/remote) | MCP HTTP | `acowork-doc::mcp::tools` (D3) |
//! | Desktop UI | REST | same Gateway path |
//!
//! The doc process runs standalone, spawned/supervised by the Gateway
//! (ADR-064 pattern); the Gateway reverse-proxies `/api/doc/*` →
//! `127.0.0.1:{doc_port}/*`.
//!
//! ## Design reference
//!
//! - Service design: `docs/design/zh/20-doc-online-document.md`
//! - Dev plan: `docs/plan/zh/doc-dev-plan.md` (D0 → D4 milestones)

pub mod api;
pub mod cli;
pub mod config;
pub mod error;
pub mod gateway_notify;
pub mod health;
pub mod limit;
pub mod mcp;
pub mod mqtt_publisher;
pub mod path;
pub mod server;
pub mod service;
pub mod state;
pub mod store;
pub mod types;

// ─────────────────────────────────────────────────────────────────────────
// Re-exports: core types (crate-level public API)
// ─────────────────────────────────────────────────────────────────────────

// Config
pub use config::DocConfig;

// Errors
pub use error::{DocError, Result};

// Domain models (design §3.2 / §5.3)
pub use types::{
    generate_dir_id, generate_doc_id, generate_request_id, generate_trash_id, DirId, DirMeta,
    DocId, DocMeta, DocRead, ImportSource, LibraryIndex, RequestId, RequestStatus, SearchHit,
    TreeNode, TrashEntry, UpdateRequest, DIR_ID_PREFIX, DOC_ID_PREFIX, REQUEST_ID_PREFIX,
    ROOT_DIR_ID, TRASH_ID_PREFIX,
};

// Path-safety utilities (design §9)
pub use path::{
    ensure_within_library, validate_dir_id, validate_doc_id, validate_doc_name,
    validate_relative_path, validate_request_id,
};

// Storage primitives
pub use store::library::LibraryStore;
pub use store::requests::RequestsStore;
pub use store::trash::TrashStore;

// Service
pub use server::DocService;

// State
pub use state::DocState;

// Service-layer traits / impls (re-exported for api/, tests)
pub use service::{
    ApproveOutcome, CreateDirectoryInput, CreateDocumentInput, DirectoryService,
    DocumentService, LibraryDirectoryService, LibraryDocumentService, LibraryRequestService,
    RequestService, SubmitRequestInput, UpdateDocumentInput,
};
