import { describe, it, expect } from "vitest";
import type { ChatMessage } from "./types";
import { withDerivedDeliverables, DeliverablesMemo } from "./deriveDeliverables";
import { DELIVERABLES_META_TYPE } from "./deliverableFiles";

let seq = 0;
function user(content = "do it"): ChatMessage {
  return { id: `u-${seq++}`, type: "user", content, timestamp: seq * 10 };
}
function assistant(content = "done"): ChatMessage {
  return { id: `a-${seq++}`, type: "assistant", content, timestamp: seq * 10 };
}
/** A tool_result carrying produced-file facts (snake_case, as the Runtime stamps). */
function fileResult(
  files: Array<{ workspace_id: string; rel_path: string; mime: string; bytes?: number; created?: boolean }>,
): ChatMessage {
  return {
    id: `t-${seq++}`,
    type: "tool_result",
    content: "Written",
    timestamp: seq * 10,
    toolName: "file_write",
    toolData: { produced: files },
  };
}
function docResult(docId: string, path: string, version = 1): ChatMessage {
  const inner = { status: "applied", doc_id: docId, name: path, path, version };
  return {
    id: `t-${seq++}`,
    type: "tool_result",
    content: JSON.stringify({ content: [{ type: "text", text: JSON.stringify(inner) }] }),
    timestamp: seq * 10,
    toolName: "mcp_doc__doc_write",
    toolStatus: "success",
  };
}
function legacyRow(files: unknown[]): ChatMessage {
  return {
    id: `legacy-${seq++}`,
    type: "system",
    content: "",
    timestamp: seq * 10,
    metadata: { type: DELIVERABLES_META_TYPE, files },
  };
}
const png = { workspace_id: "ws", rel_path: "shot.png", mime: "image/png", bytes: 10, created: true };
const md = { workspace_id: "ws", rel_path: "report.md", mime: "text/markdown", bytes: 20, created: true };

describe("withDerivedDeliverables", () => {
  it("synthesizes a banner from produced files at turn end", () => {
    const msgs = [user(), fileResult([png, md]), assistant()];
    const out = withDerivedDeliverables(msgs);
    const row = out[out.length - 1];
    expect(row.type).toBe("system");
    expect(row.metadata?.type).toBe(DELIVERABLES_META_TYPE);
    expect((row.metadata?.files as unknown[])).toHaveLength(2);
    expect(row.metadata?.derived).toBe(true);
  });

  it("excludes rewrites (created === false)", () => {
    const rewrite = { ...md, created: false };
    const out = withDerivedDeliverables([user(), fileResult([rewrite]), assistant()]);
    // No deliverable → no synthetic row, original reference preserved.
    expect(out).toHaveLength(3);
    expect(out.some((m) => m.metadata?.type === DELIVERABLES_META_TYPE)).toBe(false);
  });

  it("derives a doc-only banner (no workspace files)", () => {
    const out = withDerivedDeliverables([user(), docResult("doc-1", "p/方案.md"), assistant()]);
    const row = out[out.length - 1];
    expect((row.metadata?.files as unknown[])).toHaveLength(0);
    expect((row.metadata?.docs as unknown[])).toHaveLength(1);
  });

  it("preserves a legacy row and attaches derived docs", () => {
    const legacy = legacyRow([{ workspace_id: "ws", rel_path: "old.png", mime: "image/png", bytes: 1 }]);
    const out = withDerivedDeliverables([user(), legacy, docResult("doc-1", "p/x.md"), assistant()]);
    const rows = out.filter((m) => m.metadata?.type === DELIVERABLES_META_TYPE);
    expect(rows).toHaveLength(1); // not duplicated
    expect(rows[0].id).toBe(legacy.id); // legacy kept
    expect((rows[0].metadata?.files as unknown[])).toHaveLength(1);
    expect((rows[0].metadata?.docs as unknown[])).toHaveLength(1);
  });

  it("preserves array identity when nothing is produced", () => {
    const msgs = [user(), assistant()];
    expect(withDerivedDeliverables(msgs)).toBe(msgs);
  });

  it("caps images at 8", () => {
    const many = Array.from({ length: 12 }, (_, i) => ({ ...png, rel_path: `s${i}.png` }));
    const out = withDerivedDeliverables([user(), fileResult(many), assistant()]);
    const row = out[out.length - 1];
    expect((row.metadata?.files as unknown[])).toHaveLength(8);
  });

  it("gives each turn its own banner", () => {
    const out = withDerivedDeliverables([
      user("first"), fileResult([png]), assistant(),
      user("second"), docResult("doc-2", "p/y.md"), assistant(),
    ]);
    const rows = out.filter((m) => m.metadata?.type === DELIVERABLES_META_TYPE);
    expect(rows).toHaveLength(2);
    expect((rows[0].metadata?.files as unknown[])).toHaveLength(1);
    expect((rows[1].metadata?.docs as unknown[])).toHaveLength(1);
  });

  it("dedupes files by coordinates", () => {
    const out = withDerivedDeliverables([user(), fileResult([png, png]), assistant()]);
    const row = out[out.length - 1];
    expect((row.metadata?.files as unknown[])).toHaveLength(1);
  });
});
describe("DeliverablesMemo identity stability", () => {
  const png = { workspace_id: "w", rel_path: "a.png", mime: "image/png", bytes: 10, created: true };

  it("returns the same array reference when re-resolving identical rows", () => {
    const memo = new DeliverablesMemo();
    const rows = [user(), fileResult([png]), assistant()];
    const first = memo.resolve(rows);
    const second = memo.resolve(rows);
    expect(second).toBe(first);
  });

  it("keeps identity when the input array is rebuilt but the rows are the same references", () => {
    const memo = new DeliverablesMemo();
    const rows = [user(), fileResult([png]), assistant()];
    const first = memo.resolve(rows);
    // The adapter rebuilds the merged array on every stream delta; the element
    // references are what must stay stable, not the container.
    const second = memo.resolve([...rows]);
    expect(second).toBe(first);
  });

  it("keeps a historical banner's object identity when a later turn changes", () => {
    const memo = new DeliverablesMemo();
    const turn1 = [user("one"), fileResult([png]), assistant()];
    const first = memo.resolve(turn1);
    const banner1 = first[first.length - 1];
    // A second turn streams in; turn1's rows are untouched references.
    const turn2 = [user("two"), fileResult([{ ...png, rel_path: "b.png" }]), assistant()];
    const second = memo.resolve([...turn1, ...turn2]);
    const banner1Again = second.find((m) => m.id === banner1.id);
    expect(banner1Again).toBe(banner1);
    expect(second).not.toBe(first);
  });

  it("produces a new banner when the turn's produced facts change", () => {
    const memo = new DeliverablesMemo();
    const u = user();
    const a = assistant();
    const first = memo.resolve([u, fileResult([png]), a]);
    const changed = memo.resolve([u, fileResult([{ ...png, rel_path: "different.png" }]), a]);
    expect(changed).not.toBe(first);
    expect(changed[changed.length - 1]).not.toBe(first[first.length - 1]);
  });

  it("returns the original array untouched when no turn has a deliverable", () => {
    const memo = new DeliverablesMemo();
    const rows = [user(), assistant()];
    expect(memo.resolve(rows)).toBe(rows);
  });
});