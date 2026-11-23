/**
 * "Save As" as a promise: a hosted app's Save As cannot touch the file system
 * (the iframe is opaque-origin), so the host has to ask where. The surface that
 * received the app's save opens this dialog and awaits the target the user
 * picked; `null` = cancelled.
 *
 * A store rather than props because the dialog is one component mounted once
 * (AppLayout) while the surfaces that need it — the editor pane's visual
 * preview and the left-nav app canvas — sit in different subtrees.
 */
import { create } from "zustand";

export interface SaveAsRequest {
    agentId: string;
    /** Preselected workspace (the one the file, or the session, is on). */
    defaultWorkspaceId: string;
    /** File name the app asked for, used to prefill the name field. */
    name: string;
}

export interface SaveAsTarget {
    agentId: string;
    workspaceId: string;
    /** Workspace-relative path the bytes go to. */
    relPath: string;
}

interface PendingRequest extends SaveAsRequest {
    resolve: (target: SaveAsTarget | null) => void;
}

interface SaveAsState {
    request: PendingRequest | null;
    open: (request: SaveAsRequest) => Promise<SaveAsTarget | null>;
    settle: (target: SaveAsTarget | null) => void;
}

export const useSaveAsStore = create<SaveAsState>((set, get) => ({
    request: null,
    open: (request) =>
        new Promise<SaveAsTarget | null>((resolve) => {
            // One dialog at a time: a second request settles the first as
            // cancelled instead of leaving its promise hanging forever.
            get().request?.resolve(null);
            set({ request: { ...request, resolve } });
        }),
    settle: (target) => {
        const pending = get().request;
        set({ request: null });
        pending?.resolve(target);
    },
}));

/** Open the Save As dialog and wait for the user's choice. */
export function openSaveAs(request: SaveAsRequest): Promise<SaveAsTarget | null> {
    return useSaveAsStore.getState().open(request);
}