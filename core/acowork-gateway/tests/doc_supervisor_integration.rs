//! acowork-doc supervisor integration test (D0 exit).
//!
//! Verifies the D0-4 / D0 acceptance against the **real** `acowork-doc`
//! binary without booting a second Gateway:
//!   1. the supervisor spawns doc and writes `doc_process` with a reachable
//!      port (`GET /health` answers `process = acowork-doc`);
//!   2. after the process is killed, the supervisor auto-restarts it
//!      (exponential backoff) and `doc_process` is repopulated.
//!
//! Mirrors the pm-dev-plan lifecycle checks; the restart machinery itself is
//! shared `acowork-core::supervisor` (ADR-019), this test guards *our*
//! wiring (spawn args, port file, state write).

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use acowork_gateway::gateway::state::GatewayState;
use acowork_gateway::lifecycle::doc_supervisor::{DocProcessState, DocSupervisorConfig};
use tokio::sync::RwLock;
use tokio::time::sleep;

/// Pick a free TCP port to reduce collision risk during parallel test runs.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Locate the `acowork-doc` binary relative to the test executable.
fn doc_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let dir = exe
        .parent()
        .and_then(|p| p.parent())
        .expect("target/debug dir");
    let name = if cfg!(windows) { "acowork-doc.exe" } else { "acowork-doc" };
    let candidate = dir.join(name);
    if candidate.exists() {
        candidate
    } else {
        dir.parent().expect("target root").join(name)
    }
}

/// Kill a process by OS pid (cross-platform test helper).
fn kill_pid(pid: u32) {
    let result = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .output()
    } else {
        Command::new("kill").arg("-9").arg(pid.to_string()).output()
    };
    let _ = result;
}

/// Poll `doc_process` until it becomes `Some` (or timeout).
async fn wait_doc_process(
    state: &Arc<RwLock<GatewayState>>,
    timeout: Duration,
) -> Option<DocProcessState> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(proc) = state.read().await.doc_process.clone() {
            return Some(proc);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// Poll `doc_process` until it holds a process with a **different** pid than
/// `previous` (i.e. the supervisor has respawned after a kill).
async fn wait_doc_process_pid(
    state: &Arc<RwLock<GatewayState>>,
    previous: u32,
    timeout: Duration,
) -> Option<DocProcessState> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(proc) = state.read().await.doc_process.clone()
            && proc.pid != previous
        {
            return Some(proc);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// Start the doc supervisor on a fresh temp dir; returns state handle + cfg.
fn start_supervisor(port: u16) -> (Arc<RwLock<GatewayState>>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let state = Arc::new(RwLock::new(GatewayState::new(
        &tmp.path().join("vault").to_string_lossy(),
    )));
    let cfg = DocSupervisorConfig {
        doc_bin: doc_binary(),
        port,
        port_file: tmp.path().join("doc.port"),
        log_dir: tmp.path().join("logs"),
        gateway_health_url: "http://127.0.0.1:9/health".to_string(), // unreachable → watchdog idle
        data_dir: Some(tmp.path().join("data")),
        request_ttl_hours: None,
        mqtt_port: 19875,
        mqtt_password: None,
        // Plan §7 G5: standalone test supervisor has no Gateway to push
        // receipts to — same shape as the pre-G5 process.
        gateway_url: None,
        gateway_token: None,
    };
    acowork_gateway::lifecycle::doc_supervisor::start_doc_supervisor(cfg, state.clone());
    (state, tmp)
}

/// Supervisor spawns doc, writes `doc_process`, and `/health` answers with
/// the doc identity on the reported port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_spawns_doc_and_reports_port() {
    let port = free_port();
    let (state, _tmp) = start_supervisor(port);

    let proc = wait_doc_process(&state, Duration::from_secs(15))
        .await
        .expect("doc_process should be populated after startup grace");
    assert!(proc.ready, "doc_process should be ready");
    assert_eq!(proc.port, port, "doc should bind the requested free port");

    let resp = reqwest::get(format!("http://127.0.0.1:{}/health", port))
        .await
        .expect("GET doc /health");
    assert_eq!(resp.status(), 200, "doc /health should return 200");
    let body: serde_json::Value = resp.json().await.expect("health body is JSON");
    assert_eq!(body["process"], "acowork-doc");
}

/// Killing the doc process triggers the supervisor's auto-restart; the new
/// process reports a (possibly different) port and stays healthy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_restarts_doc_after_kill() {
    let port = free_port();
    let (state, _tmp) = start_supervisor(port);

    let first = wait_doc_process(&state, Duration::from_secs(15))
        .await
        .expect("doc_process should be populated after startup grace");
    assert!(first.ready);
    assert!(reqwest::get(format!("http://127.0.0.1:{}/health", first.port))
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false));

    // Kill the process — the supervisor must detect exit and respawn.
    kill_pid(first.pid);

    // Wait for the supervisor to clear the old entry and repopulate with a
    // genuinely *new* process (the old Some may linger a moment after kill).
    let second = wait_doc_process_pid(&state, first.pid, Duration::from_secs(20))
        .await
        .expect("doc_process should be repopulated after auto-restart");
    // Allow the restart to bind a fresh port (the old port may be in
    // TIME_WAIT), so only require readiness + reachable health.
    assert!(second.ready, "restarted doc should be ready");
    assert!(
        reqwest::get(format!("http://127.0.0.1:{}/health", second.port))
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false),
        "restarted doc /health should answer on its reported port"
    );
}
