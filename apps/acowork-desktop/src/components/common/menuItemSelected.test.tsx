/**
 * Self-check for the composer toolbar dropdowns' selected-row styling.
 *
 * User-visible feature: in the model / reasoning-effort / workspace / skills
 * popups, the CURRENT selection carries a tinted background, not just a
 * recoloured label. Before the first fix the selected row was text-only
 * while every *other* row lit up a neutral grey on hover — so the row you
 * had actually chosen read as weaker than the row the cursor happened to be
 * crossing. Then accent text ON an accent tint turned out to be the
 * loudest thing on screen, so the label reverted to the unselected rows'
 * foreground and the tint alone carries the selection.
 *
 * Why source scanning instead of rendered DOM: the tint is a Tailwind
 * arbitrary value (`bg-[var(--color-accent)]/10`) that jsdom neither
 * resolves nor asserts on. The class strings ARE the spec here — same
 * approach as capsule.test.tsx / ToolbarDropdown.fontSize.test.tsx.
 */
import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, relative, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  cardButtonNeutral,
  dialogButtonPrimary,
  dialogButtonSecondary,
  interactiveHover,
  listRowHover,
  menuItemSelected,
  toolbarButton,
  toolbarIconButton,
} from "../../lib/ui-styles";

const __dirname = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(__dirname, "..", "..", rel), "utf8");

/**
 * Every composer-toolbar popup that renders a "currently selected" row,
 * with the number of selected rows it owns.
 */
const MENUS = [
  // [file, selected rows it owns, label-foreground expressions that must not come back]
  ["components/chat/ChatPanel.tsx", 3, [
    'style={isActive ? { color: "var(--color-accent)" } : undefined}',
    'isActive && "text-[var(--color-accent)]"',
  ]],
  ["components/workspace/WorkspaceSelector.tsx", 2, [
    'isCurrent ? "text-[var(--color-accent)]"',
    'currentWsId === "__agent_home__" ? { color: "var(--color-accent)" } : {}',
  ]],
  ["components/skills/SkillMenuList.tsx", 1, [
    'isActive ? "text-[var(--color-accent)]"',
    'style={isActive ? { color: "var(--color-accent)" } : { color: "" }}',
  ]],
] as const;

describe("the popup cursor highlight", () => {
  // 光标划过下拉菜单的一行，和划过列表里的一行，必须是同一块颜色 —— 两者在
  // pm 视图里就是相邻的操作。菜单项走的是 globals.css 的 `.context-menu-item`
  // （PmPopover / ContextMenu 共用），所以这条规则在那里钉一次就够了。
  it("is the same accent tint as listRowHover, not a neutral grey", () => {
    const css = read("styles/globals.css");
    const hover = css.slice(
      css.indexOf(".context-menu-item:hover {"),
      css.indexOf(".context-menu-item:disabled"),
    );
    expect(hover).toContain("--color-accent");
    expect(hover).not.toMatch(/#f4f4f5|#3f3f46|zinc-100|zinc-700/);

    const dark = css.slice(
      css.indexOf(".dark .context-menu-item:hover {"),
      css.indexOf(".dark .context-menu-item:disabled"),
    );
    expect(dark).toContain("--color-accent");
    expect(dark).not.toMatch(/#3f3f46|zinc-700/);
  });

  it("leaves the destructive / warning variants their own hue", () => {
    const css = read("styles/globals.css");
    expect(css).toMatch(/--danger:hover \{[^}]*red/);
    expect(css).toMatch(/--warning:hover \{[^}]*#fffbeb/);
  });
});

describe("menuItemSelected", () => {
  it("is a background only — never a text colour", () => {
    // A text-only selection is the first bug this pins; accent text on an
    // accent tint is the second. The token must not set `text-` at all, so
    // each row keeps the same foreground as its unselected siblings.
    expect(menuItemSelected).toMatch(/bg-\[var\(--color-accent\)\]/);
    expect(menuItemSelected).not.toContain("text-");
    // Keeps reacting to hover, one step stronger than the resting tint.
    expect(menuItemSelected).toContain("hover:bg-[var(--color-accent)]");
    // Unselected rows take the same accent family, one step lighter, so
    // the cursor passing over them reads as "approaching selection"
    // (see listRowHover in lib/ui-styles.ts).
    expect(listRowHover).toMatch(/hover:bg-\[var\(--color-accent\)\]/);
  });
});

it.each(MENUS)("%s routes every selected row through it", (rel, expected, banned) => {
  const src = read(rel);
  // +1 accounts for the import statement itself. Every selected row must
  // pull the token in, so a new menu row cannot ship text-only by default.
  expect(src.split("menuItemSelected").length - 1).toBe(expected + 1);
  // The hand-rolled two-space hover string these rows used before is gone.
  expect(src).not.toContain('"hover:bg-zinc-50  dark:hover:bg-zinc-700/50"');
  // A selected row's LABEL must not re-apply the accent foreground behind
  // the token's back. Scoped to the label expressions, not the whole file:
  // the check mark and the surrounding badges stay accent on purpose.
  for (const frag of banned) expect(src).not.toContain(frag);
});

/**
 * Buttons used to light up a neutral zinc/grey rectangle while the menu rows
 * they open wore the accent tint. The two must now be ONE colour family —
 * source scanning, because jsdom does not resolve Tailwind arbitrary values.
 * Overlay controls (image lightbox, thumbnail delete) keep their black/white
 * scrim on purpose and are deliberately outside this rule.
 */
const NEUTRAL_BUTTON_HOVER =
  /(?<![\w-])(?:dark:)?hover:bg-(?:zinc|gray|nav-item-hover|nav-control|hover-overlay|bg-hover|panel-inset|hover)(?![\w-])/;

function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = resolve(dir, entry.name);
    if (entry.isDirectory()) out.push(...sourceFiles(path));
    else if (/\.(?:ts|tsx|css)$/.test(entry.name) && !/\.test\./.test(entry.name)) out.push(path);
  }
  return out;
}

describe("interactiveHover — one highlight for buttons and list rows", () => {
  it("is literally the list-row hover token", () => {
    expect(interactiveHover).toBe(listRowHover);
    expect(interactiveHover).toContain("hover:bg-[var(--color-accent)]/10");
    expect(interactiveHover).toContain("dark:hover:bg-[var(--color-accent)]/15");
  });

  it.each([
    ["toolbarButton", toolbarButton],
    ["toolbarIconButton", toolbarIconButton],
    ["dialogButtonSecondary", dialogButtonSecondary],
    ["cardButtonNeutral", cardButtonNeutral],
  ])("%s uses it and no neutral grey", (_name, cls) => {
    expect(cls).toContain(interactiveHover);
    expect(cls).not.toMatch(/hover:bg-(?:zinc|gray|nav)/);
  });

  it("solid primary buttons echo the accent with a ring, never a wash", () => {
    expect(dialogButtonPrimary).toContain("bg-zinc-800");
    expect(dialogButtonPrimary).toContain("hover:ring-2");
    expect(dialogButtonPrimary).toContain("hover:ring-[var(--color-accent)]/70");
    expect(dialogButtonPrimary).not.toContain("hover:bg-[var(--color-accent)]");
  });

  it("leaves no neutral grey hover on any clickable surface", () => {
    const srcDir = resolve(__dirname, "..", "..");
    const offenders: string[] = [];
    for (const file of sourceFiles(srcDir)) {
      readFileSync(file, "utf8")
        .split(/\r?\n/)
        .forEach((line, i) => {
          if (!NEUTRAL_BUTTON_HOVER.test(line)) return;
          // Solid dark primary buttons keep their opaque fill and darken it
          // by one step. They may keep that neutral hover ONLY while they
          // also echo the accent with a hover ring (see dialogButtonPrimary).
          if (/bg-zinc-(?:800|900)/.test(line) &&
              line.includes("hover:ring-[var(--color-accent)]")) return;
          offenders.push(`${relative(srcDir, file)}:${i + 1}`);
        });
    }
    expect(offenders).toEqual([]);
  });
});
