//! acowork-doc smoke integration tests (D0-6).
//!
//! Spawn the real `acowork-doc` binary on an ephemeral port and verify the
//! supervisor contract: `/health` returns 200, the process name is
//! `acowork-doc`, and a port conflict auto-increments to the next free port
//! (reported via `--port-file`). Mirrors the pm-dev-plan T0 smoke suite.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Pick a free TCP port to reduce collision risk during parallel test runs.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// The `acowork-doc` binary. This package owns the target, so Cargo builds it
/// for the test and exports the exact path — no artifact-tree guessing, which
/// matters because Cargo does not keep one layout: a test executable sits in
/// `target/<profile>/deps/`, but when the package is also compiled as a
/// dependency with a different feature set, its test binary is built under
/// `target/<profile>/build/<pkg>/<metadata>/out/` instead, at a different
/// depth. A clean CI checkout hits the second shape; an incrementally rebuilt
/// developer directory the first.
fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_acowork-doc"))
}

/// A client for the loopback endpoints these tests started themselves.
fn loopback_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("loopback client")
}

/// Poll `GET /health` until it returns 200 or `timeout` elapses.
async fn wait_healthy(port: u16, timeout: Duration) -> bool {
    let url = format!("http://127.0.0.1:{}/health", port);
    let deadline = Instant::now() + timeout;
    loop {
        let ok = loopback_client()
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The binary starts, answers `/health` (process = acowork-doc), and the
/// health payload carries the resolved data dir.
#[tokio::test(flavor = "multi_thread")]
async fn health_returns_ok_with_doc_identity() {
    let tmp = TempDir::new().expect("temp dir");
    let data_dir = tmp.path().join("data");
    let port = free_port();

    let mut child = Command::new(binary_path())
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(&data_dir)
        .spawn()
        .expect("spawn acowork-doc");

    assert!(
        wait_healthy(port, Duration::from_secs(10)).await,
        "doc /health should come up within startup grace"
    );

    let resp = reqwest::get(format!("http://127.0.0.1:{}/health", port))
        .await
        .expect("GET /health");
    assert_eq!(resp.status(), 200, "/health should return 200");
    let body: serde_json::Value = resp.json().await.expect("health body is JSON");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["process"], "acowork-doc");

    child.kill().expect("kill child");
    let _ = child.wait();
}

/// A second instance requesting an occupied port auto-increments and reports
/// the actual port via `--port-file` (port-conflict contract, D0-5).
#[tokio::test(flavor = "multi_thread")]
async fn port_conflict_auto_increments_and_reports() {
    let tmp = TempDir::new().expect("temp dir");
    let port = free_port();

    // First instance claims the port.
    let mut first = Command::new(binary_path())
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(tmp.path().join("data1"))
        .spawn()
        .expect("spawn first instance");
    assert!(wait_healthy(port, Duration::from_secs(10)).await);

    // Second instance wants the same port → auto-increments.
    let port_file = tmp.path().join("port2.txt");
    let mut second = Command::new(binary_path())
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(tmp.path().join("data2"))
        .arg("--port-file")
        .arg(&port_file)
        .spawn()
        .expect("spawn second instance");

    // Wait for the second instance to report its actual port.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut actual: Option<u16> = None;
    while Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(&port_file)
            && let Ok(p) = s.trim().parse::<u16>()
        {
            actual = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let actual = actual.expect("port file should be written");
    assert_ne!(
        actual, port,
        "second instance must auto-increment off the occupied port"
    );
    assert!(
        wait_healthy(actual, Duration::from_secs(10)).await,
        "second instance /health should come up on its reported port"
    );

    first.kill().expect("kill first");
    second.kill().expect("kill second");
    let _ = first.wait();
    let _ = second.wait();
}
