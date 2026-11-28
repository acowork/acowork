/**
 * Transport parity: the SAME turn must reconstruct the SAME banner from a live
 * transcript and from a reloaded history page.
 *
 * Live rows arrive one per tool call (MQTT `record_complete`, full metadata).
 * A history page arrives projected: `GET /messages?groups=stub` folds every
 * contiguous thought / tool_call / tool_result run into ONE `explore_stub` row
 * and forwards the run's artifact facts on `exploreStub.facts`
 * (t-01412679 + the runtime's `artifact_facts` registry).
 *
 * The banner is frontend business logic (ADR-119) and it must not care which of
 * the two shapes it is reading — that is what this file pins. Before the facts
 * were forwarded, the stubbed page produced NO banner at all: every historical
 * turn silently lost its deliverables row while the live turn still had it.
 */
import { describe, it, expect } from "vitest";
import type { ChatMessage } from "./types";
import { withDerivedDeliverables } from "./deriveDeliverables";
import { DELIVERABLES_META_TYPE } from "./deliverableFiles";

let seq = 0;
const ts = () => seq * 10;

function user(content = "do it"): ChatMessage {
  return { id: `u-${seq++}`, type: "user", content, timestamp: ts() };
}
function assistant(content = "done"): ChatMessage {
  return { id: `a-${seq++}`, type: "assistant", content, timestamp: ts() };
}

/** A live tool_result row (metadata on the row itself). */
function liveResult(toolData: Record<string, unknown>): ChatMessage {
  return {
    id: `t-${seq++}`,
    type: "tool_result",
    content: "ok",
    timestamp: ts(),
    toolName: String(toolData.tool_name ?? "tool"),
    toolData,
  };
}

/** A stub row as the client materializes it from `groups=stub`. */
function stub(facts: Record<string, unknown>[]): ChatMessage {
  return {
    id: `s-${seq++}`,
    type: "explore_stub",
    content: "",
    timestamp: ts(),
    exploreStub: {
      start: 1,
      end: 6,
      rawCount: 5,
      steps: 3,
      tools: ["file_write", "mcp_doc__doc_write"],
      toolsTruncated: false,
      facts,
    },
  };
}

const docFact = {
  tool_name: "mcp_doc__doc_write",
  tool_call_id: "c1",
  doc: { doc_id: "doc-abc", path: "研发/纪要.md", name: "纪要", version: 2 },
};
const fileFact = {
  tool_name: "file_write",
  tool_call_id: "c2",
  produced: [
    { workspace_id: "ws", rel_path: "shot.png", mime: "image/png", bytes: 10, created: true },
    { workspace_id: "ws", rel_path: "report.md", mime: "text/markdown", bytes: 20, created: true },
  ],
};

/** The same facts, once as live rows and once as a projected stub. */
function liveTurn(): ChatMessage[] {
  return [user(), liveResult(docFact), liveResult(fileFact), assistant()];
}
function stubTurn(): ChatMessage[] {
  return [user(), stub([docFact, fileFact]), assistant()];
}

function bannerOf(messages: ChatMessage[]): ChatMessage | undefined {
  const out = withDerivedDeliverables(messages);
  return out.find((m) => m.metadata?.type === DELIVERABLES_META_TYPE);
}

describe("deliverables derivation across the two transcript shapes", () => {
  it("synthesizes the banner from a projected (history) page", () => {
    const banner = bannerOf(stubTurn());
    expect(banner, "a reloaded turn must keep its deliverables row").toBeTruthy();
    const files = banner?.metadata?.files as unknown[];
    const docs = banner?.metadata?.docs as unknown[];
    expect(files).toHaveLength(2);
    expect(docs).toHaveLength(1);
    expect((docs[0] as { docId: string }).docId).toBe("doc-abc");
  });

  it("produces an identical banner from the live and the projected shape", () => {
    const live = bannerOf(liveTurn());
    const projected = bannerOf(stubTurn());
    expect(live).toBeTruthy();
    expect(projected).toBeTruthy();
    // Identity, order and coordinates — the whole content, minus the row's own
    // id/timestamp (which address the row, not the deliverables).
    expect(projected?.metadata?.files).toEqual(live?.metadata?.files);
    expect(projected?.metadata?.docs).toEqual(live?.metadata?.docs);
  });

  it("applies one display policy across both shapes (created-only, dedupe, caps)", () => {
    const rewrite = {
      tool_name: "file_write",
      tool_call_id: "c9",
      produced: [{ workspace_id: "ws", rel_path: "report.md", mime: "text/markdown", bytes: 20, created: false }],
    };
    const dup = {
      tool_name: "file_write",
      tool_call_id: "c10",
      produced: [{ workspace_id: "ws", rel_path: "report.md", mime: "text/markdown", bytes: 20, created: true }],
    };
    const liveBanner = bannerOf([user(), liveResult(docFact), liveResult(dup), liveResult(rewrite), assistant()]);
    const stubBanner = bannerOf([user(), stub([docFact, dup, rewrite]), assistant()]);
    expect(stubBanner?.metadata?.files).toEqual(liveBanner?.metadata?.files);
    expect((stubBanner?.metadata?.files as unknown[])).toHaveLength(1);
  });

  it("merges a raw trailing run with a collapsed run of the same turn", () => {
    // The newest page deliberately leaves its trailing run raw (a pending
    // authorization must stay answerable), so one turn routinely mixes both
    // shapes. Dedupe is by coordinates/doc id, so the banner is still one row
    // per artifact — not two.
    const raw = liveResult(docFact);
    const dupDoc = { ...docFact, tool_call_id: "c9" };
    const out = withDerivedDeliverables([user(), stub([fileFact]), raw, liveResult(dupDoc), assistant()]);
    const banner = out.find((m) => m.metadata?.type === DELIVERABLES_META_TYPE);
    expect((banner?.metadata?.docs as unknown[])).toHaveLength(1);
    expect((banner?.metadata?.files as unknown[])).toHaveLength(2);
  });

  it("keeps the banner when the page splits one run into two stub parts", () => {
    // A page boundary may cut a run in two: two stubs, each with its own
    // facts. Dedupe is by coordinates, so the banner is the same as unsplit.
    const out = withDerivedDeliverables([user(), stub([docFact]), stub([fileFact]), assistant()]);
    const banner = out.find((m) => m.metadata?.type === DELIVERABLES_META_TYPE);
    expect((banner?.metadata?.files as unknown[])).toHaveLength(2);
    expect((banner?.metadata?.docs as unknown[])).toHaveLength(1);
  });

  it("degrades to no banner when a stub carries no facts (older Runtime)", () => {
    // Version skew: a Runtime that predates the forwarding sends a stub with
    // no `facts`. That is "nothing known", not "nothing produced" — the banner
    // is absent, exactly as before, and nothing throws.
    const out = withDerivedDeliverables([user(), stub([]), assistant()]);
    expect(out.some((m) => m.metadata?.type === DELIVERABLES_META_TYPE)).toBe(false);
  });
});