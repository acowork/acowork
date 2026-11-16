//! Runtime-state projection for hosted apps (ADR-116).
//!
//! The registry is the declarative source of truth; process state is owned by
//! the supervisors. This module is the single place that resolves "which port
//! is app X on right now", so the proxy, the apps API and the MCP catalog can
//! never disagree about an app's availability.

use std::collections::HashMap;

use acowork_core::app::{AppManifest, AppRuntime};

use crate::gateway::state::GatewayState;

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
