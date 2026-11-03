/**
 * Workspace-file bytes — the single fetch target for every editor preview
 * that needs raw file content (PDF / xlsx / docx / pptx viewers, and the
 * image preview).
 *
 * Why this module exists as one place:
 *
 *   - Local and cluster deployments address a workspace file through the
 *     SAME Gateway route (`GET /workspace-files/{agentId}/{workspaceId}/{*path}`,
 *     ADR-055 L2-7 — a thin reverse proxy to the Runtime raw endpoint, which
 *     itself re-proxies through the Node when the workspace is remote). The
 *     callers must not care which of those topologies is live.
 *   - That route requires the bearer token, and `<img>` / `<iframe>` `src`
 *     attributes cannot carry an `Authorization` header — the exact shape of
 *     the 401-blank-avatar bug. So the bytes must come through `fetch`
 *     (the app-wide interceptor in authFetch.ts attaches the bearer and
 *     replays after a refresh) and be handed to renderers as a
 *     `URL.createObjectURL` result. Plain `fetch`, **no token plumbing**
 *     here, same contract as agentFile.ts.
 *
 * The URL builder is deliberately **not exported as a rendering target**:
 * a URL from this module is a fetch target, not an `<img src>`. Handing the
 * bare string out is how the auth-less-tag bug gets written a second time.
 * Renderers get a blob (or a same-origin `blob:` URL from the cache) instead.
 * The one sanctioned escape hatch is `workspaceFileUrlForRust`, for commands
 * that fetch the bytes themselves.
 *
 * Caching: a module-level LRU keyed by `agentId:workspaceId:relPath` — the
 * same identity the editor tabs use. Opening a preview is idempotent from
 * the user's point of view, so re-fetching the full bytes on every tab
 * switch is pure waste (and a bandwidth problem on remote Nodes). Eviction
 * revokes the `blob:` URL so the bytes are actually released. File-change
 * invalidation goes through `invalidateWorkspaceFileBlob`, called by the
 * editor store when an fs-watch reload lands.
 */
import { invoke } from "@tauri-apps/api/core";
import { getGatewayUrl } from "./config";
import { saveUrlToDisk } from "./attachmentTransfer";

/** Result of a bytes read: the blob plus the server-declared MIME type. */
export interface WorkspaceFileBytes {
    blob: Blob;
    mime: string;
}

/** Result of a renderable-URL read (iframe / `<img>` / `<a download>`). */
export interface WorkspaceFileUrl {
    blobUrl: string;
    mime: string;
}

interface CacheEntry {
    blob: Blob;
    blobUrl: string;
    mime: string;
    size: number;
}

/**
 * Percent-encode each slash-delimited segment. Mirrors the Gateway's
 * `encode_path_segments` tolerance: the route takes `{*path}` verbatim, so
 * spaces / non-ASCII must be encoded per-segment while `/` separators stay.
 */
function encodePathSegments(path: string): string {
    return path
        .replace(/\\/g, "/")
        .split("/")
        .filter((segment) => segment.length > 0)
        .map(encodeURIComponent)
        .join("/");
}

/** The one URL builder. Kept private — see the module header. */
function workspaceFileUrl(agentId: string, workspaceId: string, relPath: string): string {
    const base = getGatewayUrl().replace(/\/+$/g, "");
    return `${base}/workspace-files/${encodeURIComponent(agentId)}/${encodeURIComponent(workspaceId)}/${encodePathSegments(relPath)}`;
}

/**
 * The Gateway address of a workspace file, for **Rust commands only**
 * (`download_attachment` / `open_file_externally` / the Office→PDF
 * converter). Each of those re-fetches through `GatewayClient`, which
 * attaches the bearer token, so the webview never needs the string — and
 * the bytes never become an IPC payload.
 *
 * Do NOT use it for a `src` / `href` / render prop: a tag cannot send the
 * Authorization header, which is the exact 401-blank-render bug this
 * module exists to prevent. Renderers keep taking blob (or converted PDF)
 * bytes from the cached fetch instead.
 */
export function workspaceFileUrlForRust(
    agentId: string,
    workspaceId: string,
    relPath: string,
): string {
    return workspaceFileUrl(agentId, workspaceId, relPath);
}

function cacheKey(agentId: string, workspaceId: string, relPath: string): string {
    return `${agentId}:${workspaceId}:${relPath}`;
}

/** Strip `; charset=...` parameters; fall back to the blob's own type. */
function normalizeMime(contentType: string | null, blobType: string): string {
    const fromHeader = (contentType ?? "").split(";")[0].trim();
    return fromHeader || blobType || "application/octet-stream";
}

// ── LRU cache ──────────────────────────────────────────────────────────
// Map insertion order doubles as recency order: every hit re-inserts the
// entry, so the front of the iteration is always the least-recently-used.

const MAX_CACHE_BYTES_DEFAULT = 100 * 1024 * 1024;
let maxCacheBytes = MAX_CACHE_BYTES_DEFAULT;

const cache = new Map<string, CacheEntry>();
/** Total blob bytes currently held, kept in sync with every insert/evict. */
let cacheBytes = 0;
/** Concurrent-request dedupe: same key ⇒ one network fetch, many awaiters. */
const inFlight = new Map<string, Promise<CacheEntry>>();

function touch(key: string, entry: CacheEntry): void {
    cache.delete(key);
    cache.set(key, entry);
}

function evictToFit(): void {
    while (cacheBytes > maxCacheBytes && cache.size > 0) {
        const oldestKey = cache.keys().next().value as string;
        const oldest = cache.get(oldestKey);
        cache.delete(oldestKey);
        if (oldest) {
            cacheBytes -= oldest.size;
            URL.revokeObjectURL(oldest.blobUrl);
        }
    }
}

function storeEntry(key: string, entry: CacheEntry): CacheEntry {
    const prev = cache.get(key);
    if (prev) {
        cacheBytes -= prev.size;
        URL.revokeObjectURL(prev.blobUrl);
        cache.delete(key);
    }
    cache.set(key, entry);
    cacheBytes += entry.size;
    evictToFit();
    return entry;
}

async function loadEntry(
    agentId: string,
    workspaceId: string,
    relPath: string,
): Promise<CacheEntry> {
    // Deliberately NO caller signal here: the in-flight promise is shared
    // by every concurrent reader of this key, and a signal aborting the
    // underlying fetch would poison the shared promise with an AbortError
    // for everyone else (the StrictMode-remount failure: round two joined
    // the corpse round one just aborted). The download always runs to
    // completion and lands in the cache; callers who lose interest stop
    // waiting via `untilAbort` instead.
    const resp = await fetch(workspaceFileUrl(agentId, workspaceId, relPath));
    if (!resp.ok) throw new Error(`workspace file → ${resp.status}`);
    const blob = await resp.blob();
    const entry: CacheEntry = {
        blob,
        blobUrl: URL.createObjectURL(blob),
        mime: normalizeMime(resp.headers.get("content-type"), blob.type),
        size: blob.size,
    };
    return storeEntry(cacheKey(agentId, workspaceId, relPath), entry);
}

/**
 * Stop WAITING on `p` when `signal` fires, without cancelling `p` itself.
 * The shared download keeps filling the cache; only this caller gets the
 * AbortError (which the hook treats as expected teardown).
 */
function untilAbort<T>(p: Promise<T>, signal?: AbortSignal): Promise<T> {
    if (!signal) return p;
    if (signal.aborted) {
        return Promise.reject(new DOMException("signal is aborted without reason", "AbortError"));
    }
    return new Promise<T>((resolve, reject) => {
        const onAbort = () =>
            reject(new DOMException("signal is aborted without reason", "AbortError"));
        signal.addEventListener("abort", onAbort, { once: true });
        p.then(
            (v) => {
                signal.removeEventListener("abort", onAbort);
                resolve(v);
            },
            (e) => {
                signal.removeEventListener("abort", onAbort);
                reject(e);
            },
        );
    });
}

function getOrCreateEntry(
    agentId: string,
    workspaceId: string,
    relPath: string,
    force: boolean | undefined,
): Promise<CacheEntry> {
    const key = cacheKey(agentId, workspaceId, relPath);
    if (!force) {
        const hit = cache.get(key);
        if (hit) {
            touch(key, hit);
            return Promise.resolve(hit);
        }
        const pending = inFlight.get(key);
        if (pending) return pending;
    } else {
        // A forced reload must not race a still-pending fetch: the pending
        // one's result would be the stale bytes the caller just asked us to
        // bypass. Drop the old entry now; the new fetch re-stores it.
        invalidate(key);
    }
    const pending = loadEntry(agentId, workspaceId, relPath).finally(() => {
        inFlight.delete(key);
    });
    if (!force) inFlight.set(key, pending);
    return pending;
}

function invalidate(key: string): void {
    const entry = cache.get(key);
    cache.delete(key);
    if (entry) {
        cacheBytes -= entry.size;
        URL.revokeObjectURL(entry.blobUrl);
    }
}

/**
 * Read one workspace file's bytes through the Gateway raw route.
 *
 * Cached: a second call for the same file returns the same `Blob` without a
 * network round-trip unless `force` is set. `signal` stops THIS caller from
 * waiting (rejects with AbortError) but never cancels the shared download —
 * the bytes still land in the cache for the next reader.
 *
 * Throws on a non-2xx response or network failure — callers render the
 * error state with a retry, mirroring fetchAgentFileBlob's contract.
 */
export async function fetchWorkspaceFileBlob(
    agentId: string,
    workspaceId: string,
    relPath: string,
    opts?: { signal?: AbortSignal; force?: boolean },
): Promise<WorkspaceFileBytes> {
    const entry = await untilAbort(getOrCreateEntry(agentId, workspaceId, relPath, opts?.force), opts?.signal);
    return { blob: entry.blob, mime: entry.mime };
}

/**
 * Same read, but handing out the cached `blob:` URL — for `<iframe src>`
 * (native PDF viewer) and `<img src>`. The URL is same-origin and already
 * authorised because it was minted from a fetch that went through the
 * interceptor. Do not treat it as long-lived: the LRU can revoke it when the
 * budget overflows. Hold the promise result only as long as the component
 * using it is mounted, and refetch on remount.
 */
export async function fetchWorkspaceFileBlobUrl(
    agentId: string,
    workspaceId: string,
    relPath: string,
    opts?: { signal?: AbortSignal; force?: boolean },
): Promise<WorkspaceFileUrl> {
    const entry = await untilAbort(getOrCreateEntry(agentId, workspaceId, relPath, opts?.force), opts?.signal);
    return { blobUrl: entry.blobUrl, mime: entry.mime };
}

/**
 * Drop one cached file (revoke its blob URL) so the next read re-fetches.
 * Called when an fs-watch event lands on the editor store — the disk bytes
 * changed, so the cached preview is stale.
 */
export function invalidateWorkspaceFileBlob(
    agentId: string,
    workspaceId: string,
    relPath: string,
): void {
    invalidate(cacheKey(agentId, workspaceId, relPath));
}

/**
 * Save one workspace file through the OS save dialog.
 *
 * The URL crosses the bridge, not the bytes — Rust (`download_attachment`)
 * re-fetches with the account credentials and streams straight to disk, so
 * a 50 MB PDF never becomes a JSON array of numbers over IPC (see
 * [attachmentTransfer.ts](attachmentTransfer.ts) for why). Passing the
 * Gateway URL rather than a `blob:` URL is deliberate: the Rust side has
 * no access to the WebView's blob store, but it can authorise the same
 * raw-file route the cache was filled from.
 */
export function saveWorkspaceFileToDisk(
    agentId: string,
    workspaceId: string,
    relPath: string,
    filename: string,
    onProgress?: (received: number) => void,
): Promise<boolean> {
    return saveUrlToDisk(workspaceFileUrl(agentId, workspaceId, relPath), filename, onProgress);
}

/**
 * "Open externally": download to a temp folder and hand the file to the
 * OS default application (`open_file_externally` on the Rust side). This
 * is the escape hatch for formats the in-app viewers degrade or reject
 * (legacy .doc/.ppt, encrypted docx, corrupted pdf).
 */
export async function openWorkspaceFileExternally(
    agentId: string,
    workspaceId: string,
    relPath: string,
    filename: string,
): Promise<void> {
    await invoke("open_file_externally", {
        url: workspaceFileUrl(agentId, workspaceId, relPath),
        filename,
    });
}

/**
 * Test hook: empty the cache (revoking every blob URL) and optionally
 * override the byte budget so eviction paths are reachable without
 * allocating 100 MiB. Always restore the default between test files.
 */
export function __resetWorkspaceFileBlobCache(maxBytes?: number): void {
    for (const entry of cache.values()) URL.revokeObjectURL(entry.blobUrl);
    cache.clear();
    cacheBytes = 0;
    inFlight.clear();
    maxCacheBytes = maxBytes ?? MAX_CACHE_BYTES_DEFAULT;
}
