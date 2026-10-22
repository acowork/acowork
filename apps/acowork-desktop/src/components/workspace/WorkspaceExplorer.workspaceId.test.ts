/**
 * Self-check for the right-panel workspace fallback chain (the fix for
 * "closing every exec session blanks the workspace file list"):
 *   active exec session → its mapped workspace (else agent home)
 *   no exec session     → the Chat session's `session_config` draft
 *                         workspace (else agent home)
 */
import { describe, it, expect } from "vitest";
import { resolveExplorerWorkspaceId } from "./WorkspaceExplorer";

describe("resolveExplorerWorkspaceId", () => {
    const map = { "s-1": "w-a" };

    it("prefers the active execution session's workspace", () => {
        expect(resolveExplorerWorkspaceId("s-1", map, "w-draft")).toBe("w-a");
    });

    it("falls back to agent home for a session with no mapped workspace", () => {
        expect(resolveExplorerWorkspaceId("s-9", map, "w-draft")).toBe("__agent_home__");
    });

    it("uses the Chat draft workspace when no exec session is active", () => {
        expect(resolveExplorerWorkspaceId(null, map, "w-draft")).toBe("w-draft");
    });

    it("falls back to agent home with neither session nor draft", () => {
        expect(resolveExplorerWorkspaceId(null, map, undefined)).toBe("__agent_home__");
        expect(resolveExplorerWorkspaceId(null, map, null)).toBe("__agent_home__");
    });
});
