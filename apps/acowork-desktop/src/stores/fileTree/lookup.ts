/**
 * fileTree — read-side helpers
 *
 * Imperative lookups for the cached workspace root path. Extracted
 * because (a) three call sites used to read `treeRoots[agentId:workspaceId]`
 * directly from the workspace store, and (b) React-rendered code can
 * subscribe via the cache, but non-React code (event handlers, tool
 * callbacks) just needs a synchronous value.
 */

import { useFileTreeStore } from "./fileTreeStore";
import { treeKey, isReadyNode } from "./types";

/**
 * Resolve the absolute filesystem root for an agent+workspace, or an
 * empty string if the workspace tree has not been fetched yet.
 *
 * Use the imperative form (e.g. inside `addAttachedContext`) when
 * you cannot subscribe to a Zustand selector. For rendered components,
 * prefer `useFileTreeStore(s => s.nodes[workspaceRootKey(a,w)]?.root)`
 * so the component re-renders when the root becomes available.
 */
export function getCachedWorkspaceRoot(agentId: string, workspaceId: string): string {
  const node = useFileTreeStore.getState().getNode(treeKey(agentId, workspaceId, ""));
  return isReadyNode(node) ? node.root : "";
}

/**
 * The cached size / modification time of one file, or `null` when the
 * containing directory is not in the cache (yet).
 *
 * Cache-only on purpose — no fetch, no await. Consumers that merely want
 * a version token for a cache key (the Office→PDF converter, see
 * `lib/docConvert.ts`) must never block a preview on a tree round trip;
 * a missing answer means "re-convert next time", not "show nothing".
 *
 * `mtimeMs` is derived from the entry's RFC 3339 `modified` (second
 * resolution — the Runtime formats `SystemTime` to whole seconds), so a
 * sub-second rewrite is invisible here. Every other invalidation path
 * (the fs-watch `previewSeq` bump) covers that.
 */
export function getCachedFileVersion(
  agentId: string,
  workspaceId: string,
  relPath: string,
): { size?: number; mtimeMs?: number } | null {
  const normalized = relPath.replace(/\\/g, "/");
  const cut = normalized.lastIndexOf("/");
  const parent = cut >= 0 ? normalized.slice(0, cut) : "";
  const name = cut >= 0 ? normalized.slice(cut + 1) : normalized;
  if (!name) return null;
  const node = useFileTreeStore.getState().getNode(treeKey(agentId, workspaceId, parent));
  if (!isReadyNode(node)) return null;
  const entry = node.entries.find((e) => e.name === name && e.type === "file");
  if (!entry) return null;
  const mtimeMs = entry.modified ? Date.parse(entry.modified) : NaN;
  return {
    size: entry.size,
    mtimeMs: Number.isFinite(mtimeMs) ? mtimeMs : undefined,
  };
}
