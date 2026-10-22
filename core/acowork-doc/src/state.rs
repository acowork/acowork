//! acowork-doc shared service state.
//!
//! D0 skeleton: held the service config. D1 attaches the document /
//! directory service instances plus the underlying [`LibraryStore`], so
//! every REST handler and MCP tool shares one in-memory handle.
//!
//! All state is `Arc`'d: services are cheap to clone (one `Arc` bump)
//! and safe to share with axum's `State<T>` extractor and `tokio::spawn`
//! background tasks.

use std::sync::Arc;

use crate::config::DocConfig;
use crate::error::Result;
use crate::service::{
    AclService, DocumentService, LibraryDirectoryService, LibraryDocumentService,
    LibraryRequestService, LibrarySearchService, LibraryTrashService, TrashService,
};
use crate::store::library::LibraryStore;
use crate::store::requests::RequestsStore;
use crate::store::trash::TrashStore;

/// Shared state handed to every axum handler / MCP tool via `State<T>`.
#[derive(Clone)]
pub struct DocState {
    /// Service config (validated at startup).
    pub config: Arc<DocConfig>,
    /// Identifies the calling actor for authorization (design §9).
    ///
    /// Populated by the Gateway reverse proxy (`X-Actor: human` for REST,
    /// `X-MCP-Actor` for MCP). `None` = anonymous (read-only tools only).
    pub actor: Option<String>,
    /// Document CRUD + version concurrency (design §4).
    pub docs: Arc<LibraryDocumentService>,
    /// Directory CRUD + tree listing (design §4).
    pub dirs: Arc<LibraryDirectoryService>,
    /// PR-style update review flow (design §5).
    pub requests: Arc<LibraryRequestService>,
    /// Recycle bin (design §3.3 / §4 trash endpoints).
    pub trash: Arc<LibraryTrashService>,
    /// Cross-directory keyword search (design §4 `GET /api/search`).
    pub search: Arc<LibrarySearchService>,
    /// Node-level view/edit permissions + ACL management (design §9).
    pub acl: Arc<AclService>,
}

impl DocState {
    /// Build the state from a validated config. Wires the underlying
    /// `LibraryStore` to all services so they share the same disk view,
    /// and materialises the library root (`library.json` + `.trash/`) so
    /// the first request sees a valid tree.
    pub async fn new(config: DocConfig) -> Result<Self> {
        let store = LibraryStore::new(config.data_dir.clone());
        store.ensure_root().await?;
        let docs = Arc::new(LibraryDocumentService::new(store.clone()));
        let dirs = Arc::new(LibraryDirectoryService::new(store.clone()));
        let docs_dyn = docs.clone() as Arc<dyn DocumentService>;
        // RequestService reviews by merging through the DocumentService
        // (approve = update with base_version), TTL from config.
        let requests = Arc::new(LibraryRequestService::new(
            docs_dyn.clone(),
            RequestsStore::new(store.root().to_path_buf()),
            config.request_ttl_hours,
        ));
        // TrashService re-creates documents through the DocumentService
        // trait on restore; retention comes from config; startup runs a
        // lazy purge (design §3.3 "30 天后清理" — no background task).
        let trash = Arc::new(LibraryTrashService::new(
            docs_dyn,
            TrashStore::new(store.root().to_path_buf()),
            config.trash_retention_days,
        ));
        let search = Arc::new(LibrarySearchService::new(store.clone()));
        let acl = Arc::new(AclService::new(store.clone()));
        trash.purge_expired().await?;
        Ok(Self {
            config: Arc::new(config),
            actor: None,
            docs,
            dirs,
            requests,
            trash,
            search,
            acl,
        })
    }
}
