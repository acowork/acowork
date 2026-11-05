/**
 * The page column's arithmetic — layout, hit test, and the paint window.
 *
 * These are the three things that were not problems when the picker owned one
 * page, and are the three things that can silently be wrong when it owns a
 * column: a page centred inside a wider row, a drag that starts in the gutter,
 * and a window that decides what is cheap enough to rasterize. All of it is
 * pure, so none of it needs a DOM to prove.
 */
import { describe, expect, it } from "vitest";
import {
    MAX_PAINTED_PAGES,
    PAGE_GAP_PX,
    boxOf,
    layoutStack,
    liveWindow,
    pageAtColumnPoint,
    pageAtScroll,
    scrollTargetForPage,
    type StackLayout,
} from "./pdfPageStack";
import type { ImageSize } from "./imageViewerGeometry";

const PAGE = (width: number, height: number): ImageSize => ({ width, height });

/** Three pages, the last one narrower — centring only shows up if a row
 *  differs from the widest one. */
const SIZES: ImageSize[] = [PAGE(100, 200), PAGE(100, 200), PAGE(50, 200)];

function stack(scale = 1, sizes = SIZES): StackLayout {
    return layoutStack(sizes, scale);
}

describe("layoutStack", () => {
    it("stacks pages with one gutter between them and none after the last", () => {
        const layout = stack();

        expect(layout.width).toBe(100);
        expect(layout.height).toBe(200 * 3 + PAGE_GAP_PX * 2);
        expect(layout.boxes.map((box) => box.top)).toEqual([0, 212, 424]);
    });

    it("centres a narrower page inside the widest row", () => {
        const layout = stack();

        expect(layout.boxes[0].left).toBe(0);
        expect(layout.boxes[2].left).toBe(25);
    });

    it("scales the pages but not the gutter", () => {
        const layout = stack(2);

        expect(layout.boxes[0].height).toBe(400);
        expect(layout.boxes[1].top).toBe(400 + PAGE_GAP_PX);
        expect(layout.width).toBe(200);
        expect(layout.scale).toBe(2);
    });

    it("survives an empty document and a broken scale", () => {
        expect(layoutStack([], 1).height).toBe(0);
        expect(layoutStack([], 1).boxes).toEqual([]);
        // A zero/NaN scale (an unmeasured element upstream) must not make the
        // column collapse: it is treated as 1:1.
        expect(layoutStack(SIZES, 0).boxes[1].top).toBe(212);
        expect(layoutStack(SIZES, Number.NaN).boxes[1].top).toBe(212);
    });

    it("answers boxOf for a real page and null outside the document", () => {
        const layout = stack();

        expect(boxOf(layout, 2)?.top).toBe(212);
        expect(boxOf(layout, 0)).toBeNull();
        expect(boxOf(layout, 4)).toBeNull();
    });
});

describe("pageAtColumnPoint", () => {
    it("turns a column offset into the page and its bitmap pixel", () => {
        const layout = stack(2);

        // Screen px (20, 100) on page 1 at scale 2 is bitmap px (10, 50).
        expect(pageAtColumnPoint(layout, { x: 20, y: 100 })).toEqual({
            page: 1,
            point: { x: 10, y: 50 },
        });
    });

    it("resolves a centred narrower page back to its own left edge", () => {
        const layout = stack();

        // Page 3 starts 25 px in, so the column's x=25 is that page's x=0.
        expect(pageAtColumnPoint(layout, { x: 25, y: 500 })?.point.x).toBe(0);
        // Its own right edge (column x=75) clamps to the last addressable
        // column of a 50 px page — the same `clampPoint` rule the single-page
        // picker used, so a drag to the edge selects to the edge and no further.
        expect(pageAtColumnPoint(layout, { x: 75, y: 500 })?.point.x).toBe(49);
    });

    it("gives the gutter to the page above it", () => {
        const layout = stack();

        // 205 is past page 1's bottom (200) and before page 2's top (212).
        const hit = pageAtColumnPoint(layout, { x: 10, y: 205 });
        expect(hit?.page).toBe(1);
        // …and the point is clamped onto the page: a selection cannot live in
        // the gutter, so the last addressable row is what the drag gets.
        expect(hit?.point.y).toBe(199);
    });

    it("returns null above the first page and below the column", () => {
        const layout = stack();

        expect(pageAtColumnPoint(layout, { x: 10, y: -1 })).toBeNull();
        expect(pageAtColumnPoint(layout, { x: 10, y: layout.height + 1 })).toBeNull();
    });
});

describe("liveWindow", () => {
    /** A document tall enough that a viewport sees only part of it. */
    const TALL: ImageSize[] = Array.from({ length: 12 }, () => PAGE(100, 200));

    it("paints the visible pages plus one viewport of overscan", () => {
        const layout = layoutStack(TALL, 1);

        const window = liveWindow(layout, 0, 300);

        // Page pitch is 212 (200 + gutter). Visible: 1-2. Overscan reaches
        // y=600, which covers page 3 (424-624) but not page 4 (top 636).
        expect(window).toEqual([1, 2, 3]);
    });

    it("follows the scroll instead of staying at the top", () => {
        const layout = layoutStack(TALL, 1);

        const window = liveWindow(layout, 1000, 300);

        expect(window).not.toContain(1);
        // Page 6 (1060-1260) is on screen at scrollTop 1000.
        expect(window).toContain(6);
    });

    it("paints the first page while the viewport is unmeasured", () => {
        // jsdom (and a real node before its first frame) reports 0 height. A
        // window that answered "nothing" here would leave the surface blank
        // forever, because nothing would ever paint to trigger a measurement.
        const layout = stack();

        expect(liveWindow(layout, 0, 0)).toEqual([1]);
    });

    it("caps how many pages are alive at once", () => {
        const layout = layoutStack(TALL, 1);

        // A viewport that could see the whole document still gets the ceiling.
        const window = liveWindow(layout, 0, 100000);

        expect(window.length).toBeLessThanOrEqual(MAX_PAINTED_PAGES);
        expect(window[0]).toBe(1);
    });

    it("spends the cap on the visible pages, not on the overscan above them", () => {
        // The ceiling is what makes this worth pinning. Page pitch is 212; a
        // 700 px viewport at scrollTop 2120 shows pages 11-14, and the
        // overscan band reaches from 1420 to 3520 — eleven pages, of which the
        // four ABOVE the viewport come first in document order. A plain
        // `slice(0, MAX)` therefore painted four pages the user cannot see and
        // only two they can: zoom out far enough and the paper goes blank
        // exactly where the eyes are.
        const MANY: ImageSize[] = Array.from({ length: 20 }, () => PAGE(100, 200));
        const layout = layoutStack(MANY, 1);

        const window = liveWindow(layout, 2120, 700);

        expect(window.length).toBe(MAX_PAINTED_PAGES);
        expect(window.filter((page) => page >= 11 && page <= 14)).toEqual([11, 12, 13, 14]);
        // The two spare slots go to the pages nearest the band (10 above,
        // 15 below) — the ones a wheel flick reaches first — not to page 7.
        expect(window).toEqual([11, 12, 13, 14, 10, 15]);
    });

    it("reads the gutter off the layout it was given", () => {
        // `layoutStack` takes a gutter argument, so the band math must follow
        // it: with no gutter a point 5 px past page 1's bottom is page 2's,
        // and with a wide one it is still page 1's.
        const flush = layoutStack(SIZES, 1, 0);
        const wide = layoutStack(SIZES, 1, 100);

        expect(flush.gap).toBe(0);
        expect(pageAtColumnPoint(stack(), { x: 10, y: 205 })?.page).toBe(1);
        expect(pageAtColumnPoint(flush, { x: 10, y: 205 })?.page).toBe(2);
        expect(liveWindow(wide, 250, 0)).toEqual([1]);
        expect(liveWindow(stack(), 250, 0)).toEqual([2]);
    });

    it("is empty for an empty document", () => {
        expect(liveWindow(layoutStack([], 1), 0, 300)).toEqual([]);
    });
});

describe("scroll plumbing", () => {
    it("scrolls to a page's own top and never past the document", () => {
        const layout = stack();

        expect(scrollTargetForPage(layout, 1)).toBe(0);
        expect(scrollTargetForPage(layout, 3)).toBe(424);
        expect(scrollTargetForPage(layout, 99)).toBe(0);
    });

    it("reads the page at the top of the viewport", () => {
        const layout = stack();

        expect(pageAtScroll(layout, 0)).toBe(1);
        expect(pageAtScroll(layout, 250)).toBe(2);
        // Past the end: the readout stays on the last page rather than blanking.
        expect(pageAtScroll(layout, 100000)).toBe(3);
    });
});
