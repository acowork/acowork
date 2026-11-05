/**
 * Per-account namespace for user-level preferences (ADR-076).
 *
 * A preference is *user-level* when changing it should not change the
 * experience of the next person to sign in on this machine: theme, accent
 * color, font size, content width, window opacity, UI language. Before this
 * module those lived under one flat localStorage key shared by every account,
 * so signing in as a second user inherited the first one's appearance.
 *
 * Machine-level settings are deliberately NOT scoped — gateway URL, gateway
 * mode, log level / file size / count, auto-start-local-node. Those describe
 * the deployment, not the person, and a login screen still needs to know
 * which Gateway to reach before anyone has signed in.
 *
 * The scope is resolved **synchronously** from the persisted access token,
 * because `settingsStore` is imported at the top of `main.tsx` to apply the
 * theme before first paint — long before `authStore.init()` resolves the
 * account over HTTP. The token's `sub` claim IS the `user_id`
 * (`acowork-core/src/auth/claims.rs`), and reading it is the same
 * unverified-decode trick `lib/session/tokens.ts` already uses to schedule
 * rotation: it authenticates nothing, it only picks a storage key. A forged
 * `sub` could at worst aim a browser at another account's *appearance*
 * settings on the same machine, which the attacker already has physical
 * access to.
 *
 * In `AUTH_MODE=local` there is no token, so the scope stays `null` and every
 * key resolves to its original unscoped name — the historical single-user
 * behavior is untouched.
 */

import { decodeJwtPayload } from "./jwt";

const TOKEN_STORAGE_KEY = "acowork.auth.tokens";

/** The account whose preferences we are reading and writing, or `null` for
 *  "no account" (local mode, or logged out). */
let scope: string | null = null;

/** Listeners re-read their persisted values when the scope changes — the
 *  login path does not reload the page, so a store initialized under the
 *  pre-login scope has to be told to pick up the account's own values. */
const listeners = new Set<(scope: string | null) => void>();

/** Read `user_id` out of the persisted access token without waiting for
 *  `authStore`. Returns `null` when there is no session or the token is not
 *  JWT-shaped. */
function subFromStoredToken(): string | null {
  try {
    const raw = localStorage.getItem(TOKEN_STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as { accessToken?: string };
    if (!parsed.accessToken) return null;
    const sub = decodeJwtPayload(parsed.accessToken)?.sub;
    return typeof sub === "string" && sub ? sub : null;
  } catch {
    return null;
  }
}

// Seed at import so the very first `prefKey()` of the boot sequence is
// already account-correct.
scope = subFromStoredToken();

/**
 * The scoped localStorage name for a logical key.
 *
 * Unscoped when there is no account. Callers that need to fall back to a
 * legacy unscoped value should use `readPref` rather than this.
 */
export function prefKey(base: string): string {
  return scope === null ? base : `${base}:${scope}`;
}

/** Current scope, for logging and tests. */
export function currentScope(): string | null {
  return scope;
}

/**
 * Read a preference, falling back to the legacy unscoped key once.
 *
 * The fallback is what makes the upgrade invisible: an existing single-user
 * install keeps its theme and accent the first time it signs in, because the
 * scoped key does not exist yet and the old one still answers. From the first
 * write onward the account has its own copy and the two diverge.
 */
export function readPref(base: string): string | null {
  try {
    const scoped = localStorage.getItem(prefKey(base));
    if (scoped !== null) return scoped;
    return scope === null ? null : localStorage.getItem(base);
  } catch {
    return null;
  }
}

/** Write a preference under the current account scope. */
export function writePref(base: string, value: string): void {
  try {
    localStorage.setItem(prefKey(base), value);
  } catch {
    // Storage full / privacy mode — the in-memory value still stands.
  }
}

/**
 * Move the scope to another account (or to `null` on sign-out).
 *
 * No-ops when the scope is unchanged, which is the common case: the boot-time
 * seed already read the same `sub` that `/api/auth/me` later reports, so this
 * fires only on a real account change.
 */
export function setPrefScope(next: string | null): void {
  if (next === scope) return;
  scope = next;
  for (const cb of listeners) cb(next);
}

/** Subscribe to scope changes. Returns an unsubscribe function. */
export function onScopeChange(cb: (scope: string | null) => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}
