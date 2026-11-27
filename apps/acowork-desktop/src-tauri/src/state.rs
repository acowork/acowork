//! Application state shared across Tauri commands
//!
//! Holds shared, mutable application state accessible from any Tauri command.
//!
//! ## MQTT connection status (ADR-036 / ADR-039)
//!
//! MQTT connection state is intentionally NOT stored here.  The single
//! source of truth lives in `DesktopMqttClient::session_state` (a
//! `tokio::sync::watch` channel updated synchronously by the poll task's
//! `on_status` callback).  `get_mqtt_status` reads it directly via
//! `session_state().current()`, which means:
//!
//! - No cache to keep in sync with the watch channel
//! - No `tokio::spawn` indirection that could race the frontend's
//!   snapshot read after a webview reload
//! - One well-defined path for status transitions
//!
//! The `on_status` callback also emits a `mqtt-status` Tauri event for
//! real-time updates, but the event is best-effort: the frontend's
//! `initMqttListener` always calls `get_mqtt_status` to fetch the
//! authoritative snapshot, then subscribes for subsequent updates.

use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

use serde::{Deserialize, Serialize};

use acowork_core::mqtt_proto::{BootstrapPhase, BootstrapState};

use crate::commands::gateway::SharedBootInflight;

use crate::gateway_client::{GatewayAuth, GatewayClient};
use crate::mqtt_client::{MqttEndpoint, SharedDesktopMqttClient};

/// Gateway deployment mode, mirrors frontend `GatewayMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayMode {
    /// Local mode: Desktop App spawns a child Gateway process on the
    /// global default host:port (see `acowork_core::defaults::GATEWAY_HTTP_URL`).
    Local,
    /// Remote mode: Desktop App connects to a pre-existing Gateway at
    /// a user-configured URL (e.g. a Gateway running in WSL).
    Remote,
    /// Relay mode (design doc 24 §8.0): the Gateway is reached through
    /// the cloud relay over the public internet. `base_url` points at
    /// the relay's device domain (`https://<gw-id>.<relay-suffix>`);
    /// HTTP rides the relay's TLS byte pipe and MQTT connects over
    /// WSS to `<base_url>/mqtt` (the Gateway-side `/mqtt` WS bridge,
    /// reached through the same pipe). No local Gateway is spawned,
    /// and the strict remote MQTT listener requires the logged-in
    /// account's access token as the CONNECT password.
    Relay,
}

impl GatewayMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "remote" => GatewayMode::Remote,
            "relay" => GatewayMode::Relay,
            _ => GatewayMode::Local,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            GatewayMode::Local => "local",
            GatewayMode::Remote => "remote",
            GatewayMode::Relay => "relay",
        }
    }
}

/// Proto-name of a bootstrap phase, e.g. `BOOTING`, `SHUTTING_DOWN`.
///
/// Mirrors the Gateway's `/api/bootstrap` projection (`http/bootstrap_api.rs`
/// `phase_name`): SCREAMING_SNAKE_CASE without the `BOOTSTRAP_PHASE_` prefix
/// that `BootstrapPhase::as_str_name()` produces.
fn phase_name(phase: BootstrapPhase) -> &'static str {
    match phase {
        BootstrapPhase::Unspecified => "UNSPECIFIED",
        BootstrapPhase::Booting => "BOOTING",
        BootstrapPhase::Ready => "READY",
        BootstrapPhase::Degraded => "DEGRADED",
        BootstrapPhase::Failed => "FAILED",
        BootstrapPhase::ShuttingDown => "SHUTTING_DOWN",
    }
}

/// Latest bootstrap snapshot, mirrored from the Gateway's retained
/// `acowork/global/bootstrap` MQTT topic / `GET /api/bootstrap` projection.
///
/// Field-for-field the wire-level `BootstrapState` proto (ADR-059 §5.4.4).
/// `phase` is the SCREAMING_SNAKE_CASE name of the proto enum, matching the
/// Gateway's HTTP JSON projection exactly so Desktop and Gateway agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapStateView {
    pub protocol_version: u32,
    pub instance_id: String,
    pub version: u64,
    pub phase: String,
    pub phase_detail: String,
    pub issued_at_ms: u64,
}

impl BootstrapStateView {
    /// Convert a decoded `BootstrapState` proto (MQTT retained payload).
    pub fn from_proto(proto: &BootstrapState) -> Self {
        Self {
            protocol_version: proto.protocol_version,
            instance_id: proto.instance_id.clone(),
            version: proto.version,
            phase: phase_name(BootstrapPhase::try_from(proto.phase).unwrap_or(BootstrapPhase::Unspecified))
                .to_string(),
            phase_detail: proto.phase_detail.clone(),
            issued_at_ms: proto.issued_at_ms,
        }
    }
}

/// Shared application state
pub struct AppState {
    /// Gateway HTTP client. `base_url` reflects the active configuration:
    ///   - Local mode  → `acowork_core::defaults::GATEWAY_HTTP_URL`
    ///   - Remote mode → user-configured URL
    pub gateway: Arc<tokio::sync::RwLock<GatewayClient>>,
    /// Desktop-side mirror of the webview's account session (ADR-076
    /// §决策 3). Shared with the [`GatewayClient`] held above: every request
    /// carries its token and replays once after a transparent renewal.
    /// Exposed here so the Tauri command layer can mirror the webview's
    /// token without reaching into the HTTP client.
    pub gateway_auth: Arc<GatewayAuth>,
    /// Files whose chunked upload the user asked to stop (ADR-122 §3). Shared
    /// with the [`GatewayClient`] held above, and held here as well so the
    /// cancel command does not have to queue behind the upload's own read
    /// lock — a cancel that waits for the upload it cancels is no cancel.
    pub upload_cancels: Arc<crate::gateway_client::UploadCancels>,
    /// Active deployment mode. Set by `set_gateway_config` (called from frontend).
    pub gateway_mode: Arc<tokio::sync::RwLock<GatewayMode>>,
    /// Handle to the locally spawned Gateway process (None in remote mode,
    /// when the configured Gateway is foreign/adopted, or before
    /// `init_local_gateway` is called). `Some(child)` ⇔ Gateway is
    /// *owned* by this Desktop process.
    pub gateway_process: Arc<Mutex<Option<Child>>>,
    /// Exit policy for the owned Gateway process (set by the tray-quit
    /// dialog). `true` = the user chose "quit, keep Gateway running", so
    /// every exit path (RunEvent::Exit, AppState Drop) must leave the
    /// child process alive. Default `false` = stop the Gateway on exit
    /// (historical behavior).
    pub gateway_keep_running_on_exit: Arc<AtomicBool>,

    /// In-flight local-Gateway boot, for request coalescing.
    ///
    /// `init_local_gateway` is a whole pipeline (probe → spawn/adopt →
    /// wait for bootstrap READY), and the frontend can legitimately
    /// issue it twice concurrently — React.StrictMode double-invokes the
    /// SplashScreen effect in dev, which produced two `spawn_gateway`
    /// entries and two parallel 30 s bootstrap waits in the 2026-11-24
    /// incident log. `spawn_gateway` was already serialised, but the
    /// *wait* was not, so both callers burned the full budget and both
    /// surfaced an error.
    ///
    /// The first caller registers a watch slot and runs the pipeline;
    /// concurrent callers await the same outcome instead of starting a
    /// second one. The slot is cleared when the pipeline finishes, so
    /// this coalesces concurrent requests only — a later, deliberate
    /// retry still runs a fresh boot.
    pub gateway_boot_inflight: SharedBootInflight,

    /// ADR-033 Phase 3: Desktop MQTT client for real-time events.
    /// Connected after the Gateway is confirmed healthy. None until
    /// `connect_mqtt` is called from the frontend.
    ///
    /// Connection state lives inside the client (`session_state()`); see
    /// module docs.
    pub mqtt_client: Arc<Mutex<Option<SharedDesktopMqttClient>>>,

    /// Broker endpoint (transport + address) the active `mqtt_client`
    /// was created for.
    ///
    /// Written by `connect_mqtt` on success and cleared by
    /// `disconnect_mqtt`. It lets `connect_mqtt` detect that the
    /// Gateway address OR transport changed while a stale client was
    /// still connected (e.g. the user edited the remote URL on the
    /// SplashScreen timeout view, or switched local↔relay mode in
    /// Settings) and rebuild the connection instead of short-circuiting
    /// on the outdated client. Transport is part of the identity:
    /// `host:port` TCP and a WSS URL to the same authority are
    /// different endpoints. Lock order (when both are held):
    /// `mqtt_client` then `mqtt_endpoint`.
    pub mqtt_endpoint: Arc<Mutex<Option<MqttEndpoint>>>,

    /// ADR-059: latest Gateway bootstrap snapshot.
    ///
    /// Updated by the MQTT `bootstrap_handler` on every push of the
    /// retained `acowork/global/bootstrap` topic (including re-delivery on
    /// reconnect). `get_bootstrap()` falls back to a one-shot HTTP
    /// `GET /api/bootstrap` when this is `None` (e.g. MQTT not yet
    /// connected) and caches the result here.
    pub bootstrap_state: Arc<tokio::sync::RwLock<Option<BootstrapStateView>>>,
}

impl AppState {
    /// Create a new AppState. Initial defaults:
    ///   - mode = Local (matches the pre-bug UX where Rust spawned a local
    ///     gateway immediately; the frontend must call `set_gateway_config`
    ///     on startup to switch to Remote if needed)
    ///   - base_url = acowork_core::defaults::GATEWAY_HTTP_URL
    ///   - exit policy = stop the owned Gateway on exit (historical)
    pub fn new() -> Self {
        // One `GatewayAuth` shared by `AppState` (Tauri command layer) and
        // the `GatewayClient` inside `gateway`.
        let client = GatewayClient::new();
        let gateway_auth = client.auth_handle();
        let upload_cancels = client.cancel_handle();
        Self {
            gateway: Arc::new(tokio::sync::RwLock::new(client)),
            gateway_auth,
            upload_cancels,
            gateway_mode: Arc::new(tokio::sync::RwLock::new(GatewayMode::Local)),
            gateway_process: Arc::new(Mutex::new(None)),
            gateway_keep_running_on_exit: Arc::new(AtomicBool::new(false)),
            gateway_boot_inflight: Arc::new(tokio::sync::Mutex::new(None)),
            mqtt_client: Arc::new(Mutex::new(None)),
            mqtt_endpoint: Arc::new(Mutex::new(None)),
            bootstrap_state: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }
}

/// Last-resort cleanup: stop the local Gateway process tree when AppState is
/// dropped (e.g. on Ctrl+C termination, OS shutdown, or forced exit) —
/// UNLESS the user chose to keep the Gateway running on exit.
///
/// This is a safety net — the tray "quit" handler and `RunEvent::Exit` handler
/// in lib.rs normally stop the Gateway before Drop fires in normal shutdown.
/// But on abrupt termination (Ctrl+C in dev mode, `taskkill` of the Tauri
/// process), Rust's stack unwind will run this Drop and prevent orphaned
/// Gateway / Runtime / Embed processes from lingering (when the default
/// stop-on-exit policy applies).
impl Drop for AppState {
    fn drop(&mut self) {
        // Exit policy: if the user asked to keep the Gateway running,
        // leave the child alone (it becomes an independent process).
        if self.gateway_keep_running_on_exit.load(Ordering::Relaxed) {
            tracing::info!(
                "AppState dropped with keep-running policy — leaving Gateway process alive"
            );
            // Do NOT take the child: taking and dropping it would close our
            // handle, which is fine (Child::drop detaches), but we must not
            // kill or wait. Simply return and let the Child drop naturally.
            return;
        }

        // Only try to lock if the mutex isn't poisoned. During unwind from
        // a panic, the mutex may be poisoned.
        if let Ok(mut proc) = self.gateway_process.try_lock()
            && let Some(mut child) = proc.take()
        {
            let pid = child.id();
            tracing::info!(pid = pid, "AppState dropped, stopping Gateway process tree");
            crate::process_stop::stop_gateway_process(pid);
            let _ = child.wait();
        }
    }
}
