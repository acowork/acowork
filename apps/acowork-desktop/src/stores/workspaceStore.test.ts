/**
 * Workspace store tests.
 *
 * Regression coverage for setPromptFile: the Runtime responds with
 * `{ ok, ws_id }`, NOT a full WorkspaceDir. The store used to replace the
 * local entry with that response, corrupting the workspaces list
 * (id/path/access became undefined) — the same envelope-as-entity bug
 * class as the user profile PUT.
 */

import { describe, it, expect, vi, afterEach } from "vitest";
import { useWorkspaceStore } from "./workspaceStore";
import type { WorkspaceDir } from "./workspaceStore";

function makeWorkspace(overrides: Partial<WorkspaceDir> = {}): WorkspaceDir {
    return {
        id: "ws-1",
        path: "/tmp/ws",
        alias: null,
        access: "read-write",
        added_at: "2026-08-01T00:00:00Z",
        select_count: 0,
        last_selected_at: null,
        prompt_file: null,
        ...overrides,
    };
}

afterEach(() => {
    vi.unstubAllGlobals();
});

describe("workspaceStore.setPromptFile", () => {
    it("keeps the local entry intact and patches prompt_file from the sent value", async () => {
        // Runtime echoes { ok, ws_id } — NOT a WorkspaceDir.
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({
                    ok: true,
                    status: 200,
                    json: () => Promise.resolve({ ok: true, ws_id: "ws-1" }),
                } as Response),
            ),
        );

        useWorkspaceStore.setState({ workspaces: [makeWorkspace()] });

        const result = await useWorkspaceStore.getState().setPromptFile("agent-1", "ws-1", "CLAUDE.md");

        expect(result).toBe(true);
        const ws = useWorkspaceStore.getState().workspaces[0];
        // Regression: the old code replaced the entry with { ok, ws_id },
        // leaving id/path/access undefined and breaking tree/file ops.
        expect(ws.id).toBe("ws-1");
        expect(ws.path).toBe("/tmp/ws");
        expect(ws.access).toBe("read-write");
        expect(ws.prompt_file).toBe("CLAUDE.md");
    });

    it("clears prompt_file when null is passed (toggle off)", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({
                    ok: true,
                    status: 200,
                    json: () => Promise.resolve({ ok: true, ws_id: "ws-1" }),
                } as Response),
            ),
        );

        useWorkspaceStore.setState({
            workspaces: [makeWorkspace({ prompt_file: "AGENTS.md" })],
        });

        await useWorkspaceStore.getState().setPromptFile("agent-1", "ws-1", null);

        const ws = useWorkspaceStore.getState().workspaces[0];
        expect(ws.prompt_file).toBeNull();
        expect(ws.id).toBe("ws-1");
    });

    it("returns false and keeps state on HTTP error", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({
                    ok: false,
                    status: 500,
                    statusText: "Internal Server Error",
                    text: () => Promise.resolve("boom"),
                } as Response),
            ),
        );

        useWorkspaceStore.setState({ workspaces: [makeWorkspace()] });

        const result = await useWorkspaceStore.getState().setPromptFile("agent-1", "ws-1", "CLAUDE.md");

        expect(result).toBe(false);
        expect(useWorkspaceStore.getState().workspaces[0].prompt_file).toBeNull();
    });
});

/**
 * ADR-087: a shared agent's workspace list is `AgentManage`, so a
 * non-owner caller gets 403. The old code logged it and left the list
 * empty, which the panel rendered as "未配置工作区" — telling the caller
 * to go add one they are not allowed to add.
 */
describe("workspaceStore.fetchWorkspaces — 403 is a state, not a silent empty", () => {
    afterEach(() => {
        useWorkspaceStore.getState().reset();
    });

    it("records deniedReason and does not present it as an empty list", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({
                    ok: false,
                    status: 403,
                    statusText: "Forbidden",
                    json: () => Promise.resolve({ error: "forbidden", required: "manage" }),
                } as Response),
            ),
        );

        await useWorkspaceStore.getState().fetchWorkspaces("agent-shared");

        const { deniedReason, loading } = useWorkspaceStore.getState();
        expect(deniedReason).toBe("manage");
        expect(loading).toBe(false);
    });

    it("does not pin deniedReason on a 5xx, which still owns the retry path", async () => {
        // 5xx is transient (Runtime boot) and must keep flowing through
        // with503Retry — treating it as "denied" would permanently show
        // the permission copy for what is a start-order race.
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({
                    ok: false,
                    status: 500,
                    statusText: "Internal Server Error",
                    json: () => Promise.resolve({ error: "boom" }),
                } as Response),
            ),
        );

        await useWorkspaceStore.getState().fetchWorkspaces("agent-1");

        expect(useWorkspaceStore.getState().deniedReason).toBeNull();
    });

    it("clears a previous denial once the caller becomes a guest", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({ workspaces: [] }) } as Response),
            ),
        );
        useWorkspaceStore.setState({ deniedReason: "manage" });

        await useWorkspaceStore.getState().fetchWorkspaces("agent-1");

        expect(useWorkspaceStore.getState().deniedReason).toBeNull();
    });
});

/**
 * The list is ONE global array while the UI reads it for `selectedAgentId`,
 * and starting another agent fetches for that one. Field report: right after
 * starting agent B, B's chat-surface workspace menu listed agent A's
 * workspaces, and picking one did nothing — it wrote a workspace id B does not
 * own, so the panel resolved nothing. Two store defects fed it:
 *   - every failure path left A's array in place (only `loading` was cleared),
 *   - `setSessionWorkspace` shared the fetch's stale-response counter, so a
 *     workspace write discarded the in-flight list — and its `loading: false`.
 */
describe("workspaceStore.fetchWorkspaces ownership", () => {
    it("a failed fetch for another agent claims the slot instead of leaving A's list", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(() =>
                Promise.resolve({ ok: false, status: 500, statusText: "boom", json: () => Promise.resolve({}) } as Response),
            ),
        );
        useWorkspaceStore.setState({ workspaces: [makeWorkspace({ id: "ws-a" })], workspacesAgentId: "agent-A" });

        await useWorkspaceStore.getState().fetchWorkspaces("agent-B");

        const s = useWorkspaceStore.getState();
        // An empty list says "unknown for B". A's list says "pick me", and B
        // cannot.
        expect(s.workspaces).toEqual([]);
        expect(s.workspacesAgentId).toBe("agent-B");
        expect(s.loading).toBe(false);
    });

    it("a concurrent workspace write does not swallow the in-flight list", async () => {
        let releaseList: ((v: Response) => void) | null = null;
        vi.stubGlobal(
            "fetch",
            vi.fn((input: string) =>
                String(input).includes("/workspaces/tree") || String(input).includes("/sessions/")
                    ? Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({}) } as Response)
                    : new Promise<Response>((res) => {
                        releaseList = res;
                      }),
            ),
        );
        useWorkspaceStore.setState({ workspaces: [makeWorkspace({ id: "ws-a" })], workspacesAgentId: "agent-A" });

        const inflight = useWorkspaceStore.getState().fetchWorkspaces("agent-B");
        // The user switches workspace while B's list is still on the wire.
        await useWorkspaceStore.getState().setSessionWorkspace("agent-B", "sess-1", "ws-b");
        releaseList!({
            ok: true,
            status: 200,
            json: () => Promise.resolve({ workspaces: [makeWorkspace({ id: "ws-b" })] }),
        } as Response);
        await inflight;

        const s = useWorkspaceStore.getState();
        expect(s.workspaces.map((w) => w.id)).toEqual(["ws-b"]);
        expect(s.workspacesAgentId).toBe("agent-B");
        // The discarded-result path used to leave this pinned at true.
        expect(s.loading).toBe(false);
    });
});
