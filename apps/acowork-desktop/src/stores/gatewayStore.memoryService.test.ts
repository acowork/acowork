/**
 * ADR-101 P4: `gatewayStore.refreshMemoryService` — the readiness read that
 * gates the memory panel and the global-search memory tab.
 *
 * Facts pinned here:
 *   - `memory_service` comes from `GET /api/status` (`/health` is
 *     liveness-only and cannot carry it);
 *   - a read that says nothing (transport failure, non-2xx, a Gateway older
 *     than ADR-101 that has no such key) must NOT rewrite the value we already
 *     have — an unanswered question is not evidence;
 *   - only the FIRST read of a session gets the bounded boot-race re-check
 *     (the supervisor may still be waiting for the service to bind its port
 *     when the Desktop asks). Every later `ready: false` is a real answer and
 *     must not turn one refresh into three requests;
 *   - stopping the local Gateway clears it, so the next boot re-runs the
 *     discovery.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

const mockInvoke = vi.fn<(cmd: string) => Promise<unknown>>();
vi.mock("@tauri-apps/api/core", () => ({
    invoke: (cmd: string) => mockInvoke(cmd),
}));

vi.mock("../lib/logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
    setLevel: () => {},
    getLevel: () => "off" as const,
}));

const fetchMock = vi.fn();
vi.stubGlobal("fetch", fetchMock);

import { useGatewayStore } from "./gatewayStore";

/** A `GET /api/status` body carrying the ADR-101 projection. */
function statusBody(ready: boolean, mcpUrl: string | null = null) {
    return {
        version: "test",
        agents_installed: 0,
        agents_running: 0,
        uptime_secs: 1,
        mqtt_port: 19875,
        memory_service: { ready, mcp_url: mcpUrl },
    };
}

/** Serve `bodies` in order; the last entry repeats once they run out. */
function stubStatusSequence(bodies: Array<{ ok: boolean; body: unknown }>) {
    let n = 0;
    fetchMock.mockImplementation(() => {
        const entry = bodies[Math.min(n, bodies.length - 1)];
        n += 1;
        return Promise.resolve({
            ok: entry.ok,
            status: entry.ok ? 200 : 500,
            json: () => Promise.resolve(entry.body),
        } as Response);
    });
    return () => n;
}

const ok = (body: unknown) => ({ ok: true, body });
const fail = () => ({ ok: false, body: {} });

function reset() {
    useGatewayStore.setState({ memoryService: null });
    fetchMock.mockReset();
    mockInvoke.mockReset();
}

beforeEach(reset);

describe("gatewayStore.refreshMemoryService", () => {
    it("reads memory_service off /api/status and stores ready + mcp_url", async () => {
        stubStatusSequence([ok(statusBody(true, "http://127.0.0.1:19876/api/memory/mcp"))]);

        await useGatewayStore.getState().refreshMemoryService();

        expect(fetchMock.mock.calls[0][0]).toMatch(/\/api\/status$/);
        expect(useGatewayStore.getState().memoryService).toEqual({
            ready: true,
            mcp_url: "http://127.0.0.1:19876/api/memory/mcp",
        });
    });

    it("normalises a missing mcp_url to null (auto_inject_mcp off)", async () => {
        stubStatusSequence([ok(statusBody(true, undefined))]);

        await useGatewayStore.getState().refreshMemoryService();

        expect(useGatewayStore.getState().memoryService).toEqual({ ready: true, mcp_url: null });
    });

    it("keeps the previous value when the read fails", async () => {
        useGatewayStore.setState({ memoryService: { ready: true, mcp_url: "x" } });
        fetchMock.mockImplementation(() => Promise.reject(new TypeError("Failed to fetch")));

        await expect(useGatewayStore.getState().refreshMemoryService()).resolves.toBeUndefined();

        expect(useGatewayStore.getState().memoryService).toEqual({ ready: true, mcp_url: "x" });
    });

    it("keeps the previous value on a non-2xx", async () => {
        useGatewayStore.setState({ memoryService: { ready: true, mcp_url: "x" } });
        stubStatusSequence([fail()]);

        await useGatewayStore.getState().refreshMemoryService();

        expect(useGatewayStore.getState().memoryService).toEqual({ ready: true, mcp_url: "x" });
    });

    it("treats a pre-ADR-101 Gateway (no memory_service key) as saying nothing", async () => {
        useGatewayStore.setState({ memoryService: { ready: true, mcp_url: "x" } });
        stubStatusSequence([
            ok({ version: "0.0.0", agents_installed: 0, agents_running: 0, uptime_secs: 1, mqtt_port: 1 }),
        ]);

        await useGatewayStore.getState().refreshMemoryService();

        expect(useGatewayStore.getState().memoryService).toEqual({ ready: true, mcp_url: "x" });
    });

    it("re-checks a not-ready FIRST read, and stops as soon as it answers ready", async () => {
        const calls = stubStatusSequence([
            ok(statusBody(false)),
            ok(statusBody(true, "http://127.0.0.1:19876/api/memory/mcp")),
        ]);

        await useGatewayStore.getState().refreshMemoryService({ retryDelayMs: 0 });

        expect(calls()).toBe(2);
        expect(useGatewayStore.getState().memoryService?.ready).toBe(true);
    });

    it("bounds the boot re-check at two extra reads", async () => {
        const calls = stubStatusSequence([ok(statusBody(false))]);

        await useGatewayStore.getState().refreshMemoryService({ retryDelayMs: 0 });

        // 1 original + 2 retries — a disabled service costs three reads once
        // per session, not a poll.
        expect(calls()).toBe(3);
        expect(useGatewayStore.getState().memoryService?.ready).toBe(false);
    });

    it("takes a later not-ready answer at face value (no retries)", async () => {
        useGatewayStore.setState({ memoryService: { ready: true, mcp_url: null } });
        const calls = stubStatusSequence([ok(statusBody(false))]);

        await useGatewayStore.getState().refreshMemoryService({ retryDelayMs: 0 });

        expect(calls()).toBe(1);
        expect(useGatewayStore.getState().memoryService?.ready).toBe(false);
    });

    it("stopLocalGateway clears the projection so the next boot re-discovers", async () => {
        useGatewayStore.setState({ memoryService: { ready: true, mcp_url: null } });
        mockInvoke.mockImplementation(() => Promise.resolve(undefined));

        await useGatewayStore.getState().stopLocalGateway();

        expect(useGatewayStore.getState().memoryService).toBeNull();
    });
});

