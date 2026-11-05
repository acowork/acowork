/**
 * useWorkspaceFileBlob — React data-fetching hook over
 * `lib/workspaceFileBlob.ts` for the editor preview renderers.
 *
 * Owns the three states every viewer needs (loading / error-with-retry /
 * ready), the AbortController teardown on unmount, and the `reloadSeq`
 * re-fetch trigger wired to the editor store's fs-watch invalidation
 * (see `OpenFile.previewSeq`). Components stay fetch-free.
 *
 * `mode` picks which half of the cached entry is exposed:
 *   - "url"  → `blobUrl` for `<iframe>` / `<img>` sources (PDF native
 *     viewer, image preview);
 *   - "blob" → `Blob` for parsers that need `arrayBuffer()` (SheetJS,
 *     docx-preview, JSZip).
 * Both modes hit the same LRU, so an image tab and a "save as" on the
 * same file share one download.
 */
import { useCallback, useEffect, useState } from "react";
import {
    fetchWorkspaceFileBlob,
    fetchWorkspaceFileBlobUrl,
    type WorkspaceFileBytes,
    type WorkspaceFileUrl,
} from "../lib/workspaceFileBlob";

/** The file to read — the same triple the tab is identified by. */
export interface WorkspaceFileBlobRef {
    agentId: string;
    workspaceId: string;
    relPath: string;
}

export interface WorkspaceFileBlobState {
    /** Set when `mode === "blob"`. */
    blob: Blob | null;
    /** Set when `mode === "url"`. Revocable by the LRU — treat as valid
     *  only while this hook holds it (component mounted). */
    blobUrl: string | null;
    mime: string | null;
    loading: boolean;
    /** Error message when the last attempt failed; `retry()` clears it. */
    error: string | null;
    /** Re-run the fetch (cache may still serve it — cheap). */
    retry: () => void;
}

const IDLE: WorkspaceFileBlobState = {
    blob: null,
    blobUrl: null,
    mime: null,
    loading: false,
    error: null,
    retry: () => {},
};

export function useWorkspaceFileBlob(
    fileRef: WorkspaceFileBlobRef | null,
    opts?: { mode?: "blob" | "url"; reloadSeq?: number; enabled?: boolean },
): WorkspaceFileBlobState {
    const mode = opts?.mode ?? "blob";
    const enabled = opts?.enabled !== false;
    const [state, setState] = useState<WorkspaceFileBlobState>(IDLE);
    // Bumped by retry(); part of the effect deps so a failed read can be
    // re-attempted without remounting the component.
    const [nonce, setNonce] = useState(0);
    const retry = useCallback(() => setNonce((n) => n + 1), []);

    const { agentId, workspaceId, relPath } = fileRef ?? {};

    useEffect(() => {
        if (!enabled || agentId === undefined || workspaceId === undefined || relPath === undefined) {
            setState(IDLE);
            return;
        }
        const ctrl = new AbortController();
        let cancelled = false;
        setState((prev) => ({ ...IDLE, retry: prev.retry, loading: true }));
        const read = mode === "url" ? fetchWorkspaceFileBlobUrl : fetchWorkspaceFileBlob;
        read(agentId, workspaceId, relPath, { signal: ctrl.signal }).then(
            (res) => {
                if (cancelled) return;
                setState({
                    blob: mode === "blob" ? (res as WorkspaceFileBytes).blob : null,
                    blobUrl: mode === "url" ? (res as WorkspaceFileUrl).blobUrl : null,
                    mime: res.mime,
                    loading: false,
                    error: null,
                    retry,
                });
            },
            (e) => {
                if (cancelled) return;
                // An abort during a quick unmount is expected teardown,
                // not a user-visible failure — keep the idle state.
                if (ctrl.signal.aborted) {
                    setState(IDLE);
                    return;
                }
                setState({ blob: null, blobUrl: null, mime: null, loading: false, error: String(e), retry });
            },
        );
        return () => {
            cancelled = true;
            ctrl.abort();
        };
        // reloadSeq: the store bumps it when an fs event invalidates the
        // cache; nonce: retry(). Everything else is the file identity.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [agentId, workspaceId, relPath, mode, enabled, opts?.reloadSeq, nonce]);

    return state;
}
