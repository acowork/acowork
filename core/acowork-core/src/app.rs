//! App framework declaration surface (ADR-116).
//!
//! An *app* is a standalone application hosted by ACowork: its own process,
//! its own loopback HTTP service, its own front-end bundle and its own data
//! directory. The platform owns registration, process supervision, reverse
//! proxying, identity injection, MCP catalog publication and the navigation
//! slot. It does NOT own the app's UI, its data, or its business logic.
//!
//! This module is the shared wire contract: it lives in `acowork-core` so the
//! Gateway (registry owner) and any future app SDK agree on one shape without
//! depending on each other. Deliberately NOT a re-use of [`crate::manifest`]'s
//! `AgentManifest` — that type encodes agent semantics (`triggers`, `llm`,
//! `memory`, `identity_deps`) that have no meaning for an application.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Directory under the Gateway data dir holding user-installed app manifests.
pub const APPS_DIR_NAME: &str = "apps";

/// Persisted "disabled" set. Absence means every registered app is enabled.
pub const DISABLED_STATE_FILE: &str = "apps_disabled.json";

/// Built-in PM application declaration (ADR-064 process, ADR-116 app).
pub const PM_APP_TOML: &str = r#"
id = "pm"
version = "1.0.0"
name = "项目管理"
description = "Project and task management for humans and agents"
icon = "kanban"
builtin = true

[process]
runtime = "legacy"
legacy_slot = "pm"
binary = "acowork-pm"
port_hint = 18082
port_file = "pm.port"
health_path = "/health"

[mcp]
path = "/mcp"
protocol = "streamable-http"
auto_inject = true
gateway_path = "/api/pm/mcp"

[ui]
path = "/ui/"
mode = "webview"

[ui.nav]
label = "项目"
icon = "kanban"
order = 10

[activation]
events = ["onStartup", "onNavigate"]

[permissions]
platform = ["identity.read", "context.read", "agents.read", "notify"]
"#;

/// Built-in DOC application declaration (ADR-070 process, ADR-116 app).
pub const DOC_APP_TOML: &str = r#"
id = "doc"
version = "1.0.0"
name = "文档库"
description = "Online document library with review workflow"
icon = "file-text"
builtin = true

[process]
runtime = "legacy"
legacy_slot = "doc"
binary = "acowork-doc"
port_hint = 18081
port_file = "doc.port"
health_path = "/health"

[mcp]
path = "/mcp"
protocol = "streamable-http"
auto_inject = true
gateway_path = "/api/doc/mcp"

[ui]
path = "/ui/"
mode = "webview"

[ui.nav]
label = "文档"
icon = "file-text"
order = 20

[activation]
events = ["onStartup", "onNavigate"]

[permissions]
platform = ["identity.read", "context.read", "agents.read", "notify"]
"#;

/// How the Gateway starts and supervises an app process.
///
/// `Legacy` keeps the ADR-064 per-service supervisors (PM/DOC have bespoke
/// spawn arguments and a large identity-injection test suite). `Generic`
/// uses the framework's configuration-driven supervisor for apps installed
/// after ADR-116. Both project their runtime state into the same table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppRuntime {
    #[default]
    Legacy,
    Generic,
}

/// How the Desktop hosts the app's front end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppUiMode {
    /// The app serves its own bundle; the Desktop loads it in a sandboxed
    /// iframe through the Gateway reverse proxy.
    #[default]
    Webview,
    /// The Desktop renders a built-in React view. Migration-only: a native
    /// app still declares the rest of the contract (process/MCP/registry).
    Native,
}

/// Process declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppProcessSpec {
    /// Executable name only — never a path. Resolved next to the Gateway
    /// binary by the supervisor, which prevents an app manifest from
    /// launching an arbitrary filesystem location.
    pub binary: String,
    pub port_hint: u16,
    #[serde(default)]
    pub runtime: AppRuntime,
    /// For `Legacy`: which `GatewayState` slot holds the supervised process.
    #[serde(default)]
    pub legacy_slot: Option<String>,
    /// Filename (not path) under the Gateway data dir for the `--port-file`
    /// handshake.
    #[serde(default)]
    pub port_file: Option<String>,
    #[serde(default = "default_health_path")]
    pub health_path: String,
    /// Extra CLI arguments, with `{gateway_url}` / `{data_dir}` placeholders
    /// resolved by the generic supervisor at spawn time.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Extra environment variables. `{internal_service_token}` is resolved by
    /// the generic supervisor; credentials never travel in argv.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_health_path() -> String {
    "/health".to_string()
}

/// MCP declaration. The app implements `tools/list` and `tools/call`; the
/// platform only forwards JSON-RPC and publishes a catalog entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppMcpSpec {
    pub path: String,
    #[serde(default = "default_mcp_protocol")]
    pub protocol: String,
    #[serde(default = "default_true")]
    pub auto_inject: bool,
    /// Gateway-facing URL path used when publishing the catalog entry.
    /// Agents always connect through the Gateway, never to the app port.
    pub gateway_path: String,
}

fn default_mcp_protocol() -> String {
    "streamable-http".to_string()
}

fn default_true() -> bool {
    true
}

/// Front-end contribution. Exactly one navigation slot per app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppNavSpec {
    pub label: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppUiSpec {
    pub path: String,
    #[serde(default)]
    pub mode: AppUiMode,
    pub nav: AppNavSpec,
}

/// Lazy activation hints. Informational in the first release: built-in apps
/// start with the Gateway, while `onNavigate` / `onMcpCall` are reserved for
/// the generic supervisor's on-demand start.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppActivationSpec {
    #[serde(default)]
    pub events: Vec<String>,
}

/// Capabilities an app may call on the platform. Default-deny: an undeclared
/// `/api/platform/*` operation is rejected by the Gateway.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppPermissions {
    #[serde(default)]
    pub platform: Vec<String>,
}

/// Full declaration of one hosted application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppManifest {
    pub id: String,
    pub version: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub builtin: bool,
    pub process: AppProcessSpec,
    pub mcp: AppMcpSpec,
    pub ui: AppUiSpec,
    #[serde(default)]
    pub activation: AppActivationSpec,
    #[serde(default)]
    pub permissions: AppPermissions,
}

impl AppManifest {
    /// Parse and validate a TOML declaration.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let manifest: Self = toml::from_str(text).map_err(|e| format!("invalid app.toml: {e}"))?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }
}

/// Validate a declaration at the trust boundary.
///
/// The app id becomes a URL segment, a directory name and a cookie path, so
/// it is held to a strict charset. Paths must be absolute-within-service and
/// must not contain `..` — otherwise a crafted manifest could make the reverse
/// proxy address an unintended upstream path.
pub fn validate_manifest(m: &AppManifest) -> Result<(), String> {
    if !is_valid_app_id(&m.id) {
        return Err(format!("invalid app id {:?}: expected [a-z0-9][a-z0-9_-]{{0,63}}", m.id));
    }
    if m.version.trim().is_empty() {
        return Err("app version must not be empty".to_string());
    }
    if m.name.trim().is_empty() {
        return Err("app name must not be empty".to_string());
    }
    if m.process.binary.is_empty()
        || m.process.binary.contains('/')
        || m.process.binary.contains('\\')
        || m.process.binary.contains("..")
    {
        return Err(format!(
            "process.binary must be a plain executable name, got {:?}",
            m.process.binary
        ));
    }
    if m.process.port_hint < 1024 {
        return Err("process.port_hint must be >= 1024".to_string());
    }
    for (label, path) in [
        ("process.health_path", m.process.health_path.as_str()),
        ("mcp.path", m.mcp.path.as_str()),
        ("mcp.gateway_path", m.mcp.gateway_path.as_str()),
        ("ui.path", m.ui.path.as_str()),
    ] {
        if !path.starts_with('/') || path.contains("..") {
            return Err(format!("{label} must be an absolute in-service path: {path:?}"));
        }
    }
    if m.process.runtime == AppRuntime::Legacy && m.process.legacy_slot.is_none() {
        return Err("legacy apps must declare process.legacy_slot".to_string());
    }
    Ok(())
}

/// An app id is a URL segment, a data-dir name and a cookie path component.
pub fn is_valid_app_id(id: &str) -> bool {
    let Some(first) = id.as_bytes().first() else {
        return false;
    };
    // A leading `-` or `_` would make the id look like a CLI flag once it
    // reaches a supervisor, and a leading digit would collide with numeric
    // path parsing. Start with a letter, then allow the safe charset.
    first.is_ascii_lowercase()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// In-memory registry: built-in declarations + user-installed declarations,
/// plus the persisted enabled/disabled overlay.
#[derive(Debug, Clone)]
pub struct AppRegistry {
    apps: BTreeMap<String, AppManifest>,
    disabled: BTreeSet<String>,
}

impl AppRegistry {
    /// Built-in declarations only (used by tests and by `load` as the base).
    pub fn builtin() -> Self {
        let mut apps = BTreeMap::new();
        for text in [PM_APP_TOML, DOC_APP_TOML] {
            let manifest = AppManifest::from_toml(text)
                .expect("built-in app declarations must be valid at compile time");
            apps.insert(manifest.id.clone(), manifest);
        }
        Self {
            apps,
            disabled: BTreeSet::new(),
        }
    }

    /// Load built-ins, then overlay user-installed `{data_dir}/apps/*.toml`
    /// and the persisted disabled set. Invalid user manifests are skipped
    /// with a warning rather than taking the Gateway down.
    pub fn load(data_dir: &Path) -> Self {
        let mut registry = Self::builtin();
        let dir = data_dir.join(APPS_DIR_NAME);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut paths: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
                .collect();
            paths.sort();
            for path in paths {
                match std::fs::read_to_string(&path).and_then(|text| {
                    AppManifest::from_toml(&text).map_err(std::io::Error::other)
                }) {
                    Ok(manifest) => {
                        if manifest.builtin {
                            tracing::warn!(
                                path = %path.display(),
                                "user app manifest claims builtin=true; ignored"
                            );
                            continue;
                        }
                        registry.apps.insert(manifest.id.clone(), manifest);
                    }
                    Err(e) => tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping invalid app manifest"
                    ),
                }
            }
        }
        registry.disabled = Self::load_disabled(data_dir);
        registry
    }

    /// Build from explicit parts (tests / programmatic registration).
    pub fn from_parts(apps: BTreeMap<String, AppManifest>, disabled: BTreeSet<String>) -> Self {
        Self { apps, disabled }
    }

    pub fn load_disabled(data_dir: &Path) -> BTreeSet<String> {
        std::fs::read_to_string(data_dir.join(DISABLED_STATE_FILE))
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
            .map(|ids| ids.into_iter().filter(|id| is_valid_app_id(id)).collect())
            .unwrap_or_default()
    }

    pub fn save_disabled(data_dir: &Path, disabled: &BTreeSet<String>) -> Result<(), String> {
        let path = data_dir.join(DISABLED_STATE_FILE);
        let ids: Vec<&String> = disabled.iter().collect();
        let text = serde_json::to_string_pretty(&ids)
            .map_err(|e| format!("failed to serialize app state: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))
    }

    pub fn all(&self) -> impl Iterator<Item = &AppManifest> {
        self.apps.values()
    }

    pub fn get(&self, id: &str) -> Option<&AppManifest> {
        self.apps.get(id)
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.apps.contains_key(id) && !self.disabled.contains(id)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        if !self.apps.contains_key(id) {
            return false;
        }
        if enabled {
            self.disabled.remove(id);
        } else {
            self.disabled.insert(id.to_string());
        }
        true
    }

    pub fn disabled(&self) -> &BTreeSet<String> {
        &self.disabled
    }

    /// Enabled apps with MCP auto-injection, in stable id order.
    pub fn catalog_apps(&self) -> Vec<&AppManifest> {
        self.apps
            .values()
            .filter(|m| self.is_enabled(&m.id) && m.mcp.auto_inject)
            .collect()
    }
}

/// Reference app shell served by an app's own HTTP service.
///
/// This is a diagnostic/starting-point page, not a platform-provided UI kit:
/// the app owns its bundle. It exercises every ADR-116 channel — the ticket
/// passed in the URL fragment, the trusted identity injected by the Gateway,
/// the app's own REST API, the App → Platform API (proxied by the app's
/// backend), and the postMessage bridge to the Desktop. A real app replaces
/// this file wholesale; nothing in the framework references it.
pub fn app_shell_html(app_id: &str, app_name: &str, summary_path: &str) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    const TEMPLATE: &str = r###"<!doctype html>
<html lang="zh" data-theme="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__APP_NAME__</title>
<style>
:root { --accent: #3b82f6; --bg: #14161a; --card: #1c2028; --line: #303642;
        --fg: #e6e9ef; --muted: #9aa3b2; }
html[data-theme="light"] { --bg: #f6f7f9; --card: #ffffff; --line: #e2e6ec;
        --fg: #1c2028; --muted: #667085; }
* { box-sizing: border-box; }
body { margin: 0; padding: 24px; background: var(--bg); color: var(--fg);
       font: 13px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif; }
header { display: flex; align-items: center; gap: 10px; margin-bottom: 20px; }
.badge { font-size: 11px; padding: 2px 8px; border: 1px solid var(--line);
         border-radius: 999px; color: var(--muted); }
h1 { font-size: 15px; font-weight: 500; margin: 0; }
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 12px; }
.card { background: var(--card); border: 1px solid var(--line);
        border-radius: 10px; padding: 14px 16px; min-width: 0; }
.card h2 { font-size: 12px; font-weight: 500; color: var(--muted); margin: 0 0 10px; }
pre { margin: 0; white-space: pre-wrap; word-break: break-all; font-size: 12px; }
.ok { color: #4ade80; } .err { color: #f87171; }
.actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
button { border: 1px solid var(--line); background: transparent; color: var(--fg);
         border-radius: 6px; padding: 5px 12px; cursor: pointer; font-size: 12px; }
button.primary { background: var(--accent); border-color: var(--accent); color: #fff; }
.note { margin-top: 16px; color: var(--muted); font-size: 12px; }
</style>
</head>
<body>
<header><h1>__APP_NAME__</h1><span class="badge">ACowork app · __APP_ID__</span></header>
<div class="grid">
  <div class="card"><h2>Session &amp; injected identity</h2><pre id="ctx">loading…</pre></div>
  <div class="card"><h2>Platform API (via app backend)</h2><pre id="platform">loading…</pre></div>
  <div class="card"><h2>App REST (__SUMMARY_PATH__)</h2><pre id="data">loading…</pre></div>
</div>
<div class="actions">
  <button id="notify">Notify platform</button>
  <button id="refresh" class="primary">Refresh</button>
</div>
<div class="note" id="status"></div>
<script>
const APP = '__APP_ID__';
const SUMMARY = '__SUMMARY_PATH__';
const ticket = new URLSearchParams(location.hash.slice(1)).get('ticket') || '';
const base = '/api/apps/__APP_ID__/api/';
function call(path) {
  const headers = {};
  if (ticket) headers['X-App-Session'] = ticket;
  return fetch(base + path, { headers });
}
async function show(id, path, pick) {
  const el = document.getElementById(id);
  try {
    const r = await call(path);
    const text = await r.text();
    if (!r.ok) { el.className = 'err'; el.textContent = path + ' → ' + r.status + ' ' + text.slice(0, 300); return; }
    const json = JSON.parse(text);
    el.className = 'ok';
    el.textContent = JSON.stringify(pick ? (json[pick] ?? json) : json, null, 2).slice(0, 1200);
  } catch (e) { el.className = 'err'; el.textContent = String(e); }
}
async function refresh() {
  await show('ctx', 'app-context');
  await show('platform', 'app-context', 'platform');
  await show('data', SUMMARY);
  document.getElementById('status').textContent =
    'ticket=' + (ticket ? 'present' : 'MISSING') + ' · shell=app-owned · bridge=postMessage';
}
window.addEventListener('message', (e) => {
  const d = e.data;
  if (!d || d.channel !== 'acowork') return;
  if (d.type === 'theme') {
    document.documentElement.dataset.theme = d.theme || 'dark';
    if (d.accent) document.documentElement.style.setProperty('--accent', d.accent);
  }
});
document.getElementById('refresh').addEventListener('click', refresh);
document.getElementById('notify').addEventListener('click', () => {
  parent.postMessage({ channel: 'acowork', type: 'notify',
                       message: 'Hello from ' + APP }, '*');
});
parent.postMessage({ channel: 'acowork', type: 'ready', app: APP }, '*');
refresh();
</script>
</body>
</html>
"###;
    TEMPLATE
        .replace("__APP_ID__", app_id)
        .replace("__APP_NAME__", &esc(app_name))
        .replace("__SUMMARY_PATH__", &esc(summary_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_manifests_parse_and_validate() {
        let registry = AppRegistry::builtin();
        let ids: Vec<_> = registry.all().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["doc", "pm"]);
        assert_eq!(registry.get("pm").unwrap().process.port_hint, 18082);
        assert_eq!(registry.get("doc").unwrap().process.port_hint, 18081);
        assert_eq!(registry.catalog_apps().len(), 2);
    }

    #[test]
    fn app_id_rejects_path_traversal_and_uppercase() {
        for bad in ["", "../evil", "pm/../../etc", "PM", "-leading", "1abc", "a b"] {
            assert!(!is_valid_app_id(bad), "should reject {bad:?}");
        }
        assert!(is_valid_app_id("pm"));
        assert!(is_valid_app_id("my_app-2"));
    }

    #[test]
    fn validate_rejects_unsafe_paths_and_binaries() {
        let mut m = AppManifest::from_toml(PM_APP_TOML).unwrap();
        m.ui.path = "/ui/../../admin".to_string();
        assert!(validate_manifest(&m).is_err());

        let mut m = AppManifest::from_toml(PM_APP_TOML).unwrap();
        m.process.binary = "../../bin/sh".to_string();
        assert!(validate_manifest(&m).is_err());

        let mut m = AppManifest::from_toml(PM_APP_TOML).unwrap();
        m.process.runtime = AppRuntime::Legacy;
        m.process.legacy_slot = None;
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn enable_disable_overlay_round_trips() {
        let mut registry = AppRegistry::builtin();
        assert!(registry.is_enabled("pm"));
        assert!(registry.set_enabled("pm", false));
        assert!(!registry.is_enabled("pm"));
        assert_eq!(registry.catalog_apps().len(), 1);
        assert!(registry.set_enabled("pm", true));
        assert!(registry.is_enabled("pm"));
        assert!(!registry.set_enabled("ghost", true));

        let dir = std::env::temp_dir().join(format!("acowork-app-registry-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let disabled: BTreeSet<String> = ["doc".to_string()].into_iter().collect();
        AppRegistry::save_disabled(&dir, &disabled).unwrap();
        assert_eq!(AppRegistry::load_disabled(&dir), disabled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
