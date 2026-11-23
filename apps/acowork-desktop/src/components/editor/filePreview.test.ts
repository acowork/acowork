import { describe, expect, it } from "vitest";
import { isWorkspacePreviewable } from "./filePreview";

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
