/**
 * The assistant bubble must READ as a bubble against the chat plane.
 *
 * The bug this file exists for: the theme packs set `--color-chat-body`
 * (the chat plane) but never `--color-chat-bubble`, so the bubble kept
 * the default palette's translucent near-white `hsl(240 4.8% 95.9% / 0.3)`.
 * Composited over a pack's tinted plane that colour lands ~1 unit away
 * from the plane itself (contrast 1.00–1.01) — the bubble was literally
 * invisible in every non-default pack, in light mode only.
 *
 * Two rules are pinned here, one per scope:
 *
 *   1. LIGHT — the bubble is a WHITE card on the pack's tinted plane.
 *      The packs' plane sits at L94–95% (visibly grey), so the bubble has
 *      to be the brighter step or the two merge.
 *   2. DARK — the bubble keeps the default's translucent character, in the
 *      pack's own hue. It must be declared in the DARK scope explicitly:
 *      the pack's light scope (`:root.theme-<id>`) still matches a dark
 *      root, so a token only the light scope sets would leak its white
 *      value into dark mode.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { TINTED_PACKS } from "./themePacks";

const __dirname = dirname(fileURLToPath(import.meta.url));
const THEMES_DIR = resolve(__dirname, "themes");

/** `--theme-chroma` — the global tint-strength knob, 8% in both scopes. */
const CHROMA = 8;

interface Hsl {
  h: number;
  s: number;
  l: number;
  a: number;
}

/**
 * Parse the packs' colour syntax: `hsl(<hue> var(--theme-chroma) <L>%)`,
 * the same with an alpha (`... <L>% / 0.3`), and the scaled form
 * (`calc(var(--theme-chroma) * 0.85)`).
 */
function parseHsl(value: string): Hsl | null {
  const m = /hsl\(\s*(\d+)\s+(?:calc\(var\(--theme-chroma\)\s*\*\s*([\d.]+)\)|var\(--theme-chroma\))\s+([\d.]+)%\s*(?:\/\s*([\d.]+)\s*)?\)/.exec(
    value,
  );
  if (!m) return null;
  return {
    h: Number(m[1]),
    s: CHROMA * (m[2] ? Number(m[2]) : 1),
    l: Number(m[3]),
    a: m[4] ? Number(m[4]) : 1,
  };
}

/** The text between a scope's selector and its matching close brace. */
function scopeBody(css: string, selector: string): string {
  const start = css.indexOf(selector);
  expect(start, `missing ${selector}`).toBeGreaterThanOrEqual(0);
  return css.slice(start, css.indexOf("}", start));
}

/** One token's raw value inside a scope. */
function token(scope: string, name: string): string {
  const m = new RegExp(`^\\s*${name}:\\s*([^;]+);`, "m").exec(scope);
  expect(m, `token ${name} not found`).not.toBeNull();
  return m![1].trim();
}

function rgb({ h, s, l }: Hsl): [number, number, number] {
  const c = (1 - Math.abs((2 * l) / 100 - 1)) * (s / 100);
  const x = c * (1 - Math.abs(((h / 60) % 2) - 1));
  const m = l / 100 - c / 2;
  const [r, g, b] =
    h < 60
      ? [c, x, 0]
      : h < 120
        ? [x, c, 0]
        : h < 180
          ? [0, c, x]
          : h < 240
            ? [0, x, c]
            : h < 300
              ? [x, 0, c]
              : [c, 0, x];
  return [
    Math.round((r + m) * 255),
    Math.round((g + m) * 255),
    Math.round((b + m) * 255),
  ];
}

/** Composite a translucent colour over an opaque backdrop. */
function over(fg: Hsl, bg: [number, number, number]): [number, number, number] {
  const c = rgb(fg);
  return [
    Math.round(c[0] * fg.a + bg[0] * (1 - fg.a)),
    Math.round(c[1] * fg.a + bg[1] * (1 - fg.a)),
    Math.round(c[2] * fg.a + bg[2] * (1 - fg.a)),
  ];
}

/** WCAG relative luminance. */
function luminance([r, g, b]: [number, number, number]): number {
  const f = (v: number) => {
    const s = v / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b);
}

/** WCAG contrast ratio between two opaque colours. */
function contrast(a: [number, number, number], b: [number, number, number]): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

/** The bubble's painted colour over the chat plane, plus that plane. */
function bubbleOverPlane(scope: string): {
  bubble: [number, number, number];
  plane: [number, number, number];
} {
  const plane = parseHsl(token(scope, "--color-chat-body"))!;
  const bubble = parseHsl(token(scope, "--color-chat-bubble"))!;
  const planeRgb = rgb(plane);
  return { bubble: over(bubble, planeRgb), plane: planeRgb };
}

describe("theme pack chat bubble", () => {
  it("paints the light assistant bubble as a white card on the tinted plane", () => {
    for (const pack of TINTED_PACKS) {
      const css = readFileSync(resolve(THEMES_DIR, `${pack.id}.css`), "utf8");
      const scope = scopeBody(css, `:root.theme-${pack.id} {`);

      const bubble = parseHsl(token(scope, "--color-chat-bubble"))!;
      expect(bubble.l, `${pack.id} light bubble lightness`).toBeGreaterThanOrEqual(98);
      expect(bubble.a, `${pack.id} light bubble must be opaque`).toBe(1);

      const { bubble: painted, plane } = bubbleOverPlane(scope);
      // The plane is tinted (L94-95%), the bubble is white: they must be
      // far enough apart that the bubble reads as a card. The old
      // translucent default scored 1.00-1.01 here — the invisible bubble.
      expect(contrast(painted, plane), `${pack.id} light bubble vs plane`).toBeGreaterThan(1.08);
    }
  });

  it("gives the dark bubble its own translucent value in the pack's hue", () => {
    for (const pack of TINTED_PACKS) {
      const css = readFileSync(resolve(THEMES_DIR, `${pack.id}.css`), "utf8");
      const scope = scopeBody(css, `:root.theme-${pack.id}.dark {`);

      const bubble = parseHsl(token(scope, "--color-chat-bubble"))!;
      // Dark keeps the default palette's translucent character — a
      // translucent light step over the dark plane, NOT the light scope's
      // white card (the light scope still matches a dark root).
      expect(bubble.a, `${pack.id} dark bubble alpha`).toBeLessThan(1);
      expect(bubble.l, `${pack.id} dark bubble lightness`).toBeLessThan(50);

      const { bubble: painted, plane } = bubbleOverPlane(scope);
      expect(contrast(painted, plane), `${pack.id} dark bubble vs plane`).toBeGreaterThan(1.03);
    }
  });
});
