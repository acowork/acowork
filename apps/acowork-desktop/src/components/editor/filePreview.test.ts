import { describe, expect, it } from "vitest";
import { isWorkspacePreviewable, pickOpenActionForPath } from "./filePreview";

describe("isWorkspacePreviewable", () => {
    it("includes figcraft visual-canvas files (ADR-121)", () => {
        expect(isWorkspacePreviewable("home.figcraft")).toBe(true);
        expect(isWorkspacePreviewable("designs/a.FIGCRAFT")).toBe(true);
        expect(isWorkspacePreviewable("talk.deckcraft")).toBe(true);
        expect(isWorkspacePreviewable("mark.vectorcraft")).toBe(true);
    });

    it("still previews images and documents", () => {
        expect(isWorkspacePreviewable("a.png")).toBe(true);
        expect(isWorkspacePreviewable("a.pdf")).toBe(true);
        expect(isWorkspacePreviewable("a.rs")).toBe(false);
    });

    it("previews photo and vector files a craft app can render (ADR-121)", () => {
        expect(isWorkspacePreviewable("shots/IMG_0421.CR3")).toBe(true);
        expect(isWorkspacePreviewable("a.dng")).toBe(true);
        expect(isWorkspacePreviewable("a.tiff")).toBe(true);
        expect(isWorkspacePreviewable("logo.ai")).toBe(true);
        expect(isWorkspacePreviewable("plan.dxf")).toBe(true);
        expect(isWorkspacePreviewable("a.svgo")).toBe(false);
    });
});

describe("pickOpenActionForPath", () => {
    it("previews what the panel previews: images, documents, craft-app files", () => {
        for (const p of [
            "runs/logo.png",
            "C:/w/a.JPG",
            "icon.svg",
            "report.pdf",
            "spec.docx",
            "sheet.xlsx",
            "shots/IMG_0421.CR3",
            "deck.deckcraft",
        ]) {
            expect(pickOpenActionForPath(p)).toBe("openPreview");
        }
    });

    it("sends source and data files to Monaco, which streams large ones", () => {
        for (const p of ["big.json", "main.rs", "agent.log", "notes.txt", "data.tsv"]) {
            expect(pickOpenActionForPath(p)).toBe("openFile");
        }
    });

    /**
     * Markdown is NOT a chat-side exception any more: the same file opened from
     * the workspace tree previews, so a message link that opened the source
     * gave one file two answers depending on where it was clicked.
     */
    it("previews Markdown, exactly like the workspace tree", () => {
        for (const p of ["notes.md", "docs/adr/ADR-119.md", "README.MD"]) {
            expect(isWorkspacePreviewable(p)).toBe(true);
            expect(pickOpenActionForPath(p)).toBe("openPreview");
        }
        // `.mdx` has no renderer in the panel either, so it stays on source.
        expect(pickOpenActionForPath("post.mdx")).toBe("openFile");
    });
});
