// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Geometry for the workspace image preview (`ImageViewer`).
//
// Kept out of the component on purpose: every rule below is arithmetic that a
// human gets subtly wrong (half-pixel offsets, zoom that drifts away from the
// cursor, selections that escape the image bounds), and none of it needs a
// DOM to be checked. The component owns *when* to call these; this module owns
// *what the numbers are*.
//
// Coordinate spaces, named once so the call sites can't confuse them:
//
//   client  — viewport pixels, what a PointerEvent carries
//   image   — natural image pixels, what the chat prompt and the crop use
//   screen  — `client` minus the <img> bounding box, i.e. the scaled image
//
// `scale` is image → screen (1 = 100%). All conversions here are therefore
// divisions by `scale`, and every result that leaves this module is an integer
// image pixel inside the image rectangle.

export interface ImagePoint {
    x: number;
    y: number;
}

export interface ImageSize {
    width: number;
    height: number;
}

/** An axis-aligned rectangle in image pixels. `width` / `height` are ≥ 1. */
export interface ImageRect {
    x: number;
    y: number;
    width: number;
    height: number;
}

/** Zoom bounds. Matches the range the chat's Mermaid viewer uses closely
 *  enough that the two surfaces feel the same, without the 8× ceiling being
 *  low enough to stop a user inspecting screenshot detail: 8× turns one image
 *  pixel into an 8×8 block. */
export const MIN_ZOOM = 0.1;
export const MAX_ZOOM = 8;

/** One button click / one wheel notch. 1.25 is the multiplier that reaches
 *  8× from 100% in nine steps — fast enough to feel responsive, small enough
 *  that the intermediate steps stay recognisable. */
export const ZOOM_STEP = 1.25;

/** A drag smaller than this (in IMAGE pixels) is a click that wobbled, not a
 *  selection. Expressed in image px rather than screen px so the tolerance
 *  does not change with zoom. */
export const MIN_SELECTION_PX = 4;

/** Clamp a zoom factor into [MIN_ZOOM, MAX_ZOOM]. Non-finite input (a divisor
 *  of zero upstream, a NaN from an unmeasured element) collapses to 1 so a
 *  broken measurement cannot leave the image invisible. */
export function clampZoom(scale: number): number {
    if (!Number.isFinite(scale) || scale <= 0) return 1;
    return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, scale));
}

/**
 * The scale that fits `image` inside `viewport`, never enlarging past 1:1.
 *
 * Not enlarging is deliberate (same rule as the chat Mermaid viewer): a small
 * icon or a narrow diagram would otherwise be blown up to fill the panel and
 * look broken. Big images shrink to fit; small ones show at their own size.
 *
 * Returns 1 when either measurement is unusable (0 / NaN), which is the state
 * of a freshly mounted viewport before the first layout pass — the caller will
 * be told again by its ResizeObserver.
 */
export function fitScale(image: ImageSize, viewport: ImageSize): number {
    const vw = viewport.width;
    const vh = viewport.height;
    if (!(image.width > 0) || !(image.height > 0)) return 1;
    if (!(vw > 0) || !(vh > 0)) return 1;
    return clampZoom(Math.min(1, vw / image.width, vh / image.height));
}

/**
 * Convert a client-space point (a pointer event) to an image pixel.
 *
 * `imageOrigin` is the `<img>` element's own `getBoundingClientRect().left/top`
 * — the element is laid out at exactly `naturalSize × scale`, so dividing the
 * offset by `scale` lands on the image pixel, whatever the scroll position or
 * the centring margins happen to be. Rounding (not flooring) keeps the number
 * under the cursor stable at fractional scales instead of drifting a pixel.
 *
 * The result is NOT clamped: callers that need an in-bounds pixel go through
 * `clampPoint`, while callers that want to know "the pointer left the image"
 * must be able to see the out-of-bounds value.
 */
export function screenToImagePoint(
    client: { clientX: number; clientY: number },
    imageOrigin: { left: number; top: number },
    scale: number,
): ImagePoint {
    const s = Number.isFinite(scale) && scale > 0 ? scale : 1;
    return {
        x: Math.round((client.clientX - imageOrigin.left) / s),
        y: Math.round((client.clientY - imageOrigin.top) / s),
    };
}

/** Clamp a point to the last addressable pixel of the image (0..width-1). */
export function clampPoint(point: ImagePoint, size: ImageSize): ImagePoint {
    const maxX = Math.max(0, size.width - 1);
    const maxY = Math.max(0, size.height - 1);
    return {
        x: Math.min(maxX, Math.max(0, Math.round(point.x))),
        y: Math.min(maxY, Math.max(0, Math.round(point.y))),
    };
}

/**
 * The rectangle a drag between `anchor` and `current` covers, clamped to the
 * image. No minimum: this is the live preview drawn while the pointer is still
 * down, so a 1×1 box must render (the threshold is applied on release, see
 * `dragSelectionRect`).
 *
 * Dragging up/left is normal user behaviour, so the rectangle is normalised
 * (never negative) rather than rejected.
 */
export function normalizeDragRect(
    anchor: ImagePoint,
    current: ImagePoint,
    size: ImageSize,
): ImageRect {
    const a = clampPoint(anchor, size);
    const b = clampPoint(current, size);
    const left = Math.min(a.x, b.x);
    const top = Math.min(a.y, b.y);
    // +1: a drag from pixel 10 to pixel 10 covers one pixel, not zero.
    return {
        x: left,
        y: top,
        width: Math.max(1, Math.abs(a.x - b.x) + 1),
        height: Math.max(1, Math.abs(a.y - b.y) + 1),
    };
}

/**
 * Turn a released drag into a selection, or `null` when the drag is too small
 * to be one.
 *
 * `MIN_SELECTION_PX` is applied to both axes AFTER clamping: a 3×800 sliver is
 * as accidental as a 3×3 dot — it is the same wobble, just along one axis.
 */
export function dragSelectionRect(
    anchor: ImagePoint,
    current: ImagePoint,
    size: ImageSize,
): ImageRect | null {
    if (!(size.width > 0) || !(size.height > 0)) return null;
    const rect = normalizeDragRect(anchor, current, size);
    if (rect.width < MIN_SELECTION_PX || rect.height < MIN_SELECTION_PX) return null;
    return rect;
}

/**
 * Scroll offsets that keep the image pixel under the cursor under the cursor
 * across a zoom step.
 *
 * Measured, not derived from an assumed layout. The stage is centred with
 * `margin: auto` inside the scrolling viewport, so zooming moves the element's
 * origin for two independent reasons: the content grows (ratio), and the
 * auto-margins shrink while the element still fits. A formula that only knows
 * the ratio is exact in the overflowing case and drifts whenever the image is
 * still centred — i.e. exactly while the user is zooming into a fitted image.
 * So the caller passes the element's origin BEFORE and AFTER the re-layout and
 * this returns where the point actually landed versus where it started; adding
 * the result to `scrollLeft` / `scrollTop` puts it back.
 *
 * The clamp is the browser's: once the content no longer overflows there is
 * nowhere to scroll to, and the correction is partly ignored. That is the
 * honest outcome, and it is why the correction is expressed as a delta rather
 * than an absolute offset.
 */
export function zoomAnchorDrift(input: {
    /** Pointer position in client coordinates at the moment of the zoom. */
    pointer: ImagePoint;
    /** `<img>` bounding-box origin before the zoom. */
    originBefore: ImagePoint;
    /** `<img>` bounding-box origin after the re-layout. */
    originAfter: ImagePoint;
    prevScale: number;
    nextScale: number;
}): ImagePoint {
    const { pointer, originBefore, originAfter, prevScale, nextScale } = input;
    if (!(prevScale > 0) || !(nextScale > 0)) return { x: 0, y: 0 };
    const ratio = nextScale / prevScale;
    const offsetX = (pointer.x - originBefore.x) * ratio;
    const offsetY = (pointer.y - originBefore.y) * ratio;
    return {
        x: originAfter.x + offsetX - pointer.x,
        y: originAfter.y + offsetY - pointer.y,
    };
}

/** The percentage shown next to the zoom buttons. */
export function zoomPercent(scale: number): number {
    return Math.round(scale * 100);
}

/**
 * Multiplier for one wheel event.
 *
 * A mouse wheel sends `deltaY ≈ ±100` per notch, a trackpad sends many events
 * of `±1…20`. Treating every event as one notch would make a trackpad zoom
 * uncontrollably, and ignoring magnitude entirely would make the wheel feel
 * dead. So: a full notch is exactly `ZOOM_STEP`, smaller deltas interpolate
 * (`ZOOM_STEP^(delta/100)`), and the exponent is capped at one notch so a
 * single huge delta cannot jump the whole range.
 *
 * Direction follows the platform convention: wheel up (negative `deltaY`)
 * zooms in.
 */
export function wheelZoomFactor(deltaY: number): number {
    if (!Number.isFinite(deltaY) || deltaY === 0) return 1;
    const notches = Math.max(-1, Math.min(1, -deltaY / 100));
    return Math.pow(ZOOM_STEP, notches);
}

/** Multiplier for the zoom buttons: `direction > 0` zooms in. */
export function zoomStepFactor(direction: 1 | -1): number {
    return direction > 0 ? ZOOM_STEP : 1 / ZOOM_STEP;
}