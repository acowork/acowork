/**
 * Pure-function tests for the group members panel search box.
 *
 * The filter must be case-insensitive, trim surrounding whitespace, return
 * everything on an empty query, and keep unresolved members (null display
 * name) findable by their raw id — the behaviours that make the WeChat-style
 * member search feel right, each cheap to regress without the store graph.
 */
import { describe, it, expect } from "vitest";
import { filterMembers, withOwner } from "./GroupMembersPanel";
import type { ResolvedMember } from "../../views/pm/pmMembers";
import type { PmProjectMember } from "../../lib/pm-types";

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

const pmMember = (
  instance_id: string,
  kind: "agent" | "user" = "agent",
): PmProjectMember => ({ instance_id, kind, added_at: "2026-01-01T00:00:00Z" });

describe("withOwner (group member set)", () => {
  it("folds the owner in when they are not a member (REST-created project)", () => {
    const project = {
      created_by: "u-owner",
      created_at: "2026-01-01T00:00:00Z",
      members: [pmMember("ag1"), pmMember("ag2")],
    };
    const got = withOwner(project);
    expect(got.map((x) => x.instance_id)).toEqual(["u-owner", "ag1", "ag2"]);
    // The owner joins as a human, not an agent.
    expect(got[0].kind).toBe("user");
  });

  it("does not duplicate the owner when the creator is already a member (MCP-created)", () => {
    const project = {
      created_by: "u-owner",
      created_at: "2026-01-01T00:00:00Z",
      members: [pmMember("u-owner", "user"), pmMember("ag1")],
    };
    const got = withOwner(project);
    expect(got.filter((x) => x.instance_id === "u-owner")).toHaveLength(1);
    expect(got).toHaveLength(2);
  });

  it("returns [] for a missing project", () => {
    expect(withOwner(null)).toEqual([]);
    expect(withOwner(undefined)).toEqual([]);
  });

  it("equals the chat-header participant count when members omits the owner", () => {
    // The reported bug: a 9-member REST project whose header (owner ∪ members)
    // read 10 while the panel read 9.
    const project = {
      created_by: "u-owner",
      created_at: "2026-01-01T00:00:00Z",
      members: Array.from({ length: 9 }, (_, i) => pmMember(`ag${i}`)),
    };
    expect(withOwner(project)).toHaveLength(10);
  });
});
