/**
 * Office-document → PDF conversion client.
 *
 * Talks to the two Rust commands in `src-tauri/src/commands/doc_convert.rs`
 * (`detect_doc_convert_engine`, `convert_workspace_file_to_pdf`). Nothing
 * here renders anything: the PDF bytes it hands back are fed to the same
 * native `<iframe>` viewer the PDF tab uses, so a .docx / .pptx / .xlsx
 * preview shows the real printed layout of the user's own Office /
 * LibreOffice engine instead of a hand-rolled HTML re-layout.
 *
 * Two things are cached, deliberately:
 *   - the engine probe, in a module-level promise. The answer cannot change
 *     while the app runs (installing Office does not rewrite the running
 *     process), every document tab asks for it, and spawning `where` /
 *     `reg` per tab is pure waste. Shared promise, NO caller signal — the
 *     same rule that fixed the StrictMode AbortError in workspaceFileBlob.ts:
 *     binding one caller's abort to a shared promise poisons everyone else.
 *   - conversion results, in a small LRU keyed by file identity + version
 *     (size / mtime / previewSeq). Rust keeps its own on-disk cache, so this
 *     layer only avoids re-crossing the IPC boundary for a tab that gets
 *     switched back to. Cap by COUNT (5), not bytes: the entries are
 *     Uint8Array, and one is only ever a few MB.
 *
 * The bytes come back as raw IPC (`tauri::ipc::Response`), never as a JSON
 * value — see attachmentTransfer.ts for the freeze that a serialized byte
 * array causes. `toBytes` still tolerates the older shapes so a change in
 * the IPC layer shows up as working-slowly rather than broken.
 */
import { invoke } from "@tauri-apps/api/core";
import { log } from "./logger";
import { workspaceFileUrlForRust } from "./workspaceFileBlob";

/** Mirrors Rust `EngineKind` (serde `kebab-case`). */
export type DocConvertEngine = "libreoffice" | "ms-office-com" | "none";

/** Mirrors Rust `EngineInfo`. `path` is diagnostic only. */
export interface DocConvertEngineInfo {
    engine: DocConvertEngine;
    path?: string | null;
}

/** Error string the Rust command returns when no engine was found. */
export const DOC_CONVERT_ENGINE_UNAVAILABLE = "engine-unavailable";

/**
 * Thrown by {@link convertWorkspaceFileToPdf} when the machine has no
 * conversion engine. It is NOT a failure the user should see as an error:
 * callers fall back to the in-app JS renderer and explain that the layout
 * is approximate. Every other rejection means "an engine exists but this
 * conversion did not work".
 */
export class DocConvertUnavailableError extends Error {
    constructor(message: string = DOC_CONVERT_ENGINE_UNAVAILABLE) {
        super(message);
        this.name = "DocConvertUnavailableError";
    }
}

/** File version inputs for the cache key (all optional — see below). */
export interface DocConvertVersion {
    /** Source file size in bytes, from the cached file-tree entry. */
    size?: number;
    /** Source file mtime (epoch ms), from the cached file-tree entry. */
    mtimeMs?: number;
    /** `OpenFile.previewSeq` — bumped when an fs event invalidated the tab,
     *  so a stale tree snapshot can never pin a stale PDF. */
    seq?: number;
}

/** `engine-unavailable` arrives as a plain string over the IPC boundary. */
function errorMessage(e: unknown): string {
    if (typeof e === "string") return e;
    if (e instanceof Error) return e.message;
    return String(e);
}

function isEngineUnavailable(message: string): boolean {
    return message.includes(DOC_CONVERT_ENGINE_UNAVAILABLE);
}

// ── Engine probe ───────────────────────────────────────────────────────

let enginePromise: Promise<DocConvertEngineInfo> | null = null;

/** Coerce an unknown-shaped response into a safe answer. Anything the
 *  frontend cannot recognise means "do not attempt a conversion". */
function normalizeEngineInfo(raw: unknown): DocConvertEngineInfo {
    const engine = (raw as DocConvertEngineInfo | null)?.engine;
    if (engine === "libreoffice" || engine === "ms-office-com") {
        return { engine, path: (raw as DocConvertEngineInfo).path ?? null };
    }
    return { engine: "none", path: null };
}

/**
 * Which engine can convert on this machine. Cached for the process; the
 * promise itself is what is cached, so concurrent tabs share one probe.
 * Never rejects — a failed probe answers `none`, which routes the UI to
 * the existing JS viewers.
 */
export function detectDocConvertEngine(): Promise<DocConvertEngineInfo> {
    if (!enginePromise) {
        const probe = invoke<DocConvertEngineInfo>("detect_doc_convert_engine")
            .then(normalizeEngineInfo)
            .catch((e): DocConvertEngineInfo => {
                log.warn("[docConvert] engine probe failed, treating as none:", e);
                // Do not cache a transport failure — a later tab should be
                // able to retry (the Rust side caches its own answer, so a
                // retry is cheap when it succeeds).
                if (enginePromise === probe) enginePromise = null;
                return { engine: "none", path: null };
            });
        enginePromise = probe;
    }
    return enginePromise;
}

// ── Conversion ─────────────────────────────────────────────────────────

const MAX_CACHED_PDFS = 5;
/** key → in-flight-or-resolved conversion. Map order doubles as recency. */
const pdfCache = new Map<string, Promise<Uint8Array>>();

function cacheKey(
    agentId: string,
    workspaceId: string,
    relPath: string,
    version: DocConvertVersion,
): string {
    return [
        agentId,
        workspaceId,
        relPath,
        version.size ?? 0,
        version.mtimeMs ?? 0,
        version.seq ?? 0,
    ].join(":");
}

function toBytes(raw: unknown): Uint8Array {
    if (raw instanceof Uint8Array) return raw;
    if (raw instanceof ArrayBuffer) return new Uint8Array(raw);
    // Defensive: were the response ever JSON-encoded, IPC would deliver a
    // plain number array. Slow, but correct rather than crashing.
    if (Array.isArray(raw)) return Uint8Array.from(raw as number[]);
    throw new Error("docConvert: unexpected converter response shape");
}

/**
 * Convert one workspace file to PDF on the local engine.
 *
 * The URL is built by {@link workspaceFileUrlForRust}: the bare,
 * token-less Gateway address crosses the bridge so Rust can
 * re-authorise and stream the source itself. It must never reach a
 * render prop (see workspaceFileBlob.ts's header).
 *
 * Resolves with the PDF bytes. Rejects with
 * {@link DocConvertUnavailableError} when no engine is installed, and with
 * a plain Error for download / conversion / timeout failures — the caller
 * degrades to the JS viewer in both cases, with different copy.
 */
export async function convertWorkspaceFileToPdf(
    agentId: string,
    workspaceId: string,
    relPath: string,
    fileName: string,
    version: DocConvertVersion = {},
): Promise<Uint8Array> {
    const key = cacheKey(agentId, workspaceId, relPath, version);
    const hit = pdfCache.get(key);
    if (hit) {
        pdfCache.delete(key);
        pdfCache.set(key, hit);
        return hit;
    }

    const pending = (async (): Promise<Uint8Array> => {
        const engine = await detectDocConvertEngine();
        if (engine.engine === "none") throw new DocConvertUnavailableError();
        // Rust keys its on-disk cache on `size` + `etag`. There is no real
        // HTTP validator here, so the version tuple is folded into `etag`:
        // stable for an unchanged file, different the moment the tree
        // snapshot or the fs-watch bump moves.
        const etag = `${version.mtimeMs ?? 0}#${version.seq ?? 0}`;
        let raw: unknown;
        try {
            raw = await invoke<ArrayBuffer>("convert_workspace_file_to_pdf", {
                url: workspaceFileUrlForRust(agentId, workspaceId, relPath),
                filename: fileName,
                size: version.size ?? null,
                etag,
            });
        } catch (e) {
            const message = errorMessage(e);
            // Rust re-probes and answers `engine-unavailable` when it has
            // nothing to convert with — same meaning as our own gate.
            if (isEngineUnavailable(message)) throw new DocConvertUnavailableError(message);
            throw e instanceof Error ? e : new Error(message);
        }
        return toBytes(raw);
    })().catch((e) => {
        // A failed conversion must not be remembered, or "Retry" would
        // replay the same rejected promise forever.
        pdfCache.delete(key);
        throw e;
    });

    pdfCache.set(key, pending);
    while (pdfCache.size > MAX_CACHED_PDFS) {
        const oldest = pdfCache.keys().next().value as string;
        pdfCache.delete(oldest);
    }
    return pending;
}

/**
 * Test hook: forget the cached engine answer and every cached conversion so
 * a test can drive detection / conversion from a clean slate.
 */
export function __resetDocConvertCache(): void {
    enginePromise = null;
    pdfCache.clear();
}
