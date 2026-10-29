//! ACowork Desktop App — Tauri v2 backend
//!
//! This is the library entry point for the Tauri application.
//! It sets up the Tauri builder with all plugins, commands, and tray.
//!
//! ## Gateway boot flow
//!
//! The local Gateway is **NOT** spawned in the setup hook anymore —
//! that was the source of a long-standing bug where Rust unconditionally
//! spawned a child process on the hardcoded default URL, ignoring the
//! frontend's "remote gateway" setting.
//!
//! The new flow is:
//! 1. Setup hook only wires window/tray/single-instance plugins. No spawn.
//! 2. Frontend (`SplashScreen` init) reads its persisted `settingsStore`,
//!    calls `set_gateway_config(mode, url)` to push config into Rust.
//! 3. If mode = local, frontend then calls `init_local_gateway` which
//!    spawns the child Gateway on `defaults::GATEWAY_HTTP_URL` and waits
//!    for `/health`.
//! 4. If mode = remote, frontend skips spawn and just polls `/health`
//!    on the user-configured URL.
//! 5. After the gateway is reachable, frontend onboarding / agentStore
//!    installs agents via the standard `POST /api/agents/ensure` endpoint;
//!    no special "ensure System Agent" Tauri command is needed (ADR-077).

mod commands;
mod gateway_client;
mod mqtt_client;
mod navigation_guard;
mod process_stop;
mod state;
mod tray;
#[cfg(target_os = "windows")]
mod win_wndproc;
#[cfg(target_os = "macos")]
mod macos_workspace;
#[cfg(target_os = "linux")]
mod linux_logind;
#[cfg(target_os = "linux")]
mod linux_screensaver;
use state::AppState;
use std::time::Duration;
use tauri::{Emitter, Manager};

// ── System-sleep detection (Windows / macOS / Linux) ────────────────────────
//
// The frontend's old time-gap heuristic (heartbeat + visibilitychange) could
// not distinguish "window minimised for N seconds" from "system slept for N
// seconds", causing false `location.reload()` triggers on normal minimise →
// restore cycles.
//

// Detection now lives in the shared `acowork-mqtt-session` crate
// (`power::detect_resume` / `power::run_power_probe_loop`, ADR-065 Step 1) so
// Desktop / Node / Runtime recover from OS sleep/wake with identical timing.
// The Rust backend samples two monotonic clocks (biased vs unbiased) on each
// `Focused(true)` event and on a 2 s polling task; if the biased/unbiased gap
// exceeds the threshold, the system was genuinely asleep — not merely
// backgrounded.


/// Recovery actions after a system wake.
///
/// Called from both the 2-second polling task and the `Focused(true)`
/// window event handler when [`acowork_mqtt_session::power::detect_resume`]
/// reports genuine sleep. Recovery is a strict, ordered sequence — each
/// step is issued only after the previous one completes, which is exactly
/// what makes it race-free by construction:
///
/// 1. **Rebuild the MQTT connection deterministically** – the OS tears
///    down TCP sockets during sleep, so the old EventLoop is unusable by
///    definition. [`DesktopMqttClient::recover_after_wake`] resets the
///    session state synchronously (so `wait_for_connected` can never read
///    the stale pre-sleep `Connected` value) and requests a fresh client
///    + EventLoop pair.
///
/// 2. **Wait for a real ConnAck** – the webview reload of step 3 is
///    deliberately deferred until after `wait_for_connected` succeeds, so
///    the freshly loaded frontend sees `connected: true` on its very
///    first `get_mqtt_status` / `mqtt-status` signal. Reloading before
///    the reconnect would boot the frontend against a Connecting client
///    and show "Connecting to agent..." for tens of seconds. That UX race
///    was the historical excuse for removing the reload; ordering
///    eliminates the race instead of removing the recovery.
///
/// 3. **Wait for the display stack, then reload the webview** –
///    REQUIRED, never remove. System sleep routinely freezes or kills
///    the WebView2 renderer / GPU compositor (Chromium resume bug): JS
///    timers stop firing, the IPC channel breaks silently, the
///    compositor stops submitting frames and the window shows only its
///    transparent/blurred background. MQTT recovery (steps 1–2) does
///    nothing for a frozen renderer, and any "frontend converges in
///    place" logic only works while the webview JS is alive — after a
///    wake it may not be. The reload is the only deterministic recovery
///    for the renderer and is the programmatic equivalent of the user
///    pressing F5 (which always recovers). It is issued through the
///    native browser-process API `WebviewWindow::reload()` rather than a
///    JS `eval`-only reload, because eval requires a live JS context
///    while the native call forces a fresh renderer even when the old one
///    is frozen.
///
///    The reload is **event-driven, but OS display events are only the
///    first trigger** — they arrive while the WebView2 compositor is
///    still coming back (2026-09-06 real-world wake: a reload issued on
///    the display event executed, yet the window stayed acrylic-only
///    until a manual F5 seconds later). So each attempt waits for the
///    display event (`GUID_MONITOR_POWER_ON` / `WM_DISPLAYCHANGE` via
///    the Windows WndProc subclass, window focus on any platform; 5 s
///    timeout fallback when no event arrives) and then lets step 4
///    decide, empirically, whether that reload really displayed. See
///    [`wake_recovery`] for the full event semantics.
///
///    Immediately before reloading, a best-effort eval sets
///    the `acowork_recovery_reload` sessionStorage flag (with a short
///    grace period so the eval can run inside a thawing renderer).
///    `App.tsx` reads that flag to skip the Splash screen and —
///    critically — re-register the Tauri event listeners destroyed by
///    the reload (`initMqttListener`, `initWorkspaceFsListener`); without
///    them the frontend `mqttConnected` flag stays false and the UI would
///    hang on "Connecting to agent". The flag also arms the first-frame
///    verification of step 4 (only flag-carrying pages fire it). Losing
///    the flag (the eval never runs on a frozen renderer) is a safe
///    degradation: the normal boot path registers the same listeners
///    after the Splash gateway check, which passes instantly because the
///    Gateway survived the wake.
///
/// 4. **Verify the reload actually displayed, then retry on failure** —
///    after each reload the recovery task waits up to 10 s for the page
///    to report its first painted frame: `App.tsx` fires the
///    `desktop_recovery_visible` command from a `requestAnimationFrame`
///    callback, which the GPU compositor drives. It fires only when a
///    frame was genuinely composited after this reload, so neither a
///    thawed old page nor a mounted-but-not-composited page can fake it
///    (heartbeat-based verification was removed on 2026-09-06 for
///    exactly that false positive). If the signal does not arrive — the
///    page never mounted or the compositor is still down — the reload
///    is re-attempted (up to 3 times; the reload cooldown is checked
///    once up front, so failed attempts retry every ~10 s); each retry
///    lands later, so recovery converges once the compositor is
///    genuinely usable, no matter how long that takes.
///
/// **Concurrency guard**: `RECOVERY_IN_PROGRESS` prevents overlapping
/// calls.  Although `detect_resume()` atomically updates the clock
/// baseline (so it normally returns `true` only once per sleep event),
/// the polling task and the `Focused(true)` handler could theoretically
/// race on the very first call.  The guard ensures only one recovery
/// task runs at a time, preventing a double `recover_after_wake()` that
/// would reset the connection the first task is waiting on.
///
/// **Regression warning — do NOT remove the webview reload**: a previous
/// refactor (ADR-065 migration, Sep 2026) dropped the reload citing a
/// "race with the reconnection" that the ordered sequence above already
/// eliminates, and replaced it with "frontend converges in place". The
/// consequence was a real-world wake (2026-09-06) where the renderer
/// froze for minutes with no self-healing path until the user pressed
/// F5. MQTT and the webview renderer are independent subsystems: a
/// healthy connection says nothing about a live renderer. Future changes
/// must only harden this sequence — never remove the reload step.
static RECOVERY_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Cooldown gate for post-wake webview reloads.
///
/// `WebviewWindow::reload()` is asynchronous — the old renderer/GPU
/// processes are torn down lazily. Without a gate, rapid sleep/wake
/// cycles would stack successive reloads (and their process spawns)
/// before the previous ones exited, leaking renderer/GPU processes
/// (original rationale: commit 825ab899). 15 s is far longer than a
/// reload takes (< 2 s) and far shorter than any realistic sleep/wake
/// interval, so a genuine second wake is never starved.
mod reload_cooldown {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    const RELOAD_COOLDOWN: Duration = Duration::from_secs(15);

    static LAST_RELOAD: Mutex<Option<Instant>> = Mutex::new(None);

    /// Returns `true` when the cooldown window has elapsed since the last
    /// successful reload. A poisoned mutex degrades to "allowed" so a
    /// panic elsewhere can never block wake recovery.
    pub fn allowed() -> bool {
        let last = LAST_RELOAD.lock().unwrap_or_else(|e| e.into_inner());
        match *last {
            Some(prev) => prev.elapsed() >= RELOAD_COOLDOWN,
            None => true,
        }
    }

    /// Records that a reload was issued. Called only after a successful
    /// reload, so a failed one leaves the gate open for an immediate
    /// retry on the next wake.
    pub fn mark() {
        let mut last = LAST_RELOAD.lock().unwrap_or_else(|e| e.into_inner());
        *last = Some(Instant::now());
    }
}

/// Renderer-recovery event plumbing: "display stack is usable again"
/// signals plus frontend-liveness tracking for the post-wake reload.
///
/// # Why events instead of a hardcoded delay
///
/// A wake reload issued immediately after `detect_resume()` fails in
/// practice (2026-09-06): the OS notifies processes that the system is
/// resuming, but the display/GPU stack takes longer to come back, and a
/// renderer spawned in that window never composites — the window keeps
/// showing only its acrylic/vibrancy background while the JS side may
/// even keep running (heartbeats continue). Sleeping a fixed amount
/// before the reload would guess at hardware/driver recovery time;
/// instead we wait for events whose semantics are "the user can see the
/// window again":
///
/// - **Windows**: `GUID_MONITOR_POWER_ON` power-setting broadcast and
///   `WM_DISPLAYCHANGE`, observed by the WndProc subclass
///   (`win_wndproc.rs`). The display has been powered back on / the
///   mode rebuilt — exactly when a manual F5 starts working.
/// - **macOS**: `NSWorkspaceDidWakeNotification` posted to
///   `[NSWorkspace sharedWorkspace].notificationCenter` on every
///   system wake, observed by the long-lived block-based observer
///   installed in `macos_workspace.rs` at process startup. The
///   observer runs on the main thread and feeds the same
///   `signal_display_ready()` entry point. AppKit has no
///   `NSApplicationDidWakeNotification`; NSWorkspace is the only
///   programmatic hook.
/// - **Linux**: `org.freedesktop.login1.Manager.PrepareForSleep(b)`
///   on the system D-Bus, with the boolean argument set to `false`
///   on the wake edge. Subscribed in `linux_logind.rs` over zbus
///   (`MessageStream::for_match_rule`); the long-lived drain task
///   feeds `signal_display_ready()` on every false edge.
/// - **All platforms**: the main window gaining focus
///   (`Focused(true)`). The user is interacting with the app, so the
///   system is fully usable.
///
/// If no signal arrives (an external monitor that never powered off,
/// a driver that does not broadcast, the OS-level observer failed to
/// install at startup, or the user has not yet touched the window),
/// a bounded 5 s timeout still forces the reload so recovery never
/// stalls. The timeout is the fallback, NOT the primary trigger —
/// and because the caller verifies the reload and retries, a
/// too-early reload self-heals on the next attempt.
///
/// # First-frame verification
///
/// `recover_from_wake` needs to know whether a reload actually brought
/// a DISPLAYING page back. Heartbeats cannot prove that (2026-09-06
/// real-world wake): `WebviewWindow::reload()` is async, so the dying
/// pre-reload page's catch-up heartbeat arrived ~25 ms after the reload
/// call and falsely completed recovery while no new page ever showed.
/// The reliable signal is the page itself reporting its first painted
/// frame — `App.tsx` fires `desktop_recovery_visible` from a
/// `requestAnimationFrame` callback on the recovery-reload boot path
/// only. `requestAnimationFrame` is driven by the GPU compositor: it
/// cannot fire while the compositor is still coming back after a wake,
/// so its firing is the renderer's own "I can draw now" event — the
/// exact moment a manual F5 starts working. Old (thawed) pages never
/// fire it: their boot had no `acowork_recovery_reload` flag, and the
/// flag-set eval runs at most 50 ms before the reload tears that JS
/// context down.
mod wake_recovery {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// A display-ready event older than this no longer proves the
    /// display is usable. Must exceed the wake-detection latency (one
    /// 2 s probe tick + scheduling jitter) so a recovery started right
    /// after `detect_resume()` still sees the event that fired while
    /// the system was finishing its resume.
    const DISPLAY_READY_FRESH_WINDOW: Duration = Duration::from_secs(30);

    static LAST_DISPLAY_READY: Mutex<Option<Instant>> = Mutex::new(None);

    fn ready_notify() -> &'static tokio::sync::Notify {
        static NOTIFY: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
        NOTIFY.get_or_init(tokio::sync::Notify::new)
    }

    /// Records that the display stack is usable again. Called by the
    /// platform signal sources (Windows WndProc messages, the
    /// `Focused(true)` window-event handler). Safe from any thread.
    ///
    /// `notify_one` (not `notify_waiters`) is deliberate: it keeps one
    /// permit when no waiter is registered yet, so a signal that lands
    /// between a waiter's freshness check and its registration is not
    /// lost — the waiter consumes the permit and re-checks freshness.
    pub(crate) fn signal_display_ready() {
        if let Ok(mut last) = LAST_DISPLAY_READY.lock() {
            *last = Some(Instant::now());
        }
        ready_notify().notify_one();
    }

    /// Resolves when a display-ready event is fresh, or when `timeout`
    /// elapses with none. Returns `true` if an event was observed
    /// (event-driven reload) and `false` on the timeout fallback.
    pub(crate) async fn wait_display_ready_or_timeout(timeout: Duration) -> bool {
        let wait = async {
            loop {
                let fresh = LAST_DISPLAY_READY
                    .lock()
                    .map(|last| match *last {
                        Some(t) => t.elapsed() < DISPLAY_READY_FRESH_WINDOW,
                        None => false,
                    })
                    .unwrap_or(false);
                if fresh {
                    return;
                }
                ready_notify().notified().await;
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }

    static LAST_FRONTEND_VISIBLE: Mutex<Option<Instant>> = Mutex::new(None);

    /// Records that a page reported its first painted frame — the
    /// `desktop_recovery_visible` command fired from a
    /// `requestAnimationFrame` callback. `requestAnimationFrame` is
    /// driven by the GPU compositor, so it only fires once a frame was
    /// genuinely composited; its firing is the renderer's own "I can
    /// draw now" signal — the exact moment a manual F5 starts working
    /// (see module docs for why OS display events alone are too early).
    pub(crate) fn mark_frontend_visible() {
        if let Ok(mut last) = LAST_FRONTEND_VISIBLE.lock() {
            *last = Some(Instant::now());
        }
    }

    /// `true` when a page reported a first painted frame strictly after
    /// `after` (i.e. after this recovery's reload).
    pub(crate) fn frontend_visible_since(after: Instant) -> bool {
        LAST_FRONTEND_VISIBLE
            .lock()
            .map(|last| matches!(*last, Some(t) if t > after))
            .unwrap_or(false)
    }
}

/// Frontend → backend "first frame painted" signal for post-wake
/// renderer recovery. `App.tsx` fires it from a `requestAnimationFrame`
/// callback on the recovery-reload boot path only (the path armed by
/// the `acowork_recovery_reload` flag). See the `wake_recovery` docs.
#[tauri::command]
fn desktop_recovery_visible() {
    wake_recovery::mark_frontend_visible();
}

fn recover_from_wake(app_handle: &tauri::AppHandle) {
    // Concurrency guard: only one recovery task at a time.
    if RECOVERY_IN_PROGRESS.swap(true, std::sync::atomic::Ordering::SeqCst) {
        tracing::debug!("recover_from_wake already in progress - skipping");
        return;
    }

    let mqtt_client = app_handle.state::<AppState>().mqtt_client.clone();
    // Resolve the main window before spawning: the lookup is host-side and
    // always works, but the renderer it fronts may be frozen after a wake.
    let window = app_handle.get_webview_window("main");

    tauri::async_runtime::spawn(async move {
        // Ensure the guard is cleared even if the task panics.
        let _guard = RecoveryGuard;

        // 1. Deterministic MQTT rebuild: synchronously reset the session
        //    state to Connecting (so `wait_for_connected` can never read
        //    the stale pre-sleep Connected value) and request a fresh
        //    EventLoop + AsyncClient.
        {
            let guard = mqtt_client.lock().await;
            if let Some(client) = guard.as_ref() {
                let client = client.lock().await;
                client.recover_after_wake();
                tracing::info!("MQTT deterministic rebuild triggered by system wake");
            }
        }

        // 2. Wait for a real ConnAck. The step-3 webview reload must come
        //    AFTER this so the reloaded frontend boots against a live
        //    connection instead of a Connecting one (see function docs).
        let connected = {
            let guard = mqtt_client.lock().await;
            match guard.as_ref() {
                Some(client) => {
                    client
                        .lock()
                        .await
                        .wait_for_connected(Duration::from_secs(10))
                        .await
                }
                None => false,
            }
        };

        if connected {
            tracing::info!("MQTT reconnected after wake");
        } else {
            tracing::warn!(
                "MQTT not connected within 10s after wake - reloading anyway; frontend converges via poll fallback"
            );
        }

        // 3. Renderer recovery — event-triggered, verified by the page's
        //    own first-frame signal, with retry. A reload issued right
        //    after `detect_resume()` lands while the display/GPU stack is
        //    still coming back and the fresh renderer never composites
        //    (real-world wakes on 2026-09-06: the reload executed, yet
        //    the window stayed acrylic-only until a manual F5). Each
        //    attempt waits for a display-ready *event* (`wake_recovery`)
        //    with a 5 s timeout fallback, reloads, then waits for the new
        //    page to report its first painted frame before declaring
        //    success; a failed verification retries — each retry lands
        //    later, so recovery converges once the compositor is
        //    genuinely usable. See the function docs above.
        const RELOAD_MAX_ATTEMPTS: u32 = 3;
        const DISPLAY_READY_TIMEOUT: Duration = Duration::from_secs(5);
        const FRONTEND_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

        let window = match window {
            Some(w) => w,
            None => {
                tracing::warn!(
                    "No main webview window available after wake - renderer recovery skipped"
                );
                return;
            }
        };

        // The 15 s reload cooldown gates the renderer-recovery phase up
        // front (NOT per attempt): it prevents renderer/GPU process
        // stacking on rapid sleep/wake cycles. A rejection here means a
        // reload already happened very recently (previous wake) — skip
        // recovery. Failed-attempt retries inside this recovery bypass
        // the gate deliberately: their reloads stack nothing (the
        // previous page never displayed) and each retry must land as
        // soon as possible to converge before the user reaches for F5.
        if !reload_cooldown::allowed() {
            tracing::warn!(
                "[wake] reload cooldown active - renderer recovery skipped (recent reload should still be live)"
            );
            return;
        }

        let mut verified = false;
        for attempt in 1..=RELOAD_MAX_ATTEMPTS {
            // 3a. Wait for the display-stack signal (the event-driven
            //     trigger) or the bounded timeout fallback.
            if wake_recovery::wait_display_ready_or_timeout(DISPLAY_READY_TIMEOUT).await {
                tracing::info!(
                    attempt,
                    "[wake] display-ready event observed - reloading webview (renderer recovery)"
                );
            } else {
                tracing::warn!(
                    attempt,
                    "[wake] no display-ready event within {DISPLAY_READY_TIMEOUT:?} - reloading on timeout fallback"
                );
            }

            // 3b. Best-effort recovery flag consumed by `App.tsx` (skip
            //     Splash + re-register the Tauri event listeners that
            //     the reload destroys). The short grace period lets the
            //     eval execute inside a thawing renderer before the
            //     native reload tears the JS context down; on a frozen
            //     renderer the eval never runs and the flag is lost — a
            //     safe degradation (normal boot registers the same
            //     listeners).
            if window
                .eval("sessionStorage.setItem('acowork_recovery_reload', '1');")
                .is_ok()
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Native reload issued by the browser process — forces a
            // fresh renderer even when the old one is frozen; the
            // programmatic equivalent of the user pressing F5.
            let reloaded_at = std::time::Instant::now();
            if let Err(e) = window.reload() {
                tracing::error!(
                    error = %e,
                    "[wake] native webview reload failed (attempt {attempt}) - renderer may stay frozen until a manual F5"
                );
                return;
            }
            reload_cooldown::mark();
            tracing::info!(
                attempt,
                "[wake] webview reloaded - verifying frontend liveness for {FRONTEND_VERIFY_TIMEOUT:?}"
            );

            // 3c. Verification: the reloaded page must prove it actually
            //     displayed — App.tsx (recovery-reload boot path) fires
            //     `desktop_recovery_visible` from a requestAnimationFrame
            //     callback. rAF is driven by the GPU compositor, so it
            //     only fires once a frame was composited after this
            //     reload; thawed old pages and mounted-but-not-composited
            //     pages cannot fake it (see `wake_recovery` docs — the
            //     heartbeat signal it replaced was a false positive).
            let verify_deadline = std::time::Instant::now() + FRONTEND_VERIFY_TIMEOUT;
            while std::time::Instant::now() < verify_deadline {
                if wake_recovery::frontend_visible_since(reloaded_at) {
                    verified = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }

            if verified {
                tracing::info!(
                    "[wake] first frame reported after reload - renderer recovery complete"
                );
                break;
            }
            tracing::warn!(
                attempt,
                "[wake] no first-frame signal within {FRONTEND_VERIFY_TIMEOUT:?} after reload - retrying (attempt {attempt}/{RELOAD_MAX_ATTEMPTS})"
            );
        }

        if !verified {
            tracing::error!(
                "[wake] renderer recovery unverified after {RELOAD_MAX_ATTEMPTS} attempts - a manual reload (F5) may be required"
            );
        }
    });
}

/// RAII guard that clears `RECOVERY_IN_PROGRESS` when dropped.
struct RecoveryGuard;

impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        RECOVERY_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// Initialize tracing for the Desktop Rust backend.
///
/// Writes to a rolling file in the Gateway's data/logs directory
/// (so all ACowork logs are co-located) and to stderr in dev builds.
///
/// Without this, all `tracing::info!`/`tracing::warn!` calls in the
/// Desktop Rust code are silent no-ops, making runtime debugging
/// impossible without ad-hoc `eprintln!` additions.
fn init_desktop_tracing() {
    use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,acowork_desktop=debug"));

    // Desktop writes to its OWN log directory, independent of the
    // Gateway.  The Gateway may be remote; even when local, Desktop
    // must not write into the Gateway's data tree.  Uses the same
    // `~/.acowork` root so all ACowork artifacts are co-located.
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    #[cfg(not(windows))]
    let home = std::env::var("HOME").unwrap_or_default();

    let log_dir = std::path::PathBuf::from(home)
        .join(".acowork")
        .join("desktop-app")
        .join("logs");

    let file_appender = match acowork_core::logging::SizeRollingFileAppender::new(
        log_dir.clone(),
        5, // 5 MB per file
        3, // keep 3 rotated files
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("WARN: failed to create file appender: {}; falling back to stderr-only", e);
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .init();
            return;
        }
    };

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false)
        .with_target(true)
        .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339());

    #[cfg(debug_assertions)]
    let stderr_layer = Some(
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339()),
    );
    #[cfg(not(debug_assertions))]
    let stderr_layer: Option<tracing_subscriber::fmt::Layer<_>> = None;

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    acowork_core::logging::install_panic_hook();
    tracing::info!("Desktop tracing initialized (log_dir={:?})", log_dir);
}

pub fn run() {
    init_desktop_tracing();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Focus the main window when a second instance is launched
            let _ = app
                .get_webview_window("main")
                .expect("no main window")
                .set_focus();
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        // Last-resort navigation firewall: blocks any webview navigation
        // that is not the app's own root URL or an allow-listed scheme.
        // See `navigation_guard.rs` for the allow-list rationale.
        .plugin(navigation_guard::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::agent::list_agents,
            commands::agent::get_agent_detail,
            commands::agent::install_agent,
            commands::agent::install_bundled_agent,
            commands::agent::wait_agent_installed,
            commands::agent::uninstall_agent,
            commands::agent::start_agent,
            commands::agent::stop_agent,
            commands::agent::restart_agent_in_debug,
            commands::agent::clone_agent,
            commands::agent::update_agent_manifest_avatar,
            commands::agent::upload_agent_file,
            commands::agent::upload_user_avatar_file,
            commands::chat::upload_file,
            commands::vault::list_keys,
            commands::vault::add_key,
            commands::vault::remove_key,
            commands::vault::update_key,
            commands::vault::update_account_key,
            commands::publish::prepare_publish,
            commands::publish::build_publish,
            commands::publish::export_package,
            commands::create::create_agent,
            commands::create::preview_manifest,
            commands::create::list_builtin_tools,
            commands::create::parse_skill_zip_preview,
            commands::gateway::set_gateway_config,
            commands::gateway::get_gateway_config,
            commands::gateway::set_gateway_access_token,
            commands::gateway::init_local_gateway,
            commands::gateway::start_local_gateway,
            // ADR-087: "Create local Node" — Desktop-spawned node bound
            // to the signed-in user (token minted internally, invisible).
            commands::gateway::create_local_node,
            commands::gateway::local_node_status,
            commands::gateway::inspect_local_node_process,
            commands::gateway::stop_local_node_process,
            commands::gateway::stop_local_gateway,
            commands::gateway::get_local_gateway_status,
            // ADR-059: latest Gateway bootstrap snapshot (MQTT cache + HTTP
            // fallback).
            commands::gateway::get_bootstrap,
            commands::effects::set_window_effect,
            // ADR-048 D6: Debug Protocol RPC relay (HTTP via Gateway)
            commands::debug::debug_rpc,
            // ADR-048 follow-up: runtime DevMode activation (no agent restart).
            commands::debug::enable_agent_debug,
            // ADR-048 follow-up: symmetric counterpart to the enable
            // command — tears DevMode down at runtime so the user can
            // exit the Debug Panel without stopping + restarting the
            // agent.
            commands::debug::disable_agent_debug,
            // ADR-033 Phase 3: MQTT real-time event commands
            commands::chat_mqtt::connect_mqtt,
            commands::chat_mqtt::disconnect_mqtt,
            commands::chat_mqtt::force_reconnect_mqtt,
            commands::chat_mqtt::get_mqtt_status,
            // ADR-XXX: MQTT broker debug controls (status bar test buttons)
            commands::gateway::debug_mqtt_shutdown,
            commands::gateway::debug_mqtt_start,
            // Local-mode "Reveal in File Explorer" — opens the OS file
            // manager with the entry selected. Only meaningful when the
            // Desktop App and Gateway share a machine (local mode); the
            // frontend hides the corresponding menu item in remote mode.
            commands::reveal::reveal_in_file_explorer,
            // Inbox attachment download — see commands::save_attachment.
            // Tauri's WebView doesn't honor Content-Disposition / <a
            // download>, so the frontend shows the OS save dialog and
            // hands this command the attachment URL plus that path; Rust
            // performs the transfer (never the webview — see the module
            // docs for why bytes must not travel over IPC).
            commands::save_attachment::download_attachment,
            // OS clipboard file-path fallback for paste/upload. Returns
            // the absolute paths of any files on the clipboard so the
            // chat panel can upload files copied from the OS file manager
            // (WebView2 doesn't expose paths in ClipboardEvent).
            commands::clipboard::get_clipboard_file_paths,
            // Persist a clipboard image Blob to a temp file so the
            // path-based `upload_file` pipeline can pick it up. Bridges
            // the gap where screenshot tools put CF_BITMAP on the
            // clipboard (no CF_HDROP, no text/plain) — see
            // `commands::clipboard::save_clipboard_blob` docs.
            commands::clipboard::save_clipboard_blob,
            // Pre-flight file-size lookup so the chat panel can reject
            // oversized attachments (≥ 50 MiB runtime cap) BEFORE the
            // multipart roundtrip. See `commands::chat::get_file_size`.
            commands::chat::get_file_size,
            // Post-wake renderer recovery: page reports its first
            // painted frame (rAF-driven). See `wake_recovery` docs.
            desktop_recovery_visible,
            // TEMP DEBUG: <a> click probe — see src/main.tsx. Delete once
            // the link-click webview-crash root cause is confirmed.
            debug_log_link_click,
            // TEMP DEBUG: frontend logger bridge (mqtt reconnect
            // investigation). See `fe_log` above.
            fe_log,
            // Connectivity guard: this machine's own IPv4 addresses.
            commands::netinfo::get_local_ipv4_addresses,
        ])
        .setup(|app| {
            tray::setup(app)?;

            // ADR-076 §决策 3: give the Gateway client's account-session
            // mirror the webview handle it emits `gateway-auth-required` on.
            app.state::<AppState>()
                .gateway_auth
                .install_app_handle(app.handle().clone());

            // ── macOS vibrancy ────────────────────────────────────────────
            // The initial NSVisualEffectView material is now applied by
            // the frontend via the set_window_effect Tauri command (see
            // commands/effects.rs and AppLayout.tsx).  The frontend picks
            // the correct material (Effect::Dark or UnderWindowBackground)
            // based on the effective theme before the window is shown
            // (window starts with visible:false in tauri.conf.json), so
            // there is no flash and no race between the Rust setup retry
            // loop and the frontend's theme-aware effect.
            //
            // We intentionally do NOT apply any effect here — doing so
            // with UnderWindowBackground in a delayed retry loop races
            // with the frontend's set_window_effect call and can clobber
            // the dark-mode material, causing the window to appear whitish
            // at low opacity.

            // ── Windows acrylic blur ──────────────────────────────────────
            // Apply DWM Acrylic so the desktop shows through the transparent
            // window with a native blur.  Without this the WebView2 has
            // nothing for CSS `backdrop-filter` to blur on Windows — the
            // browser's stacking context ends at the transparent body and
            // there is no rendered content behind the root element to blur.
            //
            // Acrylic requires Windows 10+; on older Windows Tauri logs the
            // error and the window falls back to a plain transparent surface.
            // `radius` is ignored for Acrylic (system-controlled) but kept
            // for parity with the pre-c8f031a frontend `setEffects` call.
            //
            // `color` provides a subtle neutral tint that blends with the
            // acrylic backdrop, reducing the jarring transparency gap when
            // the window is resized and WebView2 content lags behind DWM.
            #[cfg(target_os = "windows")]
            {
                use tauri::utils::config::WindowEffectsConfig;
                use tauri::window::EffectState;

                let main_window = app.get_webview_window("main").expect("no main window");
                let effects = WindowEffectsConfig {
                    effects: vec![tauri::window::Effect::Acrylic],
                    state: Some(EffectState::Active),
                    radius: Some(12.0),
                    color: Some((128, 128, 128, 30).into()),
                    // macOS 27+ Liquid Glass only; irrelevant for Windows Acrylic.
                    interactive: false,
                };
                let _ = main_window.set_effects(effects);
            }

            // ── Disable native decorations ──────────────────────────────
            // set_decorations(false) removes the title bar on Linux and Windows.
            // macOS uses native traffic lights with titleBarStyle: Overlay
            // (configured in tauri.conf.json), so decorations stay On.
            #[cfg(not(target_os = "macos"))]
            {
                let main_window = app.get_webview_window("main").expect("no main window");
                let _ = main_window.set_decorations(false);
            }

            // ── Windows: restore WS_SYSMENU | WS_MINIMIZEBOX ─────────
            // set_decorations(false) strips WS_SYSMENU and WS_MINIMIZEBOX,
            // which breaks taskbar button behavior:
            //   • Explorer needs WS_SYSMENU → system menu for right-click
            //   • Explorer needs WS_MINIMIZEBOX → minimize/restore on left-click
            // Without these, Explorer falls back to showing the system menu
            // on both left-click and right-click.  We put them back into the
            // window style and force a fresh system menu so the taskbar
            // button behaves correctly.  The title bar stays hidden because
            // decorations are already disabled.
            //
            // A WndProc subclass (win_wndproc.rs) catches any remaining
            // SC_MOUSEMENU/SC_KEYMENU that Explorer might send as fallback.
            #[cfg(target_os = "windows")]
            {
                use std::ffi::c_void;

                unsafe extern "system" {
                    fn GetWindowLongPtrW(h: *mut c_void, n: i32) -> isize;
                    fn SetWindowLongPtrW(h: *mut c_void, n: i32, v: isize) -> isize;
                    fn SetWindowPos(
                        h: *mut c_void,
                        insert_after: *mut c_void,
                        x: i32, y: i32, cx: i32, cy: i32,
                        flags: u32,
                    ) -> i32;
                    fn GetSystemMenu(h: *mut c_void, b: i32) -> *mut c_void;
                }

                const GWL_STYLE: i32 = -16;
                const WS_SYSMENU: isize = 0x0008_0000;
                const WS_MINIMIZEBOX: isize = 0x0002_0000;
                const SWP_FRAMECHANGED: u32 = 0x0020;
                const SWP_NOMOVE: u32 = 0x0002;
                const SWP_NOSIZE: u32 = 0x0001;
                const SWP_NOZORDER: u32 = 0x0004;
                const SWP_NOACTIVATE: u32 = 0x0010;

                let main_window = app.get_webview_window("main").expect("no main window");
                if let Ok(hwnd) = main_window.hwnd() {
                    let raw = hwnd.0;
                    unsafe {
                        let style = GetWindowLongPtrW(raw, GWL_STYLE);
                        let needed = WS_SYSMENU | WS_MINIMIZEBOX;
                        if style & needed != needed {
                            SetWindowLongPtrW(raw, GWL_STYLE, style | needed);
                            // SWP_FRAMECHANGED triggers WM_NCCALCSIZE to recalc the
                            // non-client area, which DWM uses to render the frame.
                            // SWP_NOACTIVATE prevents focus change.
                            SetWindowPos(
                                raw, std::ptr::null_mut(),
                                0, 0, 0, 0,
                                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                            // TRUE → force fresh copy of default system menu
                            GetSystemMenu(raw, 1);
                            tracing::info!(
                                "Restored WS_SYSMENU|WS_MINIMIZEBOX + rebuilt system menu"
                            );
                        }
                    }
                }
            }

            // ── Windows: WndProc subclass for taskbar left-click ────
            #[cfg(target_os = "windows")]
            {
                let main_window = app.get_webview_window("main").expect("no main window");
                if let Ok(hwnd) = main_window.hwnd() {
                    let _ = unsafe { crate::win_wndproc::install(hwnd.0) };
                }
            }

            // ── macOS: NSWorkspace wake notification observer ───────────
            // The macOS analogue of Windows' GUID_MONITOR_POWER_ON:
            // NSWorkspace posts `NSWorkspaceDidWakeNotification` to its
            // notification center on every system wake, and we feed
            // that into the same `wake_recovery::signal_display_ready()`
            // trigger so the post-wake webview reload converges on the
            // same code path as Windows. Installed synchronously here
            // (Tauri's setup hook runs on the main thread, which is
            // where NSWorkspace / NSNotificationCenter expect to be
            // accessed). See `macos_workspace` for the threading
            // contract and failure-mode semantics.
            #[cfg(target_os = "macos")]
            {
                crate::macos_workspace::install();
            }

            // ── Linux: logind PrepareForSleep(false) edge observer ─────
            // The Linux analogue of the same event: systemd-logind
            // broadcasts `PrepareForSleep(b)` on the system bus, with
            // the boolean set to false right after a wake. We
            // subscribe via zbus on the global tokio runtime (the
            // same one the power probe loop and the MQTT client
            // already use) and feed the false edge into
            // `wake_recovery::signal_display_ready()`. Spawned in its
            // own task so the synchronous setup hook does not block
            // on D-Bus connect; failure logs and falls through to
            // the `Focused(true)` + 5 s timeout path that already
            // exists for every platform. See `linux_logind` for the
            // D-Bus path / signal / message body details.
            #[cfg(target_os = "linux")]
            {
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = crate::linux_logind::install().await {
                        tracing::warn!(
                            "Linux logind wake observer not installed: {e} - post-wake display \
                             signal falls back to Focused(true) + 5s timeout (recover_from_wake \
                             still converges)"
                        );
                    }
                });
            }

            // ── Linux: ScreenSaver ActiveChanged(false) edge observer ────
            // Covers the display-sleep gap that logind's
            // `PrepareForSleep` cannot see: a pure display sleep (DPMS
            // off, system still running) broadcasts no logind signal,
            // yet the webview compositor can stall on the wake edge
            // exactly like a system wake. The DE's screen-saver service
            // (GNOME / KDE / xscreensaver / ...) emits
            // `org.freedesktop.ScreenSaver.ActiveChanged(false)` on the
            // session bus when the display comes back; we feed that
            // into the same `wake_recovery::signal_display_ready()`
            // trigger. Best-effort: DEs without the interface simply
            // never signal, and the existing logind + Focused(true)
            // paths are unaffected. See `linux_screensaver` for the
            // D-Bus path / signal / message body details.
            #[cfg(target_os = "linux")]
            {
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = crate::linux_screensaver::install().await {
                        tracing::warn!(
                            "Linux ScreenSaver display-wake observer not installed: {e} - \
                             display-sleep recovery falls back to Focused(true) + 5s timeout \
                             (recover_from_wake still converges)"
                        );
                    }
                });
            }

            // Spawn async task for automatic sleep detection.

            // Polls biased/unbiased monotonic clocks every 2 s via the
            // shared `acowork_mqtt_session::power::run_power_probe_loop`
            // (ADR-065 Step 1 — same loop as Node / Runtime).  On

            // detecting real sleep, `recover_from_wake` deterministically
            // rebuilds the MQTT connection (the OS tears down TCP sockets
            // during sleep).  The `Focused(true)` handler below provides
            // immediate detection when the user clicks the window.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                acowork_mqtt_session::power::run_power_probe_loop(
                    move || recover_from_wake(&app_handle),
                    acowork_mqtt_session::power::POWER_PROBE_INTERVAL,
                    "desktop",
                )
                .await;
            });

            // NOTE: The local Gateway is no longer spawned here. The frontend
            // is the source of truth for gateway configuration (mode + URL,
            // persisted in its settingsStore). On startup it pushes that into
            // Rust via `set_gateway_config`, then calls `init_local_gateway`
            // if mode == local. See module-level docs above.

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                // ── System-resume detection ────────────────────────────────
                // Compares biased vs unbiased monotonic clocks to detect
                // *actual* system sleep — not merely window minimise/restore.
                // Detection lives in the shared `acowork-mqtt-session` crate
                // (ADR-065 Step 1); see the module docs above for platform
                // details.
                tauri::WindowEvent::Focused(true) => {
                    // The main window gained focus — the user sees and can
                    // interact with the window, so the display stack is
                    // usable again. Feed this into wake recovery as the
                    // cross-platform display-ready signal (Windows also
                    // has GUID_MONITOR_POWER_ON / WM_DISPLAYCHANGE via
                    // win_wndproc). After a wake, Windows restores focus
                    // to the previously focused window exactly when the
                    // display comes back, which is the moment a manual F5
                    // starts working — the same semantic on macOS/Linux.
                    // See `wake_recovery` docs for why the signal is
                    // freshness-checked, so ordinary clicks outside a
                    // recovery window are harmless.
                    crate::wake_recovery::signal_display_ready();
                    if acowork_mqtt_session::power::detect_resume() {
                        recover_from_wake(window.app_handle());
                    }
                }

                // ── OS-level file drop forwarding ─────────────────────────
                // Tauri v2 captures OS file drag-drop at the Rust layer
                // (`WindowEvent::DragDrop`) because WebView HTML5 drop
                // events do NOT expose real filesystem paths in the
                // sandboxed file object — only `name`/`size`/`type`.
                //
                // We re-emit the absolute paths to the frontend on a
                // private event channel. `ChatPanel` listens, checks
                // `document.activeElement` to know if the drop landed on
                // the chat textarea, and dispatches to `upload_file` (the
                // same pipeline as the paperclip button).
                //
                // Position is reported in physical pixels; the frontend
                // multiplies by `window.devicePixelRatio` is unnecessary
                // because we don't use position to find the target —
                // activeElement is sufficient.
                tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) => {
                    let path_strings: Vec<String> = paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect();
                    if path_strings.is_empty() {
                        return;
                    }
                    tracing::info!(
                        "[lib.rs] OS file drop received, forwarding {} path(s) to frontend",
                        path_strings.len()
                    );
                    let _ = window.emit("desktop://file-drop", path_strings);
                }

                // ── Hide to tray instead of closing ──────────────────────────
                // Only intercept close when window is visible.
                // This prevents interference with system tray menu on Windows.
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    match window.is_visible() {
                        Ok(true) => {
                            tracing::debug!("Intercepting close request, hiding to tray");
                            window.hide().unwrap();
                            api.prevent_close();
                        }
                        Ok(false) => {
                            tracing::debug!("Window not visible, allowing close to proceed");
                            // Don't intercept - let it close (for Quit menu)
                        }
                        Err(e) => {
                            tracing::warn!("Failed to check window visibility: {}", e);
                            // Safe default: allow close
                        }
                    }
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        // ── Cleanup: Stop local Gateway process tree on exit ──────────
        // Covers Ctrl+C (dev mode), tray quit, and OS shutdown.
        // On Windows, uses taskkill /T /F to kill Gateway + all children
        // (Runtime, Embed) in one shot. On Unix, sends SIGINT for clean
        // shutdown via Gateway's own signal handler.
        //
        // Exit policy: if the user chose "quit, keep Gateway running" in
        // the tray-quit dialog (`gateway_keep_running_on_exit`), we leave
        // the child process alone — it becomes an independent Gateway that
        // the next Desktop run (or remote peers) will adopt.
        if matches!(
            event,
            tauri::RunEvent::Exit
                | tauri::RunEvent::ExitRequested { .. }
        ) {
            let state = app_handle.state::<AppState>();
            if state.gateway_keep_running_on_exit.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!(
                    "App exiting with keep-running policy — leaving Gateway process alive"
                );
            } else {
                let gateway_handle = state.gateway_process.clone();
                // try_lock: if the mutex is held by an inflight init_local_gateway,
                // that command will store the child and this handler won't see it,
                // but the next exit attempt will catch it. This is non-blocking
                // because RunEvent::Exit fires in the main thread context.
                if let Ok(mut proc) = gateway_handle.try_lock()
                    && let Some(mut child) = proc.take()
                {
                    let pid = child.id();
                    tracing::info!(pid = pid, "App exiting, stopping Gateway process tree");
                    process_stop::stop_gateway_process(pid);
                    let _ = child.wait();
                }
            }
        }

        // Handle dock icon click on macOS.
        //
        // When the window is hidden to tray, clicking the dock icon fires
        // RunEvent::Reopen.  We show the window and focus it.
        #[cfg(target_os = "macos")]
        {
            if let tauri::RunEvent::Reopen { .. } = event
                && let Some(window) = app_handle.get_webview_window("main")
            {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }

        // On non-macOS platforms there are no special run events to handle.
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (app_handle, event);
        }
    });
}

// TEMP DEBUG: webview <a> click probe — see apps/acowork-desktop/src/main.tsx.
// Delete once the link-click webview-crash root cause is confirmed.
#[tauri::command]
fn debug_log_link_click(payload: String) {
    tracing::warn!(target: "webview_link_click", "{}", payload);
}

// TEMP DEBUG: frontend log bridge — the React-side `log.warn/error`
// calls forward their message here so frontend diagnostics land in the
// desktop log FILE instead of only the WebView devtools console. Added
// for the "effectiveConnection stuck at reconnecting" investigation;
// remove once that bug is closed.
#[tauri::command]
fn fe_log(level: String, msg: String) {
    match level.as_str() {
        "error" => tracing::error!(target: "fe", "{}", msg),
        _ => tracing::warn!(target: "fe", "{}", msg),
    }
}
