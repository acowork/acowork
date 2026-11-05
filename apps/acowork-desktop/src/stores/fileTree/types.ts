/**
 * fileTree — types
 *
 * Pure data contracts for the workspace tree cache. NO runtime side
 * effects, NO React imports. The single source of truth for cache node
 * shape, the cache-key encoding, and the freshness policy.
 *
 * Design note: every consumer (UI, Zustand store, tests) MUST go through
 * `treeKey()` / `splitTreeKey()` rather than concatenating strings
 * directly. The previous implementation duplicated the
 * `${agentId}:${workspaceId}:${relPath}` encoding in 6+ files and
 * silently relied on agent/workspace IDs not containing ':' — an invariant
 * that is not enforceable from outside this module. The `\0`-delimited
 * key with a brand type fixes both.
 */

/* ───────────────────────── Tree node (single source of truth) ─────── */

/**
 * The lifecycle of one cache entry. Discriminated by `kind` so callers
 * can exhaustively handle every state without sentinel `null` checks
 * (the previous design overloaded `null` for "not loaded", "failed",
 * and "empty directory" simultaneously, which made `treeLoadingPaths`
 * and `treeCache` impossible to reason about).
 */
export type TreeNode =
  | { kind: "idle" }
  | { kind: "loading" }
  | {
      kind: "ready";
      entries: TreeEntry[];
      /** Absolute filesystem path of the directory root (for path resolution) */
      root: string;
      /** ms-epoch when this snapshot was fetched */
      fetchedAt: number;
    }
  | {
      /**
       * Within the SWR window: served immediately, but a background
       * revalidation is already in flight. UI may optionally show a
       * subtle indicator.
       */
      kind: "stale";
      entries: TreeEntry[];
      root: string;
      fetchedAt: number;
    }
  | {
      kind: "error";
      error: TreeError;
    };

/** Strongly-typed error so callers can branch on cause (network, 5xx, abort). */
export type TreeError =
  | { cause: "abort" }
  | { cause: "http"; status: number; statusText: string }
  | { cause: "network"; message: string };

/* ───────────────────────── Cache key (branded, structured) ────────── */

/**
 * Branded string prevents accidental mixing with arbitrary strings.
 * The seal of `\u0000` (NUL) is used because NUL cannot appear in
 * agent IDs (reverse-domain) or workspace IDs (hex / `__agent_home__`)
 * or POSIX relative paths.
 */
export type TreeCacheKey = string & { readonly __brand: "TreeCacheKey" };

export function treeKey(agentId: string, workspaceId: string, relPath = ""): TreeCacheKey {
  return `${agentId}\u0000${workspaceId}\u0000${relPath}` as TreeCacheKey;
}

export function splitTreeKey(key: TreeCacheKey): {
  agentId: string;
  workspaceId: string;
  relPath: string;
} {
  const [agentId, workspaceId, ...rest] = key.split("\u0000");
  return { agentId, workspaceId, relPath: rest.join("\u0000") };
}

/** Stable per-workspace key used to look up the absolute root path. */
export function workspaceRootKey(agentId: string, workspaceId: string): TreeCacheKey {
  return treeKey(agentId, workspaceId, "");
}

/* ───────────────────────── Wire types (Gateway API) ───────────────── */

/** Matches `apps/acowork-gateway/src/http/routes.rs` workspaces/tree response */
export interface TreeEntry {
  name: string;
  type: "file" | "directory";
  size?: number;
  mtime?: number;
  /** RFC 3339 mtime as the Runtime actually sends it
   *  (`workspace_query.rs::TreeEntryDto::modified`). Directories carry it
   *  too; only files pair it with a `size`. */
  modified?: string;
  children?: TreeEntry[];
}

export interface TreeResponse {
  root: string;
  entries: TreeEntry[];
}

/* ───────────────────────── Freshness policy ──────────────────────── */

/**
 * Cache eviction & revalidation policy. Every number is exposed here
 * so it can be tuned in tests (fast-clock strategies) and at runtime
 * without touching the cache implementation.
 *
 *  freshMs:   the response is authoritative for this long; no refetch
 *  staleMs:   within this longer window, the response is served as
 *             `kind:"stale"` and a background revalidation is kicked
 *             off (stale-while-revalidate, RFC 5861).
 *  capacity:  hard cap on the number of cached keys (LRU evicted).
 */
export interface TreeCachePolicy {
  freshMs: number;
  staleMs: number;
  capacity: number;
}

export const DEFAULT_TREE_POLICY: TreeCachePolicy = {
  freshMs: 30_000,
  staleMs: 5 * 60_000,
  capacity: 500,
};

/* ───────────────────────── Type guards (no `any`) ─────────────────── */

export const isReadyNode = (n: TreeNode | undefined): n is Extract<TreeNode, { kind: "ready" | "stale" }> =>
  !!n && (n.kind === "ready" || n.kind === "stale");

export const isLoading = (n: TreeNode | undefined): boolean =>
  !!n && n.kind === "loading";
