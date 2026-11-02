/**
 * prefsScope — the per-account namespace for user-level preferences.
 *
 * The property that matters is *isolation*: two accounts on one machine must
 * not see each other's appearance. Everything else here exists to serve that,
 * plus one counter-property: `AUTH_MODE=local` has no account at all, so it
 * must keep using the original unscoped keys (ADR-076 §决策 12 makes local a
 * no-op, and a refactor that silently renamed single-user installs' prefs
 * would reset them).
 */
import { describe, it, expect, beforeEach, vi } from "vitest";

const TOKEN_KEY = "acowork.auth.tokens";
const BASE = "acowork-theme";

/** A JWT-shaped token whose payload carries `sub`. Only the payload matters:
 *  `prefsScope` decodes it unverified, exactly as `lib/session/tokens.ts`
 *  does for rotation timing. */
function tokenFor(sub: string): string {
  const b64 = (o: unknown) =>
    btoa(JSON.stringify(o)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  return `${b64({ alg: "none" })}.${b64({ sub, exp: Math.floor(Date.now() / 1000) + 900 })}.sig`;
}

function storedToken(sub: string | null): void {
  if (sub === null) localStorage.removeItem(TOKEN_KEY);
  else localStorage.setItem(TOKEN_KEY, JSON.stringify({ accessToken: tokenFor(sub), refreshToken: "r" }));
}

/** Re-import the module so its import-time seed runs against fresh storage. */
async function freshScope(sub: string | null) {
  storedToken(sub);
  vi.resetModules();
  return await import("./prefsScope");
}

describe("prefsScope", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("uses the unscoped key when there is no account (local mode)", async () => {
    const m = await freshScope(null);
    expect(m.prefKey(BASE)).toBe(BASE);
    expect(m.currentScope()).toBeNull();
  });

  it("seeds the scope synchronously from the stored token's sub at import", async () => {
    // This is what keeps first-paint theming account-correct: no HTTP, no
    // waiting for authStore.init().
    const m = await freshScope("user-aaa");
    expect(m.currentScope()).toBe("user-aaa");
    expect(m.prefKey(BASE)).toBe(`${BASE}:user-aaa`);
  });

  it("falls back to the legacy unscoped value until the account writes its own", async () => {
    localStorage.setItem(BASE, "dark");
    const m = await freshScope("user-aaa");
    expect(m.readPref(BASE)).toBe("dark");

    m.writePref(BASE, "light");
    // The legacy key is left alone — other accounts still start from it.
    expect(localStorage.getItem(BASE)).toBe("dark");
    expect(m.readPref(BASE)).toBe("light");
  });

  it("isolates two accounts on the same machine", async () => {
    const a = await freshScope("user-aaa");
    a.writePref(BASE, "dark");

    const b = await freshScope("user-bbb");
    // b has never written, and there is no legacy key → its own default, not a's.
    expect(b.readPref(BASE)).toBeNull();

    b.writePref(BASE, "light");
    expect((await freshScope("user-aaa")).readPref(BASE)).toBe("dark");
    expect((await freshScope("user-bbb")).readPref(BASE)).toBe("light");
  });

  it("notifies listeners only on an actual scope change", async () => {
    const m = await freshScope("user-aaa");
    const cb = vi.fn();
    m.onScopeChange(cb);

    m.setPrefScope("user-aaa");
    expect(cb).not.toHaveBeenCalled();

    m.setPrefScope("user-bbb");
    expect(cb).toHaveBeenCalledWith("user-bbb");
    expect(m.prefKey(BASE)).toBe(`${BASE}:user-bbb`);
  });

  it("survives a corrupt or non-JWT token by staying unscoped", async () => {
    localStorage.setItem(TOKEN_KEY, "{not json");
    vi.resetModules();
    const m = await import("./prefsScope");
    expect(m.currentScope()).toBeNull();

    storedToken(null);
    localStorage.setItem(TOKEN_KEY, JSON.stringify({ accessToken: "opaque-token", refreshToken: "r" }));
    vi.resetModules();
    const m2 = await import("./prefsScope");
    expect(m2.currentScope()).toBeNull();
  });
});
