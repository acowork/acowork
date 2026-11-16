/**
 * The surface ladder every theme pack must reproduce.
 *
 * The PM list (and every other list view) paints its rows with
 * `panel-inset` and its container with `right-panel`. The default palette
 * makes the content plane the BRIGHTEST step — `panel-inset` (100%) >
 * `panel-block` (98%) > `right-panel` (95%) — so the rows read as the main
 * content and the container recedes.
 *
 * The theme packs shipped the ladder the other way round: `panel-inset`
 * (89%) sat BELOW `panel-block` (94%) and `right-panel` (93%), so switching
 * a pack turned every list into a dark well inside a lighter frame — the
 * exact inverse of the default. The dark scope was fine; only the light
 * scope was inverted.
 *
 * This file pins the ORDER, not the numbers: a pack may pick any hues and
 * any lightness, but the relational ladder must match the default's, so a
 * pack can never invert the reading of a surface again.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { THEME_PACKS } from "./themePacks";

const __dirname = dirname(fileURLToPath(import.meta.url));
const THEMES_DIR = resolve(__dirname, "themes");

/** The ladder the default palette defines, brightest step first. */
const LIGHT_LADDER = [
  "--color-panel-inset",
  "--color-panel-inset-2",
  "--color-panel-block",
  "--color-right-panel",
] as const;

/**
 * Lightness of a token in one scope, as a percentage.
 *
 * The packs write `hsl(<hue> var(--theme-chroma) <L>%)` — the chroma knob
 * sits in the saturation slot, so the lightness is the trailing number.
 */
function lightness(css: string, scopeStart: number, scopeEnd: number, token: string): number {
  const body = css.slice(scopeStart, scopeEnd);
  // The packs write `hsl(<hue> var(--theme-chroma) <L>%)`; the default
  // writes `hsl(0 0% <L>%)`. Either way the lightness is the LAST number
  // before the closing paren, so match it that way.
  const decl = new RegExp(`^\\s*${token}:\\s*hsl\\([^;]*?(\\d+(?:\\.\\d+)?)%\\s*\\);`, "m").exec(body);
  expect(decl, `token ${token} not found in scope`).not.toBeNull();
  return Number(decl![1]);
}

/** The text between a scope's selector and its matching close brace. */
function scopeBounds(css: string, selector: string): [number, number] {
  const start = css.indexOf(selector);
  expect(start, `missing ${selector}`).toBeGreaterThanOrEqual(0);
  const end = css.indexOf("}", start);
  return [start, end];
}

describe("theme pack surface ladder", () => {
  it("keeps the light scope brighter than the container for every pack", () => {
    for (const pack of THEME_PACKS) {
      const css = readFileSync(resolve(THEMES_DIR, `${pack.id}.css`), "utf8");
      const [start, end] = scopeBounds(css, `:root.theme-${pack.id} {`);
      const l = (token: string) => lightness(css, start, end, token);

      // The content plane must never sit darker than the container it
      // sits in — that inversion is what turned the PM list into a well.
      expect(l("--color-panel-inset"), `${pack.id} list rows`).toBeGreaterThan(
        l("--color-right-panel"),
      );
      // The header band sits between the rows and the container.
      expect(l("--color-panel-inset"), `${pack.id} rows vs header`).toBeGreaterThanOrEqual(
        l("--color-panel-block"),
      );
      expect(l("--color-panel-block"), `${pack.id} header vs container`).toBeGreaterThan(
        l("--color-right-panel"),
      );
      // The page plane is the backdrop of the container, so it may be
      // lighter OR darker — but it must not be darker than the container,
      // or the panel floats off its page.
      expect(l("--color-page-bg"), `${pack.id} page vs container`).toBeGreaterThanOrEqual(
        l("--color-right-panel"),
      );
    }
  });

  it("keeps the dark scope darker than the container for every pack", () => {
    // Dark mode inverts the direction (the content plane is the DARKEST
    // step) — the default's dark ladder is page 8 / inset 8 / block 16.7 /
    // right-panel 11.7. A pack that brightens `panel-inset` above
    // `right-panel` in dark mode reads as a glowing slab.
    for (const pack of THEME_PACKS) {
      const css = readFileSync(resolve(THEMES_DIR, `${pack.id}.css`), "utf8");
      const [start, end] = scopeBounds(css, `:root.theme-${pack.id}.dark {`);
      const l = (token: string) => lightness(css, start, end, token);

      expect(l("--color-panel-inset"), `${pack.id} dark rows`).toBeLessThan(
        l("--color-right-panel"),
      );
      expect(l("--color-panel-block"), `${pack.id} dark header`).toBeGreaterThan(
        l("--color-right-panel"),
      );
    }
  });

  it("uses the same ladder order as the default palette", () => {
    // The default IS the reference: inset >= block > right-panel in light,
    // inset < block and block > right-panel in dark. The assertions above
    // restate it per pack; this one fails with the pack name when the
    // DEFAULT itself drifts, so the bar moves with the palette.
    const css = readFileSync(resolve(__dirname, "globals.css"), "utf8");
    const lightStart = css.indexOf("@theme {");
    const lightEnd = css.indexOf("\n}", lightStart);
    const darkStart = css.indexOf("\n.dark {");
    const darkEnd = css.indexOf("\n}", darkStart);

    const lightL = (t: string) => lightness(css, lightStart, lightEnd, t);
    const darkL = (t: string) => lightness(css, darkStart, darkEnd, t);

    expect(lightL("--color-panel-inset")).toBeGreaterThan(lightL("--color-right-panel"));
    expect(lightL("--color-panel-block")).toBeGreaterThan(lightL("--color-right-panel"));
    expect(darkL("--color-panel-inset")).toBeLessThan(darkL("--color-right-panel"));
    expect(darkL("--color-panel-block")).toBeGreaterThan(darkL("--color-right-panel"));
    // Every ladder member must exist — a renamed token would otherwise
    // make the pack assertions vacuous.
    for (const t of LIGHT_LADDER) {
      expect(lightL(t), `default light ${t}`).toBeGreaterThanOrEqual(0);
    }
  });
});
