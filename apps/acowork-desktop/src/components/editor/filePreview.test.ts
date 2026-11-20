import { describe, expect, it } from "vitest";
import { isWorkspacePreviewable } from "./filePreview";

describe("isWorkspacePreviewable", () => {
    it("includes figcraft visual-canvas files (ADR-118)", () => {
        expect(isWorkspacePreviewable("home.figcraft")).toBe(true);
        expect(isWorkspacePreviewable("designs/a.FIGCRAFT")).toBe(true);
    });

    it("still previews images and documents", () => {
        expect(isWorkspacePreviewable("a.png")).toBe(true);
        expect(isWorkspacePreviewable("a.pdf")).toBe(true);
        expect(isWorkspacePreviewable("a.rs")).toBe(false);
    });
});
