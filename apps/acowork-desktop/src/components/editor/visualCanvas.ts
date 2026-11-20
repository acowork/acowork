/**
 * Visual hosted-app preview (ADR-118): workspace files such as `.figcraft`
 * that need the app iframe rather than Monaco / ImageViewer.
 */
import type { AppDto } from "../../lib/apps-api";
import type { ChatMessage } from "../../lib/types";

/** Extensions whose workspace preview is a hosted visual-app iframe. */
export const VISUAL_CANVAS_EXT_RE = /\.figcraft$/i;

export function isVisualCanvasFile(name: string): boolean {
    return VISUAL_CANVAS_EXT_RE.test(name);
}

/** Sibling thumbnail written by the hosted-app adapter after `screenshot`. */
export function previewPngRelPath(figcraftRel: string): string {
    return figcraftRel.replace(/\.figcraft$/i, ".preview.png");
}

/**
 * Workspace-relative path from a tool argument. LLM args are relative;
 * an absolute path is reduced to its filename so we never send a host
 * path to the workspace-files API.
 */
export function visualCanvasRelFromPath(path: string): string | null {
    const n = path.replace(/\\/g, "/").trim();
    if (!n) return null;
    const abs = n.startsWith("/") || /^[A-Za-z]:\//.test(n);
    const rel = abs ? (n.split("/").pop() ?? "") : n.replace(/^\/+/, "");
    return isVisualCanvasFile(rel) ? rel : null;
}

/** Last `.figcraft` `path` among tool_calls that already succeeded. */
export function findVisualCanvasRelPath(items: ChatMessage[]): string | null {
    const successIds = new Set<string>();
    for (const item of items) {
        if (item.type === "tool_result" && item.toolStatus === "success" && item.toolCallId) {
            successIds.add(item.toolCallId);
        }
    }
    let last: string | null = null;
    for (const item of items) {
        if (item.type !== "tool_call") continue;
        if (!item.toolCallId || !successIds.has(item.toolCallId)) continue;
        try {
            const params = JSON.parse(item.content) as { path?: unknown };
            if (typeof params.path !== "string") continue;
            const rel = visualCanvasRelFromPath(params.path);
            if (rel) last = rel;
        } catch {
            // Tool args are not always JSON (shell text, truncated history).
        }
    }
    return last;
}

export function visualCanvasShotSeq(items: ChatMessage[]): number {
    let n = 0;
    for (const item of items) {
        if (item.type !== "tool_result") continue;
        if (item.toolStatus !== "success") continue;
        const name = (item.toolName ?? "").toLowerCase();
        if (name === "screenshot" || name.endsWith("__screenshot")) n += 1;
    }
    return n;
}

const autoOpened = new Set<string>();

export function resetVisualCanvasAutoOpenForTests(): void {
    autoOpened.clear();
}

/**
 * First write of a visual canvas file in the selected agent's current
 * workspace opens the editor preview once. Later saves of the same path
 * do not steal focus.
 */
export function shouldAutoOpenVisualCanvas(input: {
    kind: string;
    path: string;
    instanceId: string;
    workspaceId: string;
    selectedAgentId: string | null;
    activeWorkspaceId: string | null;
}): boolean {
    const key = `${input.instanceId}\0${input.workspaceId}\0${input.path}`;
    if (input.kind === "deleted") {
        autoOpened.delete(key);
        return false;
    }
    if (input.kind !== "created" && input.kind !== "modified") return false;
    if (!isVisualCanvasFile(input.path)) return false;
    if (!input.selectedAgentId || input.selectedAgentId !== input.instanceId) return false;
    if (!input.activeWorkspaceId || input.activeWorkspaceId !== input.workspaceId) return false;
    if (autoOpened.has(key)) return false;
    autoOpened.add(key);
    return true;
}

/**
 * Query string appended to the app `ui_path`. Ticket stays in the fragment.
 * `control=1` opts into `window.figcraft` so the adapter-injected bridge can
 * call `openFile` / theme commands.
 */
export function visualPreviewSearch(app: AppDto, relPath: string): string {
    const tmpl = (app.canvas_preview_path || "?file={workspace_file}").trim();
    const filled = tmpl.replaceAll("{workspace_file}", relPath);
    const qIndex = filled.indexOf("?");
    const raw = qIndex >= 0 ? filled.slice(qIndex + 1) : filled.replace(/^\//, "");
    const params = new URLSearchParams(raw);
    params.set("file", relPath);
    if (!params.has("control")) params.set("control", "1");
    return `?${params.toString()}`;
}

/** Prefer a `canvas.kind = visual` app; FigCraft is the first of those. */
export function pickVisualApp(apps: AppDto[], fileName: string): AppDto | null {
    if (!isVisualCanvasFile(fileName)) return null;
    const visual = apps.find((a) => (a.canvas_kind || "").toLowerCase() === "visual" && a.enabled);
    if (visual) return visual;
    return apps.find((a) => a.id === "figcraft" && a.enabled) ?? null;
}
