/**
 * A turn's *online-document* deliverables — derived in the frontend from facts.
 *
 * ## Where the fact comes from
 *
 * A doc-service document is addressable by `doc_id` through the Gateway doc
 * proxy from any machine. The fact (`doc_id`, `path`, `name`, `version`) is
 * stamped by the Runtime onto the `tool_result`'s `metadata.doc` (see the
 * Runtime's `agent::doc_deliverables` adapter). It rides metadata — not content —
 * because the live MQTT path truncates `tool_result` content to five lines
 * (ADR-035 D9.2), which would otherwise make a pretty-printed envelope
 * unparseable live. Metadata is never truncated, so the live banner matches the
 * restored one. This module turns that fact into a display deliverable; the
 * Runtime supplies the fact, the frontend decides what to show.
 *
 * ## Legacy fallback
 *
 * Sessions persisted before the metadata stamp carry the fact only in the MCP
 * result envelope (`tool_result` content). For those, this module still parses
 * the envelope: the wrapper serializes a text-only result as
 * `{"content":[{"type":"text","text":"<inner JSON>"}],...}`, whose inner JSON is
 * the doc service's response:
 *   - `doc_write` applied: `{status:"applied", doc_id, name, path, version}`
 *     (a `pending_review` proposal is NOT a deliverable — nothing landed yet);
 *   - `doc_move`: `{doc_id, name, path, version}` (no status field).
 *
 * Recognition is by the tool's bare name (`mcp_<server>__doc_write` /
 * `__doc_move`), not a hardcoded server name, plus a shape check on the result,
 * so a custom-named doc server still works and an unrelated tool cannot fake a
 * card. This adapter is isolated from the tool-agnostic file path (ADR-119).
 */
import type { ChatMessage } from "./types";

/** One online document a turn produced, addressable by `doc_id`. */
export interface DeliverableDoc {
  docId: string;
  /** Library-relative path, e.g. `项目管理/p-x/评审/y.md`. */
  path: string;
  /** Document title (the path's stem). */
  name: string;
  /** Version at the time of the write/move. */
  version: number;
}

/** The file name a person reads: the path's last segment. */
export function deliverableDocFileName(doc: DeliverableDoc): string {
  const segments = doc.path.split("/");
  return segments[segments.length - 1] || doc.name || doc.docId;
}

/** Is this tool name a doc-service write/move, regardless of server name? */
function docToolKind(toolName: string | undefined): "write" | "move" | null {
  if (!toolName || !toolName.startsWith("mcp_")) return null;
  if (toolName.endsWith("__doc_write")) return "write";
  if (toolName.endsWith("__doc_move")) return "move";
  return null;
}

/** Unwrap the MCP CallToolResult envelope to the inner result object. */
function unwrapMcpResult(content: string): Record<string, unknown> | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(content);
  } catch {
    // Truncated live content (the MQTT path caps tool_result at 5 lines) or a
    // non-JSON result: nothing to derive yet. The full disk projection parses.
    return null;
  }
  if (!parsed || typeof parsed !== "object") return null;
  const obj = parsed as Record<string, unknown>;
  // Envelope shape: { content: [{ type:"text", text:"<inner>" }, ...] }.
  if (Array.isArray(obj.content)) {
    const texts: string[] = [];
    for (const block of obj.content) {
      if (block && typeof block === "object") {
        const b = block as Record<string, unknown>;
        if (b.type === "text" && typeof b.text === "string") texts.push(b.text);
      }
    }
    if (texts.length === 0) return null;
    try {
      const inner = JSON.parse(texts.join("\n"));
      return inner && typeof inner === "object" ? (inner as Record<string, unknown>) : null;
    } catch {
      return null;
    }
  }
  // Already the inner result object (a future wrapper may pass it through).
  return obj;
}

/** Map a doc fact object (snake_case on the wire) to a DeliverableDoc. */
function docFromFact(fact: Record<string, unknown>): DeliverableDoc | null {
  const docId = typeof fact.doc_id === "string" ? fact.doc_id : "";
  const path = typeof fact.path === "string" ? fact.path : "";
  if (!docId || !path) return null;
  const name = typeof fact.name === "string" && fact.name ? fact.name : path;
  const version =
    typeof fact.version === "number" && Number.isFinite(fact.version) ? fact.version : 0;
  return { docId, path, name, version };
}

/**
 * Parse one tool_result message into a produced document, or null.
 *
 * Reads the Runtime-stamped `metadata.doc` fact first (it survives the live
 * 5-line content truncation), then falls back to parsing the persisted MCP
 * envelope for older sessions. Tolerant by construction: a malformed,
 * truncated, pending-review, or unrelated result degrades to "no document"
 * rather than throwing inside the transcript renderer.
 */
export function parseDocToolResult(msg: ChatMessage): DeliverableDoc | null {
  if (msg.type !== "tool_result") return null;
  // A failed call produced nothing.
  if (msg.toolStatus === "error") return null;
  // Preferred: the fact the Runtime stamped on metadata (live == restored).
  const meta = msg.toolData?.doc;
  if (meta && typeof meta === "object") {
    const doc = docFromFact(meta as Record<string, unknown>);
    if (doc) return doc;
  }
  // Legacy fallback: the persisted MCP envelope in content.
  const kind = docToolKind(msg.toolName);
  if (!kind) return null;
  const inner = unwrapMcpResult(msg.content);
  if (!inner) return null;
  // doc_write carries a status; only an applied write landed a document.
  // doc_move has no status field and always names the moved document.
  if (kind === "write" && inner.status !== "applied") return null;
  return docFromFact(inner);
}

/**
 * Collect the documents a turn produced, deduped by `doc_id` keeping the LAST
 * occurrence (a `doc_move` after a `doc_write` supersedes the temporary name).
 */
export function collectTurnDocs(messages: readonly ChatMessage[]): DeliverableDoc[] {
  const byId = new Map<string, DeliverableDoc>();
  for (const msg of messages) {
    const doc = parseDocToolResult(msg);
    if (doc) byId.set(doc.docId, doc);
  }
  return [...byId.values()];
}