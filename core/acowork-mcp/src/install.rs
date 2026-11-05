//! Generic MCP installer (ADR-072).
//!
//! Declarative install & spawn derivation from a [`McpPackageSpec`].
//! A preset declares facts (package manager x package id x spawn shape);
//! this module owns the procedural parts: runtime probing, install command
//! derivation, spawn config derivation, health checks, and idempotency.
//!
//! Design principles (ADR-072):
//! - Spawn command is NOT derived as "package name" — package name, entry
//!   point, and spawn args are independent axes (docling proved this).
//! - Runtime dependencies (uv/pipx/node/docker/...) are detected but NOT
//!   auto-installed (user agency). Missing runtime -> structured guidance.
//! - Health check is always the same MCP initialize handshake, never
//!   duplicated inside per-server scripts.

use std::collections::HashMap;
use std::time::Duration;

use acowork_core::process::{OutputStream, run_command_with_idle_timeout_with_output};
use acowork_core::protocol::{
    ExecOverride, McpPackageSpec, McpServerConfigDef, McpTransportDef, PackageKind, PypiRunner,
};

/// Idle timeout for install subprocesses (mirrors the LSP installer).
const INSTALL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

// ── Dependency probing ────────────────────────────────────────────────────

/// Outcome of a runtime dependency probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyStatus {
    /// Runtime present — install can proceed.
    Ready,
    /// Runtime missing — caller should surface the guidance hint.
    Missing {
        /// Human-readable runtime name, e.g. "uvx".
        runtime: String,
        /// Concrete install guidance, e.g. "pip install uv".
        install_hint: String,
    },
}

/// Check whether a binary is resolvable on PATH (fast, sync, no output captured).
fn binary_on_path(binary: &str) -> bool {
    let (prog, arg) = if cfg!(windows) {
        ("where", binary)
    } else {
        ("which", binary)
    };
    std::process::Command::new(prog)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn missing_if_absent(binary: &str, hint: &str) -> DependencyStatus {
    if binary_on_path(binary) {
        DependencyStatus::Ready
    } else {
        DependencyStatus::Missing {
            runtime: binary.to_string(),
            install_hint: hint.to_string(),
        }
    }
}

/// Probe the runtime required by a package kind (ADR-072 decision 2).
pub fn probe_runtime(pkg: &McpPackageSpec) -> DependencyStatus {
    match pkg.kind {
        PackageKind::Npm => missing_if_absent("npx", "Install Node.js (https://nodejs.org)"),
        PackageKind::Pypi => match pkg.runner {
            Some(PypiRunner::Uvx) => {
                missing_if_absent("uvx", "Install uv: `pip install uv` or https://docs.astral.sh/uv/")
            }
            Some(PypiRunner::Pipx) => missing_if_absent(
                "pipx",
                "Install pipx: `pip install pipx` or https://pipx.pypa.io/",
            ),
            Some(PypiRunner::Pip) | None => {
                if binary_on_path("python") || binary_on_path("python3") {
                    if binary_on_path("pip") || binary_on_path("pip3") {
                        DependencyStatus::Ready
                    } else {
                        DependencyStatus::Missing {
                            runtime: "pip".to_string(),
                            install_hint: "Install Python (https://python.org)".to_string(),
                        }
                    }
                } else {
                    DependencyStatus::Missing {
                        runtime: "python".to_string(),
                        install_hint: "Install Python (https://python.org)".to_string(),
                    }
                }
            }
        },
        PackageKind::Cargo => {
            missing_if_absent("cargo", "Install Rust toolchain: https://rustup.rs")
        }
        PackageKind::Go => {
            missing_if_absent("go", "Install Go: https://go.dev/dl")
        }
        PackageKind::Docker => {
            missing_if_absent("docker", "Install Docker Desktop: https://www.docker.com/products/docker-desktop")
        }
        PackageKind::Binary => DependencyStatus::Ready,
        PackageKind::Script => DependencyStatus::Ready,
    }
}

// ── Derivation helpers ────────────────────────────────────────────────────

/// Effective entry point (command name) for a package.
fn effective_entry_point(pkg: &McpPackageSpec) -> String {
    pkg.entry_point
        .clone()
        .unwrap_or_else(|| pkg.spec.clone())
}

/// Module name for `python -m` (pypi spec dashed -> underscored).
fn python_module(pkg: &McpPackageSpec) -> String {
    pkg.entry_point
        .clone()
        .unwrap_or_else(|| pkg.spec.replace('-', "_"))
}
// ── Spawn derivation ──────────────────────────────────────────────────────

/// Extra bin dirs where user-level package managers (uv/pipx/pip) install
/// executables — appended to PATH so spawned servers resolve on Windows
/// where the parent process PATH may not include them yet.
fn user_bin_dirs() -> Vec<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let mut dirs = Vec::new();
    if !home.is_empty() {
        let home_path = std::path::PathBuf::from(&home);
        dirs.push(home_path.join(".local").join("bin")); // uv / pipx
        dirs.push(home_path.join(".cargo").join("bin")); // cargo
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        dirs.push(std::path::PathBuf::from(appdata).join("Python").join("Scripts"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let base = std::path::PathBuf::from(local).join("Programs").join("Python");
        if let Ok(entries) = std::fs::read_dir(&base) {
            for e in entries.flatten() {
                dirs.push(e.path().join("Scripts"));
            }
        }
    }
    dirs
}

/// Build a spawn environment with user bin dirs prepended to PATH.
pub fn build_spawn_env(extra: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = extra.clone();
    let path_key = if cfg!(windows) { "Path" } else { "PATH" };
    let mut path = std::env::var(path_key).unwrap_or_default();
    let mut prefix = String::new();
    for dir in user_bin_dirs() {
        if dir.exists() {
            prefix.push_str(&format!("{};", dir.display()));
        }
    }
    if !prefix.is_empty() {
        if !path.is_empty() {
            path = format!("{prefix}{path}");
        } else {
            path = prefix.trim_end_matches(';').to_string();
        }
    }
    // Prefer the canonical uppercase key on all platforms for consistency.
    env.insert("PATH".to_string(), path);
    env
}

/// Derive the spawn config (command/args/env) from a package spec.
///
/// `exec_override` takes precedence; otherwise the kind-specific matrix from
/// ADR-072 decision 2 applies.
pub fn derive_spawn_config(name: &str, pkg: &McpPackageSpec) -> McpServerConfigDef {
    let mut cfg = McpServerConfigDef {
        name: name.to_string(),
        display_name: None,
        transport: McpTransportDef::Stdio,
        ..Default::default()
    };

    if let Some(ExecOverride { command, args }) = &pkg.exec_override {
        cfg.command = command.clone();
        cfg.args = args.clone();
        cfg.env = build_spawn_env(&HashMap::new());
        return cfg;
    }

    let entry = effective_entry_point(pkg);
    let args = pkg.spawn_args.clone();
    match pkg.kind {
        PackageKind::Npm => {
            cfg.command = "npx".to_string();
            let mut full = vec!["-y".to_string(), pkg.spec.clone()];
            full.extend(args);
            cfg.args = full;
        }
        PackageKind::Pypi => match pkg.runner {
            Some(PypiRunner::Uvx) | None => {
                cfg.command = "uvx".to_string();
                let mut full = vec!["--from".to_string(), pkg.spec.clone(), entry];
                full.extend(args);
                cfg.args = full;
            }
            Some(PypiRunner::Pipx) => {
                cfg.command = entry;
                cfg.args = args;
            }
            Some(PypiRunner::Pip) => {
                cfg.command = "python".to_string();
                let mut full = vec!["-m".to_string(), python_module(pkg)];
                full.extend(args);
                cfg.args = full;
            }
        },
        PackageKind::Cargo | PackageKind::Go => {
            cfg.command = entry;
            cfg.args = args;
        }
        PackageKind::Docker => {
            cfg.command = "docker".to_string();
            let mut full = vec![
                "run".to_string(),
                "-i".to_string(),
                "--rm".to_string(),
                pkg.spec.clone(),
            ];
            full.extend(args);
            cfg.args = full;
        }
        PackageKind::Binary | PackageKind::Script => {
            cfg.command = entry;
            cfg.args = args;
        }
    }
    cfg.env = build_spawn_env(&HashMap::new());
    cfg
}

// ── Install command derivation ────────────────────────────────────────────

/// Derive the explicit install command (if any) for a package.
///
/// `None` means "no separate install step" — the package manager resolves
/// the dependency lazily (npx/uvx caches), and the health check doubles as
/// the pre-warm.
pub fn derive_install_command(pkg: &McpPackageSpec) -> Option<Vec<String>> {
    match pkg.kind {
        PackageKind::Npm => None,
        PackageKind::Pypi => match pkg.runner {
            Some(PypiRunner::Uvx) | None => None, // uvx resolves on first run
            Some(PypiRunner::Pipx) => Some(vec!["pipx".to_string(), "install".to_string(), pkg.spec.clone()]),
            Some(PypiRunner::Pip) => Some(vec!["pip".to_string(), "install".to_string(), pkg.spec.clone()]),
        },
        PackageKind::Cargo => Some(vec!["cargo".to_string(), "install".to_string(), pkg.spec.clone()]),
        PackageKind::Go => {
            let spec = if pkg.spec.contains('@') {
                pkg.spec.clone()
            } else {
                format!("{}@latest", pkg.spec)
            };
            Some(vec!["go".to_string(), "install".to_string(), spec])
        }
        PackageKind::Docker => Some(vec!["docker".to_string(), "pull".to_string(), pkg.spec.clone()]),
        PackageKind::Binary => None, // resolved by entry-point presence check
        PackageKind::Script => None, // handled by gateway via install_script
    }
}
// ── Execution ─────────────────────────────────────────────────────────────

/// Output of an install run, mirrored to the frontend dialog.
#[derive(Debug, Clone, Default)]
pub struct InstallOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

/// Ensure runtime dependencies are present; returns structured guidance
/// instead of auto-installing runtimes (ADR-072 decision 2).
pub fn ensure_runtime(pkg: &McpPackageSpec) -> Result<(), DependencyStatus> {
    match probe_runtime(pkg) {
        DependencyStatus::Ready => Ok(()),
        missing @ DependencyStatus::Missing { .. } => Err(missing),
    }
}

/// Run the derived install command with idle-timeout monitoring.
pub async fn run_install(pkg: &McpPackageSpec) -> Result<InstallOutput, String> {
    run_install_with_output(pkg, &mut |_, _| {}).await
}

/// Run the install command and forward each captured output line to the caller
/// as it is produced, so a caller that surfaces progress can show what the
/// package manager is doing while it runs.
pub async fn run_install_with_output<F>(
    pkg: &McpPackageSpec,
    on_output: &mut F,
) -> Result<InstallOutput, String>
where
    F: FnMut(OutputStream, &str),
{
    let Some(cmd) = derive_install_command(pkg) else {
        // No explicit install step (npx/uvx lazy resolve; binary by presence).
        return Ok(InstallOutput {
            success: true,
            exit_code: Some(0),
            ..Default::default()
        });
    };

    let start = std::time::Instant::now();
    let mut command = tokio::process::Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    command.envs(build_spawn_env(&HashMap::new()));

    let output =
        run_command_with_idle_timeout_with_output(&mut command, INSTALL_IDLE_TIMEOUT, on_output)
            .await
            .map_err(|e| {
            format!(
                "install command `{}` failed: {}",
                cmd.join(" "),
                e.stderr.trim()
            )
        })?;

    Ok(InstallOutput {
        success: output.exit_code == Some(0),
        exit_code: output.exit_code,
        stdout: output.stdout,
        stderr: output.stderr,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

// ── Health check ──────────────────────────────────────────────────────────

/// Perform the MCP initialize + tools/list handshake against a derived spawn
/// config. This is the single health check for all kinds (ADR-072 decision 4).
pub async fn health_check(spawn: &McpServerConfigDef) -> Result<usize, String> {
    health_check_with_output(spawn, None, Some(handshake_bound_for(&spawn.command))).await
}

/// Handshake bound for a **bounded** connect, picked by the command's kind.
///
/// A lazily resolved package launches through a package manager, so its
/// handshake includes resolving and importing the package — `uvx docling-mcp`
/// measured 11.4s warm here, and cold it can exceed the protocol default
/// silently. A plain local binary answers in milliseconds, so it keeps the
/// tight default and a hang still surfaces in 30s.
///
/// The probe does NOT use this: it passes `None` and judges silence instead
/// (`constants::MCP_PROBE_IDLE_LAZY`, ADR-090 §5.4).
pub fn handshake_bound_for(command: &str) -> std::time::Duration {
    if resolves_lazily(command) {
        acowork_core::timeout_config::constants::MCP_LAZY_START
    } else {
        acowork_core::timeout_config::constants::MCP_RECV
    }
}

/// Package resolvers that download on first spawn, i.e. whose handshake window
/// must not be the protocol default (`constants::MCP_RECV`).
///
/// The classification is about the *command*, not the package spec: a custom
/// server may point straight at `npx` with no install spec at all (ADR-090
/// §5.4), and it downloads exactly the same way.
pub fn resolves_lazily(command: &str) -> bool {
    let program = std::path::Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    matches!(program.as_str(), "npx" | "pnpx" | "bunx" | "uvx")
}

/// Same handshake, forwarding the server process's stderr to `sink`, with the
/// handshake bound chosen by the caller (`None` = unbounded).
///
/// For lazily resolved packages this handshake *is* the install: `npx -y pkg`
/// downloads while the client waits, so this is the only way a caller can show
/// what a long first launch is doing — and the reason the bound is a parameter
/// rather than a constant.
pub async fn health_check_with_output(
    spawn: &McpServerConfigDef,
    sink: Option<crate::transport::ServerOutputSink>,
    recv_timeout: Option<Duration>,
) -> Result<usize, String> {
    let client = crate::client::McpClient::connect_with_timeout(spawn.clone(), sink, recv_timeout)
        .await
        .map_err(|e| format!("{:#}", e))?;
    let tools = client.tools();
    let count = tools.len();
    client.disconnect().await;
    Ok(count)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_resolvers_are_classified_by_program_name() {
        // The `npx`/`uvx` cases are what the longer probe silence budget exists
        // for: the first spawn downloads, and its output is the only progress.
        assert!(resolves_lazily("npx"));
        assert!(resolves_lazily("npx.cmd"));
        assert!(resolves_lazily(r"C:\Program Files\nodejs\npx.cmd"));
        assert!(resolves_lazily("uvx"));
        assert!(resolves_lazily("bunx"));
        assert!(resolves_lazily("pnpx"));
        // A real binary answers in milliseconds — it keeps the 30s window.
        assert!(!resolves_lazily("docling-mcp-server"));
        assert!(!resolves_lazily("uv"));
        assert!(!resolves_lazily("pnpm"));
        assert!(!resolves_lazily("pnpm.exe"));
    }

    #[test]
    fn handshake_bound_follows_the_command_kind() {
        use acowork_core::timeout_config::constants::{MCP_LAZY_START, MCP_RECV};
        // A launch through a package manager includes resolving and importing
        // the package: `uvx docling-mcp` measured 11.4s warm here, and cold it
        // can exceed the protocol default without printing anything.
        assert_eq!(handshake_bound_for("uvx"), MCP_LAZY_START);
        assert_eq!(handshake_bound_for(r"C:\tools\npx.cmd"), MCP_LAZY_START);
        // A plain binary answers in milliseconds; its hang must surface fast.
        assert_eq!(handshake_bound_for("docling-mcp-server"), MCP_RECV);
        assert_eq!(handshake_bound_for("python"), MCP_RECV);
    }

    /// Build a Vec<String> from string literals for assert_eq.
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn pkg(kind: PackageKind, spec: &str) -> McpPackageSpec {
        McpPackageSpec {
            kind,
            spec: spec.to_string(),
            runner: None,
            entry_point: None,
            spawn_args: vec![],
            exec_override: None,
            install_script: None,
            http_probe_ports: vec![],
        }
    }

    // ── spawn derivation matrix ─────────────────────────────────────────

    #[test]
    fn spawn_npm() {
        let p = pkg(PackageKind::Npm, "@playwright/mcp@latest");
        let c = derive_spawn_config("playwright", &p);
        assert_eq!(c.command, "npx");
        assert_eq!(c.args, s(&["-y", "@playwright/mcp@latest"]));
    }

    #[test]
    fn spawn_pypi_uvx_uses_entry_point_and_args() {
        let p = McpPackageSpec {
            kind: PackageKind::Pypi,
            spec: "docling-mcp".into(),
            runner: Some(PypiRunner::Uvx),
            entry_point: Some("docling-mcp-server".into()),
            spawn_args: s(&["--transport", "stdio"]),
            ..pkg(PackageKind::Pypi, "docling-mcp")
        };
        let c = derive_spawn_config("docling", &p);
        assert_eq!(c.command, "uvx");
        assert_eq!(
            c.args,
            s(&["--from", "docling-mcp", "docling-mcp-server", "--transport", "stdio"])
        );
    }

    #[test]
    fn spawn_pypi_pipx_command_is_entry_point() {
        let p = McpPackageSpec {
            kind: PackageKind::Pypi,
            spec: "some-mcp".into(),
            runner: Some(PypiRunner::Pipx),
            entry_point: Some("some-mcp".into()),
            ..pkg(PackageKind::Pypi, "some-mcp")
        };
        let c = derive_spawn_config("some", &p);
        assert_eq!(c.command, "some-mcp");
        assert!(c.args.is_empty());
    }

    #[test]
    fn spawn_pypi_pip_uses_python_module() {
        let p = McpPackageSpec {
            kind: PackageKind::Pypi,
            spec: "my-mcp".into(),
            runner: Some(PypiRunner::Pip),
            entry_point: Some("my_mcp".into()),
            ..pkg(PackageKind::Pypi, "my-mcp")
        };
        let c = derive_spawn_config("my", &p);
        assert_eq!(c.command, "python");
        assert_eq!(c.args, s(&["-m", "my_mcp"]));
    }

    #[test]
    fn spawn_docker() {
        let p = pkg(PackageKind::Docker, "ghcr.io/foo/bar-mcp:latest");
        let c = derive_spawn_config("bar", &p);
        assert_eq!(c.command, "docker");
        assert_eq!(c.args, s(&["run", "-i", "--rm", "ghcr.io/foo/bar-mcp:latest"]));
    }

    #[test]
    fn spawn_exec_override_takes_precedence() {
        let p = McpPackageSpec {
            kind: PackageKind::Npm,
            spec: "@modelcontextprotocol/server-filesystem".into(),
            exec_override: Some(ExecOverride {
                command: "npx".into(),
                args: s(&["-y", "@modelcontextprotocol/server-filesystem", "C:\\work"]),
            }),
            ..pkg(PackageKind::Npm, "@modelcontextprotocol/server-filesystem")
        };
        let c = derive_spawn_config("filesystem", &p);
        assert_eq!(c.command, "npx");
        assert_eq!(
            c.args,
            s(&["-y", "@modelcontextprotocol/server-filesystem", "C:\\work"])
        );
    }

    // ── install command derivation ──────────────────────────────────────

    #[test]
    fn install_none_for_npm_and_uvx() {
        assert!(derive_install_command(&pkg(PackageKind::Npm, "x")).is_none());
        let p = McpPackageSpec {
            kind: PackageKind::Pypi,
            spec: "docling-mcp".into(),
            runner: Some(PypiRunner::Uvx),
            ..pkg(PackageKind::Pypi, "docling-mcp")
        };
        assert!(derive_install_command(&p).is_none());
    }

    #[test]
    fn install_pipx_and_pip() {
        let p = McpPackageSpec {
            kind: PackageKind::Pypi,
            spec: "docling-mcp".into(),
            runner: Some(PypiRunner::Pipx),
            ..pkg(PackageKind::Pypi, "docling-mcp")
        };
        assert_eq!(
            derive_install_command(&p),
            Some(s(&["pipx", "install", "docling-mcp"]))
        );
        let p2 = McpPackageSpec {
            kind: PackageKind::Pypi,
            spec: "docling-mcp".into(),
            runner: Some(PypiRunner::Pip),
            ..pkg(PackageKind::Pypi, "docling-mcp")
        };
        assert_eq!(
            derive_install_command(&p2),
            Some(s(&["pip", "install", "docling-mcp"]))
        );
    }

    #[test]
    fn install_go_appends_latest() {
        let p = pkg(PackageKind::Go, "golang.org/x/tools/gopls");
        assert_eq!(
            derive_install_command(&p),
            Some(s(&["go", "install", "golang.org/x/tools/gopls@latest"]))
        );
        let pinned = pkg(PackageKind::Go, "golang.org/x/tools/gopls@v0.16.2");
        assert_eq!(
            derive_install_command(&pinned),
            Some(s(&["go", "install", "golang.org/x/tools/gopls@v0.16.2"]))
        );
    }

    #[test]
    fn install_cargo_docker() {
        assert_eq!(
            derive_install_command(&pkg(PackageKind::Cargo, "mcp-server")),
            Some(s(&["cargo", "install", "mcp-server"]))
        );
        assert_eq!(
            derive_install_command(&pkg(PackageKind::Docker, "img:latest")),
            Some(s(&["docker", "pull", "img:latest"]))
        );
    }

    // ── runtime probing (no real spawns in tests) ───────────────────────

    #[test]
    fn probe_ready_for_script_and_binary() {
        assert_eq!(
            probe_runtime(&pkg(PackageKind::Script, "x")),
            DependencyStatus::Ready
        );
        assert_eq!(
            probe_runtime(&pkg(PackageKind::Binary, "x")),
            DependencyStatus::Ready
        );
    }

    #[test]
    fn probe_pypi_pip_checks_python_and_pip() {
        let p = McpPackageSpec {
            kind: PackageKind::Pypi,
            spec: "x".into(),
            runner: Some(PypiRunner::Pip),
            ..pkg(PackageKind::Pypi, "x")
        };
        match probe_runtime(&p) {
            DependencyStatus::Ready => {}
            DependencyStatus::Missing { runtime, .. } => {
                assert!(runtime == "python" || runtime == "pip");
            }
        }
    }
}
