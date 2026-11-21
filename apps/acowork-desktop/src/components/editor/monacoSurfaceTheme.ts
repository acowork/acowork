/**
 * Monaco's document surface, sourced from the theme-pack token.
 *
 * Why this exists. The file tab has two bodies behind it: the Markdown/PDF/
 * image preview, which paints `bg-editor-canvas`, and the Monaco code editor,
 * which used the built-in `vs` / `vs-dark` themes. Those two built-ins carry
 * hardcoded #FFFFFF / #1E1E1E and are invisible to theme packs, so a pack
 * tinted the preview and left the code editor white. The tab strip then had to
 * pick a side: pinning it to `bg-white` matched the code editor and left a
 * white slab above the tinted preview (the bug this fixes); pinning it to the
 * pack colour did the reverse. Both were symptoms of the document surface
 * having two unrelated sources of truth.
 *
 * So: one token, `--color-editor-canvas`, declared in the theme-pack contract
 * and overridden per pack. The preview already reads it through Tailwind; a
 * theme registered here gives Monaco the same value. The tab strip and every
 * other chrome surface around the document read the token too, which makes the
 * whole file pane one continuous panel in every pack and both modes.
 *
 * Reading, not re-declaring. The value is read back from the DOM rather than
 * duplicated into a TS map, because the pack CSS is the definition — a second
 * copy in JS is a drift hazard the moment someone adds `themes/rosewater.css`.
 * The cost is that resolution is imperative and needs the DOM, hence the
 * fallbacks below.
 */

/** The single token every document surface in the file pane reads from. */
export const SURFACE_TOKEN = "--color-editor-canvas";

/** Theme-name prefix. A revision suffix is appended — see `surfaceThemeName`. */
export const SURFACE_THEME_PREFIX = "acowork-surface";

/**
 * Used when the token cannot be resolved (no DOM, jsdom without CSS variables,
 * or a pack that does not override the token). These are the `vs` / `vs-dark`
 * values, so the worst case is the pre-fix appearance, never an unstyled editor.
 */
export const SURFACE_FALLBACK: Record<"light" | "dark", string> = {
    light: "#FFFFFF",
    dark: "#1E1E1E",
};

/**
 * Parse a computed CSS colour into the `#rrggbb[aa]` form Monaco requires.
 *
 * `getComputedStyle` always serialises a colour as `rgb()`/`rgba()` — in both
 * the legacy comma and the modern space/slash syntax — so this is the only
 * conversion needed, and it needs no canvas backend. Monaco's own parser takes
 * hex and `rgba()` but not `hsl()`, which is why the packs' `hsl(...)` values
 * cannot be handed over raw.
 */
export function computedColorToHex(value: string): string | null {
    const m = /^rgba?\((.*)\)$/.exec(value.trim());
    if (!m) return null;
    const parts = m[1].includes(",")
        ? m[1].split(",").map((p) => p.trim())
        : m[1].split(/[\s/]+/).filter(Boolean);
    if (parts.length < 3) return null;

    const channels = parts.slice(0, 3).map((p) => Number(p));
    if (channels.some((n) => !Number.isFinite(n))) return null;

    const to2 = (n: number) => Math.round(Math.min(255, Math.max(0, n))).toString(16).padStart(2, "0");
    let alpha = "";
    if (parts.length >= 4) {
        const raw = parts[3];
        const pct = raw.endsWith("%");
        const n = Number(pct ? raw.slice(0, -1) : raw);
        if (!Number.isFinite(n)) return null;
        alpha = to2(Math.min(1, Math.max(0, pct ? n / 100 : n)) * 255);
    }
    return `#${to2(channels[0])}${to2(channels[1])}${to2(channels[2])}${alpha}`;
}

/**
 * Resolve an arbitrary CSS colour expression to hex using the browser as the
 * colour engine.
 *
 * Assigning to a *real* property (`color`) and reading the computed value back
 * goes through the full substitution pipeline, so it resolves `var()` references
 * and every colour syntax into `rgb()`. Reading the custom property directly
 * would instead rely on substitution happening at custom-property
 * computed-value time — the packs write
 * `hsl(211 var(--theme-chroma) 95%)`, and a value that failed to substitute
 * would silently degrade the editor to the fallback while the tab strip stayed
 * tinted. The probe removes that dependency.
 */
export function resolveColor(value: string): string | null {
    if (typeof document === "undefined" || typeof getComputedStyle !== "function") return null;
    const probe = document.createElement("span");
    probe.style.color = value;
    // The CSSOM rejects an unparseable assignment outright, which would
    // otherwise read back as the inherited colour and look like a real value.
    if (!probe.style.color) return null;
    const host = document.body ?? document.documentElement;
    if (!host) return null;
    host.appendChild(probe);
    try {
        return computedColorToHex(getComputedStyle(probe).color);
    } finally {
        probe.remove();
    }
}

/**
 * Resolve the current pack's document surface colour.
 *
 * Mode needs no argument: every pack defines the token twice, once on the root
 * and once under `.dark`, so the computed value already reflects whichever mode
 * is active. Passing a mode here would be a second, contradictable source.
 */
export function readSurfaceColor(isDark: boolean): string {
    const fallback = SURFACE_FALLBACK[isDark ? "dark" : "light"];
    if (typeof document === "undefined" || typeof getComputedStyle !== "function") return fallback;
    const raw = getComputedStyle(document.documentElement).getPropertyValue(SURFACE_TOKEN);
    if (!raw.trim()) return fallback;
    return resolveColor(raw) ?? fallback;
}

/** Parse `#rrggbb[aa]` into channels; null for anything else. */
function parseHex(hex: string): [number, number, number, number] | null {
    const m = /^#([0-9a-fA-F]{6})([0-9a-fA-F]{2})?$/.exec(hex);
    if (!m) return null;
    const r = parseInt(m[1].slice(0, 2), 16);
    const g = parseInt(m[1].slice(2, 4), 16);
    const b = parseInt(m[1].slice(4, 6), 16);
    const a = m[2] ? parseInt(m[2], 16) / 255 : 1;
    return [r, g, b, a];
}

/**
 * Blend the surface toward black (`towardBlack`) or white by `amount` per
 * channel, to get the line-highlight and indent-guide shades a flat tint
 * cannot express.
 *
 * Both built-ins put these on the *opposite* side of the canvas — a light grey
 * rule on `vs`'s white, a lighter grey on `vs-dark`'s near-black — so callers
 * pass `!isDark`, not `isDark`.
 *
 * Deriving from the pack's own colour rather than naming literals is what keeps
 * a pack tinted at all: a hardcoded `#F3F3F3` highlight on a `#F8F5F0` canvas is
 * the same two-tone seam, one shade lighter.
 */
export function shadeSurface(hex: string, towardBlack: boolean, amount: number): string {
    const rgb = parseHex(hex);
    if (!rgb) return hex;
    const [r, g, b, a] = rgb;
    const target = towardBlack ? 0 : 255;
    const ch = (v: number) => Math.round(v + (target - v) * amount);
    const to2 = (v: number) => v.toString(16).padStart(2, "0");
    const alpha = a < 1 ? to2(Math.round(a * 255)) : "";
    return `#${to2(ch(r))}${to2(ch(g))}${to2(ch(b))}${alpha}`;
}

/**
 * The theme definition for a given surface colour.
 *
 * `inherit: true` keeps the base theme's token colours (syntax highlighting)
 * untouched — only the flat background family is redirected, which is exactly
 * the set that produced the seam. Everything not listed stays Monaco's call.
 */
export function buildSurfaceTheme(isDark: boolean, canvas: string) {
    // Both directions go *away* from the canvas, matching `vs` (grey rules on
    // white) and `vs-dark` (lighter grey on near-black).
    const highlight = shadeSurface(canvas, !isDark, 0.03);
    const guide = shadeSurface(canvas, !isDark, 0.1);
    const guideActive = shadeSurface(canvas, !isDark, 0.28);
    return {
        base: (isDark ? "vs-dark" : "vs") as "vs" | "vs-dark",
        inherit: true as const,
        rules: [] as { token: string; foreground?: string }[],
        colors: {
            // The flat background family. Every one of these defaults to
            // #FFFFFF / #1E1E1E in the built-ins, so leaving any out re-opened
            // the seam in a corner nobody looks at until a pack is enabled:
            // the gutter beside the text, and the popups that float over it.
            "editor.background": canvas,
            "editorGutter.background": canvas,
            "editorWidget.background": canvas,
            "editorSuggestWidget.background": canvas,
            "editorHoverWidget.background": canvas,
            "editor.lineHighlightBackground": highlight,
            "editorIndentGuide.background1": guide,
            "editorIndentGuide.activeBackground1": guideActive,
        },
    };
}

/**
 * Names already registered on the live Monaco instance.
 *
 * The name embeds the resolved colour, which is load-bearing rather than
 * cosmetic: `@monaco-editor/react` re-applies a theme only when the `theme`
 * prop *changes*, so a name that stayed `acowork-surface-light` across a pack
 * switch would leave every open editor painted with the previous tint. Because
 * the name is a pure function of (mode, colour) it is also computable at render
 * time, before anything is registered.
 */
const registered = new Set<string>();

/**
 * Structural rather than `monaco.editor`: this module's whole contract is
 * `defineTheme`, and naming only that keeps the module — and its tests — free
 * of a Monaco import.
 */
export type SurfaceThemeRegistrar = {
    defineTheme(name: string, theme: ReturnType<typeof buildSurfaceTheme>): void;
};

export function surfaceThemeName(isDark: boolean, canvas: string): string {
    return `${SURFACE_THEME_PREFIX}-${isDark ? "dark" : "light"}-${canvas.replace(/^#/, "").toLowerCase()}`;
}

/**
 * The most recently registered name, or null before the first registration.
 *
 * Monaco's theme registry is process-global but React state is not, and the
 * file pane unmounts whenever the last file tab closes. Without this, every
 * re-open would start from the pack-blind built-in and repaint one frame later
 * — a white flash that reads as a glitch in exactly the tinted packs this
 * feature exists for.
 */
let lastName: string | null = null;

export function lastSurfaceThemeName(): string | null {
    return lastName;
}

export function ensureSurfaceTheme(
    registrar: SurfaceThemeRegistrar,
    isDark: boolean,
    canvas: string = readSurfaceColor(isDark),
): string {
    const name = surfaceThemeName(isDark, canvas);
    if (!registered.has(name)) {
        registrar.defineTheme(name, buildSurfaceTheme(isDark, canvas));
        registered.add(name);
    }
    lastName = name;
    return name;
}

/** Test seam: forget what was registered so each case starts clean. */
export function __resetSurfaceThemeRegistry(): void {
    registered.clear();
    lastName = null;
}
