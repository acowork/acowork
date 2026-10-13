import mermaid from "mermaid";
import { LIGHT_TOKENS, type MermaidTokens } from "./mermaidTheme";

/**
 * (Re-)initialize mermaid global config. Internal to this module: callers
 * must use `renderMermaid()` so initialization and rendering remain atomic
 * with respect to the shared Mermaid singleton.
 *
 * `tokens` carries the palette — it is a parameter, not a constant, because
 * mermaid bakes colors into the SVG at render time. Re-rendering with the
 * active theme's tokens is the only way a dark-mode diagram stops shipping
 * light-mode edges. See `mermaidTheme.ts` for the palettes themselves.
 *
 * Defaults to LIGHT_TOKENS so any caller that omits it keeps the historical
 * behavior rather than silently rendering an unstyled chart.
 */
function ensureInit(tokens: MermaidTokens = LIGHT_TOKENS) {
  mermaid.initialize({
    startOnLoad: false,
    theme: tokens.theme,
    themeVariables: tokens.themeVariables,
    themeCSS: tokens.themeCSS,
    // useMaxWidth: false → mermaid outputs SVG with explicit width/height
    // attrs (the viewBox pixel size), NOT width="100%" + max-width.
    // We need real pixel dimensions so we can measure the rendered
    // size reliably and compute the correct fit scale.
    flowchart: {
      useMaxWidth: false,
      htmlLabels: true,
      curve: "basis",
      padding: 6,
      nodeSpacing: 35,
      rankSpacing: 35,
    },
    sequence: {
      useMaxWidth: false,
      showSequenceNumbers: false,
    },
  });
}

/** Simple non-crypto hash for stable mermaid IDs. */
export function hashStr(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) {
    h = ((h << 5) - h + s.charCodeAt(i)) | 0;
  }
  return h;
}

export function isPlausibleMermaid(code: string): boolean {
  const trimmed = code.trim();
  if (!trimmed) return false;

  const lines = trimmed.split("\n");
  if (lines.length < 2) return false;

  const firstLine = lines[0].trim();
  const supported = [
    "flowchart", "graph", "sequenceDiagram", "classDiagram",
    "stateDiagram", "stateDiagram-v2", "erDiagram", "gantt",
    "pie", "gitGraph", "mindmap", "timeline", "quadrantChart",
    "xyChart", "block", "architecture", "kanban", "sankey", "xychart",
  ];
  if (!supported.some((t) => firstLine.startsWith(t))) return false;

  const lastNonEmpty = [...lines].reverse().find((l) => l.trim().length > 0);
  if (lastNonEmpty) {
    const endsWithPartial = /(?:-->|->|==>|=>|-\.->|--x|--o)$/.test(lastNonEmpty.trim());
    if (endsWithPartial) return false;
  }

  if (firstLine.startsWith("flowchart") || firstLine.startsWith("graph")) {
    let depth = 0;
    for (const line of lines) {
      const t = line.trim();
      if (/^subgraph\b/i.test(t)) depth++;
      if (/^end(\s|$)/.test(t)) depth--;
    }
    if (depth > 0) return false;
  } else if (
    firstLine.startsWith("stateDiagram") ||
    firstLine.startsWith("classDiagram")
  ) {
    let depth = 0;
    for (const line of lines) {
      for (const ch of line) {
        if (ch === "{") depth++;
        else if (ch === "}") depth--;
      }
    }
    if (depth > 0) return false;
  }

  return true;
}

let renderSeq = 0;
/** Mermaid's config and diagram parser/renderer are module-global singletons.
 *  Keep initialize + render in one FIFO critical section: changing the config
 *  between another render's initialize and config snapshot can bake the wrong
 *  palette into its SVG. */
let renderQueue: Promise<void> = Promise.resolve();

/** Render mermaid chart → SVG string. Throws on invalid diagram.
 *  IDs stay unique across queued requests; initialize + render are serialized
 *  because Mermaid owns one module-global config and renderer state.
 *
 *  Pass the active theme's tokens (`tokensFor(theme, osTheme)`) so the baked
 *  palette matches the UI. Omitting them falls back to LIGHT_TOKENS. */
export function renderMermaid(
  chart: string,
  tokens: MermaidTokens = LIGHT_TOKENS,
): Promise<string> {
  const render = renderQueue.then(async () => {
    ensureInit(tokens);
    renderSeq += 1;
    const { svg } = await mermaid.render(`md-${renderSeq}-${hashStr(chart)}`, chart);
    return svg;
  });

  // Recover the queue after either fulfillment or rejection; the caller still
  // receives `render` and observes the original error, but one invalid chart
  // must not permanently prevent every subsequent diagram from rendering.
  renderQueue = render.then(() => undefined, () => undefined);
  return render;
}
