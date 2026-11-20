/**
 * Visual hosted-app preview in the editor pane (ADR-118).
 *
 * Pins that a `.figcraft` tab mounts AppFrame (not a chat bubble) with the
 * workspace file query, and that bytes from the workspace blob feed are
 * handed to the frame as openFile.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, waitFor } from "@testing-library/react";
import { VisualAppPreview } from "./VisualAppPreview";
import type { OpenFile } from "../../stores/fileEditorStore";
import type { AppDto } from "../../lib/apps-api";

const h = vi.hoisted(() => ({
    frames: [] as Array<{ app: AppDto; search?: string; openFile?: { name: string; bytes: Uint8Array } | null }>,
    listApps: vi.fn(),
    blob: {
        blob: null as Blob | null,
        blobUrl: null as string | null,
        mime: null as string | null,
        loading: false,
        error: null as string | null,
        retry: vi.fn(),
    },
}));

vi.mock("../apps/AppFrame", () => ({
    AppFrame: (props: { app: AppDto; search?: string; openFile?: { name: string; bytes: Uint8Array } | null }) => {
        h.frames.push(props);
        return <div data-testid="visual-app-frame" />;
    },
}));

vi.mock("../../lib/apps-api", () => ({
    listApps: () => h.listApps(),
}));

vi.mock("../../hooks/useWorkspaceFileBlob", () => ({
    useWorkspaceFileBlob: () => h.blob,
}));

vi.mock("../../i18n/useTranslation", () => ({
    useTranslation: () => ({ t: (k: string) => k }),
}));

const figcraftApp: AppDto = {
    id: "figcraft",
    name: "FigCraft (ACowork)",
    version: "0.18.2",
    description: "",
    icon: "figcraft",
    builtin: false,
    enabled: true,
    status: "running",
    port: 18090,
    runtime: "generic",
    mcp_path: "/mcp",
    mcp_gateway_path: "/api/apps/figcraft/mcp",
    mcp_url: null,
    ui_path: "/ui/",
    ui_mode: "webview",
    canvas_kind: "visual",
    canvas_preview_path: "/?file={workspace_file}",
    nav_label: "FigCraft",
    nav_icon: "figcraft",
    nav_order: 40,
    health_path: "/health",
    permissions: [],
};

function file(): OpenFile {
    return {
        id: "a1:ws1:designs/home.figcraft",
        agentId: "a1",
        workspaceId: "ws1",
        relPath: "designs/home.figcraft",
        fileName: "home.figcraft",
        content: "",
        originalContent: "",
        loading: false,
        saving: false,
        language: "plaintext",
        dirty: false,
        mode: "preview",
        kind: "file",
        mimeType: "application/x-figcraft",
        previewSeq: 0,
    };
}

beforeEach(() => {
    h.frames.length = 0;
    h.listApps.mockReset();
    h.listApps.mockResolvedValue([figcraftApp]);
    h.blob.blob = new Blob([new Uint8Array([1, 2, 3])]);
    h.blob.loading = false;
    h.blob.error = null;
});

describe("VisualAppPreview", () => {
    it("loads FigCraft AppFrame with the workspace file query", async () => {
        render(<VisualAppPreview file={file()} />);
        await waitFor(() => expect(h.frames.length).toBeGreaterThan(0));
        await waitFor(() => {
            const last = h.frames[h.frames.length - 1];
            expect(last?.app.id).toBe("figcraft");
            expect(last?.search).toContain("file=designs");
            expect(last?.search).toContain("control=1");
            expect(last?.openFile?.name).toBe("home.figcraft");
            expect(Array.from(last!.openFile!.bytes)).toEqual([1, 2, 3]);
        });
    });
});
