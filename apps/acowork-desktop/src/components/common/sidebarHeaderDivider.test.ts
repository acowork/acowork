/**
 * Every sidebar that reserves the list-header band with
 * `--ui-list-header-h` must also draw a `border-b` on that band.
 *
 * Why a source scan instead of a render assertion: the consumers are five
 * unrelated components (SectionPane, DocTreeSidebar, ExtensionsView,
 * RightPanel, ProjectSidebar), each with its own store/mocks, and the
 * property is a single className on one element. Rendering all five just
 * to read a className is a lot of fixture for a one-line contract — and
 * the one case that actually regressed (pm, which had no `border-b`)
 * was invisible precisely because no test rendered that band.
 *
 * The contract: `min-h-[var(--ui-list-header-h)]` and `border-b` must
 * appear on the SAME line, since the band is a single element.
 */
import { describe, it, expect } from "vitest";
import { execSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { resolve } from "node:path";

const ROOT = resolve(__dirname, "..", "..", "..");

describe("sidebar list-header band", () => {
  it("draws a border-b on every --ui-list-header-h band", () => {
    const files = execSync(`git ls-files --cached --others --exclude-standard "src/**/*.tsx"`, {
      encoding: "utf8",
      cwd: ROOT,
    })
      .split("\n")
      .filter((f) => f && !/\.test\.tsx?$/.test(f))
      // `git ls-files` without --cached also lists index entries whose file
      // is gone from the working tree; a deleted-but-unstaged component
      // would then blow up the scan with ENOENT instead of a clean verdict.
      .filter((f) => existsSync(resolve(ROOT, f)));

    const missing: string[] = [];
    let checked = 0;

    for (const f of files) {
      const src = readFileSync(resolve(ROOT, f), "utf8");
      for (const line of src.split("\n")) {
        if (!line.includes("ui-list-header-h")) continue;
        // Prose mentions the token in comments far more often than
        // className does; only a line that actually carries a class
        // attribute can be the element we're styling.
        if (!line.includes("className")) continue;
        checked++;
        if (!line.includes("border-b")) {
          missing.push(`${f}: ${line.trim()}`);
        }
      }
    }

    // Sanity: the scan must actually be looking at the known consumers,
    // otherwise this test passes vacuously after a refactor.
    expect(checked).toBeGreaterThanOrEqual(5);
    expect(missing).toEqual([]);
  });
});
