/**
 * Tests for the gateway connectivity module — the single funnel for
 * every network-connection scenario.
 *
 * Pinned here:
 *   - the user-facing unreachable hint fires after ONE budget
 *     (`UNREACHABLE_HINT_MS`) regardless of the death classifier, and
 *     a recovery inside the budget cancels it;
 *   - `probeKnownCandidates` is the shared boot/runtime probe: folds
 *     reachable hosts in (fastest first), offers localhost as an ordinary
 *     candidate, skips an already-recovered snapshot, and discards results
 *     that settle after recovery.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []) }));

vi.mock("../logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
    setLevel: () => {},
    getLevel: () => "off" as const,
}));

import {
    UNREACHABLE_HINT_MS,
    initGatewayConnectivity,
    probeKnownCandidates,
} from "./gatewayConnectivity";
import { useChatStore } from "../../stores/chatStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSettingsStore } from "../../stores/settingsStore";

const OLD_URL = "http://192.168.1.10:19876";
const CANDIDATE_URL = "http://192.168.3.10:19876";
const LOOPBACK_URL = "http://127.0.0.1:19876";

const fetchMock = vi.fn();
vi.stubGlobal("fetch", fetchMock);

function stubReachableOnly(hostSubstring: string) {
    fetchMock.mockImplementation((url: string) => {
        if (String(url).includes(hostSubstring)) {
            return Promise.resolve({ ok: true, json: async () => ({ status: "ok" }) } as Response);
        }
        return Promise.reject(new Error("unreachable"));
    });
}

describe("unreachable hint (non-CONNACKed ≥ budget, any cause)", () => {
    let cleanup: (() => void) | null = null;

    beforeEach(() => {
        vi.useFakeTimers();
        fetchMock.mockReset();
        fetchMock.mockImplementation(() => Promise.reject(new Error("down")));
        useChatStore.setState({ mqttConnected: false });
        useGatewayStore.setState({ status: "connecting", gatewayUnreachable: false });
        // Loopback + local mode keeps the auto-heal guard inert so the
        // only observable side effect is the hint under test.
        useSettingsStore.setState({ gatewayMode: "local", gatewayUrl: LOOPBACK_URL });
    });

    afterEach(() => {
        cleanup?.();
        cleanup = null;
        vi.useRealTimers();
    });

    it("raises gatewayUnreachable after the budget — the classifier is never consulted", () => {
        cleanup = initGatewayConnectivity();
        expect(useGatewayStore.getState().gatewayUnreachable).toBe(false);
        vi.advanceTimersByTime(UNREACHABLE_HINT_MS - 1);
        expect(useGatewayStore.getState().gatewayUnreachable).toBe(false);
        vi.advanceTimersByTime(1);
        expect(useGatewayStore.getState().gatewayUnreachable).toBe(true);
    });

    it("a recovery inside the budget cancels the countdown", () => {
        cleanup = initGatewayConnectivity();
        vi.advanceTimersByTime(3_000);
        useChatStore.setState({ mqttConnected: true });
        vi.advanceTimersByTime(UNREACHABLE_HINT_MS);
        expect(useGatewayStore.getState().gatewayUnreachable).toBe(false);
    });

    it("a rise clears an already-surfaced hint", () => {
        cleanup = initGatewayConnectivity();
        vi.advanceTimersByTime(UNREACHABLE_HINT_MS);
        expect(useGatewayStore.getState().gatewayUnreachable).toBe(true);
        useChatStore.setState({ mqttConnected: true });
        expect(useGatewayStore.getState().gatewayUnreachable).toBe(false);
    });

    it("a mounted module with a healthy connection never raises the hint", () => {
        useChatStore.setState({ mqttConnected: true });
        cleanup = initGatewayConnectivity();
        vi.advanceTimersByTime(UNREACHABLE_HINT_MS * 3);
        expect(useGatewayStore.getState().gatewayUnreachable).toBe(false);
    });
});

describe("probeKnownCandidates (shared boot/runtime candidate probe)", () => {
    beforeEach(() => {
        fetchMock.mockReset();
        useChatStore.setState({ mqttConnected: false });
        useGatewayStore.setState({
            status: "connecting",
            gatewayUnreachable: true,
            candidates: [],
        });
        useSettingsStore.setState({
            gatewayMode: "remote",
            gatewayUrl: OLD_URL,
            gatewayUrlHistory: [OLD_URL, CANDIDATE_URL],
        });
    });

    it("folds reachable hosts in, fastest first", async () => {
        stubReachableOnly("192.168.3");
        const found = await probeKnownCandidates();
        expect(found).toBe(1);
        const candidates = useGatewayStore.getState().candidates;
        expect(candidates.map((c) => c.url)).toEqual([CANDIDATE_URL]);
    });

    it("offers localhost as a candidate even from a remote address (ADR-115)", async () => {
        // There is no "local mode" short-circuit any more: a Desktop stuck on
        // a dead LAN address must be able to reach its own machine's Gateway.
        useSettingsStore.setState({ gatewayUrl: OLD_URL, gatewayUrlHistory: [OLD_URL] });
        stubReachableOnly("127.0.0.1");
        expect(await probeKnownCandidates()).toBe(1);
        expect(useGatewayStore.getState().candidates.map((c) => c.url)).toEqual([
            LOOPBACK_URL,
        ]);
    });

    it("returns 0 when already recovered (a probe would be noise)", async () => {
        useChatStore.setState({ mqttConnected: true });
        expect(await probeKnownCandidates()).toBe(0);
        expect(fetchMock).not.toHaveBeenCalled();
    });

    it("discards results that settle after recovery", async () => {
        // Every host (localhost + the history entries) gets its own probe
        // promise, so collect ALL resolvers — a single-host resolver would
        // leave the batch hanging and the test would time out.
        const resolvers: ((v: Response) => void)[] = [];
        fetchMock.mockImplementation(
            () =>
                new Promise<Response>((resolve) => {
                    resolvers.push(resolve);
                }),
        );
        const p = probeKnownCandidates();
        // Gateway comes back while the probe is in flight.
        useChatStore.setState({ mqttConnected: true });
        for (const resolve of resolvers) {
            resolve({ ok: true, json: async () => ({}) } as Response);
        }
        expect(await p).toBe(0);
        expect(useGatewayStore.getState().candidates).toHaveLength(0);
    });

    it("nothing left to probe clears stale candidates without a request", async () => {
        useGatewayStore.setState({
            candidates: [{ url: "http://stale:19876", latencyMs: 1 }],
        });
        // Current address IS localhost and it is the only known address →
        // no alternative to offer.
        useSettingsStore.setState({
            gatewayUrl: LOOPBACK_URL,
            gatewayUrlHistory: [LOOPBACK_URL],
            lastGoodUrl: LOOPBACK_URL,
        });
        expect(await probeKnownCandidates()).toBe(0);
        expect(useGatewayStore.getState().candidates).toHaveLength(0);
        expect(fetchMock).not.toHaveBeenCalled();
    });
});
