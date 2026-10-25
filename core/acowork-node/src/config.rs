//! Node Agent configuration (ADR-055 §6.12 directory layout).

use std::path::{Path, PathBuf};

use crate::error::{NodeError, Result};

/// Node Agent runtime configuration.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Node data directory — `$HOME/.acowork/acowork-node/` by default
    /// (overridable via `--home` / `ACOWORK_NODE_HOME`).
    pub home: PathBuf,
    /// Agent package install directory. `None` defaults to
    /// `{home}/packages` (§6.12). Overridable via `--packages-dir` /
    /// `ACOWORK_NODE_PACKAGES_DIR` — the Gateway sets this when
    /// spawning the local node so its package inventory stays on the
    /// Gateway's own `packages_dir` (ADR-055 Phase 2b.3 hard-cut
    /// compatibility).
    pub packages_dir: Option<PathBuf>,
    /// Gateway MQTT broker host to connect to.
    pub gateway_host: String,
    /// Gateway MQTT broker port to connect to.
    pub gateway_mqtt_port: u16,
    /// Gateway HTTP base URL, explicit (`--gateway-http` /
    /// `ACOWORK_GATEWAY_HTTP`). `None` means derive it — see
    /// [`Self::gateway_http_base`]. Two consumers need it: the ADR-087
    /// first-boot credential exchange and the ADR-101 memory migration pass,
    /// both of which talk to the Gateway over HTTP while the control plane
    /// runs on MQTT.
    pub gateway_http: Option<String>,
    /// Explicit node name (`--name`). When `None`, a new identity is
    /// derived from the hostname. Display only (ADR-075 D2).
    pub name: Option<String>,
    /// Spawn marker set ONLY by the Gateway (`--gateway-managed`,
    /// hidden from help). Persisted into identity.json at creation so
    /// the Gateway can recognize its own-machine node even after a
    /// service/container restart drops the flag (ADR-075 D5).
    pub gateway_managed: bool,
    /// Enrollment token (Phase 5a — carried through the config so the
    /// CLI surface is already in place; the broker does not validate
    /// it yet).
    pub token: Option<String>,
    /// Maximum concurrent Runtime processes (§6.18).
    pub max_agents: u32,
    /// ADR-055 §6.3: the address other machines use to reach this
    /// node's reverse proxy. Defaults to `127.0.0.1` (single-machine
    /// topology); set to a non-loopback IP for remote deployments.
    pub advertise_host: String,
    /// When `true` (CLI `--advertise-host auto`), `advertise_host` is
    /// treated as a *placeholder*: the Node Agent re-detects the
    /// machine's current LAN IP at connect time (`detect_local_ip`)
    /// and publishes it live via NodeInfo (§6.3.3 self-healing). If
    /// detection fails it falls back to `advertise_host` (127.0.0.1),
    /// keeping control-plane loopback intact.
    pub advertise_host_auto: bool,
    /// Bind address for the node reverse proxy (§6.4). Defaults to
    /// `0.0.0.0` so remote nodes are reachable without reconfiguring
    /// the bind; the advertise_host remains the reachable address.
    pub proxy_bind: String,
    /// TCP port for the node reverse proxy (`/agents/{id}/*`).
    pub proxy_port: u16,
    /// TCP port for the node-local LSP relay sidecar (ADR-055 §6.7,
    /// Phase 4). The relay listens on `127.0.0.1:{port}` and its
    /// endpoint is advertised via the retained per-node `lsps` topic.
    pub lsp_relay_port: u16,
    /// ADR-089 D1: escape hatch for the relay bind address. `false`
    /// (default) binds the relay to `127.0.0.1` — the only consumer,
    /// the Desktop, reaches it through the Gateway reverse proxy
    /// (`/api/nodes/{id}/lsp/*` → node proxy `/relay/*`), so the relay
    /// never needs a LAN listener. `true` (CLI `--expose-lsp-relay`)
    /// binds the relay to `advertise_host` for operators who
    /// deliberately accept the unauthenticated LAN surface.
    pub expose_lsp_relay: bool,
    /// Runtime log file max size in MB before auto-split (forwarded to
    /// spawned Runtimes, mirrors the Gateway default).
    pub log_file_size_mb: u64,
    /// Maximum Runtime log files to keep (0 = unlimited).
    pub log_file_count: u64,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            home: acowork_core::node::default_node_home(),
            packages_dir: None,
            gateway_host: acowork_core::defaults::GATEWAY_MQTT_HOST.to_string(),
            gateway_mqtt_port: acowork_core::defaults::GATEWAY_MQTT_PORT,
            gateway_http: None,
            name: None,
            gateway_managed: false,
            token: None,
            max_agents: acowork_core::node::NODE_DEFAULT_MAX_AGENTS,
            advertise_host: "127.0.0.1".to_string(),
            advertise_host_auto: false,
            proxy_bind: "0.0.0.0".to_string(),
            proxy_port: acowork_core::node::NODE_PROXY_PORT,
            lsp_relay_port: crate::sidecar::lsp_relay::LSP_RELAY_DEFAULT_PORT,
            expose_lsp_relay: false,
            log_file_size_mb: 10,
            log_file_count: 20,
        }
    }
}

impl NodeConfig {
    /// Gateway address as `host:port` (the MQTT broker endpoint).
    pub fn gateway_addr(&self) -> String {
        format!("{}:{}", self.gateway_host, self.gateway_mqtt_port)
    }

    /// Gateway HTTP base URL (no trailing slash).
    ///
    /// The shipped pairing is HTTP = MQTT port + 1 (19875 / 19876), which is
    /// what a single-machine deployment gets without configuration; an
    /// operator who moved the HTTP listener passes `--gateway-http`.
    pub fn gateway_http_base(&self) -> String {
        self.gateway_http.clone().unwrap_or_else(|| {
            format!(
                "http://{}:{}",
                self.gateway_host,
                self.gateway_mqtt_port + 1
            )
        })
    }

    /// ADR-055 §6.3: the base URL of this node's reverse proxy as it
    /// should be advertised to Runtimes (`http://{advertise_host}:{port}`).
    /// The Runtime appends `/agents/{id}` when publishing its retained
    /// `http_endpoint`.
    pub fn proxy_advertise_endpoint(&self) -> String {
        format!("http://{}:{}", self.advertise_host, self.proxy_port)
    }

    /// Like [`Self::proxy_advertise_endpoint`], but for a caller-supplied
    /// host. Used by the self-healing path (§6.3.3): the live LAN IP is
    /// recomputed at connect time and re-published as `http://{host}:{port}`
    /// so already-running Runtimes and the Gateway both converge on the
    /// current address without a restart.
    pub fn proxy_advertise_endpoint_for(&self, host: &str) -> String {
        format!("http://{}:{}", host, self.proxy_port)
    }

    /// Resolve the agent package install directory: the explicit
    /// `--packages-dir` override, or `{home}/packages` (§6.12).
    pub fn packages_dir(&self) -> PathBuf {
        self.packages_dir
            .clone()
            .unwrap_or_else(|| self.home.join("packages"))
    }

    /// Ensure the home directory tree exists (§6.12):
    /// `identity.json`, `logs/`, `packages/`, `runtime-logs/`.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in ["logs", "runtime-logs"] {
            let dir = self.home.join(dir);
            std::fs::create_dir_all(&dir).map_err(|e| {
                NodeError::Config(format!(
                    "Failed to create node directory '{}': {}",
                    dir.display(),
                    e
                ))
            })?;
        }
        let packages = self.packages_dir();
        std::fs::create_dir_all(&packages).map_err(|e| {
            NodeError::Config(format!(
                "Failed to create node packages directory '{}': {}",
                packages.display(),
                e
            ))
        })?;
        Ok(())
    }
}

/// Resolve the node home from an optional `--home` override.
///
/// The default value is sourced from [`acowork_core::node::default_node_home`]
/// — shared with `acowork-gateway` so both sides derive a single
/// Node home directory without `acowork-gateway` depending on
/// `acowork-node` (ADR-055 §6.20 dependency red line).
pub fn resolve_home(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(acowork_core::node::default_node_home)
}

/// Best-effort system hostname — shared implementation lives in
/// `acowork-core` (used identically by the Gateway to compute the
/// local node id, so both sides always agree).
pub fn system_hostname() -> String {
    acowork_core::node::system_hostname()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_addr_formats_host_port() {
        let cfg = NodeConfig {
            gateway_host: "192.168.1.10".to_string(),
            gateway_mqtt_port: 19875,
            ..NodeConfig::default()
        };
        assert_eq!(cfg.gateway_addr(), "192.168.1.10:19875");
    }

    /// The HTTP base is derived from the MQTT endpoint unless it was given,
    /// so a Node started with only `--gateway` still reaches the Gateway API
    /// (ADR-087 enrollment, ADR-101 memory migration).
    #[test]
    fn gateway_http_base_is_derived_unless_configured() {
        let derived = NodeConfig {
            gateway_host: "192.168.1.10".to_string(),
            gateway_mqtt_port: 19875,
            ..NodeConfig::default()
        };
        assert_eq!(derived.gateway_http_base(), "http://192.168.1.10:19876");

        let explicit = NodeConfig {
            gateway_http: Some("https://gw.internal:8443".to_string()),
            ..derived
        };
        assert_eq!(explicit.gateway_http_base(), "https://gw.internal:8443");
    }

    #[test]
    fn ensure_dirs_creates_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = NodeConfig {
            home: tmp.path().join("node-home"),
            ..NodeConfig::default()
        };
        cfg.ensure_dirs().unwrap();
        assert!(cfg.home.join("logs").is_dir());
        assert!(cfg.home.join("packages").is_dir());
        assert!(cfg.home.join("runtime-logs").is_dir());
    }

    #[test]
    fn system_hostname_is_non_empty() {
        assert!(!system_hostname().is_empty());
    }
}

