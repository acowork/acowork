/**
 * Self-check for the SplashScreen 5s-fallback candidate chooser.
 *
 * Scenario: remote Gateway at URL `OLD_URL` is unreachable, but the
 * user's URL history contains `CANDIDATE_URL` which IS reachable. After
 * the 5s fallback window elapses, SplashScreen must:
 *   1. Probe the history concurrently.
 *   3. Render the candidate chooser INSIDE the loading view (NOT yet the
 *      hard-timeout view — 12s in remote/relay, 30s in local, see
 *      REMOTE_MAX_WAIT_MS / LOCAL_MAX_WAIT_MS) so the user can pick one
 *      before the timeout fires.
 *   2. Picking a candidate persists the URL and re-runs the boot flow.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, act } from "@testing-library/react";
import { SplashScreen } from "./SplashScreen";
import { useSettingsStore } from "../../stores/settingsStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { LOCAL_GATEWAY_URL } from "../../lib/gatewayUrl";

const invokeMock = vi.fn(async (..._args: unknown[]) => ({}));
vi.mock("@tauri-apps/api/core", () => ({
    invoke: (...args: unknown[]) => (invokeMock as unknown as (...a: unknown[]) => unknown)(...args),
}));

const OLD_URL = "http://192.168.1.10:19876";
const CANDIDATE_URL = "http://192.168.3.10:19876";
const RELAY_URL = "https://c8cb2bed-ffd4-4821-a913-718618482157.relay.acowork.ai";
// Must mirror `UNREACHABLE_HINT_MS` — the shared boot/runtime budget
// in lib/connectivity/gatewayConnectivity.ts.
const CANDIDATE_FALLBACK_MS = 5_000;
// Must mirror REMOTE_MAX_WAIT_MS in SplashScreen.tsx (the chooser tests
// all run in remote/relay mode, where the boot budget is the short one).
const MAX_WAIT_MS = 12_000;

async function flushMicrotasks() {
    for (let i = 0; i < 10; i++) {
        await act(async () => {});
    }
}

/** Extract the URL the gateway config was last pushed with. */
function lastPushedGatewayUrl(): string | undefined {
    const calls = invokeMock.mock.calls.filter((c) => c[0] === "set_gateway_config");
    if (calls.length === 0) return undefined;
    const payload = calls[calls.length - 1][1] as { config?: { url?: string } } | undefined;
    return payload?.config?.url;
}

/** Collect every URL passed to set_gateway_config, in order. */
function allPushedGatewayUrls(): string[] {
    return invokeMock.mock.calls
        .filter((c) => c[0] === "set_gateway_config")
        .map((c) => {
            const payload = c[1] as { config?: { url?: string } } | undefined;
            return payload?.config?.url ?? "";
        });
}

/** The full config (mode + url) of the LAST set_gateway_config push. */
function lastPushedGatewayConfig(): { mode?: string; url?: string } | undefined {
    const calls = invokeMock.mock.calls.filter((c) => c[0] === "set_gateway_config");
    if (calls.length === 0) return undefined;
    const payload = calls[calls.length - 1][1] as
        | { config?: { mode?: string; url?: string } }
        | undefined;
    return payload?.config;
}

describe("SplashScreen 5s candidate chooser", () => {
    beforeEach(() => {
        vi.useFakeTimers();
        vi.stubGlobal("requestAnimationFrame", () => 0);
        invokeMock.mockClear();
        invokeMock.mockImplementation(async (..._args: unknown[]) => ({}));
        // `OLD_URL` (192.168.1.x) is unreachable. `CANDIDATE_URL`
        // (192.168.3.x) is reachable. Probe URLs are `${url}/health` so
        // match on host substring, not "candidate".
        vi.stubGlobal(
            "fetch",
            vi.fn((url: string) => {
                if (String(url).includes("192.168.3")) {
                    return Promise.resolve({
                        ok: true,
                        json: async () => ({ status: "ok", version: "0" }),
                    } as Response);
                }
                return Promise.reject(new Error("unreachable"));
            }),
        );
        useSettingsStore.setState({
            gatewayMode: "remote",
            relayActive: false,
            gatewayUrl: OLD_URL,
            gatewayUrlHistory: [OLD_URL, CANDIDATE_URL],
            lastGoodUrl: null,
        });
        useGatewayStore.setState({
            status: "disconnected",
            health: null,
            localState: "idle",
            localOwnership: "none",
            candidates: [],
        });
    });

    afterEach(() => {
        vi.useRealTimers();
        vi.unstubAllGlobals();
        vi.restoreAllMocks();
    });

    it("surfaces a candidate chooser inside the loading view after the 5s fallback", async () => {
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();

        // Before the 5s window: no chooser yet.
        expect(screen.queryByText(/reachable/i)).toBeNull();
        expect(useGatewayStore.getState().candidates).toHaveLength(0);

        // Advance just past the fallback window (still well below the
        // hard timeout — 12s in remote/relay — so the timeout view doesn't
        // appear).
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();

        // Candidates populated in the store.
        const candidates = useGatewayStore.getState().candidates;
        expect(candidates.length).toBeGreaterThan(0);
        expect(candidates[0].url).toBe(CANDIDATE_URL);

        // Chooser is rendered with the candidate URL visible.
        expect(screen.getByText(CANDIDATE_URL)).toBeTruthy();
        // Hard timeout view has NOT appeared yet.
        expect(screen.queryByText("Retry Connection")).toBeNull();
    });

    /**
     * ADR-115: the fallback probe no longer short-circuits on a loopback
     * address. It used to (`if (gatewayMode === "local") return 0`), which
     * made "localhost" a one-way door: a dead local Gateway left the
     * splash spinning with no way to reach the LAN Gateway the user had
     * used before. Localhost is just another address now, so its history
     * candidates are offered exactly like everyone else's.
     */
    it("offers history candidates from a loopback address too", async () => {
        useSettingsStore.setState({
            gatewayMode: "local",
            gatewayUrl: LOCAL_GATEWAY_URL,
            gatewayUrlHistory: [LOCAL_GATEWAY_URL, CANDIDATE_URL],
        });
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();
        expect(useGatewayStore.getState().candidates.map((c) => c.url)).toContain(
            CANDIDATE_URL,
        );
        expect(screen.getByText(CANDIDATE_URL)).toBeTruthy();
    });

    it("does not surface a candidate when ALL probes fail", async () => {
        // Override: everything fails.
        vi.stubGlobal(
            "fetch",
            vi.fn(() => Promise.reject(new Error("down"))),
        );
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();
        expect(useGatewayStore.getState().candidates).toHaveLength(0);
        expect(screen.queryByText(CANDIDATE_URL)).toBeNull();
    });

    it("happy path: connected before fallback fires → candidates stay empty", async () => {
        // Make everything (incl. OLD_URL) reachable.
        vi.stubGlobal(
            "fetch",
            vi.fn(async () => ({
                ok: true,
                json: async () => ({ status: "ok", version: "0" }),
            } as Response)),
        );
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();
        // Health poll succeeds → status flips to connected.
        await act(async () => {
            vi.advanceTimersByTime(2_000);
        });
        await flushMicrotasks();
        expect(useGatewayStore.getState().status).toBe("connected");
        // Even after the fallback window, no chooser appears because the
        // chooser is gated on `candidates.length > 0`.
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS);
        });
        await flushMicrotasks();
        expect(useGatewayStore.getState().candidates).toHaveLength(0);
        expect(screen.queryByText(CANDIDATE_URL)).toBeNull();
    });

    it("candidate rows use the global accent button style", async () => {
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();

        const row = screen.getByRole("button", { name: new RegExp(CANDIDATE_URL) });
        // Solid accent fill, not an outline row — same global button
        // style the banner pills use.
        expect(row.className).toContain("btn-accent");
        expect(row.className).not.toMatch(/\bborder-/);
    });

    /**
     * Regression: picking a candidate must leave the store at the picked
     * URL, not roll it back to the mount-time (stale) value.
     *
     * Pre-fix bug: `gatewayUrlInput` was frozen at the mount-time store
     * URL (the old, dead address) and only refreshed when the user
     * edited the input. `handleRetry` then compared
     * `gatewayUrlInput (old) !== store (new)`, rewrote the store back to
     * the dead address, and the subsequent `bootGateway` call landed
     * there too — three back-to-back set_gateway_config calls in the
     * order (new, old, old), the reconnect stranded on the dead host.
     */
    it("picking a candidate does NOT roll the store back to the dead URL", async () => {
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();

        // Advance to the 5s fallback so the candidate chooser appears.
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();

        const candidates = useGatewayStore.getState().candidates;
        expect(candidates.length).toBeGreaterThan(0);
        expect(candidates[0].url).toBe(CANDIDATE_URL);

        // Reset the call log so we only inspect what happens from the
        // click onward (the boot path emits its own config push before
        // the user is involved).
        invokeMock.mockClear();

        // Click the candidate. The chooser renders <button> elements
        // keyed by URL; fireEvent.click on the candidate <button> drives
        // onPick synchronously.
        const pickBtn = screen.getByRole("button", { name: new RegExp(CANDIDATE_URL) });
        fireEvent.click(pickBtn);
        await flushMicrotasks();

        // Store must reflect the picked URL, NOT be rolled back to OLD_URL.
        expect(useSettingsStore.getState().gatewayUrl).toBe(CANDIDATE_URL);

        // No `set_gateway_config` push may carry the dead URL after the
        // pick. We tolerate the first push being the picked URL (from
        // `setGatewayUrl` inside `onPick`) plus the `bootGateway` push;
        // any push with the old URL would mean `handleRetry` reverted.
        const pushedUrls = allPushedGatewayUrls();
        expect(pushedUrls.length).toBeGreaterThan(0);
        expect(pushedUrls).not.toContain(OLD_URL);
        // And the final config push lands on the picked host.
        expect(lastPushedGatewayUrl()).toBe(CANDIDATE_URL);
    });

    /**
     * Regression for the relay+http outage: booting in relay mode, the 5s
     * fallback probe surfaced a reachable LAN candidate (http). The old
     * `onPick` persisted the URL but left the mode at `relay` — Rust's
     * `relay_mqtt_wss_url` only accepts `https://` device domains, so
     * every `connect_mqtt` was rejected forever while HTTP health probes
     * still answered (the connection looked "alive" but chat was dead).
     *
     * ADR-115 moves that invariant into the store: `commitGatewayUrl`
     * drops `relayActive` for any non-https address, so the illegal
     * (relay, http) pair can never be persisted. The FINAL
     * `set_gateway_config` push must therefore be (remote, candidate-url).
     */
    it("picking an http candidate while in relay mode drops relay and lands on remote", async () => {
        useSettingsStore.setState({
            gatewayMode: "relay",
            relayActive: true,
            gatewayUrl: RELAY_URL,
            gatewayUrlHistory: [RELAY_URL, CANDIDATE_URL],
            lastGoodUrl: null,
        });
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();

        // Advance to the 5s fallback so the candidate chooser appears.
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();

        const candidates = useGatewayStore.getState().candidates;
        expect(candidates.length).toBeGreaterThan(0);
        expect(candidates[0].url).toBe(CANDIDATE_URL);

        // Reset the call log so we only inspect what happens from the
        // click onward.
        invokeMock.mockClear();

        const pickBtn = screen.getByRole("button", { name: new RegExp(CANDIDATE_URL) });
        fireEvent.click(pickBtn);
        await flushMicrotasks();

        // Relay opt-in dropped in the same step as the URL pick.
        expect(useSettingsStore.getState().relayActive).toBe(false);
        expect(useSettingsStore.getState().gatewayMode).toBe("remote");
        expect(useSettingsStore.getState().gatewayUrl).toBe(CANDIDATE_URL);
        // The final config push is the legal (remote, http-lan) combo —
        // never relay+http, which Rust would reject forever.
        expect(lastPushedGatewayConfig()).toEqual({ mode: "remote", url: CANDIDATE_URL });
    });

    /**
     * Regression: pre-fix, clicking a candidate row gave the user zero
     * feedback until the boot round-trip completed (multi-second). The
     * row looked identical before and after the click, so a click that
     * "did nothing" felt broken. After the fix, the picked row must
     * synchronously swap to aria-busy + a spinner + a "Connecting..."
     * label, and the dismiss / sibling rows must be disabled so an
     * impatient multi-click cannot fan out into parallel boot
     * pipelines.
     */
    it("clicking a candidate row gives immediate picking feedback (spinner + Connecting label + disabled siblings)", async () => {
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();

        expect(useGatewayStore.getState().candidates.length).toBeGreaterThan(0);
        const pickBtn = screen.getByRole("button", { name: new RegExp(CANDIDATE_URL) });

        // No `await` between click and assertion: we want the SYNCHRONOUS
        // render that React commits in the same act() pass as the click
        // handler. The picking state is set BEFORE `handleRetry` schedules
        // any work, so the commit delivers the feedback in this very
        // paint — there is no round-trip to wait for.
        act(() => {
            fireEvent.click(pickBtn);
        });

        // The picked row now shows aria-busy + a "Connecting..." label
        // instead of the latency "ms" suffix.
        const sameTickBtn = screen.getByRole("button", {
            name: new RegExp(CANDIDATE_URL),
        });
        expect(sameTickBtn.getAttribute("aria-busy")).toBe("true");
        expect(sameTickBtn.textContent).toMatch(/Connecting/i);
        expect(sameTickBtn.textContent).not.toMatch(/\bms\b/);

        // Dismiss button is disabled while a pick is in flight so the
        // chooser cannot be hidden under the picking spinner.
        const dismissBtn = screen.getByText(/keep waiting/i) as HTMLButtonElement;
        expect(dismissBtn.disabled).toBe(true);
    });

    /**
     * Regression: pre-fix, the status line below the chooser kept reading
     * "Starting Gateway: <address>" / "Connecting to Gateway: <address>"
     * through the entire pick, so the user had no text-level cue that the
     * click had registered either. The candidate-pick path must swap
     * the status text to "Connecting to <picked-url>..." in the same
     * tick as the click.
     */
    it("clicking a candidate row updates the status text to 'Connecting to <url>...' immediately", async () => {
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();

        const pickBtn = screen.getByRole("button", { name: new RegExp(CANDIDATE_URL) });
        act(() => {
            fireEvent.click(pickBtn);
        });

        // The status line is the only paragraph in the loading view
        // whose text contains both "Connecting to" and the picked URL.
        // `screen.getByText` with a regex is substring-and-case-
        // insensitive, and the candidate row's own "Connecting..." label
        // is filtered out because it never contains the URL.
        const esc = CANDIDATE_URL.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
        const statusLine = screen.getByText(
            new RegExp(`Connecting to.*${esc}`),
        );
        expect(statusLine).toBeTruthy();
    });

    /**
     * Regression: pre-fix, a candidate pick that the Gateway refused
     * to honour (URL went away between probe and pick — typical
     * when the candidate was a transient dev box) left the URL in
     * the candidate list. The user would click the same dead address
     * repeatedly, each click silently re-spawning the boot pipeline.
     * The pick-failure path must drop the failing URL from the
     * candidates list so the only surviving rows are the ones
     * actually worth trying again.
     */
    it("a candidate pick that fails to connect prunes the dead URL from the list and falls back to the timeout view", async () => {
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();
        expect(useGatewayStore.getState().candidates.length).toBeGreaterThan(0);

        // The candidate was reachable at probe time (per the beforeEach
        // mock), but it has gone away by the time the user picks it.
        // Every subsequent `/health` fetch (doCheck inside handleRetry)
        // now rejects. The status stays "disconnected" so the timeout
        // view eventually re-asserts.
        vi.stubGlobal(
            "fetch",
            vi.fn(() => Promise.reject(new Error("now down"))),
        );

        const pickBtn = screen.getByRole("button", { name: new RegExp(CANDIDATE_URL) });
        fireEvent.click(pickBtn);
        await flushMicrotasks();

        // Timeout view replaced the chooser: the transient candidate list is
        // gone ("Keep waiting" only exists inside the chooser). The URL may
        // still be visible in the recovery view's address picker — it is the
        // address currently selected, and the picker is the thing that must
        // remain available (ADR-115) — but it must no longer be offered as a
        // candidate row to click again.
        expect(screen.getByText("Retry Connection")).toBeTruthy();
        expect(screen.queryByText(/keep waiting/i)).toBeNull();

        // The dead URL is gone from the store; a fresh probe (not part
        // of this test) is the only way it can come back.
        const remaining = useGatewayStore.getState().candidates;
        expect(remaining.find((c) => c.url === CANDIDATE_URL)).toBeUndefined();
    });

    /**
     * Regression: pre-fix, sibling candidate rows stayed clickable
     * while a pick was in flight. A frustrated user would click
     * multiple candidates in quick succession, fanning out into
     * concurrent boot pipelines. After the fix, only the picked row
     * stays clickable; every other row is `disabled` until the pick
     * settles.
     */
    it("clicking one candidate disables all sibling rows until the pick settles", async () => {
        // Seed a second reachable candidate so we can verify the
        // sibling row is locked while the picked one is in flight.
        const CANDIDATE_URL_2 = "http://192.168.3.20:19876";
        useSettingsStore.setState({
            gatewayUrlHistory: [OLD_URL, CANDIDATE_URL, CANDIDATE_URL_2],
        });
        render(<SplashScreen onReady={vi.fn()} />);
        await flushMicrotasks();
        await act(async () => {
            vi.advanceTimersByTime(CANDIDATE_FALLBACK_MS + 100);
        });
        await flushMicrotasks();

        const candidates = useGatewayStore.getState().candidates;
        expect(candidates.length).toBeGreaterThanOrEqual(2);

        const firstBtn = screen.getByRole("button", { name: new RegExp(CANDIDATE_URL) });
        act(() => {
            fireEvent.click(firstBtn);
        });

        // The picked row stays interactive (aria-busy, not disabled by
        // the visual rule because the row opacity is overridden).
        const firstAfter = screen.getByRole("button", {
            name: new RegExp(CANDIDATE_URL),
        });
        expect(firstAfter.getAttribute("aria-busy")).toBe("true");

        // The sibling row is hard-disabled so it cannot accept another
        // click that would race the in-flight boot. The host project
        // does not pull in @testing-library/jest-dom, so assert on the
        // raw `disabled` HTML attribute (which is exactly what
        // `toBeDisabled` would check anyway).
        const secondBtn = screen.getByRole("button", {
            name: new RegExp(CANDIDATE_URL_2),
        }) as HTMLButtonElement;
        expect(secondBtn.disabled).toBe(true);
    });
});