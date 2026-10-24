//! acowork-memory HTTP server entry (ADR-100).
//!
//! The memory service runs as a **standalone process** supervised by the
//! Gateway (the doc / PM / user pattern). This module provides
//! [`MemoryService::serve`] to bind a port and serve the full router
//! (REST + MCP + `/health`).
//!
//! Internal path convention (same as doc): the service itself carries **no**
//! `/api` prefix. The panel-facing routes keep the Runtime paths they replace
//! (`/memory/nodes`, `/memory/stats`, `/memory/distill`, …) so the Gateway
//! reverse proxy can stay a thin forwarder, and `/mcp` is the MCP endpoint.
//! The instance namespace (`?instance_id=` / `X-MCP-Actor`) is resolved per
//! request — see [`crate::service::state`].

use std::net::SocketAddr;
use std::sync::Arc;

use crate::service::config::MemoryServiceConfig;
use crate::service::error::Result;
use crate::service::resources::ResourceClient;
use crate::service::state::MemoryState;

/// memory service running instance.
///
/// The standalone entry (`main.rs`) holds this handle to:
/// - call [`MemoryService::serve`] to bring up the HTTP server (full router)
/// - call [`MemoryService::shutdown`] for graceful stop (flush stores, stop
///   the distiller / forgetting tasks)
pub struct MemoryService {
    pub config: MemoryServiceConfig,
    pub state: Arc<MemoryState>,
}

impl MemoryService {
    /// Construct the service instance (no server started yet).
    ///
    /// Materialises the instance namespace root; store handles open lazily
    /// per instance (see [`crate::service::state`]).
    pub async fn new(config: MemoryServiceConfig) -> Result<Self> {
        config
            .validate()
            .map_err(crate::service::error::ServiceError::BadRequest)?;
        Ok(Self {
            state: Arc::new(MemoryState::new(config.clone())?),
            config,
        })
    }

    /// Build the axum router (REST + MCP merged, **without** `/health`).
    ///
    /// P1a brought up the state; P1c merged the MCP router (`POST /mcp`); P1d
    /// the panel REST router (`/memory/*`). P1e added the consolidation triggers
    /// (`POST /memory/distill`, `PUT /memory/consolidation/status`) and the
    /// bootstrap import (`POST /admin/bootstrap`) to those two routers, and P5
    /// the one-shot legacy store upload (`POST /admin/import`) that the Node
    /// migration pass calls — see [`crate::service::api::import_legacy`].
    pub fn router(&self) -> axum::Router {
        let state = (*self.state).clone();
        axum::Router::new()
            .merge(crate::service::api::rest_router(state.clone()))
            .merge(crate::service::mcp::mcp_router(state))
    }

    /// Hand the pipeline its Gateway client (ADR-100 decision 7).
    ///
    /// Plumbing, not tuning: the Gateway base URL and the node token come from
    /// the CLI the supervisor invoked us with, so they never enter `config.toml`.
    /// Must be called before [`MemoryService::serve`]; without it the distiller
    /// runs on `config.toml` candidates alone and logs that it has none.
    pub fn bind_gateway(&self, client: ResourceClient) {
        self.state.pipeline().set_resource_client(client);
    }

    /// Serve the full router on `bind` (REST + MCP + `/health`).
    ///
    /// Port conflict auto-increments (default 18084 up, max +20); returns the
    /// **actual** bound address (reported via `--port-file` to the Gateway
    /// supervisor). The server runs in a background task; callers must invoke
    /// [`MemoryService::shutdown`] before exit.
    pub async fn serve(self: Arc<Self>, bind: SocketAddr) -> Result<SocketAddr> {
        let host = bind.ip();
        let mut port = bind.port();
        let max_port = port.saturating_add(20);

        loop {
            match tokio::net::TcpListener::bind(SocketAddr::new(host, port)).await {
                Ok(listener) => {
                    let addr = listener.local_addr()?;
                    let router = self.router().merge(crate::service::health::health_route(
                        self.config.data_dir.clone(),
                    ));
                    tracing::info!(addr = %addr, "acowork-memory server listening (full router)");
                    tokio::spawn(async move {
                        if let Err(e) = axum::serve(listener, router).await {
                            tracing::error!(
                                error = %e,
                                "acowork-memory server exited with error"
                            );
                        }
                    });
                    // Only once we are actually serving: the pipelines are
                    // background work and a failed bind must not have started
                    // them.
                    self.state.pipeline().start();
                    return Ok(addr);
                }
                Err(_) if port < max_port => {
                    tracing::warn!(port, "acowork-memory port occupied — trying next");
                    port += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Graceful shutdown (stop background pipelines, close instance stores).
    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!("acowork-memory shutting down");
        self.state.pipeline().shutdown();
        Ok(())
    }
}
