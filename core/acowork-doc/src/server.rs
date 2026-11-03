//! acowork-doc HTTP server entry.
//!
//! The doc service runs as a **standalone process** supervised by the
//! Gateway (ADR-064 pattern). This module provides [`DocService::serve`] to
//! bind a port and serve the full router (REST + MCP + `/health`).
//!
//! Internal path convention: doc routes carry **no** `/api` prefix (e.g.
//! `/api/tree`, `/api/docs/...`, `/mcp`). The Gateway reverse proxy strips
//! `/api/doc` before forwarding, so the public surface is `/api/doc/*`.
//!
//! Design ref: `docs/design/zh/20-doc-online-document.md` §4 / §7.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::DocConfig;
use crate::error::Result;
use crate::state::DocState;

/// doc service running instance.
///
/// The standalone entry (`main.rs`) holds this handle to:
/// - call [`DocService::serve`] to bring up the HTTP server (full router)
/// - call [`DocService::shutdown`] for graceful stop (flush pending writes)
pub struct DocService {
    pub config: DocConfig,
    pub state: Arc<DocState>,
}

impl DocService {
    /// Construct the service instance (no server started yet).
    pub async fn new(config: DocConfig) -> Result<Self> {
        config
            .validate()
            .map_err(crate::error::DocError::BadRequest)?;
        Ok(Self {
            state: Arc::new(DocState::new(config.clone()).await?),
            config,
        })
    }

    /// Build the axum router (REST + MCP merged, **without** `/health`).
    ///
    /// D0 skeleton: only the health route. D1 attaches the REST router
    /// (`api::router::doc_router`); D3 merges the MCP router (`POST /mcp`).
    pub fn router(&self) -> axum::Router {
        // `max_doc_size_bytes` bounds the **document**; the wire body is
        // bounded by `limit::transport_ceiling` (escaping headroom), see that
        // module for why they are two numbers.
        //
        // The layer below is only a memory backstop for routes that do not go
        // through `api::body::DocJson` or the MCP endpoint (both of which read
        // the body themselves and return the `payload_too_large` JSON
        // envelope). Those two paths are what a document actually travels
        // through, and they intercept at the same ceiling first.
        let limit = crate::limit::transport_ceiling(self.config.max_doc_size_bytes);
        axum::Router::new()
            .merge(crate::api::router::doc_router((*self.state).clone()))
            .merge(crate::mcp::mcp_router((*self.state).clone()))
            .layer(axum::extract::DefaultBodyLimit::max(limit))
    }

    /// Serve the full router on `bind` (REST + MCP + `/health`).
    ///
    /// Port conflict auto-increments (default 18081 up, max +20); returns the
    /// **actual** bound address (reported via `--port-file` to the Gateway
    /// supervisor). The server runs in a background task; callers must invoke
    /// [`DocService::shutdown`] before exit to flush pending writes.
    pub async fn serve(self: Arc<Self>, bind: SocketAddr) -> Result<SocketAddr> {
        let host = bind.ip();
        let mut port = bind.port();
        let max_port = port.saturating_add(20);

        loop {
            match tokio::net::TcpListener::bind(SocketAddr::new(host, port)).await {
                Ok(listener) => {
                    let addr = listener.local_addr()?;
                    let router = self
                        .router()
                        .merge(crate::health::health_route(self.config.data_dir.clone()));
                    tracing::info!(addr = %addr, "acowork-doc server listening (full router)");
                    tokio::spawn(async move {
                        if let Err(e) = axum::serve(listener, router).await {
                            tracing::error!(error = %e, "acowork-doc server exited with error");
                        }
                    });
                    return Ok(addr);
                }
                Err(_) if port < max_port => {
                    tracing::warn!(port, "acowork-doc port occupied — trying next");
                    port += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Graceful shutdown (flush pending writes, close store).
    ///
    /// D1 attaches the store's flush here.
    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!("acowork-doc shutting down");
        Ok(())
    }
}
