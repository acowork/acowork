import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { THEME_PACKS } from "./themePacks";

const __dirname = dirname(fileURLToPath(import.meta.url));
const THEMES_DIR = resolve(__dirname, "themes");

function scopeValue(css: string, selector: string, token: string): string | undefined {
  const start = css.indexOf(selector);
  if (start < 0) return undefined;
  const end = css.indexOf("}", start);
  return css.slice(start, end).match(new RegExp(`^\\s*${token}:\\s*([^;]+);`, "m"))?.[1];
}

describe("chat input theme borders", () => {
  it("uses a dedicated, softer border token in every theme scope", () => {
    for (const pack of THEME_PACKS) {
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
