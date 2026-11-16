/**
 * Hosted-application iframe host contract (ADR-116).
 *
 * The two security-relevant facts pinned here are: the capability ticket is
 * delivered in the URL **fragment** (never a query string), and the sandbox
 * deliberately omits `allow-same-origin` so the app runs in an opaque origin.
 * PM and DOC are both exercised as the first registered apps.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { AppFrame } from "./AppFrame";
import { mintAppTicket, type AppDto } from "../../lib/apps-api";

vi.mock("../../lib/config", () => ({
    getGatewayUrl: () => "http://127.0.0.1:19876",
}));

vi.mock("../../lib/apps-api", async (importOriginal) => {
    const actual = await importOriginal<typeof import("../../lib/apps-api")>();
    return { ...actual, mintAppTicket: vi.fn() };
});

const GATEWAY = "http://127.0.0.1:19876";
const TICKET = "v1.1899999999.68656d616e.00112233445566778899aabbccddeeff";
const mint = vi.mocked(mintAppTicket);

function makeApp(id: string): AppDto {
    return {
        id,
        name: id === "pm" ? "项目管理" : "文档库",
        version: "1.0.0",
        description: "",
        icon: "",
        builtin: true,
        enabled: true,
        status: "running",
        port: null,
        runtime: "legacy",
        mcp_path: "/mcp",
        mcp_gateway_path: `/api/${id}/mcp`,
        mcp_url: null,
        ui_path: "/ui/",
        ui_mode: "webview",
        nav_label: id,
        nav_icon: "",
        nav_order: 0,
        health_path: "/health",
        permissions: [],
    };
}

beforeEach(() => {
    mint.mockReset();
    mint.mockResolvedValue(TICKET);
});

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("AppFrame", () => {
    it.each(["pm", "doc"])(
        "loads %s through the Gateway proxy with the ticket in the fragment",
        async (id) => {
            render(<AppFrame app={makeApp(id)} />);

            const frame = await waitFor(() => {
                const el = document.querySelector("iframe");
                expect(el).not.toBeNull();
                return el as HTMLIFrameElement;
            });

            const src = frame.getAttribute("src") ?? "";
            const url = new URL(src);
            expect(url.origin).toBe(GATEWAY);
            expect(url.pathname).toBe(`/api/apps/${id}/ui/`);
            expect(url.search).toBe("");
            expect(url.hash).toBe(`#ticket=${TICKET}`);
            expect(src).not.toContain("?ticket");
            expect(mint).toHaveBeenCalledWith(id);
        },
    );

    it("keeps the iframe in an opaque origin (no allow-same-origin)", async () => {
        render(<AppFrame app={makeApp("pm")} />);
        const frame = await waitFor(() => {
            const el = document.querySelector("iframe");
            expect(el).not.toBeNull();
            return el as HTMLIFrameElement;
        });

        const sandbox = frame.getAttribute("sandbox") ?? "";
        expect(sandbox).toContain("allow-scripts");
        expect(sandbox).not.toContain("allow-same-origin");
    });

    it("shows an inline error instead of an empty frame when minting fails", async () => {
        mint.mockRejectedValue(new Error("unauthorized"));
        render(<AppFrame app={makeApp("doc")} />);

        await waitFor(() => {
            expect(screen.getByText(/Failed to open/)).toBeTruthy();
        });
        expect(screen.getByText("unauthorized")).toBeTruthy();
        expect(document.querySelector("iframe")).toBeNull();
    });
});
