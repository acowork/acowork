/**
 * The row-shape adapter: which transcript rows carry artifact facts, and what
 * they read as. Policy (created-only, dedupe, caps, ordering) is NOT tested
 * here — that is `deriveDeliverables.test.ts`. One file per question, so a
 * change to the transcript projection cannot silently rewrite display policy
 * and vice versa.
 */
import { describe, it, expect } from "vitest";
import type { ChatMessage } from "./types";
import { rowArtifactFacts, turnArtifactFacts } from "./artifactFacts";

const doc = { doc_id: "doc-abc", path: "研发/纪要.md", name: "纪要", version: 2 };
const produced = [
  { workspace_id: "ws", rel_path: "report.md", mime: "text/markdown", bytes: 20, created: true },
];

/** A raw `tool_result` (live row / raw page). */
function raw(toolData?: Record<string, unknown>, extra: Partial<ChatMessage> = {}): ChatMessage {
  return { id: `r-${Math.random()}`, type: "tool_result", content: "ok", timestamp: 1, toolData, ...extra };
}

/** An `explore_stub` (projected history page). */
function stub(facts?: Record<string, unknown>[]): ChatMessage {
  return {
    id: `s-${Math.random()}`,
    type: "explore_stub",
    content: "",
    timestamp: 1,
    exploreStub: { start: 1, end: 4, rawCount: 3, steps: 2, tools: [], toolsTruncated: false, facts },
  };
}

describe("rowArtifactFacts", () => {
  it("reads a raw tool_result's own metadata", () => {
    const facts = rowArtifactFacts(raw({ produced, doc }));
    expect(facts?.files).toHaveLength(1);
    expect(facts?.docs?.[0].docId).toBe("doc-abc");
  });

  it("reads a stub's forwarded facts", () => {
    const facts = rowArtifactFacts(stub([{ doc }, { produced }]));
    expect(facts?.files).toHaveLength(1);
    expect(facts?.docs?.[0].docId).toBe("doc-abc");
  });

  it("returns null for a stub with no facts (older Runtime = not known)", () => {
    expect(rowArtifactFacts(stub(undefined))).toBeNull();
    expect(rowArtifactFacts(stub([]))).toBeNull();
  });

  it("returns null for rows that cannot carry facts", () => {
    expect(rowArtifactFacts({ id: "u", type: "user", content: "hi", timestamp: 1 })).toBeNull();
    expect(rowArtifactFacts({ id: "a", type: "assistant", content: "hi", timestamp: 1 })).toBeNull();
  });

  it("tolerates junk facts instead of throwing", () => {
    // The reader is the last line of defence: a malformed element degrades to
    // "produced nothing", never to a renderer exception.
    const facts = rowArtifactFacts(stub([{ nope: true }, { doc: { path: 5 } }, { doc }]));
    expect(facts?.docs).toHaveLength(1);
    expect(facts?.files).toHaveLength(0);
  });

  it("keeps the legacy content-envelope fallback for raw rows only", () => {
    // Pre-stamp sessions carry the doc fact in the result envelope. That is a
    // property of a persisted row, which a projection cannot produce — the
    // adapter must not pretend a forwarded fact has a content envelope.
    const inner = JSON.stringify({ status: "applied", doc_id: "d1", name: "x", path: "x.md", version: 1 });
    const legacy = raw(undefined, {
      content: JSON.stringify({ content: [{ type: "text", text: inner }] }),
      toolName: "mcp_any__doc_write",
      toolStatus: "success",
    });
    expect(rowArtifactFacts(legacy)?.docs?.[0].docId).toBe("d1");
    expect(rowArtifactFacts(stub([{ content: "irrelevant" }]))).toBeNull();
  });
});

describe("turnArtifactFacts", () => {
  it("unions the turn's rows in log order without any policy", () => {
    // Order and duplicates are preserved on purpose: the policy layer needs the
    // real sequence to decide "last write wins" (docs) and "first wins" (files).
    const turn = [
      raw({ doc }),
      { id: "b", type: "assistant", content: "…", timestamp: 1 } as ChatMessage,
      stub([{ doc: { ...doc, doc_id: "doc-2" } }, { produced }]),
      raw({ produced }),
    ];
    const facts = turnArtifactFacts(turn);
    expect(facts.docs.map((d) => d.docId)).toEqual(["doc-abc", "doc-2"]);
    expect(facts.files).toHaveLength(2);
  });

  it("is empty for a turn that produced nothing", () => {
    expect(turnArtifactFacts([stub(undefined), raw({})])).toEqual({ files: [], docs: [] });
  });
});