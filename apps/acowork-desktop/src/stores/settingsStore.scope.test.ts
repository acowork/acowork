/**
 * Preference isolation across accounts, through the real store.
 *
 * `prefsScope.test.ts` pins the key naming. This pins the part that can only
 * break at the seam: `settingsStore` reads its values once at import — before
 * anyone has signed in — and `authStore.login()` resolves the account *in
 * place* without reloading the page (only sign-out and account switch
 * reload). So a store initialized under the pre-login scope has to be pushed
 * onto the new account's values, or signing in as a second user keeps showing
 * the first user's appearance until the next restart.
 *
 * The DOM is asserted alongside the state because the theme and accent are
 * the whole point: a store that re-reads but never re-applies would render
 * account B's UI in account A's colors.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";

const root = () => document.documentElement;

/** Import both modules fresh, so the store's import-time read runs against
 *  the storage and scope the test just set up. */
async function boot(userId: string | null) {
  vi.resetModules();
  const scope = await import("../lib/prefsScope");
  scope.setPrefScope(userId);
  const { useSettingsStore } = await import("./settingsStore");
  return { useSettingsStore, scope };
}

describe("settingsStore per-account isolation", () => {
  beforeEach(() => {
    localStorage.clear();
    root().classList.remove("dark");
    root().style.removeProperty("--color-accent");
    root().style.removeProperty("--glass-tint-light");
    root().style.removeProperty("--glass-tint-dark");
  });

  it("adopts the signing-in account's theme and accent without a reload", async () => {
    // Both accounts' preferences exist on disk before anyone signs in.
    localStorage.setItem("acowork-theme:u-a", "dark");
    localStorage.setItem("acowork-accent-color:u-a", "#d4228a");
    localStorage.setItem("acowork-theme:u-b", "light");

    // Boot happens with no account: nothing unscoped exists, so defaults.
    const anon = await boot(null);
    expect(anon.useSettingsStore.getState().theme).toBe("system");

    // The login resolves account A — in place, no page reload.
    anon.scope.setPrefScope("u-a");
    expect(anon.useSettingsStore.getState().theme).toBe("dark");
    expect(anon.useSettingsStore.getState().accentColor).toBe("#d4228a");
    expect(root().classList.contains("dark")).toBe(true);
    expect(root().style.getPropertyValue("--color-accent")).toBe("#d4228a");

    // Switching to B picks up B's theme, and B's *absent* accent must not
    // leave A's custom glass tint stranded on the surface.
    anon.scope.setPrefScope("u-b");
    expect(anon.useSettingsStore.getState().theme).toBe("light");
    expect(root().classList.contains("dark")).toBe(false);
    expect(root().style.getPropertyValue("--glass-tint-light")).toBe("");
  });

  it("writes land under the account's own key, not the shared one", async () => {
    const { useSettingsStore } = await boot("u-a");
    useSettingsStore.getState().setTheme("dark");
    expect(localStorage.getItem("acowork-theme:u-a")).toBe("dark");
    // The unscoped legacy key stays untouched, so a second account still
    // starts from whatever the machine had before this feature.
    expect(localStorage.getItem("acowork-theme")).toBeNull();
  });

  it("keeps one flat namespace when no account is in play (local mode)", async () => {
    const { useSettingsStore } = await boot(null);
    useSettingsStore.getState().setTheme("dark");
    expect(localStorage.getItem("acowork-theme")).toBe("dark");
    expect(localStorage.getItem("acowork-theme:null")).toBeNull();
  });

  it("inherits a pre-existing global preference on first sign-in", async () => {
    // An existing single-user install: the legacy key holds its theme. The
    // first account to sign in keeps it rather than waking up on defaults.
    localStorage.setItem("acowork-theme", "dark");
    const { useSettingsStore } = await boot("u-a");
    expect(useSettingsStore.getState().theme).toBe("dark");
  });

  it("leaves machine-level settings out of the namespace", async () => {
    // The Gateway URL is what the login round-trip itself depends on; scoping
    // it would strand a returning user on the wrong server before they sign in.
    localStorage.setItem("acowork-gateway-url", "http://10.0.0.7:19876");
    const { useSettingsStore } = await boot("u-a");
    expect(useSettingsStore.getState().gatewayUrl).toBe("http://10.0.0.7:19876");
    useSettingsStore.getState().setGatewayUrl("http://10.0.0.9:19876");
    expect(localStorage.getItem("acowork-gateway-url")).toBe("http://10.0.0.9:19876");
  });
});
