//! Live MCP connection state per server, for the Desktop Tools panel.
//!
//! `agent_mcp_tools.json` is reconciled against the live `tools/list`, so a
//! **failed** connect empties that server's tool rows. The card then renders
//! switch-on with no tools and no error — indistinguishable from "this server
//! exposes nothing". Every agent hit that at once on 2026-11-26: a sleep/wake
//! left the NIC unready for the ~1s the reconnect ran, `os error 10051`
//! (network unreachable) went to the log only, and the panel showed three
//! enabled servers with nothing under them.
//!
//! So the connect pass reports itself here, and `GET /agents/{id}/mcp-tools`
//! forwards it. Three states, because the panel must not have to guess:
//!
//! - **absent** — the server connected on the last pass (its tools are in
//!   `agent_mcp_tools.json`);
//! - **connecting** — a pass is running right now;
//! - **failed** — the last pass could not connect; carries the error.
//!
//! Nothing is persisted: a connect outcome is a live fact, and a stale one is
//! worse than none — the same reasoning that keeps
//! `SessionManager::apply_mcp_connection_result` from writing a failure notice
//! into the session history.
//!
//! ## Why a process-wide registry instead of a field on `HttpState`
//!
//! The writes happen inside [`crate::tools::mcp_manager::connect_mcp_with_reconcile_and_filter`],
//! a free async function with no HTTP state in scope, and they must happen for
//! *every* caller of it (startup, MQTT-driven reload, the panel's retry) — one
//! funnel, one place to look. The read side is the HTTP handler. This is the
//! same bridge `mcp_ensure` already uses for its install status.
//!
//! ponytail: process-wide map assumes one agent per Runtime process (true
//! today — the Runtime is spawned per agent instance). Ceiling: a Runtime
//! hosting several agents would need `(instance_id, name)` keys, exactly like
//! `mcp_ensure`'s registry.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use acowork_core::protocol::McpServerConfigDef;
use acowork_mcp::client::McpConnectionFailure;

/// One server's live connect state. Serialized as
/// `{"state":"connecting"}` / `{"state":"failed","error":"..."}` — the same
/// shape `EnsureStatus` uses, so the panel renders both with one convention.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ServerStatus {
    Connecting,
    Failed { error: String },
}

impl ServerStatus {
    pub fn is_connecting(&self) -> bool {
        matches!(self, Self::Connecting)
    }
}

fn registry() -> &'static Mutex<HashMap<String, ServerStatus>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, ServerStatus>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, ServerStatus>> {
    registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Mark every server of this pass as connecting. Called *before* the connect
/// I/O so a slow or hanging handshake is a state the panel can show, not a
/// blank row.
pub fn begin_attempt(configs: &[McpServerConfigDef]) {
    let attempted: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
    let mut map = lock();
    apply_begin(&mut map, &attempted);
}

/// Record the outcome of that pass: servers that connected lose their entry
/// (their tools are the evidence), failures keep the error. A retry that
/// succeeds therefore erases the previous message by construction.
pub fn record_outcome(configs: &[McpServerConfigDef], failures: &[McpConnectionFailure]) {
    let attempted: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
    let failed: Vec<(String, String)> = failures
        .iter()
        .map(|f| (f.server_name.clone(), f.error_message.clone()))
        .collect();
    let mut map = lock();
    apply_outcome(&mut map, &attempted, &failed);
}

/// Snapshot for the HTTP layer. `BTreeMap` keeps the wire order stable.
pub fn snapshot() -> BTreeMap<String, ServerStatus> {
    let map = lock();
    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn apply_begin(map: &mut HashMap<String, ServerStatus>, attempted: &[&str]) {
    for name in attempted {
        map.insert((*name).to_string(), ServerStatus::Connecting);
    }
}

fn apply_outcome(
    map: &mut HashMap<String, ServerStatus>,
    attempted: &[&str],
    failed: &[(String, String)],
) {
    // A server this pass did not attempt — disabled, or dropped from the
    // config since — has no live status left. Keeping its old entry would
    // show a failure for a server the Runtime is not even trying to connect.
    let attempted_set: HashSet<&str> = attempted.iter().copied().collect();
    map.retain(|name, _| attempted_set.contains(name.as_str()));
    // Attempted servers start from a clean slate: connected means no entry at
    // all, so a retry that succeeds cannot leave the old message behind.
    for name in attempted {
        map.remove(*name);
    }
    for (server, error) in failed {
        map.insert(
            server.clone(),
            ServerStatus::Failed {
                error: error.clone(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed_of<'a>(map: &'a HashMap<String, ServerStatus>, name: &str) -> Option<&'a str> {
        match map.get(name) {
            Some(ServerStatus::Failed { error }) => Some(error.as_str()),
            _ => None,
        }
    }

    #[test]
    fn a_pass_in_flight_is_reported_as_connecting() {
        let mut map: HashMap<String, ServerStatus> = HashMap::new();
        apply_begin(&mut map, &["pm", "doc"]);
        assert!(map.values().all(ServerStatus::is_connecting));
    }

    #[test]
    fn a_successful_retry_erases_the_previous_error() {
        let mut map: HashMap<String, ServerStatus> = HashMap::new();
        apply_outcome(
            &mut map,
            &["pm", "doc"],
            &[
                ("pm".to_string(), "os error 10051".to_string()),
                ("doc".to_string(), "timeout".to_string()),
            ],
        );
        assert_eq!(map.len(), 2);
        assert_eq!(failed_of(&map, "pm"), Some("os error 10051"));

        // Same pass, network back: both attempted, none failed.
        apply_begin(&mut map, &["pm", "doc"]);
        assert!(map.values().all(ServerStatus::is_connecting));
        apply_outcome(&mut map, &["pm", "doc"], &[]);
        assert!(
            map.is_empty(),
            "a connected server must carry no status entry at all"
        );
    }

    #[test]
    fn a_pass_that_stops_attempting_a_server_drops_its_status() {
        let mut map: HashMap<String, ServerStatus> = HashMap::new();
        apply_outcome(&mut map, &["pm", "doc"], &[("doc".to_string(), "timeout".to_string())]);
        assert!(map.contains_key("doc"));

        // `doc` was switched off: the next pass does not attempt it, so its
        // stale failure must not survive as a live status.
        apply_outcome(&mut map, &["pm"], &[]);
        assert!(map.is_empty());
    }

    #[test]
    fn a_server_that_fails_again_reports_the_new_message() {
        let mut map: HashMap<String, ServerStatus> = HashMap::new();
        apply_outcome(&mut map, &["pm"], &[("pm".to_string(), "first".to_string())]);
        apply_outcome(&mut map, &["pm"], &[("pm".to_string(), "second".to_string())]);
        assert_eq!(failed_of(&map, "pm"), Some("second"));
    }
}