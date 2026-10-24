//! acowork-memory — standalone memory service process (ADR-100).
//!
//! Lifecycle is managed by the Gateway supervisor (spawn / monitor / restart).
//! The process listens on its own port (default 18084) and serves the full
//! router (panel REST + MCP + `/health`).
//!
//! Data lives independently of the Gateway and of every Agent package:
//! `$HOME/.acowork/acowork-memory/instances/{instance_id}/memory.db` — one
//! SQLite per Agent instance, replacing the former
//! `{workspace}/memory/private.grafeo`. Service tuning comes from
//! `{data_dir}/config.toml` (the distiller / forgetting pipelines are
//! scheduled here, not by the Runtime).
//!
//! Public contract: the Desktop panel goes through `{gw}/api/agents/{id}/memory/*`
//! and `{gw}/api/memory/*`; Agents go through the Gateway-injected MCP server
//! (`http://{advertise_host}:{gw_http_port}/api/memory/mcp`). Both are
//! reverse-proxied by the Gateway — see P2.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use acowork_memory::service::resources::ResourceClient;
use acowork_memory::service::{Cli, MemoryService, MemoryServiceConfig};

/// Timeout for one Gateway global-resources round-trip. The pull loop owns the
/// overall deadline; this only bounds a single hung request.
const GATEWAY_PULL_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Subprocess logging (Gateway redirects stderr to memory.log).
    acowork_core::logging::init_subprocess_logging(&cli.log_level);
    acowork_core::logging::install_panic_hook();

    let config = match MemoryServiceConfig::load(&cli) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(error = %e, "Invalid acowork-memory config");
            std::process::exit(1);
        }
    };
    if let Err(e) = config.validate() {
        tracing::error!(error = %e, "Invalid acowork-memory config");
        std::process::exit(1);
    }

    if !config.enabled {
        tracing::warn!("acowork-memory disabled via config (enabled=false) — exiting");
        std::process::exit(0);
    }

    // ADR-018: Gateway health watchdog (self-exit if the Gateway dies).
    if let Some(ref url) = cli.gateway_health_url {
        spawn_gateway_health_watchdog(
            url.clone(),
            Duration::from_millis(cli.gateway_health_interval_ms),
            Duration::from_millis(cli.gateway_health_timeout_ms),
        );
        tracing::info!(
            url = %url,
            interval_ms = cli.gateway_health_interval_ms,
            timeout_ms = cli.gateway_health_timeout_ms,
            "Gateway health watchdog started (ADR-018)"
        );
    }

    let service = match MemoryService::new(config.clone()).await {
        Ok(svc) => Arc::new(svc),
        Err(e) => {
            tracing::error!(error = %e, "Failed to initialize acowork-memory service");
            std::process::exit(1);
        }
    };

    // Distiller / embedding credentials arrive from the Gateway snapshot, not
    // from config.toml (ADR-100 decision 7). The bearer token is the internal
    // service credential the supervisor generated at boot (same as PM); with
    // `local` auth there is none and the loopback route is open.
    let resource_client = match &cli.gateway_url {
        Some(url) => ResourceClient::for_base(url, cli.gateway_token.clone(), GATEWAY_PULL_TIMEOUT),
        None => ResourceClient::new("127.0.0.1", cli.gateway_token.clone(), GATEWAY_PULL_TIMEOUT),
    };
    tracing::info!(url = %resource_client.url(), "Gateway global-resources pull configured");
    service.bind_gateway(resource_client);

    let bind = format!("{}:{}", cli.host, config.port);
    let bind_addr: std::net::SocketAddr = match bind.parse() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(bind = %bind, error = %e, "Invalid bind address");
            std::process::exit(1);
        }
    };

    let addr = match service.clone().serve(bind_addr).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "Failed to start acowork-memory server");
            std::process::exit(1);
        }
    };
    tracing::info!(
        addr = %addr,
        data_dir = %config.data_dir.display(),
        distiller_enabled = config.distiller.enabled,
        forgetting_enabled = config.forgetting.enabled,
        "acowork-memory service listening (standalone)"
    );

    // Write the actual bound port to the port file (Gateway supervisor reads it).
    if let Some(path) = &cli.port_file {
        if let Err(e) = std::fs::write(path, addr.port().to_string()) {
            tracing::error!(path = %path.display(), error = %e, "Failed to write port file");
            std::process::exit(1);
        }
        tracing::info!(path = %path.display(), port = addr.port(), "Port file written");
    }

    // Graceful shutdown: wait for signal, then stop the pipelines.
    let shutdown = acowork_core::shutdown::Shutdown::new();
    acowork_core::shutdown::install_signal_handlers(shutdown.clone());
    while !shutdown.is_shutting_down() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::info!("Graceful shutdown initiated");
    if let Err(e) = service.shutdown().await {
        tracing::error!(error = %e, "acowork-memory shutdown error");
    }
    tracing::info!("acowork-memory service shut down");
}

/// ADR-018: periodically probe Gateway `/health`; self-exit when unreachable.
///
/// Same pattern as PM / doc / user / LSP relay: when the Gateway crashes
/// (panic / SIGKILL) it cannot reap child processes, so each child must detect
/// its parent's death and exit.
fn spawn_gateway_health_watchdog(health_url: String, interval: Duration, timeout: Duration) {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to build HTTP client for Gateway health probe");

        let mut last_success = std::time::Instant::now();
        loop {
            tokio::time::sleep(interval).await;

            let healthy = match client.get(&health_url).send().await {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            };

            if healthy {
                last_success = std::time::Instant::now();
            } else if last_success.elapsed() >= timeout {
                tracing::error!(
                    elapsed_secs = last_success.elapsed().as_secs(),
                    timeout_secs = timeout.as_secs(),
                    "Gateway unreachable — self-exiting (ADR-018)"
                );
                std::process::exit(0);
            }
        }
    });
}
