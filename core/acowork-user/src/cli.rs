//! acowork-user standalone-process CLI (ADR-084 §决策 5).
//!
//! Mirrors the PM/doc supervisor contract: `--host/--port/--port-file` for
//! binding and port reporting, `--data-dir` for storage,
//! `--gateway-health-url` for the ADR-018 watchdog, and `--config` for the
//! TOML overlay.
//!
//! ADR-114: there is no `--auth-mode` any more — the account system is
//! always on, so the service has exactly one behaviour.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "acowork-user",
    about = "Account / user-domain service (accounts, profiles, avatars, user chat)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Bind host. Must be loopback: the service trusts Gateway-injected
    /// `X-Auth-*` headers (ADR-084 §决策 7).
    #[arg(long, default_value = "127.0.0.1", global = true)]
    pub host: String,

    /// HTTP listen port (default 18083; auto-increments on conflict by 20).
    #[arg(long, global = true)]
    pub port: Option<u16>,

    /// Write the actually-bound port here (read by the Gateway supervisor).
    #[arg(long, global = true)]
    pub port_file: Option<PathBuf>,

    /// Data directory. Default `$HOME/.acowork/acowork-user/`.
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    /// TOML config file to layer over the defaults.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Gateway health URL (ADR-018): self-exit when the Gateway is gone.
    #[arg(long, global = true)]
    pub gateway_health_url: Option<String>,

    /// Gateway health probe interval (ms, default 10000 = 10s).
    #[arg(long, default_value = "10000", global = true)]
    pub gateway_health_interval_ms: u64,

    /// Gateway unreachable self-exit timeout (ms, default 300000 = 5min).
    #[arg(long, default_value = "300000", global = true)]
    pub gateway_health_timeout_ms: u64,

    /// MQTT broker host (the embedded Gateway broker; default 127.0.0.1).
    #[arg(long, default_value = "127.0.0.1", global = true)]
    pub mqtt_host: String,

    /// MQTT broker port (default 19875).
    #[arg(long, default_value_t = 19875, global = true)]
    pub mqtt_port: u16,

    /// MQTT broker CONNECT password: the Gateway's internal publisher
    /// token, forwarded by the supervisor only when `mqtt.auth_enabled`
    /// is on (the broker admits any `user:service:*` client id with
    /// it, ADR-084 §决策 4b — see `mqtt_publisher::client_id`).
    /// Standalone mode can set it manually; env `ACOWORK_MQTT_PASSWORD`
    /// as fallback.
    #[arg(long, env = "ACOWORK_MQTT_PASSWORD", global = true)]
    pub mqtt_password: Option<String>,

    /// Log level.
    #[arg(long, default_value = "info", global = true)]
    pub log_level: String,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Seed the first administrator, then exit.
    AdminSetup {
        /// Read the password from this file.
        #[arg(long)]
        password_file: Option<PathBuf>,

        /// Read the password from stdin (for piping from a secret store).
        #[arg(long)]
        password_stdin: bool,

        /// Only report whether setup is still required; set no password.
        ///
        /// Exit `0` = a passwordless admin exists (first boot), `1` =
        /// already configured. Used by the Gateway's CLI to decide whether
        /// to offer the first-boot prompt, replacing the account-store read
        /// it used to do itself (ADR-084).
        #[arg(long)]
        check: bool,
    },
    /// Print the earliest-created active admin's `user_id`, then exit.
    ///
    /// ADR-087: the Gateway shells out to this to resolve the default owner
    /// for resources with no interactive owner (server-started local node,
    /// CLI-issued enrollment tokens) — reading `accounts.json` directly
    /// would make the Gateway a second party with account-store knowledge
    /// (ADR-084). Read-only: never seeds, never creates the auth dir.
    /// Exit `0` = found (user_id on stdout), `1` = no admin account exists.
    FirstAdmin,
}
