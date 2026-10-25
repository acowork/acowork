import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import type { HealthResponse, GatewayStatus, LocalGatewayState, GatewayOwnership, GatewayBootResult, AgentMigrationProgress, MigrationAgentEntry, MemoryServiceStatus, SystemStatusResponse } from "../lib/types";
import { fetchMigrationProgress } from "../lib/gateway-api";
import { log } from "../lib/logger";

// ── Gateway liveness: single-authority model ─────────────────────────
//
// The MQTT CONNACK is the liveness authority: the broker runs inside
// the Gateway *process*, so a connack is sufficient proof it is alive.
// The HTTP `/health` probe is a subordinate *death classifier* — it
// runs only while MQTT is down, to distinguish "Gateway died" (fast
// refusal / reset / unreachable → `dead`) from "the path was cut"
// (probe answers, or no verdict at all).
//
// Invariants:
//   1. At most ONE probe in flight. A newer probe (or a CONNACK)
//      aborts the in-flight one; an aborted probe exits silently and
//      NEVER writes state — a superseded question must not pollute a
//      newer fact.
//   2. A timeout is NOT death evidence (the path may be black-holed,
//      which says nothing about the Gateway). Only a fast network
//      failure may declare `dead`, and only while MQTT is still down
//      (that is the classifier's licence).
//   3. `status` is a DISPLAY channel (ADR-051 semantics, read by the
//      banner / settings page). Connection gating reads the MQTT
//      authority — never `status`.
//
// The user-facing "unreachable" hint is NOT derived from the classifier:
// a gateway that stays non-connected for the connectivity module's
// budget (lib/connectivity/gatewayConnectivity.ts) is an outage the
// user must see and act on, whatever the cause (black-holed path,
// refused connection, dead process). The module debounces the MQTT
// authority into `gatewayUnreachable`; the classifier keeps producing
// its alive/dead verdict for diagnostics only.
export type GatewayAlive = "unknown" | "alive" | "dead";

/** `/health` probe budget. Aborting after it is "inconclusive" —
 *  never "dead" (invariant 2). */
const HEALTH_TIMEOUT_MS = 10_000;
/** Death-classifier retry cadence while MQTT is down. */
const DEATH_WATCH_INTERVAL_MS = 3_000;

/** Single in-flight probe (invariant 1). */
let _inFlightHealth: AbortController | null = null;
let _deathWatchHandle: ReturnType<typeof setInterval> | null = null;

type ProbeOutcome =
  | { kind: "ok"; health: HealthResponse; latencyMs: number }
  | { kind: "http_error"; status: number }
  | { kind: "network_error" }
  | { kind: "timeout" }
  | { kind: "aborted" };

/**
 * One `/health` round-trip.
 *
 * `preempt: true`  → a newer caller supersedes the in-flight probe
 *                    (abort reason "superseded"); used by explicit
 *                    `checkHealth()` calls.
 * `preempt: false` → bail out as "aborted" if a probe is already in
 *                    flight; used by the death-classifier tick so it
 *                    never cancels someone else's probe.
 */
async function probeHealthRoundtrip(preempt: boolean): Promise<ProbeOutcome> {
  if (_inFlightHealth) {
    if (!preempt) return { kind: "aborted" };
    _inFlightHealth.abort("superseded");
  }
  const ctrl = new AbortController();
  _inFlightHealth = ctrl;
  const timer = setTimeout(() => ctrl.abort("timeout"), HEALTH_TIMEOUT_MS);
  const t0 = performance.now();
  try {
    const resp = await fetch(`${getGatewayUrl()}/health`, {
      signal: ctrl.signal,
      cache: "no-store",
    });
    if (!resp.ok) return { kind: "http_error", status: resp.status };
    const health = (await resp.json()) as HealthResponse;
    return { kind: "ok", health, latencyMs: performance.now() - t0 };
  } catch {
    if (ctrl.signal.aborted) {
      // Distinguish our own budget expiry from an external cancel.
      return ctrl.signal.reason === "timeout" ? { kind: "timeout" } : { kind: "aborted" };
    }
    // The network stack answered (refused / reset / unreachable) —
    // fast failure IS death evidence for the classifier.
    return { kind: "network_error" };
  } finally {
    clearTimeout(timer);
    if (_inFlightHealth === ctrl) _inFlightHealth = null;
  }
}

/**
 * Consecutive inconclusive probes.
 *
 * One timeout is never death evidence (invariant 2) and that stays true:
 * nothing here writes `status` or `gatewayAlive`. But a path that answers
 * nothing three times in a row is not "slow" — it is a half-dead transport.
 * Observed live on the relay tunnel: yamux ping/pong kept answering, the
 * device stream stopped delivering, every request queued behind it, and the
 * UI spun with no error anywhere. The one client-side action that fixes
 * that is rebuilding the connection, so that is all this does.
 */
const INCONCLUSIVE_STREAK_LIMIT = 3;
/** Minimum gap between two forced reconnects, whatever the streak does. */
const FORCE_RECONNECT_COOLDOWN_MS = 60_000;
let _inconclusiveStreak = 0;
let _lastForcedReconnectAt = 0;

async function noteInconclusiveProbe(): Promise<void> {
  _inconclusiveStreak += 1;
  if (_inconclusiveStreak < INCONCLUSIVE_STREAK_LIMIT) return;
  _inconclusiveStreak = 0;
  const now = Date.now();
  if (now - _lastForcedReconnectAt < FORCE_RECONNECT_COOLDOWN_MS) return;
  _lastForcedReconnectAt = now;
  log.warn(
    `[gateway-health] ${INCONCLUSIVE_STREAK_LIMIT} probes in a row with no answer — ` +
      "transport is half-dead, forcing a fresh MQTT connection",
  );
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("force_reconnect_mqtt");
  } catch (err) {
    // No live MQTT client yet (boot, or already torn down) — nothing to rebuild.
    log.debug("[gateway-health] force_reconnect_mqtt skipped:", err);
  }
}

/**
 * The liveness probe every caller uses. Streak accounting lives here
 * because this is the single choke point both the manual `checkHealth()`
 * and the death-classifier tick pass through.
 */
async function probeHealthOnce(preempt: boolean): Promise<ProbeOutcome> {
  const outcome = await probeHealthRoundtrip(preempt);
  if (outcome.kind === "timeout") {
    void noteInconclusiveProbe();
  } else if (outcome.kind !== "aborted") {
    // Any real verdict — OK, HTTP error, fast refusal — means bytes moved.
    // An aborted probe asked nothing worth answering (invariant 1).
    _inconclusiveStreak = 0;
  }
  return outcome;
}

/**
 * Abort the in-flight probe, if any. The aborted probe exits silently
 * — a newer authority has taken over. Called on MQTT CONNACK and
 * during shutdown.
 */
export function cancelHealthProbe(reason: string): void {
  const ctrl = _inFlightHealth;
  if (!ctrl) return;
  _inFlightHealth = null;
  ctrl.abort(reason);
  log.debug(`[gateway-health] in-flight probe cancelled (${reason})`);
}

/**
 * Mark the Gateway alive. Only two writers exist: CONNACK / a
 * successful probe (via `checkHealth`) write `alive`; the death
 * classifier writes `dead` and only `dead`. No third path — that is
 * the whole point of the model.
 */
export function markGatewayAlive(reason: string): void {
  if (useGatewayStore.getState().gatewayAlive === "alive") return;
  log.warn(`[gateway-health] liveness → alive (${reason})`);
  useGatewayStore.setState({ gatewayAlive: "alive" });
}

/** Apply one classifier verdict. The ONLY writer of `dead`. */
function applyClassifierOutcome(outcome: ProbeOutcome): void {
  switch (outcome.kind) {
    case "ok":
      log.warn("[gateway-health] classifier probe OK — gateway alive (path was cut)");
      useGatewayStore.setState({
        status: "connected",
        health: outcome.health,
        gatewayAlive: "alive",
      });
      break;
    case "network_error":
      log.warn("[gateway-health] classifier probe failed fast — gateway declared dead");
      useGatewayStore.setState({ status: "error", health: null, gatewayAlive: "dead" });
      break;
    case "http_error":
      // TCP + HTTP round-trip succeeded — the process is demonstrably
      // alive (unhealthy, but alive). Do not churn `status`.
      log.warn(`[gateway-health] classifier probe HTTP ${outcome.status} — alive but unhealthy`);
      useGatewayStore.setState({ gatewayAlive: "alive" });
      break;
    case "timeout":
      // Inconclusive — retry on the next tick. NEVER death evidence.
      log.warn("[gateway-health] classifier probe timed out — inconclusive, retrying");
      break;
    case "aborted":
      // Superseded (CONNACK / newer probe) — silent by contract.
      break;
  }
}

/**
 * Start the death classifier: while MQTT is down, probe `/health`
 * every `DEATH_WATCH_INTERVAL_MS` and let `applyClassifierOutcome`
 * decide. Idempotent; stopped by CONNACK (`stopGatewayDeathWatch`).
 */
export function startGatewayDeathWatch(): void {
  if (_deathWatchHandle !== null) return;
  log.warn("[gateway-health] MQTT down — starting death classifier watch");
  const tick = () => {
    // Never preempt: if a probe (ours or a manual checkHealth) is in
    // flight, wait for its verdict — the next tick retries.
    void probeHealthOnce(false).then(applyClassifierOutcome);
  };
  tick(); // immediate first classification
  _deathWatchHandle = setInterval(tick, DEATH_WATCH_INTERVAL_MS);
}

export function stopGatewayDeathWatch(): void {
  if (_deathWatchHandle === null) return;
  clearInterval(_deathWatchHandle);
  _deathWatchHandle = null;
  log.warn("[gateway-health] death classifier watch stopped");
}

export interface GatewayCandidate {
  url: string;
  latencyMs: number;
}

// ── ADR-101: memory service availability ──────────────────────────────
//
// `/api/status` is the only place the Gateway publishes the memory
// supervisor's readiness, and the Desktop must read it to decide whether the
// memory panel / search tab exist at all (a panel over a service that is not
// running answers 503 on every call). `/health` is liveness-only and cannot
// carry it.
//
// Boot race: the supervisor spawns `acowork-memory` while the Gateway is
// coming up, so the very first read can land while the service is still
// binding its port and report `ready: false`. That single first read gets a
// bounded re-check (below) — everything later is an explicit refresh from a
// lifecycle edge, and a `false` answer there is taken at face value. This is
// deliberately NOT a poll (ADR-052).
const MEMORY_READY_RETRY_LIMIT = 2;
const MEMORY_READY_RETRY_DELAY_MS = 2_000;

/** Read `GET /api/status` and project the memory service slice.
 *  Returns `null` when the read says nothing usable: transport failure, a
 *  non-2xx, or a pre-ADR-101 Gateway with no such field. */
async function readMemoryServiceStatus(): Promise<MemoryServiceStatus | null> {
  try {
    const resp = await fetch(`${getGatewayUrl()}/api/status`);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const data = (await resp.json()) as SystemStatusResponse;
    if (!data.memory_service) return null;
    return {
      ready: data.memory_service.ready === true,
      mcp_url: data.memory_service.mcp_url ?? null,
    };
  } catch (e) {
    log.warn(`[memory-service] /api/status read failed: ${String(e)}`);
    return null;
  }
}

const delay = (ms: number) => new Promise((r) => setTimeout(r, ms));

interface GatewayStore {
  status: GatewayStatus;
  /**
   * Single-authority liveness verdict. `alive` is written by CONNACK /
   * a successful probe, `dead` ONLY by the death classifier (fast
   * network failure while MQTT is down). Read by
   * `applyGatewayTransition` (drop/rise edges) — never by input gating.
   */
  gatewayAlive: GatewayAlive;
  /**
   * User-facing outage signal, owned by the connectivity module
   * (lib/connectivity/gatewayConnectivity.ts): true once the MQTT
   * authority has been down for its `UNREACHABLE_HINT_MS` budget.
   * Gates the TitleBar chip (hint + candidates) — deliberately
   * independent of `status`/`gatewayAlive`, so a black-holed path or a
   * slow classifier can never hide the outage. Cleared on the next
   * MQTT rise.
   */
  gatewayUnreachable: boolean;
  health: HealthResponse | null;
  /**
   * ADR-101: memory service availability, read from `GET /api/status`.
   * `null` = never read (or the Gateway does not publish it) — which the
   * gating helpers treat as "not ready", so the memory surface stays hidden
   * until a read proves otherwise.
   */
  memoryService: MemoryServiceStatus | null;
  localState: LocalGatewayState;
  /**
   * Single-topology ownership of the reachable Gateway (see
   * `GatewayOwnership`). Kept separate from `localState` — the state
   * machine tracks the local *process*, this field tracks *who started
   * the Gateway*. Recorded from the boot results returned by
   * `init_local_gateway` / `start_local_gateway` and from
   * `checkLocalStatus` (a live in-process child ⇔ owned).
   */
  localOwnership: GatewayOwnership;
  /** Migration progress for all agents (polled from Gateway) */
  migrationProgress: Record<string, AgentMigrationProgress>;
  /**
   * The active migration session — the agents being re-embedded, whether the
   * rebuild has actually started, and which model it is rebuilding for.
   *
   * This lives in the store, not in EmbeddingModelTab's `useState`, because a
   * migration outlives the component that began it. The tab unmounts whenever
   * the user visits the chat, and with the session held locally the progress
   * list simply vanished mid-rebuild and never came back — the job kept
   * running on the Gateway with no way to watch or stop it. A store slice
   * survives the unmount, so the panel reappears exactly as it was.
   *
   * `started` is the difference between "we know these agents need
   * re-embedding" (the pre-flight list, dismissible) and "the Gateway is
   * rebuilding them right now" (the live list, which must not be dismissible
   * and must not vanish).
   */
  migrationSession: MigrationSession | null;
  /** Begin a migration session (pre-flight: agents awaiting confirmation). */
  beginMigrationSession: (session: MigrationSession) => void;
  /** Mark the session as actually started on the Gateway. */
  markMigrationStarted: () => void;
  /** Replace the selected agent set (pre-flight checkbox toggles). */
  setMigrationSelection: (instanceIds: Set<string>) => void;
  /** Dismiss the session — pre-flight cancel, or close a finished rebuild. */
  clearMigrationSession: () => void;
  /**
   * Reachable gateway URLs discovered by SplashScreen's 5s fallback probe.
   * Populated only when the persisted URL fails to respond; cleared once
   * the user picks one (or the normal boot completes). Stays empty during
   * a happy-path boot — the UI never reads this in that case.
   */
  candidates: GatewayCandidate[];
  checkHealth: () => Promise<void>;
  /**
   * ADR-101: re-read the memory service readiness from `GET /api/status`.
   * Called on mount and on the liveness edges that re-pull Gateway state.
   * A failed read leaves the previous value alone (it says nothing).
   *
   * `retryDelayMs` exists for tests, which must not sleep for real.
   */
  refreshMemoryService: (opts?: { retryDelayMs?: number }) => Promise<void>;
  startLocalGateway: () => Promise<void>;
  stopLocalGateway: () => Promise<void>;
  checkLocalStatus: () => Promise<void>;
  /** Record the outcome of a Rust boot call (`init` / `start`) */
  recordBootResult: (result: GatewayBootResult) => void;
  /** Poll migration progress from Gateway, returns true if any migration is in progress */
  pollMigrationProgress: () => Promise<boolean>;
  /** Update migration progress for a single agent (from WebSocket event) */
  updateMigrationProgress: (agentId: string, reconstructed: number, totalScanned: number) => void;
  /**
   * Replace the candidate list (used by SplashScreen probe + recovery).
   *
   * Accepts a functional updater as well as a value because the title-bar
   * chip folds probe results in one host at a time (see `probeGateways`'s
   * `onSettled`) — a plain value setter would drop whatever a concurrently
   * settling host had already published.
   */
  setCandidates: (
    candidates: GatewayCandidate[] | ((prev: GatewayCandidate[]) => GatewayCandidate[]),
  ) => void;
  /** Clear candidates — call when a Gateway connects or the user dismisses the chooser. */
  clearCandidates: () => void;
}

/**
 * An embedding-dimension migration in flight (or awaiting confirmation).
 * Owned by the store so it outlives the tab that started it.
 */
export interface MigrationSession {
  /** Model the agents are being rebuilt onto. */
  modelId: string;
  /** Old → new vector width, for the "why" line. Null when unknown. */
  oldDimension: number | null;
  newDimension: number;
  /** Backend's explanation of why a rebuild is required. */
  message: string;
  /** Agents in the session; a progress poll updates each entry's status. */
  agents: MigrationAgentEntry[];
  /** Agents the user has ticked (pre-flight only). */
  selected: Set<string>;
  /** False until the Gateway confirms the rebuild is running. */
  started: boolean;
}

export const useGatewayStore = create<GatewayStore>((set, get) => ({
  // ADR-051 + ADR-052 (lifecycle ownership):
  //   `SplashScreen` is the SOLE owner of startup-time health probing.
  //   It calls `checkHealth()` in a poll loop until the Gateway responds,
  //   then calls `onReady()` which mounts `AppLayout`. By the time
  //   `AppLayout` reads `status`, SplashScreen has already pushed it
  //   to `connected`. We start at `disconnected` so that any banner /
  //   indicator keyed on `status === "disconnected"` shows the right
  //   thing before SplashScreen takes over — but AppLayout is gated
  //   by `gatewayReady` in App.tsx, so no banner is visible during the
  //   startup window regardless of this initial value.
  status: "disconnected",
  gatewayAlive: "unknown",
  gatewayUnreachable: false,
  health: null,
  memoryService: null,
  localState: "idle",
  localOwnership: "none",
  migrationProgress: {},
  migrationSession: null,
  candidates: [],

  beginMigrationSession: (session) => set({ migrationSession: session }),

  markMigrationStarted: () =>
    set((s) =>
      s.migrationSession
        ? { migrationSession: { ...s.migrationSession, started: true } }
        : s,
    ),

  setMigrationSelection: (instanceIds) =>
    set((s) =>
      s.migrationSession
        ? { migrationSession: { ...s.migrationSession, selected: instanceIds } }
        : s,
    ),

  clearMigrationSession: () =>
    set({ migrationSession: null, migrationProgress: {} }),

  checkHealth: async () => {
    const prev = get().status;
    const outcome = await probeHealthOnce(true);
    switch (outcome.kind) {
      case "ok":
        set({ status: "connected", health: outcome.health, gatewayAlive: "alive" });
        log.debug(
          `[checkHealth] OK prev=${prev} → connected (${outcome.latencyMs.toFixed(1)}ms)`,
        );
        break;
      case "http_error":
      case "network_error": {
        const label =
          outcome.kind === "http_error" ? `HTTP ${outcome.status}` : "network error (fast fail)";
        log.error(`[checkHealth] FAIL prev=${prev} → ${label}`);
        // ADR-051 display rule: never let a fresh probe failure surface
        // as `error` (the red bar would flash during a normal boot) —
        // only a probe that fails while `connected` is a genuine
        // outage. Note this is DISPLAY only; liveness (`gatewayAlive`)
        // is not written from a plain checkHealth failure — death is
        // declared by the classifier alone.
        set(
          prev === "connected"
            ? { status: "error" as const, health: null }
            : { status: "connecting" as const, health: null },
        );
        break;
      }
      case "timeout":
        // Inconclusive by contract: a slow / black-holed path says
        // nothing about the Gateway, so nothing is touched (the
        // classifier watch retries while MQTT is down).
        log.error(
          `[checkHealth] FAIL prev=${prev} → no response in ${HEALTH_TIMEOUT_MS}ms (inconclusive)`,
        );
        break;
      case "aborted":
        // Superseded by a newer probe / CONNACK — silent by contract
        // (invariant 1). This is the branch that used to arrive 21s
        // late and clobber a healed status.
        log.debug(`[checkHealth] aborted (superseded) prev=${prev}`);
        break;
    }
  },

  refreshMemoryService: async (opts) => {
    const wasUnknown = get().memoryService === null;
    const first = await readMemoryServiceStatus();
    if (!first) return;
    set({ memoryService: first });
    if (first.ready || !wasUnknown) return;
    // Boot-race re-check: only for the very first read of the session, and
    // only while it came back not-ready. A `false` on a later refresh is a
    // real answer (the operator disabled the service, or it gave up
    // restarting) and re-reading would just repeat the same request.
    const retryDelayMs = opts?.retryDelayMs ?? MEMORY_READY_RETRY_DELAY_MS;
    for (let attempt = 0; attempt < MEMORY_READY_RETRY_LIMIT; attempt++) {
      await delay(retryDelayMs);
      // Re-read the store, not the local: another edge may have refreshed it
      // while we were waiting, and a newer fact wins.
      if (get().memoryService?.ready) return;
      const again = await readMemoryServiceStatus();
      if (again) set({ memoryService: again });
    }
  },

  startLocalGateway: async () => {
    // Sync with the Rust-side process handle before checking the guard.
    // The SplashScreen boot path calls `init_local_gateway` directly (not
    // this action), so `localState` may still be "idle" even though the
    // backend already has a running child process.
    await get().checkLocalStatus();
    if (get().localState === "starting") return;
    if (get().localState === "running") {
      // Gateway process already exists (e.g. from a previous session or
      // SplashScreen boot path), but we may not have checked health yet.
      // Without this call, `status` stays "disconnected" and the UI shows
      // "Not started" even though the Gateway is actually reachable.
      await get().checkHealth();
      return;
    }
    set({ localState: "starting" });
    try {
      // Dynamically import invoke to avoid issues when not in Tauri context
      const { invoke } = await import("@tauri-apps/api/core");
      const result = await invoke<GatewayBootResult>("start_local_gateway");
      // Single-topology probe-then-spawn: "owned" means Desktop spawned a
      // child (localState → running); "foreign" means a Gateway was
      // already answering at the URL, so no child exists and the process
      // state stays "stopped" even though the Gateway is reachable.
      set({
        localState: result.ownership === "owned" ? "running" : "stopped",
        localOwnership: result.ownership,
      });
      // Check health now that the local gateway is up
      await get().checkHealth();
    } catch (err) {
      log.error("Failed to start local gateway:", err);
      set({ localState: "error" });
    }
  },

  stopLocalGateway: async () => {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("stop_local_gateway");
      set({
        localState: "stopped",
        localOwnership: "none",
        status: "disconnected",
        health: null,
        // The Gateway was the memory service's host — with it gone the
        // service is gone too, and `memoryService` goes back to "never read"
        // so the next boot runs the readiness re-check again.
        memoryService: null,
        // The user just stopped the Gateway — liveness is a confirmed
        // fact (the drop edge in applyGatewayTransition fires from it).
        gatewayAlive: "dead",
      });
    } catch (err) {
      log.error("Failed to stop local gateway:", err);
      set({ localState: "error" });
    }
  },

  checkLocalStatus: async () => {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      const running = await invoke<boolean>("get_local_gateway_status");
      set((s) => ({
        // `get_local_gateway_status` reports whether a live in-process
        // *child* exists, which is exactly the "owned" condition. A
        // foreign (adopted) Gateway has no child handle, so it never
        // flips `localOwnership` to "owned" here — that stays whatever
        // the boot result recorded.
        localState: running ? "running" : "stopped",
        localOwnership: running ? "owned" : s.localOwnership,
      }));
    } catch {
      // Not in Tauri context (e.g. plain web dev mode) or command failed.
      // Leave localState unchanged so we don't clobber a valid "running"
      // state from a previous successful start.
    }
  },

  recordBootResult: (result) => {
    set({
      localOwnership: result.ownership,
      localState: result.ownership === "owned" ? "running" : "stopped",
    });
  },

  pollMigrationProgress: async () => {
    if (get().status !== "connected") return false;
    try {
      const resp = await fetchMigrationProgress();
      const progress: Record<string, AgentMigrationProgress> = {};
      let anyInProgress = false;
      for (const agent of resp.agents) {
        progress[agent.instance_id] = agent;
        if (!agent.done && !agent.error) anyInProgress = true;
      }
      set({ migrationProgress: progress });
      return anyInProgress;
    } catch {
      return false;
    }
  },

  updateMigrationProgress: (agentId: string, reconstructed: number, totalScanned: number) => {
    set((state) => {
      const existing = state.migrationProgress[agentId];
      if (!existing) return state;
      return {
        migrationProgress: {
          ...state.migrationProgress,
          [agentId]: {
            ...existing,
            progress: {
              rebuilt: reconstructed,
              total_scanned: totalScanned,
              errors: existing.progress?.errors ?? 0,
              phase: "reembed",
              label: existing.progress?.label ?? "",
            },
          },
        },
      };
    });
  },

  setCandidates: (candidates) =>
    set((s) => ({
      candidates: typeof candidates === "function" ? candidates(s.candidates) : candidates,
    })),
  clearCandidates: () => set({ candidates: [] }),
}));

