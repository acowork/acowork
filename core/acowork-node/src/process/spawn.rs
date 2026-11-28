//! Runtime process table + spawn/kill/reap + health probe (ADR-055
//! §6.20 — migrated from gateway `lifecycle/process.rs` + `manager.rs`).
//!
//! Re-based from `GatewayError` / `SharedState` to
//! [`crate::error::NodeError`] / [`crate::state::SharedNodeState`].
//! Sibling-binary location, process-group isolation, reaper and health
//! semantics are unchanged from the Gateway implementation.
//!
//! Re-adopt (§6.19) reconciles the local process table against the
//! broker's retained `acowork/agents/{id}/status` and the machine's
//! process list after a Node restart — landed in `manager.rs`.

use std::path::Path;
use std::process::Stdio;

use crate::error::{NodeError, Result};
use crate::state::{AgentSlot, SharedNodeState};

/// Handle to a spawned agent process.
///
/// Stores the PID after spawn. The actual `Child` handle is detached
/// into a background reaper task that — in addition to reaping the
/// zombie — removes the agent from the node process table when the
/// child exits, regardless of cause (idle auto-sleep, crash, kill).
pub struct AgentChild {
    pid: u32,
}

impl AgentChild {
    /// Get the process ID of the spawned agent.
    pub fn id(&self) -> u32 {
        self.pid
    }
}

/// Remove a reaped agent from the node process table and release its
/// reserved ports (spawn-time reservation, TOCTOU fix).
///
/// Lock discipline: the write guard is scoped to one synchronous
/// block. tokio RwLock is not reentrant — the 261a8f77 regression
/// self-deadlocked here by re-acquiring a read lock inside a
/// `match state.write().await ...` scrutinee (the guard lives for the
/// whole match expression), freezing every state reader (run_loop
/// heartbeat, proxy, supervisor) until the node was killed.
async fn reap_agent(state: &SharedNodeState, instance_id: &str) -> Option<AgentSlot> {
    let mut s = state.write().await;
    // ADR-073: the process table is keyed by instance id, NOT package
    // id. The pre-fix `remove_agent(agent_id)` call looked up by
    // package id, missed every time, and left dead slots forever —
    // the port allocator never released and `agents list` reported
    // a stopped Runtime as "running".
    let removed = s.remove_agent(instance_id);
    // The allocator is an Arc with its own internal lock, so
    // releasing under the state guard is safe and keeps removal +
    // release atomic; no await happens under the guard.
    if let Some(r) = &removed {
        s.port_allocator.release(r.http_port);
        if let Some(dp) = r.debug_port {
            s.port_allocator.release(dp);
        }
    }
    removed
}

/// Spawn an agent Runtime process.
///
/// `shared_state` is used by the reaper to remove the agent from the
/// node process table when the child exits. Pass `None` in tests that
/// don't have a `NodeState`.
///
/// `gateway_host` is forwarded to the Runtime as `--gateway-host`
/// (ADR-055 L3-5): the node's own MQTT connection target, so a remote
/// node's Runtimes reach the same broker.
///
/// `node_id` is forwarded as `--node-id` (ADR-055 §6.7, Phase 4): the
/// Runtime subscribes to `acowork/nodes/{node_id}/lsps` so it can
/// register the `codebase` tool when this node's LSP relay is ready.
///
/// `http_port` is the loopback HTTP port the Node allocated for this
/// Runtime (ADR-055 §6.4); `http_advertise_endpoint` is the Node
/// reverse-proxy base URL (`http://{advertise_host}:{proxy_port}`)
/// injected so the Runtime publishes `{base}/agents/{id}` as its
/// retained `http_endpoint` (§6.3). When `None` (standalone/test), the
/// Runtime falls back to the direct loopback endpoint.
///
/// `node_token` (Phase 5a): when the node holds a Gateway-issued
/// token it is injected as `--mqtt-username agent:{instance_id}` /
/// `--mqtt-password {token}` so spawned Runtimes authenticate against
/// the broker's `agent:{instance_id}` CONNECT rule (§6.8). The
/// username MUST match the client_id namespace (ADR-073: every
/// `{id}` in MQTT is the instance id).
#[allow(clippy::too_many_arguments)]
pub async fn spawn_agent_process(
    instance_id: &str,
    agent_id: &str,
    install_path: &str,
    workspace: &Path,
    dev_mode: bool,
    debug_port: Option<u16>,
    log_file_size_mb: u64,
    log_file_count: u64,
    mqtt_port: Option<u16>,
    gateway_host: &str,
    node_id: &str,
    http_port: u16,
    http_advertise_endpoint: Option<&str>,
    node_token: Option<&str>,
    shared_state: Option<SharedNodeState>,
) -> Result<AgentChild> {
    // Locate the acowork-runtime binary (sibling of current executable)
    let runtime_bin = std::env::current_exe()
        .map_err(|e| NodeError::Lifecycle(format!("Cannot find current executable: {}", e)))?
        .parent()
        .map(|p| {
            let bin_name = if cfg!(windows) {
                "acowork-runtime.exe"
            } else {
                "acowork-runtime"
            };
            p.join(bin_name)
        })
        .unwrap_or_else(|| {
            let bin_name = if cfg!(windows) {
                "acowork-runtime.exe"
            } else {
                "acowork-runtime"
            };
            std::path::PathBuf::from(bin_name)
        });

    let manifest_path = Path::new(install_path).join("manifest.toml");

    let mut cmd = tokio::process::Command::new(&runtime_bin);
    cmd.arg("--agent-id")
        .arg(agent_id)
        // ADR-073: pass the instance identity so the Runtime builds its
        // MQTT topics under `acowork/agents/{instance_id}/...` and
        // reports status on the instance-scoped path.
        .arg("--agent-instance-id")
        .arg(instance_id)
        .arg("--package-path")
        .arg(install_path)
        .arg("--manifest-path")
        .arg(&manifest_path)
        .arg("--work-dir")
        .arg(workspace);

    // ADR-055 L3-5: forward the node's Gateway broker host so the
    // Runtime connects to the same broker the node is enrolled with.
    cmd.arg("--gateway-host").arg(gateway_host);

    // ADR-055 §6.7 (Phase 4): forward the hosting node id so the Runtime
    // subscribes to this node's LSP relay topic (`acowork/nodes/{id}/lsps`)
    // and registers the `codebase` tool when the relay is ready.
    cmd.arg("--node-id").arg(node_id);

    // ADR-055 Phase 5a §6.8 + ADR-073: when the node holds a
    // Gateway-issued token, Runtimes connect to the broker with it.
    // The CONNECT username `agent:{instance_id}` mirrors the client_id
    // convention (`agent:{instance_id}` on the runtime side, verified
    // at `acowork-runtime/src/mqtt/client.rs:957`). Two instances of
    // the same package MUST end up on distinct CONNECT usernames so
    // the broker can attribute them.
    if let Some(token) = node_token {
        cmd.arg("--mqtt-username")
            .arg(format!("agent:{instance_id}"))
            .arg("--mqtt-password")
            .arg(token)
            // ADR-124 §4.1: the same token authorizes the Runtime's one HTTP
            // call back into this Node — the identity projection it issues
            // after a rename (`POST /agents/{id}/identity/refresh`). Without
            // it the Runtime can write its own name but never publish it, and
            // the list view keeps the old one. Secret: the Runtime keeps it
            // out of logs, tool results and error messages.
            .arg("--node-token")
            .arg(token);
    }

    // ADR-033: Pass MQTT port so Runtime connects via MQTT.
    if let Some(port) = mqtt_port {
        cmd.arg("--mqtt-port").arg(port.to_string());
        // ADR-055 §6.4: the Node allocates a concrete loopback port so
        // its reverse proxy can route `/agents/{id}/*` here.
        cmd.arg("--http-port").arg(http_port.to_string());
    }

    // ADR-055 §6.3: inject the Node reverse-proxy base URL so the
    // Runtime publishes `{base}/agents/{id}` as its retained
    // `http_endpoint` (the Gateway then proxies through the Node).
    if let Some(base) = http_advertise_endpoint {
        cmd.arg("--http-advertise-endpoint").arg(base);
    }

    // Developer mode: lower log level to DEBUG for detailed diagnostics
    let log_level = if dev_mode { "debug" } else { "info" };
    cmd.arg("--log-level")
        .arg(log_level)
        .arg("--log-file-size-mb")
        .arg(log_file_size_mb.to_string())
        .arg("--log-file-count")
        .arg(log_file_count.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    // Developer mode: pass --dev-mode so Runtime starts Debug Protocol.
    if dev_mode {
        cmd.arg("--dev-mode");
        if let Some(port) = debug_port {
            cmd.arg("--debug-port").arg(port.to_string());
        }
    }

    // On Unix, create a new process group so we can kill the entire group later
    #[cfg(unix)]
    #[allow(unused_imports)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|e| {
        NodeError::Lifecycle(format!(
            "Failed to spawn agent instance '{}' (package '{}', binary: {:?}): {}",
            instance_id, agent_id, runtime_bin, e
        ))
    })?;

    let pid = child.id().ok_or_else(|| {
        NodeError::Lifecycle(format!(
            "Failed to get PID for agent instance '{}' (package '{}', process may have exited immediately)",
            instance_id, agent_id
        ))
    })?;

    // Spawn a background reaper task: reap the child and remove the
    // agent from the node process table on exit.
    // ADR-073: the reaper MUST look up the slot by instance id (the
    // process-table key), not by package id.
    let instance_id_owned = instance_id.to_string();
    let agent_id_owned = agent_id.to_string();
    tokio::spawn(async move {
        let exit_status = child.wait().await;
        if let Some(state) = shared_state {
            match reap_agent(&state, &instance_id_owned).await {
                Some(removed) => {
                    tracing::info!(
                        instance_id = %instance_id_owned,
                        agent_id = %agent_id_owned,
                        pid = removed.pid,
                        exit_status = ?exit_status,
                        "Runtime process exited, removed from node process table"
                    );
                }
                None => {
                    tracing::debug!(
                        instance_id = %instance_id_owned,
                        agent_id = %agent_id_owned,
                        exit_status = ?exit_status,
                        "Runtime process exited but was not tracked in node process table"
                    );
                }
            }
        } else {
            tracing::debug!(
                instance_id = %instance_id_owned,
                agent_id = %agent_id_owned,
                exit_status = ?exit_status,
                "Runtime process exited (no shared_state wired; reaper skipped cleanup)"
            );
        }
    });

    // ADR-073: include both instance id (runtime key) and package id
    // (which package this instance belongs to) so an operator
    // distinguishing two instances of the same package can do so from
    // the spawn log line alone.
    tracing::info!(
        "Spawned agent Runtime: instance={} package={} (PID: {})",
        instance_id, agent_id, pid
    );
    Ok(AgentChild { pid })
}

/// Kill a process by PID.
///
/// On Unix: sends SIGTERM via the `kill` command.
/// On Windows: uses `taskkill /F /T /PID` to forcefully terminate the
/// process tree.
pub async fn kill_agent_process(pid: u32) -> Result<()> {
    if cfg!(unix) {
        let output = tokio::process::Command::new("kill")
            .arg(pid.to_string())
            .output()
            .await
            .map_err(|e| {
                NodeError::Lifecycle(format!("Failed to execute kill for PID {}: {}", pid, e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NodeError::Lifecycle(format!(
                "kill command failed for PID {}: {}",
                pid,
                stderr.trim()
            )));
        }
    } else {
        let output = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .await
            .map_err(|e| {
                NodeError::Lifecycle(format!(
                    "Failed to execute taskkill for PID {}: {}",
                    pid, e
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NodeError::Lifecycle(format!(
                "taskkill command failed for PID {}: {}",
                pid,
                stderr.trim()
            )));
        }
    }

    tracing::info!("Killed process: PID {}", pid);
    Ok(())
}

/// Process-local registry of allocated Runtime loopback ports.
///
/// ADR-055 §6.4: the Node allocates a concrete HTTP port per Runtime
/// (rather than `--http-port 0`) so its reverse proxy has a stable
/// `{instance_id} → port` mapping. The original probe-and-return helper
/// released its probe listener immediately, which raced when two
/// `start` commands ran concurrently (e.g. the Desktop booting the
/// system agent and a user agent at once): both probes saw the same
/// free port, both Runtimes were launched with it, the first bind
/// won and the second crashed with "Address already in use"
/// (os error 48) — leaving its HTTP server down, its `http_endpoint`
/// unpublished, and the Gateway reverse-proxy returning 503s.
///
/// `allocate` probes and reserves under one mutex, so within this
/// process two allocations can never collide; cross-process leftovers
/// (orphans, other node instances) are still rejected by the bind
/// probe. Reservations must be released on stop / exit / spawn
/// failure so ports stay reusable.
#[derive(Debug, Default)]
pub struct PortAllocator {
    used: std::sync::Mutex<std::collections::HashSet<u16>>,
}

impl PortAllocator {
    /// Create an empty allocator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve the first port `>= base` that is neither reserved in
    /// this allocator nor bound by any process. The reservation is
    /// held until [`Self::release`] is called.
    pub fn allocate(&self, base: u16) -> u16 {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        let mut port = base;
        loop {
            if !used.contains(&port) {
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                if std::net::TcpListener::bind(addr).is_ok() {
                    used.insert(port);
                    tracing::info!(port, "Allocated Runtime loopback port");
                    return port;
                }
            }
            tracing::debug!(port, "Port in use or reserved, trying next");
            port += 1;
        }
    }

    /// Release a previously allocated port so it can be re-used by a
    /// later allocation.
    pub fn release(&self, port: u16) {
        self.used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&port);
    }
}

/// Check if a process with the given PID is still running (async).
///
/// Linux: `/proc/{pid}`; Windows: `tasklist`; macOS/other: `ps -p`.
pub async fn check_health(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        tokio::fs::metadata(format!("/proc/{}", pid)).await.is_ok()
    } else if cfg!(windows) {
        match tokio::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    } else {
        match tokio::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
        {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }
}

/// Synchronous process liveness check (no await, safe inside locks).
///
/// Linux: checks `/proc/{pid}` (instant). Other platforms: returns
/// `true` (assumes alive — self-corrects on next call).
#[cfg(target_os = "linux")]
pub fn is_process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

#[cfg(not(target_os = "linux"))]
pub fn is_process_alive(_pid: u32) -> bool {
    true // fallback: assume alive if we have a PID record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_child_id() {
        let child = AgentChild { pid: 12345 };
        assert_eq!(child.id(), 12345);
    }

    #[tokio::test]
    async fn reap_agent_removes_slot_and_releases_ports_without_deadlock() {
        use crate::state::NodeState;
        use std::sync::Arc;
        use tokio::sync::RwLock;
        use tokio::time::{timeout, Duration};

        let state: SharedNodeState = Arc::new(RwLock::new(NodeState::new(8)));
        // Fake ports: the reap path only forwards them to
        // `PortAllocator::release` (idempotent for unreserved ports).
        // Real allocations would race the sibling PortAllocator tests
        // over the shared loopback port namespace.
        let http_port = acowork_core::node::NODE_HTTP_PORT_BASE;
        let debug_port = http_port + 1;
        let instance_id = "550e8400-e29b-41d4-a716-446655440000";
        state.write().await.add_agent(AgentSlot {
            instance_id: instance_id.to_string(),
            agent_id: "com.test.reap".to_string(),
            pid: 424242,
            started_at: chrono::Utc::now(),
            workspace: "/tmp".to_string(),
            dev_mode: true,
            debug_port: Some(debug_port),
            http_port,
        });

        // Regression guard for 261a8f77: re-acquiring a read lock
        // under the write guard would hang reap_agent forever.
        let removed = timeout(Duration::from_secs(5), reap_agent(&state, instance_id))
            .await
            .expect("reap_agent must not deadlock")
            .expect("agent slot must be removed");

        assert_eq!(removed.http_port, http_port);
        assert_eq!(removed.debug_port, Some(debug_port));
        assert!(!state.read().await.is_running(instance_id));
    }

    #[test]
    fn port_allocator_concurrent_allocations_are_disjoint() {
        // Two allocations from the same base must never collide: the
        // first reservation is held until released (the TOCTOU fix).
        // Dedicated base: sibling PortAllocator tests share the global
        // loopback port namespace and would otherwise race over the
        // same ports (Windows rebind hysteresis amplifies this).
        let alloc = PortAllocator::new();
        let first = alloc.allocate(acowork_core::node::NODE_HTTP_PORT_BASE + 100);
        let second = alloc.allocate(acowork_core::node::NODE_HTTP_PORT_BASE + 100);
        assert_ne!(first, second);
    }

    #[test]
    fn port_allocator_released_port_is_reusable() {
        // Dedicated base (see concurrent_allocations_are_disjoint).
        let alloc = PortAllocator::new();
        let first = alloc.allocate(acowork_core::node::NODE_HTTP_PORT_BASE + 200);
        alloc.release(first);
        let again = alloc.allocate(acowork_core::node::NODE_HTTP_PORT_BASE + 200);
        assert_eq!(again, first);
    }

    #[test]
    fn port_allocator_skips_externally_bound_port() {
        // A port held by another process must be skipped even if it is
        // not reserved in this allocator (cross-process guard).
        let alloc = PortAllocator::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied = listener.local_addr().unwrap().port();
        let got = alloc.allocate(occupied);
        assert_ne!(got, occupied);
        alloc.release(got);
    }

    #[tokio::test]
    async fn test_spawn_nonexistent_binary() {
        // Spawning a non-existent agent should fail (binary won't be found).
        let result = spawn_agent_process(
            "inst-com.test.nonexistent",
            "com.test.nonexistent",
            "/nonexistent/path",
            Path::new("/tmp/nonexistent-workspace"),
            false,
            None,
            10,
            20,
            None,
            "127.0.0.1",
            "test-node",
            acowork_core::node::NODE_HTTP_PORT_BASE,
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_check_health_current_process() {
        let pid = std::process::id();
        assert!(check_health(pid).await);
    }

    #[tokio::test]
    async fn test_check_health_nonexistent_pid() {
        assert!(!check_health(999999999).await);
    }

    #[tokio::test]
    async fn test_kill_nonexistent_pid() {
        let result = kill_agent_process(999999999).await;
        assert!(result.is_err());
    }
}
