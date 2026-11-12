//! Node Agent OS service integration (ADR-055 §6.13.2).
//!
//! `acowork-node service install|uninstall` makes the node a resident
//! daemon managed by the platform's service supervisor, so a machine
//! reboot brings the node back without a manual `start`:
//!
//! - **macOS** — a launchd LaunchAgent plist under
//!   `~/Library/LaunchAgents/com.acowork.node.plist` (`RunAtLoad` +
//!   `KeepAlive`).
//! - **Linux** — a systemd **user** unit under
//!   `~/.config/systemd/user/acowork-node.service` (`Restart=always`).
//!   A user unit keeps the node scoped to the owning user (no root),
//!   matching the `$HOME/.acowork/acowork-node` data layout.
//! - **Windows** — documented guidance only (`sc create` / NSSM); no
//!   unit file is written (ADR-055 §6.13.2).
//!
//! The generated unit pins the node's runtime identity: it always runs
//! `acowork-node start` with the explicit `--name`, `--home`,
//! `--gateway` captured from the persisted
//! `identity.json`, so the service re-connects to the same Gateway
//! under the same node_id across reboots.

use std::path::PathBuf;

#[cfg(not(windows))]
use std::path::Path;

use crate::config::NodeConfig;
use crate::error::{NodeError, Result};
use crate::identity::NodeIdentity;

/// The launchd label, fixed for the node agent (macOS-only: the systemd
/// unit hard-codes its `Description=` instead of deriving from this).
#[cfg(target_os = "macos")]
const SERVICE_LABEL: &str = "com.acowork.node";

/// Split a persisted `gateway_addr` (`host:port`) into its parts,
/// falling back to the configured defaults when the identity has never
/// recorded a Gateway.
fn gateway_parts(identity: &Option<NodeIdentity>, config: &NodeConfig) -> (String, u16) {
    match identity.as_ref().and_then(|i| i.gateway_addr.as_deref()) {
        Some(addr) => match addr.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port.parse().unwrap_or(config.gateway_mqtt_port)),
            None => (addr.to_string(), config.gateway_mqtt_port),
        },
        None => (config.gateway_host.clone(), config.gateway_mqtt_port),
    }
}

/// Escape a string for safe embedding in a launchd plist XML value.
#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Escape a string for safe embedding in a systemd unit (ExecStart
/// arguments use the same quoting rules as a shell word).
#[cfg(all(unix, not(target_os = "macos")))]
fn systemd_escape(s: &str) -> String {
    // Keep it conservative: single-quote and escape embedded quotes.
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Where the supervisor unit file lives on this platform.
#[cfg(not(windows))]
fn unit_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(
            dirs_home()?
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{SERVICE_LABEL}.plist")),
        )
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Some(
            dirs_home()?
                .join(".config")
                .join("systemd")
                .join("user")
                .join("acowork-node.service"),
        )
    }
}

/// The user home directory (`$HOME` / `%USERPROFILE%`).
#[cfg(not(windows))]
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The node binary path (the running executable).
fn node_binary() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("acowork-node"))
}

/// Render the launchd plist body for the current identity.
#[cfg(target_os = "macos")]
fn render_launchd_plist(
    bin: &Path,
    host: &str,
    port: u16,
    node_id: &str,
    home: &Path,
) -> String {
    let log_stdout = xml_escape(&home.join("logs").join("service.stdout.log").to_string_lossy());
    let log_stderr = xml_escape(&home.join("logs").join("service.stderr.log").to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>start</string>
        <string>--gateway</string>
        <string>{host}:{port}</string>
        <string>--name</string>
        <string>{node_id}</string>
        <string>--home</string>
        <string>{home}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_stdout}</string>
    <key>StandardErrorPath</key>
    <string>{log_stderr}</string>
</dict>
</plist>
"#,
        label = SERVICE_LABEL,
        bin = xml_escape(&bin.to_string_lossy()),
        host = xml_escape(host),
        port = port,
        node_id = xml_escape(node_id),
        home = xml_escape(&home.to_string_lossy()),
        log_stdout = log_stdout,
        log_stderr = log_stderr,
    )
}

/// Render the systemd user unit body for the current identity.
#[cfg(all(unix, not(target_os = "macos")))]
fn render_systemd_unit(
    bin: &Path,
    host: &str,
    port: u16,
    node_id: &str,
    home: &Path,
) -> String {
    let exec_start = format!(
        "{} start --gateway {host}:{port} --name {} --home {}",
        systemd_escape(&bin.to_string_lossy()),
        systemd_escape(node_id),
        systemd_escape(&home.to_string_lossy()),
    );
    format!(
        r#"[Unit]
Description=ACowork Node Agent (ADR-055)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exec_start}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"#
    )
}

/// `acowork-node service install` — write the supervisor unit for the
/// current identity and print the enable/start commands.
pub fn install_service(config: &NodeConfig) -> Result<()> {
    let identity = NodeIdentity::load(&config.home)?;
    let node_id = identity
        .as_ref()
        .map(|i| i.node_id.clone())
        .ok_or_else(|| {
            NodeError::Identity("No identity.json — run `acowork-node start` first".to_string())
        })?;
    let (host, port) = gateway_parts(&identity, config);
    let bin = node_binary();

    #[cfg(target_os = "macos")]
    {
        let Some(path) = unit_path() else {
            return Err(NodeError::Config("Cannot resolve the home directory".to_string()));
        };
        let body = render_launchd_plist(&bin, &host, port, &node_id, &config.home);
        write_unit(&path, &body)?;
        println!("Installed launchd agent: {}", path.display());
        println!(
            "Enable & start with:\n  launchctl load {}\n  launchctl start {SERVICE_LABEL}",
            path.display()
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let Some(path) = unit_path() else {
            return Err(NodeError::Config("Cannot resolve the home directory".to_string()));
        };
        let body = render_systemd_unit(&bin, &host, port, &node_id, &config.home);
        write_unit(&path, &body)?;
        println!("Installed systemd user unit: {}", path.display());
        println!(
            "Enable & start with:\n  systemctl --user daemon-reload\n  systemctl --user enable --now acowork-node.service"
        );
    }

    #[cfg(windows)]
    {
        let _ = (&bin, &host, port, &node_id);
        // ADR-055 §6.13.2: Windows service installation is documented
        // guidance only (sc / NSSM), no unit file is written.
        println!(
            "Windows: install the node as a service manually, e.g.\n  \
             nssm install acowork-node \"{}\" start --gateway {host}:{port} --name {node_id} --home \"{}\"\n\
             or\n  \
             sc create acowork-node binPath= \"\\\"{}\\\" start --gateway {host}:{port} --name {node_id} --home \\\"{}\\\"\" start= auto",
            bin.display(),
            config.home.display(),
            bin.display(),
            config.home.display(),
        );
    }

    Ok(())
}

/// `acowork-node service uninstall` — remove the supervisor unit and
/// print the stop/disable commands.
pub fn uninstall_service(config: &NodeConfig) -> Result<()> {
    let _ = config;

    #[cfg(not(windows))]
    {
        let Some(path) = unit_path() else {
            return Err(NodeError::Config("Cannot resolve the home directory".to_string()));
        };
        if !path.exists() {
            println!("No installed service unit found at {}", path.display());
            return Ok(());
        }
        std::fs::remove_file(&path)
            .map_err(|e| NodeError::Config(format!("Failed to remove '{}': {}", path.display(), e)))?;
        println!("Removed service unit: {}", path.display());

        #[cfg(target_os = "macos")]
        println!("Stop first with:\n  launchctl unload {}", path.display());
        #[cfg(all(unix, not(target_os = "macos")))]
        println!("Stop first with:\n  systemctl --user disable --now acowork-node.service");
    }

    #[cfg(windows)]
    {
        println!("Windows: remove the service manually, e.g.\n  sc delete acowork-node\n  nssm remove acowork-node confirm");
    }

    Ok(())
}

/// Write a unit file, creating its parent directory first.
#[cfg(not(windows))]
fn write_unit(path: &Path, body: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| NodeError::Config(format!("Invalid unit path '{}'", path.display())))?;
    std::fs::create_dir_all(parent).map_err(|e| {
        NodeError::Config(format!("Failed to create '{}': {}", parent.display(), e))
    })?;
    std::fs::write(path, body)
        .map_err(|e| NodeError::Config(format!("Failed to write '{}': {}", path.display(), e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_parts_parses_host_port() {
        let config = NodeConfig::default();
        let mut identity = NodeIdentity::load_or_create(
            tempfile::tempdir().unwrap().path(),
            Some("gpu-server"),
            Some("192.168.1.10:19876"),
        )
        .unwrap();
        identity.gateway_addr = Some("192.168.1.10:19876".to_string());
        let (host, port) = gateway_parts(&Some(identity), &config);
        assert_eq!(host, "192.168.1.10");
        assert_eq!(port, 19876);
    }

    #[test]
    fn gateway_parts_falls_back_to_config() {
        let config = NodeConfig {
            gateway_host: "10.0.0.9".to_string(),
            gateway_mqtt_port: 29875,
            ..NodeConfig::default()
        };
        let (host, port) = gateway_parts(&None, &config);
        assert_eq!(host, "10.0.0.9");
        assert_eq!(port, 29875);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn xml_escape_handles_specials() {
        assert_eq!(xml_escape("a&b<c>\"d'e"), "a&amp;b&lt;c&gt;&quot;d&apos;e");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_contains_identity() {
        let home = tempfile::tempdir().unwrap();
        let body = render_launchd_plist(
            Path::new("/usr/local/bin/acowork-node"),
            "192.168.1.10",
            19875,
            "gpu-server",
            home.path(),
        );
        assert!(body.contains("<string>gpu-server</string>"));
        assert!(body.contains("<string>192.168.1.10:19875</string>"));
        assert!(body.contains("<key>KeepAlive</key>"));
        assert!(body.contains("<key>RunAtLoad</key>"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn systemd_unit_contains_identity() {
        let home = tempfile::tempdir().unwrap();
        let body = render_systemd_unit(
            Path::new("/usr/local/bin/acowork-node"),
            "192.168.1.10",
            19875,
            "gpu-server",
            home.path(),
        );
        assert!(body.contains("Restart=always"));
        assert!(body.contains("gpu-server"));
        assert!(body.contains("--gateway"));
        assert!(body.contains("192.168.1.10:19875"));
    }
}
