// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// The page column: every number the continuous region picker needs.
//
// Why this is a separate module rather than code inside the viewer:
// the picker used to own exactly one page, so "the stage rect ÷ scale" was the
// whole coordinate problem. A column of pages adds three new questions that are
// pure arithmetic and therefore worth pinning without a DOM — which page is
// under this point, how tall the column is, and which pages must actually be
// painted right now. Painting every page of a 30-page deck at
// `PDF_OUTPUT_SCALE` is ~8 MB of canvas per page (A4 at 2x is 1190×1684 px at
// 4 bytes), so thirty of them is a quarter of a gigabyte — the last question is
// not an optimisation, it is the only thing that keeps the feature affordable.
//
// Units, spelled out because mixing them is the classic way to break this:
//
//   - `ImageSize` of a page is its BITMAP size (print px × `PDF_OUTPUT_SCALE`),
//     the same space a crop is measured in and the same space `ImageViewer`
//     uses for an `<img>`'s `naturalWidth`.
//   - a `StackLayout` is in COLUMN px = bitmap px × display scale. It is what
//     the DOM is laid out at, so a pointer offset divided by the scale lands
//     back in bitmap px — the one-division contract `imageViewerGeometry`
//     documents.
//   - scroll offsets are CSS px inside the scroll viewport.
//
// Gaps between pages are fixed screen px, not scaled: the native viewer's grey
// gutter does not grow when you zoom either, and a scaled gap would make the
// column's height depend on a number the user cannot see.
import type { ImagePoint, ImageSize } from "./imageViewerGeometry";

/** Screen px of gutter between two pages. */
export const PAGE_GAP_PX = 12;

/** How far beyond the visible viewport pages stay painted, in viewport
 *  heights. One screenful means a wheel flick arrives at already-painted
 *  paper instead of a blank box. */
export const OVERSCAN_VIEWPORTS = 1;

/** Hard ceiling on simultaneously painted pages, regardless of how many fit in
 *  the viewport + overscan. Bounds the worst case (a zoomed-out view of a
 *  portrait document) at roughly `MAX_PAINTED_PAGES × 8 MB` of canvas for a
 *  16:9 slide, which is the number this feature must not exceed. */
export const MAX_PAINTED_PAGES = 6;

/** The widest page's bitmap width — what a fit-to-width pass scales against. */
export function widestPage(sizes: ImageSize[]): number {
    return sizes.reduce((max, size) => Math.max(max, size.width), 0);
}

/** Where one page sits in the column, in column px. */
export interface PageBox {
    /** 1-based, the same numbering pdf.js and the attachment hand-off use. */
    page: number;
    top: number;
    left: number;
    width: number;
    height: number;
    /** Bitmap size, so a caller can go from column px back to image px. */
    natural: ImageSize;
}

/** The whole column, in screen px (the DOM is laid out at exactly this). */
export interface StackLayout {
    boxes: PageBox[];
    /** Widest page × scale — the box a page is centred inside. */
    width: number;
    /** Bottom of the last page: the column's scrollable height. */
    height: number;
    /** The gutter this layout was built with, in screen px. Carried here so
     *  the band and window lookups below read the gap THIS column uses instead
     *  of the module default — a caller that lays out with a different gutter
     *  would otherwise get hit tests that disagree with the DOM by that gap. */
    gap: number;
    /** The scale the layout was built at. Hit testing needs it to turn a
     *  screen-px offset back into a bitmap px, and carrying it here is what
     *  keeps a caller from passing a scale that disagrees with the layout. */
    scale: number;
}

/**
 * Lay `sizes` out as a vertical column at `scale`, centred horizontally.
 *
 * Pages are centred per row (not left-aligned) because PDF pages can differ in
 * size — a mixed portrait/landscape document — and a column that jittered its
 * left edge page to page reads as broken. The gutter is `gap` screen px.
 */
export function layoutStack(
    sizes: ImageSize[],
    scale: number,
    gap: number = PAGE_GAP_PX,
): StackLayout {
    const s = Number.isFinite(scale) && scale > 0 ? scale : 1;
    const g = Number.isFinite(gap) && gap >= 0 ? gap : PAGE_GAP_PX;
    const widest = sizes.reduce((max, size) => Math.max(max, size.width * s), 0);
    let top = 0;
    const boxes: PageBox[] = sizes.map((natural, index) => {
        const width = natural.width * s;
        const height = natural.height * s;
        const box: PageBox = {
            page: index + 1,
            top,
            left: (widest - width) / 2,
            width,
            height,
            natural,
        };
        top += height + g;
        return box;
    });
    return {
        boxes,
        width: widest,
        // The trailing gutter belongs to no page: the last page ends the run.
        height: boxes.length ? top - g : 0,
        gap: g,
        scale: s,
    };
}

/** The `page`-th box (1-based), or `null` for a page outside the document. */
export function boxOf(layout: StackLayout, page: number): PageBox | null {
    return layout.boxes[page - 1] ?? null;
}

/**
 * Project a screen-px column point onto ONE page, in that page's bitmap px.
 *
 * The clamp is the whole point: a drag that wanders off the page it started on
 * is pulled back to that page's edge instead of jumping to its neighbour,
 * because a crop is one bitmap and cannot straddle two.
 */
export function pointOnPage(
    layout: StackLayout,
    page: number,
    point: ImagePoint,
): ImagePoint | null {
    const box = boxOf(layout, page);
    if (!box) return null;
    const s = layout.scale > 0 ? layout.scale : 1;
    return {
        x: Math.min(Math.max(Math.round((point.x - box.left) / s), 0), Math.max(box.natural.width - 1, 0)),
        y: Math.min(Math.max(Math.round((point.y - box.top) / s), 0), Math.max(box.natural.height - 1, 0)),
    };
}

/** Which page a column point lands on (gutter included), plus that point. */
/**
 * Which page a screen-px point in the column belongs to, and where that point
 * is inside it — in BITMAP px, the space a crop and a selection are measured in.
 *
 * This is the one division by `scale` the whole surface needs: the caller
 * subtracts the column's own `getBoundingClientRect().left/top` from a pointer
 * event and hands the raw offset over, so page centring, the gutter and the
 * scroll position are all resolved here rather than being inverted at every
 * call site.
 *
 * A point in the gutter is attributed to the page ABOVE it: the band of a page
 * runs from its top edge down past its bottom edge by one gap, so a drag that
 * starts just below a page frames that page rather than the next one. The last
 * page's band stops at its bottom edge. The point is clamped into the page by
 * `pointOnPage`, because a selection has no meaning on the gutter — and only
 * `y` selects the page: the horizontal margins a centred narrower page leaves
 * belong to that page as far as framing is concerned.
 *
 * Returns `null` only for a point above the first page or below the column —
 * i.e. outside the document entirely.
 */
export function pageAtColumnPoint(
    layout: StackLayout,
    point: ImagePoint,
): { page: number; point: ImagePoint } | null {
    const index = layout.boxes.findIndex(
        (candidate, i) =>
            point.y >= candidate.top &&
            (i === layout.boxes.length - 1
                ? point.y <= candidate.top + candidate.height
                : point.y < candidate.top + candidate.height + layout.gap),
    );
    if (index < 0) return null;
    const box = layout.boxes[index];
    return { page: box.page, point: pointOnPage(layout, box.page, point)! };
}

/**
 * The pages that must be painted for a viewport at `scrollTop`, and nothing
 * else.
 *
 * "Visible" means the page's band intersects the viewport grown by
 * `OVERSCAN_VIEWPORTS` on each side. The result is then trimmed to
 * `MAX_PAINTED_PAGES`, and the trim is the reason this function is not a
 * one-line filter: the pages actually inside the viewport are taken FIRST, so
 * the ceiling can never spend the whole paint budget on the overscan band
 * above the user's eyes. A document-order slice would do exactly that once a
 * zoomed-out column shows more pages than the cap fits — the visible area
 * would go blank precisely when the user is looking at the most pages at once.
 * When even the visible band overflows the cap, the topmost visible pages win.
 * The leftover slots go to the overscan pages NEAREST the viewport, because
 * those are the ones a wheel flick reaches first.
 *
 * An unmeasured viewport (`height === 0`, the state of a jsdom node before any
 * layout, and of a real one before its first frame) still answers with the
 * first page: a surface that paints nothing until it is measured is a surface
 * that never paints.
 */
export function liveWindow(
    layout: StackLayout,
    scrollTop: number,
    viewportHeight: number,
): number[] {
    if (!layout.boxes.length) return [];
    const height = Math.max(viewportHeight, 0);
    const overscan = height > 0 ? height * OVERSCAN_VIEWPORTS : 0;
    const from = scrollTop - overscan;
    const to = scrollTop + height + overscan;
    const intersects = (box: PageBox, low: number, high: number) =>
        box.top < high && box.top + box.height + layout.gap > low;
    const wanted = layout.boxes.filter((box) => intersects(box, from, to));
    if (!wanted.length) return [layout.boxes[0].page];

    const visible = new Set(
        wanted.filter((box) => intersects(box, scrollTop, scrollTop + height)).map((box) => box.page),
    );
    const bandLow = scrollTop;
    const bandHigh = scrollTop + height;
    // Distance from the visible band, so the spare slots go to the pages a
    // flick in either direction reaches first rather than to whatever sits at
    // the top of the overscan.
    const distanceFromBand = (box: PageBox) =>
        Math.max(0, bandLow - (box.top + box.height + layout.gap), box.top - bandHigh);
    const beyond = wanted
        .filter((box) => !visible.has(box.page))
        .sort((a, b) => distanceFromBand(a) - distanceFromBand(b));
    const ordered = [
        ...wanted.filter((box) => visible.has(box.page)),
        ...beyond,
    ];
    return ordered.slice(0, MAX_PAINTED_PAGES).map((box) => box.page);
}

/**
 * The scroll offset that puts `page` at the top of the viewport.
 *
 * The first page scrolls to 0 rather than to its own top (which is 0 anyway,
 * but the clamp is what keeps a caller from scrolling past the document's end
 * with a page number it got from a stale layout).
 */
export function scrollTargetForPage(layout: StackLayout, page: number): number {
    const box = boxOf(layout, page);
    return box ? Math.max(0, box.top) : 0;
}

/** The page whose band contains the top of the viewport — what the page
 *  readout shows while the user scrolls. */
export function pageAtScroll(layout: StackLayout, scrollTop: number): number {
    // A point inside the first column pixel of the viewport: `x` is irrelevant
    // to the band lookup, so the left edge is as good a probe as any.
    const probe = { x: 0, y: scrollTop + 1 };
    const hit = pageAtColumnPoint(layout, probe);
    return hit ? hit.page : layout.boxes.length ? layout.boxes[layout.boxes.length - 1].page : 1;
}
