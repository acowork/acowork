/**
 * Workspace HTML preview → the shared address-bar browser (ADR-112).
 *
 * This replaces the old blob-based `HtmlPreviewView`. That renderer fetched the
 * HTML text over the authenticated JSON API, wrapped it in a Blob URL and
 * injected a `<base href>` pointing at the bearer-gated `/workspace-files/…`
 * route. The document rendered (it was local), but every `<link>` / `<script>` /
 * `@font-face` it referenced was loaded **by the browser** without an
 * `Authorization` header → 401 → unstyled skeleton (ADR-112 §决策 1).
 *
 * Now the page is served as a real HTTP document from a capability URL, so:
 *   - relative paths (`styles.css`, `assets/logo.svg`) resolve natively — the
 *     browser does the work a `<base>` + attribute rewriting was approximating;
 *   - CSS, JS, fonts, images, `<iframe>` and `<video>` sub-resources all
 *     authenticate by being under the same ticketed prefix;
 *   - the surface is `UrlPreviewView` — the same address-bar browser that opens
 *     a link from a chat message, which is what a web page deserves regardless
 *     of whether it came from `https://…` or the workspace tree.
 *
 * The tab itself stays `kind: "file"`, so the source ⇄ preview toggle in
 * `FileEditorPanel` keeps working: source is Monaco, preview is this.
 */

import { useCallback, useEffect, useState } from "react";
import { AlertCircle, Loader2, RotateCw } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { UrlPreviewView } from "./UrlPreviewView";
import { getWorkspacePreviewUrl } from "../../lib/workspacePreviewUrl";
import { log } from "../../lib/logger";

interface WorkspaceHtmlPreviewProps {
    agentId: string;
    workspaceId: string;
    /** Workspace-relative path of the HTML file being previewed. */
    relPath: string;
    fileName: string;
}

type PreviewState =
    | { status: "loading" }
    | { status: "ready"; url: string }
    | { status: "error"; message: string };

export function WorkspaceHtmlPreview({ agentId, workspaceId, relPath, fileName }: WorkspaceHtmlPreviewProps) {
    const { t } = useTranslation();
    const [state, setState] = useState<PreviewState>({ status: "loading" });
    /** Bumped by "retry" to force a fresh mint after a failure. */
    const [attempt, setAttempt] = useState(0);

    useEffect(() => {
        let cancelled = false;
        setState({ status: "loading" });

        getWorkspacePreviewUrl(agentId, workspaceId, relPath).then(
            (url) => {
                if (!cancelled) setState({ status: "ready", url });
            },
            (error: unknown) => {
                if (cancelled) return;
                // Nothing to invalidate: a ticket is minted per open, so the
                // retry below always asks for a new one rather than replaying
                // the link that just failed.
                log.error("[WorkspaceHtmlPreview] preview ticket failed:", error);
                setState({
                    status: "error",
                    message: error instanceof Error ? error.message : String(error),
                });
            },
        );

        return () => {
            cancelled = true;
        };
    }, [agentId, workspaceId, relPath, attempt]);

    const retry = useCallback(() => setAttempt((n) => n + 1), []);

    if (state.status === "loading") {
        return (
            <div className="flex h-full w-full flex-col items-center justify-center gap-3 bg-right-panel">
                <Loader2 className="h-8 w-8 animate-spin text-text-tertiary" />
                <span className="text-sm text-text-tertiary">{t("fileEditor.loadingUrl")}</span>
            </div>
        );
    }

    if (state.status === "error") {
        return (
            <div className="flex h-full w-full flex-col items-center justify-center gap-3 bg-right-panel px-6 text-center">
                <AlertCircle className="h-8 w-8 text-amber-500" />
                <span className="text-sm text-text-secondary">{t("fileEditor.htmlPreviewFailed")}</span>
                <span className="max-w-md break-words text-xs text-text-tertiary">{state.message}</span>
                <button
                    type="button"
                    onClick={retry}
                    className="inline-flex items-center gap-1.5 rounded border border-border-divider px-2.5 py-1 text-xs text-text-secondary transition-colors hover:bg-zinc-100 dark:hover:bg-zinc-800"
                >
                    <RotateCw className="h-3 w-3" />
                    {t("fileEditor.htmlPreviewRetry")}
                </button>
            </div>
        );
    }

    return <UrlPreviewView url={state.url} fileName={fileName} />;
}
