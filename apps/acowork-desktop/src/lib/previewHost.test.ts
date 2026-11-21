/**
 * `downloadActionUrl` — which URL a refused download's action should target.
 *
 * The regression this pins: the first cut handed the download URL straight to the
 * system browser, so a page that exports a CSV or a PNG in memory (`blob:` URL)
 * produced a toast whose button did nothing — the shell plugin's default `open`
 * scope accepts `mailto:` / `tel:` / `http(s)` only, and the rejection was
 * swallowed. A page-generated download has no openable URL, so the page itself is
 * the target; with no usable target the action is omitted rather than dead.
 */
import { describe, expect, it } from "vitest";
import { downloadActionUrl } from "./previewHost";

const PAGE = "https://example.com/files";
const PDF = "https://example.com/report.pdf";

describe("downloadActionUrl", () => {
    it("uses the download URL when a browser can fetch it itself", () => {
        expect(downloadActionUrl(PDF, PAGE)).toBe(PDF);
    });

    it("falls back to the page for a page-generated blob", () => {
        expect(downloadActionUrl("blob:https://example.com/9f1e-2b", PAGE)).toBe(PAGE);
        expect(downloadActionUrl("blob:null/9f1e-2b", PAGE)).toBe(PAGE);
    });

    it("falls back to the page for a data: download", () => {
        expect(downloadActionUrl("data:text/csv,a,b", PAGE)).toBe(PAGE);
    });

    it("has no action at all when neither URL is openable", () => {
        expect(downloadActionUrl("blob:https://example.com/x", null)).toBeNull();
        expect(downloadActionUrl("blob:https://example.com/x", undefined)).toBeNull();
        // `about:blank` previews exist; they are not an openable target either.
        expect(downloadActionUrl("blob:https://example.com/x", "about:blank")).toBeNull();
    });

    it("accepts http as well as https, case-insensitively", () => {
        expect(downloadActionUrl("http://127.0.0.1:3000/out.zip", PAGE)).toBe(
            "http://127.0.0.1:3000/out.zip",
        );
        expect(downloadActionUrl("HTTPS://example.com/b.pdf", PAGE)).toBe("HTTPS://example.com/b.pdf");
    });
});