/**
 * The WeChat-style group avatar tiers.
 *
 * Pins the visual contract the sidebar group row depends on: the composite
 * grows from a 2×2 four-grid, to two rows, to a 3×3 nine-grid as the member
 * count climbs, and always shows at least the four cells of the four-grid so
 * a small group is still recognisably "a group" (empty cells render as
 * gray placeholder blocks).
 */
import { describe, it, expect, vi } from "vitest";

// The component module pulls in the store graph; keep it importable in a
// bare node/jsdom test the same way the other sidebar tests do.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => ({})) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

import { gridFor } from "./GroupAvatarStack";

describe("gridFor — WeChat group-avatar tiers", () => {
  it("uses a 2×2 four-grid up to four members", () => {
    for (const n of [0, 1, 2, 3, 4]) {
      expect(gridFor(n)).toMatchObject({ cols: 2, cells: 4 });
    }
  });

  it("uses two rows (3 columns) for five and six members", () => {
    for (const n of [5, 6]) {
      expect(gridFor(n)).toMatchObject({ cols: 3, cells: 6 });
    }
  });

  it("uses a 3×3 nine-grid for more than six, capping what is shown", () => {
    for (const n of [7, 9, 42]) {
      expect(gridFor(n)).toMatchObject({ cols: 3, cells: 9 });
    }
  });

  it("fills the 40×40 footprint: cols×tile + gaps + padding ≤ 40", () => {
    for (const n of [4, 6, 9]) {
      const { cols, cells, tile, gap, pad } = gridFor(n);
      const width = cols * tile + (cols - 1) * gap + pad * 2;
      const rows = cells / cols;
      const height = rows * tile + (rows - 1) * gap + pad * 2;
      expect(width).toBeLessThanOrEqual(40);
      expect(height).toBeLessThanOrEqual(40);
    }
  });
});
