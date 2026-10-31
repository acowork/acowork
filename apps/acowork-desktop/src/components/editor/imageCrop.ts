// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Crop a rectangle out of an already-decoded image and hand back PNG bytes.
//
// Why draw from the live `<img>` element rather than from the data URL again:
// the element in the workspace preview IS the decoded bitmap, so a second
// `new Image()` would decode the same megabytes a second time for no gain. The
// source is a `data:` URL (the Gateway returns base64 for binary files — see
// `workspace_query_impl::BINARY_EXTENSIONS`), which is same-origin as far as
// the canvas is concerned, so reading the pixels back never taints it.
//
// The canvas constructor is injectable because jsdom has no 2-D context: the
// test supplies a stub and asserts on the draw call, which is the only part of
// this function that can be wrong. The production path is the one-liner below.
//
// Output is always PNG: a crop of a JPEG or GIF re-encodes losslessly, so the
// region the model receives is exactly the pixels the user framed.

import type { ImageRect } from "./imageViewerGeometry";

/** Anything `<img>`-shaped. Narrow on purpose so a test stub type-checks. */
export interface CropSource {
    naturalWidth: number;
    naturalHeight: number;
}

export type CanvasFactory = (width: number, height: number) => HTMLCanvasElement;

function defaultCanvasFactory(width: number, height: number): HTMLCanvasElement {
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    return canvas;
}

/**
 * Draw `region` (image pixels) of `source` onto a fresh canvas and resolve
 * with its PNG blob.
 *
 * Rejects rather than resolving an empty blob: the caller turns a rejection
 * into a toast, and a zero-byte attachment would instead surface later as an
 * unreadable file in the transcript.
 */
export async function cropImageRegion(
    source: CanvasImageSource & CropSource,
    region: ImageRect,
    createCanvas: CanvasFactory = defaultCanvasFactory,
): Promise<Blob> {
    if (region.width < 1 || region.height < 1) {
        throw new Error(`Refusing to crop an empty region (${region.width}x${region.height})`);
    }
    const canvas = createCanvas(region.width, region.height);
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("2D canvas context unavailable — cannot crop the image");
    // Destination is the region's own size: a crop never rescales, so the
    // image pixel the user picked maps 1:1 onto the PNG the model receives.
    ctx.drawImage(
        source,
        region.x,
        region.y,
        region.width,
        region.height,
        0,
        0,
        region.width,
        region.height,
    );
    return await new Promise<Blob>((resolve, reject) => {
        canvas.toBlob(
            (blob) => (blob ? resolve(blob) : reject(new Error("canvas.toBlob returned null"))),
            "image/png",
        );
    });
}

/**
 * Name for the cropped attachment: the source's stem plus `-region.png`.
 *
 * The name matters — it is what the transcript chip and the model's file list
 * show, and it is the only cue tying the crop back to the image it came from.
 * Extension handling covers dotted stems (`logo.v2.png` → `logo.v2-region.png`)
 * and names with no extension at all.
 */
export function regionFileName(fileName: string, region: ImageRect): string {
    const stem = fileName.replace(/\.[^./\\]+$/, "") || fileName;
    return `${stem}-region-${region.width}x${region.height}.png`;
}