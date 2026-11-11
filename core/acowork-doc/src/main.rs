//! acowork-doc — standalone document library service process (ADR-064 pattern).
//!
//! Lifecycle is managed by the Gateway supervisor (spawn / monitor /
//! restart). It listens on its own port and serves the full router
//! (REST + MCP + `/health`).
//!
//! The data directory resolves independently to
//! `$HOME/.acowork/acowork-doc/` (peer of `acowork-pm/`, `acowork-gateway/`,
//! `acowork-node/`) — plan decision D-2.
//!
//! Public contract: Desktop goes through `{gw}/api/doc/*`; remote Agents go
//! through `http://{advertise_host}:{gw_http_port}/api/doc/mcp`. Both are
//! reverse-proxied by the Gateway.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use acowork_doc::cli::Cli;
use acowork_doc::config::DocConfig;
use acowork_doc::mqtt_publisher;
use acowork_doc::server::DocService;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Subprocess logging (Gateway redirects stderr to doc.log).
    acowork_core::logging::init_subprocess_logging(&cli.log_level);
    acowork_core::logging::install_panic_hook();

    let mut config = DocConfig::default();
    if let Some(dir) = cli.data_dir {
        config.data_dir = dir;
    }
    if let Some(port) = cli.port {
        config.port = port;
    }
    if let Some(ttl) = cli.request_ttl_hours {
        config.request_ttl_hours = ttl;
    }
    // Plan §7 G5: Gateway endpoint for review receipts. The token rides
    // env (supervisor sets `ACOWORK_DOC_GATEWAY_TOKEN`); clap already reads
    // it into `cli.gateway_token` via the `env` attribute.
    if cli.gateway_url.is_some() {
        config.gateway_url = cli.gateway_url.clone();
    }
    if cli.gateway_token.is_some() {
        config.gateway_token = cli.gateway_token.clone();
    }
    if let Err(e) = config.validate() {
        tracing::error!(error = %e, "Invalid acowork-doc config");
        std::process::exit(1);
    }

    if !config.enabled {
        tracing::warn!("acowork-doc disabled via config (enabled=false) — exiting");
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

    // MQTT tree-change publisher (acowork/doc/tree/changed). Broker is
    // the embedded Gateway broker on this host; a not-yet-ready broker
    // is fine (auto-reconnect). Failure never blocks startup — events
    // are best-effort, the Desktop's 30s poll is the fallback.
    mqtt_publisher::init(&cli.mqtt_host, cli.mqtt_port, cli.mqtt_password.clone()).await;

    let service = match DocService::new(config.clone()).await {
        Ok(svc) => Arc::new(svc),
        Err(e) => {
            tracing::error!(error = %e, "Failed to initialize acowork-doc service");
            std::process::exit(1);
        }
    };

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
            tracing::error!(error = %e, "Failed to start acowork-doc server");
            std::process::exit(1);
        }
    };
    tracing::info!(
        addr = %addr,
        data_dir = %config.data_dir.display(),
        "acowork-doc service listening (standalone)"
    );

    // Write the actual bound port to the port file (Gateway supervisor reads it).
    if let Some(path) = &cli.port_file {
        if let Err(e) = std::fs::write(path, addr.port().to_string()) {
            tracing::error!(path = %path.display(), error = %e, "Failed to write port file");
            std::process::exit(1);
        }
        tracing::info!(path = %path.display(), port = addr.port(), "Port file written");
    }

    // Graceful shutdown: wait for signal, then flush the store.
    let shutdown = acowork_core::shutdown::Shutdown::new();
    acowork_core::shutdown::install_signal_handlers(shutdown.clone());
    while !shutdown.is_shutting_down() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::info!("Graceful shutdown initiated");
    if let Err(e) = service.shutdown().await {
        tracing::error!(error = %e, "acowork-doc shutdown error");
    }
    tracing::info!("acowork-doc service shut down");
}

/// ADR-018: periodically probe Gateway `/health`; self-exit when unreachable.
///
/// Same pattern as PM / LSP relay: when the Gateway crashes (panic / SIGKILL)
/// it cannot reap child processes, so each child must detect its parent's
/// death and exit.
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
