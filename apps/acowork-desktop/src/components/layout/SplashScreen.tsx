import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useAuthStore } from "../../stores/authStore";
import { initMqttListener } from "../../stores/chatStore";
import { initWorkspaceFsListener } from "../../lib/workspaceFsEvents";
import { initDocTreeChangeListener } from "../../lib/docFsEvents";
import { getGatewayUrl } from "../../lib/config";
import { UNREACHABLE_HINT_MS, probeKnownCandidates } from "../../lib/connectivity/gatewayConnectivity";
import { useTranslation } from "../../i18n/useTranslation";
import { GatewayAddressPicker } from "../connection/GatewayAddressPicker";
// eslint-disable-next-line @typescript-eslint/no-require-imports
import pkg from "../../../package.json";
import brandMark from "../../../../../assets/brand-mark.svg";
import { log } from "../../lib/logger";
import type { BootstrapStateView, GatewayBootResult } from "../../lib/types";

const POLL_INTERVAL = 500;
const MIN_SPLASH_MS = 1500;
/**
 * Hard cap on the boot pipeline before the recovery (timeout) view is
 * surfaced. Mode-aware, because the two paths have very different
 * legitimate latencies:
 *   - remote/relay only PROBE a Gateway for reachability, so a dead host is
 *     known quickly — a short budget gets the user to the recovery view fast.
 *   - local additionally WAITS for the Gateway to spawn and finish its
 *     bootstrap (ONNX / embed model load), which can legitimately take tens
 *     of seconds. A false timeout there is worse than a slightly long wait,
 *     so local keeps the bigger budget.
 */
const REMOTE_MAX_WAIT_MS = 12_000;
const LOCAL_MAX_WAIT_MS = 30_000;
/**
 * Fallback-window: how long SplashScreen waits before giving up on the
 * persisted Gateway URL and surfacing alternative reachable URLs from
 * the URL history (laptop moved to a new LAN). Shared with the runtime
 * unreachable hint — boot and runtime use ONE number
 * (`UNREACHABLE_HINT_MS`, see lib/connectivity/gatewayConnectivity.ts).
 * The happy path is unaffected — once the Gateway connects, the
 * candidate UI is immediately dismissed. 30s still acts as the hard
 * upper bound for the underlying boot pipeline.
 */

/**
 * Push the persisted address into Rust and, when that address is this
 * machine, boot the Gateway — probe-then-spawn. Returns nothing; the poll
 * loop in `SplashScreen` decides when the Gateway is ready.
 *
 * ADR-115 single-address policy: `mode` is derived from the URL, never
 * read from a separate persisted knob. A loopback URL means Desktop MAY
 * spawn/adopt a local Gateway; any other address is connect-only. This is
 * what fixes the old "mode=local but address=remote" deadlock where Rust
 * spawned on 127.0.0.1 while every probe went to a LAN host.
 */
async function bootGateway(): Promise<void> {
    const settings = useSettingsStore.getState();
    const mode = settings.gatewayMode;
    const url = settings.gatewayUrl;
    const gatewayStore = useGatewayStore.getState();

    // 1) Sync config into Rust.
    await invoke("set_gateway_config", {
        config: { mode, url },
    });

    // 2) Boot the gateway itself — only for a loopback address.
    if (mode === "local") {
        // Probe-then-spawn (see spawn_gateway in commands/gateway.rs):
        // adopts an already-running Gateway ("foreign") or spawns a
        // child ("owned"). Waits for bootstrap READY before returning.
        const boot = await invoke<GatewayBootResult>("init_local_gateway");
        gatewayStore.recordBootResult(boot);
        // Rust re-points base_url at the fixed loopback port when it had to
        // spawn (the child cannot serve an arbitrary alias/port). Mirror
        // that back into the frontend so every subsequent HTTP call and
        // the persisted address agree with the process that is running.
        useSettingsStore.getState().adoptBootUrl(boot.base_url);
        // Sync the frontend store with the Rust-side process handle.
        await gatewayStore.checkLocalStatus();
    }
    // Remote / relay: the Gateway is presumed already running on `url`.
    // The poll loop below verifies reachability; Desktop never spawns.

    // 3) ADR-077: System Agent is a regular bundled agent. Installation
    //    happens through the standard flow (onboarding wizard / agentStore
    //    `installAgent` → `POST /api/agents/install`), not a dedicated Tauri
    //    command. No per-boot System-Agent step needed here.
    //
    // 4) Register the frontend event listeners UNCONDITIONALLY and BEFORE
    //    the connect attempt (ADR-033 / ADR-058). They are pure event
    //    subscriptions — valid while the Rust client slot is still `None`
    //    (pre-login relay boot, or a failed connect) — and the status
    //    watchdog converges the UI as soon as a client appears. Merging
    //    them into the connect try-block (previous behaviour) meant any
    //    connect rejection skipped ALL THREE, so even a later successful
    //    reconnect (auth bridge / manual Connect) left the UI frozen at
    //    `idle` with no live event stream.
    try {
        await initMqttListener();
        log.debug("[bootGateway] MQTT listener initialized");
    } catch (err) {
        log.warn("[bootGateway] initMqttListener failed:", err);
    }
    try {
        // ADR-058: workspace fs-changed listener (auto tree refresh +
        // editor conflict UX + reconnect full-sync fallback).
        await initWorkspaceFsListener();
        log.debug("[bootGateway] workspace fs listener initialized");
    } catch (err) {
        log.warn("[bootGateway] initWorkspaceFsListener failed:", err);
    }
    try {
        // Doc library tree-change listener (event-driven refresh of the
        // doc sidebar; reconnect full-sync fallback built in).
        await initDocTreeChangeListener();
        log.debug("[bootGateway] doc tree-change listener initialized");
    } catch (err) {
        log.warn("[bootGateway] initDocTreeChangeListener failed:", err);
    }

    // 5) Connect the MQTT client for real-time events (ADR-033).
    //
    //    Relay-mode prerequisite: the strict remote listener rejects
    //    CONNECT without a mirrored access token (chat_mqtt.rs:111). No
    //    token can exist this early on a fresh boot (login happens AFTER
    //    the splash), so waiting blocks the whole pipeline for nothing —
    //    skip the call and let the auth bridge (`gatewayAuthBridge.ts`)
    //    fire `connect_mqtt` on the null→non-null token transition. The
    //    chat banner / services panel keep a manual escape meanwhile.
    if (mode === "relay" && !useAuthStore.getState().accessToken) {
        log.info(
            "[bootGateway] relay mode without an access token; " +
            "deferring connect_mqtt to the auth bridge (fires on login)",
        );
    } else {
        try {
            await invoke("connect_mqtt");
            log.debug("[bootGateway] MQTT connected");
        } catch (err) {
            log.warn("connect_mqtt failed:", err);
        }
    }
}

/** Best-effort human-readable text for a rejected Tauri `invoke`. */
function describeBootError(err: unknown): string {
    if (typeof err === "string") return err;
    if (err instanceof Error) return err.message;
    return String(err);
}

/**
 * Every key the boot status line may show. A closed union, not `string`:
 * the line is rendered as `t(status.key)`, so a typo here is invisible to the
 * i18n scanner (it only sees `t("literal")` calls) and would surface as a raw
 * key name on the very screen a broken install lands on.
 */
export const SPLASH_STATUS_KEYS = [
    "splashScreen.ready",
    "splashScreen.waitingForServices",
    "splashScreen.preparing",
    "splashScreen.startingGateway",
    "splashScreen.connectingToGateway",
    "splashScreen.connectingToUrl",
    "splashScreen.retryingConnection",
] as const;

type SplashStatusKey = (typeof SPLASH_STATUS_KEYS)[number];

/** A localizable status line. Stored as key + params so the string is
 *  rendered through `t()` at paint time — see the `status` state comment. */
type StatusSpec = { key: SplashStatusKey; params?: Record<string, string | number> };

interface SplashScreenProps {
    onReady: () => void;
}

function LoadingDots({ className = "" }: { className?: string }) {
    const [dots, setDots] = useState(0);
    useEffect(() => {
        const id = setInterval(() => setDots((d) => (d + 1) % 4), 400);
        return () => clearInterval(id);
    }, []);
    return (
        <span className={className}>
            {".".repeat(dots)}
            <span className="opacity-0">{".".repeat(3 - dots)}</span>
        </span>
    );
}

/**
 * Inline chooser rendered alongside the boot spinner when the 5s
 * fallback probe finds reachable Gateways other than the persisted
 * one. Stays non-modal — the original boot spinner keeps running so the
 * persisted URL still has a chance to come up (Wi-Fi flapping).
 *
 * Sort order is set upstream in the probe caller (fastest first). The
 * "Keep waiting" button just dismisses the chooser — the main boot
 * pipeline continues unchanged.
 *
 * `pickingUrl` is set by the parent the moment the user clicks a row,
 * BEFORE the boot retry has a chance to round-trip. While it is set:
 *   - the picked row swaps its latency label for a spinner +
 *     "Connecting..." text so the user gets immediate feedback that
 *     the click landed (no more "did I really click that?" feeling)
 *   - every other row + the dismiss button are disabled, so an
 *     impatient multi-click can't fan out into parallel boot pipelines
 *     or hide the chooser under the spinner.
 * The parent clears `pickingUrl` once the retry resolves (success OR
 * failure).
 */
function CandidateChooser({
    candidates,
    pickingUrl,
    onPick,
    onDismiss,
}: {
    candidates: { url: string; latencyMs: number }[];
    /** URL the user just clicked. `null` while idle. */
    pickingUrl: string | null;
    onPick: (url: string) => void;
    onDismiss: () => void;
}) {
    const { t } = useTranslation();
    const isPicking = pickingUrl !== null;
    return (
        <div className="mb-2 w-[28rem] max-w-full rounded-md border border-border-outer bg-panel-base p-3 text-left shadow-sm">
            <div className="mb-2 flex items-center justify-between">
                <p className="text-xs font-medium text-text-secondary">
                    {t("splashScreen.candidateTitle")}
                </p>
                <button
                    onClick={onDismiss}
                    disabled={isPicking}
                    className="text-11 text-text-tertiary hover:text-text-secondary disabled:cursor-not-allowed disabled:opacity-40"
                >
                    {t("splashScreen.candidateKeepWaiting")}
                </button>
            </div>
            <ul className="flex flex-col gap-1">
                {candidates.map((c) => {
                    const isThisRow = pickingUrl === c.url;
                    return (
                        <li key={c.url}>
                            <button
                                onClick={() => onPick(c.url)}
                                disabled={isPicking}
                                aria-busy={isThisRow || undefined}
                                style={isThisRow ? { opacity: 1 } : undefined}
                                className="btn-accent flex w-full items-center justify-between rounded px-2.5 py-1.5 text-left disabled:cursor-not-allowed disabled:opacity-50"
                            >
                                {/* White on the accent fill — never the theme text
                                    tokens, which are unreadable on a solid accent. */}
                                <span
                                    className="truncate font-mono text-xs text-white"
                                    title={c.url}
                                >
                                    {c.url}
                                </span>
                                {isThisRow ? (
                                    <span className="ml-2 flex shrink-0 items-center gap-1.5 text-11 text-white/90">
                                        <span
                                            aria-hidden="true"
                                            className="inline-block h-3 w-3 animate-spin rounded-full border-[1.5px] border-white/40 border-t-white"
                                        />
                                        {t("splashScreen.candidateConnecting")}
                                    </span>
                                ) : (
                                    <span className="ml-2 shrink-0 text-11 text-white/80">
                                        {Math.round(c.latencyMs)} ms
                                    </span>
                                )}
                            </button>
                        </li>
                    );
                })}
            </ul>
        </div>
    );
}

export function SplashScreen({ onReady }: SplashScreenProps) {
    const { t } = useTranslation();
    const checkHealth = useGatewayStore((s) => s.checkHealth);
    const candidates = useGatewayStore((s) => s.candidates);
    const clearCandidates = useGatewayStore((s) => s.clearCandidates);
    const gatewayMode = useSettingsStore((s) => s.gatewayMode);
    /**
     * Whether the candidate chooser has been offered in this boot
     * session. Gates the 5s fallback timer so we don't reprobe on every
     * `handleRetry` — once we've offered the picker, we let the existing
     * 30s timeout / user-driven retry path take over.
     */
    const [candidatesOffered, setCandidatesOffered] = useState(false);
    /**
     * A status line is stored as `(key, params)`, never as a rendered string.
     * Two reasons:
     *   - "Starting Gateway: <address>" has to name the address the boot is
     *     actually aimed at (ADR-115: the address IS the setting, so a status
     *     line that hides it says nothing useful);
     *   - rendering through `t()` at paint time keeps the line correct if the
     *     language changes, instead of freezing whatever locale was active
     *     when the state was set.
     */
    const [status, setStatus] = useState<StatusSpec>(() => {
        const { gatewayUrl: url, gatewayMode: mode } = useSettingsStore.getState();
        return mode === "local"
            ? { key: "splashScreen.startingGateway", params: { url } }
            : { key: "splashScreen.connectingToGateway", params: { url } };
    });
    const [timedOut, setTimedOut] = useState(false);
    /**
     * The boot budget that actually fired the current timeout, captured at
     * fire time so the recovery message can print the real number even after
     * the user switches mode on the timeout view (which would otherwise make
     * a freshly-derived value disagree with what just happened).
     */
    const [timedOutBudgetMs, setTimedOutBudgetMs] = useState(REMOTE_MAX_WAIT_MS);
    // Hard boot failure (a rejected `invoke` inside `bootGateway`).
    // Rendered in the same recovery view as `timedOut`, with the error
    // text instead of the "did not respond" copy. Cleared by retry.
    const [bootError, setBootError] = useState<string | null>(null);
    const [retrying, setRetrying] = useState(false);
    /**
     * The candidate URL the user just clicked, set the instant the click
     * lands (NOT after the boot round-trip). Drives the candidate row's
     * own "Connecting..." + spinner state and disables the rest of the
     * chooser so impatient multi-clicks don't fan out into parallel boot
     * pipelines. Mirrored in `pickingUrlRef` so the async `handleRetry`
     * can read it without waiting for React's setState to commit.
     * Cleared in `handleRetry`'s `finally` once the retry resolves.
     */
    const [pickingUrl, setPickingUrl] = useState<string | null>(null);
    const pickingUrlRef = useRef<string | null>(null);
    const [fadeIn, setFadeIn] = useState(false);
    const mountedRef = useRef(true);
    const startTimeRef = useRef(Date.now());

    useEffect(() => {
        requestAnimationFrame(() => setFadeIn(true));
    }, []);

    // Candidate dismissal on a CONNECTED transition and URL-history
    // recording are both owned by top-level subscribers now
    // (lib/connectivity/gatewayConnectivity.ts and App.tsx) — one
    // implementation shared by the boot and runtime paths, instead of
    // the same status transition being handled in two places.

    /**
     * Check bootstrap readiness via `GET /api/bootstrap` (ADR-059 §5.1)
     * — the aggregated phase replaces the old "find system agent in
     * /api/agents" heuristic, which guessed at readiness from an
     * unrelated inventory snapshot.
     */
    const checkBootstrapReady = async (): Promise<BootstrapStateView | null> => {
        if (!mountedRef.current) return null;
        try {
            const resp = await fetch(`${getGatewayUrl()}/api/bootstrap`);
            if (!resp.ok) return null;
            return (await resp.json()) as BootstrapStateView;
        } catch {
            return null;
        }
    };

    const doCheck = async (): Promise<boolean> => {
        if (!mountedRef.current) return false;
        await checkHealth();
        if (useGatewayStore.getState().status === "connected") {
            return true;
        }
        return false;
    };

    const finish = async () => {
        if (!mountedRef.current) return;
        // ADR-052: /api/bootstrap READY does NOT imply `checkHealth()` has
        // run. The local-gateway boot path (`bootGateway` -> Rust
        // `init_local_gateway`) returns READY directly and never touches the
        // health probe, so `status` may still be `disconnected` here. Ensure
        // the store reflects a connected Gateway BEFORE we hand off to
        // AppLayout - otherwise the chat input stays disabled ("Gateway not
        // connected") even though the Gateway booted fine.
        await checkHealth();
        const elapsed = Date.now() - startTimeRef.current;
        const remaining = Math.max(0, MIN_SPLASH_MS - elapsed);
        setStatus({ key: "splashScreen.ready" });
        setTimeout(() => {
            if (mountedRef.current) onReady();
        }, remaining);
    };

    useEffect(() => {
        startTimeRef.current = Date.now();
        mountedRef.current = true;

        // Mode-aware boot budget: remote/relay probe reachability (fail fast),
        // local additionally waits for spawn + bootstrap (see the constants).
        const maxWaitMs =
            gatewayMode === "local" ? LOCAL_MAX_WAIT_MS : REMOTE_MAX_WAIT_MS;

        let pollTimer: ReturnType<typeof setInterval>;
        let maxTimer: ReturnType<typeof setTimeout>;
        let candidateTimer: ReturnType<typeof setTimeout> | undefined;

        const pollGateway = (onDone: () => void) => {
            pollTimer = setInterval(async () => {
                const done = await doCheck();
                if (done) {
                    clearInterval(pollTimer);
                    onDone();
                }
            }, POLL_INTERVAL);
        };

        const pollAgentReady = () => {
            // Phase not READY yet: surface the Gateway's own diagnostic
            // (subsystem-level phase detail) instead of a fixed message.
            setStatus({ key: "splashScreen.waitingForServices" });
            const onSnapshot = (view: BootstrapStateView | null) => {
                if (!view) return false;
                if (view.phase === "READY" || view.phase === "DEGRADED") return true;
                if (view.phase === "FAILED" || view.phase === "SHUTTING_DOWN") {
                    setTimedOut(true);
                    return true;
                }
                setStatus({
                    key: "splashScreen.preparing",
                    params: { detail: view.phase_detail },
                });
                return false;
            };
            // Try immediately first
            checkBootstrapReady().then((view) => {
                if (!mountedRef.current) return;
                if (onSnapshot(view)) { finish(); return; }
                // Then poll
                pollTimer = setInterval(async () => {
                    if (!mountedRef.current) { clearInterval(pollTimer); return; }
                    const view = await checkBootstrapReady();
                    if (onSnapshot(view)) {
                        clearInterval(pollTimer);
                        finish();
                    }
                }, POLL_INTERVAL);
            });
        };

        const init = async () => {
            // Name the address, and drop the word "local": since ADR-115
            // there is no local/remote *mode* for the user to reason about —
            // there is one server address, and this line should say which.
            // A loopback address is also the only one Desktop may spawn, so
            // "starting" is the honest verb there.
            const bootUrl = getGatewayUrl();
            setStatus(
                gatewayMode === "local"
                    ? { key: "splashScreen.startingGateway", params: { url: bootUrl } }
                    : { key: "splashScreen.connectingToGateway", params: { url: bootUrl } },
            );

            // Fallback: if the persisted URL hasn't connected within the
            // shared budget, probe the URL history through the
            // connectivity module — the SAME candidate probe the runtime
            // chip uses (lib/connectivity/gatewayConnectivity.ts). Fires
            // on a parallel track; doesn't cancel the main boot pipeline.
            // The helper re-reads the store at fire time (a retry may
            // have moved the URL), skips local mode / an already-connected
            // Gateway, and folds results in per host.
            if (!candidatesOffered) {
                candidateTimer = setTimeout(async () => {
                    if (!mountedRef.current) return;
                    const found = await probeKnownCandidates();
                    if (mountedRef.current && found > 0) setCandidatesOffered(true);
                }, UNREACHABLE_HINT_MS);
            }

            // Push persisted settings into Rust and (for local mode)
            // spawn the child Gateway. The previous architecture did
            // this in Rust's setup hook, which ignored the user's
            // remote-gateway setting.
            try {
                await bootGateway();
            } catch (err) {
                log.error("bootGateway failed:", err);
                // Do NOT fall through to the health/readiness poll. A
                // rejected invoke means the Rust-side boot steps (config
                // push / local spawn / MQTT client) never ran — a
                // reachable Gateway would then still leave the chat input
                // permanently disabled ("Gateway not connected") with no
                // visible error. Stay on the splash and surface it; the
                // user fixes the cause and hits Retry.
                if (mountedRef.current) {
                    setBootError(describeBootError(err));
                    setTimedOut(true);
                }
                return;
            }

            // ADR-059 §5.1: readiness is the /api/bootstrap phase, not a
            // /health ping. `init_local_gateway` already waited for READY
            // (Rust side); this poll only covers the remote-gateway path
            // and the window before the MQTT snapshot arrives.
            const view = await checkBootstrapReady();
            if (view && (view.phase === "READY" || view.phase === "DEGRADED")) {
                pollAgentReady();
                return;
            }
            pollGateway(() => pollAgentReady());
        };

        maxTimer = setTimeout(() => {
            if (mountedRef.current) {
                clearInterval(pollTimer);
                setTimedOutBudgetMs(maxWaitMs);
                setTimedOut(true);
            }
        }, maxWaitMs);

        init();

        return () => {
            mountedRef.current = false;
            clearInterval(pollTimer);
            clearTimeout(maxTimer);
            if (candidateTimer) clearTimeout(candidateTimer);
        };
    }, []); // eslint-disable-line react-hooks/exhaustive-deps

    const handleRetry = async (overrideUrl?: string) => {
        setRetrying(true);
        setTimedOut(false);
        setBootError(null);
        startTimeRef.current = Date.now();
        // Snapshot the picking URL set by the candidate click. The
        // candidate-pick path sets `pickingUrl` + `pickingUrlRef` BEFORE
        // calling `handleRetry(url)`, so this ref is already current
        // here (the manual "Retry" button path leaves it null). Reading
        // it via ref avoids waiting for React's setState to commit.
        const pickingOnEntry = pickingUrlRef.current;
        // `overrideUrl` comes from the candidate-chip path. Commit it with
        // the LOW-LEVEL setter: this retry already calls `bootGateway()`
        // below, so a `switchGateway()` here would restart the whole
        // SplashScreen and fan out two boot pipelines. The mode is derived
        // from the URL now, so there is no relay+http inconsistency to fix.
        if (overrideUrl) {
            useSettingsStore.getState().setGatewayUrl(overrideUrl);
        }
        // The candidate-pick path already set a "Connecting to <url>..."
        // status text the instant the row was clicked — don't overwrite
        // it with the generic "Retrying..." string until the pick settles.
        if (!pickingOnEntry) {
            setStatus({ key: "splashScreen.retryingConnection" });
        }
        try {
            // Re-run the FULL boot sequence (config push → spawn/adopt →
            // system agent → MQTT + listeners) instead of the previous
            // ad-hoc `startLocalGateway` + `connect_mqtt`-with-swallowed-
            // error combination: the old flow could report "Ready" while
            // MQTT was never connected. Safe to repeat — `connect_mqtt`
            // reuses an existing client bound to the same broker.
            try {
                await bootGateway();
            } catch (err) {
                log.error("bootGateway retry failed:", err);
                if (!mountedRef.current) return;
                setBootError(describeBootError(err));
                // Do NOT setTimedOut here — the `finally` block is the
                // single source of truth for the recovery transition so
                // the picking-state cleanup + dead-candidate pruning
                // run on the same code path regardless of why the retry
                // failed (boot invoke throw vs unreachable URL).
                return;
            }
            const gDone = await doCheck();
            if (gDone) {
                // ADR-059 §5.1: wait for bootstrap phase READY (replaces
                // the old agent-inventory readiness guess).
                const view = await checkBootstrapReady();
                if (view && (view.phase === "READY" || view.phase === "DEGRADED")) {
                    finish();
                } else {
                    // Quick poll a few times
                    for (let i = 0; i < 20; i++) {
                        if (!mountedRef.current) break;
                        await new Promise((r) => setTimeout(r, 500));
                        const v = await checkBootstrapReady();
                        if (v && (v.phase === "READY" || v.phase === "DEGRADED")) { finish(); break; }
                    }
                }
            }
        } finally {
            if (!mountedRef.current) return;
            setRetrying(false);
            // Clear the picking state so the chooser re-enables. If we
            // managed to connect, the parent unmounts us via onReady
            // and this setState is a no-op render; if the pick failed,
            // the chooser needs to be live again so the user can try
            // another candidate (or edit the URL on the timeout view).
            pickingUrlRef.current = null;
            setPickingUrl(null);
            // Failed retry must return to the timeout view (with the
            // editable address field) — otherwise the UI is stuck on
            // the "Retrying..." spinner forever and the user can't
            // correct the URL a second time. For the candidate-pick
            // path, also prune the failing URL from the candidate list
            // so the user can't re-click the exact same dead address.
            if (useGatewayStore.getState().status !== "connected") {
                if (pickingOnEntry) {
                    const dead = pickingOnEntry;
                    useGatewayStore.getState().setCandidates((prev) =>
                        prev.filter((c) => c.url !== dead),
                    );
                }
                setTimedOut(true);
            }
        }
    };

    return (
        <div className="relative flex h-screen w-screen flex-col items-center justify-center overflow-hidden bg-page-bg">
            <div
                className={`relative z-10 flex flex-col items-center gap-10 transition-all duration-700 ${fadeIn ? "translate-y-0 opacity-100" : "translate-y-4 opacity-0"
                    }`}
            >
                {/* Title — ACowork brand mark (shared SVG with README).
                    331 keeps the mark at the size it had before the asset was
                    re-framed to artwork + a uniform 6px gutter (396 → 364.25 wide). */}
                <div className="flex flex-col items-center gap-5">
                    <h1 className="leading-none">
                        <img src={brandMark} alt={t("splashScreen.altLogo")} width="331" />
                    </h1>
                    <p className="text-sm font-medium uppercase tracking-[0.25em] text-text-tertiary ">
                        Collaborate with your colleagues and AI agents
                    </p>
                </div>

                {/* Status area */}
                <div className="flex min-h-[60px] flex-col items-center gap-3">
                    {timedOut ? (
                        <>
                            <div className="flex items-center gap-2">
                                <div className="h-2 w-2 animate-pulse rounded-full bg-amber-500" />
                                <p className="max-w-md text-center text-sm break-words text-text-secondary ">
                                    {bootError
                                        ? t("splashScreen.bootFailed", { error: bootError })
                                        : t("splashScreen.timeoutNoResponse", {
                                              seconds: timedOutBudgetMs / 1000,
                                          })}
                                </p>
                            </div>
                            <p className="max-w-md text-center text-xs text-text-tertiary ">
                                {bootError
                                    ? t("splashScreen.bootFailedHint")
                                    : t("splashScreen.timeoutHint")}
                            </p>
                            {/* ADR-115: ONE address list, always available.
                                No mode picker — localhost is just an entry,
                                and choosing it re-runs the boot pipeline,
                                which spawns a local Gateway if none answers. */}
                            <div className="mt-2 w-full max-w-sm rounded-md border border-border-outer bg-panel-base p-3">
                                <GatewayAddressPicker
                                    compact
                                    busy={retrying}
                                    onSelect={(url) => {
                                        // `switchGateway` drops the session and
                                        // restarts the boot gate (App.tsx keys
                                        // the SplashScreen on `epoch`), so the
                                        // new address gets a full, fresh boot.
                                        void useSettingsStore.getState().switchGateway(url);
                                    }}
                                    onReset={() => {
                                        void useSettingsStore.getState().resetConnectionSettings();
                                    }}
                                />
                            </div>
                            <button
                                onClick={() => { void handleRetry(); }}
                                disabled={retrying}
                                className="btn-accent mt-1 rounded-md px-4 py-[var(--ui-btn-py)] text-xs font-medium disabled:opacity-40"
                            >
                                {retrying
                                    ? t("splashScreen.retrying")
                                    : t("splashScreen.retryConnection")}
                            </button>
                        </>
                    ) : (
                        <>
                            {/* Candidate chooser — only appears if the
                                5s fallback probe found other reachable
                                Gateways in URL history. Original boot
                                spinner stays untouched below. */}
                            {candidates.length > 0 && (
                                <CandidateChooser
                                        candidates={candidates}
                                        pickingUrl={pickingUrl}
                                        onPick={(url) => {
                                            // Set the picking state FIRST so
                                            // the row can render its
                                            // "Connecting..." spinner + grey
                                            // out the rest of the chooser in
                                            // the same paint that the click
                                            // handler returns. Without this,
                                            // the user sees zero feedback
                                            // until the boot round-trip
                                            // completes (multi-second) and
                                            // concludes the click "did
                                            // nothing".
                                            setPickingUrl(url);
                                            pickingUrlRef.current = url;
                                            setStatus({
                                                key: "splashScreen.connectingToUrl",
                                                params: { url },
                                            });
                                            // Reuse the existing retry path so
                                            // we don't fork the boot flow. It
                                            // commits `overrideUrl` with the
                                            // low-level setter (mode is now
                                            // derived from the URL, so there
                                            // is no relay+http combo to fix).
                                            handleRetry(url);
                                        }}
                                        onDismiss={() => clearCandidates()}
                                />
                            )}
                            {/* Progress bar */}
                            <div className="h-0.5 w-48 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
                                <div
                                    className="h-full animate-[pulse_1.5s_ease-in-out_infinite] rounded-full"
                                    style={{
                                        background:
                                            "linear-gradient(90deg, #10b981, #06b6d4, #34d399, #10b981)",
                                        backgroundSize: "200% 100%",
                                        animation:
                                            "shimmer 2s ease-in-out infinite, pulse 1.5s ease-in-out infinite",
                                    }}
                                />
                            </div>
                            <p className="text-sm text-text-tertiary ">
                                {t(status.key, status.params)}
                                <LoadingDots className="inline-block w-5 text-left text-text-tertiary " />
                            </p>
                        </>
                    )}
                </div>
            </div>

            {/* Version in bottom corner */}
            <div
                className={`absolute bottom-6 transition-all duration-700 delay-300 ${fadeIn ? "translate-y-0 opacity-100" : "translate-y-2 opacity-0"
                    }`}
            >
                <span className="text-11 text-text-tertiary ">ACowork v{pkg.version}</span>
            </div>

            {/* Keyframe styles injected once */}
            <style>{`
                @keyframes shimmer {
                    0% { background-position: 200% 0; }
                    100% { background-position: -200% 0; }
                }
            `}</style>
        </div>
    );
}
