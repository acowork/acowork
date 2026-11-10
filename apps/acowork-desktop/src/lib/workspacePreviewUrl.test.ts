// @vitest-environment jsdom
/**
 * `workspacePreviewUrl.ts` — the capability-URL builder for the workspace HTML
 * preview (ADR-112).
 *
 * Pinned behaviours, each one a design decision rather than an implementation
 * detail:
 *   - the ticket rides in the **path**, so relative sub-resource loads
 *     (`styles.css`, `assets/logo.svg`) inherit it. A `?ticket=` query would be
 *     dropped by relative resolution and every CSS/JS/font would 401 again;
 *   - per-segment encoding keeps spaces / non-ASCII filenames resolvable while
 *     `/` separators survive for the Gateway's `{*path}` route;
 *   - a ticket is minted per navigation, never cached across them — the
 *     signing key is per Gateway process and the Desktop cannot observe a
 *     restart, so a cached link could silently stop verifying (ADR-112 §决策 5);
 *   - concurrent callers still share one in-flight mint, so the document and
 *     the sub-resources of one navigation cost one call.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./config", () => ({ getGatewayUrl: () => "http://gateway:19876/" }));
vi.mock("./logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

import {
    __resetWorkspacePreviewUrlCache,
    composePreviewUrl,
    getPreviewTicket,
    getWorkspacePreviewUrl,
} from "./workspacePreviewUrl";

const fetchMock = vi.fn();

let ticketSeq = 0;

/** Queue a successful mint response for the next `fetch`. */
function respondWithTicket() {
    const seconds = Math.floor(Date.now() / 1000) + 8 * 3600;
    fetchMock.mockResolvedValueOnce({
        ok: true,
        status: 200,
        json: async () => ({ ticket: `v1.${seconds}.ticket${++ticketSeq}`, expires_at: seconds, ttl_secs: 28800 }),
    });
}

beforeEach(() => {
    __resetWorkspacePreviewUrlCache();
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
    ticketSeq = 0;
});

describe("composePreviewUrl", () => {
    it("puts the ticket in the path so relative sub-resources inherit it", () => {
        const url = composePreviewUrl({
            gatewayUrl: "http://gateway:19876/",
            ticket: "v1.1000.deadbeef",
            agentId: "agent-1",
            workspaceId: "ws-1",
            relPath: "pages/demo/index.html",
        });
        expect(url).toBe(
            "http://gateway:19876/workspace-preview/v1.1000.deadbeef/agent-1/ws-1/pages/demo/index.html",
        );
        // The half that actually fixes the bug: a sibling asset resolved
        // against this URL still carries the ticket.
        expect(new URL("styles.css", url).pathname).toBe(
            "/workspace-preview/v1.1000.deadbeef/agent-1/ws-1/pages/demo/styles.css",
        );
    });

    it("encodes each path segment but keeps separators and the ticket intact", () => {
        const url = composePreviewUrl({
            gatewayUrl: "http://gateway:19876",
            ticket: "v1.1000.deadbeef",
            agentId: "agent/1",
            workspaceId: "ws 1",
            relPath: "my page/sub dir/中文.html",
        });
        expect(url).toContain("/workspace-preview/v1.1000.deadbeef/agent%2F1/ws%201/");
        expect(url).toContain("my%20page/sub%20dir/");
        // `.` before the version segment must never be percent-encoded — the
        // ticket grammar is `v1.<exp>.<mac>`.
        expect(url).toContain("/workspace-preview/v1.1000.deadbeef/");
        expect(new URL(url).pathname).toBe(
            "/workspace-preview/v1.1000.deadbeef/agent%2F1/ws%201/my%20page/sub%20dir/" +
                encodeURIComponent("中文.html"),
        );
    });

    it("tolerates a gateway base url with a trailing slash", () => {
        expect(
            composePreviewUrl({
                gatewayUrl: "http://gateway:19876////",
                ticket: "t",
                agentId: "a",
                workspaceId: "w",
                relPath: "index.html",
            }),
        ).toBe("http://gateway:19876/workspace-preview/t/a/w/index.html");
    });
});

describe("getPreviewTicket", () => {
    it("mints per navigation — a cached ticket could outlive the signing key", async () => {
        respondWithTicket();
        respondWithTicket();
        const first = await getPreviewTicket("agent-1", "ws-1");
        const second = await getPreviewTicket("agent-1", "ws-1");

        // Two opens, two mints: nothing cached can be replayed after a Gateway
        // restart, because nothing is cached.
        expect(second).not.toBe(first);
        expect(fetchMock).toHaveBeenCalledTimes(2);
        expect(fetchMock.mock.calls[1][0]).toBe(
            "http://gateway:19876/api/agents/agent-1/workspaces/preview-ticket",
        );
        expect(fetchMock.mock.calls[0][1]).toMatchObject({
            method: "POST",
            body: JSON.stringify({ workspace_id: "ws-1" }),
        });
    });

    it("mints separately per workspace — a ticket is bound to one workspace", async () => {
        respondWithTicket();
        respondWithTicket();
        const wsOne = await getPreviewTicket("agent-1", "ws-1");
        const wsTwo = await getPreviewTicket("agent-1", "ws-2");

        expect(wsTwo).not.toBe(wsOne);
        expect(fetchMock).toHaveBeenCalledTimes(2);
    });

    it("shares one in-flight mint between concurrent callers", async () => {
        respondWithTicket();
        const [a, b] = await Promise.all([
            getPreviewTicket("agent-1", "ws-1"),
            getPreviewTicket("agent-1", "ws-1"),
        ]);
        expect(a).toBe(b);
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });

    it("propagates a mint failure instead of caching a broken ticket", async () => {
        fetchMock.mockResolvedValueOnce({ ok: false, status: 403, json: async () => ({}) });
        await expect(getPreviewTicket("agent-1", "ws-1")).rejects.toThrow(/HTTP 403/);

        // The failure must not poison the cache: the next call tries again.
        respondWithTicket();
        await expect(getPreviewTicket("agent-1", "ws-1")).resolves.toContain("v1.");
        expect(fetchMock).toHaveBeenCalledTimes(2);
    });

    it("rejects a response that is missing the ticket", async () => {
        fetchMock.mockResolvedValueOnce({
            ok: true,
            status: 200,
            json: async () => ({ expires_at: 1 }),
        });
        await expect(getPreviewTicket("agent-1", "ws-1")).rejects.toThrow(/missing ticket/);
    });

});

describe("getWorkspacePreviewUrl", () => {
    it("composes the url from the minted ticket and the requested file", async () => {
        respondWithTicket();
        const url = await getWorkspacePreviewUrl("agent-1", "ws-1", "pages/index.html");
        expect(url).toMatch(
            /^http:\/\/gateway:19876\/workspace-preview\/v1\.\d+\.ticket\d+\/agent-1\/ws-1\/pages\/index\.html$/,
        );
        expect(fetchMock).toHaveBeenCalledTimes(1);
    });
});
