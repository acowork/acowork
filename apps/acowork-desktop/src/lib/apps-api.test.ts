/**
 * Hosted-application API client contract (ADR-116).
 *
 * The Desktop is a presentation layer: every app call must go through the
 * Gateway control plane. These tests pin the exact routes, methods and error
 * surface used by `ExtensionsView` / `AppFrame`, so a silent URL change (e.g.
 * addressing an app's loopback port directly) is caught here.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { listApps, setAppEnabled, mintAppTicket, AppApiError, type AppDto } from "./apps-api";

vi.mock("./config", () => ({ getGatewayUrl: () => "http://127.0.0.1:19876" }));

const GATEWAY = "http://127.0.0.1:19876";
const calls: Array<{ url: string; init?: RequestInit }> = [];

function mockFetch(body: unknown, status = 200) {
    vi.stubGlobal(
        "fetch",
        vi.fn((url: string | URL, init?: RequestInit) => {
            calls.push({ url: String(url), init });
            return Promise.resolve({
                ok: status >= 200 && status < 300,
                status,
                statusText: status === 200 ? "OK" : "Error",
                json: () => Promise.resolve(body),
            } as Response);
        }),
    );
}

function makeApp(id: string): AppDto {
    return {
        id,
        name: id,
        version: "1.0.0",
        description: "",
        icon: "",
        builtin: true,
        enabled: true,
        status: "running",
        port: id === "pm" ? 18082 : 18081,
        runtime: "legacy",
        mcp_path: "/mcp",
        mcp_gateway_path: `/api/${id}/mcp`,
        mcp_url: `${GATEWAY}/api/${id}/mcp`,
        ui_path: "/ui/",
        ui_mode: "webview",
        nav_label: id,
        nav_icon: "",
        nav_order: 0,
        health_path: "/health",
        permissions: ["context.read"],
    };
}

beforeEach(() => {
    calls.length = 0;
});

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("apps-api control plane", () => {
    it("lists pm and doc through GET /api/apps", async () => {
        mockFetch({ apps: [makeApp("pm"), makeApp("doc")] });
        const apps = await listApps();

        expect(apps.map((a) => a.id)).toEqual(["pm", "doc"]);
        expect(calls).toHaveLength(1);
        expect(calls[0].url).toBe(`${GATEWAY}/api/apps`);
        expect(calls[0].init?.method).toBeUndefined();
    });

    it("returns an empty list when the envelope has no apps key", async () => {
        mockFetch({});
        await expect(listApps()).resolves.toEqual([]);
    });

    it("maps enable / disable to the exact POST routes", async () => {
        mockFetch({ ok: true });
        await setAppEnabled("pm", false);
        await setAppEnabled("doc", true);

        expect(calls.map((c) => [c.url, c.init?.method])).toEqual([
            [`${GATEWAY}/api/apps/pm/disable`, "POST"],
            [`${GATEWAY}/api/apps/doc/enable`, "POST"],
        ]);
    });

    it("URL-encodes the app id", async () => {
        mockFetch({ ok: true });
        await setAppEnabled("my app", false);
        expect(calls[0].url).toBe(`${GATEWAY}/api/apps/my%20app/disable`);
    });

    it("mints a ticket via POST and returns it without leaking it into the URL", async () => {
        const ticket = "v1.1899999999.68656d616e.00112233445566778899aabbccddeeff";
        mockFetch({ ticket, expires_at: 1899999999, ui_url: "/api/apps/pm/ui/" });
        await expect(mintAppTicket("pm")).resolves.toBe(ticket);

        expect(calls[0].url).toBe(`${GATEWAY}/api/apps/pm/session-ticket`);
        expect(calls[0].init?.method).toBe("POST");
        expect(calls[0].url).not.toContain("ticket=");
    });

    it("surfaces backend failures as AppApiError with the server message", async () => {
        mockFetch({ error: "capability_not_declared" }, 403);
        await expect(mintAppTicket("doc")).rejects.toMatchObject({
            name: "AppApiError",
            status: 403,
            message: "capability_not_declared",
        });
        await expect(listApps()).rejects.toBeInstanceOf(AppApiError);
    });
});
