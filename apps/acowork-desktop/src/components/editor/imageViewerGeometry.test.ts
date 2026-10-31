// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.

import { describe, expect, it } from "vitest";
import {
    MAX_ZOOM,
    MIN_SELECTION_PX,
    MIN_ZOOM,
    ZOOM_STEP,
    clampPoint,
    clampZoom,
    dragSelectionRect,
    fitScale,
    normalizeDragRect,
    screenToImagePoint,
    wheelZoomFactor,
    zoomPercent,
    zoomAnchorDrift,
    zoomStepFactor,
} from "./imageViewerGeometry";

const SIZE = { width: 1000, height: 500 };

describe("clampZoom", () => {
    it("bounds the factor to the supported range", () => {
        expect(clampZoom(0.001)).toBe(MIN_ZOOM);
        expect(clampZoom(64)).toBe(MAX_ZOOM);
        expect(clampZoom(2.5)).toBe(2.5);
    });

    it("falls back to 1:1 when the measurement is unusable", () => {
        expect(clampZoom(Number.NaN)).toBe(1);
        expect(clampZoom(0)).toBe(1);
        expect(clampZoom(-3)).toBe(1);
    });
});

describe("fitScale", () => {
    it("shrinks a large image to fit the smaller dimension", () => {
        // 1000x500 image in a 400x400 viewport: width is the binding axis.
        expect(fitScale(SIZE, { width: 400, height: 400 })).toBeCloseTo(0.4);
    });

    it("never enlarges a small image past 1:1", () => {
        expect(fitScale({ width: 40, height: 20 }, { width: 800, height: 600 })).toBe(1);
    });

    it("returns 1 when the viewport has not been measured yet", () => {
        expect(fitScale(SIZE, { width: 0, height: 0 })).toBe(1);
        expect(fitScale({ width: 0, height: 0 }, { width: 400, height: 400 })).toBe(1);
    });
});

describe("screenToImagePoint", () => {
    it("divides the client offset by the scale", () => {
        // Image starts at (100, 50) and is drawn at 2x.
        expect(
            screenToImagePoint({ clientX: 300, clientY: 150 }, { left: 100, top: 50 }, 2),
        ).toEqual({ x: 100, y: 50 });
    });

    it("keeps the pixel stable at fractional scales instead of truncating", () => {
        // 0.5x: 1 screen px = 2 image px; rounding, not flooring.
        expect(
            screenToImagePoint({ clientX: 11, clientY: 11 }, { left: 0, top: 0 }, 0.5),
        ).toEqual({ x: 22, y: 22 });
    });

    it("reports out-of-bounds coordinates as-is", () => {
        expect(
            screenToImagePoint({ clientX: 0, clientY: 0 }, { left: 100, top: 100 }, 1),
        ).toEqual({ x: -100, y: -100 });
    });

    it("treats a broken scale as 1:1 rather than dividing by zero", () => {
        expect(
            screenToImagePoint({ clientX: 60, clientY: 60 }, { left: 10, top: 10 }, 0),
        ).toEqual({ x: 50, y: 50 });
    });
});

describe("clampPoint", () => {
    it("pulls a point back into the last addressable pixel", () => {
        expect(clampPoint({ x: 5000, y: -20 }, SIZE)).toEqual({ x: 999, y: 0 });
    });
});

describe("dragSelectionRect", () => {
    it("normalises a drag towards the top-left", () => {
        expect(dragSelectionRect({ x: 90, y: 40 }, { x: 10, y: 10 }, SIZE)).toEqual({
            x: 10,
            y: 10,
            width: 81,
            height: 31,
        });
    });

    it("counts the anchor pixel itself", () => {
        expect(dragSelectionRect({ x: 10, y: 10 }, { x: 20, y: 20 }, SIZE)).toEqual({
            x: 10,
            y: 10,
            width: 11,
            height: 11,
        });
    });

    it("clips a drag that leaves the image", () => {
        expect(dragSelectionRect({ x: 990, y: 490 }, { x: 1200, y: 900 }, SIZE)).toEqual({
            x: 990,
            y: 490,
            width: 10,
            height: 10,
        });
    });

    it("rejects a drag that wobbled instead of selecting", () => {
        const tiny = MIN_SELECTION_PX - 2;
        expect(dragSelectionRect({ x: 10, y: 10 }, { x: 10 + tiny, y: 10 }, SIZE)).toBeNull();
        expect(dragSelectionRect({ x: 10, y: 10 }, { x: 10, y: 10 + tiny }, SIZE)).toBeNull();
    });

    it("rejects any drag on an unmeasured image", () => {
        expect(dragSelectionRect({ x: 0, y: 0 }, { x: 50, y: 50 }, { width: 0, height: 0 })).toBeNull();
    });
});

describe("normalizeDragRect", () => {
    it("keeps a single-pixel preview visible while the pointer is down", () => {
        expect(normalizeDragRect({ x: 10, y: 10 }, { x: 10, y: 10 }, SIZE)).toEqual({
            x: 10,
            y: 10,
            width: 1,
            height: 1,
        });
    });

    it("clips the preview to the image bounds", () => {
        expect(normalizeDragRect({ x: 995, y: 495 }, { x: 1200, y: 900 }, SIZE)).toEqual({
            x: 995,
            y: 495,
            width: 5,
            height: 5,
        });
    });
});

describe("zoomAnchorDrift", () => {
    it("scrolls towards the cursor when zooming in", () => {
        // Origin pinned at the top-left, pointer 100px into the image, 1x -> 2x:
        // the point now sits 100px further right, so scroll by that much.
        expect(
            zoomAnchorDrift({
                pointer: { x: 100, y: 40 },
                originBefore: { x: 0, y: 0 },
                originAfter: { x: 0, y: 0 },
                prevScale: 1,
                nextScale: 2,
            }),
        ).toEqual({ x: 100, y: 40 });
    });

    it("scrolls back when zooming out", () => {
        expect(
            zoomAnchorDrift({
                pointer: { x: 100, y: 40 },
                originBefore: { x: 0, y: 0 },
                originAfter: { x: 0, y: 0 },
                prevScale: 2,
                nextScale: 1,
            }),
        ).toEqual({ x: -50, y: -20 });
    });

    it("accounts for an origin that moved because the element was re-centred", () => {
        // A centred 200px image in a 400px viewport: origin 100 before, 0 after
        // (it now fills the width). The point at client 150 lands at 100, so the
        // correction is negative — the caller's `scrollLeft` clamps it to 0,
        // which is the honest "nothing left to scroll" answer.
        expect(
            zoomAnchorDrift({
                pointer: { x: 150, y: 0 },
                originBefore: { x: 100, y: 0 },
                originAfter: { x: 0, y: 0 },
                prevScale: 1,
                nextScale: 2,
            }),
        ).toEqual({ x: -50, y: 0 });
    });

    it("is a no-op when the scale did not change or is unusable", () => {
        const base = {
            pointer: { x: 100, y: 40 },
            originBefore: { x: 0, y: 0 },
            originAfter: { x: 0, y: 0 },
        };
        expect(zoomAnchorDrift({ ...base, prevScale: 2, nextScale: 2 })).toEqual({ x: 0, y: 0 });
        expect(zoomAnchorDrift({ ...base, prevScale: 0, nextScale: 2 })).toEqual({ x: 0, y: 0 });
    });
});

describe("display helpers", () => {
    it("rounds the zoom percentage", () => {
        expect(zoomPercent(1)).toBe(100);
        expect(zoomPercent(0.3333)).toBe(33);
    });
});

describe("wheelZoomFactor", () => {
    it("treats a full mouse-wheel notch as one zoom step", () => {
        expect(wheelZoomFactor(-100)).toBeCloseTo(ZOOM_STEP);
        expect(wheelZoomFactor(100)).toBeCloseTo(1 / ZOOM_STEP);
    });

    it("interpolates a trackpad's small deltas", () => {
        // A fifth of a notch is a fifth of the exponent, not a full step.
        expect(wheelZoomFactor(-20)).toBeCloseTo(Math.pow(ZOOM_STEP, 0.2));
        expect(wheelZoomFactor(-20)).toBeLessThan(ZOOM_STEP);
        expect(wheelZoomFactor(-20)).toBeGreaterThan(1);
    });

    it("caps a single event at one notch", () => {
        expect(wheelZoomFactor(-5000)).toBeCloseTo(ZOOM_STEP);
        expect(wheelZoomFactor(5000)).toBeCloseTo(1 / ZOOM_STEP);
    });

    it("is a no-op for a missing delta", () => {
        expect(wheelZoomFactor(0)).toBe(1);
        expect(wheelZoomFactor(Number.NaN)).toBe(1);
    });
});

describe("zoomStepFactor", () => {
    it("multiplies in both directions", () => {
        expect(zoomStepFactor(1)).toBe(ZOOM_STEP);
        expect(zoomStepFactor(-1) * ZOOM_STEP).toBeCloseTo(1);
    });
});