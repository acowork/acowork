/// <reference types="vite/client" />
/**
 * Theme packs — drop a CSS file into ./themes/ and it joins the app.
 *
 * The eager glob is both the registry and the loader: Vite bundles and injects
 * every themes/*.css at build time, with no manifest, no import list and no
 * settingsStore entry to update. Each file is scoped to `:root.theme-<id>` /
 * `:root.theme-<id>.dark`, so inactive packs are inert by construction and
 * activating one is just a class toggle. That is what makes "add one CSS
 * file" sufficient, and it avoids runtime <style> injection (which a Tauri
 * WebView CSP can block).
 *
 * Trade-off: every pack ships in the main CSS bundle. Packs contain only
 * custom-property overrides, so this is a few KB per theme with zero runtime
 * cost when switching.
 *
 * A pack may ONLY override custom properties declared by the contract in
 * globals.css. Unknown names are rejected by dev/ci.sh's
 * run_css_theme_redline and by themePacks.test.ts — a typo here would
 * otherwise be a silent no-op rather than a visible failure.
 */
const STORAGE_KEY = "acowork-theme-pack";
const ATTR = "data-theme-pack";
export const DEFAULT_THEME_PACK = "default";

const modules = import.meta.glob("./themes/*.css", { eager: true });

export interface ThemePack {
  id: string;
  name: string;
}

function idFromPath(path: string): string {
  return path.split("/").pop()!.replace(/\.css$/, "");
}

function displayName(id: string): string {
  return id
    .split(/[-_]/)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

/** Sorted so a future picker is stable regardless of filesystem order. */
export const THEME_PACKS: ThemePack[] = Object.keys(modules)
  .map((path) => {
    const id = idFromPath(path);
    return { id, name: displayName(id) };
  })
  .sort((a, b) => a.id.localeCompare(b.id));

/**
 * Activate a pack by id. `default` (or any unknown id, e.g. a stale value for
 * a pack that was removed) restores the built-in :root/.dark palette.
 */
export function applyThemePack(id: string): void {
  if (typeof document === "undefined") return;
  const root = document.documentElement;
  for (const cls of Array.from(root.classList)) {
    if (cls.startsWith("theme-")) root.classList.remove(cls);
  }
  root.removeAttribute(ATTR);
  if (!THEME_PACKS.some((p) => p.id === id)) return;
  root.classList.add(`theme-${id}`);
  root.setAttribute(ATTR, id);
}

/**
 * Change-notification for the pack's TWO doors (the title-bar quick menu
 * and Settings ▸ Appearance). Packs are plain DOM state — a class on
 * <html> — so there is no store to subscribe to; this tiny listener set
 * is what keeps the two entry points from drifting when one of them
 * changes the pack while the other is mounted.
 */
const listeners = new Set<() => void>();

export function subscribeThemePacks(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function notifyThemePacks(): void {
  for (const listener of listeners) listener();
}

export function getActiveThemePack(): string {
  if (typeof document === "undefined") return DEFAULT_THEME_PACK;
  return document.documentElement.getAttribute(ATTR) ?? DEFAULT_THEME_PACK;
}

export function setThemePack(id: string): void {
  try {
    if (id === DEFAULT_THEME_PACK || !THEME_PACKS.some((p) => p.id === id)) {
      localStorage.removeItem(STORAGE_KEY);
    } else {
      localStorage.setItem(STORAGE_KEY, id);
    }
  } catch {
    // localStorage can be unavailable in a locked-down WebView; the in-memory
    // pack still applies for this session.
  }
  applyThemePack(id);
  notifyThemePacks();
}

/** Called once at boot, after settingsStore has applied the light/dark class. */
export function initThemePacks(): void {
  let id = DEFAULT_THEME_PACK;
  try {
    id = localStorage.getItem(STORAGE_KEY) ?? DEFAULT_THEME_PACK;
  } catch {
    // fall through to the default palette
  }
  applyThemePack(THEME_PACKS.some((p) => p.id === id) ? id : DEFAULT_THEME_PACK);
}
