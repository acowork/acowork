/**
 * The glue over pdf.js — page bounds, canvas sizing, release, error shape,
 * and the buffer invariant.
 *
 * pdf.js itself is NEVER loaded here: `createPdfPageRenderer` takes the loader
 * and the canvas factory precisely so this file can assert on what the picker
 * depends on (that a page renders into a canvas the page sized, that a bad
 * page number is refused before any work, that a destroyed picker destroyed
 * the document) without a worker, a parser, or a 2-D context.
 *
 * The one thing an injected loader cannot show is what the REAL loader does to
 * the caller's bytes, so the last block mocks the pdf.js module itself and
 * lets a real `structuredClone` transfer eat a real buffer: `getDocument` posts
 * the document with the caller's `ArrayBuffer` in the transfer list
 * (`sendWithPromise("GetDocRequest", docParams, [data.buffer])`), which
 * DETACHES it — `byteLength` drops to 0 and the next use throws
 * `DataCloneError: An ArrayBuffer is detached`. `PdfRegionPicker` opens from an
 * effect, and an effect may re-run over the same `bytes` prop (a remount,
 * React StrictMode's dev double-invoke), so the copy has to live at the
 * transfer point. Pinning that here is what keeps a framed screenshot from
 * answering "Could not read this PDF" while the native preview beside it is
 * perfectly fine.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
    createPdfPageRenderer,
    PDF_OUTPUT_SCALE,
    type PdfPageDocument,
} from "./pdfRasterize";

// ── The real loader, over a fake pdf.js ────────────────────────────
// Only the last block needs these; the mocks are hoisted, so they are inert
// for every test that injects its own loader.
const pdf = vi.hoisted(() => ({
    getDocument: vi.fn(),
    GlobalWorkerOptions: { workerSrc: "" },
}));

vi.mock("pdfjs-dist/legacy/build/pdf.mjs", () => ({
    GlobalWorkerOptions: pdf.GlobalWorkerOptions,
    getDocument: (params: { data: Uint8Array }) => pdf.getDocument(params),
}));
vi.mock("pdfjs-dist/legacy/build/pdf.worker.min.mjs?url", () => ({
    default: "http://localhost/pdf.worker.min.mjs",
}));

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

describe("the buffer handed to the real pdf.js loader", () => {
    /** What pdf.js received, captured BEFORE its transfer eats the buffer — a
     *  detached array cannot be read back by an assertion. */
    let handed: { buffer: ArrayBuffer; bytes: number[] }[] = [];

    /** Fresh bytes per test: a transfer invalidates the buffer it ate, so no
     *  test may inherit another test's detached array. */
    function pdfBytes(): Uint8Array {
        return new Uint8Array([0x25, 0x50, 0x44, 0x46, 1, 2, 3, 4]);
    }

    /** Behave exactly like pdf.js 6.x: transfer the buffer it is given. */
    beforeEach(() => {
        handed = [];
        pdf.getDocument.mockReset();
        pdf.getDocument.mockImplementation(({ data }: { data: Uint8Array }) => {
            handed.push({ buffer: data.buffer, bytes: Array.from(data) });
            // The real call is `sendWithPromise(…, [data.buffer])`. Handing
            // over an already-detached buffer is the bug under test: the
            // browser throws here and the picker shows its failure body.
            structuredClone({ docParams: { data } }, { transfer: [data.buffer] });
            const doc = {
                numPages: 3,
                getPage: async (pageNumber: number) => ({
                    pageNumber,
                    getViewport: ({ scale }: { scale: number }) => ({
                        width: 400 * scale,
                        height: 300 * scale,
                    }),
                    render: () => ({ promise: Promise.resolve() }),
                }),
                loadingTask: { destroy: () => Promise.resolve() },
            };
            return { promise: Promise.resolve(doc) };
        });
    });

    it("leaves the caller's bytes usable after one open", async () => {
        const bytes = pdfBytes();

        const renderer = await createPdfPageRenderer(bytes);

        expect(renderer.numPages).toBe(3);
        // The whole point: the array the caller kept is not the one pdf.js ate.
        expect(bytes.byteLength).toBe(8);
        await renderer.destroy();
    });

    it("opens the same bytes twice, because an effect may re-run", async () => {
        const bytes = pdfBytes();

        const first = await createPdfPageRenderer(bytes);
        await first.destroy();
        // Without a copy at the transfer point this rejects with DataCloneError.
        const second = await createPdfPageRenderer(bytes);

        expect(handed).toHaveLength(2);
        expect(second.numPages).toBe(3);
        await second.destroy();
    });

    it("hands pdf.js a buffer that is not the caller's", async () => {
        const bytes = pdfBytes();

        const renderer = await createPdfPageRenderer(bytes);

        expect(handed).toHaveLength(1);
        expect(handed[0].buffer).not.toBe(bytes.buffer);
        expect(handed[0].bytes).toEqual(Array.from(bytes));
        await renderer.destroy();
    });
});
