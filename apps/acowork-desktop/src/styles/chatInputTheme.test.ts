import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { THEME_PACKS, TINTED_PACKS } from "./themePacks";

const __dirname = dirname(fileURLToPath(import.meta.url));
const THEMES_DIR = resolve(__dirname, "themes");

/** Lightness of an `hsl(...)` value — the trailing percentage. */
function lightness(value: string): number {
  return Number(/hsl\([^;]*?(\d+(?:\.\d+)?)%\s*\)/.exec(value)?.[1]);
}

function scopeValue(css: string, selector: string, token: string): string | undefined {
  const start = css.indexOf(selector);
  if (start < 0) return undefined;
  const end = css.indexOf("}", start);
  return css.slice(start, end).match(new RegExp(`^\\s*${token}:\\s*([^;]+);`, "m"))?.[1];
}

describe("chat input theme borders", () => {
  it("uses a dedicated, softer border token in every theme scope", () => {
    for (const pack of TINTED_PACKS) {
      const css = readFileSync(resolve(THEMES_DIR, `${pack.id}.css`), "utf8");
      for (const scope of ["light", "dark"] as const) {
        const selector = scope === "light" ? `:root.theme-${pack.id} {` : `:root.theme-${pack.id}.dark {`;
        const value = scopeValue(css, selector, "--color-chat-input-border");
        expect(value, `${pack.id} ${scope} chat input border`).toBeDefined();
        if (scope === "light") {
          expect(value, `${pack.id} light border must be softer than the outer frame`).not.toBe("var(--color-border-outer)");
          expect(value).toContain("0.55) 85%");
        }
      }
    }
  });
});

// The composer is two layers: the raised write surface (`-top`) and the frame
// strip that carries model / reasoning / workspace (`-bg`). The whole point is
// the step between them — a pack that copies the strip's value (or inverts it,
// the way the packs once inverted the panel ladder) turns the raised card into
// an invisible one, which no jsdom test can see.
describe("chat input top layer", () => {
  it("is lighter than the strip beneath it in every theme, light and dark", () => {
    for (const pack of THEME_PACKS) {
      const css = readFileSync(resolve(THEMES_DIR, `${pack.id}.css`), "utf8");
      for (const scope of ["light", "dark"] as const) {
        const selector = scope === "light" ? `:root.theme-${pack.id} {` : `:root.theme-${pack.id}.dark {`;
        const top = scopeValue(css, selector, "--color-chat-input-top");
        const base = scopeValue(css, selector, "--color-chat-input-bg");
        expect(top, `${pack.id} ${scope} chat input top`).toBeDefined();
        expect(base, `${pack.id} ${scope} chat input bg`).toBeDefined();
        expect(lightness(top!), `${pack.id} ${scope} top vs strip`).toBeGreaterThan(lightness(base!));
      }
    }
  });
});
