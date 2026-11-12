//! Local Node Agent supervisor (ADR-055 §6.11, Phase 2a).
//!
//! D1: single topology protocol — the Gateway's own machine is just
//! another node, named by its machine hostname slug (unified naming
//! rule: every node defaults to the hostname, whether Gateway-spawned
//! or remotely self-started). The Gateway spawns it as a sibling
//! binary at startup (after the MQTT broker is ready) and supervises
//! it:
//!
//! 1. **Orphan cleanup** — a local node orphaned by a previous
//!    Gateway run (Gateway crashed / was killed) is SIGTERM'd before
//!    spawning a fresh one. Detection mirrors
//!    `cleanup_orphaned_runtimes`: process-list scan for
//!    `acowork-node` whose cmdline carries our spawn markers
//!    (`--spawn-marker --gateway 127.0.0.1:{port}`). ADR-113: the
//!    marker is a LOCAL process-table hint only — it is never written
//!    into identity.json, never published in NodeInfo, and never read
//!    by any protocol path. It exists solely so this launcher can reap
//!    its own children without touching user-started nodes.
//!
//! **Loopback-only spawn rule (local-mode decision):** a
//! Gateway-spawned node is by definition on this very machine, so it
//! always connects to the broker and advertises itself via
//! `127.0.0.1` — never the broker bind host (which may be `0.0.0.0`
//! for remote nodes and is NOT a valid connect target) and never a
//! LAN IP (which would break the link whenever the machine changes
//! networks). See [`LOCAL_GATEWAY_HOST`].
//! 2. **Reuse window** — the Gateway client is already subscribed to
//!    `acowork/nodes/+/status`; wait a short window for a retained
//!    `online` from THIS machine's node (persisted anchor first, then
//!    a same-hostname scan — see `local_node_id`), reusing it instead
//!    of spawning a duplicate. ADR-113: the scan uses only protocol
//!    state (online + reported hostname), so a node started by the
//!    Desktop, a CLI or a remote launcher is reused just the same.
//! 3. **Spawn + reaper** — the supervisor task owns the `Child`,
//!    reaps it on exit, and respawns after a retry delay (re-checking
//!    the registry first: another instance may have taken over).
//! 4. **Graceful shutdown** — on Gateway Ctrl-C the child is
//!    SIGTERM'd via its process group (the supervisor's `wait()`
//!    observes the exit and does NOT respawn). If the Gateway dies
//!    ungracefully, the node's own MQTT reconnect keeps it alive
//!    until the next Gateway run performs step 1.
//!
//! Phase 2a: the local node manages no Runtimes yet — this module
//! only establishes the residency and the supervision lifecycle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::sync::RwLock;

use crate::mqtt::node_control::{NodeControlClient, NodeInstallDispatch, NodePackageSource};
use crate::mqtt::node_registry::{local_node_id, SharedNodeRegistry};

/// URL host a package-dispatch download must target: a node on the
/// Gateway's own machine dials the (normalized) HTTP bind host — a
/// wildcard bind must still be reached via loopback — while other nodes
/// dial the advertise host they can route to (ADR-055 D3).
/// ADR-113: `local_node_id` is resolved from protocol state only
/// (online + hostname); this is a routing optimisation, never a
/// statement about who launched the node.
pub fn dispatch_url_host<'a>(
    local_node_id: Option<&str>,
    node_id: &str,
    http_host: &'a str,
    advertise_host: &'a str,
) -> &'a str {
    if Some(node_id) == local_node_id {
        acowork_core::addr::connect_host(http_host)
    } else {
        advertise_host
    }
}

/// How long to wait for a retained `online` from an already-running
/// local node before spawning our own. The Gateway client re-subscribes
/// to `acowork/nodes/+/status` immediately before this runs, so a
/// retained message arrives within milliseconds — 500 ms is plenty.
/// (The old 3 s window delayed every clean cold start by 3 s.)
const REUSE_WINDOW: Duration = Duration::from_millis(500);

/// Delay before respawning a crashed local node (§6.11 step 3, 60s).
const RESPAWN_DELAY: Duration = Duration::from_secs(60);

/// Loopback address used to reach the Gateway's OWN machine node:
/// both the `--gateway` spawn target and the orphan-cleanup marker.
///
/// A Gateway-spawned node is always same-machine, so `127.0.0.1` is
/// always reachable and immune to LAN/WiFi IP changes (local-mode
/// loopback-only rule). The broker may bind `0.0.0.0` to accept remote
/// nodes, but `0.0.0.0` is not a valid connect target — the spawn
/// target must never be the bind host.
const LOCAL_GATEWAY_HOST: &str = "127.0.0.1";

/// Grace period after SIGTERM before escalating to SIGKILL.
///
/// Only referenced in the `#[cfg(unix)]` branch of `kill_process_group`;
/// Windows uses `taskkill /F` (no grace). Gated to unix so non-unix
/// targets don't even compile the constant.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Shared supervisor state: current child PID + stop flag.
///
/// The supervisor TASK owns the `Child` handle (needed for `wait()`);
/// other paths only see the PID and the stop flag — no handle
/// contention.
pub struct LocalNodeSupervisor {
    pid: RwLock<Option<u32>>,
    stopping: AtomicBool,
}

impl LocalNodeSupervisor {
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self {
            pid: RwLock::new(None),
            stopping: AtomicBool::new(false),
        })
    }

    /// Graceful shutdown: mark stopping + SIGTERM the child's process
    /// group. The supervisor task reaps the exit and stands down.
    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let pid = *self.pid.read().await;
        if let Some(pid) = pid {
            tracing::info!(pid, "Shutting down local node agent");
            kill_process_group(pid).await;
        }
    }
}

/// Locate the `acowork-node` binary — sibling of the current
/// executable (same convention as `acowork-runtime`, L1-1).
fn node_binary() -> std::path::PathBuf {
    let bin_name = if cfg!(windows) {
        "acowork-node.exe"
    } else {
        "acowork-node"
    };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join(bin_name)))
        .unwrap_or_else(|| std::path::PathBuf::from(bin_name))
}

/// SIGTERM a process group, escalating to SIGKILL after
/// [`KILL_GRACE`]. No-op when the group is already gone.
async fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-15", &format!("-{pid}")])
            .output();
        // Escalation: the supervisor task's wait() would time out
        // eventually; a bounded sleep + SIGKILL keeps teardown snappy.
        tokio::time::sleep(KILL_GRACE).await;
        let _ = std::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .output();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}

/// Find processes whose command line contains `pattern`, returning
/// `(pid, full command line)` pairs.
///
/// The classic `pgrep -af` trick silently breaks on macOS: BSD pgrep
/// ignores `-a` and prints bare PIDs, so a `splitn(2)` parse yields an
/// empty cmdline and every orphan-cleanup check matches nothing.
/// `ps -axo pid=,command=` is portable across Linux/macOS and prints
/// the full argv; on Windows neither `ps` nor `pgrep` exists, so this
/// returns empty and callers skip cleanup (same as before).
pub(crate) fn find_procs_by_cmdline(pattern: &str) -> Vec<(u32, String)> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(), // no ps (Windows) — skip cleanup
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, char::is_whitespace);
            let pid: u32 = parts.next()?.trim().parse().ok()?;
            let cmdline = parts.next().unwrap_or("").trim();
            if cmdline.contains(pattern) {
                Some((pid, cmdline.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Kill orphaned local-node processes from a previous Gateway run.
///
/// Marker: cmdline contains `acowork-node`, our internal
/// `--spawn-marker` argv hint, and our `--gateway {host}:{port}`
/// value. The legacy `--gateway-managed` spelling is still matched so
/// an upgrade can reap children spawned by a pre-ADR-113 Gateway.
/// pgrep-less environments (Windows): skipped (returns 0) — same
/// limitation as `cleanup_orphaned_runtimes`.
fn cleanup_orphaned_local_nodes(mqtt_port: u16) -> usize {
    let my_pid = std::process::id();
    let gateway_marker = format!("--gateway {LOCAL_GATEWAY_HOST}:{mqtt_port}");

    let pids: Vec<u32> = find_procs_by_cmdline("acowork-node")
        .into_iter()
        .filter_map(|(pid, cmdline)| {
            if pid == my_pid {
                return None;
            }
            let spawned_by_us = cmdline.contains("--spawn-marker")
                // Pre-ADR-113 binaries used this argv spelling.
                || cmdline.contains("--gateway-managed");
            if spawned_by_us && cmdline.contains(&gateway_marker) {
                Some(pid)
            } else {
                None
            }
        })
        .collect();

    if pids.is_empty() {
        return 0;
    }
    tracing::info!(
        count = pids.len(),
        "Killing orphaned local node agent(s) from a previous Gateway run"
    );
    for pid in &pids {
        let _ = std::process::Command::new("kill")
            .args(["-15", &pid.to_string()])
            .output();
    }
    pids.len()
}

// ── Own-machine node identity anchor (ADR-113) ──────────────────────
//
// This is launcher-local bookkeeping, NOT protocol state. The Gateway
// records the node_id it last spawned/reused in
// `{data_dir}/local_node.json` so the next boot can ask the registry
// "is that exact node still online?" without a hostname scan.
//
// It is deliberately NOT used as an install target: install/ensure
// requests always carry an explicit node_id. A node started by the
// Desktop, a CLI, or a remote machine is indistinguishable here.

/// `{data_dir}/local_node.json` — the persisted own-machine node id.
fn local_node_anchor_path(data_dir: &str) -> std::path::PathBuf {
    std::path::Path::new(data_dir).join("local_node.json")
}

/// Persisted content of `local_node.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedLocalNode {
    node_id: String,
    #[serde(default)]
    recorded_at: Option<String>,
}

/// Load the persisted own-machine node id. `None` when never recorded
/// or unreadable — never fatal, the same-hostname fallback applies.
pub(crate) fn load_persisted_local_node_id(data_dir: &str) -> Option<String> {
    let path = local_node_anchor_path(data_dir);
    let content = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<PersistedLocalNode>(&content) {
        Ok(record) if !record.node_id.is_empty() => Some(record.node_id),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "local_node.json corrupt — ignoring");
            None
        }
    }
}

/// Persist the own-machine node id (best-effort: a failed write only
/// costs the next boot an extra same-hostname scan).
pub(crate) fn persist_local_node_id(data_dir: &str, node_id: &str) {
    let path = local_node_anchor_path(data_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let record = PersistedLocalNode {
        node_id: node_id.to_string(),
        recorded_at: Some(chrono::Utc::now().to_rfc3339()),
    };
    match serde_json::to_string_pretty(&record) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "Could not persist local node anchor"
                );
            }
        }
        Err(e) => tracing::debug!(error = %e, "Could not serialize local node anchor"),
    }
}

/// Read `node_id` out of the Node Agent's own identity file
/// (`{node_home}/identity.json`). Only the id is extracted — the file
/// also carries the node's long-lived credential, which stays on the
/// node side.
fn read_node_identity_id(identity_path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(identity_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    value
        .get("node_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Learn the spawned child's node_id from its identity file (written
/// at startup, before the MQTT connect) and record it as the anchor:
/// in-memory on the registry + persisted to `{data_dir}/local_node.json`.
async fn learn_and_persist_local_node_id(
    node_registry: SharedNodeRegistry,
    data_dir: String,
) {
    // The child writes identity.json — possibly reusing the existing
    // file — within milliseconds of spawn; poll briefly to cover a
    // cold first boot and torn writes.
    let identity_path = acowork_core::node::default_node_home().join("identity.json");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(node_id) = read_node_identity_id(&identity_path) {
            node_registry.write().await.set_local_node_anchor(&node_id);
            persist_local_node_id(&data_dir, &node_id);
            tracing::info!(node_id = %node_id, "Recorded local node identity anchor");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::debug!(
                path = %identity_path.display(),
                "Node identity file not readable — local node anchor not recorded (same-hostname fallback applies)"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Ensure a local node agent is running; supervise it forever.
///
/// Called from `Gateway::run` AFTER the MQTT broker + Gateway MQTT
/// client (with its `acowork/nodes/+/status` subscription) are up, so
/// a retained `online` from an already-running node reaches the
/// registry during the reuse window.
///
/// `local_token` (ADR-075) is the local node's boot credential — a
/// one-time enrollment token when MQTT auth is enabled, forwarded to
/// the child via `--token` and swapped for the long-lived node_token at
/// enroll time (None keeps the credential-less spawn).
///
/// `data_dir` holds the own-machine node anchor (`local_node.json`) —
/// see the section comment above.
pub async fn ensure_local_node(
    mqtt_port: u16,
    packages_dir: &str,
    node_registry: SharedNodeRegistry,
    local_token: Option<String>,
    proxy_port: Option<u16>,
    lsp_relay_port: Option<u16>,
    data_dir: &str,
) -> std::io::Result<Arc<LocalNodeSupervisor>> {
    // Step 0: pin the persisted own-machine node id so the reuse
    // window below matches the exact node this launcher saw last.
    if let Some(anchor) = load_persisted_local_node_id(data_dir) {
        node_registry
            .write()
            .await
            .set_local_node_anchor(&anchor);
    }

    // Step 1: kill orphans from a previous Gateway run (they carry our
    // local `--spawn-marker` argv hint — user-started nodes never match).
    let orphans = cleanup_orphaned_local_nodes(mqtt_port);
    if orphans > 0 {
        // Avoid a spawn race with the dying process. Retained state is
        // not a concern: the in-memory broker died with the old
        // Gateway, so a fresh start sees an empty retained set.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Step 2: reuse window — an `online` node OF THIS MACHINE (a
    // survivor of a previous Gateway run, or externally managed) wins
    // over spawning our own.
    let deadline = tokio::time::Instant::now() + REUSE_WINDOW;
    let mut reused = false;
    while tokio::time::Instant::now() < deadline {
        if let Some(resolved) = local_node_id(&node_registry).await {
            tracing::info!(node_id = %resolved, "Local node agent already online — reusing it");
            // First successful resolution on this machine records the
            // anchor (e.g. right after an upgrade — no
            // `local_node.json` exists yet).
            if load_persisted_local_node_id(data_dir).as_deref() != Some(resolved.as_str()) {
                persist_local_node_id(data_dir, &resolved);
            }
            reused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let supervisor = LocalNodeSupervisor::new_shared();
    if !reused {
        spawn_and_supervise(
            mqtt_port,
            packages_dir,
            node_registry.clone(),
            supervisor.clone(),
            local_token.as_deref(),
            proxy_port,
            lsp_relay_port,
        )
        .await?;
        // Background: learn the child's node_id from its identity file
        // and record it as the anchor (a fast restart before the record
        // lands just falls back to the same-hostname scan).
        tokio::spawn(learn_and_persist_local_node_id(
            node_registry,
            data_dir.to_string(),
        ));
    }
    Ok(supervisor)
}

/// Spawn the local node child and supervise it: reaper + respawn with
/// re-check, forever (until `shutdown()` sets the stop flag).
#[allow(clippy::too_many_arguments)]
async fn spawn_and_supervise(
    mqtt_port: u16,
    packages_dir: &str,
    node_registry: SharedNodeRegistry,
    supervisor: Arc<LocalNodeSupervisor>,
    local_token: Option<&str>,
    proxy_port: Option<u16>,
    lsp_relay_port: Option<u16>,
) -> std::io::Result<()> {
    let bin = node_binary();
    // Note: previously this function silently returned Ok(()) when
    // the binary was missing. That branch was removed when the
    // `--no-spawn-local-node` flag / `[local_node] enabled = false`
    // config option became the proper opt-out — silently disabling
    // spawn on a missing binary obscured build problems and made the
    // topology non-deterministic.
    //
    // If you want a Gateway that does NOT auto-spawn a local node,
    // pass `--no-spawn-local-node` (or set `[local_node] enabled =
    // false` in `gateway.toml`). Otherwise the sibling `acowork-node`
    // binary must be present alongside `acowork-gateway`.
    let mut child = spawn_node_child(
        &bin,
        mqtt_port,
        packages_dir,
        local_token,
        proxy_port,
        lsp_relay_port,
    )?;
    tracing::info!(pid = child.id(), binary = %bin.display(), "Local node agent spawned");

    let supervise_packages_dir = packages_dir.to_string();
    let supervise_token = local_token.map(str::to_string);
    tokio::spawn(async move {
        loop {
            let pid = child.id();
            *supervisor.pid.write().await = pid;
            let exit = child.wait().await;
            *supervisor.pid.write().await = None;

            if supervisor.stopping.load(Ordering::SeqCst) {
                tracing::info!(exit_status = ?exit, "Local node agent stopped (gateway shutdown)");
                return;
            }
            tracing::warn!(exit_status = ?exit, "Local node agent exited — will respawn");

            // Retry delay, then re-check: another instance may have
            // taken over while we were down.
            tokio::time::sleep(RESPAWN_DELAY).await;
            if supervisor.stopping.load(Ordering::SeqCst) {
                return;
            }
            if local_node_id(&node_registry).await.is_some() {
                tracing::info!("Local node agent online again (external) — not respawning");
                return;
            }

            match spawn_node_child(&bin, mqtt_port, &supervise_packages_dir, supervise_token.as_deref(), proxy_port, lsp_relay_port) {
                Ok(new_child) => {
                    child = new_child;
                    tracing::info!(pid = child.id(), "Local node agent respawned");
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to respawn local node agent — will keep retrying"
                    );
                    loop {
                        tokio::time::sleep(RESPAWN_DELAY).await;
                        if supervisor.stopping.load(Ordering::SeqCst) {
                            return;
                        }
                        if local_node_id(&node_registry).await.is_some() {
                            tracing::info!(
                                "Local node agent online (external) — supervisor standing down"
                            );
                            return;
                        }
                        if let Ok(new_child) = spawn_node_child(&bin, mqtt_port, &supervise_packages_dir, supervise_token.as_deref(), proxy_port, lsp_relay_port) {
                            child = new_child;
                            tracing::info!(pid = child.id(), "Local node agent respawned");
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok(())
}

/// Spawn the `acowork-node start` child with the LOCAL process marker
/// used by orphan cleanup (`--spawn-marker --gateway 127.0.0.1:{port}`).
/// ADR-113: the marker is discarded by the node (see acowork-node cli.rs);
/// it never becomes identity or protocol state.
/// No `--name` is passed: the node defaults to its machine hostname
/// slug (unified naming rule). `token` (ADR-055 Phase 5a) is the
/// pre-issued local node credential, forwarded as `--token` when MQTT
/// auth is enabled.
///
/// Loopback-only (local-mode rule): the spawned node connects to the
/// broker via [`LOCAL_GATEWAY_HOST`] and advertises itself with the
/// same loopback address (`--addr`) — never the bind host, never a
/// LAN IP. This makes the Gateway↔local-node link immune to network
/// changes on the host.
fn spawn_node_child(
    bin: &std::path::Path,
    mqtt_port: u16,
    packages_dir: &str,
    token: Option<&str>,
    proxy_port: Option<u16>,
    lsp_relay_port: Option<u16>,
) -> std::io::Result<Child> {
    let mut cmd = Command::new(bin);
    let effective_proxy_port = proxy_port.unwrap_or(acowork_core::node::NODE_PROXY_PORT);
    cmd.args([
        "start",
        "--gateway",
        &format!("{LOCAL_GATEWAY_HOST}:{mqtt_port}"),
        "--addr",
        &format!("{LOCAL_GATEWAY_HOST}:{effective_proxy_port}"),
        "--proxy-port",
        &effective_proxy_port.to_string(),
        "--spawn-marker",
        "--packages-dir",
        packages_dir,
    ]);
    if let Some(token) = token {
        cmd.args(["--token", token]);
    }
    // ADR-055 multi-instance: a second Gateway on the same machine
    // (tests, previews) must not steal the primary node's LSP relay
    // (:19878) port — forward the configured override when present.
    // (The reverse-proxy port is already carried by `--addr` above.)
    if let Some(port) = lsp_relay_port {
        cmd.args(["--lsp-relay-port", &port.to_string()]);
    }
    // The Gateway-spawned node is a daemon, never an interactive CLI:
    // all three std handles must be explicitly null. The previous
    // `stderr(inherit)` + default stdin inherit leaked the *launcher
    // chain's* console handles into a process attached to a different
    // console. Concrete failure (Windows, dev build, fresh machine):
    // the node inherited the dev terminal's console input handle, so
    // `IsTerminal` (a handle-level check) said "TTY" and the fresh-start
    // path entered the interactive ADR-087 enrollment prompt
    // (`interactive_enrollment_token`), whose `read_line` on a console
    // the node is not attached to blocks forever — node alive, zero
    // MQTT traffic, no identity.json, 0-byte file log. Matching the
    // Desktop's `create_local_node` stdio byte-for-byte also keeps the
    // ADR-113 invariant: swapping *who* spawns the node never changes
    // its startup behavior. The node's own rolling file log under
    // `{home}/logs/` is its diagnostics channel, not inherited stderr.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        // Same console-window suppression as the Desktop spawn path
        // (`create_local_node`): the node is a console binary; never
        // flash a terminal window when the launcher chain has no
        // console of its own (release Desktop).
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(unix)]
    {
        // Own process group: Ctrl-C on the Gateway terminal must not
        // SIGINT the node directly (the Gateway shuts it down
        // explicitly); a kill can target the whole group.
        cmd.process_group(0);
    }

    cmd.spawn()
}

/// Connect to the running Gateway's broker as the publisher client and
/// collect the retained node status/info topics into a NodeRegistry.
///
/// The publisher client auto-subscribes `acowork/nodes/+/status` +
/// `acowork/nodes/+/info` via the ConnAck persistent-subscription
/// handler; we wait a short window for the retained messages to drain,
/// then hand back the registry. No daemon state or HTTP endpoint is
/// touched — the retained topics ARE the node view (ADR-055 §6.13.3).
async fn collect_node_registry_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
) -> crate::error::Result<crate::mqtt::node_registry::SharedNodeRegistry> {
    use crate::mqtt::client::{GatewayMqttClient, MqttMessageCallback};

    let node_registry = crate::mqtt::node_registry::new_shared_registry();

    // The client subscribes node topics automatically on ConnAck (they
    // are in PERSISTENT_SUBSCRIPTIONS). The callback feeds the registry.
    let reg_for_cb = node_registry.clone();
    let callback: MqttMessageCallback = Arc::new(move |topic, payload| {
        // Only node topics feed the node registry (the publisher client
        // also receives agent status/ready/http_endpoint messages).
        if !topic.starts_with("acowork/nodes/") {
            return;
        }
        let reg = reg_for_cb.clone();
        tokio::spawn(async move {
            let mut registry = reg.write().await;
            if topic.ends_with("/status") {
                registry.update_status_from_mqtt(&topic, &payload);
            } else if topic.ends_with("/info") {
                registry.update_info_from_mqtt(&topic, &payload);
            }
        });
    });

    let client = GatewayMqttClient::new_publisher_with_callback(mqtt_host, mqtt_port, callback)
        .await
        .map_err(|e| {
            crate::error::GatewayError::Config(format!(
                "Cannot reach the Gateway MQTT broker at {}:{} — is the Gateway daemon running? ({})",
                mqtt_host, mqtt_port, e
            ))
        })?;

    // Let retained status/info messages drain in.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    drop(client);
    Ok(node_registry)
}

/// `acowork-gateway nodes list` — query the running Gateway's broker
/// for retained node status/info and print a table.
pub async fn list_nodes_via_mqtt(mqtt_host: &str, mqtt_port: u16) -> crate::error::Result<()> {
    let node_registry = collect_node_registry_via_mqtt(mqtt_host, mqtt_port).await?;
    let nodes = node_registry.read().await.list_nodes();

    if nodes.is_empty() {
        println!("No nodes discovered.");
        return Ok(());
    }

    println!(
        "{:<16} {:<8} {:<8} {:<10} {:<12} {:<10} {:<10}",
        "NODE ID", "ONLINE", "AGENTS", "OS", "ARCH", "VERSION", "HOSTNAME"
    );
    for node in nodes {
        let info = node.info.as_ref();
        println!(
            "{:<16} {:<8} {:<8} {:<10} {:<12} {:<10} {:<10}",
            node.node_id,
            if node.online { "yes" } else { "no" },
            info.map(|i| i.agent_count.to_string())
                .unwrap_or_else(|| "-".to_string()),
            info.map(|i| i.os.clone()).unwrap_or_else(|| "-".to_string()),
            info.map(|i| i.arch.clone()).unwrap_or_else(|| "-".to_string()),
            info.map(|i| i.node_version.clone())
                .unwrap_or_else(|| "-".to_string()),
            info.map(|i| i.hostname.clone())
                .unwrap_or_else(|| "-".to_string()),
        );
    }
    Ok(())
}

/// Extract the `agent_id` from a node agent topic
/// (`acowork/nodes/{node_id}/agents/{agent_id}/{kind}`) when the topic
/// matches the given node and `kind`.
fn extract_node_agent_id(topic: &str, node_id: &str, kind: &str) -> Option<String> {
    let prefix = format!("acowork/nodes/{node_id}/agents/");
    let suffix = format!("/{kind}");
    let rest = topic.strip_prefix(&prefix)?;
    let agent_id = rest.strip_suffix(&suffix)?;
    if agent_id.is_empty() || agent_id.contains('/') {
        return None;
    }
    Some(agent_id.to_string())
}

/// `nodes drain <node_id>` — stop every agent running on a node
/// (ADR-055 §6.13.3, migration precursor).
///
/// Runs as a standalone CLI process: connects to the running Gateway's
/// broker, discovers the node's installed agents from the retained
/// `installed` topics, then issues `stop` commands through the
/// [`NodeControlClient`]. `stop` is idempotent, so already-stopped
/// agents simply report `ok` (§6.2).
pub async fn drain_node_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
) -> crate::error::Result<()> {
    use std::collections::HashSet;
    use std::sync::OnceLock;

    use acowork_core::mqtt_proto::{data_envelope, DataEnvelope};
    use prost::Message;
    use tokio::sync::Mutex;

    use crate::mqtt::client::{GatewayMqttClient, MqttMessageCallback};
    use crate::mqtt::node_control::NodeControlClient;

    let installed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let control_slot: Arc<OnceLock<Arc<NodeControlClient>>> = Arc::new(OnceLock::new());

    let cb_installed = installed.clone();
    let cb_control = control_slot.clone();
    let cb_node_id = node_id.to_string();
    let callback: MqttMessageCallback = Arc::new(move |topic, payload| {
        let node_id = cb_node_id.clone();
        let installed = cb_installed.clone();
        let control = cb_control.clone();
        let topic = topic.to_string();
        let payload = payload.to_vec();
        tokio::spawn(async move {
            if let Some(agent_id) = extract_node_agent_id(&topic, &node_id, "installed") {
                if !payload.is_empty() {
                    installed.lock().await.insert(agent_id);
                }
            } else if let Some(_agent_id) = extract_node_agent_id(&topic, &node_id, "events") {
                // Correlate NodeEvent replies with in-flight commands.
                if let Some(control) = control.get()
                    && let Ok(envelope) = DataEnvelope::decode(payload.as_slice())
                    && let Some(data_envelope::Payload::NodeEvent(event)) = envelope.payload
                {
                    control.handle_event(event).await;
                }
            }
        });
    });

    let client = GatewayMqttClient::new_publisher_with_callback(mqtt_host, mqtt_port, callback)
        .await
        .map_err(|e| {
            crate::error::GatewayError::Config(format!(
                "Cannot reach the Gateway MQTT broker at {mqtt_host}:{mqtt_port} — is the Gateway daemon running? ({e})"
            ))
        })?;
    let client = Arc::new(client);
    let _ = control_slot.set(Arc::new(NodeControlClient::new(client.clone())));

    // Let retained `installed` messages drain in.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let agents: Vec<String> = installed.lock().await.iter().cloned().collect();
    let control = control_slot
        .get()
        .cloned()
        .expect("control slot set above");

    if agents.is_empty() {
        println!("Node '{node_id}' has no installed agents to drain.");
        return Ok(());
    }

    for agent_id in &agents {
        // ADR-073: the `installed` topic variable is the INSTANCE
        // identity — pass it as such (agent_id falls back to the same
        // string for legacy node inventory entries).
        match control.stop_agent(node_id, agent_id, agent_id, "drain").await {
            Ok(event) => {
                if event.status == "ok" {
                    println!("stopped {agent_id}");
                } else {
                    println!("{agent_id}: {}", event.message);
                }
            }
            Err(e) => println!("{agent_id}: error — {e}"),
        }
    }
    Ok(())
}

/// Split `acowork/nodes/{node_id}/agents/{instance_id}/installed` into
/// (node_id, instance_id). Used by the CLI `package` inventory scan.
fn split_installed_topic(topic: &str) -> Option<(String, String)> {
    let rest = topic.strip_prefix("acowork/nodes/")?;
    let rest = rest.strip_suffix("/installed")?;
    let (node_id, instance_id) = rest.split_once("/agents/")?;
    if node_id.is_empty() || instance_id.is_empty() || instance_id.contains('/') {
        return None;
    }
    Some((node_id.to_string(), instance_id.to_string()))
}

/// `acowork-gateway package <agent-id>` — build a `.agent` package from an
/// installed agent on the node that hosts it (ADR-055 §6.13.3; replaces
/// the pre-node-topology local build).
///
/// Runs as a standalone CLI process: connects to the running Gateway's
/// broker, collects the retained `installed` inventory to resolve the
/// package-id (or instance UUID) to a (node, instance) pair, then issues
/// a blocking `publish_build` round-trip. `output_dir` and `key_dir` are
/// **node-local** paths — empty `output_dir` builds into the node's own
/// packages dir (same default as the HTTP publish API).
pub async fn package_agent_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    agent_id: &str,
    output_dir: &str,
    sign: bool,
    key_dir: &str,
) -> crate::error::Result<()> {
    use acowork_core::mqtt_proto::{data_envelope, DataEnvelope, InstalledAgentInfo};
    use prost::Message;
    use std::sync::OnceLock;
    use tokio::sync::Mutex;

    use crate::mqtt::client::{GatewayMqttClient, MqttMessageCallback};

    let inventory: Arc<Mutex<Vec<(String, InstalledAgentInfo)>>> = Arc::new(Mutex::new(Vec::new()));
    let control_slot: Arc<OnceLock<Arc<NodeControlClient>>> = Arc::new(OnceLock::new());

    let cb_inventory = inventory.clone();
    let cb_control = control_slot.clone();
    let callback: MqttMessageCallback = Arc::new(move |topic, payload| {
        let inventory = cb_inventory.clone();
        let control = cb_control.clone();
        let topic = topic.to_string();
        let payload = payload.to_vec();
        tokio::spawn(async move {
            if let Some((node_id, _)) = split_installed_topic(&topic)
                && !payload.is_empty()
                && let Ok(envelope) = DataEnvelope::decode(payload.as_slice())
                && let Some(data_envelope::Payload::InstalledAgentInfo(info)) = envelope.payload
            {
                inventory.lock().await.push((node_id, info));
            } else if topic.ends_with("/events")
                && let Some(control) = control.get()
                && let Ok(envelope) = DataEnvelope::decode(payload.as_slice())
                && let Some(data_envelope::Payload::NodeEvent(event)) = envelope.payload
            {
                control.handle_event(event).await;
            }
        });
    });

    let client = GatewayMqttClient::new_publisher_with_callback(mqtt_host, mqtt_port, callback)
        .await
        .map_err(|e| {
            crate::error::GatewayError::Config(format!(
                "Cannot reach the Gateway MQTT broker at {mqtt_host}:{mqtt_port} — is the Gateway daemon running? ({e})"
            ))
        })?;
    let client = Arc::new(client);
    let _ = control_slot.set(Arc::new(NodeControlClient::new(client.clone())));

    // Let retained `installed` messages drain in (same window as drain).
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Resolve the CLI argument: instance UUID first (exact, ADR-073),
    // then package-id match. Multiple package-id matches are ambiguous
    // and must be disambiguated by instance UUID.
    let all = inventory.lock().await;
    let matches: Vec<&(String, InstalledAgentInfo)> = all
        .iter()
        .filter(|(_, info)| info.instance_id == agent_id || info.agent_id == agent_id)
        .collect();
    let (node_id, info) = match matches.len() {
        0 => {
            return Err(crate::error::GatewayError::Package(format!(
                "Agent '{agent_id}' is not installed on any online node (retained inventory was empty — is the Node Agent running?)"
            )))
        }
        1 => (matches[0].0.clone(), matches[0].1.clone()),
        n => {
            let list: Vec<String> = matches
                .iter()
                .map(|(node, info)| format!("{} (node {node})", info.instance_id))
                .collect();
            return Err(crate::error::GatewayError::Package(format!(
                "{n} instances of '{agent_id}' are installed — pass the instance UUID instead: {}",
                list.join(", ")
            )));
        }
    };

    let control = control_slot.get().cloned().expect("control slot set above");
    let event = control
        .publish_build(&node_id, &info.instance_id, &info.agent_id, output_dir, sign, key_dir)
        .await
        .map_err(|e| crate::error::GatewayError::Lifecycle(e.to_string()))?;
    NodeControlClient::check_reply(&info.instance_id, &event)
        .map_err(|e| crate::error::GatewayError::Lifecycle(e.to_string()))?;

    let json = event.result_json.as_deref().ok_or_else(|| {
        crate::error::GatewayError::Lifecycle("node reply missing result_json".to_string())
    })?;
    let result: crate::http::publish_api::BuildResponse = serde_json::from_str(json)
        .map_err(|e| crate::error::GatewayError::Lifecycle(format!("bad build result: {e}")))?;
    println!(
        "Packaged '{}' v{} → {} (signed: {}, {} bytes, on node '{node_id}' — path is node-local)",
        info.agent_id, info.version, result.output_path, result.signed, result.file_size
    );
    Ok(())
}

/// `nodes remove <node_id>` — remove a node's records by clearing its
/// retained topics (ADR-055 §6.13.3). The empty retained status is the
/// removal signal: the Gateway dispatcher reacts to it by deregistering
/// the node's bootstrap subsystem and dropping all node/instance
/// records, and the empty ready/lsps/info snapshots ensure a Gateway
/// restart cannot replay a ghost.
pub async fn remove_node_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
) -> crate::error::Result<()> {
    use acowork_core::node::{
        node_info_topic, node_lsps_topic, node_ready_topic, node_status_topic,
    };

    use crate::mqtt::client::{GatewayMqttClient, MqttQoS};

    let client = GatewayMqttClient::new_publisher(mqtt_host, mqtt_port)
        .await
        .map_err(|e| {
            crate::error::GatewayError::Config(format!(
                "Cannot reach the Gateway MQTT broker at {mqtt_host}:{mqtt_port} — is the Gateway daemon running? ({e})"
            ))
        })?;

    // Publishing an empty retained message clears the retained entry
    // (MQTT semantics). Status goes first — it is the signal the
    // Gateway dispatcher turns into the full record cleanup.
    let topics = [
        node_status_topic(node_id),
        node_info_topic(node_id),
        node_ready_topic(node_id),
        node_lsps_topic(node_id),
    ];
    for topic in &topics {
        client
            .publish_raw(topic, Vec::new(), MqttQoS::AtLeastOnce, true)
            .await
            .map_err(|e| crate::error::GatewayError::Ipc(e.to_string()))?;
    }

    println!("Removed node '{node_id}' (cleared retained status/info/ready/lsps).");
    Ok(())
}

/// Connect to the running Gateway's broker as a standalone CLI control
/// client. Returns a [`NodeControlClient`] whose event replies are routed
/// back through the broker callback (same pattern as
/// [`drain_node_via_mqtt`], §6.13.3 CLI control plane).
async fn cli_control_client(
    mqtt_host: &str,
    mqtt_port: u16,
) -> crate::error::Result<Arc<NodeControlClient>> {
    use std::sync::OnceLock;

    use acowork_core::mqtt_proto::{data_envelope, DataEnvelope};
    use prost::Message;

    use crate::mqtt::client::{GatewayMqttClient, MqttMessageCallback};
    use crate::mqtt::node_control::NodeControlClient;

    let control_slot: Arc<OnceLock<Arc<NodeControlClient>>> = Arc::new(OnceLock::new());
    let cb_control = control_slot.clone();
    let callback: MqttMessageCallback = Arc::new(move |topic, payload| {
        let control = cb_control.clone();
        let topic = topic.to_string();
        let payload = payload.to_vec();
        tokio::spawn(async move {
            if !topic.ends_with("/events") {
                return;
            }
            if let Some(control) = control.get()
                && let Ok(envelope) = DataEnvelope::decode(payload.as_slice())
                && let Some(data_envelope::Payload::NodeEvent(event)) = envelope.payload
            {
                control.handle_event(event).await;
            }
        });
    });

    let client = GatewayMqttClient::new_publisher_with_callback(mqtt_host, mqtt_port, callback)
        .await
        .map_err(|e| {
            crate::error::GatewayError::Config(format!(
                "Cannot reach the Gateway MQTT broker at {mqtt_host}:{mqtt_port} — is the Gateway daemon running? ({e})"
            ))
        })?;
    let client = Arc::new(client);
    let control = Arc::new(NodeControlClient::new(client));
    let _ = control_slot.set(control.clone());
    Ok(control)
}

/// CLI package-dispatch context shared by the `install`/`upgrade`
/// commands (ADR-055 §6.13.3): the broker endpoint, the target node, and
/// the pieces needed to spool a package into the registry and build the
/// download URL.
pub struct CliPackageDispatch<'a> {
    pub mqtt_host: &'a str,
    pub mqtt_port: u16,
    pub node_id: &'a str,
    pub registry_dir: &'a std::path::Path,
    pub advertise_host: &'a str,
    pub http_host: &'a str,
    pub http_port: u16,
    pub dev_mode: bool,
}

/// `install --node <node>` — dispatch an install to a target node via the
/// CLI control plane (ADR-055 §6.13.3).
///
/// Spools the package into the Gateway registry, then issues the async
/// install command carrying the registry download URL. Completion is
/// observed by the daemon via the node's retained `installed` inventory,
/// not here (fire-and-forget, ADR-055 §3.2).
pub async fn install_agent_via_mqtt(
    dispatch: &CliPackageDispatch<'_>,
    package_path: &std::path::Path,
) -> crate::error::Result<()> {
    let manifest = crate::http::agents::extract_manifest_from_package(package_path)?;
    let agent_id = manifest.agent_id.clone();

    std::fs::create_dir_all(dispatch.registry_dir).map_err(crate::error::GatewayError::Io)?;
    let registry_path = dispatch.registry_dir.join(format!("{agent_id}.agent"));
    std::fs::copy(package_path, &registry_path).map_err(crate::error::GatewayError::Io)?;

    // The download URL must be reachable from the target node: the
    // loopback-bound local node dials the HTTP bind host, remote nodes
    // the advertise host (ADR-055 D3). CLI dispatch has no registry —
    // the local-node resolution is not available here, so a CLI-named
    // node always gets the advertise host (ADR-075 D5 boundary).
    let url_host = dispatch_url_host(None, dispatch.node_id, dispatch.http_host, dispatch.advertise_host);
    let url = format!(
        "http://{url_host}:{}/api/packages/{agent_id}/download",
        dispatch.http_port
    );

    let control = cli_control_client(dispatch.mqtt_host, dispatch.mqtt_port).await?;
    // ADR-073 决策 5: the Gateway (here: the CLI control plane)
    // generates the instance identity at install time; the node uses it
    // verbatim for the on-disk directory and the Runtime flag.
    let instance_id = uuid::Uuid::new_v4().to_string();
    // ADR-059 §6: the CLI dispatch has no operation store; a fresh
    // operation id still gives the NodeEvent reply a correlation id.
    let operation_id = acowork_core::operation::OperationId::new();
    control
        .install_agent_by_url(NodeInstallDispatch {
            node_id: dispatch.node_id,
            instance_id: &instance_id,
            agent_id: &agent_id,
            source: NodePackageSource::Url(&url),
            dev_mode: dispatch.dev_mode,
            // CLI `install` is an explicit install of one more copy.
            ensure: false,
            operation_id: operation_id.as_str(),
        })
        .await
        .map_err(|e| crate::error::GatewayError::Lifecycle(e.to_string()))?;

    println!("Install dispatched to node '{}': {agent_id} (instance {instance_id})", dispatch.node_id);
    Ok(())
}

/// `upgrade --node <node>` — dispatch an upgrade to a target node via the
/// CLI control plane (ADR-055 §6.13.3).
pub async fn upgrade_agent_via_mqtt(
    dispatch: &CliPackageDispatch<'_>,
    agent_id: &str,
    package_path: &std::path::Path,
) -> crate::error::Result<()> {
    let manifest = crate::http::agents::extract_manifest_from_package(package_path)?;
    if manifest.agent_id != agent_id {
        return Err(crate::error::GatewayError::Package(format!(
            "Package agent_id '{}' does not match upgrade target '{}'",
            manifest.agent_id, agent_id
        )));
    }

    std::fs::create_dir_all(dispatch.registry_dir).map_err(crate::error::GatewayError::Io)?;
    let registry_path = dispatch.registry_dir.join(format!("{agent_id}.agent"));
    std::fs::copy(package_path, &registry_path).map_err(crate::error::GatewayError::Io)?;

    // Same host selection as install: the local node dials the bind
    // host, remote nodes the advertise host (ADR-055 D3). No registry
    // on the CLI path (ADR-075 D5) → advertise host.
    let url_host = dispatch_url_host(None, dispatch.node_id, dispatch.http_host, dispatch.advertise_host);
    let url = format!(
        "http://{url_host}:{}/api/packages/{agent_id}/download",
        dispatch.http_port
    );

    let control = cli_control_client(dispatch.mqtt_host, dispatch.mqtt_port).await?;
    // ADR-073: the CLI passes the same identifier as instance and
    // package identity — the node resolves the instance table by
    // instance_id first, falling back to the package id for legacy
    // installs.
    control
        .upgrade_agent_by_url(dispatch.node_id, agent_id, agent_id, &url, dispatch.dev_mode)
        .await
        .map_err(|e| crate::error::GatewayError::Lifecycle(e.to_string()))?;

    println!("Upgrade dispatched to node '{}': {agent_id}", dispatch.node_id);
    Ok(())
}

/// `uninstall --node <node>` — uninstall an agent on a target node
/// (blocking command round-trip, ADR-055 §6.13.3).
pub async fn uninstall_agent_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
    agent_id: &str,
) -> crate::error::Result<()> {
    let control = cli_control_client(mqtt_host, mqtt_port).await?;
    match control.uninstall_agent(node_id, agent_id, agent_id).await {
        Ok(event) if event.status == "ok" => {
            println!("Uninstalled '{agent_id}' on node '{node_id}'");
            Ok(())
        }
        Ok(event) => Err(crate::error::GatewayError::Lifecycle(event.message)),
        Err(e) => Err(crate::error::GatewayError::Lifecycle(e.to_string())),
    }
}

/// `start --node <node>` — start an agent on a target node (blocking
/// command round-trip, ADR-055 §6.13.3). Non-dev (production) start.
pub async fn start_agent_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
    agent_id: &str,
) -> crate::error::Result<()> {
    let control = cli_control_client(mqtt_host, mqtt_port).await?;
    match control.start_agent(node_id, agent_id, agent_id, false).await {
        Ok(event) if event.status == "ok" => {
            println!("Started '{agent_id}' on node '{node_id}'");
            Ok(())
        }
        Ok(event) => Err(crate::error::GatewayError::Lifecycle(event.message)),
        Err(e) => Err(crate::error::GatewayError::Lifecycle(e.to_string())),
    }
}

/// `stop --node <node>` — stop an agent on a target node (blocking
/// command round-trip, ADR-055 §6.13.3).
pub async fn stop_agent_via_mqtt(
    mqtt_host: &str,
    mqtt_port: u16,
    node_id: &str,
    agent_id: &str,
) -> crate::error::Result<()> {
    let control = cli_control_client(mqtt_host, mqtt_port).await?;
    match control.stop_agent(node_id, agent_id, agent_id, "cli").await {
        Ok(event) if event.status == "ok" => {
            println!("Stopped '{agent_id}' on node '{node_id}'");
            Ok(())
        }
        Ok(event) => Err(crate::error::GatewayError::Lifecycle(event.message)),
        Err(e) => Err(crate::error::GatewayError::Lifecycle(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_installed_topic_parses_node_and_instance() {
        assert_eq!(
            split_installed_topic("acowork/nodes/gpu-1/agents/0f1e2d3c-4b5a-4671-9992-8589a917451c/installed"),
            Some(("gpu-1".to_string(), "0f1e2d3c-4b5a-4671-9992-8589a917451c".to_string()))
        );
        // Not an installed topic / malformed shapes → None.
        assert_eq!(split_installed_topic("acowork/nodes/gpu-1/agents/x/status"), None);
        assert_eq!(split_installed_topic("acowork/nodes//agents/x/installed"), None);
        assert_eq!(split_installed_topic("acowork/nodes/n/agents//installed"), None);
        assert_eq!(split_installed_topic("acowork/nodes/n/agents/a/b/installed"), None);
    }

    #[test]
    fn read_node_identity_id_extracts_only_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");
        std::fs::write(
            &path,
            r#"{"node_id":"id-1","node_name":"mac","node_token":"secret"}"#,
        )
        .unwrap();
        assert_eq!(read_node_identity_id(&path).as_deref(), Some("id-1"));

        // Missing / malformed / empty id → None (anchor degrades to
        // the hostname fallback, never a bogus id).
        std::fs::write(&path, r#"{"node_name":"mac"}"#).unwrap();
        assert_eq!(read_node_identity_id(&path), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_node_identity_id(&path), None);
        assert_eq!(read_node_identity_id(&dir.path().join("absent.json")), None);
    }

    #[test]
    fn local_node_anchor_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_string_lossy().to_string();
        assert_eq!(load_persisted_local_node_id(&data_dir), None);

        persist_local_node_id(&data_dir, "id-1");
        assert_eq!(
            load_persisted_local_node_id(&data_dir).as_deref(),
            Some("id-1")
        );

        // Corruption is swallowed (the next boot just falls back to
        // the same-hostname scan).
        std::fs::write(local_node_anchor_path(&data_dir), "{oops").unwrap();
        assert_eq!(load_persisted_local_node_id(&data_dir), None);
    }

    #[test]
    fn extract_installed_agent_id() {
        assert_eq!(
            extract_node_agent_id("acowork/nodes/gpu-1/agents/com.example/installed", "gpu-1", "installed"),
            Some("com.example".to_string())
        );
    }

    #[test]
    fn extract_events_agent_id() {
        assert_eq!(
            extract_node_agent_id("acowork/nodes/gpu-1/agents/com.example/events", "gpu-1", "events"),
            Some("com.example".to_string())
        );
    }

    #[test]
    fn extract_wrong_node_is_none() {
        assert_eq!(
            extract_node_agent_id("acowork/nodes/gpu-2/agents/com.example/installed", "gpu-1", "installed"),
            None
        );
    }

    #[test]
    fn extract_wrong_kind_is_none() {
        assert_eq!(
            extract_node_agent_id("acowork/nodes/gpu-1/agents/com.example/status", "gpu-1", "installed"),
            None
        );
    }

    #[test]
    fn extract_malformed_is_none() {
        assert_eq!(extract_node_agent_id("acowork/nodes/gpu-1", "gpu-1", "installed"), None);
        assert_eq!(extract_node_agent_id("", "gpu-1", "installed"), None);
    }
}
