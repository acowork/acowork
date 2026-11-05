import { describe, expect, it } from "vitest";
import { blobToBase64, extensionForFile, extensionForMime } from "./clipboardImageExt";

describe("extensionForMime", () => {
  it("maps known raster MIMEs to lowercase extensions without a dot", () => {
    // The known-good set covers what WebView2 actually surfaces from
    // CF_BITMAP / CF_PNG (PNG), Snipaste (PNG), ShareX (PNG/BMP),
    // and the Win11 Snipping Tool (PNG). The runtime's `upload_file`
    // pipeline uses the extension to pick the image vs document code
    // path, so PNG specifically MUST be stable.
    expect(extensionForMime("image/png")).toBe("png");
    expect(extensionForMime("image/jpeg")).toBe("jpg");
    expect(extensionForMime("image/jpg")).toBe("jpg");
    expect(extensionForMime("image/gif")).toBe("gif");
    expect(extensionForMime("image/webp")).toBe("webp");
    expect(extensionForMime("image/bmp")).toBe("bmp");
  });

  it("is case-insensitive on the MIME", () => {
    // Some clipboard sources (third-party screen capture) serialise
    // the MIME with the subtype in upper case. We accept it without
    // a roundtrip through a lowercasing call site.
    expect(extensionForMime("IMAGE/PNG")).toBe("png");
    expect(extensionForMime("Image/Jpeg")).toBe("jpg");
  });

  it("falls back to the subtype sanitised for unknown raster MIMEs", () => {
    // e.g. "image/heic" → "heic", "image/x-portable-bitmap" → "xportablebitmap"
    // (the last one is intentional — we strip `-` and keep the prefix
    // so the result stays filename-safe; the runtime treats unknown
    // extensions as opaque image blobs anyway).
    expect(extensionForMime("image/heic")).toBe("heic");
  });

  it("returns png as the ultimate fallback so a broken MIME never produces an empty extension", () => {
    // The Rust writer refuses an empty extension and substitutes
    // ".bin", but ".bin" would then mis-classify the upload as a
    // document. A non-empty image extension keeps the right branch.
    expect(extensionForMime("not-a-mime")).toBe("png");
    expect(extensionForMime("")).toBe("png");
    expect(extensionForMime("image/")).toBe("png");
  });
});

describe("extensionForFile", () => {
  it("takes the extension off the name, which is what the runtime classifies by", () => {
    // The bridge hands over a `File` the sender named, and `upload_file` reads
    // the extension to pick the image vs document path. A PDF sent from the
    // viewer MUST arrive as a document, whatever its MIME says.
    expect(extensionForFile(new File(["%PDF"], "report.pdf", { type: "application/pdf" }))).toBe("pdf");
    expect(extensionForFile(new File([""], "slide-1.PNG", { type: "image/png" }))).toBe("png");
    // `image/jpeg` would have answered "jpg"; the name the sender chose wins,
    // because that name is what the transcript chip and the agent both see.
    expect(extensionForFile(new File([""], "clip.jpeg", { type: "image/jpeg" }))).toBe("jpeg");
  });

  it("reads the final suffix of a dotted name", () => {
    // A crop is named after its source, dotted stems included.
    expect(extensionForFile(new File([""], "logo.v2-region-10x8.png", { type: "image/png" }))).toBe("png");
    expect(extensionForFile(new File([""], "archive.tar.gz", { type: "application/gzip" }))).toBe("gz");
    expect(extensionForFile(new File([""], "notes.txt", { type: "text/plain" }))).toBe("txt");
  });

  it("falls back to the MIME only when the name carries no extension", () => {
    // A clipboard blob has no name at all — the case `extensionForMime` was
    // written for, kept reachable rather than replaced.
    expect(extensionForFile(new File([""], "", { type: "image/webp" }))).toBe("webp");
    expect(extensionForFile(new File([""], "no-extension", { type: "image/gif" }))).toBe("gif");
    // An unstructured MIME with no name has nothing better to offer than the
    // raster default, which is what keeps it on the image branch.
    expect(extensionForFile(new File([""], "no-extension"))).toBe("png");
  });

  it("refuses a suffix too long to be an extension", () => {
    // "report.final.2024notes" is a stem that happens to contain a dot, not a
    // file type; taking it would invent an extension the runtime cannot
    // classify. The MIME gets to answer instead.
    expect(extensionForFile(new File([""], "report.final.2024notes", { type: "image/png" }))).toBe("png");
  });
});

describe("blobToBase64", () => {
  it("round-trips bytes losslessly and strips the data-URL prefix", async () => {
    // PNG magic + arbitrary binary, including bytes that make naive
    // string conversions disagree (0x00, 0xff) and a value that would
    // break if the prefix split used the base64 payload instead of the
    // header (`,` never appears in the alphabet, so indexOf is safe).
    const bytes = new Uint8Array([
      0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff, 0x7f, 0x01,
    ]);
    const blob = new Blob([bytes], { type: "image/png" });

    const b64 = await blobToBase64(blob);

    expect(b64).not.toContain("data:");
    expect(b64).not.toContain(",");
    const back = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    expect(Array.from(back)).toEqual(Array.from(bytes));
  });

  it("handles the empty blob without throwing", async () => {
    // An empty clipboard read should surface as a zero-byte temp file
    // decision made by the caller, not a rejection from the encoder.
    const b64 = await blobToBase64(new Blob([], { type: "image/png" }));
    expect(b64).toBe("");
  });

  it("scales past the btoa spread-argument limit", async () => {
    // Regression guard for the reason this helper exists: a 2 MB buffer
    // is well beyond what `btoa(String.fromCharCode(...arr))` survives,
    // while readAsDataURL is size-agnostic.
    const size = 2 * 1024 * 1024;
    const bytes = new Uint8Array(size);
    for (let i = 0; i < size; i += 1) bytes[i] = i % 251;

    const b64 = await blobToBase64(new Blob([bytes], { type: "application/octet-stream" }));
    const back = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));

    expect(back.length).toBe(size);
    expect(back[0]).toBe(0);
    expect(back[size - 1]).toBe((size - 1) % 251);
  });
});
