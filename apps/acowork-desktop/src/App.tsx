import { useState, useEffect } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import { AppLayout } from "./components/layout/AppLayout";
import { SplashScreen } from "./components/layout/SplashScreen";
import { OnboardingFlow } from "./components/onboarding/OnboardingFlow";
import { LoginView } from "./components/account/LoginView";
import { SetupRequiredView } from "./components/account/SetupRequiredView";
import { ToastProvider } from "./components/common/ToastProvider";
import { ImageLightboxHost } from "./components/common/ImageLightbox";
import { ErrorBoundary } from "./components/common/ErrorBoundary";
import { useAuthStore } from "./stores/authStore";
import { initMqttListener } from "./stores/chatStore";
import { initWorkspaceFsListener } from "./lib/workspaceFsEvents";
import { initDocTreeChangeListener } from "./lib/docFsEvents";
import { useGatewayStore } from "./stores/gatewayStore";
import { useSettingsStore } from "./stores/settingsStore";
import { useConnectionStore } from "./stores/connectionStore";
import { log } from "./lib/logger";
import { isRecoveryReload, clearRecoveryReload } from "./lib/recoveryReload";
import { initGatewayConnectivity } from "./lib/connectivity/gatewayConnectivity";

function App() {
  // On sleep-recovery reload, skip splash screen — gateway is already running
  // (Rust backend survives reload) and Zustand persisted stores restore from
  // localStorage, so we can jump straight to AppLayout. The value is cached at
  // module scope (see lib/recoveryReload.ts) so a StrictMode remount cannot
  // re-read the flag after the effect below clears it.
  log.debug("[App] boot branch selection", { isRecoveryReload });

  const [onboardingDone, setOnboardingDone] = useState(() => {
    return localStorage.getItem("acowork_onboarding") === "completed";
  });

  // ADR-115: the boot gate lives in a store, not local state. Changing the
  // server address from the login / setup screen calls `restart()`, which
  // drops `ready` and bumps `epoch` so a fresh SplashScreen boot runs
  // against the new address (including auto-spawning a local Gateway when
  // the picked address is localhost). A terminal local `useState(true)` was
  // the reason a wrong address could never be escaped.
  const gatewayReady = useConnectionStore((s) => s.ready);
  const bootEpoch = useConnectionStore((s) => s.epoch);

  // ADR-114: resolve the account state once the Gateway is reachable, and
  // restore a stored session. Runs for the recovery-reload path too.
  const [authProbed, setAuthProbed] = useState(false);
  useEffect(() => {
    if (!gatewayReady) return;
    setAuthProbed(false);
    void useAuthStore
      .getState()
      .init()
      .finally(() => setAuthProbed(true));
  }, [gatewayReady, bootEpoch]);

  const authStatus = useAuthStore((s) => s.status);

  // Clear the recovery flag after mount so it doesn't affect future loads.
  // Also re-register the MQTT listener: recovery reload skips SplashScreen
  // (gateway is already running), but the webview reload destroyed all
  // Tauri event listeners. Without re-registering, chatStore.mqttConnected
  // stays false forever and the UI permanently shows "Connecting to agent".
  useEffect(() => {
    if (isRecoveryReload) {
      clearRecoveryReload();
      initMqttListener().catch((e) =>
        log.warn("[App] initMqttListener failed on recovery reload:", e)
      );
      // ADR-058: workspace fs-changed listeners die with the webview
      // reload — re-register alongside the MQTT listener.
      initWorkspaceFsListener().catch((e) =>
        log.warn("[App] initWorkspaceFsListener failed on recovery reload:", e)
      );
      // Doc tree-change listener (same webview-reload death).
      initDocTreeChangeListener().catch((e) =>
        log.warn("[App] initDocTreeChangeListener failed on recovery reload:", e)
      );
      // Post-wake renderer recovery: report the first painted frame to
      // the Rust backend. requestAnimationFrame is driven by the GPU
      // compositor — it only fires once a frame was actually composited,
      // so this is the page's own "the UI is truly visible again" signal
      // that `recover_from_wake` verifies against (heartbeat-based
      // verification was a false positive: a thawed old page's catch-up
      // heartbeat landed right after the async reload call). If the
      // compositor is still coming back, the rAF callback is deferred
      // until it recovers, and the backend's verify window catches it.
      requestAnimationFrame(() => {
        invoke("desktop_recovery_visible").catch((e) =>
          log.warn("[App] desktop_recovery_visible invoke failed:", e)
        );
      });
    }
  }, [isRecoveryReload]);

  // Unified connectivity handling — boot fallback, runtime unreachable
  // hint, wake/Wi-Fi auto-heal all live in ONE module so the splash,
  // the runtime UI and the sleep/wake paths never re-implement the
  // logic (see lib/connectivity/gatewayConnectivity.ts). Root-mounted,
  // so it also covers the splash phase.
  useEffect(() => initGatewayConnectivity(), []);

  // Show the window after first render. The window starts hidden (visible:false
  // in tauri.conf.json) so the user never sees the empty/transparent window or
  // the decoration flicker that occurs before React mounts. By the time this
  // effect fires, SplashScreen / OnboardingFlow / AppLayout is already painted.
  useEffect(() => {
    const showWindow = async () => {
      try {
        const win = getCurrentWindow();
        await win.show();
        await win.setFocus();
      } catch (e) {
        log.error("Failed to show window:", e);
      }
    };
    showWindow();
  }, []);

  // Connection lifecycle → gateway URL history. Record ONLY on a genuine
  // CONNECTED transition: a URL the user typed but never reached must never
  // become a "known-good" address (that was how a wrong remote address kept
  // winning the next boot).
  useEffect(() => {
    const unsub = useGatewayStore.subscribe((state, prev) => {
      if (state.status === "connected" && prev.status !== "connected") {
        useSettingsStore.getState().recordGatewayUrl(
          useSettingsStore.getState().gatewayUrl,
        );
      }
    });
    return unsub;
  }, []);

  // NOTE: a steady-state drop (laptop woke from sleep / switched Wi-Fi)
  // used to fire a background candidate probe from here, so the title-bar
  // outage chip could list reachable addresses before the user clicked it.
  // That chip is gone: the outage now surfaces on the gateway switcher
  // button itself, and that menu probes the history when opened. Nothing
  // is lost — the probe runs when the user asks for it instead of eagerly
  // burning the network on a state they may never look at.

  // Gateway boot is the first stage of EVERY topology — a fresh install
  // included. It used to be gated on `onboardingDone`, which broke the
  // "Settings → reset onboarding" path: that action only drops the
  // localStorage flag, so with the gate in place SplashScreen never mounted,
  // `gatewayReady` stayed false and `authStore.init()` never ran — the profile
  // tab kept reporting "no profile" no matter what the wizard wrote.
  //
  // `bootGateway` is the only path that pushes the Gateway config into Rust,
  // spawns/adopts a local Gateway, ensures the System Agent and connects MQTT.
  // OnboardingFlow's GatewayStep reuses the result: it finds the Gateway
  // already connected and only offers the mode/URL switch.
  if (!gatewayReady) {
    return (
      <div className="h-screen w-screen overflow-hidden">
        <SplashScreen
          key={bootEpoch}
          onReady={() => useConnectionStore.getState().markReady()}
        />
      </div>
    );
  }

  return (
    <ErrorBoundary>
      <ToastProvider>
        {/* Chat-attachment image previews open from a plain module
            (`lib/openChatAttachment.ts`), so the overlay is hosted once above
            the shell instead of inside a virtualized chat row. */}
        <ImageLightboxHost />
        {authStatus === "logged_out" ? (
          <LoginView />
        ) : authStatus === "setup_required" ? (
          <SetupRequiredView />
        ) : authStatus === "unknown" && !authProbed ? (
          // Auth probe in flight: the deployment mode has not resolved, or a
          // stored session's `/api/auth/me` has not answered yet. Show a bare
          // surface rather than flashing the main UI before LoginView. Once
          // the probe settles, an `unknown` status can only mean the Gateway
          // never answered `/api/status` — that falls through below instead of
          // hanging on a blank screen.
          <div className="flex h-screen w-screen items-center justify-center bg-page-bg" />
        ) : !onboardingDone ? (
          // ADR-076 §决策 7: the account comes first — with the always-on account system the
          // wizard PATCHES the signed-in account, so it must not run before
          // the login gate.
          <OnboardingFlow onComplete={() => setOnboardingDone(true)} />
        ) : (
          <AppLayout />
        )}
      </ToastProvider>
    </ErrorBoundary>
  );
}

export default App;
