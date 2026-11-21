/**
 * The deliverables row's wire shape and the rules a reader applies to it.
 *
 * These are the only place the `metadata` layout of the Runtime's
 * `turn_deliverables` row is interpreted, so a change there is a change here —
 * and a row recorded by an older Runtime must degrade to "no deliverables",
 * never to a crash inside the transcript renderer.
 */
import { describe, expect, it } from "vitest";
import {
  DELIVERABLES_META_TYPE,
  deliverableFileName,
  formatDeliverableSize,
  isDeliverablesRow,
  isImageDeliverable,
  parseDeliverableFiles,
} from "./deliverableFiles";
import { isTurnPartMeta, isStandalonePart, isUserTurnPart } from "../components/chat/messageParts";

const shot = {
  workspace_id: "__agent_home__",
  rel_path: "artifacts/session-1/call_a-0.png",
  mime: "image/png",
  bytes: 2048,
};

describe("parseDeliverableFiles", () => {
  it("reads the Runtime's shape", () => {
    expect(
      parseDeliverableFiles({ type: DELIVERABLES_META_TYPE, files: [shot] }),
    ).toEqual([
      {
        workspaceId: "__agent_home__",
        relPath: "artifacts/session-1/call_a-0.png",
        mime: "image/png",
        bytes: 2048,
      },
    ]);
  });

  it("ignores metadata that is not a deliverables row", () => {
    expect(parseDeliverableFiles(undefined)).toEqual([]);
    expect(parseDeliverableFiles({ type: "context_ref", files: [shot] })).toEqual([]);
  });

  it("degrades malformed entries instead of throwing", () => {
    expect(parseDeliverableFiles({ type: DELIVERABLES_META_TYPE })).toEqual([]);
    expect(parseDeliverableFiles({ type: DELIVERABLES_META_TYPE, files: "nope" })).toEqual([]);
    expect(
      parseDeliverableFiles({
        type: DELIVERABLES_META_TYPE,
        files: [null, 42, { workspace_id: "ws-1" }, { rel_path: "a.png" }],
      }),
    ).toEqual([]);
  });

  it("drops an entry with no MIME — it could not be opened", () => {
    expect(
      parseDeliverableFiles({
        type: DELIVERABLES_META_TYPE,
        files: [{ workspace_id: "ws-1", rel_path: "a", bytes: 1 }],
      }),
    ).toEqual([]);
  });

  it("keeps the first of a duplicated path", () => {
    const files = parseDeliverableFiles({
      type: DELIVERABLES_META_TYPE,
      files: [shot, { ...shot, bytes: 9 }],
    });
    expect(files).toHaveLength(1);
    expect(files[0].bytes).toBe(2048);
  });

  it("survives a missing size", () => {
    const files = parseDeliverableFiles({
      type: DELIVERABLES_META_TYPE,
      files: [{ workspace_id: "ws-1", rel_path: "a.md", mime: "text/markdown" }],
    });
    expect(files[0].bytes).toBe(0);
  });
});

describe("the row's place in the transcript", () => {
  it("keeps its metadata through the loader", () => {
    expect(isTurnPartMeta("system", { type: DELIVERABLES_META_TYPE })).toBe(true);
  });

  it("is a standalone row, never a part of the user turn", () => {
    const msg = {
      id: "r",
      type: "system" as const,
      content: "",
      timestamp: 0,
      metadata: { type: DELIVERABLES_META_TYPE },
    };
    expect(isStandalonePart(msg)).toBe(true);
    expect(isUserTurnPart(msg)).toBe(false);
  });

  it("is recognised by the renderer's predicate", () => {
    expect(isDeliverablesRow({ type: "system", metadata: { type: "deliverables" } })).toBe(true);
    expect(isDeliverablesRow({ type: "assistant", metadata: { type: "deliverables" } })).toBe(false);
    expect(isDeliverablesRow({ type: "system" })).toBe(false);
  });
});

describe("display helpers", () => {
  it("names a file by its last segment", () => {
    expect(deliverableFileName({ workspaceId: "w", relPath: "a/b/c.png", mime: "image/png", bytes: 1 })).toBe("c.png");
    expect(deliverableFileName({ workspaceId: "w", relPath: "c.png", mime: "image/png", bytes: 1 })).toBe("c.png");
  });

  it("distinguishes thumbnails from capsules", () => {
    expect(isImageDeliverable({ workspaceId: "w", relPath: "a.png", mime: "image/png", bytes: 1 })).toBe(true);
    expect(isImageDeliverable({ workspaceId: "w", relPath: "a.pdf", mime: "application/pdf", bytes: 1 })).toBe(false);
  });

  it("formats sizes the way a person reads them", () => {
    expect(formatDeliverableSize(0)).toBe("0 B");
    expect(formatDeliverableSize(980)).toBe("980 B");
    expect(formatDeliverableSize(340 * 1024)).toBe("340 KB");
    expect(formatDeliverableSize(Math.round(1.25 * 1024 * 1024))).toBe("1.3 MB");
  });
});