/**
 * WorkspaceExplorer.search.test.ts — the search box's mode split.
 *
 * The box answers two different questions through two different endpoints.
 * Getting the wiring wrong is silent (an empty dropdown looks like "no
 * match"), so the pair is pinned here: `files` → `/workspaces/find`,
 * `content` → `/workspaces/search` with `match_file_names=false` plus the
 * case/whole-word flags the two extra icons set.
 *
 * Run: `npx vitest run src/components/workspace/WorkspaceExplorer.search.test.ts`
 */

import { beforeEach, describe, expect, it, vi } from "vitest";
import { searchRows } from "./WorkspaceExplorer";

vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://gw" }));
vi.mock("../common/SetiIcon", () => ({ SetiIcon: () => null }));

function jsonResponse(body: unknown): Response {
    return { ok: true, json: async () => body } as unknown as Response;
}

function stubFetch(body: unknown) {
    const fetchMock = vi.fn(async () => jsonResponse(body));
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
}

const signal = () => new AbortController().signal;

describe("searchRows", () => {
    beforeEach(() => {
        vi.unstubAllGlobals();
        vi.restoreAllMocks();
    });

    it("file mode asks /workspaces/find and splits the path", async () => {
        const fetchMock = stubFetch({
            matches: [{ name: "a.ts", relPath: "src/a.ts", score: 1 }],
        });

        const rows = await searchRows("files", "agent-1", "ws-1", "a", signal());

        expect(String(fetchMock.mock.calls[0][0])).toContain(
            "/api/agents/agent-1/workspaces/find",
        );
        expect(rows).toEqual([{ name: "a.ts", relPath: "src/a.ts", dir: "src" }]);
    });

    it("content mode asks /workspaces/search, content-only, with the flags", async () => {
        const fetchMock = stubFetch({
            matches: [{ file: "core/b.rs", line: 12, text: "  needle here" }],
        });

        const rows = await searchRows("content", "agent-1", "__agent_home__", "needle", signal(), {
            caseSensitive: true,
            wholeWord: true,
        });

        const url = String(fetchMock.mock.calls[0][0]);
        expect(url).toContain("/api/agents/agent-1/workspaces/search?");
        // Content mode must not fall back to names — that is the whole point
        // of the radio split.
        expect(url).toContain("match_file_names=false");
        expect(url).toContain("case_sensitive=true");
        expect(url).toContain("whole_word=true");
        // Agent home has no workspace id to send.
        expect(url).not.toContain("workspace_id");
        expect(rows).toEqual([
            { name: "b.rs", relPath: "core/b.rs", dir: "core", line: 12, text: "  needle here" },
        ]);
    });

    it("content mode reads a failed scan as no rows", async () => {
        vi.stubGlobal("fetch", vi.fn(async () => ({ ok: false }) as unknown as Response));

        expect(await searchRows("content", "agent-1", "ws-1", "x", signal())).toEqual([]);
    });
});