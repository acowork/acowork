/**
 * The search row's control layout — the contract behind the last round of
 * "these icons are unreadable / too loud / unevenly spaced" fixes.
 *
 * What this pins:
 *   1. All four icon controls — the mode radio pair and the two ripgrep
 *      flags — sit in ONE cluster with ONE gap. They used to be two
 *      groups, and the seam between them showed up as an uneven gap.
 *   2. Every one of them is the SAME BOX (`h-6 w-6`), so no icon in the
 *      row is bigger or smaller than its neighbour.
 *   3. The flags are DISABLED (greyed) in file mode, not hidden: a fuzzy
 *      file-name search has no case/word boundary to toggle, so a
 *      live-looking switch would be lying.
 *   4. Every icon in the row carries the same stroke weight. Lucide's
 *      default 2 on a 16px glyph lands at ~1.3px of stroke and reads
 *      heavier than the 12px icons beside it; 1.5 is ~1px, which matches.
 *
 * Why source scanning instead of rendered DOM:
 *   The properties above are pure class-string and JSX-shape contracts, and
 *   the file's imports pull in the whole app shell (stores, the gateway
 *   client). jsdom would compute no layout anyway. Same approach as
 *   workspaceSearchRow.test.ts.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SRC = readFileSync(resolve(__dirname, "WorkspaceExplorer.tsx"), "utf8");

/** The search row: from the magnifier to the clear button. */
function searchRow(): string {
  const start = SRC.indexOf("min-h-[2.5rem]");
  expect(start, "search row not found").toBeGreaterThan(-1);
  const end = SRC.indexOf("ariaLabelClearSearch", start);
  expect(end, "search row end not found").toBeGreaterThan(start);
  // The clear button's icon sits just past its own title attribute.
  return SRC.slice(start, end + 400);
}

/** The shared class every icon button in the row is built from. */
function searchToggleClassBody(): string {
  const start = SRC.indexOf("function searchToggleClass");
  expect(start, "searchToggleClass not found").toBeGreaterThan(-1);
  const end = SRC.indexOf("\n}", start);
  return SRC.slice(start, end);
}

describe("workspace search row controls", () => {
  it("all four icon controls sit in one cluster, one pixel apart", () => {
    const row = searchRow();
    // One wrapper carries the mode pair AND the flags, so the gap between
    // the second mode icon and the first flag is the same gap as between
    // the two mode icons.
    expect(row).toMatch(/<div className="flex shrink-0 items-center gap-px">/);
    // The radiogroup inside it repeats the same gap rather than letting the
    // cluster's gap apply twice (which would read as a wider seam).
    expect(row).toMatch(/className="flex items-center gap-px"/);
  });

  it("every icon button in the row is the same box", () => {
    const body = searchToggleClassBody();
    expect(body).toMatch(/h-6 w-6/);
    // One class for all of them — no per-call-site sizing.
    const row = searchRow();
    expect(row).not.toMatch(/className="[^"]*h-5 w-5/);
    expect(row).not.toMatch(/className="[^"]*h-7 w-7/);
  });

  it("the mode pair is plain icon buttons, not a segmented control", () => {
    const row = searchRow();
    // No sunken track / lifted segment around the two mode buttons.
    expect(row).not.toMatch(/bg-panel-block/);
    expect(row).not.toMatch(/ring-border-outer/);
    // The shared toggle class carries the selected chip instead.
    expect(row).toMatch(/searchToggleClass\(searchMode === mode\)/);
  });

  it("the ripgrep flags are inline and disabled in file mode", () => {
    const row = searchRow();
    expect(row).toMatch(/CaseSensitive/);
    expect(row).toMatch(/WholeWord/);
    expect(row).toMatch(/disabled=\{searchMode !== "content"\}/);
    // Disabled means greyed, not merely unfocusable — and a flag that is ON
    // but not applicable must not keep its accent chip.
    expect(searchToggleClassBody()).toMatch(/text-text-disabled/);
  });

  it("every icon in the row draws at the same stroke weight", () => {
    const row = searchRow();
    const icons = row.match(/<[A-Z]\w*\s+className="[^"]*"[^>]*\/>/g) ?? [];
    expect(icons.length).toBeGreaterThan(0);
    for (const icon of icons) {
      expect(icon, `icon without an explicit strokeWidth: ${icon}`).toMatch(
        /strokeWidth=\{1\.5\}/,
      );
    }
  });
});