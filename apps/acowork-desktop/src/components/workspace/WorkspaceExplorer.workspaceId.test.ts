/**
 * Self-check for the right-panel workspace chain. The panel belongs to the
 * surface that is on screen:
 *
 *   execution surface → the active session's workspace (its real working
 *                       directory), else agent home
 *   Chat surface      → the Chat session's OWN workspace pick, falling back to
 *                       the session's (display default only) and then agent
 *                       home
 *
 * The Chat-surface branch is what keeps the composer chip and the panel from
 * showing two different workspaces — and what makes Chat-surface "Add to Chat"
 * coherent, since the tree the reference comes from is that same workspace. A
 * Chat-surface pick never re-points an execution session.
 */
import { describe, it, expect } from "vitest";
import { resolveExplorerWorkspaceId } from "./WorkspaceExplorer";

describe("resolveExplorerWorkspaceId", () => {
    const map = { "s-1": "w-a" };

    it("prefers the active execution session's workspace", () => {
        expect(resolveExplorerWorkspaceId("s-1", map, "w-draft", false)).toBe("w-a");
    });

    it("falls back to agent home for a session with no mapped workspace", () => {
        expect(resolveExplorerWorkspaceId("s-9", map, "w-draft", false)).toBe("__agent_home__");
    });

    it("execution surface with no session at all → agent home, draft ignored", () => {
        expect(resolveExplorerWorkspaceId(null, map, "w-draft", false)).toBe("__agent_home__");
    });

    it("Chat surface: the Chat session's own pick wins over the session's", () => {
        expect(resolveExplorerWorkspaceId("s-1", map, "w-draft", true)).toBe("w-draft");
    });

    it("Chat surface with no pick yet mirrors the session's workspace", () => {
        expect(resolveExplorerWorkspaceId("s-1", map, undefined, true)).toBe("w-a");
        expect(resolveExplorerWorkspaceId("s-1", map, null, true)).toBe("w-a");
    });

    it("Chat surface with neither a pick nor a session → agent home", () => {
        expect(resolveExplorerWorkspaceId(null, map, undefined, true)).toBe("__agent_home__");
        expect(resolveExplorerWorkspaceId("s-9", map, undefined, true)).toBe("__agent_home__");
    });
});
