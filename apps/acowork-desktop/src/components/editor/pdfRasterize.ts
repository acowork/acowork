// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Rasterize a PDF page to a canvas — the one place that touches pdf.js.
//
// Why pdf.js at all: the preview itself is the WebView's native viewer (an
// `<iframe>`), which paints into a plugin surface we get no pixels back from.
// Cropping a region out of what the user saw therefore needs a second
// renderer, and it is only ever loaded for that gesture — `PdfViewer` lazy
// imports the picker, which imports this module, so a normal preview never
// pays for the ~1 MB engine (the P0 note in `PdfViewer.tsx` named this as the
// deliberate later step).
//
// Why the crop never reuses the native viewer's own geometry: we cannot read
// its page, its zoom or its scroll offset, so a rectangle over the iframe
// would have no defined mapping onto the document. The picker renders its own
// page and owns every coordinate in it — an honest approximation the user can
// see, rather than a guess about a surface we cannot observe.
//
// pdf.js is imported inside `loadPdfDocument` and the loader is injectable for
// the same reason `imageCrop.ts` injects a canvas factory: jsdom has no 2-D
// context and no way to parse a PDF, so the unit under test is the glue above
// it (page bounds, sizing, destroy), which the stub lets us pin exactly.

/**
 * Output resolution, as pdf.js viewport scale. A PDF point is 1/72 inch, so
 * `2` renders an A4 page at ~170 dpi — comfortably readable text for a model
 * while keeping one crop a few hundred KB. Display size is a separate number
 * (the picker scales the canvas with CSS), so this never changes what the user
 * framed.
 */
export const PDF_OUTPUT_SCALE = 2;

/** The slice of a pdf.js document this module needs. Keeps the stub honest. */
export interface PdfPageDocument {
    numPages: number;
    /** Draw page `pageNumber` (1-based) at `scale` onto `canvas`. */
    renderPage(pageNumber: number, scale: number, canvas: HTMLCanvasElement): Promise<void>;
    /** Release the worker + document. */
    destroy(): Promise<void>;
}

export type PdfDocumentLoader = (bytes: Uint8Array) => Promise<PdfPageDocument>;
export type RasterCanvasFactory = (width: number, height: number) => HTMLCanvasElement;

function defaultCanvasFactory(width: number, height: number): HTMLCanvasElement {
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    return canvas;
}

let workerSrcConfigured = false;

/**
 * Load pdf.js (legacy build) and open `bytes`.
 *
 * The legacy bundle is deliberate: the modern one targets `Promise.withResolvers`,
 * which older WKWebView builds do not have — and a preview gesture must not
 * depend on the newest engine the OS happens to ship.
 *
 * `bytes` stays the caller's afterwards: it is copied at the transfer point
 * below, so an array can be opened as many times as a re-running effect asks.
 */
async function loadPdfDocument(bytes: Uint8Array): Promise<PdfPageDocument> {
    const [pdfjs, workerUrl] = await Promise.all([
        import("pdfjs-dist/legacy/build/pdf.mjs"),
        import("pdfjs-dist/legacy/build/pdf.worker.min.mjs?url"),
    ]);
    if (!workerSrcConfigured) {
        pdfjs.GlobalWorkerOptions.workerSrc = workerUrl.default;
        workerSrcConfigured = true;
    }
    // pdf.js does not copy what it is given: it posts the document to its
    // worker with the underlying `ArrayBuffer` in the transfer list
    // (`sendWithPromise("GetDocRequest", docParams, [data.buffer])`), which
    // DETACHES it for the caller — `byteLength` drops to 0 and the next use
    // throws `DataCloneError: An ArrayBuffer is detached`. So the copy is
    // taken here, at the transfer point: whoever invalidates a buffer has to
    // own it. Leaving it to the callers is what broke the framed screenshot —
    // `PdfRegionPicker` opens from an effect, an effect may re-run over the
    // same `bytes` prop (a remount, React StrictMode's dev double-invoke),
    // and the second open was handed a buffer the first one had already given
    // away. A few MB copied once per gesture is the whole cost.
    const loading = pdfjs.getDocument({ data: bytes.slice(0) as Uint8Array<ArrayBuffer> });
    const doc = await loading.promise;
    return {
        numPages: doc.numPages,
        async renderPage(pageNumber, scale, canvas) {
            const page = await doc.getPage(pageNumber);
            const viewport = page.getViewport({ scale });
            canvas.width = Math.floor(viewport.width);
            canvas.height = Math.floor(viewport.height);
            await page.render({ canvas, viewport }).promise;
        },
        // The loading task owns the worker; `PDFDocumentProxy` itself only
        // exposes `cleanup()`, which frees the page caches but leaves the
        // transport — and its worker — running. Destroying the task ends both.
        destroy: () => doc.loadingTask.destroy(),
    };
}

export interface PdfPageRenderer {
    /** Page count as the document reported it (≥ 1). */
    numPages: number;
    /**
     * A canvas holding page `pageNumber` at `scale`.
     * Rejects on a bad page number or a failed render — the caller turns that
     * into a toast rather than silently attaching a blank image.
     */
    render(pageNumber: number, scale?: number): Promise<HTMLCanvasElement>;
    destroy(): Promise<void>;
}

/**
 * Open a PDF for on-demand page rasterization.
 *
 * `load` / `createCanvas` are injectable for the reason spelled out at the top
 * of the file; production always takes the defaults.
 */
export async function createPdfPageRenderer(
    bytes: Uint8Array,
    load: PdfDocumentLoader = loadPdfDocument,
    createCanvas: RasterCanvasFactory = defaultCanvasFactory,
): Promise<PdfPageRenderer> {
    const doc = await load(bytes);
    return {
        numPages: doc.numPages,
        async render(pageNumber: number, scale = PDF_OUTPUT_SCALE) {
            if (!Number.isInteger(pageNumber) || pageNumber < 1 || pageNumber > doc.numPages) {
                throw new Error(`Page ${pageNumber} is outside 1..${doc.numPages}`);
            }
            // Size the canvas from the page itself: pdf.js writes
            // `viewport.width/height` back into the canvas we hand it, and an
            // unsized 300×150 default would silently crop the page.
            const canvas = createCanvas(0, 0);
            await doc.renderPage(pageNumber, scale, canvas);
            return canvas;
        },
        destroy: () => doc.destroy(),
    };
}
