/**
 * ADR-05x frontend tests: single-topology Gateway ownership.
 *
 * `localOwnership` ("owned" | "foreign" | "none") is a NEW parameter that
 * sits BESIDE the existing `localState` state machine (which is never
 * modified). These tests pin the semantics agreed for the single-topology
 * design:
 *
 *   - "owned"   ⇔ this Desktop session spawned the Gateway child.
 *   - "foreign" ⇔ a Gateway answers at the configured URL, but Desktop
 *                 did NOT spawn it → Desktop must never show Stop /
 *                 force-kill it, and quitting never prompts for it.
 *   - "none"    ⇔ nothing reachable / ownership unknown yet.
 *
 * Covers: boot-result recording, status sync (recovery reload), the
 * probe-then-spawn outcome from `start_local_gateway`, and the reset on
 * stop.
 *
 * ── Liveness (single-authority model, 2026-09-22 incident) ────────────
 * `gatewayAlive` facts pinned here:
 *   - CONNACK / a successful probe ⇒ `alive`; the death-classifier watch
 *     is the ONLY writer of `dead`.
 *   - A plain `checkHealth()` fast failure is DISPLAY-only (ADR-051) and
 *     must never write `dead`; a timeout writes nothing at all.
 *   - A superseded / cancelled probe exits silently — a stale question
 *     must never pollute a newer fact (this was the 21s-late response
 *     that clobbered a healed status).
 *   - Three timeouts in a row DO act — they rebuild the MQTT connection
 *     (a transport that answers nothing is half-dead, not merely slow),
 *     and still write no verdict.
 *   - The classifier tick retries every 3s while MQTT is down and never
 *     preempts a probe already in flight.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// ── Mock Tauri invoke ────────────────────────────────────────────────────
// The store actions use `await import("@tauri-apps/api/core")` — vi.mock
// intercepts dynamic imports too.

const mockInvoke = vi.fn<(cmd: string) => Promise<unknown>>();
vi.mock("@tauri-apps/api/core", () => ({
    invoke: (cmd: string) => mockInvoke(cmd),
}));

// ── Mock the logger (keeps test output clean) ────────────────────────────

vi.mock("../lib/logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
    setLevel: () => {},
    getLevel: () => "off" as const,
}));

// ── Mock global fetch (checkHealth posts to /health) ─────────────────────
//
// `fetchMock` is re-pointed per test: `stubFetchOk` for the happy path,
// `stubFetchFailFast` for classifier death evidence, `stubFetchHanging`
// for timeout / cancellation coverage (abort-aware, so AbortController
// preemption semantics are exercised for real).

const fetchMock = vi.fn();
vi.stubGlobal("fetch", fetchMock);

function stubFetchOk() {
    fetchMock.mockImplementation(() =>
        Promise.resolve({
            ok: true,
            status: 200,
            json: () => Promise.resolve({ status: "healthy", version: "test" }),
        } as Response),
    );
}

function stubFetchFailFast() {
    fetchMock.mockImplementation(() => Promise.reject(new TypeError("Failed to fetch")));
}

function stubFetchHanging() {
    fetchMock.mockImplementation((_input: unknown, init?: RequestInit) => {
        return new Promise<Response>((_resolve, reject) => {
            const signal = init?.signal ?? null;
            if (signal?.aborted) {
                reject(new Error("AbortError"));
                return;
            }
            signal?.addEventListener("abort", () => reject(new Error("AbortError")), { once: true });
        });
    });
}

// ── SUT ──────────────────────────────────────────────────────────────────

import {
    useGatewayStore,
    cancelHealthProbe,
    markGatewayAlive,
    startGatewayDeathWatch,
    stopGatewayDeathWatch,
} from "./gatewayStore";

function resetStore() {
    useGatewayStore.setState({
        status: "disconnected",
        gatewayAlive: "unknown",
        gatewayUnreachable: false,
        health: null,
        localState: "idle",
        localOwnership: "none",
        migrationProgress: {},
    });
    mockInvoke.mockReset();
}

/** Simulate `get_local_gateway_status` returning `running`. */
function stubChildAlive(running: boolean) {
    mockInvoke.mockImplementation((cmd: string) => {
        if (cmd === "get_local_gateway_status") return Promise.resolve(running);
        if (cmd === "stop_local_gateway") return Promise.resolve(undefined);
        return Promise.reject(new Error(`Unexpected invoke: ${cmd}`));
    });
}

/** One probe that runs out its 10 s budget (inconclusive). Needs fake timers. */
async function hangingProbe() {
    stubFetchHanging();
    const p = useGatewayStore.getState().checkHealth();
    await vi.advanceTimersByTimeAsync(10_001);
    await p;
    // Settle the streak accounting (dynamic import + invoke are both async).
    await vi.advanceTimersByTimeAsync(1);
}

/** One probe that answers. Needs fake timers. */
async function answerProbe() {
    stubFetchOk();
    const p = useGatewayStore.getState().checkHealth();
    await vi.advanceTimersByTimeAsync(1);
    await p;
}

beforeEach(() => {
    resetStore();
    // Tear down module-level liveness machinery left over from the
    // previous test: an in-flight probe would leak into this one, and a
    // running watch would keep ticking against stale stubs.
    stopGatewayDeathWatch();
    cancelHealthProbe("test-reset");
    stubFetchOk();
    fetchMock.mockClear();
});

afterEach(() => {
    // Every fake-timer test must hand the clock back to the runner.
    vi.useRealTimers();
});

describe("gatewayStore.localOwnership (single-topology)", () => {
    it("starts at none / idle", () => {
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("none");
        expect(s.localState).toBe("idle");
    });

    it("recordBootResult owned → running + owned (Desktop spawned)", () => {
        useGatewayStore.getState().recordBootResult({
            base_url: "http://127.0.0.1:19876",
            ownership: "owned",
        });
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("owned");
        expect(s.localState).toBe("running");
    });

    it("recordBootResult foreign → stopped + foreign, NOT running (adopted)", () => {
        useGatewayStore.getState().recordBootResult({
            base_url: "http://127.0.0.1:19876",
            ownership: "foreign",
        });
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("foreign");
        // No in-process child ⇒ the localState machine must NOT report
        // running — stop/restart buttons stay hidden.
        expect(s.localState).toBe("stopped");
    });

    it("checkLocalStatus alive child ⇔ owned (recovery reload path)", async () => {
        stubChildAlive(true);
        await useGatewayStore.getState().checkLocalStatus();
        const s = useGatewayStore.getState();
        expect(s.localState).toBe("running");
        expect(s.localOwnership).toBe("owned");
    });

    it("checkLocalStatus with no child does NOT clobber foreign ownership", async () => {
        // Boot recorded a foreign adoption; the status probe later finds
        // no child (expected — Desktop never spawned it).
        useGatewayStore.getState().recordBootResult({
            base_url: "http://127.0.0.1:19876",
            ownership: "foreign",
        });
        stubChildAlive(false);
        await useGatewayStore.getState().checkLocalStatus();
        const s = useGatewayStore.getState();
        expect(s.localState).toBe("stopped");
        expect(s.localOwnership).toBe("foreign");
    });

    it("startLocalGateway spawns (owned) → running + owned", async () => {
        // First probe: no child alive.
        stubChildAlive(false);
        // start_local_gateway answers "owned" (Desktop spawned it).
        mockInvoke.mockImplementation((cmd: string) => {
            if (cmd === "start_local_gateway") {
                return Promise.resolve({
                    base_url: "http://127.0.0.1:19876",
                    ownership: "owned",
                });
            }
            return Promise.reject(new Error(`Unexpected invoke: ${cmd}`));
        });

        await useGatewayStore.getState().startLocalGateway();
        const s = useGatewayStore.getState();
        expect(s.localState).toBe("running");
        expect(s.localOwnership).toBe("owned");
        // Probe-then-spawn: health was re-checked after boot.
        expect(s.status).toBe("connected");
    });

    it("startLocalGateway adopts existing Gateway (foreign) → stopped + foreign", async () => {
        stubChildAlive(false);
        mockInvoke.mockImplementation((cmd: string) => {
            if (cmd === "start_local_gateway") {
                // A Gateway was already answering at the URL; nothing was
                // spawned. Desktop must record "foreign" and NOT show
                // managed controls.
                return Promise.resolve({
                    base_url: "http://127.0.0.1:19876",
                    ownership: "foreign",
                });
            }
            return Promise.reject(new Error(`Unexpected invoke: ${cmd}`));
        });

        await useGatewayStore.getState().startLocalGateway();
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("foreign");
        expect(s.localState).toBe("stopped");
        expect(s.status).toBe("connected");
    });

    it("stopLocalGateway resets ownership to none", async () => {
        useGatewayStore.getState().recordBootResult({
            base_url: "http://127.0.0.1:19876",
            ownership: "owned",
        });
        mockInvoke.mockImplementation((cmd: string) => {
            if (cmd === "stop_local_gateway") return Promise.resolve(undefined);
            return Promise.reject(new Error(`Unexpected invoke: ${cmd}`));
        });

        await useGatewayStore.getState().stopLocalGateway();
        const s = useGatewayStore.getState();
        expect(s.localOwnership).toBe("none");
        expect(s.localState).toBe("stopped");
        expect(s.status).toBe("disconnected");
        // The user stopped it — liveness is a confirmed fact.
        expect(s.gatewayAlive).toBe("dead");
    });
});

describe("gatewayStore.gatewayAlive (single-authority liveness)", () => {
    it("starts at unknown", () => {
        expect(useGatewayStore.getState().gatewayAlive).toBe("unknown");
    });

    it("checkHealth success writes alive + connected", async () => {
        await useGatewayStore.getState().checkHealth();
        const s = useGatewayStore.getState();
        expect(s.status).toBe("connected");
        expect(s.gatewayAlive).toBe("alive");
        expect(s.health?.status).toBe("healthy");
    });

    it("markGatewayAlive records a CONNACK proof", () => {
        markGatewayAlive("mqtt-connack");
        expect(useGatewayStore.getState().gatewayAlive).toBe("alive");
    });

    it("a fast probe failure is display-only — it never writes dead", async () => {
        useGatewayStore.setState({ status: "connected" });
        stubFetchFailFast();
        await useGatewayStore.getState().checkHealth();
        const s = useGatewayStore.getState();
        // ADR-051 display rule: a probe failing while `connected` is a
        // genuine outage as far as the DISPLAY goes ...
        expect(s.status).toBe("error");
        // ... but the death conviction belongs to the classifier alone.
        expect(s.gatewayAlive).toBe("unknown");
    });

    it("a newer checkHealth supersedes the in-flight probe — the loser stays silent", async () => {
        stubFetchHanging();
        const first = useGatewayStore.getState().checkHealth();
        stubFetchOk();
        const second = useGatewayStore.getState().checkHealth();
        await Promise.all([first, second]);
        const s = useGatewayStore.getState();
        expect(s.status).toBe("connected");
        expect(s.gatewayAlive).toBe("alive");
        expect(fetchMock).toHaveBeenCalledTimes(2);
    });

    it("cancelHealthProbe aborts the in-flight probe silently (CONNACK hand-off)", async () => {
        useGatewayStore.setState({ status: "connecting" });
        stubFetchHanging();
        const p = useGatewayStore.getState().checkHealth();
        cancelHealthProbe("mqtt-connected");
        await p;
        const s = useGatewayStore.getState();
        // A cancelled question carries no verdict: neither connected nor
        // dead — the CONNACK is the newer fact and it is never touched.
        expect(s.status).toBe("connecting");
        expect(s.gatewayAlive).toBe("unknown");
    });

    it("a 10s timeout is inconclusive — nothing is written", async () => {
        vi.useFakeTimers();
        useGatewayStore.setState({ status: "connecting" });
        stubFetchHanging();
        const p = useGatewayStore.getState().checkHealth();
        await vi.advanceTimersByTimeAsync(10_001);
        await p;
        const s = useGatewayStore.getState();
        expect(s.status).toBe("connecting");
        expect(s.gatewayAlive).toBe("unknown");
    });

    /**
     * Half-dead transport (2026-10-04 relay incident): yamux ping/pong kept
     * answering while the device stream stopped delivering, so every probe
     * timed out forever — inconclusive by invariant 2, and nothing anywhere
     * acted. Three timeouts in a row now rebuild the connection, which is
     * still NOT a verdict about the Gateway.
     */
    it("three inconclusive probes in a row force a fresh connection", async () => {
        vi.useFakeTimers();
        mockInvoke.mockImplementation((cmd: string) =>
            cmd === "force_reconnect_mqtt"
                ? Promise.resolve(undefined)
                : Promise.reject(new Error(`Unexpected invoke: ${cmd}`)),
        );
        // Normalise the module-level streak before counting: a probe that
        // answered zeroes it (see the next test).
        await answerProbe();
        mockInvoke.mockClear();
        for (let i = 0; i < 3; i++) {
            await hangingProbe();
        }
        expect(mockInvoke).toHaveBeenCalledTimes(1);
        expect(mockInvoke).toHaveBeenCalledWith("force_reconnect_mqtt");
        // Rebuild, not convict: the streak never writes a liveness verdict.
        const s = useGatewayStore.getState();
        expect(s.gatewayAlive).not.toBe("dead");
        expect(s.status).toBe("connected");
    });

    it("a probe that answered resets the inconclusive streak", async () => {
        vi.useFakeTimers();
        mockInvoke.mockImplementation((cmd: string) =>
            cmd === "force_reconnect_mqtt"
                ? Promise.resolve(undefined)
                : Promise.reject(new Error(`Unexpected invoke: ${cmd}`)),
        );
        await answerProbe();
        // Outlive any cooldown left behind by another test's forced reconnect.
        await vi.advanceTimersByTimeAsync(61_000);
        mockInvoke.mockClear();
        // 2 + 2 timeouts split by a successful probe never reach the limit.
        await hangingProbe();
        await hangingProbe();
        await answerProbe();
        await hangingProbe();
        await hangingProbe();
        expect(mockInvoke).not.toHaveBeenCalled();
    });
});

describe("gatewayStore death classifier (MQTT-down watch)", () => {
    it("a fast network failure while MQTT is down declares dead", async () => {
        stubFetchFailFast();
        startGatewayDeathWatch();
        await vi.waitFor(() => {
            expect(useGatewayStore.getState().gatewayAlive).toBe("dead");
        });
        expect(useGatewayStore.getState().status).toBe("error");
        stopGatewayDeathWatch();
    });

    it("a classifier timeout never declares dead", async () => {
        vi.useFakeTimers();
        stubFetchHanging();
        startGatewayDeathWatch();
        await vi.advanceTimersByTimeAsync(11_000);
        expect(useGatewayStore.getState().gatewayAlive).toBe("unknown");
        stopGatewayDeathWatch();
    });

    it("the tick never preempts an in-flight probe", async () => {
        vi.useFakeTimers();
        stubFetchHanging();
        startGatewayDeathWatch();
        // Ticks at 0s and 3s; the second must detect the in-flight probe
        // and bail out instead of spawning a parallel one.
        await vi.advanceTimersByTimeAsync(3_500);
        expect(fetchMock).toHaveBeenCalledTimes(1);
        stopGatewayDeathWatch();
    });

    it("stopGatewayDeathWatch halts retries", async () => {
        vi.useFakeTimers();
        stubFetchFailFast();
        startGatewayDeathWatch();
        await vi.advanceTimersByTimeAsync(1);
        expect(useGatewayStore.getState().gatewayAlive).toBe("dead");
        stopGatewayDeathWatch();
        fetchMock.mockClear();
        await vi.advanceTimersByTimeAsync(9_000);
        expect(fetchMock).not.toHaveBeenCalled();
    });
});
