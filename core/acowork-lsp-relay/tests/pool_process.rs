//! Process-level tests for [`LspPool`].
//!
//! These live in `tests/` rather than in `src/pool.rs`'s unit module because
//! every case spawns a real child process, and an integration target is the
//! only place Cargo exposes `CARGO_BIN_EXE_*` — which is how the pool gets a
//! **portable** mock LSP server. The unit module used to hardcode the POSIX
//! `cat` (and `echo`), so all twelve cases were red on any Windows machine
//! ("program not found") while Linux CI stayed green; `mock_lsp` copies bytes
//! stdin → stdout exactly like `cat` did, so the behaviour under test is
//! unchanged, only the fixture is now cross-platform.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use acowork_lsp_relay::pool::LspPool;

/// The mock LSP server — Cargo builds this package's bins before running
/// this target, so the path always exists.
const MOCK_CMD: &str = env!("CARGO_BIN_EXE_mock_lsp");

/// The flag that makes the mock exit at once instead of echoing: stands in
/// for the old `echo` invocation, to exercise stale-entry detection.
const EXIT_NOW_ARGS: &[&str] = &["--exit-now"];

fn mock_args() -> Vec<String> {
    vec![]
}

fn mock_workspace() -> String {
    std::env::temp_dir().to_string_lossy().to_string()
}

#[tokio::test]
async fn get_or_spawn_creates_new_entry() {
    let pool = LspPool::new();
    let entry = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("should spawn mock LSP");

    assert_eq!(entry.command, MOCK_CMD);
    assert_eq!(entry.workspace_root, mock_workspace());
    assert!(entry.pid > 0);
    assert_eq!(
        entry.active_clients.load(Ordering::Relaxed),
        1,
        "first client should set active_clients=1"
    );
    assert!(
        entry.last_idle_since.lock().await.is_none(),
        "should not be idle while client is active"
    );
}

#[tokio::test]
async fn get_or_spawn_reuses_existing_entry() {
    let pool = LspPool::new();

    let entry1 = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("first spawn");

    let entry2 = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("should reuse");

    assert_eq!(
        entry1.pid, entry2.pid,
        "same key must return the same process"
    );
    assert_eq!(
        entry2.active_clients.load(Ordering::Relaxed),
        2,
        "second client should increment to 2"
    );
}

#[tokio::test]
async fn client_disconnected_decrements_count() {
    let pool = LspPool::new();

    let entry = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("spawn");

    assert_eq!(entry.active_clients.load(Ordering::Relaxed), 1);

    pool.client_disconnected(MOCK_CMD, &mock_args(), &mock_workspace())
        .await;

    assert_eq!(
        entry.active_clients.load(Ordering::Relaxed),
        0,
        "after disconnect, active_clients should be 0"
    );
    assert!(
        entry.last_idle_since.lock().await.is_some(),
        "entry should be marked idle after last client disconnects"
    );
}

#[tokio::test]
async fn client_disconnected_does_not_mark_idle_if_other_clients_remain() {
    let pool = LspPool::new();

    // Two clients connect
    let entry = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("first spawn");
    pool.get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("second spawn");

    assert_eq!(entry.active_clients.load(Ordering::Relaxed), 2);

    // One disconnects
    pool.client_disconnected(MOCK_CMD, &mock_args(), &mock_workspace())
        .await;

    assert_eq!(
        entry.active_clients.load(Ordering::Relaxed),
        1,
        "one client should remain"
    );
    assert!(
        entry.last_idle_since.lock().await.is_none(),
        "should not be idle while a client is still active"
    );
}

#[tokio::test]
async fn reap_idle_removes_idle_entries() {
    let pool = LspPool::new();

    // Spawn and immediately disconnect → entry becomes idle
    let entry = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("spawn");
    pool.client_disconnected(MOCK_CMD, &mock_args(), &mock_workspace())
        .await;

    // Wait a tiny bit so `elapsed()` > 0
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Reap with 1ms timeout — should remove the idle entry
    pool.reap_idle(Duration::from_millis(1)).await;

    // A new get_or_spawn should create a NEW process (different PID),
    // because the old one was reaped.
    let entry2 = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("new spawn after reap");

    assert_ne!(
        entry.pid, entry2.pid,
        "reaped entry should be replaced by a new process"
    );
}

#[tokio::test]
async fn reap_idle_does_not_remove_active_entries() {
    let pool = LspPool::new();

    // Spawn but don't disconnect — entry is active
    let entry = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("spawn");

    // Reap with very short timeout — should NOT remove the active entry
    pool.reap_idle(Duration::from_millis(0)).await;

    // Same PID should be returned (entry was not reaped)
    let entry2 = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("should still exist");

    assert_eq!(
        entry.pid, entry2.pid,
        "active entry should not be reaped"
    );
}

#[tokio::test]
async fn different_workspaces_get_different_processes() {
    let pool = LspPool::new();

    let ws1 = std::env::temp_dir().to_string_lossy().to_string();
    let ws2 = std::env::temp_dir()
        .join("other")
        .to_string_lossy()
        .to_string();
    std::fs::create_dir_all(&ws2).ok();

    let entry1 = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &ws1)
        .await
        .expect("spawn ws1");
    let entry2 = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &ws2)
        .await
        .expect("spawn ws2");

    assert_ne!(
        entry1.pid, entry2.pid,
        "different workspaces should get different processes"
    );
}

// ── make_key tests (indirect via get_or_spawn behavior) ─────────────────

#[tokio::test]
async fn make_key_differentiates_by_args() {
    // Same command + workspace, different args → different pool keys. The mock
    // ignores unknown arguments, so both invocations start normally and differ
    // only by key.
    let pool = LspPool::new();
    let ws = mock_workspace();

    let entry1 = pool
        .get_or_spawn(MOCK_CMD, &[], &ws)
        .await
        .expect("spawn no args");

    let entry2 = pool
        .get_or_spawn(MOCK_CMD, &["--stdio".to_string()], &ws)
        .await
        .expect("spawn with args");

    assert_ne!(
        entry1.pid, entry2.pid,
        "different args should produce different pool keys"
    );
}

// ── stdin/stdout relay tests ────────────────────────────────────────────

#[tokio::test]
async fn stdin_stdout_relay_round_trip() {
    // The mock echoes stdin to stdout, so the pool's two relay tasks form a
    // round trip:
    //   - stdin writer: receives JSON text, prepends a Content-Length header,
    //     writes to child stdin
    //   - stdout reader: parses the header, extracts the JSON body, broadcasts
    //     it on a channel
    // So sending JSON via stdin_tx should result in the same JSON arriving on
    // stdout_tx.
    let pool = LspPool::new();
    let entry = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("spawn");

    let test_msg = r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#;

    // Send a message through stdin
    entry
        .stdin_tx
        .send(test_msg.to_string())
        .expect("send to stdin");

    // Subscribe to stdout BEFORE sending (broadcast only delivers to
    // existing subscribers)
    let mut rx = entry.stdout_tx.subscribe();

    // Re-send because we subscribed after the first send
    entry
        .stdin_tx
        .send(test_msg.to_string())
        .expect("re-send to stdin");

    // Wait for the response with a timeout
    let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout waiting for stdout");

    assert_eq!(
        received.unwrap(),
        test_msg,
        "the mock should echo back the same JSON message"
    );
}

#[tokio::test]
async fn stdin_stdout_relay_multiple_messages() {
    let pool = LspPool::new();
    let entry = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("spawn");

    let mut rx = entry.stdout_tx.subscribe();

    let messages = vec![
        r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#,
        r#"{"jsonrpc":"2.0","method":"shutdown","id":2}"#,
        r#"{"jsonrpc":"2.0","method":"exit"}"#,
    ];

    for msg in &messages {
        entry
            .stdin_tx
            .send(msg.to_string())
            .expect("send to stdin");
    }

    // Receive all messages (the mock echoes them back)
    let mut received = Vec::new();
    for _ in &messages {
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for stdout")
            .expect("channel closed");
        received.push(msg);
    }

    assert_eq!(
        received.len(),
        messages.len(),
        "should receive all echoed messages"
    );
    for (i, msg) in messages.iter().enumerate() {
        assert_eq!(&received[i], msg, "message {i} mismatch");
    }
}

// ── start_reaper tests ──────────────────────────────────────────────────

#[tokio::test]
async fn start_reaper_evicts_idle_processes() {
    // start_reaper uses REAPER_INTERVAL (60s) and DEFAULT_IDLE_TIMEOUT
    // (600s) — too slow for tests. Instead, we verify that start_reaper
    // can be called without panicking, and that the reaper logic
    // (reap_idle) works correctly when called manually (already tested
    // in reap_idle_removes_idle_entries).
    //
    // Here we just verify start_reaper spawns a task successfully.
    let pool = Arc::new(LspPool::new());

    // Spawn a mock process so the pool has something to reap
    let _entry = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("spawn");

    // Start the reaper — should not panic
    LspPool::start_reaper(Arc::clone(&pool));

    // Give it a moment to tick
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The process should still be active (reaper interval is 60s,
    // and the process hasn't been disconnected)
    let entry2 = pool
        .get_or_spawn(MOCK_CMD, &mock_args(), &mock_workspace())
        .await
        .expect("should still exist");

    // Same PID = not reaped
    assert!(_entry.pid == entry2.pid);
}

#[tokio::test]
async fn pool_handles_process_death() {
    // When a process dies (stdin_tx is closed), get_or_spawn should
    // detect the stale entry and spawn a new process.
    let pool = LspPool::new();
    let ws = mock_workspace();

    // Spawn a short-lived process: the mock exits immediately with --exit-now
    let exit_now = EXIT_NOW_ARGS.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let _entry1 = pool
        .get_or_spawn(MOCK_CMD, &exit_now, &ws)
        .await
        .expect("spawn exiting mock");

    // Wait for it to exit
    tokio::time::sleep(Duration::from_millis(200)).await;

    // get_or_spawn should detect the dead process and spawn a new one
    let entry2 = pool
        .get_or_spawn(MOCK_CMD, &exit_now, &ws)
        .await
        .expect("should spawn new process");

    // The new process may or may not have a different PID depending
    // on timing, but the pool should not return a stale entry.
    // If stdin_tx is closed, the entry is stale and should be replaced.
    assert!(
        !entry2.stdin_tx.is_closed(),
        "new entry's stdin_tx should be open"
    );
}
