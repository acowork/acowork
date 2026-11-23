/**
 * Visual hosted-app preview (ADR-118): workspace files whose preview is
 * the hosted-app iframe rather than Monaco / ImageViewer / the built-in
 * document viewers.
 *
 * Routing is data-driven: each visual app declares `canvas_extensions`.
 * The `figcraft` id still matches `.figcraft` when that list is empty so
 * already-installed FigCraft packages keep working. An enabled visual app
 * that lists `docx` / `xlsx` / `pptx` / `pdf` takes those over from the
 * built-in viewers; one that lists an image or vector format takes it over
 * from ImageViewer.
 */
import type { AppDto } from "../../lib/apps-api";
import type { ChatMessage } from "../../lib/types";
import { HOSTED_PREVIEW_EXT_RE, NATIVE_CRAFT_EXTS, previewExtRe } from "./filePreview";

/**
 * Native craft formats: a workspace preview tab opens them in the app
 * iframe even before `listApps` returns. Office / PDF / photo / vector
 * files stay on their built-in viewers until a matching visual app is
 * enabled (`HOSTED_PREVIEW_EXT_RE` is the wider candidate set).
 */
export const VISUAL_CANVAS_EXT_RE = previewExtRe(NATIVE_CRAFT_EXTS);

export function fileExtension(name: string): string | null {
    const base = name.replace(/\\/g, "/").split("/").pop() ?? "";
    const i = base.lastIndexOf(".");
    if (i <= 0 || i === base.length - 1) return null;
    return base.slice(i + 1).toLowerCase();
}

export function isVisualCanvasFile(name: string): boolean {
    return VISUAL_CANVAS_EXT_RE.test(name);
}

/** Branded canvas, or a format a hosted visual app may take over. */
export function isHostedPreviewCandidate(name: string): boolean {
    return HOSTED_PREVIEW_EXT_RE.test(name);
}

function normalizeExt(ext: string): string {
    return ext.trim().replace(/^\./, "").toLowerCase();
}

/** Extensions a visual app owns. Empty list: FigCraft fallback only. */
export function visualExtsOf(app: AppDto): string[] {
    const declared = (app.canvas_extensions ?? []).map(normalizeExt).filter(Boolean);
    if (declared.length) return declared;
    if (app.id === "figcraft") return ["figcraft"];
    return [];
}

export function appOwnsVisualExt(app: AppDto, ext: string): boolean {
    if (!app.enabled) return false;
    if ((app.canvas_kind || "").toLowerCase() !== "visual") return false;
    return visualExtsOf(app).includes(ext);
}

/** Sibling thumbnail written by the hosted-app adapter after `screenshot`. */
export function previewPngRelPath(rel: string): string {
    const n = rel.replace(/\\/g, "/");
    const slash = n.lastIndexOf("/");
    const dir = slash >= 0 ? n.slice(0, slash + 1) : "";
    const base = slash >= 0 ? n.slice(slash + 1) : n;
    const dot = base.lastIndexOf(".");
    const stem = dot > 0 ? base.slice(0, dot) : base;
    return `${dir}${stem}.preview.png`;
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
    return isHostedPreviewCandidate(rel) ? rel : null;
}

/** Last visual-canvas `path` among tool_calls that already succeeded. */
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
 * do not steal focus. Takeover of an office / image / vector file is on
 * explicit open, not auto-open, so a missing hosted app does not replace
 * the built-in viewer.
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
 * `control=1` opts into `window.<app>` so the adapter-injected bridge can
 * call `openFile` / theme commands.
 */
export function visualPreviewSearch(app: AppDto, relPath: string): string {
    // The file travels through `openFile`, not through the URL. Putting the path
    // in the query changed the iframe's src for every document, so switching
    // files reloaded the frame — a fresh wasm instance per open, which threw
    // away every font the app had loaded and paid for it again. Apps that really
    // do read a path out of the URL declare it in `canvas_preview_path`.
    const tmpl = (app.canvas_preview_path || "").trim();
    const filled = tmpl.includes("{workspace_file}") ? tmpl.replaceAll("{workspace_file}", relPath) : tmpl;
    const qIndex = filled.indexOf("?");
    const raw = qIndex >= 0 ? filled.slice(qIndex + 1) : filled.replace(/^\//, "");
    const params = new URLSearchParams(raw);
    if (!params.has("control")) params.set("control", "1");
    return `?${params.toString()}`;
}

/**
 * Prefer the enabled visual app that declared this file's extension. Two
 * apps claiming one extension resolve by nav order, then id — a stable
 * winner beats a random one, and the loser is still reachable in the app.
 */
export function pickVisualApp(apps: AppDto[], fileName: string): AppDto | null {
    const ext = fileExtension(fileName);
    if (!ext) return null;
    const owners = apps.filter((a) => appOwnsVisualExt(a, ext));
    owners.sort((a, b) => a.nav_order - b.nav_order || a.id.localeCompare(b.id));
    return owners[0] ?? null;
}