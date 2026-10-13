/**
 * Every `var(--color-*)` the frontend names must be a token that actually
 * exists in [globals.css](./globals.css).
 *
 * Why this file exists:
 *   `NavButton`'s unread dot was `bg-[var(--color-danger)] ring-2
 *   ring-[var(--color-bg-primary)]` for its whole life. Neither token has ever
 *   been declared — the desktop palette calls them `--color-destructive` and
 *   `--color-nav-surface`. CSS resolves an undefined custom property WITHOUT a
 *   fallback to "invalid at computed-value time", which for
 *   `background-color` means the declared value is thrown away, so the dot
 *   rendered as nothing. Its own test passed the entire time because it
 *   asserted `data-testid="nav-badge"` exists in the DOM, and it did —
 *   invisibly.
 *
 *   That failure mode is invisible to jsdom (no cascade, no paint), invisible
 *   to `tsc` (it is a string), and invisible to Tailwind (it emits the
 *   declaration verbatim). The only cheap check left is the name itself.
 *
 * What it deliberately does NOT police:
 *   - `var(--x, fallback)` — a fallback renders, so a typo degrades to the
 *     fallback rather than to nothing. Still worth fixing, not a red line.
 *   - non-`--color-*` families (`--space-*`, `--ui-*`, `--text-*`): the
 *     component layer almost never names those by hand, and each has its own
 *     generator. Widen this list when one bites.
 *   - tokens declared by Tailwind itself. Anything Tailwind *emits* comes from
 *     the `@theme` block in globals.css, which this scan counts as declared.
 */
import { readFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve, join } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(__dirname, "..", "..");

/** Every custom property the stylesheet declares, at any nesting level. */
function declaredTokens(): Set<string> {
  const css = readFileSync(resolve(__dirname, "globals.css"), "utf8");
  return new Set([...css.matchAll(/(--[a-zA-Z0-9-]+)\s*:/g)].map((m) => m[1]));
}

/** `.tsx` / `.ts` / `.css` under src/, minus tests and fixtures. */
function sourceFiles(): string[] {
  const out: string[] = [];
  const walk = (dir: string) => {
    for (const entry of readdirSync(dir)) {
      const p = join(dir, entry);
      if (statSync(p).isDirectory()) walk(p);
      else if (/\.(tsx|ts|css)$/.test(entry) && !/\.(test|spec)\.(tsx|ts)$/.test(entry)) {
        out.push(p);
      }
    }
  };
  walk(resolve(ROOT, "src"));
  return out;
}

/**
 * `var(--color-*)` uses that have no token behind them and no fallback.
 * Comment bodies are stripped first: this file's own prose names
 * `--color-danger` and `--color-bg-primary` to explain them.
 */
function undefinedColorTokens(): Array<{ token: string; file: string }> {
  const declared = declaredTokens();
  const hits: Array<{ token: string; file: string }> = [];
  for (const file of sourceFiles()) {
    const rel = file.slice(ROOT.length + 1).replace(/\\/g, "/");
    const code = readFileSync(file, "utf8")
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .replace(/\/\/[^\n]*/g, "");
    for (const m of code.matchAll(/var\((--color-[a-zA-Z0-9-]+)(\s*,[^)]*)?\)/g)) {
      if (m[2]) continue; // has a fallback — degrades, does not vanish
      if (declared.has(m[1])) continue;
      hits.push({ token: m[1], file: rel });
    }
  }
  return hits;
}

describe("theme colour tokens resolve", () => {
  it("names no --color-* token that globals.css does not declare", () => {
    const hits = undefinedColorTokens();
    // Sorted `token @ file` list so a failure says which dot went invisible.
    expect(
      hits.map((h) => `${h.token}  <-  ${h.file}`).sort(),
    ).toEqual([]);
  });

  it("keeps the destructive red spelled `--color-destructive`", () => {
    // The specific typo this file was written for. Asserted separately from
    // the sweep so a regression names itself rather than appearing as one
    // line in a generic list.
    const declared = declaredTokens();
    expect(declared.has("--color-destructive")).toBe(true);
    expect(declared.has("--color-danger")).toBe(false);
  });
});
