//! ADR-090: MCP first-enable ensure (install action lives in the Agent process).
//!
//! When the user opens an MCP server's toggle in the Desktop Tools panel,
//! the Runtime (not the Gateway) makes the server usable **on this machine**:
//!
//! ```text
//! health_check(spawn)  ──ok──▶  Usable (binary already on PATH — no install)
//!        │
//!     not-found
//!        ▼
//! run_install(spec) ──ok──▶ health_check again ──ok──▶ Usable
//!        │                                    │
//!      error                                error
//!        ▼                                    ▼
//!     Failed ─────────────────────────▶ Failed (server auto-disabled,
//!                                        toggle springs back to off)
//! ```
//!
//! "Installed or not" is never persisted — the local PATH is the shared
//! state (ADR-090 D6). The in-process registry below only tracks *in-flight
//! and last-outcome* status so the UI can render 探测中/本机可用/失败 and the
//! toggle can spring back on failure.
//!
//! ## Why the status also carries a log
//!
//! A first-time install is the only operation in the Tools panel that can take
//! minutes (a package-manager download), and it used to be an opaque spinner.
//! Each registry entry therefore also carries a bounded ring of log lines —
//! stage markers plus the installer's own stdout/stderr — served by the same
//! `GET /agents/{id}/mcp-ensure/{name}` payload the UI already polls. No new
//! route, no new service boundary, no new persisted state.
//!
//! For `npx`/`uvx` presets there is no explicit install command, so the download
//! happens *inside* the health check's spawn. That spawn's stderr is therefore
//! observed too (`health_check_with_output` → stdio transport sink) and logged
//! as `[server]`, which is what makes a one-minute silent spinner legible.
//! The long-lived connection pool's later reconnects are not observed — only
//! the ensure/verify spawns, which is where the waiting happens.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use acowork_core::process::OutputStream;
use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};

use crate::agent_config;
use crate::mcp_notify::McpNotifyRef;

/// One entry in the ensure registry.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct EnsureStatus {
    /// "probing" | "installing" | "usable" | "failed"
    pub state: String,
    /// Detailed failure reason (missing runtime + hint, install stderr
    /// summary, or handshake error). Only set when `state == "failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Bounded, chronological output of the current attempt. Absent for
    /// entries written before any line was captured.
    #[serde(default)]
    pub logs: Vec<String>,
}

/// In-process ensure registry: server name → last status.
///
/// ponytail: process-wide map assumes one agent per Runtime process (true
/// today — the Runtime is spawned per agent instance). Ceiling: if a future
/// Runtime hosts multiple agents with same-named servers, key by
/// (instance_id, name).
fn registry() -> &'static Mutex<HashMap<String, EnsureStatus>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, EnsureStatus>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Start a fresh attempt: a retry must not inherit the previous run's lines,
/// or one transcript would mix two installs and read as a contradiction.
fn begin_attempt(name: &str, state: &str) {
    registry()
        .lock()
        .expect("ensure registry lock poisoned")
        .insert(
            name.to_string(),
            EnsureStatus { state: state.into(), error: None, logs: Vec::new() },
        );
}

/// Move to a new state, carrying the attempt's log forward.
fn set_state(name: &str, state: &str, error: Option<String>) {
    let mut statuses = registry().lock().expect("ensure registry lock poisoned");
    let logs = statuses.get(name).map(|s| s.logs.clone()).unwrap_or_default();
    statuses.insert(name.to_string(), EnsureStatus { state: state.into(), error, logs });
}

/// Cap on what one attempt may hold in memory. A `cargo install` emits far
/// more than this; the tail is what matters when something hangs.
const MAX_LOG_LINES: usize = 500;
const MAX_LOG_BYTES: usize = 64 * 1024;

/// Where a log line came from — the `[tag]` prefix the UI shows.
///
/// [`OutputStream`] is the process fact; `Progress` is the runtime's own stage
/// marker, so the user sees *what* is happening even while the installer is
/// silent.
#[derive(Debug, Clone, Copy)]
enum EnsureLogSource {
    /// Install command's stdout.
    Stdout,
    /// Install command's stderr.
    Stderr,
    /// The MCP server process's own stderr — for `npx`/`uvx` presets this is
    /// where the first-launch download reports progress.
    Server,
    Progress,
}

impl EnsureLogSource {
    fn label(self) -> &'static str {
        match self {
            EnsureLogSource::Stdout => "stdout",
            EnsureLogSource::Stderr => "stderr",
            EnsureLogSource::Server => "server",
            EnsureLogSource::Progress => "progress",
        }
    }
}

impl From<OutputStream> for EnsureLogSource {
    fn from(stream: OutputStream) -> Self {
        match stream {
            OutputStream::Stdout => EnsureLogSource::Stdout,
            OutputStream::Stderr => EnsureLogSource::Stderr,
        }
    }
}

/// Append one line to an attempt's ring, dropping the oldest lines to stay
/// inside [`MAX_LOG_LINES`] / [`MAX_LOG_BYTES`].
///
/// No-op when no attempt is registered for `name` (every caller begins one
/// first) — a log without a status would be unreachable through the API.
fn append_log(name: &str, source: EnsureLogSource, line: &str) {
    let Ok(mut statuses) = registry().lock() else { return };
    let Some(status) = statuses.get_mut(name) else { return };

    let mut entry = format!("[{}] {line}", source.label());
    if entry.len() > MAX_LOG_BYTES {
        // Truncate on a char boundary — `String::truncate` panics otherwise,
        // and installer output is not guaranteed to be ASCII.
        let mut boundary = MAX_LOG_BYTES;
        while !entry.is_char_boundary(boundary) {
            boundary -= 1;
        }
        entry.truncate(boundary);
    }
    status.logs.push(entry);

    let mut total_bytes: usize = status.logs.iter().map(String::len).sum();
    while status.logs.len() > MAX_LOG_LINES || total_bytes > MAX_LOG_BYTES {
        let Some(removed) = status.logs.first() else { break };
        total_bytes = total_bytes.saturating_sub(removed.len());
        status.logs.remove(0);
    }
}

fn add_progress(name: &str, text: &str) {
    append_log(name, EnsureLogSource::Progress, text);
}

/// Stderr observer for the health-check spawn under `name`.
///
/// Handed to `acowork_mcp::health_check_with_output`: for lazy package kinds
/// that spawn is the install, and its stderr carries the download progress.
fn server_output_sink(name: &str) -> acowork_mcp::ServerOutputSink {
    let name = name.to_string();
    std::sync::Arc::new(move |line: &str| append_log(&name, EnsureLogSource::Server, line))
}

/// Snapshot of the current ensure status for `name` (None = never ensured).
pub fn ensure_status(name: &str) -> Option<EnsureStatus> {
    registry().lock().expect("ensure registry lock poisoned").get(name).cloned()
}

/// Classify a `health_check` error: is the spawn command missing
/// (⇒ install can help), or is it a different failure (⇒ install is
/// pointless and the raw reason must surface)?
fn is_command_missing(err: &str) -> bool {
    let e = err.to_lowercase();
    // Known spawn-failure shapes (see acowork-mcp client tests — the exact
    // text differs across platforms and transports):
    e.contains("program not found")
        || e.contains("entity not found")
        || e.contains("kind: notfound")
        || e.contains("no such file or directory")
        || e.contains("not recognized")
        // Windows: the stdio wrapper exits 1 on spawn failure and the
        // handshake dies with this message instead of an io::Error.
        || e.contains("mcp server closed stdout")
}

/// How long the probe may wait for the MCP handshake.
///
/// A package resolver (`npx` / `uvx` / …) downloads during its first spawn, so
/// the protocol default would abort a healthy first launch and report a broken
/// server. Everything else is a local binary and keeps the tight window.
fn probe_window_for(spawn: &McpServerConfigDef) -> std::time::Duration {
    if acowork_mcp::resolves_lazily(&spawn.command) {
        acowork_core::timeout_config::constants::MCP_LAZY_HANDSHAKE
    } else {
        acowork_core::timeout_config::constants::MCP_RECV
    }
}

/// Build the spawn def for a health check: env with the user bin dirs
/// (pipx home, `~/.cargo/bin`, ...) prepended to PATH, so a binary
/// installed by any agent on this machine resolves (ADR-090 "PATH is the
/// shared state").
fn spawn_def_with_path(def: &McpServerConfigDef) -> McpServerConfigDef {
    let mut d = def.clone();
    d.env = acowork_mcp::build_spawn_env(&def.env);
    d
}

/// The core ensure-usable routine (ADR-090 §5.4, D5/D6).
///
/// Returns `Ok(())` when the server is usable on this machine. On any
/// failure returns a detailed, user-actionable reason.
async fn ensure_usable_inner(def: &McpServerConfigDef) -> Result<(), String> {
    let spawn = spawn_def_with_path(def);
    // 1. Probe first — PATH is the state. If the binary is already here
    //    (installed by this or any other agent on this machine), done.
    add_progress(&def.name, "Checking MCP server availability");
    let probe_window = probe_window_for(&spawn);
    if acowork_mcp::resolves_lazily(&spawn.command) {
        add_progress(
            &def.name,
            &format!(
                "`{}` resolves packages on first launch — the download happens inside this probe",
                spawn.command
            ),
        );
    }
    match acowork_mcp::health_check_with_output(
        &spawn,
        Some(server_output_sink(&def.name)),
        probe_window,
    )
    .await
    {
        Ok(_tools) => {
            add_progress(&def.name, "Server answered MCP initialize — no install necessary");
            return Ok(());
        }
        Err(e) if !is_command_missing(&e) => {
            // Connectable failure that an install cannot fix.
            add_progress(&def.name, &format!("MCP probe failed: {e}"));
            return Err(e);
        }
        // Command missing → proceed to install.
        Err(e) => add_progress(&def.name, &format!("Server command unavailable: {e}")),
    }

    // 2. Install (declarative spec only — no shell). npm/uvx kinds derive
    //    no install command (lazy `npx -y` / `uvx` resolve on first spawn);
    //    `run_install` returns Ok immediately for them, and the re-probe
    //    below is what actually downloads + starts the server. The runtime
    //    probe runs regardless — a missing `npx`/`uvx` is exactly the
    //    command-not-found we just saw, and the hint is user-actionable.
    let Some(spec) = def.install.as_ref() else {
        return Err(
            "MCP server command not found on this machine and no install spec is attached \
             (custom server — install the command manually or add it via a preset)"
                .to_string(),
        );
    };
    set_state(&def.name, "installing", None);

    if let Err(missing) = acowork_mcp::ensure_runtime(&spec.package) {
        // Dependency (pipx/cargo/docker/node...) absent — never auto-install
        // underlying runtimes (ADR-072 user-agency principle).
        let acowork_mcp::DependencyStatus::Missing { runtime, install_hint } = missing else {
            unreachable!("ensure_runtime only returns Missing on Err");
        };
        add_progress(&def.name, &format!("Required runtime '{runtime}' is missing"));
        return Err(format!(
            "Missing runtime '{runtime}'. {install_hint} — install it first, then retry."
        ));
    }
    add_progress(&def.name, &install_command_summary(&spec.package));
    let mut capture =
        |stream: OutputStream, line: &str| append_log(&def.name, stream.into(), line);
    let output = acowork_mcp::run_install_with_output(&spec.package, &mut capture)
        .await
        .map_err(|e| {
            add_progress(&def.name, &format!("install error: {e}"));
            format!("install failed: {e}")
        })?;
    if !output.success {
        add_progress(
            &def.name,
            &format!("Install command exited with code {:?}", output.exit_code),
        );
        let stderr: String = output.stderr.chars().take(2000).collect();
        return Err(format!(
            "install command exited with code {:?}: {stderr}",
            output.exit_code
        ));
    }

    // 3. Re-probe. For `npx`/`uvx` presets this is where the package is
    //    actually downloaded (no explicit install command), so name the step
    //    before blocking on it.
    add_progress(&def.name, "Verifying the server starts and answers MCP initialize");
    match acowork_mcp::health_check_with_output(
        &spawn,
        Some(server_output_sink(&def.name)),
        probe_window,
    )
    .await
    {
        Ok(_) => {
            add_progress(&def.name, "Server is ready");
            Ok(())
        }
        Err(e) => {
            add_progress(&def.name, &format!("Server still unusable after install: {e}"));
            Err(format!("installed but health check still fails: {e}"))
        }
    }
}

/// One-line description of what the install will run, so a silent minute has
/// a visible cause.
fn install_command_summary(pkg: &acowork_core::protocol::McpPackageSpec) -> String {
    match acowork_mcp::derive_install_command(pkg) {
        Some(parts) => format!("Running install command: {}", parts.join(" ")),
        None => format!(
            "No explicit install command for {}; the next step resolves it",
            pkg.spec
        ),
    }
}

/// Remove a server from `active_names` (toggle spring-back, ADR-090 D6).
/// Read-modify-write; no-op when the name is not active.
fn deactivate_server(work_dir: &Path, name: &str) {
    let Some(mut cfg) = agent_config::load_agent_mcp_config(work_dir)
        .ok()
        .flatten()
    else {
        return;
    };
    let Some(active) = cfg.active_names.as_ref() else {
        return;
    };
    if !active.iter().any(|n| n == name) {
        return;
    }
    cfg.active_names = Some(active.iter().filter(|n| n.as_str() != name).cloned().collect());
    if let Err(e) = agent_config::save_agent_mcp_config(work_dir, &cfg) {
        tracing::error!(error = %e, server = %name, "ADR-090: failed to persist auto-deactivate after ensure failure");
    } else {
        tracing::info!(
            server = %name,
            "ADR-090: ensure failed — server deactivated (toggle springs back)"
        );
    }
}

/// Record a failed attempt and, when this attempt owned an install step, undo
/// the user's selection.
///
/// The asymmetry is the point: D6's spring-back exists so a switch cannot keep
/// claiming "on" after an install the Runtime itself performed failed. A custom
/// server has no install step (`install == None`) — ensure only *observed* it,
/// nothing on this machine changed, so the selection is the user's declaration
/// and stays; the failure and its log are reported instead
/// (`state == "failed"`, error text on the row).
fn record_failure(def: &McpServerConfigDef, work_dir: &Path, error: String) {
    tracing::warn!(server = %def.name, %error, "ADR-090 ensure_usable failed");
    set_state(&def.name, "failed", Some(error));
    if def.install.is_some() {
        deactivate_server(work_dir, &def.name);
        add_progress(&def.name, "Install failed — server removed from the active list");
    } else {
        add_progress(
            &def.name,
            "This server declares no install step, so its selection is left as-is",
        );
    }
}

/// Spawn the ensure task for one server if it needs ensuring.
///
/// A server "needs ensure" when: transport is stdio and no ensure is currently
/// running. **Having an install spec is not required** — a custom stdio server
/// (`npx -y something-mcp`, no spec) downloads on first launch exactly like a
/// preset does, so it needs the same probe (and the same log) even though there
/// is nothing to install; `ensure_usable_inner` reports the missing spec only
/// when the command is genuinely absent.
///
/// Idempotent while an ensure is in flight (second open-toggle click sees
/// the running "probing"/"installing" status, no duplicate task — the
/// package-manager race D7 accepted only applies to *different processes*).
/// Returns `true` when an attempt is now registered (fresh or already
/// running) — i.e. when `GET /mcp-ensure/{name}` will answer for this server.
pub fn spawn_ensure(def: McpServerConfigDef, work_dir: PathBuf, notifier: McpNotifyRef) -> bool {
    if def.transport != McpTransportDef::Stdio {
        return false;
    }
    let already_running = registry()
        .lock()
        .expect("ensure registry lock poisoned")
        .get(&def.name)
        .map(|s| s.state == "probing" || s.state == "installing")
        .unwrap_or(false);
    if already_running {
        return true;
    }
    begin_attempt(&def.name, "probing");
    let name = def.name.clone();
    tokio::spawn(async move {
        match ensure_usable_inner(&def).await {
            Ok(()) => {
                set_state(&name, "usable", None);
                // Reconnect the pool so the now-usable server's tools land
                // in agent_mcp_tools.json without a user round-trip.
                if let Some(n) = notifier {
                    n.notify();
                }
            }
            Err(error) => {
                record_failure(&def, &work_dir, error);
                if let Some(n) = notifier {
                    n.notify();
                }
            }
        }
    });
    true
}

/// Trigger ensure for every newly-enabled server in `enabled` (the diff of
/// a `PUT /agents/{id}/mcp-servers` against the previous selection).
/// Returns the names that now have a watchable ensure attempt — the HTTP
/// handler echoes exactly this list as `ensuring`, so the client never polls a
/// server that will 404 (ADR-090 D5).
pub fn ensure_newly_enabled(
    work_dir: &Path,
    enabled: &[String],
    notifier: McpNotifyRef,
) -> Vec<String> {
    let Some(cfg) = agent_config::load_agent_mcp_config(work_dir).ok().flatten() else {
        return Vec::new();
    };
    let mut ensuring = Vec::new();
    for name in enabled {
        if let Some(def) = cfg.catalog.iter().find(|d| &d.name == name)
            && spawn_ensure(def.clone(), work_dir.to_path_buf(), notifier.clone())
        {
            ensuring.push(name.clone());
        }
    }
    ensuring
}

/// Clear the registry entry when the user closes the toggle (retry = reopen,
/// D6 — a stale "failed" must not shadow the next attempt's status).
pub fn clear_status(name: &str) {
    if let Ok(mut reg) = registry().lock() {
        reg.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_missing_classification() {
        assert!(is_command_missing("failed to create transport: program not found"));
        assert!(is_command_missing("...Os { code: 2, kind: NotFound, message: \"系统找不到指定的文件。\" }"));
        assert!(!is_command_missing("timed out waiting for initialize response"));
        assert!(!is_command_missing("rejected initialize: -32601"));
    }

    #[test]
    fn log_ring_keeps_the_tail_and_stays_bounded() {
        let name = "adr090-log-ring-test";
        begin_attempt(name, "installing");
        for index in 0..(MAX_LOG_LINES + 10) {
            append_log(name, EnsureLogSource::Stdout, &format!("line-{index}"));
        }
        let status = ensure_status(name).expect("attempt registered");
        assert_eq!(status.logs.len(), MAX_LOG_LINES);
        assert!(status.logs.first().unwrap().contains("line-10"));
        assert!(status.logs.last().unwrap().contains("line-509"));
        clear_status(name);
    }

    #[test]
    fn begin_attempt_drops_the_previous_run_log() {
        let name = "adr090-log-reset-test";
        begin_attempt(name, "installing");
        append_log(name, EnsureLogSource::Stderr, "first attempt failed");
        begin_attempt(name, "probing");
        assert!(ensure_status(name).unwrap().logs.is_empty());
        clear_status(name);
    }

    #[test]
    fn state_transitions_carry_the_log_forward() {
        let name = "adr090-log-carry-test";
        begin_attempt(name, "probing");
        append_log(name, EnsureLogSource::Progress, "checking");
        set_state(name, "installing", None);
        with_state_assert(name);
        clear_status(name);
    }

    /// Split out so the assertion reads as one statement per invariant.
    fn with_state_assert(name: &str) {
        let status = ensure_status(name).expect("attempt registered");
        assert_eq!(status.state, "installing");
        assert_eq!(status.logs.len(), 1);
        assert!(status.logs[0].contains("checking"));
    }

    #[test]
    fn server_stderr_sink_lands_in_the_log_as_server_lines() {
        let name = "adr090-server-sink-test";
        begin_attempt(name, "probing");
        let sink = server_output_sink(name);
        sink("npm http fetch GET 200 https://registry.npmjs.org/playwright 42ms");
        sink("added 3 packages in 51s");
        let status = ensure_status(name).expect("attempt registered");
        assert_eq!(status.logs.len(), 2);
        assert!(status.logs[0].starts_with("[server]"), "{:?}", status.logs[0]);
        assert!(status.logs[1].contains("added 3 packages"));
        clear_status(name);
    }

    #[test]
    fn lazy_resolver_gets_the_wide_probe_window() {
        // `npx` downloads during the probe; a local binary must not.
        let lazy = McpServerConfigDef {
            name: "adr090-lazy".into(),
            transport: McpTransportDef::Stdio,
            command: "npx".into(),
            ..Default::default()
        };
        assert_eq!(
            probe_window_for(&lazy),
            acowork_core::timeout_config::constants::MCP_LAZY_HANDSHAKE
        );
        let local = McpServerConfigDef {
            name: "adr090-local".into(),
            transport: McpTransportDef::Stdio,
            command: "docling-mcp-server".into(),
            ..Default::default()
        };
        assert_eq!(
            probe_window_for(&local),
            acowork_core::timeout_config::constants::MCP_RECV
        );
    }

    #[tokio::test]
    async fn custom_stdio_server_without_spec_is_still_ensured() {
        // The regression this covers: a user-added `npx` server carries no
        // install spec, so the old guard skipped ensure entirely and the UI had
        // no progress channel (chrome-devtools, 2026-11-02).
        let def = McpServerConfigDef {
            name: "adr090-custom-no-spec".into(),
            transport: McpTransportDef::Stdio,
            command: "definitely-not-a-real-binary-adr090".into(),
            ..Default::default()
        };
        assert!(def.install.is_none(), "precondition: no install spec");
        let work_dir = std::env::temp_dir().join("adr090-ensure-custom-test");
        assert!(spawn_ensure(def.clone(), work_dir, None));
        let status = ensure_status(&def.name).expect("attempt registered for a custom server");
        assert_eq!(status.state, "probing");
        clear_status(&def.name);
    }

    /// The asymmetry D6 needs: an attempt that installed nothing may report the
    /// failure but must not rewrite the user's selection; one that owned an
    /// install must spring the switch back.
    #[test]
    fn failure_only_reverts_the_selection_when_the_attempt_owned_an_install() {
        let work_dir = std::env::temp_dir().join("adr090-failure-asymmetry-test");
        let _ = std::fs::remove_dir_all(&work_dir);
        std::fs::create_dir_all(&work_dir).unwrap();

        let write_config = || {
            let cfg = agent_config::AgentMcpConfig {
                catalog: vec![],
                local: vec![],
                active_names: Some(vec!["custom".into(), "preset".into()]),
                recommended: vec![],
                dismissed: vec![],
            };
            agent_config::save_agent_mcp_config(&work_dir, &cfg).unwrap();
        };
        let active = || {
            agent_config::load_agent_mcp_config(&work_dir)
                .unwrap()
                .unwrap()
                .active_names
                .unwrap_or_default()
        };

        // Custom server: no spec → selection untouched, failure still recorded.
        write_config();
        let custom = McpServerConfigDef {
            name: "custom".into(),
            transport: McpTransportDef::Stdio,
            command: "npx".into(),
            ..Default::default()
        };
        begin_attempt(&custom.name, "probing");
        record_failure(&custom, &work_dir, "boom".into());
        assert_eq!(ensure_status(&custom.name).unwrap().state, "failed");
        assert!(active().contains(&"custom".to_string()));

        // Preset server: has a spec → the switch must spring back.
        let preset = McpServerConfigDef {
            name: "preset".into(),
            transport: McpTransportDef::Stdio,
            command: "npx".into(),
            install: Some(acowork_core::protocol::McpInstallSpec {
                package: acowork_core::protocol::McpPackageSpec {
                    kind: acowork_core::protocol::PackageKind::Npm,
                    spec: "@playwright/mcp@latest".into(),
                    runner: None,
                    entry_point: None,
                    spawn_args: Vec::new(),
                    exec_override: None,
                    install_script: None,
                    http_probe_ports: Vec::new(),
                },
                state: acowork_core::protocol::InstallState::Unknown,
            }),
            ..Default::default()
        };
        begin_attempt(&preset.name, "installing");
        record_failure(&preset, &work_dir, "boom".into());
        assert!(!active().contains(&"preset".to_string()));

        clear_status("custom");
        clear_status("preset");
        let _ = std::fs::remove_dir_all(&work_dir);
    }

    #[test]
    fn non_stdio_server_gets_no_attempt() {
        let def = McpServerConfigDef {
            name: "adr090-http".into(),
            transport: McpTransportDef::Http,
            url: Some("https://example.invalid/mcp".into()),
            ..Default::default()
        };
        assert!(!spawn_ensure(def.clone(), std::env::temp_dir(), None));
        assert!(ensure_status(&def.name).is_none());
    }

    #[test]
    fn registry_roundtrip_and_clear() {
        let name = "adr090-test-server";
        assert!(ensure_status(name).is_none());
        begin_attempt(name, "probing");
        assert_eq!(ensure_status(name).unwrap().state, "probing");
        clear_status(name);
        assert!(ensure_status(name).is_none());
    }

    #[tokio::test]
    async fn ensure_fails_fast_on_non_spawn_error_without_spec() {
        // A stdio def with a bogus command and no install spec: health_check
        // fails with command-missing → must NOT be reported as connectable
        // failure, and without a spec the error mentions "no install spec".
        let def = McpServerConfigDef {
            name: "adr090-no-spec".into(),
            display_name: None,
            transport: McpTransportDef::Stdio,
            command: "definitely-not-a-real-binary-adr090".into(),
            ..Default::default()
        };
        // `ensure_usable` registers the attempt before delegating; mirror it so
        // the log assertions below see the real registry entry.
        begin_attempt(&def.name, "probing");
        let err = ensure_usable_inner(&def).await.unwrap_err();
        assert!(err.contains("no install spec"), "unexpected error: {err}");
        // The same run must still have left a progress trail for the UI dialog.
        let status = ensure_status(&def.name).expect("attempt registered");
        assert!(
            status.logs.iter().any(|line| line.contains("[progress]")),
            "no progress lines: {:?}",
            status.logs
        );
        clear_status(&def.name);
    }
}
