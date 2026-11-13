/**
 * Gateway connectivity module — the ONE place network-connection
 * problems are handled for every UI scenario.
 *
 * Scenarios and how they all funnel through here:
 *   - boot (SplashScreen): candidate fallback after `UNREACHABLE_HINT_MS`
 *     via the shared `probeKnownCandidates` (same budget as the runtime
 *     hint — boot and runtime use one number).
 *   - runtime (TitleBar chip): `gatewayUnreachable` — any non-connected
 *     state for `UNREACHABLE_HINT_MS`, whatever the cause (black-holed
 *     path, refused connection, dead process) — then the same candidate
 *     probe runs immediately.
 *   - sleep/wake & Wi-Fi hop: `visibilitychange` / `online` triggers and
 *     a 5 s tick while disconnected drive `maybeAutoSwitchLocalGateway`
 *     (a local gateway silently re-points at loopback; a true remote
 *     gateway falls through to hint + candidates).
 *
 * Authority model (unchanged, see gatewayStore header): MQTT CONNACK is
 * the liveness authority; `/health` stays a subordinate death
 * classifier for diagnostics. This module only *derives* user-facing
 * signals — it never invents liveness.
 *
 * `initGatewayConnectivity()` is called once from App.tsx (root, always
 * mounted — this also covers the splash phase). It returns a cleanup so
 * StrictMode double-mounts and tests stay deterministic.
 */
import { probeGateways } from "../gateway-probe";
import { gatewayAddressOptions } from "../gatewayUrl";
import { log } from "../logger";
import { useChatStore } from "../../stores/chatStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { maybeAutoSwitchLocalGateway } from "./localNetwork";

/** User-facing unreachable budget: a gateway that is not CONNACKed for
 *  this long is surfaced as unreachable — hint + candidate list — no
 *  matter why. A black-holed path is as actionable for the user as a
 *  dead process. One number for boot (splash fallback) and runtime. */
export const UNREACHABLE_HINT_MS = 5_000;

/** Tick cadence for the local-network auto-heal while disconnected. */
const LOCAL_GUARD_INTERVAL_MS = 5_000;

/** Pending unreachable-hint countdown (single, module-owned). */
let _hintTimer: ReturnType<typeof setTimeout> | null = null;

/** True when the snapshot says we are back: MQTT authority first, the
 *  display `status` as a belt-and-braces second. */
function isRecovered(): boolean {
  return (
    useChatStore.getState().mqttConnected ||
    useGatewayStore.getState().status === "connected"
  );
}

/** MQTT authority → unreachable hint debounce (see module header). */
function syncMqttLiveness(connected: boolean): void {
  if (connected) {
    if (_hintTimer !== null) {
      clearTimeout(_hintTimer);
      _hintTimer = null;
    }
    if (useGatewayStore.getState().gatewayUnreachable) {
      useGatewayStore.setState({ gatewayUnreachable: false });
    }
    return;
  }
  // Already surfaced or already counting down — idempotent.
  if (_hintTimer !== null || useGatewayStore.getState().gatewayUnreachable) return;
  _hintTimer = setTimeout(() => {
    _hintTimer = null;
    if (isRecovered()) return;
    useGatewayStore.setState({ gatewayUnreachable: true });
    log.warn(
      `[gateway-connectivity] MQTT down for ${UNREACHABLE_HINT_MS}ms — gateway unreachable (hint + candidates)`,
    );
  }, UNREACHABLE_HINT_MS);
}

/**
 * Probe the URL history (minus the current URL) and fold reachable
 * hosts into `gatewayStore.candidates` as each settles — the single
 * candidate probe for BOTH the splash fallback and the runtime chip.
 * Returns the number of candidates added (0 when skipped: already
 * recovered, or empty history).
 *
 * ADR-115: no local-mode short-circuit. A loopback address is just one
 * entry in the list, so a Desktop sitting on a dead LAN address must
 * still be offered localhost as an alternative — that is precisely the
 * escape route the old `if (gatewayMode === "local") return 0;` denied.
 */
export async function probeKnownCandidates(): Promise<number> {
  if (isRecovered()) return 0;
  const { gatewayUrl, gatewayUrlHistory, lastGoodUrl } = useSettingsStore.getState();
  const others = gatewayAddressOptions(gatewayUrl, gatewayUrlHistory, lastGoodUrl).filter(
    (u) => u !== gatewayUrl,
  );
  if (others.length === 0) {
    useGatewayStore.getState().setCandidates([]);
    return 0;
  }
  let added = 0;
  await probeGateways(others, {
    onSettled: (r) => {
      if (!r.ok || isRecovered()) return;
      added += 1;
      useGatewayStore.getState().setCandidates((prev) =>
        prev.some((c) => c.url === r.url)
          ? prev
          // Fastest first — a slow-but-reachable host must never
          // outrank a fast one that landed later.
          : [...prev, { url: r.url, latencyMs: r.latencyMs }].sort(
              (a, b) => a.latencyMs - b.latencyMs,
            ),
      );
    },
  });
  // Everything that answered is already in; this only matters when a
  // host settled after we stopped caring (recovered mid-probe).
  if (isRecovered()) useGatewayStore.getState().setCandidates([]);
  return added;
}

/**
 * Initialize the module once per app mount (App.tsx root effect).
 * Returns a cleanup that removes every listener/subscription/timer it
 * owns.
 */
export function initGatewayConnectivity(): () => void {
  // ── 1) MQTT authority → unreachable hint + local-network tick ──
  let guardHandle: ReturnType<typeof setInterval> | null = null;
  const tickLocalGuard = () => {
    void maybeAutoSwitchLocalGateway();
  };
  const syncMqtt = (connected: boolean) => {
    syncMqttLiveness(connected);
    if (connected) {
      if (guardHandle !== null) {
        clearInterval(guardHandle);
        guardHandle = null;
      }
      // One pass while healthy records the "host is local" hint for a
      // later network change.
      tickLocalGuard();
    } else if (guardHandle === null) {
      guardHandle = setInterval(tickLocalGuard, LOCAL_GUARD_INTERVAL_MS);
      tickLocalGuard();
    }
  };
  syncMqtt(useChatStore.getState().mqttConnected);
  const unsubMqtt = useChatStore.subscribe((s, p) => {
    if (s.mqttConnected !== p.mqttConnected) syncMqtt(s.mqttConnected);
  });

  // ── 2) Candidates belong to the outage: a rise dismisses them ──
  // (covers the splash phase too — this module is initialized at root).
  const unsubGateway = useGatewayStore.subscribe((s, p) => {
    if (s.status === "connected" && p.status !== "connected") {
      useGatewayStore.getState().setCandidates([]);
    }
  });

  // ── 3) Sleep/wake & Wi-Fi hop triggers ──
  const onVisibility = () => {
    if (document.visibilityState === "visible") tickLocalGuard();
  };
  document.addEventListener("visibilitychange", onVisibility);
  window.addEventListener("online", tickLocalGuard);

  return () => {
    if (_hintTimer !== null) {
      clearTimeout(_hintTimer);
      _hintTimer = null;
    }
    if (guardHandle !== null) {
      clearInterval(guardHandle);
      guardHandle = null;
    }
    unsubMqtt();
    unsubGateway();
    document.removeEventListener("visibilitychange", onVisibility);
    window.removeEventListener("online", tickLocalGuard);
  };
}
