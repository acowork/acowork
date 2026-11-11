//! CLI argument parsing for the standalone acowork-doc process (ADR-064).

use std::path::PathBuf;

use clap::Parser;

/// CLI arguments for the acowork-doc standalone process.
#[derive(Parser)]
#[command(
    name = "acowork-doc",
    about = "ACowork online document library service (standalone process)"
)]
pub struct Cli {
    /// HTTP listen address.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// HTTP listen port (default 18081; auto-increments on conflict, max +20).
    #[arg(long)]
    pub port: Option<u16>,

    /// Data directory override (default `$HOME/.acowork/acowork-doc`).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

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
    #[arg(long, default_value = "127.0.0.1")]
    pub mqtt_host: String,

    /// MQTT broker port (default 19875).
    #[arg(long, default_value_t = 19875)]
    pub mqtt_port: u16,

    /// MQTT broker CONNECT password: the Gateway's internal publisher
    /// token, forwarded by the supervisor only when `mqtt.auth_enabled`
    /// is on (the broker admits any `doc:service:*` client id with it,
    /// ADR-084 §决策 4b — see `mqtt_publisher::client_id`). Standalone
    /// mode can set it manually; env `ACOWORK_MQTT_PASSWORD` as
    /// fallback.
    #[arg(long, env = "ACOWORK_MQTT_PASSWORD")]
    pub mqtt_password: Option<String>,

    /// Override for update-request TTL (hours) forwarded by the Gateway.
    #[arg(long)]
    pub request_ttl_hours: Option<u32>,

    /// Gateway HTTP base URL (plan §7 G5): forwarded by the supervisor so
    /// the doc process can push review receipts to
    /// `POST /api/intents/doc-review`. `None` (standalone) drops the push —
    /// the agent-side `doc_check_request` poll is the fallback.
    #[arg(long)]
    pub gateway_url: Option<String>,

    /// Internal-service Bearer token for Gateway HTTP calls. Env-only by
    /// design (`ACOWORK_DOC_GATEWAY_TOKEN`, same channel as PM's
    /// `ACOWORK_PM_GATEWAY_TOKEN`); the CLI flag exists for manual runs.
    #[arg(long, env = "ACOWORK_DOC_GATEWAY_TOKEN")]
    pub gateway_token: Option<String>,

    /// Log level.
    #[arg(long, default_value = "info")]
    pub log_level: String,
}
