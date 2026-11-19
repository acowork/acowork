//! Runtime-state projection for hosted apps (ADR-116).
//!
//! The registry is the declarative source of truth; process state is owned by
//! the supervisors. This module is the single place that resolves "which port
//! is app X on right now", so the proxy, the apps API and the MCP catalog can
//! never disagree about an app's availability.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use acowork_core::app::{
    APPS_DIR_NAME, AppInstallState, AppManifest, AppRuntime, APP_STATE_FILE, default_package_entry,
};

use crate::gateway::state::GatewayState;
use crate::lifecycle::process::sibling_binary;

/// Runtime state of one supervised app process (generic runtime only).
#[derive(Debug, Clone)]
pub struct AppProcessState {
    pub pid: u32,
    pub port: u16,
    pub ready: bool,
}

/// Stable runtime projection stored on [`GatewayState`].
pub type AppProcessTable = HashMap<String, AppProcessState>;

/// Resolve the loopback port an app is currently serving on.
///
/// Returns `None` when the app is disabled at the platform level or when its
/// supervisor has not reported readiness. Both cases map to `503` in the
/// proxy, matching the ADR-064 contract.
pub fn resolve_app_port(gw: &GatewayState, manifest: &AppManifest) -> Option<u16> {
    let registry = gw.app_registry.read().ok()?;
    if !registry.is_enabled(&manifest.id) {
        return None;
    }
    drop(registry);

    match manifest.process.runtime {
        AppRuntime::Legacy => match manifest.process.legacy_slot.as_deref() {
            Some("pm") => gw.pm_process.as_ref().map(|p| p.port),
            Some("doc") => gw.doc_process.as_ref().map(|p| p.port),
            _ => None,
        },
        AppRuntime::Generic => gw.app_processes.get(&manifest.id).map(|p| p.port),
    }
}

/// A coarse, UI-friendly status string.
pub fn app_status(gw: &GatewayState, manifest: &AppManifest) -> &'static str {
    let enabled = gw
        .app_registry
        .read()
        .map(|r| r.is_enabled(&manifest.id))
        .unwrap_or(false);
    if !enabled {
        return "disabled";
    }
    if resolve_app_port(gw, manifest).is_some() {
        "ready"
    } else {
        "starting"
    }
}

/// The Gateway-facing MCP URL for one app, built from the advertise host so
/// remote Nodes can reach it through the Gateway (never the app port).
pub fn app_mcp_url(gw: &GatewayState, manifest: &AppManifest) -> Option<String> {
    if resolve_app_port(gw, manifest).is_none() {
        return None;
    }
    Some(format!(
        "http://{}:{}{}",
        gw.advertise_host, gw.http_port, manifest.mcp.gateway_path
    ))
}

/// Three-tier binary resolution (design doc 34 §6.5). Hit-and-stop: a path
/// that exists but is outside the allowed roots or not executable is a hard
/// failure — we do **not** fall through to the next tier.
pub fn resolve_app_binary(data_dir: &Path, manifest: &AppManifest) -> Result<PathBuf, String> {
    let apps_root = data_dir.join(APPS_DIR_NAME);
    let app_root = apps_root.join(&manifest.id);

    // Tier 1: installed package entry under versions/{current}/.
    if let Some(state) = read_install_state(&app_root) {
        let entry = manifest
            .package
            .as_ref()
            .and_then(|p| p.entry.clone())
            .unwrap_or_else(|| default_package_entry(&manifest.process.binary));
        let version_root = app_root.join("versions").join(&state.current);
        let mut candidates = vec![version_root.join(&entry)];
        // Authors often declare `entry = "bin/foo"` even when packing on
        // Windows produces `bin/foo.exe`. Try the platform twin before
        // falling through — still hit-and-stop once a path exists.
        if cfg!(windows) && !entry.ends_with(".exe") {
            candidates.push(version_root.join(format!("{entry}.exe")));
        }
        if !cfg!(windows) && entry.ends_with(".exe") {
            candidates.push(version_root.join(entry.trim_end_matches(".exe")));
        }
        for candidate in candidates {
            if candidate.exists() {
                return accept_binary(&candidate, &[apps_root.as_path()]);
            }
        }
    }

    // Tier 2: flat development layout `apps/{id}/{binary}`.
    let flat_name = if cfg!(windows) {
        format!("{}.exe", manifest.process.binary)
    } else {
        manifest.process.binary.clone()
    };
    let flat = app_root.join(&flat_name);
    if flat.exists() {
        return accept_binary(&flat, &[apps_root.as_path()]);
    }

    // Tier 3: sibling of the Gateway binary (built-ins).
    let sibling = sibling_binary(&manifest.process.binary);
    if sibling.exists() {
        // Sibling lives next to the Gateway exe — not under apps/. Allowed
        // by design for built-in / shipped binaries; no apps_root check.
        if !is_executable_file(&sibling) {
            return Err(format!(
                "app binary at {} is not executable",
                sibling.display()
            ));
        }
        return Ok(sibling);
    }

    Err(format!(
        "app binary for '{}' not found (checked package entry, apps/{}/{}, sibling)",
        manifest.id, manifest.id, flat_name
    ))
}

fn read_install_state(app_root: &Path) -> Option<AppInstallState> {
    let path = app_root.join(APP_STATE_FILE);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn accept_binary(path: &Path, allowed_roots: &[&Path]) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path).map_err(|e| {
        format!(
            "app binary at {} cannot be canonicalized: {e}",
            path.display()
        )
    })?;
    let under_allowed = allowed_roots.iter().any(|root| {
        std::fs::canonicalize(root)
            .ok()
            .is_some_and(|r| canonical.starts_with(&r))
    });
    if !under_allowed {
        return Err(format!(
            "app binary at {} is outside the allowed app directories",
            canonical.display()
        ));
    }
    if !is_executable_file(&canonical) {
        return Err(format!(
            "app binary at {} is not executable",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"))
            || path.is_file()
    }
}

#[cfg(test)]
mod binary_tests {
    use super::*;
    use acowork_core::app::PM_APP_TOML;
    use std::io::Write;

    #[test]
    fn resolve_falls_through_to_sibling_for_builtins() {
        let manifest = AppManifest::from_toml(PM_APP_TOML).unwrap();
        let dir = std::env::temp_dir().join(format!("acowork-bin-resolve-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Without an install ledger or flat binary, resolve should attempt
        // the sibling path. Whether it exists depends on the test host; we
        // only assert the error message shape when missing.
        match resolve_app_binary(&dir, &manifest) {
            Ok(p) => assert!(p.as_os_str().len() > 0),
            Err(e) => assert!(e.contains("not found"), "{e}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_prefers_flat_layout_under_apps() {
        let mut manifest = AppManifest::from_toml(PM_APP_TOML).unwrap();
        manifest.process.runtime = AppRuntime::Generic;
        manifest.process.legacy_slot = None;
        let dir = std::env::temp_dir().join(format!("acowork-bin-flat-{}", std::process::id()));
        let app_dir = dir.join(APPS_DIR_NAME).join(&manifest.id);
        std::fs::create_dir_all(&app_dir).unwrap();
        let bin_name = if cfg!(windows) {
            format!("{}.exe", manifest.process.binary)
        } else {
            manifest.process.binary.clone()
        };
        let bin_path = app_dir.join(&bin_name);
        {
            let mut f = std::fs::File::create(&bin_path).unwrap();
            f.write_all(b"fake").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin_path, perms).unwrap();
        }
        let resolved = resolve_app_binary(&dir, &manifest).unwrap();
        assert_eq!(
            std::fs::canonicalize(&bin_path).unwrap(),
            resolved
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
