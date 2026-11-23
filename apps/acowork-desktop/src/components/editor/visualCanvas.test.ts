import { describe, expect, it } from "vitest";
import type { AppDto } from "../../lib/apps-api";
import {
    findVisualCanvasRelPath,
    isHostedPreviewCandidate,
    isVisualCanvasFile,
    pickVisualApp,
    previewPngRelPath,
    resetVisualCanvasAutoOpenForTests,
    shouldAutoOpenVisualCanvas,
    visualCanvasRelFromPath,
    visualCanvasShotSeq,
    visualPreviewSearch,
} from "./visualCanvas";
import type { ChatMessage } from "../../lib/types";

function app(partial: Partial<AppDto> & Pick<AppDto, "id">): AppDto {
    return {
        name: partial.id,
        version: "1.0.0",
        description: "",
        icon: "",
        builtin: false,
        enabled: true,
        status: "ready",
        port: 18090,
        runtime: "generic",
        mcp_path: "/mcp",
        mcp_gateway_path: `/api/apps/${partial.id}/mcp`,
        mcp_url: null,
        ui_path: "/ui/",
        ui_mode: "webview",
        canvas_kind: "visual",
        canvas_preview_path: "/?file={workspace_file}",
        canvas_extensions: partial.id === "figcraft" ? ["figcraft"] : [],
        nav_label: partial.id,
        nav_icon: "",
        nav_order: 40,
        health_path: "/health",
        permissions: [],
        ...partial,
    };
}

describe("visual canvas preview", () => {
    it("treats branded craft files as visual workspace files", () => {
        expect(isVisualCanvasFile("home.figcraft")).toBe(true);
        expect(isVisualCanvasFile("HOME.FIGCRAFT")).toBe(true);
        expect(isVisualCanvasFile("talk.deckcraft")).toBe(true);
        expect(isVisualCanvasFile("mark.vectorcraft")).toBe(true);
        expect(isVisualCanvasFile("home.png")).toBe(false);
        expect(isVisualCanvasFile("letter.docx")).toBe(false);
    });

    it("builds a query with file + control and keeps the ticket out of search", () => {
        const q = visualPreviewSearch(app({ id: "figcraft" }), "designs/a.figcraft");
        expect(q.startsWith("?")).toBe(true);
        const params = new URLSearchParams(q.slice(1));
        expect(params.get("file")).toBe("designs/a.figcraft");
        expect(params.get("control")).toBe("1");
        expect(q).not.toContain("ticket");
    });

    it("keeps the query stable across documents when the app does not want a path", () => {
        // The document reaches the app through `openFile`. A per-file query
        // would change the iframe src, reload the frame, and make the app load
        // its fonts again for every document — see the font hand-over.
        const plain = app({ id: "wordcraft", canvas_preview_path: "" });
        const a = visualPreviewSearch(plain, "reports/a.docx");
        const b = visualPreviewSearch(plain, "reports/b.docx");
        expect(a).toBe(b);
        expect(new URLSearchParams(a.slice(1)).get("control")).toBe("1");
    });

    it("picks the visual app for a figcraft file", () => {
        const picked = pickVisualApp(
            [app({ id: "pm", canvas_kind: "library", builtin: true }), app({ id: "figcraft" })],
            "a.figcraft",
        );
        expect(picked?.id).toBe("figcraft");
        expect(pickVisualApp([app({ id: "figcraft" })], "a.png")).toBeNull();
    });

    it("routes office/pdf files to the visual app that declared them", () => {
        const word = app({ id: "wordcraft", canvas_extensions: ["docx", "doc"], nav_order: 41 });
        const pdf = app({ id: "pdfcraft", canvas_extensions: ["pdf"], nav_order: 44 });
        const fig = app({ id: "figcraft", canvas_extensions: ["figcraft"], nav_order: 40 });
        expect(pickVisualApp([fig, word, pdf], "a.docx")?.id).toBe("wordcraft");
        expect(pickVisualApp([fig, word, pdf], "a.pdf")?.id).toBe("pdfcraft");
        expect(pickVisualApp([fig, word, pdf], "a.figcraft")?.id).toBe("figcraft");
        expect(pickVisualApp([fig], "a.docx")).toBeNull();
        expect(pickVisualApp([word], "a.figcraft")).toBeNull();
    });

    it("routes photos to LightCraft and vector art to VectorCraft", () => {
        const light = app({
            id: "lightcraft",
            canvas_extensions: ["dng", "cr3", "jpg", "png"],
            nav_order: 46,
        });
        const vector = app({
            id: "vectorcraft",
            canvas_extensions: ["svg", "ai", "eps"],
            nav_order: 45,
        });
        const apps = [light, vector];
        expect(pickVisualApp(apps, "shots/IMG_0421.CR3")?.id).toBe("lightcraft");
        expect(pickVisualApp(apps, "a.dng")?.id).toBe("lightcraft");
        expect(pickVisualApp(apps, "photo.png")?.id).toBe("lightcraft");
        expect(pickVisualApp(apps, "logo.svg")?.id).toBe("vectorcraft");
        expect(pickVisualApp(apps, "logo.ai")?.id).toBe("vectorcraft");
        // A disabled app claims nothing: the built-in viewer stays in charge.
        expect(pickVisualApp([{ ...light, enabled: false }], "a.dng")).toBeNull();
        // Two apps on one extension: nav order, then id — never a coin flip.
        const a = app({ id: "alpha", canvas_extensions: ["png"], nav_order: 50 });
        const b = app({ id: "beta", canvas_extensions: ["png"], nav_order: 50 });
        expect(pickVisualApp([a, b], "x.png")?.id).toBe("alpha");
        expect(pickVisualApp([b, a], "x.png")?.id).toBe("alpha");
    });

    it("counts photo and vector files as hosted-preview candidates", () => {
        expect(isHostedPreviewCandidate("a.dng")).toBe(true);
        expect(isHostedPreviewCandidate("a.docx")).toBe(true);
        expect(isHostedPreviewCandidate("notes.md")).toBe(false);
        expect(isHostedPreviewCandidate("a.png")).toBe(false);
    });

    it("maps a figcraft path to its sibling preview png", () => {
        expect(previewPngRelPath("designs/Home.figcraft")).toBe("designs/Home.preview.png");
    });

    it("accepts relative figcraft tool paths and drops host prefixes", () => {
        expect(visualCanvasRelFromPath("designs/a.figcraft")).toBe("designs/a.figcraft");
        expect(visualCanvasRelFromPath("C:/Users/me/ws/a.figcraft")).toBe("a.figcraft");
        expect(visualCanvasRelFromPath("notes.md")).toBeNull();
    });

    it("finds the last figcraft path in an explore group", () => {
        const items: ChatMessage[] = [
            {
                id: "1",
                type: "tool_call",
                content: JSON.stringify({ path: "designs/a.figcraft" }),
                timestamp: 1,
                toolName: "mcp_figcraft__save_file",
                toolCallId: "a",
            },
            {
                id: "1r",
                type: "tool_result",
                content: "{}",
                timestamp: 2,
                toolName: "mcp_figcraft__save_file",
                toolCallId: "a",
                toolStatus: "success",
            },
            {
                id: "2",
                type: "tool_call",
                content: JSON.stringify({ path: "designs/b.figcraft" }),
                timestamp: 3,
                toolName: "mcp_figcraft__save_file",
                toolCallId: "b",
            },
            {
                id: "2r",
                type: "tool_result",
                content: "{}",
                timestamp: 4,
                toolName: "mcp_figcraft__save_file",
                toolCallId: "b",
                toolStatus: "success",
            },
        ];
        expect(findVisualCanvasRelPath(items)).toBe("designs/b.figcraft");
        expect(findVisualCanvasRelPath(items.slice(0, 1))).toBeNull();
        expect(visualCanvasShotSeq([
            {
                id: "3",
                type: "tool_result",
                content: "",
                timestamp: 3,
                toolName: "mcp_figcraft__screenshot",
                toolStatus: "success",
            },
        ])).toBe(1);
    });

    it("auto-opens a visual canvas file once per agent/workspace/path", () => {
        resetVisualCanvasAutoOpenForTests();
        const base = {
            path: "home.figcraft",
            instanceId: "agent-1",
            workspaceId: "ws-1",
            selectedAgentId: "agent-1",
            activeWorkspaceId: "ws-1",
        };
        expect(shouldAutoOpenVisualCanvas({ ...base, kind: "created" })).toBe(true);
        expect(shouldAutoOpenVisualCanvas({ ...base, kind: "modified" })).toBe(false);
        expect(shouldAutoOpenVisualCanvas({ ...base, kind: "created", path: "other.md" })).toBe(false);
        expect(shouldAutoOpenVisualCanvas({ ...base, kind: "created", selectedAgentId: "other" })).toBe(false);
        expect(shouldAutoOpenVisualCanvas({ ...base, kind: "deleted" })).toBe(false);
        expect(shouldAutoOpenVisualCanvas({ ...base, kind: "created" })).toBe(true);
    });
});
