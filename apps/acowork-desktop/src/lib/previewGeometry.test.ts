/**
 * Geometry rules for the native preview webview.
 *
 * These are the four numbers handed to WebView2's `SetWindowPos`, so the tests
 * pin the two decisions that have visible consequences: snapping to integer
 * logical pixels (fractional positions render soft text and churn IPC on every
 * resize frame) and never producing a negative origin.
 */

import { describe, it, expect } from "vitest";
import { isParkedBounds, sameBounds, toPreviewBounds } from "./previewGeometry";

describe("toPreviewBounds", () => {
    it("rounds a DOM rect onto integer logical pixels", () => {
        expect(toPreviewBounds({ left: 10.4, top: 20.6, width: 300.5, height: 199.49 })).toEqual({
            x: 10,
            y: 21,
            width: 301,
            height: 199,
        });
    });

    it("clamps a negative origin (sub-pixel layout offsets) to zero", () => {
        expect(toPreviewBounds({ left: -3, top: -0.4, width: 100, height: 50 })).toEqual({
            x: 0,
            y: 0,
            width: 100,
            height: 50,
        });
    });

    it("keeps a collapsed rect at zero instead of inventing a size", () => {
        expect(toPreviewBounds({ left: 0, top: 0, width: 0, height: 0 })).toEqual({
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });
    });
});

describe("sameBounds", () => {
    it("treats identical geometry as unchanged", () => {
        expect(
            sameBounds(
                { x: 1, y: 2, width: 3, height: 4 },
                { x: 1, y: 2, width: 3, height: 4 },
            ),
        ).toBe(true);
    });

    it("detects a one-pixel move — a resize drag must reach the webview", () => {
        expect(
            sameBounds(
                { x: 1, y: 2, width: 3, height: 4 },
                { x: 2, y: 2, width: 3, height: 4 },
            ),
        ).toBe(false);
    });
});

describe("isParkedBounds", () => {
    it("parks anything thinner than one pixel", () => {
        expect(isParkedBounds({ x: 0, y: 0, width: 0, height: 300 })).toBe(true);
        expect(isParkedBounds({ x: 0, y: 0, width: 300, height: 0 })).toBe(true);
    });

    it("does not park a real rect", () => {
        expect(isParkedBounds({ x: 0, y: 0, width: 1, height: 1 })).toBe(false);
    });
});