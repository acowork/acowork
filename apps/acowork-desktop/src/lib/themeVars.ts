/**
 * Live CSS-variable reads for surfaces that third-party renderers bake into
 * pixels (mermaid SVGs, VS Code-style widget chrome) and therefore cannot
 * re-point at a `var()` after the fact.
 *
 * Every call passes an explicit fallback: the pinned literal the call site
 * used before theming. jsdom resolves no custom properties, so tests keep
 * exercising the fallback path and the byte-equality expectations hold; in
 * the real DOM the resolved value (light, dark, or an active theme pack)
 * wins. A theme pack recolours these surfaces by overriding the token in
 * globals.css — no code change, no renderer change.
 */

/** Read a custom property from <html> with a fallback. Never throws. */
export function readThemeVar(name: string, fallback: string): string {
  if (typeof document === "undefined") return fallback;
  try {
    const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return value || fallback;
  } catch {
    return fallback;
  }
}
