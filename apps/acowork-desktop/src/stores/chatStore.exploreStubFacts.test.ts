/**
 * The stub → `ChatMessage` conversion must carry the run's forwarded artifact
 * facts (`exploreStub.facts`), and must not invent them.
 *
 * This is the seam the history-page banner depends on: the Runtime forwards the
 * facts on `metadata.facts` (t-01412679 projection), and `deriveDeliverables`
 * reads them off `exploreStub.facts`. If this conversion drops them, the banner
 * silently disappears from every reloaded session — with no error anywhere,
 * which is exactly how the bug that motivated the field shipped.
 *
 * Covered here: the passthrough, the absence case (older Runtime), and the
 * malformed-element case (one bad element must not poison the whole run, nor
 * reach the renderer).
 */
import { describe, it, expect } from "vitest";
import { convertConversationEntry } from "./chatStore";
import type { ConversationEntry } from "../lib/types";

const AGENT = "com.test.Agent";

function stubEntry(metadata: Record<string, unknown>): ConversationEntry {
  return {
    id: "run-first-row",
    ts: "2026-01-01T12:00:00.000Z",
    role: "assistant",
    content: "",
    kind: "explore_stub",
    metadata,
  } as unknown as ConversationEntry;
}

const doc = { doc_id: "doc-abc", path: "研发/纪要.md", name: "纪要", version: 2 };
const produced = [
  { workspace_id: "ws", rel_path: "shot.png", mime: "image/png", bytes: 10, created: true },
];

describe("explore_stub conversion — artifact facts", () => {
  it("carries the forwarded facts verbatim", () => {
    const msg = convertConversationEntry(
      stubEntry({
        start: 1,
        end: 6,
        raw_count: 5,
        steps: 3,
        tools: ["bash"],
        facts: [{ tool_name: "x", tool_call_id: "c1", doc }, { tool_name: "y", produced }],
      }),
      AGENT,
    );
    expect(msg?.type).toBe("explore_stub");
    const facts = msg?.exploreStub?.facts;
    expect(facts).toHaveLength(2);
    expect(facts?.[0].doc).toEqual(doc);
    expect(facts?.[1].produced).toEqual(produced);
  });

  it("leaves facts undefined when the Runtime sent none (version skew)", () => {
    const msg = convertConversationEntry(
      stubEntry({ start: 1, end: 4, raw_count: 3, steps: 2, tools: ["bash"] }),
      AGENT,
    );
    expect(msg?.exploreStub?.facts).toBeUndefined();
  });

  it("drops malformed elements instead of failing the whole run", () => {
    const msg = convertConversationEntry(
      stubEntry({
        start: 1,
        end: 4,
        raw_count: 3,
        steps: 2,
        tools: [],
        facts: [null, "junk", { doc }],
      }),
      AGENT,
    );
    const facts = msg?.exploreStub?.facts;
    expect(facts).toHaveLength(1);
    expect(facts?.[0].doc).toEqual(doc);
  });
});