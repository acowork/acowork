import { describe, expect, it } from "vitest";
import { SETI_ICONS } from "../../../assets/seti-icons";
import { getFileIcon } from "./fileIcons";

/**
 * Word / PowerPoint files used to fall through to the default icon while
 * spreadsheets were mapped. Both halves matter: a name in `EXT_MAP` whose SVG is
 * absent from `SETI_ICONS` renders the default glyph and the grey fallback colour
 * anyway (`SetiIcon` falls back on both), so the mapping alone is not the fix.
 */
const CASES = [
    ["report.docx", "word"],
    ["notes.doc", "word"],
    ["brief.docm", "word"],
    ["deck.pptx", "pptx"],
    ["legacy.ppt", "pptx"],
    ["macro.pptm", "pptx"],
    ["book.xlsx", "xls"], // already worked — guards the neighbouring group
] as const;

describe("getFileIcon — Office documents", () => {
    it.each(CASES)("maps %s to the %s icon", (filename, expected) => {
        expect(getFileIcon(filename)).toEqual({ name: expected });
    });

    it("has every mapped icon name in SETI_ICONS, so nothing falls back", () => {
        for (const [filename, expected] of CASES) {
            expect(SETI_ICONS[expected], `${filename} → ${expected}`).toBeTruthy();
            expect(SETI_ICONS[expected]).toContain("<svg");
        }
    });

    it("is case-insensitive and keeps unmapped extensions on the default icon", () => {
        expect(getFileIcon("REPORT.DOCX")).toEqual({ name: "word" });
        expect(getFileIcon("unknown.xyz")).toEqual({ name: "default" });
    });
});
