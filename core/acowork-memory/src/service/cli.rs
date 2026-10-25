//! CLI argument parsing for the standalone acowork-memory process (ADR-101).
//!
//! Mirrors `acowork-doc` / `acowork-pm` / `acowork-user`: the Gateway
//! supervisor owns only the process lifecycle and therefore passes just the
//! plumbing flags (`--host` / `--port` / `--port-file` / `--gateway-health-url`
//! / `--log-level` / MQTT). Service tuning (distiller thresholds, forgetting,
//! embedding endpoint) lives in the TOML config file — see
//! [`super::config::MemoryServiceConfig`].

use std::path::PathBuf;

use clap::Parser;

/// CLI arguments for the acowork-memory standalone process.
#[derive(Parser)]
#[command(
    name = "acowork-memory",
    about = "ACowork memory service (standalone process, ADR-101)"
)]
pub struct Cli {
    /// HTTP listen address.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// HTTP listen port (default 18084; auto-increments on conflict, max +20).
    #[arg(long)]
    pub port: Option<u16>,

    /// Data directory override (default `$HOME/.acowork/acowork-memory`).
    #[arg(long, env = "ACOWORK_MEMORY_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// TOML config file path (default `{data_dir}/config.toml`).
    #[arg(long, env = "ACOWORK_MEMORY_CONFIG")]
    pub config: Option<PathBuf>,

    /// Write the actual bound port to this file (read by the Gateway supervisor).
    #[arg(long)]
    pub port_file: Option<PathBuf>,

    /// Gateway health URL (ADR-018): self-exit when the Gateway is unreachable.
    #[arg(long)]
    pub gateway_health_url: Option<String>,

    /// Gateway health probe interval (ms, default 10000 = 10s).
    #[arg(long, default_value = "10000")]
    pub gateway_health_interval_ms: u64,

    /// Gateway unreachable self-exit timeout (ms, default 300000 = 5min).
    #[arg(long, default_value = "300000")]
    pub gateway_health_timeout_ms: u64,

    /// MQTT broker host (the embedded Gateway broker; default 127.0.0.1).
    ///
    /// Reserved: the credentials are pulled over HTTP today
    /// ([`super::resources`]) and these flags are the upgrade path if the
    /// refresh cadence ever has to tighten to a subscription.
    #[arg(long, default_value = "127.0.0.1")]
    pub mqtt_host: String,

    /// MQTT broker port (default 19875).
    #[arg(long, default_value_t = 19875)]
    pub mqtt_port: u16,

    /// MQTT broker CONNECT password: the Gateway's internal publisher token,
    /// forwarded by the supervisor only when `mqtt.auth_enabled` is on.
    /// Env `ACOWORK_MQTT_PASSWORD` as fallback.
    ///
    /// Reserved with the two `--mqtt-*` flags above: this process speaks no MQTT
    /// today, so nothing consumes it. The credential that *is* consumed on the
    /// Gateway HTTP call is `--gateway-token`.
    #[arg(long, env = "ACOWORK_MQTT_PASSWORD")]
    pub mqtt_password: Option<String>,

    /// Internal service token for the Gateway HTTP API, presented as
    /// `Authorization: Bearer` on the global-resources pull (ADR-101
    /// decision 7). The supervisor generates it at boot and passes it at
    /// spawn — the same mechanism `acowork-pm` uses — because this process has
    /// no user account and no node enrollment of its own.
    ///
    /// Under the default `local` auth mode the Gateway answers unauthenticated
    /// anyway; under `multi_user` a missing token means a 401, and the distiller
    /// then runs on `config.toml` candidates alone.
    #[arg(long, env = "ACOWORK_MEMORY_GATEWAY_TOKEN")]
    pub gateway_token: Option<String>,

    /// Gateway base URL for the global-resources pull (ADR-101 decision 7),
    /// e.g. `http://127.0.0.1:19876`. Unset → loopback on the default Gateway
    /// HTTP port. Without a reachable Gateway the distiller still runs, on the
    /// `[distiller].models` chain in `config.toml` alone.
    #[arg(long)]
    pub gateway_url: Option<String>,

    /// Log level.
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

