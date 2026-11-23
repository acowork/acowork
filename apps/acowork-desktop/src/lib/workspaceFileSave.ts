/**
 * Write bytes back into a workspace file — the save half of the hosted-app
 * visual canvas (ADR-118).
 *
 * The text save path (`fileEditorStore.saveFile`) PUTs a string `content`;
 * a hosted app hands back an `ArrayBuffer` (`.docx`, `.xlsx`, `.pptx`,
 * `.pdf`), so the bytes ride as base64 in `contentBase64`. The Runtime
 * decodes them and writes them verbatim.
 *
 * Same origin, auth and error contract as the rest of the workspace file
 * plumbing: `getGatewayUrl()` + the app-wide `authFetch` interceptor, no
 * token plumbing here.
 */
import { getGatewayUrl } from "./config";
import { primeWorkspaceFileBlob } from "./workspaceFileBlob";
import { useAgentStore } from "../stores/agentStore";
import { useChatStore } from "../stores/chatStore";
import { useWorkspaceStore } from "../stores/workspaceStore";
import { openSaveAs } from "../stores/saveAsStore";
import { refreshTreeNode } from "../stores/fileTree/fileTreeStore";

export interface WorkspaceFileRef {
    agentId: string;
    workspaceId: string;
    relPath: string;
}

/** A refused workspace write, with the status that refused it. */
export class WorkspaceSaveError extends Error {
    constructor(
        message: string,
        readonly status: number,
    ) {
        super(message);
        this.name = "WorkspaceSaveError";
    }
}

function toBase64(bytes: Uint8Array): string {
    // Chunked so a large document doesn't blow the argument limit of
    // `String.fromCharCode.apply`.
    let out = "";
    const chunk = 0x8000;
    for (let i = 0; i < bytes.length; i += chunk) {
        out += String.fromCharCode(...bytes.subarray(i, i + chunk));
    }
    return btoa(out);
}

/**
 * Overwrite one workspace file with `bytes`. Throws when the Runtime
 * refuses (missing file, path escape, invalid payload) — the caller
 * surfaces that to the user instead of reporting a save that didn't happen.
 */
export async function saveWorkspaceFileBytes(
    file: WorkspaceFileRef,
    bytes: Uint8Array,
): Promise<{ size?: number; modified?: string }> {
    const base = getGatewayUrl().replace(/\/+$/g, "");
    const params = new URLSearchParams({
        workspace_id: file.workspaceId,
        path: file.relPath,
    });
    const url = `${base}/api/agents/${encodeURIComponent(file.agentId)}/workspaces/file?${params.toString()}`;
    const b64 = toBase64(bytes);
    const resp = await fetch(url, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ contentBase64: b64 }),
    });
    if (!resp.ok) {
        const errText = await resp.text().catch(() => "");
        throw new WorkspaceSaveError(errText || `HTTP ${resp.status}`, resp.status);
    }
    // The disk bytes changed; seed the cache with exactly what we wrote so the
    // next preview open serves the saved bytes (a delete-only invalidation
    // leaves a mounted preview holding the pre-save blob).
    primeWorkspaceFileBlob(file.agentId, file.workspaceId, file.relPath, bytes);
    const saved = (await resp.json().catch(() => ({}))) as { modified?: string; size?: number };
    return saved;
}

/** Directory part of a workspace-relative path; "" is the workspace root. */
function dirOf(relPath: string): string {
    const i = relPath.lastIndexOf("/");
    return i <= 0 ? "" : relPath.slice(0, i);
}

/**
 * The single path segment an app's save name may become. An app is a guest in
 * the workspace: anything that could climb (`..`), or name a directory, is
 * dropped in favour of `fallback`.
 */
export function saveNameSegment(name: string, fallback: string): string {
    const base = (name || "").split(/[\\/]/).pop()?.trim() ?? "";
    if (!base || base === "." || base === "..") return fallback;
    return base;
}

/**
 * POST an empty file: `PUT /workspaces/file` only overwrites, so a Save As to a
 * path the workspace does not have yet needs the create first. `overwrite` so a
 * concurrent create or an already-there file is not a 409.
 */
async function createWorkspaceFile(file: WorkspaceFileRef, relPath: string): Promise<void> {
    const base = getGatewayUrl().replace(/\/+$/g, "");
    const params = new URLSearchParams({ workspace_id: file.workspaceId });
    const url = `${base}/api/agents/${encodeURIComponent(file.agentId)}/workspaces/file?${params.toString()}`;
    const resp = await fetch(url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path: relPath, overwrite: true }),
    });
    if (!resp.ok) {
        const errText = await resp.text().catch(() => "");
        throw new WorkspaceSaveError(errText || `HTTP ${resp.status}`, resp.status);
    }
}

/**
 * Save As (ADR-118): write the app's bytes under the name the app chose, in
 * the directory of `file` — `relPath: ""` means the workspace root. The same
 * name overwrites; another name is a new sibling file, which is what an app's
 * Save As / Export asks for and what keeps a `.pdf` export off the `.docx`.
 */
export async function saveWorkspaceFileAs(
    file: WorkspaceFileRef,
    name: string,
    bytes: Uint8Array,
): Promise<{ relPath: string; size?: number; modified?: string }> {
    const segment = saveNameSegment(name, file.relPath.split("/").pop() ?? "");
    if (!segment) throw new Error("the app did not name the file it saved");
    const dir = dirOf(file.relPath);
    const relPath = dir ? `${dir}/${segment}` : segment;
    const saved = await writeWorkspaceFileBytes({ ...file, relPath }, bytes);
    return { relPath, ...saved };
}

/**
 * Write bytes to a workspace path, creating the file first when it is not
 * there yet.
 *
 * `PUT /workspaces/file` only overwrites (the Runtime answers 404 for a path it
 * does not have), so the first save of a document the app made itself has to
 * POST the empty file, then write. Applies to any path, not just a Save As.
 */
export async function writeWorkspaceFileBytes(
    file: WorkspaceFileRef,
    bytes: Uint8Array,
): Promise<{ size?: number; modified?: string }> {
    try {
        return await saveWorkspaceFileBytes(file, bytes);
    } catch (e) {
        if (!(e instanceof WorkspaceSaveError) || e.status !== 404) throw e;
        await createWorkspaceFile(file, file.relPath);
        return saveWorkspaceFileBytes(file, bytes);
    }
}

/** The save the user cancelled in the Save As dialog — not an error to show. */
export class SaveCancelled extends Error {
    constructor() {
        super("save cancelled");
        this.name = "SaveCancelled";
    }
}

/**
 * One hosted-app save, whichever surface it came from (ADR-118 §6.4).
 *
 * - `saveAs: false` with a file bound to the surface → that file is overwritten.
 * - `saveAs: true`, or nothing bound (the left-nav app canvas) → the user picks
 *   the workspace, folder and name; cancelling writes nothing.
 *
 * Afterwards the file tree is refreshed for the directory that changed, so a
 * new file shows up instead of the save landing invisibly.
 */
export async function saveHostedAppFile({
    bound,
    saveAs,
    name,
    bytes,
}: {
    /** The workspace file the surface is showing, if any. */
    bound: WorkspaceFileRef | null;
    /** The app asked for Save As / Export. */
    saveAs: boolean;
    name: string;
    bytes: Uint8Array;
}): Promise<void> {
    if (!saveAs && bound && bound.relPath) {
        const saved = await saveWorkspaceFileAs(bound, name, bytes);
        if (saved.relPath !== bound.relPath) {
            void refreshTreeNode(bound.agentId, bound.workspaceId, dirOf(saved.relPath));
        }
        return;
    }

    const agentId = bound?.agentId ?? useAgentStore.getState().selectedAgentId;
    if (!agentId) throw new Error("no agent is selected to save into");
    const sessionId = useChatStore.getState().getActiveSessionId(agentId);
    const defaultWorkspaceId =
        bound?.workspaceId ??
        (sessionId ? useWorkspaceStore.getState().getSessionWorkspaceId(sessionId) : "__agent_home__");
    const target = await openSaveAs({ agentId, defaultWorkspaceId, name });
    if (!target) throw new SaveCancelled();
    await writeWorkspaceFileBytes(target, bytes);
    void refreshTreeNode(target.agentId, target.workspaceId, dirOf(target.relPath));
}