import { useEffect } from "react";
import { type UnlistenFn } from "@tauri-apps/api/event";
import { subscribePreviewNewWindow, subscribePreviewState } from "../lib/previewHost";
import { openExternal } from "../lib/openExternal";
import { usePreviewSessionStore } from "../stores/previewSessionStore";
import { useFileEditorStore } from "../stores/fileEditorStore";

/**
 * Wires the native preview browser's unsolicited events into the frontend.
 *
 * Mounted **once** by `FileEditorPanel` (not by each preview surface) because
 * the events are global: `preview://state` carries the label, and the store
 * routes it to whichever tab owns that session.
 *
 * Two behaviours worth naming:
 *
 *   - State events are *state*, not commands: they are applied to the store even
 *     for sessions the UI has forgotten, so a tab remount never shows a stale
 *     phase. The store drops unknown labels on its own.
 *   - `window.open` / `target="_blank"` inside a preview becomes **another
 *     preview tab** when the editor knows the workspace (that is what a browser
 *     user expects), and falls back to the system browser when it does not.
 *     Rust already denied the native popup (`NewWindowResponse::Deny`).
 */
export function usePreviewBridge(agentId: string | null): void {
    const openUrl = useFileEditorStore((s) => s.openUrl);

    useEffect(() => {
        let disposed = false;
        const unlisteners: UnlistenFn[] = [];
        const track = (pending: Promise<UnlistenFn>) =>
            void pending.then((unlisten) => {
                if (disposed) unlisten();
                else unlisteners.push(unlisten);
            });

        track(
            subscribePreviewState((state) => {
                usePreviewSessionStore.getState().applyState(state);
            }),
        );

        track(
            subscribePreviewNewWindow(({ url }) => {
                if (agentId) {
                    openUrl(agentId, url);
                } else {
                    void openExternal(url).catch(() => {});
                }
            }),
        );

        return () => {
            disposed = true;
            for (const unlisten of unlisteners) unlisten();
        };
    }, [agentId, openUrl]);
}