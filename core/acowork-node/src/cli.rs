//! acowork-node CLI (ADR-055 §6.13.2).
//!
//! Phase 2a command surface: `start` / `enroll` / `status`.
//! Phase 2c adds the read-only + emergency-write agent tooling:
//! `agents list/logs/kill`. The remaining command face (`rename`,
//! `leave`, `service install`) lands in Phase 3.
//!
//! ADR-101 adds `migrate-memory`: the operator-triggered half of the
//! one-shot hand-over of the legacy per-agent memory store to the memory
//! service (the daemon runs the same pass automatically at startup).
//!
//! Design principle (§6.13.5): the node-local CLI is read-only +
//! node-self-lifecycle tooling — it is NOT a second control plane.
//! install/uninstall/start/stop for agents always go through the
//! Gateway. The two write-shaped exceptions are both operator escape
//! hatches: `agents kill` (emergency stop) and `migrate-memory`
//! (ADR-101 one-shot data hand-over, which only the Node can perform
//! because the store it reads is node-local).

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::config::{resolve_home, NodeConfig};
use crate::control::NodeControlPlane;
use crate::error::NodeError;
use crate::identity::NodeIdentity;
use crate::state::NodeState;

/// ACowork Node Agent — per-machine Runtime host (ADR-055).
#[derive(Parser)]
#[command(name = "acowork-node")]
#[command(about = "ACowork Node Agent — hosts Agent Runtimes on this machine (ADR-055)")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the node daemon (foreground). Auto-enrolls when
    /// identity.json does not exist yet — one command = deployed.
    Start {
        /// Gateway MQTT broker `HOST:PORT` (required).
        #[arg(long, value_name = "HOST:PORT", required = true)]
        gateway: String,
        /// This node's public address `HOST:PORT` (advertise host +
        /// reverse-proxy port). Default: the machine's first non-loopback
        /// IPv4 (fallback `127.0.0.1`) + `19900`. The proxy always binds
        /// `0.0.0.0`; this value is what gets registered on the Gateway.
        /// `auto` (or `auto:PORT`) = live-detect the LAN IP at connect
        /// time and self-heal NodeInfo when the machine changes network
        /// (ADR-055 §6.3.3).
        #[arg(long, value_name = "HOST:PORT|auto")]
        addr: Option<String>,
        /// Reverse-proxy port override (default 19900; used by the
        /// Gateway when spawning a second node instance on the same
        /// machine). Prefer `--addr HOST:PORT` for explicit control.
        #[arg(long, env = "ACOWORK_NODE_PROXY_PORT")]
        proxy_port: Option<u16>,
        /// Node display name (slug, ADR-075 D2). Default: derived from
        /// the hostname at first start; ignored when identity.json
        /// already exists. Display only — never a routing key.
        #[arg(long, env = "ACOWORK_NODE_NAME")]
        name: Option<String>,
        /// Node data / work directory (default: $HOME/.acowork/acowork-node).
        /// `--work-dir` is an alias with identical semantics.
        #[arg(long, visible_alias = "work-dir", env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        /// Agent package install directory (default: {home}/packages).
        /// Set by the Gateway when spawning the local node to keep the
        /// package inventory on the Gateway's own packages_dir.
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
        /// Enrollment token (Phase 5a — accepted, not yet validated).
        #[arg(long, env = "ACOWORK_NODE_TOKEN")]
        token: Option<String>,
        /// Gateway HTTP base URL for the first-boot credential exchange
        /// (ADR-087): `acowork-node start` with no token and no enrolled
        /// identity prompts for the operator's account and mints a
        /// one-time enrollment token bound to that user. Default:
        /// `http://{gateway-host}:{mqtt-port + 1}` (the shipped defaults
        /// 19875/19876); pass this when the Gateway HTTP port differs.
        #[arg(long, value_name = "URL", env = "ACOWORK_GATEWAY_HTTP")]
        gateway_http: Option<String>,
        /// Maximum concurrent Runtime processes (§6.18).
        #[arg(long, env = "ACOWORK_NODE_MAX_AGENTS", default_value = "16")]
        max_agents: u32,
        /// Node-local LSP relay TCP port (ADR-055 §6.7, default 19878).
        #[arg(long, env = "ACOWORK_NODE_LSP_RELAY_PORT", default_value = "19878")]
        lsp_relay_port: u16,
        /// ADR-089 D1 escape hatch: bind the LSP relay to
        /// `advertise_host` instead of `127.0.0.1`. The relay has no
        /// authentication — only set this when you deliberately accept
        /// an open LAN port. The supported access path is the Gateway
        /// reverse proxy (`/api/nodes/{id}/lsp/*`), which needs no
        /// relay listener beyond loopback.
        #[arg(long, env = "ACOWORK_NODE_EXPOSE_LSP_RELAY", default_value = "false")]
        expose_lsp_relay: bool,
        /// Internal spawn marker: set ONLY by the Gateway when it
        /// spawns its own-machine node (hidden from help). Persisted
        /// into identity.json at creation so the Gateway can recognize
        /// its own node even after a service/container restart drops
        /// this flag (ADR-075 D5). No other behavioural effect.
        #[arg(long, hide = true)]
        gateway_managed: bool,
    },
    /// Register the node identity against a Gateway without staying
    /// resident (script / bulk-deployment friendly; idempotent).
    Enroll {
        /// Gateway MQTT broker `HOST:PORT` (required).
        #[arg(long, value_name = "HOST:PORT", required = true)]
        gateway: String,
        #[arg(long, env = "ACOWORK_NODE_NAME")]
        name: Option<String>,
        #[arg(long, visible_alias = "work-dir", env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_TOKEN")]
        token: Option<String>,
    },
    /// Print this node's identity and daemon state (read-only).
    Status {
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        /// Output as JSON (for scripts).
        #[arg(long)]
        json: bool,
    },
    /// Agent tooling (ADR-055 §6.13.2) — read-only + emergency kill.
    Agents {
        #[command(subcommand)]
        cmd: AgentsCommands,
    },
    /// Rename this node's display name (ADR-075 D4). Changes ONLY
    /// `node_name` — `node_id` (the routing key) never changes, so no
    /// retained migration is needed and the daemon may keep running.
    /// Requires the node to be online.
    Rename {
        /// New node display name (slug; `"local"` is reserved).
        new_name: String,
        #[arg(long, visible_alias = "work-dir", env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        /// Gateway MQTT broker `HOST:PORT` (required).
        #[arg(long, value_name = "HOST:PORT", required = true)]
        gateway: String,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
    },
    /// Decommission this node (ADR-055 §6.13.2): drain local Runtimes,
    /// clear retained state, drop from the Gateway's view.
    Leave {
        /// Skip the graceful drain and go offline immediately.
        #[arg(long)]
        force: bool,
        #[arg(long, visible_alias = "work-dir", env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        /// Gateway MQTT broker `HOST:PORT` (required).
        #[arg(long, value_name = "HOST:PORT", required = true)]
        gateway: String,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
    },
    /// OS service integration (systemd user unit / launchd agent).
    Service {
        #[command(subcommand)]
        cmd: ServiceCommands,
    },
    /// Hand the memories that still live in an agent's workspace store to the
    /// memory service (ADR-101 decision 10). Idempotent: an instance already
    /// dealt with is marked by `{workspace}/memory/.migrated` and skipped
    /// unless `--force` is given. The daemon runs this pass at every start;
    /// this command is the operator's retry once a refusal has been fixed.
    MigrateMemory {
        /// Gateway HTTP base URL (the reverse proxy in front of the memory
        /// service). Default: `http://{gateway-host}:{mqtt-port + 1}`.
        #[arg(long, value_name = "URL", env = "ACOWORK_GATEWAY_HTTP")]
        gateway_http: Option<String>,
        /// Gateway MQTT broker `HOST:PORT`, used only to derive the HTTP base
        /// when `--gateway-http` is absent (ADR-055 shipped pairing).
        #[arg(long, value_name = "HOST:PORT")]
        gateway: Option<String>,
        /// Migrate one instance only (ADR-073 instance id).
        #[arg(long, value_name = "INSTANCE_ID")]
        instance: Option<String>,
        /// Re-run past the `.migrated` sentinel.
        #[arg(long)]
        force: bool,
        /// Machine-readable report on stdout (one object per instance).
        #[arg(long)]
        json: bool,
        #[arg(long, visible_alias = "work-dir", env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
    },
}

/// Node-local agent subcommands (ADR-055 §6.13.2).
#[derive(Subcommand)]
pub enum AgentsCommands {
    /// List locally installed agents (version / running state / PID).
    List {
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
    },
    /// Tail a Runtime's log file (troubleshooting tool).
    Logs {
        /// Instance ID whose logs to tail (ADR-073). Find via
        /// `acowork-node agents list` — `INSTANCE` column.
        instance_id: String,
        /// Follow the log (reserved for Phase 3; currently no-op).
        #[arg(short = 'f', long)]
        follow: bool,
        /// Number of trailing lines to print (default 50).
        #[arg(long, default_value = "50")]
        lines: usize,
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
        #[arg(long, env = "ACOWORK_NODE_PACKAGES_DIR")]
        packages_dir: Option<PathBuf>,
    },
    /// Emergency stop (SIGKILL process group). Escape hatch for when
    /// the Gateway is unreachable; state converges via Runtime LWT.
    Kill {
        /// Instance ID to kill (ADR-073).
        instance_id: String,
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
    },
}

/// OS service subcommands (ADR-055 §6.13.2).
#[derive(Subcommand)]
pub enum ServiceCommands {
    /// Install the resident-daemon service unit.
    Install {
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
    },
    /// Remove the resident-daemon service unit.
    Uninstall {
        #[arg(long, env = "ACOWORK_NODE_HOME")]
        home: Option<PathBuf>,
    },
}

/// Parse the mandatory `--gateway HOST:PORT` argument (MQTT broker).
fn split_gateway(s: &str) -> Result<(String, u16), NodeError> {
    let hp =
        acowork_core::addr::parse_host_port(s, acowork_core::defaults::GATEWAY_MQTT_PORT)
            .map_err(|e| NodeError::Config(format!("invalid --gateway '{s}': {e}")))?;
    Ok((hp.host, hp.port))
}

/// ADR-087: exchange the operator's account credentials for a one-time
/// enrollment token bound to that user (headless `start` on a fresh
/// machine).
///
/// The token is a runtime-layer artifact: it never appears in argv,
/// config files, or logs. Returns `None` when the flow must not run —
/// stdin is not a TTY (service / pipe startup keeps the credential-less
/// behavior; the Gateway's default-owner fallback attributes the node) —
/// so the caller can fall through to the normal start path.
async fn interactive_enrollment_token(gateway_http: &str) -> Result<Option<String>, NodeError> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    eprintln!("This machine is not enrolled with the Gateway yet — sign in with your account.");
    eprint!("Username: ");
    let _ = std::io::stderr().flush();
    let mut username = String::new();
    if std::io::stdin().read_line(&mut username).unwrap_or(0) == 0 {
        return Ok(None);
    }
    let username = username.trim_end_matches(['\n', '\r']).to_string();
    let mut password = rpassword::prompt_password("Password: ")?;

    let client = reqwest::Client::new();
    let hint = |e: reqwest::Error| {
        NodeError::Config(format!(
            "Gateway HTTP request to {gateway_http} failed: {e} \
             (wrong HTTP port? pass --gateway-http URL)"
        ))
    };
    let login = client
        .post(format!("{gateway_http}/api/auth/login"))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .map_err(hint)?;
    zeroize_password(&mut password);
    if !login.status().is_success() {
        let code = login.status();
        return Err(NodeError::Config(format!("login failed: HTTP {code}")));
    }
    let pair: serde_json::Value = login.json().await.map_err(hint)?;
    let access = pair
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| NodeError::Config("login response has no access_token".to_string()))?;
    let resp = client
        .post(format!("{gateway_http}/api/nodes/enrollment-tokens"))
        .bearer_auth(access)
        .json(&serde_json::json!({ "ttl_seconds": 3600 }))
        .send()
        .await
        .map_err(hint)?;
    if !resp.status().is_success() {
        let code = resp.status();
        return Err(NodeError::Config(format!(
            "enrollment token request failed: HTTP {code}"
        )));
    }
    let body: serde_json::Value = resp.json().await.map_err(hint)?;
    let token = body
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| NodeError::Config("token response has no `token` field".to_string()))?
        .to_string();
    tracing::info!("enrollment token obtained for the signed-in user");
    Ok(Some(token))
}

/// Best-effort clear of the plaintext password buffer (no `zeroize`
/// crate in this binary; the value is short-lived and never cloned
/// onward).
fn zeroize_password(p: &mut String) {
    p.clear();
}

/// Parse the optional `--addr HOST:PORT` (this node's proxy address).
fn split_addr(s: &str) -> Result<(String, u16), NodeError> {
    let hp = acowork_core::addr::parse_host_port(s, acowork_core::node::NODE_PROXY_PORT)
        .map_err(|e| NodeError::Config(format!("invalid --addr '{s}': {e}")))?;
    Ok((hp.host, hp.port))
}

impl Cli {
    pub fn run(self) -> Result<(), NodeError> {
        match self.command {
            Some(Command::Start {
                gateway,
                addr,
                proxy_port: proxy_port_override,
                name,
                home,
                packages_dir,
                token,
                gateway_http,
                max_agents,
                lsp_relay_port,
                expose_lsp_relay,
                gateway_managed,
            }) => {
                let (gateway_host, gateway_mqtt_port) = split_gateway(&gateway)?;
                // Public address: explicit `--addr`, else the machine's
                // first non-loopback IPv4 (fallback loopback) + default
                // proxy port. The proxy always binds 0.0.0.0. A granular
                // `--proxy-port` (internal second-instance override) wins
                // over the port carried by `--addr`.
                //
                // `--addr auto` (§6.3.3): the host is a placeholder —
                // the LAN IP is re-detected at connect time and published
                // live, so a laptop that switches Wi-Fi hotspots keeps
                // control-plane + Runtime reachability without a restart.
                let (advertise_host, advertise_host_auto, default_proxy_port) = match addr {
                    Some(a) if a == "auto" || a.starts_with("auto:") => {
                        let port = a
                            .split_once(':')
                            .and_then(|(_, p)| p.parse().ok())
                            .unwrap_or(acowork_core::node::NODE_PROXY_PORT);
                        ("127.0.0.1".to_string(), true, port)
                    }
                    Some(a) => {
                        let (host, port) = split_addr(&a)?;
                        (host, false, port)
                    }
                    None => (
                        acowork_core::addr::detect_non_loopback_ipv4()
                            .unwrap_or_else(|| "127.0.0.1".to_string()),
                        false,
                        acowork_core::node::NODE_PROXY_PORT,
                    ),
                };
                let proxy_port = proxy_port_override.unwrap_or(default_proxy_port);
                let mut config = NodeConfig {
                    home: resolve_home(home.as_deref()),
                    packages_dir,
                    gateway_host,
                    gateway_mqtt_port,
                    gateway_http,
                    name,
                    gateway_managed,
                    token,
                    max_agents,
                    advertise_host,
                    advertise_host_auto,
                    proxy_port,
                    lsp_relay_port,
                    expose_lsp_relay,
                    ..NodeConfig::default()
                };
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(async move {
                    // ADR-087: an unenrolled machine started from the CLI
                    // exchanges the operator's account credentials for a
                    // one-time enrollment token bound to that user — the
                    // owner binding happens at the interaction layer, the
                    // token never surfaces in argv / config / logs.
                    if config.token.is_none() {
                        let enrolled = NodeIdentity::load(&config.home)?
                            .is_some_and(|i| i.node_token.is_some());
                        if !enrolled {
                            let http = config.gateway_http_base();
                            if let Some(token) = interactive_enrollment_token(&http).await? {
                                config.token = Some(token);
                            }
                        }
                    }
                    NodeControlPlane::run(config).await
                })
            }
            Some(Command::Enroll {
                gateway,
                name,
                home,
                packages_dir,
                token,
            }) => {
                let (gateway_host, gateway_mqtt_port) = split_gateway(&gateway)?;
                let config = NodeConfig {
                    home: resolve_home(home.as_deref()),
                    packages_dir,
                    gateway_host,
                    gateway_mqtt_port,
                    name,
                    token,
                    ..NodeConfig::default()
                };
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(NodeControlPlane::enroll(config))
            }
            Some(Command::Status { home, json }) => {
                let home = resolve_home(home.as_deref());
                let identity = NodeIdentity::load(&home)?;
                let snapshot = NodeState::load_snapshot(&home);
                if json {
                    let doc = serde_json::json!({
                        "identity": identity,
                        "runtime": snapshot,
                    });
                    println!("{}", serde_json::to_string_pretty(&doc).map_err(|e| {
                        NodeError::Config(format!("serialize status: {e}"))
                    })?);
                } else {
                    print_status(&identity, snapshot.as_ref());
                }
                Ok(())
            }
            Some(Command::Agents { cmd }) => match cmd {
                AgentsCommands::List { home, packages_dir } => {
                    let home = resolve_home(home.as_deref());
                    let packages = packages_dir
                        .unwrap_or_else(|| home.join("packages"));
                    print_agents(&home, &packages);
                    Ok(())
                }
                AgentsCommands::Logs {
                    instance_id,
                    follow,
                    lines,
                    home,
                    packages_dir,
                } => {
                    let home = resolve_home(home.as_deref());
                    let packages = packages_dir
                        .unwrap_or_else(|| home.join("packages"));
                    tail_logs(&packages, &instance_id, lines, follow)
                }
                AgentsCommands::Kill { instance_id, home } => {
                    let home = resolve_home(home.as_deref());
                    kill_agent(&home, &instance_id)
                }
            },
            Some(Command::Rename {
                new_name,
                home,
                gateway,
                packages_dir,
            }) => {
                let (gateway_host, gateway_mqtt_port) = split_gateway(&gateway)?;
                let config = NodeConfig {
                    home: resolve_home(home.as_deref()),
                    packages_dir,
                    gateway_host,
                    gateway_mqtt_port,
                    ..NodeConfig::default()
                };
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(NodeControlPlane::rename(config, &new_name))
            }
            Some(Command::Leave {
                force,
                home,
                gateway,
                packages_dir,
            }) => {
                let (gateway_host, gateway_mqtt_port) = split_gateway(&gateway)?;
                let config = NodeConfig {
                    home: resolve_home(home.as_deref()),
                    packages_dir,
                    gateway_host,
                    gateway_mqtt_port,
                    ..NodeConfig::default()
                };
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(NodeControlPlane::leave(config, force))
            }
            Some(Command::Service { cmd }) => match cmd {
                ServiceCommands::Install { home } => {
                    let config = NodeConfig {
                        home: resolve_home(home.as_deref()),
                        ..NodeConfig::default()
                    };
                    crate::service::install_service(&config)
                }
                ServiceCommands::Uninstall { home } => {
                    let config = NodeConfig {
                        home: resolve_home(home.as_deref()),
                        ..NodeConfig::default()
                    };
                    crate::service::uninstall_service(&config)
                }
            },
            Some(Command::MigrateMemory {
                gateway_http,
                gateway,
                instance,
                force,
                json,
                home,
                packages_dir,
            }) => {
                let mut config = NodeConfig {
                    home: resolve_home(home.as_deref()),
                    packages_dir,
                    gateway_http,
                    ..NodeConfig::default()
                };
                // Only the HTTP base matters here; `--gateway` is accepted so
                // a one-liner copied from `start` derives the same URL.
                if let Some(gateway) = gateway.as_deref() {
                    let (host, port) = split_gateway(gateway)?;
                    config.gateway_host = host;
                    config.gateway_mqtt_port = port;
                }
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(migrate_memory(&config, instance.as_deref(), force, json))
            }
            None => {
                // No subcommand = start (daemon in foreground), per
                // §6.13.2 `acowork-node` with no args.
                let config = NodeConfig::default();
                init_tracing(&config);
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
                rt.block_on(NodeControlPlane::run(config))
            }
        }
    }
}

fn print_status(identity: &Option<NodeIdentity>, snapshot: Option<&crate::state::NodeRuntimeSnapshot>) {
    match identity {
        Some(id) => {
            println!("node_id     : {}", id.node_id);
            println!("node_name   : {}", id.node_name);
            println!("enrollment  : {:?}", id.enrollment);
            println!("gateway     : {}", id.gateway_addr.as_deref().unwrap_or("(never connected)"));
            println!("created_at  : {}", id.created_at.to_rfc3339());
            if let Some(at) = id.enrolled_at {
                println!("enrolled_at : {at}");
            }
            if let Some(snap) = snapshot {
                println!("daemon      : {}", if snap.connected { "connected" } else { "disconnected" });
                if let Some(at) = snap.last_connected_at {
                    println!("last_connect: {at}");
                }
                println!("agents      : {}", snap.agent_count);
            } else {
                println!("daemon      : not running (no state.json)");
            }
        }
        None => {
            println!("No node identity found — run `acowork-node start` to create one.");
        }
    }
}

/// Run one memory-migration pass over the local install table (ADR-101).
///
/// A single round, no backoff: an operator watching this output wants an
/// immediate answer, and every refusal already carries its reason. Exit status
/// is non-zero when at least one instance did not migrate, so a deployment
/// script cannot mistake a partial pass for a finished one.
async fn migrate_memory(
    config: &NodeConfig,
    instance: Option<&str>,
    force: bool,
    json: bool,
) -> Result<(), NodeError> {
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::RwLock;

    let identity = NodeIdentity::load(&config.home)?.ok_or_else(|| {
        NodeError::Identity("No identity.json — run `acowork-node start` first".to_string())
    })?;

    // The install table comes off the packages dir, exactly as the daemon and
    // `agents list` build it (§6.5). A running agent's store is skipped by the
    // pass itself, so this needs no live daemon to consult.
    let mut state = NodeState::new(config.max_agents);
    crate::package::restore_installed_agents(&mut state, &config.packages_dir());
    let mut targets = crate::memory_migration::plan(&state);
    if let Some(instance) = instance {
        targets.retain(|t| t.instance_id == instance);
        if targets.is_empty() {
            return Err(NodeError::AgentNotFound(format!(
                "instance '{instance}' not installed (run `agents list` to see installed instances)"
            )));
        }
    }
    if targets.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No agents installed on this node.");
        }
        return Ok(());
    }

    let gateway_http = config.gateway_http_base();
    if !json {
        println!("Migrating agent memories to {gateway_http} ({} instances)", targets.len());
    }
    let identity = Arc::new(RwLock::new(identity));
    let options = crate::memory_migration::PassOptions {
        rounds: 1,
        pause: Duration::ZERO,
        force,
    };
    let results =
        crate::memory_migration::run_pass(&targets, &gateway_http, &identity, &options).await;

    let mut failed = 0usize;
    let mut rows = Vec::with_capacity(results.len());
    for (instance_id, outcome) in &results {
        let (status, detail) = match outcome {
            Ok(outcome) => ("ok", outcome.to_string()),
            Err(error) => {
                failed += 1;
                ("failed", error.to_string())
            }
        };
        rows.push(serde_json::json!({
            "instance_id": instance_id,
            "status": status,
            "detail": detail,
        }));
        if !json {
            println!("  {:<38} {detail}", instance_id.as_str());
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows)
                .map_err(|e| NodeError::Config(format!("serialize report: {e}")))?
        );
    }
    if failed > 0 {
        return Err(NodeError::MemoryMigration(format!(
            "{failed} of {} instances did not migrate (see the lines above)",
            results.len()
        )));
    }
    Ok(())
}

/// Print locally installed agents with version / running state / PID.
///
/// Installed set comes from the packages dir (the node's own inventory
/// authority, §6.5); running state comes from the persisted
/// `state.json` (PID + loopback HTTP port, written by the daemon on
/// start/stop/shutdown).
///
/// ADR-073: shows the instance id (short prefix) alongside the package
/// id so multiple instances of the same package are distinguishable.
fn print_agents(home: &Path, packages_dir: &Path) {
    // Rebuild the install table from disk (same logic the daemon uses).
    let mut state = NodeState::new(16);
    crate::package::restore_installed_agents(&mut state, packages_dir);

    let snapshot = NodeState::load_snapshot(home);

    if state.installed_agents.is_empty() {
        println!("No agents installed on this node.");
        return;
    }

    println!(
        "{:<16} {:<40} {:<12} {:<8} {:<8} HTTP_PORT",
        "INSTANCE", "AGENT", "VERSION", "STATE", "PID"
    );
    for info in state.installed_agents.values() {
        let running = snapshot
            .as_ref()
            .and_then(|s| s.agents.iter().find(|a| a.instance_id == info.instance_id));
        let (st, pid, port) = match running {
            Some(a) => ("running", a.pid.to_string(), a.http_port.to_string()),
            None => ("stopped", "-".to_string(), "-".to_string()),
        };
        // Short instance id (first 16 hex chars of the UUID is enough
        // to disambiguate in practice; full UUID remains in state.json).
        let instance_short = short_instance_id(&info.instance_id);
        println!(
            "{:<16} {:<40} {:<12} {:<8} {:<8} {}",
            instance_short, info.agent_id, info.version, st, pid, port
        );
    }
}

/// Short display form of an instance id: first 16 hex chars (or the
/// full string if shorter — malformed input passes through verbatim
/// so the operator can spot it).
fn short_instance_id(instance_id: &str) -> &str {
    if instance_id.len() >= 16 {
        &instance_id[..16]
    } else {
        instance_id
    }
}

/// Tail a Runtime's most recent log file.
///
/// Logs live under `{install_path}/workspace/logs/` (the Runtime's
/// `--work-dir`), named `YYYYMMDD_HHMMSS.log` by
/// `SizeRollingFileAppender`.
///
/// ADR-073: `instance_id` resolves to the install path via the
/// install table; the package id is no longer a path component.
///
/// With `follow` this keeps polling the file for appended lines and
/// switches to the newest file on log rotation (ADR-055 §6.13.2).
fn tail_logs(
    packages_dir: &Path,
    instance_id: &str,
    lines: usize,
    follow: bool,
) -> Result<(), NodeError> {
    use std::io::Write as _;

    // Resolve install path from the install table. The daemon rebuilds
    // this on every start; the CLI does the same on demand rather than
    // relying on a stale state.json (which only persists running-agent
    // PIDs, not the install inventory).
    let mut state = NodeState::new(16);
    crate::package::restore_installed_agents(&mut state, packages_dir);
    let info = state
        .installed_agents
        .get(instance_id)
        .ok_or_else(|| {
            NodeError::AgentNotFound(format!(
                "instance '{instance_id}' not installed (run `agents list` to see installed instances)"
            ))
        })?;
    let log_dir = acowork_core::workspace::workspace_dir(&info.install_path).join("logs");

    /// List the `.log` files sorted by name (timestamped, newest last).
    fn latest_log(log_dir: &Path) -> Option<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(log_dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("log"))
            .collect();
        files.sort();
        files.last().cloned()
    }

    /// Print the trailing `lines` lines of a file, returning its byte
    /// length (used as the follow offset).
    fn print_tail(path: &Path, lines: usize) -> Result<usize, NodeError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            NodeError::Config(format!("Cannot read log '{}': {}", path.display(), e))
        })?;
        let len = content.len();
        let all: Vec<&str> = content.lines().collect();
        let start = all.len().saturating_sub(lines);
        for line in &all[start..] {
            println!("{line}");
        }
        Ok(len)
    }

    let Some(mut current) = latest_log(&log_dir) else {
        return Err(NodeError::Config(format!(
            "No log files found in '{}'",
            log_dir.display()
        )));
    };

    let mut last_len = print_tail(&current, lines)?;
    if !follow {
        return Ok(());
    }

    // Follow: poll for appended bytes, switch on rotation.
    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));

        // Rotation: a newer timestamped file appeared — switch to it.
        if let Some(newest) = latest_log(&log_dir)
            && newest != current
        {
            println!("--- rotated to {} ---", newest.display());
            current = newest;
            last_len = 0;
            continue;
        }

        // Appended bytes. `last_len` is a char boundary (the previous
        // `String` length), so slicing is safe.
        let Ok(content) = std::fs::read_to_string(&current) else {
            continue;
        };
        let len = content.len();
        if len > last_len {
            print!("{}", &content[last_len..]);
            let _ = std::io::stdout().flush();
            last_len = len;
        }
    }
}

/// Emergency-kill a Runtime by instance id (from the persisted snapshot).
///
/// ADR-073: keyed by instance id, not package id — two instances of
/// the same package share the package id but MUST differ in
/// instance id.
fn kill_agent(home: &Path, instance_id: &str) -> Result<(), NodeError> {
    let snapshot = NodeState::load_snapshot(home)
        .ok_or_else(|| NodeError::Config("No state.json — is the node daemon running?".to_string()))?;
    let slot = snapshot
        .agents
        .iter()
        .find(|a| a.instance_id == instance_id)
        .ok_or_else(|| NodeError::AgentNotFound(instance_id.to_string()))?;

    // Use the same kill path as the daemon (SIGTERM on Unix via `kill`,
    // taskkill on Windows). SIGKILL semantics are the daemon's reaper
    // concern; here we just terminate the process group.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| NodeError::Config(format!("tokio runtime: {e}")))?;
    rt.block_on(crate::process::spawn::kill_agent_process(slot.pid))?;
    println!(
        "Sent termination signal to instance '{}' ({} PID {})",
        instance_id, slot.agent_id, slot.pid
    );
    Ok(())
}

/// Initialize tracing: stderr + rolling file under `{home}/logs/`.
fn init_tracing(config: &NodeConfig) {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let log_dir = config.home.join("logs");
    let file_layer = std::fs::create_dir_all(&log_dir)
        .ok()
        .and_then(|_| {
            acowork_core::logging::SizeRollingFileAppender::new(log_dir, 10, 10)
                .ok()
                .map(std::sync::Arc::new)
        })
        .map(|appender| {
            tracing_subscriber::fmt::layer()
                .with_writer(appender)
                .with_ansi(false)
                .with_timer(acowork_core::logging::ChronoLocalTimer)
        });

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .with_timer(acowork_core::logging::ChronoLocalTimer)
        .compact();

    let base_level = "info";
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(base_level));
    // `log`-only dependencies (rumqttc) are dropped before tracing sees them
    // until the facade gate is raised — see `sync_log_facade`.
    acowork_core::logging::sync_log_facade(base_level);

    match file_layer {
        Some(file) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file)
                .init();
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .init();
        }
    }
    acowork_core::logging::install_panic_hook();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_parse_start() {
        let cli = Cli::parse_from([
            "acowork-node", "start",
            "--gateway", "192.168.1.10:19875",
            "--name", "gpu-server",
        ]);
        match cli.command {
            Some(Command::Start { gateway, name, addr, .. }) => {
                assert_eq!(gateway, "192.168.1.10:19875");
                assert_eq!(name.as_deref(), Some("gpu-server"));
                assert!(addr.is_none());
            }
            _ => panic!("Expected Start"),
        }
    }

    #[test]
    fn cli_parse_start_work_dir_alias() {
        let cli = Cli::parse_from([
            "acowork-node", "start",
            "--gateway", "127.0.0.1:19875",
            "--work-dir", "/tmp/node-home",
        ]);
        match cli.command {
            Some(Command::Start { home, .. }) => {
                assert_eq!(home.as_deref(), Some(std::path::Path::new("/tmp/node-home")));
            }
            _ => panic!("Expected Start"),
        }
    }

    #[test]
    fn cli_parse_start_defaults_gateway_required() {
        // A missing --gateway is a CLI error (remote boot is explicit).
        let err = Cli::command()
            .try_get_matches_from(["acowork-node", "start"])
            .unwrap_err();
        assert!(err.to_string().contains("--gateway"));
    }

    #[test]
    fn cli_parse_no_subcommand_is_start() {
        let cli = Cli::parse_from(["acowork-node"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_parse_status() {
        let cli = Cli::parse_from(["acowork-node", "status", "--json"]);
        match cli.command {
            Some(Command::Status { json, .. }) => assert!(json),
            _ => panic!("Expected Status"),
        }
    }

    #[test]
    fn cli_parse_enroll_gateway() {
        let cli = Cli::parse_from([
            "acowork-node", "enroll",
            "--gateway", "10.0.0.1:19875",
            "--token", "tok_x",
        ]);
        match cli.command {
            Some(Command::Enroll { gateway, token, .. }) => {
                assert_eq!(gateway, "10.0.0.1:19875");
                assert_eq!(token.as_deref(), Some("tok_x"));
            }
            _ => panic!("Expected Enroll"),
        }
    }

    #[test]
    fn cli_parse_agents_list() {
        let cli = Cli::parse_from(["acowork-node", "agents", "list"]);
        match cli.command {
            Some(Command::Agents { cmd: AgentsCommands::List { .. } }) => {}
            _ => panic!("Expected Agents List"),
        }
    }

    #[test]
    fn cli_parse_agents_logs() {
        let cli = Cli::parse_from([
            "acowork-node",
            "agents",
            "logs",
            "550e8400-e29b-41d4-a716-446655440000",
            "--lines",
            "20",
        ]);
        match cli.command {
            Some(Command::Agents {
                cmd: AgentsCommands::Logs { instance_id, lines, .. },
            }) => {
                assert_eq!(instance_id, "550e8400-e29b-41d4-a716-446655440000");
                assert_eq!(lines, 20);
            }
            _ => panic!("Expected Agents Logs"),
        }
    }

    #[test]
    fn cli_parse_agents_kill() {
        let cli = Cli::parse_from([
            "acowork-node",
            "agents",
            "kill",
            "550e8400-e29b-41d4-a716-446655440000",
        ]);
        match cli.command {
            Some(Command::Agents {
                cmd: AgentsCommands::Kill { instance_id, .. },
            }) => assert_eq!(instance_id, "550e8400-e29b-41d4-a716-446655440000"),
            _ => panic!("Expected Agents Kill"),
        }
    }

    #[test]
    fn cli_parse_rename() {
        let cli = Cli::parse_from([
            "acowork-node", "rename", "gpu-2",
            "--gateway", "127.0.0.1:19875",
        ]);
        match cli.command {
            Some(Command::Rename { new_name, gateway, .. }) => {
                assert_eq!(new_name, "gpu-2");
                assert_eq!(gateway, "127.0.0.1:19875");
            }
            _ => panic!("Expected Rename"),
        }
    }

    #[test]
    fn cli_parse_leave_force() {
        let cli = Cli::parse_from([
            "acowork-node", "leave", "--force",
            "--gateway", "127.0.0.1:19875",
        ]);
        match cli.command {
            Some(Command::Leave { force, gateway, .. }) => {
                assert!(force);
                assert_eq!(gateway, "127.0.0.1:19875");
            }
            _ => panic!("Expected Leave"),
        }
    }

    #[test]
    fn cli_parse_service_install() {
        let cli = Cli::parse_from(["acowork-node", "service", "install"]);
        match cli.command {
            Some(Command::Service {
                cmd: ServiceCommands::Install { .. },
            }) => {}
            _ => panic!("Expected Service Install"),
        }
    }
}

