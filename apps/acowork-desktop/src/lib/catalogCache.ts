/**
 * Persistent cache for network-backed catalogs (app gallery, agent hub,
 * installed apps).
 *
 * The problem this solves: every surface that draws catalog artwork — the
 * left-nav hosted-app icons, the Apps workbench, the Agent 库 tab — started
 * from an empty state and painted the real icon only after a network fetch
 * resolved, so each app launch showed a letter placeholder that later
 * flipped to the artwork. A successful fetch is now written here, and the
 * next entry into a surface seeds its state synchronously from the cache
 * (optimistic first paint), then revalidates over the network and applies
 * the fresh data ONLY when it actually differs — an unchanged payload must
 * not cause a re-render.
 *
 * Why localStorage and not a file: this is the repo's established webview
 * persistence (`acowork.*` keys in agentStore profiles, settingsStore,
 * session tokens, the gallery URL override itself). The payloads are small
 * (the published gallery index is ~15 KB including every README), the read
 * must be synchronous to seed a first frame, and a second persistence
 * mechanism would be exactly the inconsistency this repo's conventions
 * warn against.
 *
 * All operations are total: a missing key, a private-mode throw, or a
 * corrupt value all degrade to `null` / a no-op — the cache is an
 * optimization and must never become a failure mode.
 */

const PREFIX = "acowork.cache.";

/** The last successfully fetched catalog payload, or `null` when absent or
 *  unreadable. `validate` guards against a stale or hand-edited value that
 *  parses but is not the shape the caller will index into. */
export function readCatalog<T>(key: string, validate?: (value: unknown) => boolean): T | null {
    try {
        const raw = localStorage.getItem(PREFIX + key);
        if (!raw) return null;
        const parsed = JSON.parse(raw) as T;
        if (parsed === null || parsed === undefined) return null;
        if (validate && !validate(parsed)) return null;
        return parsed;
    } catch {
        // localStorage unavailable (private mode) or corrupt JSON: no cache,
        // not an error — the caller falls back to the network-only path.
        return null;
    }
}

/** Persist a fresh catalog payload. Failures (quota, private mode) are
 *  swallowed: losing the cache costs one more letter-tile frame, never the
 *  data itself. */
export function writeCatalog(key: string, data: unknown): void {
    try {
        localStorage.setItem(PREFIX + key, JSON.stringify(data));
    } catch {
        // Quota exceeded or storage disabled — the next entry just seeds
        // from nothing and behaves like today.
    }
}

/** Drop a cached payload (test seam / explicit invalidation). */
export function clearCatalog(key: string): void {
    try {
        localStorage.removeItem(PREFIX + key);
    } catch {
        // ignore
    }
}

/**
 * Structural equality for catalog payloads.
 *
 * `JSON.stringify` comparison is sound HERE because both sides come from
 * the same producer (one server's JSON, or the same Rust serializer), so
 * key order is stable; a hand-built object and a parsed one are never
 * compared. Callers pass the result to a functional `setState` — returning
 * the previous reference is what makes React skip the re-render entirely.
 */
export function sameCatalog(a: unknown, b: unknown): boolean {
    if (a === b) return true;
    try {
        return JSON.stringify(a) === JSON.stringify(b);
    } catch {
        return false;
    }
}
