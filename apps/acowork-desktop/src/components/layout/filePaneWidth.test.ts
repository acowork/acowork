/**
 * Source guard: the file pane's width has no hand-picked ceiling.
 *
 * Why this exists on top of `panelClamp.test.ts`:
 *   Those tests pin the *policy* — `filePaneCeiling` returns the row's budget,
 *   `initialFileWidth` prefers the user's recorded width. Neither can see the
 *   call sites, and the bug this replaces was exactly a call site: four
 *   independent `Math.min(MAX_FILE_WIDTH, …)` style formulas, one of which was
 *   `const MAX_FILE_WIDTH = 900`.
 *
 *   A policy function makes re-adding a cap harder but not impossible —
 *   `Math.min(900, filePaneCeiling(…))` still type-checks. This file closes
 *   that hole for the one number that matters.
 *
 * Why source scanning instead of a rendered assertion (same reasoning as
 * `capsule.test.tsx`):
 *   jsdom computes no layout, and AppLayout pulls in Tauri / MQTT / monaco
 *   stores, so a rendered width assertion would be both brittle and
 *   unrepresentative. The arithmetic IS the spec here.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));

const read = (rel: string) => readFileSync(resolve(__dirname, "..", "..", rel), "utf8");

const APP_LAYOUT = "components/layout/AppLayout.tsx";

/** Every place the file pane's width is decided. */
const WRITE_PATHS = [
  "storedWidth: readStoredFileWidth()", // initial state
  "storedWidth: readStoredFileWidth()", // first open after the last tab closed
  "clampFileWidth(", // proportional window-resize scaling
  "maxFileWidthRef.current = filePaneCeiling(", // drag
];

describe("the file pane has no fixed width ceiling", () => {
  it("keeps no fixed-cap constant for the file pane", () => {
    const src = read(APP_LAYOUT);
    expect(src).not.toMatch(/MAX_FILE_WIDTH/);
    // A cap that came back under another name would still look like a pixel
    // constant being min'd into a width.
    expect(src).not.toMatch(/Math\.min\(\s*\d{3,}\s*,\s*(filePaneCeiling|Math\.max)/);
  });

  it("derives the bound from panelClamp instead of re-deriving the row", () => {
    const src = read(APP_LAYOUT);
    expect(src).toContain("filePaneCeiling(");
    // CHROME_WIDTH is panelClamp's business; AppLayout importing it is how the
    // chrome arithmetic drifted into four slightly different formulas.
    expect(src).not.toContain("CHROME_WIDTH");
  });

  it("routes every width decision through the shared helpers", () => {
    const src = read(APP_LAYOUT);
    for (const marker of new Set(WRITE_PATHS)) {
      expect(src, `missing: ${marker}`).toContain(marker);
    }
  });
});
