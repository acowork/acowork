/**
 * Source guard: the file pane borrows the right panel's width, and the rule
 * that decides when lives in one pure reducer.
 *
 * Why this file exists on top of `filePanelAutoCollapse.test.ts`:
 *   That file pins the DECISION (fold on a file appearing, restore on the
 *   last file closing, the reader's own click wins). It cannot see whether
 *   AppLayout actually wires the decision to the layout — and a layout that
 *   computes the same rule inline would pass every behavioural test while
 *   drifting from the reducer, which is exactly how the two copies diverge.
 *
 * Why source scanning instead of a rendered assertion (same reasoning as
 *   `filePaneWidth.test.ts` and `capsule.test.tsx`):
 *   jsdom computes no layout, and AppLayout pulls in Tauri / MQTT / monaco
 *   stores, so a rendered width assertion would be both brittle and
 *   unrepresentative. What must hold is the WIRING: one reducer, called
 *   from the layout, and no second copy of the rule.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));

const read = (rel: string) => readFileSync(resolve(__dirname, "..", "..", rel), "utf8");

const APP_LAYOUT = "components/layout/AppLayout.tsx";

describe("the right panel folds for the file pane", () => {
  it("drives the fold from the one reducer", () => {
    const src = read(APP_LAYOUT);
    expect(src).toContain("reduceFilePanelAuto");
    // The rule must not be re-implemented inline: a second copy of the
    // decision is a second copy of the bug. The decision is made by the
    // reducer alone — the layout only applies what it returns.
    const decision = src.slice(
      src.indexOf('type: "files"'),
      src.indexOf('type: "panel"'),
    );
    expect(decision).toContain('decision.action.kind === "collapse"');
    expect(decision).toContain('decision.action.kind === "expand"');
  });

  it("folds and restores through the same panel flag the rail toggles", () => {
    const src = read(APP_LAYOUT);
    // The reader's own button writes the same flag, so the fold and the
    // manual toggle cannot disagree about the panel's state.
    expect(src).toContain("setRightPanelCollapsed(true)");
    expect(src).toContain("setRightPanelCollapsed(false)");
  });

  it("folds in a layout effect, so no frame shows both panels", () => {
    const src = read(APP_LAYOUT);
    // A passive effect would let the browser paint one frame with the
    // panel AND the file pane — the squeeze this feature exists to remove.
    const filesEffect = src.slice(
      src.indexOf('type: "files"'),
      src.indexOf('type: "panel"'),
    );
    expect(filesEffect).toContain("useLayoutEffect");
  });

  it("splits the row with the chat pane instead of stacking on top of it", () => {
    const src = read(APP_LAYOUT);
    // The first attempt lent the panel's width to the file pane
    // (`fileWidth += panelWidth`), which pushed the chat pane onto its floor.
    // The width is now DERIVED from the row's budget.
    expect(src).toContain("sharedFilePaneWidth");
    // The `+=` must not come back in CODE. The comment that explains why it
    // is wrong may name it, so the scan skips comment lines.
    const code = src
      .split("\n")
      .filter((line) => !line.trimStart().startsWith("//"))
      .join("\n");
    expect(code).not.toMatch(/fileWidth\s*\+=\s*/);
    // The pane renders with the split until the reader sizes it by hand.
    expect(src).toContain("renderedFileWidth");
  });

  it("lets the reader's drag win over the split", () => {
    const src = read(APP_LAYOUT);
    // A drag is a recorded intent: from then on the split must not override
    // it, or the pane would snap back the moment the row changed.
    expect(src).toContain("setFilePaneUserSized(true)");
    expect(src).toMatch(/filePaneUserSized\s*\?\s*fileWidth\s*:\s*splitFileWidth/);
  });
});
