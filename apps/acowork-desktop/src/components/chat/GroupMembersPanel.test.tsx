/**
 * Pure-function tests for the group members panel search box.
 *
 * The filter must be case-insensitive, trim surrounding whitespace, return
 * everything on an empty query, and keep unresolved members (null display
 * name) findable by their raw id — the behaviours that make the WeChat-style
 * member search feel right, each cheap to regress without the store graph.
 */
import { describe, it, expect } from "vitest";
import { filterMembers } from "./GroupMembersPanel";
import type { ResolvedMember } from "../../views/pm/pmMembers";

function member(over: Partial<ResolvedMember> & { instance_id: string }): ResolvedMember {
  return {
    kind: "agent",
    displayName: null,
    avatarUrl: null,
    builtinAvatarId: null,
    unresolved: false,
    ...over,
  };
}

const rows: ResolvedMember[] = [
  member({ instance_id: "a-1", displayName: "Alice", kind: "agent" }),
  member({ instance_id: "u-2", displayName: "Bob 鲍勃", kind: "user" }),
  member({ instance_id: "deadbeef-3", displayName: null, unresolved: true }),
];

describe("filterMembers (group members search)", () => {
  it("returns every member on an empty or whitespace-only query", () => {
    expect(filterMembers(rows, "")).toHaveLength(3);
    expect(filterMembers(rows, "   ")).toHaveLength(3);
  });

  it("matches display name case-insensitively", () => {
    expect(filterMembers(rows, "ali").map((m) => m.instance_id)).toEqual(["a-1"]);
    expect(filterMembers(rows, "BOB").map((m) => m.instance_id)).toEqual(["u-2"]);
  });

  it("matches a CJK substring in the display name", () => {
    expect(filterMembers(rows, "鲍勃").map((m) => m.instance_id)).toEqual(["u-2"]);
  });

  it("keeps unresolved members findable by their raw id", () => {
    expect(filterMembers(rows, "dead").map((m) => m.instance_id)).toEqual([
      "deadbeef-3",
    ]);
  });

  it("returns an empty list when nothing matches", () => {
    expect(filterMembers(rows, "zzz")).toEqual([]);
  });
});
