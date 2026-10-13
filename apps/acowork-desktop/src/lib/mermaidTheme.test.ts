/**
 * Mermaid palette contracts.
 *
 * Why this file exists:
 *   `ensureInit()` used to hardcode one light palette forever. Mermaid bakes
 *   colors into the SVG at render time, so every diagram kept that palette
 *   even under `.dark` — edges rendered as `#94a3b8` on `#131316` (2.3:1,
 *   below the 3:1 WCAG 1.4.11 floor for graphical objects), and edge labels
 *   painted a white plate that read as a white line.
 *
 *   That is invisible to jsdom (no cascade, no paint) and to `tsc` (they are
 *   strings). So the checks below assert on the token values themselves:
 *   light is pinned to the exact pre-split palette (no visual regression),
 *   dark is pinned to contrast ratios that actually resolve the complaint.
 */
import { describe, it, expect } from "vitest";
import {
  LIGHT_TOKENS,
  DARK_TOKENS,
  tokensFor,
  isDarkTheme,
} from "./mermaidTheme";

/** WCAG 2.x relative luminance + contrast, for 6-digit hex. */
function luminance(hex: string): number {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) throw new Error(`not a 6-digit hex color: ${hex}`);
  const int = parseInt(m[1], 16);
  const srgb = [(int >> 16) & 255, (int >> 8) & 255, int & 255].map((c) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  });
  return 0.2126 * srgb[0] + 0.7152 * srgb[1] + 0.0722 * srgb[2];
}

function contrast(a: string, b: string): number {
  const la = luminance(a);
  const lb = luminance(b);
  const [hi, lo] = la > lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

/** WCAG 1.4.11 non-text contrast floor for boundaries/graphics. */
const GRAPHIC_MIN = 3;
/** WCAG 1.4.3 normal-text floor. */
const TEXT_MIN = 4.5;

describe("mermaid light palette — regression lock", () => {
  // These are the exact values that were hardcoded in ensureInit() before
  // theming was extracted. If one changes, light mode has a visual
  // regression and this test must fail loudly.
  const PINNED = {
    background: "#ffffff",
    primaryColor: "#f8fafc",
    primaryBorderColor: "#cbd5e1",
    primaryTextColor: "#334155",
    lineColor: "#94a3b8",
    secondaryColor: "#f0fdf5",
    tertiaryColor: "#fdfaf5",
    clusterBkg: "#f8fafc",
    clusterBorder: "#d1d5db",
    edgeLabelBackground: "#ffffff",
    nodeBorder: "#cbd5e1",
    nodeTextColor: "#334155",
    fontSize: "12px",
    nodeBorderRadius: 12,
  } as const;

  it.each(Object.entries(PINNED))("keeps %s unchanged", (key, expected) => {
    expect(LIGHT_TOKENS.themeVariables[key]).toBe(expected);
  });

  it("preserves the rounded node shape", () => {
    expect(LIGHT_TOKENS.themeCSS).toContain("rx: 12px !important");
    expect(LIGHT_TOKENS.themeCSS).toContain("ry: 12px !important");
  });

  it("preserves the light subgraph fill tints", () => {
    for (const fill of ["#f0fdf5", "#fdfaf5", "#f8f6fc"]) {
      expect(LIGHT_TOKENS.themeCSS).toContain(fill);
    }
  });

  it("adds no dark-mode thickness/arrow overrides", () => {
    // Stroke-width and arrowhead fixes are dark-only; if they leak into
    // light they would silently restyle the light diagram.
    expect(LIGHT_TOKENS.themeCSS).not.toContain(".arrowheadPath");
    expect(LIGHT_TOKENS.themeCSS).not.toContain("stroke-width: 1.5px");
  });
});

describe("mermaid dark palette — contrast contract", () => {
  const bg = DARK_TOKENS.themeVariables.background;

  it("paints on the app's dark chat surface, not white", () => {
    // Matches --color-chat-body in globals.css .dark block (zinc-950).
    expect(bg).toBe("#131316");
  });

  it("keeps edges above the 3:1 graphical-object floor", () => {
    // This is the reported bug: 1px edges were unreadable on the dark
    // canvas. WCAG 1.4.11 floor is 3:1; light mode's own lineColor only
    // manages 2.56:1 on white, which is why dark needs a brighter value,
    // not a 1:1 port of the light palette.
    expect(contrast(DARK_TOKENS.themeVariables.lineColor, bg)).toBeGreaterThanOrEqual(GRAPHIC_MIN);
  });

  it("gives blocks at least light mode's silhouette contrast", () => {
    // Node fill and border are a hairline pair, not a text graphic: light
    // mode ships fill-vs-canvas 1.05 and border-vs-fill 1.42. Asserting an
    // absolute 3:1 here would force a redesign of the established design
    // language in BOTH modes. Instead: dark must be no worse than light,
    // which is what keeps "blocks are clearly readable" true after the fix.
    const fillVsCanvas = contrast(DARK_TOKENS.themeVariables.primaryColor, bg);
    const borderVsFill = contrast(DARK_TOKENS.themeVariables.nodeBorder, DARK_TOKENS.themeVariables.primaryColor);
    expect(fillVsCanvas).toBeGreaterThanOrEqual(contrast(LIGHT_TOKENS.themeVariables.primaryColor, LIGHT_TOKENS.themeVariables.background));
    expect(borderVsFill).toBeGreaterThanOrEqual(contrast(LIGHT_TOKENS.themeVariables.nodeBorder, LIGHT_TOKENS.themeVariables.primaryColor));
    // And the border must remain perceptible in absolute terms, not just
    // in parity, so a future palette tweak cannot quietly zero it out.
    expect(borderVsFill).toBeGreaterThan(1.3);
  });

  it("keeps labels above the 4.5:1 text floor", () => {
    expect(contrast(DARK_TOKENS.themeVariables.nodeTextColor, bg)).toBeGreaterThanOrEqual(TEXT_MIN);
    expect(contrast(DARK_TOKENS.themeVariables.primaryTextColor, bg)).toBeGreaterThanOrEqual(TEXT_MIN);
  });

  it("makes nodes distinguishable from the canvas", () => {
    // A node fill identical to the background would render as a
    // borderless ghost — blocks must read as filled surfaces.
    expect(DARK_TOKENS.themeVariables.primaryColor).not.toBe(bg);
    expect(contrast(DARK_TOKENS.themeVariables.primaryColor, bg)).toBeGreaterThan(1);
  });

  it("paints edge labels with the surface, not white", () => {
    // The white edge-label plate was the "some lines look white" report.
    expect(DARK_TOKENS.themeVariables.edgeLabelBackground).toBe(
      DARK_TOKENS.themeVariables.primaryColor,
    );
    expect(DARK_TOKENS.themeVariables.edgeLabelBackground).not.toBe("#ffffff");
  });

  it("overrides the rules themeVariables cannot reach", () => {
    const css = DARK_TOKENS.themeCSS;
    // .arrowheadPath is hardcoded fill:#000000 by mermaid.
    expect(css).toContain(".arrowheadPath");
    // .edgeLabel rect + .labelBkg paint the white plate.
    expect(css).toContain(".edgeLabel rect");
    expect(css).toContain(".labelBkg");
    // Cluster labels are hardcoded near-black.
    expect(css).toContain(".cluster-label text");
    // 1px hairlines vanish below one device pixel under fit scaling.
    expect(css).toContain(".edge-thickness-normal");
    expect(css).toContain("stroke-width: 1.5px");
  });

  it("never leaves a light-mode hex in dark CSS", () => {
    // Regression guard for the class of bug reported: a stray light color
    // surviving into dark mode. Scan every hex actually emitted.
    const LIGHT_ONLY = ["#ffffff", "#f8fafc", "#cbd5e1", "#94a3b8", "#334155", "#d1d5db"];
    const haystack = [DARK_TOKENS.themeCSS, JSON.stringify(DARK_TOKENS.themeVariables)].join("\n");
    const found = haystack.match(/#[0-9a-f]{6}/gi) ?? [];
    expect(found.length).toBeGreaterThan(0);
    for (const hex of found) {
      expect(LIGHT_ONLY).not.toContain(hex.toLowerCase());
    }
  });

  it("keeps light-mode hexes out of the dark palette", () => {
    for (const v of Object.values(DARK_TOKENS.themeVariables)) {
      expect(String(v).toLowerCase()).not.toBe("#ffffff");
    }
  });
});

describe("tokensFor", () => {
  it("returns dark tokens for an explicit dark preference", () => {
    expect(tokensFor("dark", "light")).toBe(DARK_TOKENS);
  });

  it("returns light tokens for an explicit light preference", () => {
    expect(tokensFor("light", "dark")).toBe(LIGHT_TOKENS);
  });

  it("resolves system against the OS theme", () => {
    // Must agree with applyTheme() in settingsStore, otherwise diagrams
    // desync from the .dark class on <html>.
    expect(tokensFor("system", "dark")).toBe(DARK_TOKENS);
    expect(tokensFor("system", "light")).toBe(LIGHT_TOKENS);
  });

  it("returns a stable reference so memo deps do not thrash", () => {
    // MermaidBlock memoizes on this; a fresh object each call would
    // re-render every diagram on every store update.
    expect(tokensFor("dark", "dark")).toBe(tokensFor("dark", "dark"));
  });
});

describe("isDarkTheme", () => {
  it.each([
    ["dark", "light", true],
    ["dark", "dark", true],
    ["light", "dark", false],
    ["light", "light", false],
    ["system", "dark", true],
    ["system", "light", false],
  ] as const)("theme=%s os=%s -> %s", (theme, os, expected) => {
    expect(isDarkTheme(theme, os)).toBe(expected);
  });

  it("agrees with tokensFor on every combination", () => {
    for (const theme of ["light", "dark", "system"] as const) {
      for (const os of ["light", "dark"] as const) {
        expect(isDarkTheme(theme, os)).toBe(tokensFor(theme, os) === DARK_TOKENS);
      }
    }
  });
});