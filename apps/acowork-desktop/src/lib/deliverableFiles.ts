/**
 * A turn's deliverables — the files a turn **produced**, as one transcript row.
 *
 * The Runtime writes that row when the turn ends (`turn_deliverables` in
 * `acowork-runtime`), and this module is the only place that knows its wire
 * shape. Two consumers read it: the transcript loader (which must *keep* the
 * row's metadata) and the renderer (which draws thumbnails from it).
 *
 * ## What the row is, and is not
 *
 * It is a `system` entry whose `metadata.type` is `deliverables`, carrying
 * `files: [{ workspace_id, rel_path, mime, bytes }]`. It is **not** a part of
 * the user turn above it — a deliverable belongs to the assistant's answer, not
 * to the human's request — so it must never join `isUserTurnPart`, or every
 * thumbnail would fold under the user's words.
 *
 * ## Why only paths
 *
 * Bytes never travel over the message channel: the Runtime persists them on the
 * node and stores `(workspace_id, rel_path)`, and the desktop re-fetches through
 * the ordinary workspace-file route ([workspaceFileBlob.ts](workspaceFileBlob.ts)).
 * That is the same route the right-hand workspace panel uses, which is what
 * makes "click a thumbnail" and "click the file in the tree" the same action —
 * one viewer, not two.
 */

/** `metadata.type` of a deliverables row (mirrors `META_TYPE` in the Runtime). */
export const DELIVERABLES_META_TYPE = "deliverables";

/** One file a turn produced, as the Runtime describes it. */
export interface DeliverableFile {
  /** Workspace the file belongs to (`__agent_home__` for the agent home). */
  workspaceId: string;
  /** Path relative to that workspace root, forward slashes. */
  relPath: string;
  /** MIME type, always known — an unidentifiable file is never listed. */
  mime: string;
  /** Size in bytes at turn end. */
  bytes: number;
  /** Whether the write created the file (did not exist before). Present on the
   *  per-tool_result `metadata.produced` facts; absent on a legacy turn row
   *  (which already applied the created-only policy). The frontend decides
   *  whether a rewrite counts as a deliverable. */
  created?: boolean;
}

/** The file name a person reads: the path's last segment. */
export function deliverableFileName(file: DeliverableFile): string {
  const segments = file.relPath.split("/");
  return segments[segments.length - 1] || file.relPath;
}

/** Does this MIME type have a thumbnail, or only a file capsule? */
export function isImageDeliverable(file: DeliverableFile): boolean {
  return file.mime.startsWith("image/");
}

/** A person-readable size: `1.2 MB`, `340 KB`, `980 B`. */
export function formatDeliverableSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  // One decimal only where it says something: `1.2 MB` reads better than
  // `1.23 MB`, and `340 KB` better than `340.0 KB`.
  const rendered = unit === 0 || value >= 100 ? Math.round(value) : Math.round(value * 10) / 10;
  return `${rendered} ${units[unit]}`;
}

/**
 * Parse a deliverables row's `metadata` into files.
 *
 * Tolerant by construction: this reads persisted history written by whatever
 * Runtime version a session was recorded with, so a malformed or future entry
 * degrades to "no deliverables" rather than throwing inside the transcript
 * renderer. Entries missing a workspace or path are dropped — they cannot be
 * fetched, and a chip that cannot open is worse than no chip.
 */
export function parseDeliverableFiles(
  metadata: Record<string, unknown> | undefined | null,
): DeliverableFile[] {
  if (!metadata || metadata.type !== DELIVERABLES_META_TYPE) return [];
  const raw = metadata.files;
  if (!Array.isArray(raw)) return [];

  const out: DeliverableFile[] = [];
  const seen = new Set<string>();
  for (const entry of raw) {
    if (!entry || typeof entry !== "object") continue;
    const record = entry as Record<string, unknown>;
    const workspaceId = typeof record.workspace_id === "string" ? record.workspace_id : "";
    const relPath = typeof record.rel_path === "string" ? record.rel_path : "";
    const mime = typeof record.mime === "string" ? record.mime : "";
    if (!workspaceId || !relPath || !mime) continue;
    // Same identity the editor tabs and the blob LRU use, so a duplicate in
    // one row cannot open two tabs.
    const key = `${workspaceId}:${relPath}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const bytes = typeof record.bytes === "number" && Number.isFinite(record.bytes) ? record.bytes : 0;
    out.push({ workspaceId, relPath, mime, bytes });
  }
  return out;
}

/** True when a chat message is a deliverables row. */
export function isDeliverablesRow(message: {
  type?: string;
  metadata?: Record<string, unknown>;
}): boolean {
  return message.type === "system" && message.metadata?.type === DELIVERABLES_META_TYPE;
}

/**
 * Parse the `produced` facts a tool stamped on its own `tool_result` metadata.
 *
 * The Runtime maps each produced file onto addressable `(workspace_id, rel_path)`
 * coordinates and keeps the writer's `created` flag (see `to_coordinates` in
 * `acowork-runtime`); it applies no display policy. This reads those facts into
 * the same {@link DeliverableFile} shape the legacy turn row uses, so one renderer
 * draws both. Tolerant by construction: a malformed entry is dropped, never
 * thrown. Deduped by coordinates keeping the first occurrence.
 */
export function parseProducedFiles(
  toolData: Record<string, unknown> | undefined | null,
): DeliverableFile[] {
  const raw = toolData?.produced;
  if (!Array.isArray(raw)) return [];
  const out: DeliverableFile[] = [];
  const seen = new Set<string>();
  for (const entry of raw) {
    if (!entry || typeof entry !== "object") continue;
    const record = entry as Record<string, unknown>;
    const workspaceId = typeof record.workspace_id === "string" ? record.workspace_id : "";
    const relPath = typeof record.rel_path === "string" ? record.rel_path : "";
    const mime = typeof record.mime === "string" ? record.mime : "";
    if (!workspaceId || !relPath || !mime) continue;
    const key = `${workspaceId}:${relPath}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const bytes =
      typeof record.bytes === "number" && Number.isFinite(record.bytes) ? record.bytes : 0;
    const created = typeof record.created === "boolean" ? record.created : undefined;
    out.push({ workspaceId, relPath, mime, bytes, created });
  }
  return out;
}