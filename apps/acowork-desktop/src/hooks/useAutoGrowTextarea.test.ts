import { describe, expect, it } from "vitest";
import { clampFieldHeight } from "./useAutoGrowTextarea";

// 14px font, 21px line box, py-2 field (8 + 8).
const LINE = 21;
const PAD = 16;

describe("clampFieldHeight", () => {
  it("follows the content while it is under the cap", () => {
    expect(clampFieldHeight(37, LINE, PAD, 6)).toBe(37);
  });

  it("stops at six lines + padding once the content is taller", () => {
    expect(clampFieldHeight(500, LINE, PAD, 6)).toBe(LINE * 6 + PAD);
  });

  it("passes the cap through unchanged", () => {
    expect(clampFieldHeight(LINE * 6 + PAD, LINE, PAD, 6)).toBe(LINE * 6 + PAD);
  });
});