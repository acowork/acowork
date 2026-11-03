/**
 * The glue over pdf.js — page bounds, canvas sizing, release, error shape.
 *
 * pdf.js itself is NEVER loaded here: `createPdfPageRenderer` takes the loader
 * and the canvas factory precisely so this file can assert on what the picker
 * depends on (that a page renders into a canvas the page sized, that a bad
 * page number is refused before any work, that a destroyed picker destroyed
 * the document) without a worker, a parser, or a 2-D context.
 */
import { describe, expect, it, vi } from "vitest";
import {
    createPdfPageRenderer,
    PDF_OUTPUT_SCALE,
    type PdfPageDocument,
} from "./pdfRasterize";

/** A stand-in for the pdf.js document: it sizes the canvas it is handed, the
 *  way `render({ viewport })` does, and records every call. */
function stubDocument(numPages: number) {
    const renders: Array<{ pageNumber: number; scale: number }> = [];
    const doc: PdfPageDocument = {
        numPages,
        async renderPage(pageNumber, scale, canvas) {
            renders.push({ pageNumber, scale });
            // A4 at scale 2: the numbers only have to be distinguishable.
            canvas.width = Math.floor(595 * scale);
            canvas.height = Math.floor(842 * scale);
        },
        destroy: vi.fn(async () => {}),
    };
    return { doc, renders };
}

function stubCanvasFactory() {
    const create = vi.fn((width: number, height: number) => {
        const canvas = document.createElement("canvas");
        canvas.width = width;
        canvas.height = height;
        return canvas;
    });
    return { create };
}

const BYTES = new Uint8Array([0x25, 0x50, 0x44, 0x46]);

describe("createPdfPageRenderer", () => {
    it("reports the page count the document gave", async () => {
        const { doc } = stubDocument(7);
        const renderer = await createPdfPageRenderer(BYTES, async () => doc, stubCanvasFactory().create);
        expect(renderer.numPages).toBe(7);
    });

    it("renders at the output scale and returns the canvas the document sized", async () => {
        const { doc, renders } = stubDocument(3);
        const { create } = stubCanvasFactory();
        const renderer = await createPdfPageRenderer(BYTES, async () => doc, create);
        const canvas = await renderer.render(2);
        // The canvas is created unmeasured — the page's own viewport is the
        // only authority on its size — and `renderPage` writes that size in.
        expect(create).toHaveBeenCalledWith(0, 0);
        expect(renders).toEqual([{ pageNumber: 2, scale: PDF_OUTPUT_SCALE }]);
        expect(canvas.width).toBe(Math.floor(595 * PDF_OUTPUT_SCALE));
        expect(canvas.height).toBe(Math.floor(842 * PDF_OUTPUT_SCALE));
    });

    it("honours an explicit scale override", async () => {
        const { doc, renders } = stubDocument(3);
        const renderer = await createPdfPageRenderer(BYTES, async () => doc, stubCanvasFactory().create);
        await renderer.render(1, 1);
        expect(renders).toEqual([{ pageNumber: 1, scale: 1 }]);
    });

    it("refuses a page outside the document without rendering anything", async () => {
        const { doc, renders } = stubDocument(3);
        const renderer = await createPdfPageRenderer(BYTES, async () => doc, stubCanvasFactory().create);
        // 0 and numPages+1 are the two off-by-one edges; a fractional page has
        // no meaning to pdf.js and must not reach it either.
        await expect(renderer.render(0)).rejects.toThrow(/outside 1\.\.3/);
        await expect(renderer.render(4)).rejects.toThrow(/outside 1\.\.3/);
        await expect(renderer.render(1.5)).rejects.toThrow(/outside 1\.\.3/);
        expect(renders).toHaveLength(0);
    });

    it("propagates a failed page render as a rejection", async () => {
        const doc: PdfPageDocument = {
            numPages: 2,
            renderPage: vi.fn(async () => {
                throw new Error("cannot build canvas context");
            }),
            destroy: vi.fn(async () => {}),
        };
        const renderer = await createPdfPageRenderer(BYTES, async () => doc, stubCanvasFactory().create);
        // A rejected render, not a blank canvas: the caller turns it into a
        // toast rather than attaching an unreadable image.
        await expect(renderer.render(1)).rejects.toThrow("cannot build canvas context");
        expect(doc.renderPage).toHaveBeenCalledTimes(1);
    });

    it("propagates a failure to open the document", async () => {
        const load = vi.fn(async () => {
            throw new Error("Invalid PDF structure");
        });
        await expect(createPdfPageRenderer(BYTES, load, stubCanvasFactory().create)).rejects.toThrow(
            "Invalid PDF structure",
        );
    });

    it("destroys the document it opened", async () => {
        const { doc } = stubDocument(3);
        const renderer = await createPdfPageRenderer(BYTES, async () => doc, stubCanvasFactory().create);
        await renderer.destroy();
        expect(doc.destroy).toHaveBeenCalledTimes(1);
    });
});
