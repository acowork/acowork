/**
 * Self-check for the toolbar trigger's shrink chain.
 *
 * Bug: in the workspace panel the trigger sat next to three action buttons
 * (new file / new folder / refresh). A long workspace name overlapped them —
 * the trigger kept its content width because every box between the row and
 * the label was a flex item with the default `min-width: auto`.
 *
 * The chain that has to hold (row → trigger root → tooltip wrapper → button
 * → label): each link needs `min-w-0` for the squeeze to propagate, and the
 * label needs `truncate` so the overflow becomes "文字…".
 *
 * jsdom computes no layout, so this reads the source instead of the
 * rendered DOM (same approach as ToolbarDropdown.fontSize.test.tsx).
 */
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";

const triggerSrc = readFileSync(path.join(__dirname, "ToolbarDropdown.tsx"), "utf8");
const tooltipSrc = readFileSync(path.join(__dirname, "Tooltip.tsx"), "utf8");

describe("toolbar trigger shrink chain", () => {
  it("gives the trigger root min-w-0 so it can shrink", () => {
    // The root must NOT be `inline-block`: an inline-block flex item keeps
    // its min-content width, which is what made the name overlap.
    const root = triggerSrc.slice(triggerSrc.indexOf("data-toolbar-btn"), triggerSrc.indexOf("data-toolbar-btn") + 200);
    expect(root).toContain("min-w-0");
    expect(root).not.toContain("inline-block");
  });

  it("truncates the label with an ellipsis", () => {
    const label = triggerSrc.slice(triggerSrc.indexOf("data-toolbar-text"));
    expect(label).toMatch(/min-w-0[^"]*truncate/);
  });

  it("lets the tooltip wrapper shrink too (both variants)", () => {
    const wrappers = tooltipSrc.match(/className="relative inline-flex[^"]*"/g) ?? [];
    expect(wrappers.length).toBeGreaterThanOrEqual(2);
    for (const w of wrappers) {
      expect(w).toContain("min-w-0");
    }
  });
});
