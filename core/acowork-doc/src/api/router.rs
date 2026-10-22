//! Axum router assembly for the doc service.
//!
//! Routes are mounted **without** an `/api` prefix — the Gateway
//! reverse proxy strips `/api/doc` before forwarding (see
//! `crate::server::DocService::router`).
//!
//! Conventions:
//! - `POST   /docs`           create
//! - `GET    /docs?dir_id=…`  list under dir
//! - `GET    /docs/{id}`      read (meta + content)
//! - `PUT    /docs/{id}`      update (versioned)
//! - `PATCH  /docs/{id}/title` rename
//! - `POST   /docs/{id}/move`  move
//! - `DELETE /docs/{id}`      soft-delete → .trash/
//! - `GET    /docs/{id}/path`  on-disk relative path
//! - `GET    /docs/{id}/requests` per-doc review history
//! - `POST   /dirs`           create subdir
//! - `GET    /dirs/{id}`      read dir meta
//! - `PATCH  /dirs/{id}/name` rename
//! - `DELETE /dirs/{id}`      cascade-delete
//! - `GET    /tree?dir_id=…`  immediate children
//! - `POST   /requests`       submit update proposal (agent)
//! - `GET    /requests?status=` review queue
//! - `GET    /requests/{id}`  request detail / status
//! - `POST   /requests/{id}/approve` review-approve (merge)
//! - `POST   /requests/{id}/reject`  review-reject (note)
//! - `GET    /trash`          recycle-bin list
//! - `POST   /trash/{id}/restore`
//! - `DELETE /trash/{id}`     purge forever
//! - `GET    /search?keyword=` cross-directory search
//! - `GET    /permissions/{kind}/{id}` node ACL view (design §9)
//! - `PUT    /permissions/{kind}/{id}` replace node ACL (creator/admin)

use axum::routing::{delete, get, patch, post, put};
use axum::Router;

use crate::api::dirs as d;
use crate::api::docs as c;
use crate::api::permissions as p;
use crate::api::requests as r;
use crate::api::search as s;
use crate::api::trash as t;
use crate::state::DocState;

pub fn doc_router(state: DocState) -> Router {
    Router::new()
        // ── docs ────────────────────────────────────────────────────
        .route("/docs", post(c::create_doc))
        .route("/docs", get(c::list_docs))
        .route("/docs/{doc_id}", get(c::read_doc))
        .route("/docs/{doc_id}", put(c::update_doc))
        .route("/docs/{doc_id}", delete(c::delete_doc))
        .route("/docs/{doc_id}/title", patch(c::rename_doc))
        .route("/docs/{doc_id}/move", post(c::move_doc))
        .route("/docs/{doc_id}/path", get(c::doc_path))
        .route("/docs/{doc_id}/requests", get(r::list_doc_requests))
        // ── dirs ────────────────────────────────────────────────────
        .route("/dirs", post(d::create_dir))
        .route("/dirs/{dir_id}", get(d::read_dir))
        .route("/dirs/{dir_id}", delete(d::delete_dir))
        .route("/dirs/{dir_id}/name", patch(d::rename_dir))
        // ── tree ────────────────────────────────────────────────────
        .route("/tree", get(d::list_tree))
        // ── update requests (design §5) ─────────────────────────────
        .route("/requests", post(r::submit_request))
        .route("/requests", get(r::list_requests))
        .route("/requests/{request_id}", get(r::get_request))
        .route("/requests/{request_id}/approve", post(r::approve_request))
        .route("/requests/{request_id}/reject", post(r::reject_request))
        // ── recycle bin ─────────────────────────────────────────────
        .route("/trash", get(t::list_trash))
        .route("/trash/{trash_id}/restore", post(t::restore_trash))
        .route("/trash/{trash_id}", delete(t::purge_trash))
        // ── search ──────────────────────────────────────────────────
        .route("/search", get(s::search))
        // ── permissions (design §9) ───────────────────────────────────
        .route("/permissions/{kind}/{id}", get(p::get_permissions))
        .route("/permissions/{kind}/{id}", put(p::put_permissions))
        .with_state(state)
}
