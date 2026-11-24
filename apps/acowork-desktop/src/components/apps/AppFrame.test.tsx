/**
 * Hosted-application iframe host contract (ADR-116).
 *
 * The two security-relevant facts pinned here are: the capability ticket is
 * delivered in the URL **fragment** (never a query string), and the sandbox
 * deliberately omits `allow-same-origin` so the app runs in an opaque origin.
 * PM and DOC are both exercised as the first registered apps.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { act, render, screen, waitFor } from "@testing-library/react";
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

/** An app → host message, as the bridge posts it (source identity is checked). */
function fromApp(frame: HTMLIFrameElement, data: Record<string, unknown>) {
    window.dispatchEvent(
        new MessageEvent("message", {
            data: { channel: "acowork", ...data },
            origin: new URL(frame.src).origin,
            source: frame.contentWindow,
        }),
    );
}

async function mountedFrame() {
    const frame = await waitFor(() => {
        const el = document.querySelector("iframe");
        expect(el).not.toBeNull();
        return el as HTMLIFrameElement;
    });
    return frame;
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
            expect(url.searchParams.get("control")).toBe("1");
            expect(url.hash).toBe(`#ticket=${TICKET}`);
            expect(src).not.toContain("?ticket");
            expect(mint).toHaveBeenCalledWith(id);
        },
    );

    it("gives the app a real origin, so its storage APIs work", async () => {
        render(<AppFrame app={makeApp("pm")} />);
        const frame = await waitFor(() => {
            const el = document.querySelector("iframe");
            expect(el).not.toBeNull();
            return el as HTMLIFrameElement;
        });

        const sandbox = frame.getAttribute("sandbox") ?? "";
        expect(sandbox).toContain("allow-scripts");
        expect(sandbox).toContain("allow-downloads");
        // Opaque origin = localStorage/IndexedDB throw SecurityError, which is
        // how a hosted app loses its preferences and its user fonts.
        expect(sandbox).toContain("allow-same-origin");
        // Still no top navigation / popups escape beyond a popup window.
        expect(sandbox).not.toContain("allow-top-navigation");
        expect(sandbox).not.toContain("allow-modals");
    });

    it("puts visual-preview search in the query and keeps the ticket in the fragment", async () => {
        render(
            <AppFrame
                app={makeApp("figcraft")}
                search="?file=designs/a.figcraft&control=1"
            />,
        );
        const frame = await waitFor(() => {
            const el = document.querySelector("iframe");
            expect(el).not.toBeNull();
            return el as HTMLIFrameElement;
        });
        const src = frame.getAttribute("src") ?? "";
        const url = new URL(src);
        expect(url.pathname).toBe("/api/apps/figcraft/ui/");
        expect(url.searchParams.get("file")).toBe("designs/a.figcraft");
        expect(url.searchParams.get("control")).toBe("1");
        expect(url.hash).toBe(`#ticket=${TICKET}`);
        expect(src).not.toContain("?ticket");
    });

    it("re-posts theme when the shell light/dark class changes", async () => {
        const posted: Array<Record<string, unknown>> = [];
        render(<AppFrame app={makeApp("pm")} />);
        const frame = await waitFor(() => {
            const el = document.querySelector("iframe");
            expect(el).not.toBeNull();
            return el as HTMLIFrameElement;
        });
        Object.defineProperty(frame, "contentWindow", {
            configurable: true,
            value: {
                postMessage: (data: Record<string, unknown>) => {
                    posted.push(data);
                },
            },
        });
        frame.dispatchEvent(new Event("load"));
        expect(posted.some((p) => p.type === "theme")).toBe(true);

        posted.length = 0;
        document.documentElement.classList.add("dark");
        await waitFor(() => {
            expect(posted.some((p) => p.type === "theme" && p.theme === "dark")).toBe(true);
        });

        posted.length = 0;
        document.documentElement.classList.remove("dark");
        await waitFor(() => {
            expect(posted.some((p) => p.type === "theme" && p.theme === "light")).toBe(true);
        });
    });


    it("hands the document over only after the bridge says hello, and stops once it is acked", async () => {
        vi.useFakeTimers();
        try {
            const posted: Array<Record<string, unknown>> = [];
            const openBytes = new Uint8Array([1, 2, 3]);
            const openFile = { name: "a.docx", getBytes: () => openBytes };
            await act(async () => {
                render(<AppFrame app={makeApp("figcraft")} openFile={openFile} />);
            });
            const frame = document.querySelector("iframe") as HTMLIFrameElement;
            expect(frame).not.toBeNull();
            Object.defineProperty(frame, "contentWindow", {
                configurable: true,
                value: { postMessage: (data: Record<string, unknown>) => posted.push(data) },
            });
            frame.dispatchEvent(new Event("load"));
            // Nothing is delivered to an app that has not answered the host.
            expect(posted.some((p) => p.type === "openFile")).toBe(false);

            await act(async () => {
                fromApp(frame, { type: "hello", bridge: 2 });
            });
            const first = posted.find((p) => p.type === "openFile");
            expect(first).toBeTruthy();
            expect(typeof first?.id).toBe("number");

            // Unacked: the same delivery is retried, under the same id.
            await act(async () => {
                await vi.advanceTimersByTimeAsync(2000);
            });
            const attempts = posted.filter((p) => p.type === "openFile");
            expect(attempts.length).toBeGreaterThan(1);
            expect(new Set(attempts.map((p) => p.id)).size).toBe(1);

            // Acked: nothing more goes out.
            const before = attempts.length;
            await act(async () => {
                fromApp(frame, { type: "opened", id: first?.id });
                await vi.advanceTimersByTimeAsync(20_000);
            });
            expect(posted.filter((p) => p.type === "openFile").length).toBe(before);
        } finally {
            vi.useRealTimers();
        }
    });


    it("shows an error with a retry instead of a loader when the app never answers", async () => {
        vi.useFakeTimers();
        try {
            await act(async () => {
                render(<AppFrame app={makeApp("figcraft")} />);
            });
            expect(document.querySelector("iframe")).not.toBeNull();
            await act(async () => {
                await vi.advanceTimersByTimeAsync(26_000);
            });
            expect(screen.getByText(/did not start/)).toBeTruthy();
            expect(screen.getByRole("button", { name: /Try again/ })).toBeTruthy();
        } finally {
            vi.useRealTimers();
        }
    });

    it("surfaces an exception the app forwarded from inside the frame", async () => {
        render(<AppFrame app={makeApp("figcraft")} />);
        const frame = await mountedFrame();
        await act(async () => {
            fromApp(frame, { type: "error", message: "Cannot read properties of undefined" });
        });
        expect(screen.getByText(/Cannot read properties of undefined/)).toBeTruthy();
    });

    it("keeps a loading surface over the app until it has the document", async () => {
        render(
            <AppFrame
                app={makeApp("doc")}
                openFile={{ name: "report.docx", getBytes: () => new Uint8Array([1, 2, 3]) }}
            />,
        );
        const frame = await mountedFrame();
        const posted: Array<Record<string, unknown>> = [];
        Object.defineProperty(frame, "contentWindow", {
            configurable: true,
            value: { postMessage: (data: Record<string, unknown>) => posted.push(data) },
        });
        frame.dispatchEvent(new Event("load"));

        // Boot: the frame is a black rectangle and the app has drawn nothing.
        expect(screen.getByText(/Opening 文档库/)).toBeTruthy();

        await act(async () => {
            fromApp(frame, { type: "hello", bridge: 2 });
        });
        const delivery = posted.find((p) => p.type === "openFile");
        expect(delivery).toBeTruthy();
        // The app's start screen is not the file the user opened: the cover
        // stays until the app has taken the document.
        expect(screen.getByText(/Opening report\.docx/)).toBeTruthy();

        await act(async () => {
            fromApp(frame, { type: "opened", id: delivery?.id });
            // Uncovered one repaint after the ack, not on the ack itself.
            await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
        });
        expect(screen.queryByText(/Opening/)).toBeNull();
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
