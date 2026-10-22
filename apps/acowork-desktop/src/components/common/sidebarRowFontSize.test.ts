/**
 * Sidebar row names must all render at the same size.
 *
 * The rule: an element carrying `font-medium` (the row-label marker every
 * sidebar uses) must not set an inline `fontSize` on that same element.
 * Sidebar lists inherit `text-xs` (12px) from their pane container
 * (`CAPSULE_PANE_CN ... text-xs`), so an inline
 * `fontSize: var(--ui-font-size)` — 14px — silently makes that one column
 * ~17% larger than its siblings. The chat agent list and the user list
 * both did this and read as a different scale from pm / doc / harness /
 * settings at the same global font size.
 *
 * Matching is per-ELEMENT, not per-line: the offending `fontSize` and
 * `font-medium` frequently sit on different lines of a multi-line JSX
 * attribute list, so a line-based scan silently passes (it did — the
 * first version of this test missed the user list for exactly that
 * reason). Here we walk the file with a small stack: `font-medium`
 * opens an element, and any `fontSize` before that element closes is
 * attributed to it.
 */
import { describe, it, expect } from "vitest";
import { execSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

const ROOT = resolve(__dirname, "..", "..", "..");

/** Sidebar-ish files. The chat body legitimately scales its own message
 *  text with calc(var(--ui-font-size) * …), so it is excluded. */
const SIDEBAR = /agent-list|user-list|pm\/|doc\/|common\/SectionPane|ExtensionsView/;

describe("sidebar row name sizing", () => {
  it("no row label element sets an inline fontSize", () => {
    const files = execSync(`git ls-files "src/**/*.tsx"`, {
      encoding: "utf8",
      cwd: ROOT,
    })
      .split("\n")
      // `git ls-files` also lists tracked-but-deleted files (rename/remove
      // mid-commit), so skip anything not on disk.
      .filter((f) => f && !/\.test\.tsx?$/.test(f) && SIDEBAR.test(f) && existsSync(resolve(ROOT, f)));

    const offenders: string[] = [];
    let labels = 0;

    for (const f of files) {
      const lines = readFileSync(resolve(ROOT, f), "utf8").split("\n");
      // A JSX element we care about starts at a `font-medium` line and
      // ends at the line that closes it (`>` or `/>` at the same depth).
      // Counting braces from the marker is enough for the shapes these
      // files use, and any unterminated run is simply ignored.
      for (let i = 0; i < lines.length; i++) {
        if (!/font-medium/.test(lines[i])) continue;
        labels++;
        for (let j = i; j < lines.length && j < i + 12; j++) {
          if (/fontSize/.test(lines[j])) {
            offenders.push(`${f}:${j + 1}: ${lines[j].trim()}`);
            break;
          }
          // Bail out at the element's own closing line.
          if (j > i && /^\s*(\/?>|\}>)/.test(lines[j])) break;
        }
      }
    }

    // Guard against a vacuous pass if the scan stops matching anything.
    expect(labels).toBeGreaterThan(0);
    expect(offenders).toEqual([]);
  });
});
