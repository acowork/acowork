import { describe, expect, it } from "vitest";
import { resolveMentions } from "./GroupChatPanel";

describe("resolveMentions", () => {
  const agents = [
    { id: "a-eng", name: "Senior Engineer" },
    { id: "a-eng2", name: "Engineer" },
    { id: "a-bob", name: "Bob" },
  ];

  it("matches @name tokens against agent participants", () => {
    expect(resolveMentions("@Bob take a look", agents)).toEqual(["a-bob"]);
  });

  it("prefers the longest name so 'Engineer' does not shadow 'Senior Engineer'", () => {
    expect(resolveMentions("@Senior Engineer go", agents)).toEqual(["a-eng"]);
  });

  it("dedupes repeated mentions and reports each target once", () => {
    // Iteration is longest-name-first, so `Engineer` lands before `Bob`.
    expect(resolveMentions("@Bob @Bob @Engineer", agents)).toEqual(["a-eng2", "a-bob"]);
  });

  it("returns nothing when no agent is mentioned", () => {
    expect(resolveMentions("plain message", agents)).toEqual([]);
  });
});
