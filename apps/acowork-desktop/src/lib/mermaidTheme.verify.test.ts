/**
 * End-to-end proof that theming did not disturb light mode.
 *
 * `mermaidTheme.test.ts` pins the token VALUES. That is necessary but not
 * sufficient: mermaid derives extra rules from those values (e.g.
 * `.cluster-label text` keys off `titleColor`), and concatenates `themeCSS`
 * verbatim. So a token-level green build can still emit a different
 * stylesheet. This file renders the same chart twice — once with the
 * pre-refactor config, captured in `__fixtures__/mermaid-legacy-light.json`
 * straight out of git history — and once with `LIGHT_TOKENS`, then
 * compares the generated `<style>` block byte for byte.
 *
 * Both failures it catches were real during this change:
 *   - `titleColor` added to LIGHT_TOKENS flipped 4 rules #02050a -> #334155
 *   - moving `SHAPE_CSS` to the end re-ordered the sheet (same rules,
 *     different cascade order)
 *
 * Renders through the real mermaid, so it also fails loudly if an upgrade
 * changes generated CSS — the fixture then needs an intentional re-bless.
 */
import { it, expect, beforeAll } from "vitest";
import { JSDOM } from "jsdom";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { LIGHT_TOKENS, DARK_TOKENS } from "./mermaidTheme";
import { renderMermaid } from "./mermaid";

const FIXTURE = resolve(process.cwd(), "src/lib/__fixtures__/mermaid-legacy-light.json");

/** jsdom has no SVG layout engine, so stub the measurement APIs mermaid
 *  needs to size nodes. Values are approximations — irrelevant here, since
 *  both renders use the same stubs and only the stylesheet is compared. */
function installDom() {
  const dom = new JSDOM("<!doctype html><html><body></body></html>", { pretendToBeVisual: true });
  const g = globalThis as unknown as Record<string, unknown>;
  const win = dom.window as unknown as Record<string, unknown>;
  for (const k of [
    "window", "document", "navigator", "Element", "SVGElement", "HTMLElement",
    "Node", "MutationObserver", "CSSStyleSheet", "getComputedStyle",
    "DocumentFragment", "NodeFilter", "XMLSerializer", "SVGGraphicsElement",
  ]) {
    if (win[k] !== undefined) g[k] = win[k];
  }
  g.requestAnimationFrame = (cb: (t: number) => void) => setTimeout(() => cb(0), 0);

  const approx = (el: Element) => {
    const lines = ((el && el.textContent) || "").split("\n");
    return { x: 0, y: 0, width: Math.max(1, ...lines.map((l) => l.length)) * 7.2, height: lines.length * 14.4 };
  };
  const svgProto = dom.window.SVGElement.prototype as unknown as Record<string, unknown>;
  svgProto.getBBox = function () { return approx(this as unknown as Element); };
  svgProto.getComputedTextLength = function () { return approx(this as unknown as Element); };
  svgProto.getScreenCTM = () => ({ a: 1, b: 0, c: 0, d: 1, e: 0, f: 0 });
  (dom.window.Element.prototype as unknown as Record<string, unknown>).getBoundingClientRect =
    function () {
      const b = approx(this as unknown as Element);
      return { ...b, top: b.y, left: b.x, right: b.x + b.width, bottom: b.y + b.height };
    };
}

/** Exercises edges (solid/dotted/labeled), a decision node, nested
 *  subgraphs at two depths, and a second root — i.e. every rule family the
 *  palette is supposed to influence. */
const CHART = [
  "flowchart TD",
  "  A[Input] --> B[Process]",
  "  B --> C[Output]",
  "  B -- yes --> D{Choice}",
  "  D -->|a| E[Alt]",
  "  subgraph S1[Group]",
  "    F[Inner] --> G[Inner2]",
  "  end",
  "  subgraph S2[Outer]",
  "    H[H1] --> I[I1]",
  "  end",
  "  A -.-> C",
].join("\n");

/** Pull out the generated stylesheet, normalising the svg id (differs per
 *  render by design, carries no theme information). */
function cssOf(svg: string, id: string): string {
  return svg
    .slice(svg.indexOf("<style>") + 7, svg.indexOf("</style>"))
    .split(id)
    .join("ID");
}

let before: string;
let after: string;

beforeAll(async () => {
  installDom();
  const mermaid = (await import("mermaid")).default;
  const legacy = JSON.parse(readFileSync(FIXTURE, "utf8")) as {
    themeVariables: Record<string, unknown>;
    themeCSS: string[];
  };

  const draw = async (id: string, tokens: { themeVariables: Record<string, unknown>; themeCSS: string }) => {
    mermaid.initialize({
      startOnLoad: false,
      theme: "base",
      themeVariables: tokens.themeVariables,
      themeCSS: tokens.themeCSS,
      flowchart: { useMaxWidth: false, htmlLabels: true, curve: "basis", padding: 6, nodeSpacing: 35, rankSpacing: 35 },
      sequence: { useMaxWidth: false, showSequenceNumbers: false },
    });
    return (await mermaid.render(id, CHART)).svg;
  };

  // Reassemble the legacy config exactly as the old ensureInit() had it.
  before = cssOf(
    await draw("legacy-light", {
      // nodeBorderRadius was passed by the old code even though mermaid
      // ignores it (absent from its entire dist) — kept for fidelity.
      themeVariables: { ...legacy.themeVariables, nodeBorderRadius: 12 },
      themeCSS: legacy.themeCSS.join("\n"),
    }),
    "legacy-light",
  );
  after = cssOf(await draw("themed-light", LIGHT_TOKENS), "themed-light");
}, 60_000);

it("serializes concurrent requests with different palettes", async () => {
  // All components share this module's singleton Mermaid instance. Start
  // opposing palettes together repeatedly; each SVG must retain the tokens
  // passed to its call, regardless of which request entered first.
  const requests = Array.from({ length: 8 }, (_, index) => {
    const useDark = index % 2 === 1;
    const id = useDark ? "#a1a1aa" : "#94a3b8";
    const tokens = useDark ? DARK_TOKENS : LIGHT_TOKENS;
    return renderMermaid(["flowchart TD", " A-->B"].join("\n"), tokens).then((svg) => ({ svg, id }));
  });

  for (const { svg, id } of await Promise.all(requests)) {
    const styleStart = svg.indexOf("<style>");
    const styleEnd = svg.indexOf("</style>", styleStart);
    expect(styleStart).toBeGreaterThanOrEqual(0);
    expect(styleEnd).toBeGreaterThan(styleStart);
    expect(svg.slice(styleStart, styleEnd)).toContain(`.flowchart-link{stroke:${id}`);
  }
});

it("continues rendering after a failed request", async () => {
  // A rejection must not poison the FIFO tail or strand later callers.
  await expect(renderMermaid("this is not a mermaid diagram")).rejects.toThrow();
  const svg = await renderMermaid(["flowchart TD", " A-->B"].join("\n"), DARK_TOKENS);
  expect(svg).toContain(".flowchart-link{stroke:#a1a1aa");
});

it("emits an identical stylesheet for the pre-refactor light palette", () => {
  if (before !== after) {
    const b = before.split("}");
    const a = after.split("}");
    // Surface the offending rules; a bare 5 KB string diff is useless.
    console.log("ONLY IN LEGACY:", JSON.stringify(b.filter((x) => !a.includes(x)), null, 1));
    console.log("ONLY IN THEMED:", JSON.stringify(a.filter((x) => !b.includes(x)), null, 1));
  }
  expect(after).toBe(before);
});

it("produced a non-trivial stylesheet (guards against a vacuous pass)", () => {
  // If mermaid ever emits no <style>, both sides would be "" and the test
  // above would pass for the wrong reason.
  expect(before.length).toBeGreaterThan(1000);
  expect(after).toBe(before);
});
