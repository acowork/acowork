//! Gateway CLI
//!
//! Supports daemon mode and CLI subcommands for package management,
//! agent lifecycle control, and listing.

use clap::{Parser, Subcommand};
use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::gateway::Gateway;
use std::io::IsTerminal;
use std::sync::Arc;

/// Global reference to the SizeRollingFileAppender for Gateway log file count
/// dynamic updates. Set by init_tracing() and read by update_log_file_count().
static FILE_APPENDER: std::sync::OnceLock<Arc<acowork_core::logging::SizeRollingFileAppender>> =
    std::sync::OnceLock::new();

/// Print ACowork logo with cyan (宝蓝) ANSI color on startup.
fn print_logo() {
    let color = "\x1b[36m";
    let reset = "\x1b[0m";
    let version = env!("CARGO_PKG_VERSION");
    let logo = format!(
        r#"{color}


 █████╗   ██████╗ ██████╗ ██╗    ██╗ ██████╗ ██████╗ ██╗  ██╗
██╔══██╗ ██╔════╝██╔═══██╗██║    ██║██╔═══██╗██╔══██╗██║ ██╔╝
███████║ ██║     ██║   ██║██║ █╗ ██║██║   ██║██████╔╝█████╔╝ 
██╔══██║ ██║     ██║   ██║██║███╗██║██║   ██║██╔══██╗██╔═██╗ 
██║  ██║ ╚██████╗╚██████╔╝╚███╔███╔╝╚██████╔╝██║  ██║██║  ██╗
╚═╝  ╚═╝  ╚═════╝ ╚═════╝  ╚══╝╚══╝  ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝
═════════════════════════════════════════════════════════════
{reset}{color}                           Gateway{reset}
{color}                           v{version}{reset}
"#,
        color = color,
        reset = reset,
        version = version
    );
    println!("{}", logo);
}

/// Gateway CLI
#[derive(Parser)]
#[command(name = "acowork-gateway")]
#[command(about = "ACowork Gateway - Agent lifecycle manager and gRPC coordinator")]
#[command(version)]
pub struct Cli {
    /// Run as daemon (background service)
    #[arg(long, env = "ACOWORK_GATEWAY_DAEMON")]
    pub daemon: bool,

    /// Vault directory (overrides config)
    #[arg(long, env = "ACOWORK_GATEWAY_VAULT_DIR")]
    pub vault_dir: Option<String>,

    /// Packages directory (overrides config)
    #[arg(long, env = "ACOWORK_GATEWAY_PACKAGES_DIR")]
    pub packages_dir: Option<String>,

    /// Config file path
    #[arg(long, global = true, env = "ACOWORK_GATEWAY_CONFIG")]
    pub config_path: Option<String>,

    /// Gateway home (application root) directory.
    ///
    /// Contains `config/` and `data/` subdirectories. Default:
    /// - Linux/macOS: `$HOME/.acowork/acowork-gateway/`
    /// - Windows:     `%USERPROFILE%\.acowork\acowork-gateway\`
    ///
    /// Overrides the `ACOWORK_HOME` environment variable. Useful for
    /// tests and running multiple isolated instances side-by-side.
    /// `--work-dir` is an alias with identical semantics.
    #[arg(
        long,
        global = true,
        visible_alias = "work-dir",
        env = "ACOWORK_HOME"
    )]
    pub home: Option<String>,

    /// Advertise host: the address other machines should use to reach
    /// services on this Gateway (embed / LSP / broker endpoints
    /// distributed via MQTT). Distinct from the bind address.
    /// If unset, Gateway auto-detects the first non-loopback IP and
    /// warns; defaults to 127.0.0.1 when detection fails.
    /// (ADR-055 D3 §6.3)
    #[arg(long, env = "ACOWORK_GATEWAY_ADVERTISE_HOST")]
    pub advertise_host: Option<String>,

    /// HTTP API listen address as `HOST:PORT` (bind host + port).
    ///
    /// Overrides `[http].host` / `[http].port` and the
    /// `ACOWORK_GATEWAY_HTTP_PORT` env var. Default: `127.0.0.1:19876`.
    /// IPv6 must be bracketed (`[::1]:19876`); a bare host without a
    /// port keeps the default port. Port `0` is rejected — use the TOML
    /// `[http] port = 0` auto-assign if you need an ephemeral port.
    #[arg(long, value_name = "HOST:PORT")]
    pub addr: Option<String>,

    /// MQTT broker listen address as `HOST:PORT` (bind host + port).
    ///
    /// Overrides `[mqtt].host` / `[mqtt].port` and the
    /// `ACOWORK_GATEWAY_MQTT_PORT` env var. Default: `127.0.0.1:19875`.
    /// Same grammar as `--addr`.
    #[arg(long, value_name = "HOST:PORT")]
    pub mqtt_addr: Option<String>,

    /// Log level (trace/debug/info/warn/error)
    #[arg(long, env = "ACOWORK_GATEWAY_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Skip auto-spawning the local Node Agent on daemon startup.
    ///
    /// Default behaviour (ADR-055 §6.11): Gateway spawns a `name=local`
    /// Node Agent as a sibling child process and supervises it for the
    /// lifetime of the daemon. Pass this flag when:
    ///   - the local Node is started manually (CI / containers /
    ///     multi-node verification) and you don't want a duplicate
    ///     `name=local` race
    ///   - the `acowork-node` binary is intentionally absent
    ///   - you run Gateway in pure-orchestrator mode against remote
    ///     nodes only
    ///
    /// Mirror in TOML: `[local_node] enabled = false`.
    #[arg(
        long,
        env = "ACOWORK_GATEWAY_NO_SPAWN_LOCAL_NODE",
        default_value_t = false
    )]
    pub no_spawn_local_node: bool,

    /// Deployment auth mode (ADR-076 §决策 12): `local` or `multi_user`.
    ///
    /// Overrides the bind-address inference (loopback → `local`, anything
    /// else → `multi_user`). Use for edge cases such as a reverse proxy
    /// fronting a `0.0.0.0` bind that is only reachable on a trusted
    /// subnet. TOML counterpart: `auth_mode = "local"`.
    #[arg(
        long,
        global = true,
        value_name = "local|multi_user",
        env = "ACOWORK_GATEWAY_AUTH_MODE"
    )]
    pub auth_mode: Option<String>,

    /// Subcommands
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Install a .agent package
    Install {
        /// Path to .agent package file
        package: String,
        /// Target node (ADR-055 §6.13.3; default: the Gateway's own
        /// machine node, resolved from the broker's retained node
        /// registry — pass `--node <node_id>` to target a remote node).
        /// Wiring lands in Phase 3 —
        /// the command is currently delegated to the local node via the
        /// HTTP API.
        #[arg(long)]
        node: Option<String>,
    },
    /// Uninstall an agent
    Uninstall {
        /// Agent ID to uninstall
        agent_id: String,
        /// Target node (ADR-055 §6.13.3; default: the Gateway's own
        /// machine node, resolved from the broker's retained node
        /// registry — pass `--node <node_id>` to target a remote node).
        #[arg(long)]
        node: Option<String>,
    },
    /// Upgrade an installed agent
    Upgrade {
        /// Agent ID to upgrade
        agent_id: String,
        /// Path to new .agent package file
        package: String,
        /// Target node (ADR-055 §6.13.3; default: the Gateway's own
        /// machine node, resolved from the broker's retained node
        /// registry — pass `--node <node_id>` to target a remote node).
        #[arg(long)]
        node: Option<String>,
    },
    /// Start an agent
    Start {
        /// Agent ID to start
        agent_id: String,
        /// Target node (ADR-055 §6.13.3; default: the Gateway's own
        /// machine node, resolved from the broker's retained node
        /// registry — pass `--node <node_id>` to target a remote node).
        #[arg(long)]
        node: Option<String>,
    },
    /// Stop a running agent
    Stop {
        /// Agent ID to stop
        agent_id: String,
        /// Target node (ADR-055 §6.13.3; default: the Gateway's own
        /// machine node, resolved from the broker's retained node
        /// registry — pass `--node <node_id>` to target a remote node).
        #[arg(long)]
        node: Option<String>,
    },
    /// List installed agents
    List,
    /// Package an installed agent into .agent file (built on the hosting
    /// node; --output/--key-dir are node-local paths, empty output uses
    /// the node's packages dir)
    Package {
        /// Agent ID (package id) or instance UUID to package
        agent_id: String,
        /// Output directory on the hosting node (default: node packages dir)
        #[arg(long, env = "ACOWORK_PACKAGE_OUTPUT")]
        output: Option<String>,
        /// Sign the package with developer key
        #[arg(long)]
        sign: bool,
        /// Signing key directory (default: agents/.signing-keys)
        #[arg(long, env = "ACOWORK_PACKAGE_KEY_DIR")]
        key_dir: Option<String>,
    },
    /// Node Agent management (ADR-055)
    Nodes {
        #[command(subcommand)]
        cmd: NodesCommands,
    },

    /// First-boot admin password setup (ADR-076 §决策 12 v2).
    ///
    /// Sets the password of the passwordless `admin` account. The account
    /// store belongs to the `acowork-user` process (ADR-084), so this is a
    /// delegate: it spawns that binary's own `admin-setup` and exits. Does
    /// NOT start the Gateway. For interactive TTY startup use the daemon's
    /// prompt instead.
    AdminSetup {
        /// Read password from this file (first line, trimmed).
        /// Useful for systemd `EnvironmentFile=` / k8s `Secret` mounts.
        #[arg(long, value_name = "PATH", conflicts_with = "password_stdin")]
        password_file: Option<String>,

        /// Read password from stdin (first line, trimmed).
        /// Use this when piping from another process: `vault kv get ... | acowork-gateway admin-setup --password-stdin`.
        #[arg(long, conflicts_with = "password_file")]
        password_stdin: bool,
    },
}

/// Node Agent subcommands (ADR-055 §6.13.3).
#[derive(Subcommand)]
pub enum NodesCommands {
    /// List known Node Agents (queries the running Gateway's MQTT
    /// broker for retained status/info topics).
    List,
    /// Stop every agent running on a node (migration precursor).
    Drain {
        /// Target node id.
        node_id: String,
    },
    /// Remove a node's records (requires the node to be offline).
    Remove {
        /// Target node id.
        node_id: String,
    },
    /// Generate an enrollment token (Phase 5a placeholder).
    Token {
        #[command(subcommand)]
        cmd: TokenCommands,
    },
}

/// Enrollment-token subcommands (ADR-055 §6.13.3 — Phase 5a).
#[derive(Subcommand)]
pub enum TokenCommands {
    /// Create an enrollment token.
    Create {
        /// Token lifetime (default 30m).
        #[arg(long, default_value = "30m")]
        ttl: String,
    },
}

impl Cli {
    /// Run the CLI
    pub fn run(self) -> Result<(), GatewayError> {
        // Print ACowork logo with green color
        print_logo();

        // Apply --home flag to ACOWORK_HOME so config path resolution
        // picks it up consistently (CLI > env > default).
        if let Some(home) = &self.home
            && !home.is_empty() {
                // SAFETY: single-threaded at this point in startup.
                unsafe { std::env::set_var("ACOWORK_HOME", home); }
            }

        // Load config.
        let config = GatewayConfig::from_cli(&self)?;
        // Initialize tracing with reload support
        let log_reload_handle = init_tracing(&config.log_level, config.log_file_size_mb, config.log_file_count);

        // Install global panic hook AFTER tracing is initialized so panic
        // messages are captured in both stderr and the rolling log file.
        acowork_core::logging::install_panic_hook();

        // Extract data_flow config before config is moved into Gateway::new
        let worker_threads = config.data_flow.worker_threads;
        // ADR-055: capture MQTT broker address before config moves, for
        // the `nodes list` CLI query (it talks to the running daemon's
        // broker directly, no daemon state).
        let mqtt_host = config.mqtt.host.clone();
        let mqtt_port = config.mqtt.port;
        // ADR-055 §6.13.3: capture the pieces the install/upgrade CLI
        // commands need to spool packages + build the registry download
        // URL (all before config moves into Gateway::new).
        let registry_dir = config.package_registry_dir();
        let advertise_host = crate::config::resolve_advertise_host(&config);
        let http_host = config.http.host.clone();
        let http_port = config.http.port;
        let dev_mode = config.dev_mode;
        // ADR-055 Phase 5a: data dir is needed by `nodes token create`
        // (the enrollment token store lives under it) — captured before
        // config moves into Gateway::new.
        let data_dir = config.data_dir.clone();
        // Captured before `config` is moved into `Gateway::new`: the
        // `admin-setup` subcommand delegates to the owning process (ADR-084).
        let user_data_dir = config.user.data_dir.clone();
        // ADR-076 §决策 12 v2: the `admin-setup` subcommand needs the
        // configured password policy + resolved auth mode to validate the
        // new password. Captured here so we can still build the gateway
        // (which consumes `config`).
        let admin_setup_auth_mode = config.effective_auth_mode();
        let gateway = Gateway::new(config)?;
        match self.command {
            Some(Commands::Install { package, node }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let node = resolve_target_node(&rt, node, &mqtt_host, mqtt_port)?;
                let dispatch = crate::gateway::node_manager::CliPackageDispatch {
                    mqtt_host: &mqtt_host,
                    mqtt_port,
                    node_id: &node,
                    registry_dir: &registry_dir,
                    advertise_host: &advertise_host,
                    http_host: &http_host,
                    http_port,
                    dev_mode,
                };
                rt.block_on(crate::gateway::node_manager::install_agent_via_mqtt(
                    &dispatch,
                    std::path::Path::new(&package),
                ))?;
            }
            Some(Commands::Uninstall { agent_id, node }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let node = resolve_target_node(&rt, node, &mqtt_host, mqtt_port)?;
                rt.block_on(crate::gateway::node_manager::uninstall_agent_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node,
                    &agent_id,
                ))?;
            }
            Some(Commands::Upgrade { agent_id, package, node }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let node = resolve_target_node(&rt, node, &mqtt_host, mqtt_port)?;
                let dispatch = crate::gateway::node_manager::CliPackageDispatch {
                    mqtt_host: &mqtt_host,
                    mqtt_port,
                    node_id: &node,
                    registry_dir: &registry_dir,
                    advertise_host: &advertise_host,
                    http_host: &http_host,
                    http_port,
                    dev_mode,
                };
                rt.block_on(crate::gateway::node_manager::upgrade_agent_via_mqtt(
                    &dispatch,
                    &agent_id,
                    std::path::Path::new(&package),
                ))?;
            }
            Some(Commands::Start { agent_id, node }) => {
                // Need async runtime for start/stop
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let node = resolve_target_node(&rt, node, &mqtt_host, mqtt_port)?;
                rt.block_on(crate::gateway::node_manager::start_agent_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node,
                    &agent_id,
                ))?;
            }
            Some(Commands::Stop { agent_id, node }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let node = resolve_target_node(&rt, node, &mqtt_host, mqtt_port)?;
                rt.block_on(crate::gateway::node_manager::stop_agent_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node,
                    &agent_id,
                ))?;
            }
            Some(Commands::List) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                let entries = rt.block_on(gateway.list_agents());
                if entries.is_empty() {
                    println!("No agents installed.");
                } else {
                    for entry in entries {
                        println!("  {}", entry);
                    }
                }
            }
            Some(Commands::Package { agent_id, output, sign, key_dir }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                // ADR-055: the package lives on the hosting node — the
                // build is dispatched over the MQTT control plane.
                // output/key_dir are node-local paths; empty output →
                // the node's own packages dir.
                rt.block_on(crate::gateway::node_manager::package_agent_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &agent_id,
                    output.as_deref().unwrap_or(""),
                    sign,
                    key_dir.as_deref().unwrap_or(""),
                ))?;
            }
            Some(Commands::Nodes { cmd: NodesCommands::List }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                rt.block_on(crate::gateway::node_manager::list_nodes_via_mqtt(
                    &mqtt_host, mqtt_port,
                ))?;
            }
            Some(Commands::Nodes { cmd: NodesCommands::Drain { node_id } }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                rt.block_on(crate::gateway::node_manager::drain_node_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node_id,
                ))?;
            }
            Some(Commands::Nodes { cmd: NodesCommands::Remove { node_id } }) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(GatewayError::Io)?;
                rt.block_on(crate::gateway::node_manager::remove_node_via_mqtt(
                    &mqtt_host,
                    mqtt_port,
                    &node_id,
                ))?;
            }
            Some(Commands::Nodes {
                cmd: NodesCommands::Token { cmd: TokenCommands::Create { ttl } },
            }) => {
                // ADR-055 Phase 5a: real enrollment-token issuance. The
                // plaintext is printed exactly once — only its sha256
                // hash is persisted in `enrollment_tokens.json`.
                let ttl = parse_ttl(&ttl)?;
                let mut store = crate::mqtt::enrollment::EnrollmentTokenStore::load(
                    std::path::Path::new(&data_dir),
                );
                // ADR-087: a CLI-issued token has no interactive owner —
                // the operator running this on the Gateway machine IS the
                // admin, so bind the earliest-created admin as the node
                // owner up front. Local mode (no accounts) keeps None:
                // ownerless = the machine is the owner.
                let owner = if admin_setup_auth_mode.is_multi_user() {
                    user_first_admin(user_data_dir.as_deref())
                } else {
                    None
                };
                let plaintext = store.create_token(ttl, owner);
                println!("Enrollment token created (one-time, TTL {}m):", ttl.as_secs() / 60);
                println!("{plaintext}");
                println!("\nPass it to a node on first boot:");
                println!("  acowork-node start --token <token>");
            }
            Some(Commands::AdminSetup { password_file, password_stdin }) => {
                // ADR-076 §决策 12 v2: first-boot admin password setup.
                // Runs synchronously, no Gateway start. The AccountStore
                // resolution matches `Gateway::new` — load policy from the
                // same TOML, build the same `AuthService`, call
                // `set_admin_password`.
                if !admin_setup_auth_mode.is_multi_user() {
                    eprintln!(
                        "admin-setup requires AUTH_MODE=multi_user (resolved mode: {admin_setup_auth_mode})."
                    );
                    eprintln!(
                        "Either set `auth_mode = \"multi_user\"` in gateway.toml or remove \
                         this invocation — local-mode deployments do not have an admin account."
                    );
                    return Err(GatewayError::Config(format!(
                        "admin-setup requires multi_user mode (got {admin_setup_auth_mode})"
                    )));
                }
                // ADR-084: the account store belongs to `acowork-user`, so
                // this subcommand is a thin alias for that binary's own
                // `admin-setup` — it must not open the store here, or the
                // Gateway would be a second writer.
                let mut password = read_admin_password(password_file.as_deref(), password_stdin)
                    .map_err(GatewayError::Config)?;
                let result = user_admin_setup(user_data_dir.as_deref(), &password);
                // Zeroize the local buffer immediately — the Argon2id hash
                // is the only durable artifact.
                zeroize::Zeroize::zeroize(&mut password);
                result.map_err(GatewayError::Config)?;
                // No restart needed since ADR-084: the Gateway polls the
                // user service `/health` every 2s, so the first-boot gate
                // and `/api/status` clear themselves within that window.
                println!("Admin password set. A running gateway picks it up within a few seconds.");
                return Ok(());
            }
            None => {
                // ADR-076 §决策 12 v3: first-boot admin setup is an
                // *independent* operation, decoupled from whether the
                // caller also asked the daemon to start. Two reasons:
                //
                //  1. UX — `acowork-gateway admin-setup` is the documented
                //     entry point, but if the operator forgets that and
                //     just runs the bare binary on a fresh install we
                //     shouldn't dead-end them at a help banner.
                //  2. Safety — the prompt path must NEVER silently start a
                //     daemon afterwards. The operator should see the
                //     prompt, set the password, then explicitly choose to
                //     run with `--daemon` (or skip daemon entirely).
                //
                // We therefore handle first-boot setup *before* the
                // daemon/help dispatch, regardless of `self.daemon`. If
                // the operator didn't ask for the daemon, we exit after
                // the prompt so they can confirm before launching the
                // service.
                if gateway.is_first_boot_restricted() {
                    // ADR-076 §决策 12 v3: the prompt is *best effort* and
                    // must never abort the boot — a Gateway that refuses to
                    // start cannot serve the restricted-mode surface
                    // (`/api/status` + `requires_setup`), so the Desktop
                    // would see "connection refused" instead of the gate.
                    // `build_macos.sh --start` is exactly that case: it
                    // backgrounds us with stdio redirected.
                    if can_prompt_interactively() {
                        eprintln!(
                            "First-boot setup required: a passwordless 'admin' account has \
                             been seeded.\n\
                             This prompt will only appear once on a fresh install."
                        );
                        match read_admin_password(None, false) {
                            Ok(mut pw) => {
                                gateway
                                    .set_admin_password(&pw)
                                    .map_err(GatewayError::Config)?;
                                zeroize::Zeroize::zeroize(&mut pw);
                                eprintln!("Admin password set.\n");
                                if !self.daemon {
                                    // Operator did NOT ask for the daemon.
                                    // Exit cleanly — they can re-run with
                                    // `--daemon` (or pick a subcommand) on
                                    // the next invocation. We deliberately
                                    // don't auto-promote them into daemon
                                    // mode: that would couple first-boot UX
                                    // to a launch decision they haven't
                                    // made yet.
                                    eprintln!(
                                        "Run again with --daemon to start the gateway, \
                                         or use a subcommand (run --help to list them)."
                                    );
                                    return Ok(());
                                }
                            }
                            Err(e) => {
                                // Mismatch / policy / no TTY after all. Keep
                                // booting (restricted) rather than dying, so
                                // the operator can retry with `admin-setup`
                                // against the running Gateway.
                                eprintln!("First-boot setup incomplete: {e}");
                                warn_first_boot_restricted();
                            }
                        }
                    } else {
                        warn_first_boot_restricted();
                    }
                }
                if self.daemon {
                    tracing::info!("Starting gateway in daemon mode");
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(worker_threads)
                        .enable_all()
                        .build()
                        .map_err(GatewayError::Io)?;
                    rt.block_on(async_main(gateway, log_reload_handle))?;
                } else {
                    // No subcommand and no daemon flag, and no
                    // first-boot setup pending — show help.
                    println!("ACowork Gateway — use subcommands or --daemon to start service");
                    println!("Run with --help for usage information");
                }
            }
        }
        Ok(())
    }
}

/// Whether a hidden-echo prompt can reach a human at all.
///
/// `rpassword` opens `/dev/tty` directly instead of fd 0/1, so the prompt is
/// *not* covered by redirecting stdio — a backgrounded start (build script,
/// systemd, Tauri child) would block on a prompt the operator cannot see.
/// Requiring both stdio ends to be terminals is the conservative proxy for
/// "a human is attached"; anything else falls back to the file / stdin /
/// `admin-setup` paths.
fn can_prompt_interactively() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Tell the operator how to complete first-boot setup — on stderr *and* in
/// the rolling log file, because the motivating path (`build_macos.sh
/// --start`) sends stderr to `/dev/null`. ADR-076 §决策 12 v3.
fn warn_first_boot_restricted() {
    const HOW: &str = "Complete first-boot setup with ONE of:\n  \
                       - run 'acowork-gateway' from an interactive shell (you will be prompted)\n  \
                       - run 'acowork-gateway admin-setup --password-file <path>' \
                       once with a file containing the new password\n  \
                       - run 'acowork-gateway admin-setup --password-stdin < secret.txt'\n  \
                       - set bootstrap_admin in the USER SERVICE config \
                       (gateway.toml [user] config -> the service TOML, or the service's \
                       own --config) and restart the Gateway";
    eprintln!(
        "Gateway is in first-boot restricted mode (admin has no password).\n\n{HOW}\n\n\
         Only /health and /api/status are reachable until setup is complete."
    );
    tracing::warn!(
        "first-boot restricted mode (passwordless admin) — only /health and /api/status are \
         served. {HOW}"
    );
}

/// Read the admin password from one of: `--password-file`, `--password-stdin`,
/// or an interactive hidden-echo read (TTY only).
///
/// ADR-076 §决策 12 v2: the first password never crosses the network. This
/// helper exists to make that an enforced invariant — there is no `--password`
/// CLI flag, only file / stdin / interactive.
fn read_admin_password(
    password_file: Option<&str>,
    password_stdin: bool,
) -> Result<String, String> {
    use std::io::{BufRead, Read};

    if let Some(path) = password_file {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("failed to open password file '{}': {}", path, e))?;
        // Cap at 1 KiB. A password past 1 KiB is operator error.
        let mut buf = String::new();
        file.take(1024)
            .read_to_string(&mut buf)
            .map_err(|e| format!("failed to read password file '{}': {}", path, e))?;
        return Ok(buf.trim_end_matches(['\n', '\r']).to_string());
    }

    if password_stdin {
        let stdin = std::io::stdin();
        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| format!("failed to read password from stdin: {e}"))?;
        return Ok(line.trim_end_matches(['\n', '\r']).to_string());
    }

    if !can_prompt_interactively() {
        return Err(
            "no --password-file / --password-stdin given and there is no interactive \
             terminal (stdin and stdout must both be a TTY).\n\
             Hint: pass --password-file <path>, --password-stdin, or run from an interactive shell."
                .to_string(),
        );
    }

    let pw = rpassword::prompt_password("New admin password: ")
        .map_err(|e| format!("password prompt failed: {e}"))?;
    let confirm = rpassword::prompt_password("Confirm admin password: ")
        .map_err(|e| format!("password confirm prompt failed: {e}"))?;
    if pw != confirm {
        return Err("passwords do not match".into());
    }
    Ok(pw)
}

/// Parse a TTL spec ("30m", "1h", "90s", "2d") into a Duration.
fn parse_ttl(spec: &str) -> Result<std::time::Duration, GatewayError> {
    let spec = spec.trim();
    let (num, unit) = spec.split_at(spec.len().saturating_sub(1));
    let value: u64 = num.parse().map_err(|_| {
        GatewayError::Config(format!(
            "invalid TTL '{spec}' (expected e.g. 30m, 1h, 90s, 2d)"
        ))
    })?;
    let seconds = match unit {
        "s" => value,
        "m" => value * 60,
        "h" => value * 3600,
        "d" => value * 86400,
        _ => {
            return Err(GatewayError::Config(format!(
                "invalid TTL unit '{unit}' (expected s/m/h/d)"
            )))
        }
    };
    Ok(std::time::Duration::from_secs(seconds))
}

/// Cross-platform CRLF conversion for terminal log output.
///
/// On Windows, Rust's `io::Stderr` writes `\n` byte-for-byte, producing
/// unix-style line endings.  This wrapper inserts `\r` before each `\n`.
/// On Unix it is a transparent pass-through.
///
/// Uses the shared [`acowork_core::crlf::CrlfWriter`] implementation.
struct CrlfStderr;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CrlfStderr {
    type Writer = acowork_core::crlf::CrlfWriter<std::io::Stderr>;

    fn make_writer(&self) -> Self::Writer {
        acowork_core::crlf::CrlfWriter {
            inner: std::io::stderr(),
        }
    }
}
///
/// Logs are written to stderr AND to `<root>/data/logs/gateway-<timestamp>.log`
/// (sibling of `embed.log`; both gateway and embed logs live under `data/logs/`).
///
/// Returns the reload handle so the Gateway can dynamically change
/// the log level at runtime via the HTTP config API.
fn init_tracing(level: &str, log_file_size_mb: u64, log_file_count: u64) -> Option<crate::LogReloadHandle> {
    use tracing_subscriber::{reload, layer::SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;
    use acowork_core::logging::ChronoLocalTimer;
    use crate::config::GatewayConfig;

    let env_filter = acowork_core::logging::build_env_filter(level);
    // `log`-only dependencies (yamux) stay silent until the facade gate is
    // raised; the reload layer below only does that on an actual reload.
    acowork_core::logging::sync_log_facade(level);
    let log_dir = GatewayConfig::project_data_dir().join("logs");

    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "WARN: failed to create log directory {:?}: {}; falling back to stderr-only",
            log_dir, e
        );
        return init_stderr_only(env_filter);
    }

    // Size-based rolling file appender: splits when file exceeds log_file_size_mb
    let max_file_count = if log_file_count > 0 { log_file_count as usize } else { 0 };
    // File appender creation may fail (e.g. macOS sandbox EPERM on $HOME paths,
    // full disk, missing perms). Fall back to stderr-only rather than panicking,
    // so a transient filesystem error does not abort gateway startup.
    let file_appender = match acowork_core::logging::SizeRollingFileAppender::new(
        log_dir.clone(),
        if log_file_size_mb > 0 { log_file_size_mb } else { 10 },
        max_file_count,
    ) {
        Ok(appender) => Arc::new(appender),
        Err(e) => {
            eprintln!(
                "WARN: failed to open log file in {:?}: {}; falling back to stderr-only",
                log_dir, e
            );
            return init_stderr_only(env_filter);
        }
    };
    // Store for dynamic log file count updates
    let _ = FILE_APPENDER.set(file_appender.clone());
    let (filter, handle) = reload::Layer::new(env_filter);

    // Stderr layer (for terminal output, compact format, no colors)
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(CrlfStderr)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_ansi(false)
        .with_timer(ChronoLocalTimer)
        .compact();

    // File layer (file_appender implements MakeWriter, writes \n line endings
    // which most modern editors handle; for Notepad, use a proper text editor)
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_ansi(false)
        .with_timer(ChronoLocalTimer);

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Some(handle)
}

/// Initialize a stderr-only tracing subscriber with reload support.
///
/// Used as the fallback when the rolling file appender cannot be opened
/// (sandbox EPERM, missing parent dir, full disk). Keeping the reload
/// handle means the gateway can still push dynamic log-level updates
/// even when the file writer is unavailable.
fn init_stderr_only(env_filter: tracing_subscriber::EnvFilter) -> Option<crate::LogReloadHandle> {
    use tracing_subscriber::{reload, layer::SubscriberExt, util::SubscriberInitExt};
    use acowork_core::logging::ChronoLocalTimer;
    let (filter, handle) = reload::Layer::new(env_filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(CrlfStderr)
                .with_target(false)
                .with_timer(ChronoLocalTimer)
                .compact(),
        )
        .init();
    Some(handle)
}

/// Dynamically update the Gateway's own log file maximum count.
/// Immediately enforces the limit by deleting the oldest files
/// from the Gateway log directory.
pub(crate) fn update_log_file_count(count: u64) {
    if let Some(appender) = FILE_APPENDER.get() {
        let max = if count > 0 { count as usize } else { 0 };
        appender.set_max_file_count(max);
        tracing::info!(log_file_count = count, "Gateway log file count updated dynamically");
    }
}

// Re-export from acowork-core (shared with Agent Runtime)

/// Resolve the target node of a node-scoped CLI command (ADR-075 D5).
///
/// An explicit `--node` wins. Without it the command targets the
/// Gateway's own-machine node — which must be resolved from the running
/// broker's retained node registry, because the node id is a UUID the
/// Node minted (a standalone CLI process has nothing to derive it from).
/// The `"local"` anchor is deliberately NOT used here: no Node subscribes
/// to `acowork/nodes/local/...`, so dispatching to it would silently
/// never arrive.
fn resolve_target_node(
    rt: &tokio::runtime::Runtime,
    node: Option<String>,
    mqtt_host: &str,
    mqtt_port: u16,
) -> Result<String, GatewayError> {
    match node {
        Some(node_id) => Ok(node_id),
        None => rt.block_on(crate::gateway::node_manager::resolve_local_node_via_mqtt(
            mqtt_host, mqtt_port,
        )),
    }
}

/// Async main entry point for daemon mode
async fn async_main(
    mut gateway: Gateway,
    log_reload_handle: Option<crate::LogReloadHandle>,
) -> Result<(), GatewayError> {
    tracing::info!("Gateway daemon starting");

    // Run the gateway event loop
    gateway.run(log_reload_handle).await?;

    Ok(())
}

// ── acowork-user delegation (ADR-084) ───────────────────────────────────────
//
// The Gateway no longer opens the account store. These two helpers shell out
// to the owning process instead, so every write to `accounts.json` happens
// under one owner — and, for `--check`, so the boot-time prompt reads the
// same predicate the service publishes as `requires_setup`.

/// Path of the `acowork-user` binary (sibling of this executable).
fn user_binary() -> std::path::PathBuf {
    crate::lifecycle::process::sibling_binary("acowork-user")
}

/// A `acowork-user` invocation with the shared top-level arguments.
///
/// The top-level flags must precede the subcommand (`global = true` is not
/// set on them, so `admin-setup --data-dir X` would be parsed as a
/// subcommand argument and silently ignored).
fn user_command(user_data_dir: Option<&std::path::Path>) -> std::process::Command {
    let mut cmd = std::process::Command::new(user_binary());
    cmd.args(["--auth-mode", "multi_user"]);
    if let Some(dir) = user_data_dir {
        cmd.arg("--data-dir").arg(dir);
    }
    cmd
}

/// Whether the admin account still has no password (first boot).
///
/// `user_data_dir` is the `[user].data_dir` override — `None` (the usual
/// case) lets the service resolve its own default, so this can never
/// disagree with the copy `restricted_mode` gates on.
///
/// `Err` when the service cannot be asked at all — the caller decides whether
/// that is fatal (`admin-setup`) or merely means "do not prompt" (boot).
pub fn user_setup_required(user_data_dir: Option<&std::path::Path>) -> Result<bool, String> {
    let output = user_command(user_data_dir)
        .args(["admin-setup", "--check"])
        .output()
        .map_err(|e| format!("failed to run `acowork-user admin-setup --check`: {e}"))?;
    match output.status.code() {
        // 0 = setup required, 1 = already configured (see the service's CLI).
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "`acowork-user admin-setup --check` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Resolve the default owner for resources with no interactive owner
/// (ADR-087): the earliest-created admin account, via the owning process.
///
/// `None` (no admin yet / service not installed) is not an error — the
/// caller keeps the old ownerless behavior and the claim endpoint
/// (`POST /api/{nodes,agents}/{id}/claim`) remains as the fallback.
pub fn user_first_admin(user_data_dir: Option<&std::path::Path>) -> Option<String> {
    let output = user_command(user_data_dir)
        .arg("first-admin")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// Set the first-boot admin password, via the owning process.
pub fn user_admin_setup(user_data_dir: Option<&std::path::Path>, password: &str) -> Result<(), String> {
    use std::io::Write;

    let mut child = user_command(user_data_dir)
        .args(["admin-setup", "--password-stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run `acowork-user admin-setup`: {e}"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "failed to open stdin for `acowork-user admin-setup`".to_string())?;
        stdin
            .write_all(password.as_bytes())
            .and_then(|()| stdin.write_all(b"
"))
            .map_err(|e| format!("failed to send the password to acowork-user: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for acowork-user: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "acowork-user admin-setup failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_cli_parse_daemon() {
        let cli = Cli::parse_from(["acowork-gateway", "--daemon"]);
        assert!(cli.daemon);
        assert!(!cli.no_spawn_local_node);
    }

    #[test]
    fn test_cli_parse_no_spawn_local_node() {
        let cli = Cli::parse_from(["acowork-gateway", "--daemon", "--no-spawn-local-node"]);
        assert!(cli.daemon);
        assert!(cli.no_spawn_local_node);
    }

    #[test]
    fn test_no_spawn_local_node_default_false() {
        let cli = Cli::parse_from(["acowork-gateway", "--daemon"]);
        assert!(
            !cli.no_spawn_local_node,
            "default must be false (spawn local node by default)"
        );
    }

    #[test]
    fn test_cli_parse_install() {
        let cli = Cli::parse_from(["acowork-gateway", "install", "weather.agent"]);
        match cli.command {
            Some(Commands::Install { package, .. }) => {
                assert_eq!(package, "weather.agent");
            }
            _ => panic!("Expected Install command"),
        }
    }

    #[test]
    fn test_cli_parse_start() {
        let cli = Cli::parse_from(["acowork-gateway", "start", "com.example.weather"]);
        match cli.command {
            Some(Commands::Start { agent_id, .. }) => {
                assert_eq!(agent_id, "com.example.weather");
            }
            _ => panic!("Expected Start command"),
        }
    }

    #[test]
    fn test_cli_parse_stop() {
        let cli = Cli::parse_from(["acowork-gateway", "stop", "com.example.weather"]);
        match cli.command {
            Some(Commands::Stop { agent_id, .. }) => {
                assert_eq!(agent_id, "com.example.weather");
            }
            _ => panic!("Expected Stop command"),
        }
    }

    #[test]
    fn test_cli_parse_list() {
        let cli = Cli::parse_from(["acowork-gateway", "list"]);
        match cli.command {
            Some(Commands::List) => {}
            _ => panic!("Expected List command"),
        }
    }

    #[test]
    fn test_cli_parse_nodes_list() {
        let cli = Cli::parse_from(["acowork-gateway", "nodes", "list"]);
        match cli.command {
            Some(Commands::Nodes {
                cmd: NodesCommands::List,
            }) => {}
            _ => panic!("Expected Nodes List command"),
        }
    }

    #[test]
    fn test_cli_parse_upgrade() {
        let cli = Cli::parse_from([
            "acowork-gateway",
            "upgrade",
            "com.example.weather",
            "weather-v2.agent",
        ]);
        match cli.command {
            Some(Commands::Upgrade { agent_id, package, .. }) => {
                assert_eq!(agent_id, "com.example.weather");
                assert_eq!(package, "weather-v2.agent");
            }
            _ => panic!("Expected Upgrade command"),
        }
    }

    #[test]
    fn test_cli_default_log_level() {
        // Override any env var set in developer's environment so the
        // clap default_value is used.
        unsafe { std::env::set_var("ACOWORK_GATEWAY_LOG_LEVEL", "info") };
        let cli = Cli::parse_from(["acowork-gateway"]);
        assert_eq!(cli.log_level, "info");
    }

    #[test]
    fn test_cli_env_vars() {
        let cli = Cli::parse_from(["acowork-gateway", "--log-level", "debug"]);
        assert_eq!(cli.log_level, "debug");
    }

    #[test]
    fn test_cli_parse_token_create() {
        let cli = Cli::parse_from([
            "acowork-gateway",
            "nodes",
            "token",
            "create",
            "--ttl",
            "2h",
        ]);
        match cli.command {
            Some(Commands::Nodes {
                cmd: NodesCommands::Token { cmd: TokenCommands::Create { ttl } },
            }) => assert_eq!(ttl, "2h"),
            _ => panic!("Expected nodes token create command"),
        }
    }

    #[test]
    fn test_parse_ttl_units() {
        assert_eq!(parse_ttl("30m").unwrap(), std::time::Duration::from_secs(1800));
        assert_eq!(parse_ttl("1h").unwrap(), std::time::Duration::from_secs(3600));
        assert_eq!(parse_ttl("90s").unwrap(), std::time::Duration::from_secs(90));
        assert_eq!(parse_ttl("2d").unwrap(), std::time::Duration::from_secs(172800));
        assert!(parse_ttl("30").is_err(), "missing unit must fail");
        assert!(parse_ttl("30x").is_err(), "unknown unit must fail");
        assert!(parse_ttl("abc").is_err(), "non-numeric must fail");
    }
}
