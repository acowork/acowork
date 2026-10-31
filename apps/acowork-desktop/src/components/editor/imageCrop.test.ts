// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.

import { describe, expect, it, vi } from "vitest";
import { cropImageRegion, regionFileName, type CanvasFactory } from "./imageCrop";

/** jsdom has no 2-D context, so the canvas is stubbed. What can actually be
 *  wrong here is the draw arguments and the blob plumbing — both are asserted. */
function stubCanvas(blob: Blob | null = new Blob(["png"], { type: "image/png" })) {
    const drawImage = vi.fn();
    const toBlob = vi.fn((cb: (b: Blob | null) => void) => cb(blob));
    const createCanvas: CanvasFactory = (width, height) =>
        ({
            width,
            height,
            getContext: () => ({ drawImage }),
            toBlob,
        }) as unknown as HTMLCanvasElement;
    return { createCanvas, drawImage, toBlob };
}

const SOURCE = { naturalWidth: 800, naturalHeight: 600 } as CanvasImageSource & {
    naturalWidth: number;
    naturalHeight: number;
};

describe("cropImageRegion", () => {
    it("draws the region 1:1 onto a canvas of the region's size", async () => {
        const { createCanvas, drawImage } = stubCanvas();
        const blob = await cropImageRegion(
            SOURCE,
            { x: 10, y: 20, width: 100, height: 50 },
            createCanvas,
        );
        expect(blob.type).toBe("image/png");
        expect(drawImage).toHaveBeenCalledWith(
            SOURCE,
            10, 20, 100, 50,
            0, 0, 100, 50,
        );
    });

    it("rejects an empty region instead of producing a zero-byte file", async () => {
        const { createCanvas } = stubCanvas();
        await expect(
            cropImageRegion(SOURCE, { x: 0, y: 0, width: 0, height: 10 }, createCanvas),
        ).rejects.toThrow(/empty region/);
    });

    it("rejects when the canvas cannot produce a blob", async () => {
        const { createCanvas } = stubCanvas(null);
        await expect(
            cropImageRegion(SOURCE, { x: 0, y: 0, width: 4, height: 4 }, createCanvas),
        ).rejects.toThrow(/toBlob/);
    });

    it("rejects when there is no 2-D context at all", async () => {
        const createCanvas: CanvasFactory = () =>
            ({ getContext: () => null, toBlob: () => {} }) as unknown as HTMLCanvasElement;
        await expect(
            cropImageRegion(SOURCE, { x: 0, y: 0, width: 4, height: 4 }, createCanvas),
        ).rejects.toThrow(/context unavailable/);
    });
});

describe("regionFileName", () => {
    it("appends the region size to the stem", () => {
        expect(regionFileName("logo.png", { x: 0, y: 0, width: 32, height: 16 })).toBe(
            "logo-region-32x16.png",
        );
    });

    it("keeps dotted stems intact", () => {
        expect(regionFileName("logo.v2.png", { x: 0, y: 0, width: 8, height: 8 })).toBe(
            "logo.v2-region-8x8.png",
        );
    });

    it("handles an extension-less name", () => {
        expect(regionFileName("screenshot", { x: 0, y: 0, width: 8, height: 8 })).toBe(
            "screenshot-region-8x8.png",
        );
    });
});