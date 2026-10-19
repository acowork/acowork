/**
 * Pure-function tests for the group-chat @-mention autocomplete helpers.
 *
 * The picker must trigger only on a word-initial `@`, close as soon as the
 * query hits whitespace, and splice `@Name ` back over exactly the active
 * token — these are the behaviours that make it feel like a normal IM, and
 * each is cheap to regress. Rendering the whole panel would need the store
 * graph; the decision logic is all in these three pure functions.
 */
import { describe, it, expect } from "vitest";
import {
  resolveMentions,
  detectMention,
  filterMentionCandidates,
  applyMention,
} from "./GroupChatPanel";

describe("detectMention (@ token under the caret)", () => {
  it("opens on a word-initial @ and reports the query + @ index", () => {
    const value = "hi @po";
    expect(detectMention(value, value.length)).toEqual({ query: "po", start: 3 });
  });

  it("opens immediately after the @ (empty query lists everyone)", () => {
    const value = "@";
    expect(detectMention(value, 1)).toEqual({ query: "", start: 0 });
  });

  it("does NOT trigger on an @ glued to a preceding word (email-like)", () => {
    const value = "a@b";
    expect(detectMention(value, value.length)).toBeNull();
  });

  it("closes once the query contains whitespace", () => {
    const value = "@pony tail";
    expect(detectMention(value, value.length)).toBeNull();
  });

  it("tracks the last @ when several are present", () => {
    const value = "@alice and @bo";
    expect(detectMention(value, value.length)).toEqual({ query: "bo", start: 11 });
  });

  it("reads the token up to the caret, ignoring text after it", () => {
    const value = "@al rest";
    // caret right after "@al" (index 3)
    expect(detectMention(value, 3)).toEqual({ query: "al", start: 0 });
  });
});

describe("filterMentionCandidates", () => {
  const agents = [
    { id: "a1", name: "Ponytail" },
    { id: "a2", name: "PM" },
    { id: "a3", name: "Senior Engineer" },
  ];

  it("returns everyone for an empty query", () => {
    expect(filterMentionCandidates(agents, "")).toEqual(agents);
  });

  it("matches case-insensitively as a substring", () => {
    expect(filterMentionCandidates(agents, "pony")).toEqual([agents[0]]);
    expect(filterMentionCandidates(agents, "ENGINEER")).toEqual([agents[2]]);
  });

  it("yields nothing when no name matches", () => {
    expect(filterMentionCandidates(agents, "zzz")).toEqual([]);
  });
});

describe("applyMention (splice @Name over the active token)", () => {
  it("replaces the token and appends one trailing space", () => {
    const value = "hi @po";
    const r = applyMention(value, value.length, 3, "Ponytail");
    expect(r.value).toBe("hi @Ponytail ");
    expect(r.caret).toBe("hi @Ponytail ".length);
  });

  it("replaces only up to the caret, preserving text typed after it", () => {
    const value = "@po is busy";
    // caret right after "@po" (index 4? -> "@po" ends at 3)
    const r = applyMention(value, 3, 0, "Ponytail");
    expect(r.value).toBe("@Ponytail  is busy");
  });
});

describe("resolveMentions (picked name must actually fan out)", () => {
  it("maps an inserted @Ponytail back to the agent id", () => {
    const agents = [{ id: "a1", name: "Ponytail" }];
    expect(resolveMentions("hi @Ponytail ", agents)).toEqual(["a1"]);
  });
});
