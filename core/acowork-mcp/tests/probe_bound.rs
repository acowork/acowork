//! Regression: the handshake bound belongs to the caller, not to the transport.
//!
//! A first enable of a lazily resolved MCP package (`uvx`, `npx`) downloads
//! *inside* the probe's own spawn, and the transfer can be silent far longer
//! than the protocol's 30 s round-trip default (measured: 31 s of complete
//! silence for a 12 MiB wheel). A constant inside the stdio transport cannot
//! tell that from a hang, and it used to fire under an `None` (= unbounded)
//! caller — the Runtime's probe — turning a healthy install into
//! "timeout waiting for MCP response" (ADR-090 §5.4).

use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};
use acowork_mcp::health_check_with_output;
use std::time::{Duration, Instant};

const INIT_RESPONSE: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}}"#;
const LIST_RESPONSE: &str = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;

fn def(command: &str, args: &[&str]) -> McpServerConfigDef {
    McpServerConfigDef {
        name: "fake-slow-start".into(),
        transport: McpTransportDef::Stdio,
        command: command.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        ..Default::default()
    }
}

/// A server that stays completely silent for `delay`, then answers the
/// handshake — the shape of `uvx <pkg>` while it downloads.
#[cfg(windows)]
fn sleepy_spawn(delay: Duration) -> McpServerConfigDef {
    let script = format!(
        "Start-Sleep -Milliseconds {}; \
         [Console]::Out.Write('{INIT_RESPONSE}'); [Console]::Out.Write(\"`n\"); \
         [Console]::Out.Write('{LIST_RESPONSE}'); [Console]::Out.Write(\"`n\"); \
         [Console]::Out.Flush(); Start-Sleep -Seconds 2",
        delay.as_millis()
    );
    def("powershell", &["-NoProfile", "-NonInteractive", "-Command", &script])
}

#[cfg(unix)]
fn sleepy_spawn(delay: Duration) -> McpServerConfigDef {
    let script = format!(
        "sleep {}; printf '%s\\n' '{INIT_RESPONSE}' '{LIST_RESPONSE}'; sleep 2",
        delay.as_secs_f64()
    );
    def("sh", &["-c", &script])
}

#[tokio::test]
async fn caller_bound_is_honoured_and_none_waits_for_a_silent_child() {
    let silence = Duration::from_millis(2500);

    // A bounded caller still gets its bound — the transport is not free-running.
    let bounded = health_check_with_output(
        &sleepy_spawn(silence),
        None,
        Some(Duration::from_millis(500)),
    )
    .await;
    assert!(
        bounded.is_err(),
        "a 500ms bound must not wait out a 2.5s silent start"
    );

    // An unbounded caller waits for the child instead of dying at 30s.
    let started = Instant::now();
    let unbounded = health_check_with_output(&sleepy_spawn(silence), None, None).await;
    assert!(unbounded.is_ok(), "unbounded probe failed: {unbounded:?}");
    assert!(
        started.elapsed() >= silence,
        "probe returned before the child answered"
    );
}
