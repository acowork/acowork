/**
 * Authenticated image loading (ADR-076 follow-up).
 *
 * The Gateway gates every `/api/*` route — the avatar byte routes included —
 * on `Authorization: Bearer <access_token>`. A bare `<img src="…">` cannot
 * carry that header: the WebView issues the request itself, outside
 * `window.fetch`, so the interceptor in [authFetch.ts](../../lib/authFetch.ts)
 * never sees it. Every custom avatar therefore came back 401 and silently
 * degraded to its builtin icon.
 *
 * `AuthedImage` fetches the bytes through `window.fetch` (token attached and
 * rotated by the interceptor) and renders them from a `blob:` URL.
 *
 * ## Why there is a blob cache here
 *
 * The bytes only exist after an async round-trip, so a cold mount has
 * nothing to paint and must render `fallback`. That is unavoidable and
 * correct. Re-mounting with the SAME url, however, is not: the panel that
 * owns these avatars (right-panel AgentSetupTab) unmounts and remounts on
 * every tab switch, so without a cache every reopen re-fetched the bytes and
 * re-painted the fallback for a frame or two — the builtin icon visibly
 * flashing before the real avatar, which read as "the avatar I set is being
 * ignored". A URL-keyed cache makes every mount after the first resolve
 * during render, so the image is in the very first painted frame.
 *
 * The cache is keyed by URL and bounded. Fresh uploads pick a new
 * `avatar-NN` name (`nextAvatarName`), so a replaced file is a different
 * key rather than a stale hit; `clearAuthedImageCache()` exists for the
 * cases where the same path's bytes did change.
 *
 * ## Lifetime
 *
 * Cached object URLs outlive their consumers — that is the entire point,
 * since the previous mount is gone by the time the next one asks. Entries
 * are therefore reference-counted, and eviction (LRU, over
 * `MAX_CACHE_ENTRIES`) skips entries a mounted `<img>` is still displaying;
 * revoking one of those would blank the image out from under the user.
 *
 * ponytail: no `img.decode()` / no cross-tab `BroadcastChannel` — one fetch
 * (HTTP-cached underneath) + one object URL per distinct url, per session.
 */

import { useEffect, useState, type CSSProperties, type ReactNode } from "react";

import { log } from "../../lib/logger";

export interface AuthedImageProps {
  /** HTTP URL that needs the caller's bearer token. Falsy renders `fallback`. */
  src: string | null | undefined;
  alt?: string;
  className?: string;
  style?: CSSProperties;
  onClick?: () => void;
  /** Rendered while loading and when the fetch fails. */
  fallback?: ReactNode;
}

export function AuthedImage({
  src,
  alt,
  className,
  style,
  onClick,
  fallback,
}: AuthedImageProps) {
  const resolved = useAuthedImageSrc(src ?? null);
  if (!resolved) return <>{fallback ?? null}</>;
  return (
    <img
      src={resolved}
      alt={alt}
      draggable={false}
      className={className}
      style={style}
      onClick={onClick}
    />
  );
}

// ── Cache ─────────────────────────────────────────────────────────────────

/**
 * Upper bound on retained object URLs. Avatars are 64–512px images and the
 * number of distinct ones is bounded by the number of agents with an
 * uploaded avatar, so this is only a guard against unbounded growth in a
 * long session, not a real tuning knob.
 */
const MAX_CACHE_ENTRIES = 64;

interface CacheEntry {
  /**
   * The object URL once the fetch has settled, or `null` while it is still
   * in flight / after it failed. Readable during render — that is what lets
   * a remount paint on its first frame instead of one frame later.
   */
  objectUrl: string | null;
  /** Settles with the object URL, or `null` on failure. Deduplicates fetches. */
  promise: Promise<string | null>;
  /** Live consumers currently rendering this entry. */
  refs: number;
}

/** Insertion-ordered: re-inserting on hit makes this LRU. */
const blobUrlCache = new Map<string, CacheEntry>();

/** The cached object URL for `url`, readable during render. */
function peekCached(url: string | null): string | null {
  if (!url) return null;
  return blobUrlCache.get(url)?.objectUrl ?? null;
}

function fetchBlobUrl(url: string): Promise<string | null> {
  return fetch(url)
    .then((resp) => {
      // A bare `<img>` failing with 401 is precisely the bug this
      // component exists to prevent (see module docs). Staying silent
      // here made that regression indistinguishable from "no custom
      // avatar configured" — both render `fallback`. Log the status so
      // the two are told apart in the Desktop log.
      if (!resp.ok) {
        log.warn(
          `[AuthedImage] avatar fetch failed: status=${resp.status} url=${url}`,
        );
        return null;
      }
      return resp.blob();
    })
    .then((blob) => (blob ? URL.createObjectURL(blob) : null))
    .catch((err) => {
      // Offline or unauthorized — the caller's fallback is the answer.
      log.warn(`[AuthedImage] avatar fetch threw: url=${url}`, err);
      return null;
    });
}

/** Get-or-create the entry for `url`, count this consumer, return it. */
function acquire(url: string): CacheEntry {
  const existing = blobUrlCache.get(url);
  if (existing) {
    existing.refs += 1;
    // Refresh recency for the LRU sweep.
    blobUrlCache.delete(url);
    blobUrlCache.set(url, existing);
    return existing;
  }

  const entry: CacheEntry = {
    objectUrl: null,
    // Assigned immediately below; the promise needs `entry` to publish its
    // result for render-phase readers, hence the two-step construction.
    promise: Promise.resolve(null),
    refs: 1,
  };
  entry.promise = fetchBlobUrl(url).then((objectUrl) => {
    // Guarded against a concurrent eviction having already replaced or
    // removed this entry.
    if (blobUrlCache.get(url) === entry) entry.objectUrl = objectUrl;
    return objectUrl;
  });
  blobUrlCache.set(url, entry);
  evict();
  return entry;
}

function release(entry: CacheEntry): void {
  entry.refs = Math.max(0, entry.refs - 1);
}

/**
 * Release an evicted entry's object URL.
 *
 * A settled entry is revoked synchronously — the bytes are already known,
 * so there is no reason to leave a dangling URL alive for a microtask. A
 * still-pending one is revoked when it lands, because by then nothing
 * references it and it would otherwise leak for the rest of the session.
 */
function releaseEntry(entry: CacheEntry): void {
  if (entry.objectUrl) {
    URL.revokeObjectURL(entry.objectUrl);
    entry.objectUrl = null;
    return;
  }
  entry.promise
    .then((objectUrl) => {
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    })
    .catch(() => {
      // Already settled in the fetch's own catch; nothing to revoke.
    });
}

/**
 * Drop least-recently-used entries until the cache is back under the bound.
 * Entries with live consumers are left alone — revoking an object URL an
 * `<img>` is still pointing at blanks the image. If everything in the cache
 * is live the cache is allowed to sit above the bound until the next sweep,
 * which self-corrects as soon as anything unmounts.
 */
function evict(): void {
  if (blobUrlCache.size <= MAX_CACHE_ENTRIES) return;
  for (const [url, entry] of blobUrlCache) {
    if (blobUrlCache.size <= MAX_CACHE_ENTRIES) return;
    if (entry.refs > 0) continue;
    blobUrlCache.delete(url);
    releaseEntry(entry);
  }
}

/**
 * Drop every cached object URL, revoking each one. Exported for tests, and
 * for the rare case where a file's bytes changed under a path that stayed
 * the same.
 */
export function clearAuthedImageCache(): void {
  for (const entry of blobUrlCache.values()) releaseEntry(entry);
  blobUrlCache.clear();
}

// ── Hook ──────────────────────────────────────────────────────────────────

/**
 * Resolve `url` to a `blob:` URL that can be rendered. `null` while a cold
 * fetch is in flight and on every failure, so the caller can pick its own
 * fallback.
 *
 * A cache hit resolves during render (see `peekCached`), so the image is
 * part of the first painted frame instead of appearing one frame after the
 * fallback.
 */
export function useAuthedImageSrc(url: string | null): string | null {
  const [state, setState] = useState<{ url: string | null; objectUrl: string | null }>(
    () => ({ url, objectUrl: peekCached(url) }),
  );

  // Render-phase adjustment (React's documented "adjusting state when a
  // prop changes" pattern): when `url` changes, re-read the cache now so a
  // hit for the new url lands in this frame rather than the next one.
  if (state.url !== url) {
    setState({ url, objectUrl: peekCached(url) });
  }
  const objectUrl = state.url === url ? state.objectUrl : peekCached(url);

  useEffect(() => {
    if (!url) return;
    const entry = acquire(url);
    let cancelled = false;
    entry.promise.then((resolvedUrl) => {
      if (cancelled) return;
      if (resolvedUrl === null) {
        // Failures must not be cached — a later mount has to be able to
        // retry (a re-login restores the token a 401 was missing).
        if (blobUrlCache.get(url) === entry) blobUrlCache.delete(url);
      }
      setState({ url, objectUrl: resolvedUrl });
    });
    return () => {
      cancelled = true;
      release(entry);
    };
  }, [url]);

  return objectUrl;
}