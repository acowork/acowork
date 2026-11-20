import { describe, expect, it } from "vitest";
import type { AppDto } from "../../lib/apps-api";
import {
    findVisualCanvasRelPath,
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
        nav_label: partial.id,
        nav_icon: "",
        nav_order: 40,
        health_path: "/health",
        permissions: [],
        ...partial,
    };
}

describe("visual canvas preview", () => {
    it("treats .figcraft as a visual workspace file", () => {
        expect(isVisualCanvasFile("home.figcraft")).toBe(true);
        expect(isVisualCanvasFile("HOME.FIGCRAFT")).toBe(true);
        expect(isVisualCanvasFile("home.png")).toBe(false);
    });

    it("builds a query with file + control and keeps the ticket out of search", () => {
        const q = visualPreviewSearch(app({ id: "figcraft" }), "designs/a.figcraft");
        expect(q.startsWith("?")).toBe(true);
        const params = new URLSearchParams(q.slice(1));
        expect(params.get("file")).toBe("designs/a.figcraft");
        expect(params.get("control")).toBe("1");
        expect(q).not.toContain("ticket");
    });

    it("picks the visual app for a figcraft file", () => {
        const picked = pickVisualApp(
            [app({ id: "pm", canvas_kind: "library", builtin: true }), app({ id: "figcraft" })],
            "a.figcraft",
        );
        expect(picked?.id).toBe("figcraft");
        expect(pickVisualApp([app({ id: "figcraft" })], "a.png")).toBeNull();
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
