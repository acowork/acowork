import { describe, expect, it } from "vitest";
import { fileTreeViewKey } from "./FileTree";

/**
 * The identity rule behind `WorkspaceExplorer`'s `key` on the tree. If this key
 * stops carrying a view identity, the tree's virtualizer offset leaks from one
 * view to the next — the same defect class as the unkeyed chat panes.
 */
const base = { agentId: "agent-1", workspaceId: "ws-1", sessionId: "s-1", fontSize: 14 };

describe("fileTreeViewKey", () => {
  it("changes with every input that identifies the view", () => {
    const key = fileTreeViewKey(base);
    expect(fileTreeViewKey({ ...base, agentId: "agent-2" })).not.toBe(key);
    expect(fileTreeViewKey({ ...base, workspaceId: "ws-2" })).not.toBe(key);
    expect(fileTreeViewKey({ ...base, sessionId: "s-2" })).not.toBe(key);
    expect(fileTreeViewKey({ ...base, fontSize: 16 })).not.toBe(key);
  });

  it("is stable for the same view, and treats 'no session' as its own view", () => {
    expect(fileTreeViewKey(base)).toBe(fileTreeViewKey({ ...base }));
    expect(fileTreeViewKey({ ...base, sessionId: null })).not.toBe(fileTreeViewKey(base));
  });

  it("cannot collide across field boundaries", () => {
    // The store keys these ids the same way (NUL-separated), so an id that
    // contains the separator is the only way two views could share a key.
    expect(
      fileTreeViewKey({ agentId: "a", workspaceId: "b:c", sessionId: "d", fontSize: 1 }),
    ).not.toBe(
      fileTreeViewKey({ agentId: "a:b", workspaceId: "c", sessionId: "d", fontSize: 1 }),
    );
  });
});