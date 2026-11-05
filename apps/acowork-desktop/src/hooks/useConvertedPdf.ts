/**
 * useConvertedPdf — the React side of the Office → PDF preview.
 *
 * Drives one document tab through the two steps `lib/docConvert.ts`
 * exposes: probe the local engine, then convert. The status is finer than
 * a loading/error pair on purpose, because the two non-ready outcomes need
 * different copy from the UI:
 *
 *   - `unavailable` — the machine has no Office / LibreOffice. Expected on
 *     a bare system: the viewer shows its existing JS renderer plus a
 *     "this is an approximate layout" banner. Not an error.
 *   - `failed` — an engine exists but this file did not convert (encrypted,
 *     corrupted, timed out). Same fallback, different banner, and `retry()`
 *     is the answer.
 *
 * `size` / `mtimeMs` are the file version captured when the tab was opened
 * (see `OpenFile.previewSize` / `previewMtimeMs`); `reloadSeq` is the
 * store's fs-watch bump. Together they form the converter's cache identity:
 * missing version data is tolerated (the converter then re-runs per open),
 * never a reason to skip the preview.
 */
import { useCallback, useEffect, useState } from "react";
import {
    convertWorkspaceFileToPdf,
    detectDocConvertEngine,
    DocConvertUnavailableError,
} from "../lib/docConvert";

export interface ConvertedPdfRef {
    agentId: string;
    workspaceId: string;
    relPath: string;
    fileName: string;
    /** Source size / mtime as recorded by the tab (optional — see above). */
    size?: number;
    mtimeMs?: number;
}

export type ConvertedPdfStatus =
    | "idle"
    | "detecting"
    | "converting"
    | "ready"
    | "unavailable"
    | "failed";

export interface ConvertedPdfState {
    status: ConvertedPdfStatus;
    /** PDF bytes when `status === "ready"`. */
    pdfBytes: Uint8Array | null;
    /** Failure message when `status === "failed"`. */
    error: string | null;
    /** Re-run probe + conversion (the caches may answer instantly). */
    retry: () => void;
}

const IDLE: ConvertedPdfState = {
    status: "idle",
    pdfBytes: null,
    error: null,
    retry: () => {},
};

export function useConvertedPdf(
    fileRef: ConvertedPdfRef | null,
    opts?: { enabled?: boolean; reloadSeq?: number },
): ConvertedPdfState {
    const enabled = opts?.enabled !== false;
    const [state, setState] = useState<ConvertedPdfState>(IDLE);
    const [nonce, setNonce] = useState(0);
    const retry = useCallback(() => setNonce((n) => n + 1), []);

    const { agentId, workspaceId = "", relPath, fileName, size, mtimeMs } = fileRef ?? {};
    const reloadSeq = opts?.reloadSeq;

    useEffect(() => {
        if (!enabled || !agentId || !relPath || !fileName) {
            setState(IDLE);
            return;
        }
        let cancelled = false;
        setState({ ...IDLE, status: "detecting", retry });
        void (async () => {
            try {
                const engine = await detectDocConvertEngine();
                if (cancelled) return;
                if (engine.engine === "none") {
                    setState({ ...IDLE, status: "unavailable", retry });
                    return;
                }
                setState((prev) => ({ ...prev, status: "converting" }));
                const bytes = await convertWorkspaceFileToPdf(
                    agentId,
                    workspaceId,
                    relPath,
                    fileName,
                    { size, mtimeMs, seq: reloadSeq },
                );
                if (cancelled) return;
                setState({ status: "ready", pdfBytes: bytes, error: null, retry });
            } catch (e) {
                if (cancelled) return;
                if (e instanceof DocConvertUnavailableError) {
                    setState({ ...IDLE, status: "unavailable", retry });
                    return;
                }
                setState({ ...IDLE, status: "failed", error: String(e), retry });
            }
        })();
        // Tab identity + the version inputs + the store's invalidate bump.
        // Nothing aborts: `invoke` cannot be cancelled, and the conversion
        // result is cached either way, so a fast unmount simply loses
        // interest (the `cancelled` guard) instead of failing.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [agentId, workspaceId, relPath, fileName, size, mtimeMs, enabled, reloadSeq, nonce]);

    return state;
}
