import type { PreviewBounds } from "./previewHost";

/**
 * Geometry helpers for the native preview webview.
 *
 * The native webview lives in window-client coordinates, and so does
 * `getBoundingClientRect()` — no scroll or DPI translation is needed, which is
 * why these are pure functions over four numbers rather than a layout engine.
 * Rounding matters: fractional native positions make WebView2 render text on
 * half-pixels, and sub-pixel churn would otherwise spam `preview_set_bounds`
 * on every resize frame.
 */

/** The subset of `DOMRect` this module needs — plain numbers so it is testable. */
export interface RectLike {
    left: number;
    top: number;
    width: number;
    height: number;
}

/** Snap a DOM rect to integer logical pixels, never negative. */
export function toPreviewBounds(rect: RectLike): PreviewBounds {
    return {
        x: Math.max(0, Math.round(rect.left)),
        y: Math.max(0, Math.round(rect.top)),
        width: Math.max(0, Math.round(rect.width)),
        height: Math.max(0, Math.round(rect.height)),
    };
}

/** Whether two rects describe the same native geometry (after rounding). */
export function sameBounds(a: PreviewBounds, b: PreviewBounds): boolean {
    return a.x === b.x && a.y === b.y && a.width === b.width && a.height === b.height;
}

/**
 * A rect that cannot display anything. The Rust side parks (hides) the webview
 * for these instead of sizing it to zero, because a zero-sized webview still
 * paints on some platforms.
 */
export function isParkedBounds(bounds: PreviewBounds): boolean {
    return bounds.width < 1 || bounds.height < 1;
}