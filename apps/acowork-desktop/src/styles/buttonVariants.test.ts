/**
 * The button surface vocabulary is closed: `btn-solid` and `btn-accent`.
 *
 * Why this file exists:
 *   These classes replaced hand-rolled `bg-zinc-200` / `border-zinc-300`
 *   button surfaces (globals.css "Button surface variants"), and the app
 *   settled on two tones — a neutral fill for secondary actions, the
 *   accent fill for the one primary/confirm action in a dialog. Two other
 *   variants had drifted in (`btn-outline`, `btn-option`); after the
 *   ProfileTab row switched to the standard trailing-action grammar they
 *   had zero callers, and their CSS was deleted.
 *
 *   The failure mode this pins is a third tone creeping back in at a new
 *   call site: nothing about `btn-outline` looked wrong in isolation, it
 *   just rendered a bordered ghost button next to a row of solid ones.
 *   That is a visual bug jsdom cannot see and no type error catches, so
 *   the spec is the class name itself.
 *
 * Why source scanning:
 *   Same reason as capsule.test.tsx / globalFontSize.test.ts — these are
 *   stylesheet and class-string contracts. Nothing renders them.
 *
 * What it deliberately does NOT police: which tone a given action picks.
 * Danger-red for destructive actions is a per-component decision
 * (AccountMenu's 注销, the delete buttons); "red is not a button variant"
 * is the rule, not "nothing may ever be red".
 */
import { readFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve, join } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(__dirname, "..", "..");
const css = readFileSync(resolve(__dirname, "globals.css"), "utf8");

/** The closed set. Adding a name here is the design decision — the test
 *  only exists to make sure the CSS and the call sites agree with it. */
const VARIANTS = ["btn-solid", "btn-accent"] as const;

/** Every `.tsx` under src/, minus tests. `py-[var(--ui-btn-py)]` contains
 *  `btn-py` as a substring but is not a variant, so match on word
 *  boundaries with a leading `-`/`\b` guard and an explicit trailing
 *  boundary — `btn-solidly` must not satisfy `btn-solid`. */
function sourceFiles(): string[] {
  const out: string[] = [];
  const walk = (dir: string) => {
    for (const entry of readdirSync(dir)) {
      const p = join(dir, entry);
      if (statSync(p).isDirectory()) {
        walk(p);
      } else if (entry.endsWith(".tsx") && !entry.endsWith(".test.tsx")) {
        out.push(p);
      }
    }
  };
  walk(resolve(ROOT, "src"));
  return out;
}

/** Every `btn-*` token that looks like a class usage rather than a CSS
 *  custom property (`--ui-btn-py`, `--ui-btn-compact-py`). */
function usedVariants(): Map<string, string[]> {
  const found = new Map<string, string[]>();
  for (const file of sourceFiles()) {
    const rel = file.slice(ROOT.length + 1).replace(/\\/g, "/");
    const src = readFileSync(file, "utf8");
    // Skip comment bodies so this file's own prose (and the ProfileTab
    // note explaining what btn-outline WAS) cannot pin itself as a caller.
    const code = src.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");
    for (const m of code.matchAll(/(?<![\w-])(btn-[a-z0-9]+(?:-[a-z0-9]+)*)/g)) {
      const list = found.get(m[1]) ?? [];
      list.push(rel);
      found.set(m[1], list);
    }
  }
  return found;
}

describe("button surface variants", () => {
  it("every btn-* class used in src/ is a registered variant", () => {
    const unregistered: string[] = [];
    for (const [name, files] of usedVariants()) {
      if (!(VARIANTS as readonly string[]).includes(name)) {
        unregistered.push(`${name} — ${[...new Set(files)].join(", ")}`);
      }
    }
    expect(
      unregistered,
      `unregistered button variant(s). Add the tone to globals.css + ` +
        `VARIANTS here, or use ${VARIANTS.join(" / ")}.`,
    ).toEqual([]);
  });

  it("every registered variant is actually defined in globals.css", () => {
    for (const v of VARIANTS) {
      expect(css, `globals.css has no .${v} rule`).toMatch(
        new RegExp(`^\\.${v} \\{`, "m"),
      );
      // …and has a light + dark treatment, so no call site can ship a
      // tone that only exists in one mode.
      expect(css, `globals.css has no .dark .${v} rule`).toMatch(
        new RegExp(`^\\.dark \\.${v}(:hover)? \\{`, "m"),
      );
    }
  });

  it("registers no dead variant — a name with no caller is not a style", () => {
    const used = usedVariants();
    const dead = VARIANTS.filter((v) => !used.has(v));
    expect(
      dead,
      `dead CSS: ${dead.join(", ")} has no caller. Delete its rules.`,
    ).toEqual([]);
  });

  it("ProfileTab's account actions use the neutral tone, not a ghost one", () => {
    // The original bug, pinned at its source: the two buttons next to the
    // avatar were the app's only `btn-outline` users — bordered ghosts in a
    // row of solid fills. Also a named check so the regression reads as
    // "this specific pair is neutral" rather than only the blanket rule
    // above, which a future rename could make vacuous.
    const code = readFileSync(resolve(ROOT, "src/components/settings/ProfileTab.tsx"), "utf8")
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .replace(/\/\/[^\n]*/g, "");
    const tones = [...code.matchAll(/(?<![\w-])(btn-[a-z0-9-]+)/g)].map((m) => m[1]);
    expect(tones.length, "expected the two account-action buttons").toBeGreaterThan(0);
    expect(new Set(tones)).toEqual(new Set(["btn-solid"]));
  });
});