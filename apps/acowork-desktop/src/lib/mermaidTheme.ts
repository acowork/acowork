/**
 * Mermaid theme tokens — the single data model for diagram palettes.
 *
 * Why this exists: mermaid bakes its palette into the SVG at render time
 * (`mermaid.initialize({ themeVariables })`), so a diagram rendered under a
 * light palette stays light forever — there is no CSS var to re-point at
 * runtime. The only correct fix is to render with the palette that matches
 * the active theme, so the palette must live in data, not in the renderer.
 *
 * Consumers pass `tokensFor(...)` into `renderMermaid(chart, tokens)`
 * (see `mermaid.ts`); nothing else may call `mermaid.initialize` directly.
 *
 * LIGHT_TOKENS is a verbatim transcription of the palette that was
 * previously hardcoded inside `ensureInit()`. It is asserted byte-for-byte
 * in `mermaidTheme.test.ts` so light mode cannot regress.
 *
 * Dark values align with the app's zinc surface ramp and
 * `--color-chat-body` (dark) = #131316, so a diagram sits on the same
 * plane as the chat surface around it instead of punching a white hole.
 */

/** Mermaid's accepted theme names, narrowed to what we use. */
export type MermaidThemeName = "base";

/** Subset of mermaid config we need to vary per theme. Structural —
 *  kept open so `mermaid.initialize` accepts it directly.
 *  Values are `string | number` because mermaid types themeVariables as
 *  `any` and some entries (e.g. `nodeBorderRadius`) are numeric. */
export interface MermaidTokens {
  theme: MermaidThemeName;
  themeVariables: Record<string, string | number>;
  /** Extra CSS appended after the generated theme CSS. Used to override
   *  rules mermaid hardcodes outside `themeVariables` (arrowheads, edge
   *  label backgrounds, cluster labels). */
  themeCSS: string;
}

/** Font stack shared by both themes — typography is not a theming concern. */
const FONT_FAMILY =
  '-apple-system, BlinkMacSystemFont, "Inter", "Segoe UI", "Noto Sans SC", "Microsoft YaHei", system-ui, sans-serif';

const FONT_SIZE = "12px";

/** Line thickness is hardcoded by mermaid (`.edge-thickness-normal{stroke-width:1px}`)
 *  and is not a theme variable. In dark mode a 1px hairline at fit-scale
 *  < 1 (MermaidBlock caps scaling at FIT_MAX = 1, so wide diagrams shrink)
 *  drops below one device pixel and anti-aliases into the background.
 *  Bumping to 1.5px is the cheapest way to keep edges legible. */
const DARK_EDGE_THICKNESS_CSS = [
  ".edge-thickness-normal { stroke-width: 1.5px; }",
  ".edge-thickness-thick { stroke-width: 4px; }",
].join("\n");

/** Raw dark palette values. Declared before `DARK_OVERRIDE_CSS`, which
 *  interpolates them — a forward reference here is a TDZ error at module
 *  load, not a build error, so ordering is load-bearing. */
const DARK = {
  background: "#131316",   // = --color-chat-body (dark), zinc-950
  surface: "#1e1f24",     // node fill — one step above the canvas
  surfaceMuted: "#27272a", // subgraph fill (zinc-800)
  border: "#3f3f46",       // node border (zinc-700)
  borderMuted: "#52525b",  // subgraph border (zinc-600)
  line: "#a1a1aa",         // edges/arrows (zinc-400) — 7.5:1 on #131316
  text: "#e4e4e7",         // labels (zinc-200) — 13.9:1 on #131316
  textMuted: "#a1a1aa",    // secondary labels (zinc-400)
  title: "#e4e4e7",
  criticalSurface: "#450a0a",
  criticalText: "#fecaca",
  arrow: "#d4d4d8",        // arrowheads — brighter than the edge they cap,
                            // so the head still reads at fit-scale < 1
};

/**
 * Rules that mermaid's `themeVariables` does NOT reach, so they must be
 * overridden explicitly or they stay light in dark mode:
 *  - `.arrowheadPath` is hardcoded `fill:#000000`
 *  - `.edgeLabel rect` / `.labelBkg` paint a white plate behind edge
 *    labels, which is what makes label-bearing edges read as a white line
 *  - `.cluster-label text|span` is hardcoded near-black
 *  - `.root .anchor path` pins the anchor stroke to `lineColor!important`
 */
const DARK_OVERRIDE_CSS = [
  `.arrowheadPath { fill: ${DARK.arrow} !important; }`,
  `.marker { fill: ${DARK.line} !important; stroke: ${DARK.line} !important; }`,
  `.marker.cross { stroke: ${DARK.line} !important; }`,
  `.marker path, .marker polygon, .marker circle { fill: ${DARK.line} !important; stroke: ${DARK.line} !important; }`,
  `.edgeLabel, .edgeLabel p { background-color: ${DARK.surface} !important; color: ${DARK.text} !important; }`,
  `.edgeLabel rect { opacity: 1 !important; fill: ${DARK.surface} !important; stroke: none !important; }`,
  `.labelBkg { background-color: ${DARK.surface} !important; }`,
  `.cluster-label text, .cluster-label span { fill: ${DARK.text} !important; color: ${DARK.text} !important; }`,
  `.error-icon { fill: ${DARK.criticalSurface} !important; }`,
  `.error-text { fill: ${DARK.criticalText} !important; stroke: ${DARK.criticalText} !important; }`,
  `.root .anchor path { fill: ${DARK.line} !important; stroke: ${DARK.line} !important; }`,
  DARK_EDGE_THICKNESS_CSS,
].join("\n");

/** Light palette — transcribed from the original hardcoded config.
 *  DO NOT restyle: `mermaidTheme.test.ts` pins these values so light mode
 *  stays byte-identical to the pre-theme-split behaviour. */
const LIGHT = {
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
  titleColor: "#334155",
  errorBkgColor: "#fdfaf5",
  errorTextColor: "#02050a",
  labelBackground: "#ffffff",
  fontSize: FONT_SIZE,
};

/** Rounded-corner overrides for nodes + depth-shifted subgraph fills.
 *  Shared by both themes: corner radius is not a contrast concern, and the
 *  nesting tint reads as hierarchy in either mode. */
const SHAPE_CSS = [
  ".node.default > rect,",
  ".node.default > .label-container,",
  ".node > rect,",
  ".node > .label-container {",
  "  rx: 12px !important;",
  "  ry: 12px !important;",
  "}",
  ".label-container {",
  "  border-radius: 12px !important;",
  "}",
].join("\n");

/** Subgraph fills — light uses pastel tints, dark uses the zinc ramp. */
function subgraphCss(fill: string, stroke: string, depth: number): string {
  const sel = ".cluster > g > ".repeat(depth) + ".node.default > rect," +
    ".cluster > g > ".repeat(depth) + ".node > rect";
  return `${sel} { fill: ${fill} !important; stroke: ${stroke} !important; }`;
}

export const LIGHT_TOKENS: MermaidTokens = {
  theme: "base",
  themeVariables: {
    background: LIGHT.background,
    primaryColor: LIGHT.primaryColor,
    primaryBorderColor: LIGHT.primaryBorderColor,
    primaryTextColor: LIGHT.primaryTextColor,
    lineColor: LIGHT.lineColor,
    secondaryColor: LIGHT.secondaryColor,
    tertiaryColor: LIGHT.tertiaryColor,
    clusterBkg: LIGHT.clusterBkg,
    clusterBorder: LIGHT.clusterBorder,
    edgeLabelBackground: LIGHT.edgeLabelBackground,
    nodeBorder: LIGHT.nodeBorder,
    nodeTextColor: LIGHT.nodeTextColor,
    // NOTE: `titleColor` is deliberately NOT set. The pre-refactor config
    // omitted it, so mermaid fell back to its own default (#02050a) for
    // `.cluster-label text/span`. Setting it to primaryTextColor looked
    // equivalent but shifted those four rules from #02050a to #334155 —
    // a real, if subtle, light-mode change. Omitting it keeps light output
    // byte-identical; dark sets it explicitly below because #02050a is
    // invisible on a dark canvas.
    errorBkgColor: LIGHT.errorBkgColor,
    errorTextColor: LIGHT.errorTextColor,
    labelBackground: LIGHT.labelBackground,
    fontSize: LIGHT.fontSize,
    fontFamily: FONT_FAMILY,
    nodeBorderRadius: 12,
  },
  // Node fills stay light-mode pastels: light mode has no contrast problem
  // and changing them would be a visual regression.
  // ORDER MATTERS — mermaid concatenates themeCSS verbatim, so this list
  // reproduces the original rule order exactly. Reordering produces a
  // different (if visually equivalent) stylesheet; the byte-equality test
  // in `mermaidTheme.verify.test.ts` is what pins it.
  themeCSS: [
    ".node.default > rect,",
    ".node.default > .label-container,",
    ".node > rect,",
    ".node > .label-container {",
    "  rx: 12px !important;",
    "  ry: 12px !important;",
    "}",
    ".node.default > rect,",
    ".node > rect {",
    `  fill: ${LIGHT.primaryColor} !important;`,
    `  stroke: ${LIGHT.primaryBorderColor} !important;`,
    "}",
    subgraphCss("#f0fdf5", "#a7c2b4", 1),
    subgraphCss("#fdfaf5", "#c4b8a8", 2),
    subgraphCss("#f8f6fc", "#bdb8c8", 3),
    ".label-container {",
    "  border-radius: 12px !important;",
    "}",
  ].join("\n"),
};

export const DARK_TOKENS: MermaidTokens = {
  theme: "base",
  themeVariables: {
    background: DARK.background,
    primaryColor: DARK.surface,
    primaryBorderColor: DARK.border,
    primaryTextColor: DARK.text,
    lineColor: DARK.line,
    secondaryColor: DARK.surfaceMuted,
    tertiaryColor: DARK.surfaceMuted,
    clusterBkg: DARK.surfaceMuted,
    clusterBorder: DARK.borderMuted,
    edgeLabelBackground: DARK.surface,
    nodeBorder: DARK.border,
    nodeTextColor: DARK.text,
    titleColor: DARK.title,
    errorBkgColor: DARK.criticalSurface,
    errorTextColor: DARK.criticalText,
    labelBackground: DARK.surface,
    fontSize: FONT_SIZE,
    fontFamily: FONT_FAMILY,
    nodeBorderRadius: 12,
  },
  themeCSS: [
    // Node fill/stroke pinned explicitly: mermaid's generated
    // `.node rect{...}` uses primaryColor/nodeBorder, but the neo-look and
    // cluster-nesting selectors both override it, so we restate depth by depth.
    ".node.default > rect,",
    ".node > rect {",
    `  fill: ${DARK.surface} !important;`,
    `  stroke: ${DARK.border} !important;`,
    "}",
    ".node.default > .label-container,",
    ".node > .label-container {",
    `  fill: ${DARK.surface} !important;`,
    "}",
    subgraphCss(DARK.surfaceMuted, DARK.borderMuted, 1),
    subgraphCss("#1c1c1f", "#45454d", 2),
    subgraphCss("#18181b", "#3f3f46", 3),
    SHAPE_CSS,
    DARK_OVERRIDE_CSS,
  ].join("\n"),
};

/** The app's theme preference, mirroring `settingsStore`'s `Theme`. */
export type ThemePreference = "light" | "dark" | "system";

/**
 * Resolve the effective theme and return its tokens.
 *
 * `system` resolves against `osTheme`, matching `applyTheme()` in
 * settingsStore and the `isDark` derivation used by SetiIcon/NavBar/
 * AppLayout — all three must agree or diagrams desync from the DOM's
 * `.dark` class.
 */
export function tokensFor(theme: ThemePreference, osTheme: "light" | "dark"): MermaidTokens {
  const effective = theme === "system" ? osTheme : theme;
  return effective === "dark" ? DARK_TOKENS : LIGHT_TOKENS;
}

/** Effective-theme boolean, for callers that only need the predicate. */
export function isDarkTheme(theme: ThemePreference, osTheme: "light" | "dark"): boolean {
  return (theme === "system" ? osTheme : theme) === "dark";
}