import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  DEFAULT_THEME_PACK,
  THEME_PACKS,
  applyThemePack,
  getActiveThemePack,
  initThemePacks,
  setThemePack,
  subscribeThemePacks,
} from "./themePacks";

const __dirname = dirname(fileURLToPath(import.meta.url));
const THEMES_DIR = resolve(__dirname, "themes");
const STORAGE_KEY = "acowork-theme-pack";
/** The reference pack — its token surface is the coverage bar for all
 *  others (see the "SAME token surface" test below). */
const REF_PACK = "paper";
const packCss = (id: string) => readFileSync(resolve(THEMES_DIR, `${id}.css`), "utf8");

/** Tokens globals.css declares, at any nesting level. */
function declaredTokens(): Set<string> {
  const css = readFileSync(resolve(__dirname, "globals.css"), "utf8");
  return new Set([...css.matchAll(/(--[a-zA-Z0-9-]+)\s*:/g)].map((m) => m[1]));
}

/** Token names a pack sets inside ONE scope (light or dark). Blocks are
 *  flat, so scope = text from the selector to the matching close brace. */
function scopeTokens(id: string, scope: "light" | "dark"): string[] {
  const css = packCss(id);
  const marker = scope === "light" ? `:root.theme-${id} {` : `:root.theme-${id}.dark {`;
  const start = css.indexOf(marker);
  expect(start, `${id} missing ${scope} scope marker`).toBeGreaterThanOrEqual(0);
  const end = css.indexOf("}", start);
  return [...css.slice(start, end).matchAll(/^\s*(--[a-zA-Z0-9-]+)\s*:/gm)].map((m) => m[1]);
}

afterEach(() => {
  applyThemePack(DEFAULT_THEME_PACK);
  localStorage.removeItem(STORAGE_KEY);
});

describe("theme packs", () => {
  it("discovers every themes/*.css file without a manifest", () => {
    const files = readdirSync(THEMES_DIR).filter((f) => f.endsWith(".css"));
    expect(THEME_PACKS).toHaveLength(files.length);
    expect(THEME_PACKS.map((p) => p.id)).toEqual(files.map((f) => f.replace(/\.css$/, "")).sort());
    expect(THEME_PACKS.find((p) => p.id === REF_PACK)?.name).toBe("Paper");
  });

  it("parses a preview hue from every pack's light --color-bg", () => {
    for (const pack of THEME_PACKS) {
      expect(pack.hue, `${pack.id} hue`).not.toBeNull();
      expect(pack.hue!).toBeGreaterThanOrEqual(0);
      expect(pack.hue!).toBeLessThan(360);
    }
    // paper.css light block: `--color-bg: hsl(40 var(--theme-chroma) 96%)`
    expect(THEME_PACKS.find((p) => p.id === REF_PACK)?.hue).toBe(40);
  });

  it("overrides only tokens declared in the globals.css contract", () => {
    const declared = declaredTokens();
    for (const pack of THEME_PACKS) {
      const names = [...packCss(pack.id).matchAll(/^\s*(--[a-zA-Z0-9-]+)\s*:/gm)].map((m) => m[1]);
      expect(names.length, `${pack.id} declares no tokens`).toBeGreaterThan(0);
      for (const name of names) {
        expect(declared.has(name), `${pack.id} overrides undeclared ${name}`).toBe(true);
      }
    }
  });

  it("scopes every pack to its own light and dark selectors", () => {
    for (const pack of THEME_PACKS) {
      const css = packCss(pack.id);
      expect(css, `${pack.id} missing light scope`).toContain(`:root.theme-${pack.id} {`);
      expect(css, `${pack.id} missing dark scope`).toContain(`:root.theme-${pack.id}.dark {`);
    }
  });

  it("every pack covers the SAME token surface as the reference pack", () => {
    // The app has no partial-theme support: a pack that only recolours some
    // tokens would leave the rest on the default palette, mixing two themes
    // on screen. Both scopes must therefore cover the full reference set.
    const refLight = scopeTokens(REF_PACK, "light").sort();
    const refDark = scopeTokens(REF_PACK, "dark").sort();
    for (const pack of THEME_PACKS) {
      expect(scopeTokens(pack.id, "light").sort(), `${pack.id} light coverage`).toEqual(refLight);
      expect(scopeTokens(pack.id, "dark").sort(), `${pack.id} dark coverage`).toEqual(refDark);
    }
  });

  it("activates exactly one scope and cleans up on switch", () => {
    applyThemePack(REF_PACK);
    const root = document.documentElement;
    expect(root.classList.contains(`theme-${REF_PACK}`)).toBe(true);
    expect(getActiveThemePack()).toBe(REF_PACK);
    expect([...root.classList].filter((c) => c.startsWith("theme-"))).toEqual([`theme-${REF_PACK}`]);
    // Packs are bundled CSS, not runtime-injected styles — no CSP surface.
    expect(document.querySelectorAll("style[data-theme-pack]")).toHaveLength(0);

    applyThemePack(DEFAULT_THEME_PACK);
    expect(root.classList.contains(`theme-${REF_PACK}`)).toBe(false);
    expect([...root.classList].filter((c) => c.startsWith("theme-"))).toEqual([]);
    expect(getActiveThemePack()).toBe(DEFAULT_THEME_PACK);
  });

  it("never leaves two packs active when switching", () => {
    applyThemePack(REF_PACK);
    applyThemePack(REF_PACK);
    expect([...document.documentElement.classList].filter((c) => c.startsWith("theme-"))).toEqual([
      `theme-${REF_PACK}`,
    ]);
  });

  it("restores the persisted pack at boot and ignores removed packs", () => {
    setThemePack(REF_PACK);
    expect(localStorage.getItem(STORAGE_KEY)).toBe(REF_PACK);
    applyThemePack(DEFAULT_THEME_PACK);
    initThemePacks();
    expect(getActiveThemePack()).toBe(REF_PACK);

    localStorage.setItem(STORAGE_KEY, "pack-that-no-longer-exists");
    initThemePacks();
    expect(getActiveThemePack()).toBe(DEFAULT_THEME_PACK);
  });

  it("keeps the .dark class untouched so light/dark still works", () => {
    const root = document.documentElement;
    root.classList.add("dark");
    applyThemePack(REF_PACK);
    expect(root.classList.contains("dark")).toBe(true);
    expect(root.classList.contains(`theme-${REF_PACK}`)).toBe(true);
    root.classList.remove("dark");
  });

  it("notifies subscribers when the pack changes (two doors stay in sync)", () => {
    // The title-bar menu and Settings ▸ Appearance both render the active
    // pack. `setThemePack` must notify them so a pick in one door is
    // immediately visible in the other, and unsubscribing stops the flow.
    const seen: string[] = [];
    const unsub = subscribeThemePacks(() => seen.push(getActiveThemePack()));
    setThemePack(REF_PACK);
    unsub();
    setThemePack(DEFAULT_THEME_PACK);
    expect(seen).toEqual([REF_PACK]);
  });
});
