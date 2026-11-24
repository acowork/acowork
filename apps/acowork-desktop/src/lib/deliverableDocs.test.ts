import { describe, it, expect } from "vitest";
import type { ChatMessage } from "./types";
import { parseDocToolResult, collectTurnDocs, deliverableDocFileName } from "./deliverableDocs";

/** Build a tool_result ChatMessage carrying an MCP text envelope. */
function docResult(
  toolName: string,
  inner: Record<string, unknown> | string,
  opts: { error?: boolean; raw?: string } = {},
): ChatMessage {
  const text = typeof inner === "string" ? inner : JSON.stringify(inner);
  const content = opts.raw ?? JSON.stringify({ content: [{ type: "text", text }], isError: false });
  return {
    id: `r-${Math.random()}`,
    type: "tool_result",
    content,
    timestamp: 0,
    toolName,
    toolStatus: opts.error ? "error" : "success",
  };
}

const applied = {
  status: "applied",
  doc_id: "doc-abc",
  name: "方案.md",
  path: "项目管理/p-x/技术方案/方案.md",
  version: 3,
};

describe("parseDocToolResult", () => {
  it("parses an applied doc_write", () => {
    const doc = parseDocToolResult(docResult("mcp_doc__doc_write", applied));
    expect(doc).toEqual({
      docId: "doc-abc",
      path: "项目管理/p-x/技术方案/方案.md",
      name: "方案.md",
      version: 3,
    });
  });

  it("ignores a pending_review proposal (nothing landed)", () => {
    const pending = { status: "pending_review", request_id: "r-1", doc_id: "doc-abc", path: "x.md" };
    expect(parseDocToolResult(docResult("mcp_doc__doc_write", pending))).toBeNull();
  });

  it("parses a doc_move (no status field)", () => {
    const moved = { doc_id: "doc-abc", name: "终稿.md", path: "项目管理/p-x/评审/终稿.md", version: 4 };
    const doc = parseDocToolResult(docResult("mcp_doc__doc_move", moved));
    expect(doc?.docId).toBe("doc-abc");
    expect(doc?.path).toBe("项目管理/p-x/评审/终稿.md");
    expect(doc?.version).toBe(4);
  });

  it("recognizes a custom-named doc server", () => {
    expect(parseDocToolResult(docResult("mcp_myserver__doc_write", applied))?.docId).toBe("doc-abc");
  });

  it("ignores unrelated tools", () => {
    expect(parseDocToolResult(docResult("mcp_doc__doc_read", applied))).toBeNull();
    expect(parseDocToolResult(docResult("file_write", applied))).toBeNull();
  });

  it("ignores a failed call", () => {
    expect(parseDocToolResult(docResult("mcp_doc__doc_write", applied, { error: true }))).toBeNull();
  });

  it("degrades to null on truncated / non-JSON content", () => {
    expect(parseDocToolResult(docResult("mcp_doc__doc_write", "", { raw: '{"content": [' }))).toBeNull();
    expect(parseDocToolResult(docResult("mcp_doc__doc_write", "", { raw: "not json" }))).toBeNull();
  });

  it("requires doc_id and path", () => {
    expect(parseDocToolResult(docResult("mcp_doc__doc_write", { status: "applied", doc_id: "d" }))).toBeNull();
    expect(parseDocToolResult(docResult("mcp_doc__doc_write", { status: "applied", path: "x.md" }))).toBeNull();
  });

  it("ignores non-tool_result rows", () => {
    const msg: ChatMessage = { id: "u", type: "user", content: "hi", timestamp: 0 };
    expect(parseDocToolResult(msg)).toBeNull();
  });
});

describe("parseDocToolResult — metadata.doc fact (live channel)", () => {
  /** A tool_result whose content is the live 5-line truncation (unparseable). */
  function truncatedLive(toolName: string, doc?: Record<string, unknown>): ChatMessage {
    return {
      id: "r-live",
      type: "tool_result",
      content: '{\n  "content": [\n    {\n      "type": "text",\n...(truncated)',
      timestamp: 0,
      toolName,
      toolStatus: "success",
      toolData: doc ? { doc } : undefined,
    };
  }

  it("derives from metadata.doc even when content is truncated", () => {
    const doc = parseDocToolResult(
      truncatedLive("mcp_doc__doc_write", {
        doc_id: "doc-live",
        path: "研发/x.md",
        name: "x.md",
        version: 2,
      }),
    );
    expect(doc).toEqual({ docId: "doc-live", path: "研发/x.md", name: "x.md", version: 2 });
  });

  it("prefers metadata.doc over the content envelope", () => {
    const msg = docResult("mcp_doc__doc_write", applied);
    msg.toolData = { doc: { doc_id: "doc-meta", path: "m.md", name: "m.md", version: 9 } };
    expect(parseDocToolResult(msg)?.docId).toBe("doc-meta");
  });

  it("falls back to the envelope when metadata.doc is malformed", () => {
    const msg = docResult("mcp_doc__doc_write", applied);
    msg.toolData = { doc: { path: "no-id.md" } };
    expect(parseDocToolResult(msg)?.docId).toBe("doc-abc");
  });

  it("still ignores a failed call carrying metadata", () => {
    const msg = truncatedLive("mcp_doc__doc_write", { doc_id: "d", path: "p.md", version: 1 });
    msg.toolStatus = "error";
    expect(parseDocToolResult(msg)).toBeNull();
  });

  it("defaults name to path and version to 0 from metadata", () => {
    const doc = parseDocToolResult(
      truncatedLive("mcp_doc__doc_move", { doc_id: "d", path: "a/b.md" }),
    );
    expect(doc).toEqual({ docId: "d", path: "a/b.md", name: "a/b.md", version: 0 });
  });
});

describe("collectTurnDocs", () => {
  it("dedupes by doc_id keeping the last occurrence (move supersedes write)", () => {
    const write = docResult("mcp_doc__doc_write", applied);
    const move = docResult("mcp_doc__doc_move", {
      doc_id: "doc-abc",
      name: "终稿.md",
      path: "项目管理/p-x/评审/终稿.md",
      version: 4,
    });
    const docs = collectTurnDocs([write, move]);
    expect(docs).toHaveLength(1);
    expect(docs[0].path).toBe("项目管理/p-x/评审/终稿.md");
    expect(docs[0].version).toBe(4);
  });

  it("keeps distinct documents", () => {
    const a = docResult("mcp_doc__doc_write", applied);
    const b = docResult("mcp_doc__doc_write", { ...applied, doc_id: "doc-def", path: "other.md" });
    expect(collectTurnDocs([a, b])).toHaveLength(2);
  });
});

describe("deliverableDocFileName", () => {
  it("returns the path's last segment", () => {
    expect(deliverableDocFileName({ docId: "d", path: "a/b/方案.md", name: "方案.md", version: 1 })).toBe("方案.md");
  });
});