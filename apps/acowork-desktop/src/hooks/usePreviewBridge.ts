import { useEffect, useRef } from "react";
import { type UnlistenFn } from "@tauri-apps/api/event";
import {
    downloadActionUrl,
    subscribePreviewDownload,
    subscribePreviewNewWindow,
    subscribePreviewState,
} from "../lib/previewHost";
import { openExternal } from "../lib/openExternal";
import { usePreviewSessionStore } from "../stores/previewSessionStore";
import { useFileEditorStore } from "../stores/fileEditorStore";
import { showToast, type ToastInput } from "../components/common/ToastProvider";
import { useTranslation } from "../i18n/useTranslation";

/**
 * Wires the native preview browser's unsolicited events into the frontend.
 *
 * Mounted **once** by `FileEditorPanel` (not by each preview surface) because
 * the events are global: `preview://state` carries the label, and the store
 * routes it to whichever tab owns that session.
 *
 * Three behaviours worth naming:
 *
 *   - State events are *state*, not commands: they are applied to the store even
 *     for sessions the UI has forgotten, so a tab remount never shows a stale
 *     phase. The store drops unknown labels on its own.
 *   - `window.open` / `target="_blank"` inside a preview becomes **another
 *     preview tab** when the editor knows the workspace (that is what a browser
 *     user expects), and falls back to the system browser when it does not.
 *     Rust already denied the native popup (`NewWindowResponse::Deny`).
 *   - A **download** is refused by Rust (the app owns no download shelf) and the
 *     refusal is reported instead of swallowed: the toast offers the system
 *     browser, which is the one surface here that can complete it. The action
 *     targets the page rather than a `blob:` download URL — see
 *     `downloadActionUrl`.
 *
 * The subscriptions are installed **once** (`[]` below): everything they read
 * that can change — the agent id, `openUrl`, `t` — goes through a ref. Re-running
 * the effect on an agent switch or a language change would leave a window in
 * which a download request has no listener, and a dropped toast is
 * indistinguishable from the silent refusal this hook was extended to fix.
 *
 * One lifetime dependency is inherent and recorded here because nothing enforces
 * it: the toast needs this hook mounted, and this hook is mounted by
 * `FileEditorPanel`, which `AppLayout` renders under `hasOpenFiles`. A preview
 * webview only exists while its preview tab is open, and an open tab implies
 * `hasOpenFiles`, so the two lifetimes coincide today. A preview that could
 * outlive the panel would make the refusal silent again.
 */
/** Translator shape this module needs (`useTranslation().t`). */
type Translate = (key: string, options?: Record<string, unknown>) => string;

/**
 * The toast for a refused download.
 *
 * Module-level and pure so the message/action pair is decided in one place: the
 * event handler only supplies the two facts (the download URL and the page it
 * came from). It also keeps the `t("…")` call sites literal, which is what
 * `scripts/check-i18n.mjs` scans — alias the translator and the keys stop being
 * verified against the locale files.
 */
export function downloadToast(
    url: string,
    pageUrl: string | null | undefined,
    t: Translate,
): ToastInput {
    // The download URL is only openable when a browser can fetch it itself; a
    // page-generated `blob:` export falls back to the page (`downloadActionUrl`).
    const target = downloadActionUrl(url, pageUrl);
    return {
        type: "warning",
        message: t("fileEditor.downloadBlocked"),
        ...(target
            ? {
                  action: {
                      label: t("fileEditor.openInBrowser"),
                      onClick: () => void openExternal(target).catch(() => {}),
                  },
              }
            : {}),
    };
}

export function usePreviewBridge(agentId: string | null): void {
    const openUrl = useFileEditorStore((s) => s.openUrl);
    const { t } = useTranslation();
    // Written during render, read from the event handlers: the handlers must see
    // the current values without being re-created (see the doc comment above).
    const live = useRef({ agentId, openUrl, t });
    live.current = { agentId, openUrl, t };

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
                const { agentId: agent, openUrl: open } = live.current;
                if (agent) {
                    open(agent, url);
                } else {
                    void openExternal(url).catch(() => {});
                }
            }),
        );

        track(
            subscribePreviewDownload(({ url, from }) => {
                const pageUrl = usePreviewSessionStore.getState().states[from]?.url;
                showToast(downloadToast(url, pageUrl, live.current.t));
            }),
        );

        return () => {
            disposed = true;
            for (const unlisten of unlisteners) unlisten();
        };
    }, []);
}