import { useCallback, useEffect, useState, type KeyboardEvent } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { Loader2, ExternalLink, ArrowLeft, ArrowRight, RefreshCw, AlertTriangle } from "lucide-react";
import {
    normalizePreviewUrl,
    previewHistory,
    reloadPreview,
    navigatePreview,
    parsePreviewError,
    type PreviewError,
} from "../../lib/previewHost";
import { openExternal } from "../../lib/openExternal";
import { usePreviewSessionStore } from "../../stores/previewSessionStore";
import { WebPreviewHost } from "./WebPreviewHost";

interface UrlPreviewViewProps {
    url: string;
    fileName: string;
    /**
     * Identity of the owning tab. Defaults to the URL so ad-hoc callers still
     * get session reuse; editor tabs pass their `OpenFile.id`.
     */
    sessionKey?: string;
}

/**
 * In-app browser surface: navigation toolbar + native webview below it.
 *
 * ADR-118 — why a native webview and not an `<iframe>`:
 *   `X-Frame-Options: deny` / `CSP frame-ancestors 'none'` are enforced by the
 *   engine on the *embedded* document, so github.com, google.com and the like
 *   render blank in a frame no matter what the frontend does. The preview runs
 *   as a child webview (`Webview::add_child`) that loads the page as a
 *   top-level document, which those headers do not restrict — the same approach
 *   as Cursor / VS Code Simple Browser.
 *
 * Layout consequence: the native surface is composited **above** the HTML, so
 * nothing can be drawn over it. Every control therefore lives in this toolbar
 * (outside the webview rect), and failure states replace the surface instead of
 * overlaying it.
 */
export function UrlPreviewView({ url, fileName, sessionKey }: UrlPreviewViewProps) {
    const { t } = useTranslation();
    const key = sessionKey ?? url;

    const label = usePreviewSessionStore((s) => s.labels[key]);
    const state = usePreviewSessionStore((s) => (label ? s.states[label] : undefined));
    // Navigate away from an errored session by remounting the host.
    const [hostEpoch, setHostEpoch] = useState(0);

    const [openError, setOpenError] = useState<PreviewError | null>(null);
    const [urlInputValue, setUrlInputValue] = useState(url);
    const [isEditingUrl, setIsEditingUrl] = useState(false);

    const currentUrl = state?.url || url;
    const loading = !state || state.phase === "loading";

    useEffect(() => {
        setUrlInputValue(url);
        setIsEditingUrl(false);
        setOpenError(null);
        setHostEpoch(0);
    }, [url]);

    const handleOpenError = useCallback((error: PreviewError) => setOpenError(error), []);

    const handleRefresh = useCallback(() => {
        if (!label) {
            setOpenError(null);
            setHostEpoch((epoch) => epoch + 1);
            return;
        }
        void reloadPreview(label).catch((error) => setOpenError(parsePreviewError(error)));
    }, [label]);

    const handleHistory = useCallback(
        (direction: "back" | "forward") => {
            if (!label) return;
            void previewHistory(label, direction).catch(() => {
                /* ends of history are no-ops, not errors */
            });
        },
        [label],
    );

    const navigateToUrl = useCallback(
        (raw: string) => {
            const target = normalizePreviewUrl(raw);
            if (!target) return;
            setIsEditingUrl(false);
            setUrlInputValue(target);
            setOpenError(null);
            if (!label) {
                setHostEpoch((epoch) => epoch + 1);
                return;
            }
            void navigatePreview(label, target).catch((error) =>
                setOpenError(parsePreviewError(error)),
            );
        },
        [label],
    );

    const handleUrlKeyDown = useCallback(
        (e: KeyboardEvent<HTMLInputElement>) => {
            if (e.key === "Enter") navigateToUrl(urlInputValue);
            if (e.key === "Escape") {
                setIsEditingUrl(false);
                setUrlInputValue(currentUrl);
            }
        },
        [urlInputValue, currentUrl, navigateToUrl],
    );

    const errorText = openError ? describePreviewError(openError, t) : null;

    return (
        <div className="flex h-full w-full flex-col" aria-label={fileName} role="region">
            {/* ── Navigation toolbar (outside the native surface, by necessity) ── */}
            <div className="flex shrink-0 items-center gap-1 border-b border-border-divider bg-surface-subtle px-2 py-1.5">
                <button
                    onClick={() => handleHistory("back")}
                    className="rounded p-1 text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
                    title={t("fileEditor.navBack")}
                >
                    <ArrowLeft className="h-3.5 w-3.5" />
                </button>
                <button
                    onClick={() => handleHistory("forward")}
                    className="rounded p-1 text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
                    title={t("fileEditor.navForward")}
                >
                    <ArrowRight className="h-3.5 w-3.5" />
                </button>
                <button
                    onClick={handleRefresh}
                    className="rounded p-1 text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
                    title={t("fileEditor.navRefresh")}
                >
                    <RefreshCw className="h-3.5 w-3.5" />
                </button>

                {loading && !openError && (
                    <span title={t("fileEditor.previewLoading")} className="flex shrink-0">
                        <Loader2 className="h-3.5 w-3.5 animate-spin text-text-tertiary" />
                    </span>
                )}

                {/* Editable URL bar */}
                <div className="mx-1 flex-1 overflow-hidden rounded bg-modal-surface">
                    <input
                        type="text"
                        aria-label={t("fileEditor.previewAddressLabel")}
                        value={isEditingUrl ? urlInputValue : currentUrl}
                        onChange={(e) => {
                            setUrlInputValue(e.target.value);
                            setIsEditingUrl(true);
                        }}
                        onFocus={() => {
                            setUrlInputValue(currentUrl);
                            setIsEditingUrl(true);
                        }}
                        onBlur={() => {
                            setIsEditingUrl(false);
                            setUrlInputValue(currentUrl);
                        }}
                        onKeyDown={handleUrlKeyDown}
                        className="w-full border-0 bg-transparent px-2 py-1 text-xs text-text-secondary outline-none placeholder-text-disabled focus:ring-0  dark:focus:text-zinc-100"
                        placeholder={currentUrl}
                    />
                </div>

                {/* Open in the system browser — a real top-level browser window,
                    the fallback for anything the in-app surface should not host. */}
                <button
                    onClick={() => {
                        void openExternal(currentUrl).catch(() => {});
                    }}
                    className="flex shrink-0 items-center gap-1 rounded px-2 py-1 text-xs text-info transition-colors hover:bg-info-surface"
                    title={t("fileEditor.openInBrowser")}
                >
                    <ExternalLink className="h-3.5 w-3.5" />
                </button>
            </div>

            {/* ── Surface ── */}
            <div className="relative min-h-0 flex-1 bg-modal-surface">
                {errorText ? (
                    <div className="flex h-full flex-col items-center justify-center gap-3 px-8 text-center">
                        <AlertTriangle className="h-8 w-8 text-warning" />
                        <div className="text-sm text-text-secondary">
                            {t("fileEditor.previewErrorTitle")}
                        </div>
                        <div className="max-w-md text-xs text-text-tertiary">{errorText}</div>
                        <button
                            onClick={() => {
                                setOpenError(null);
                                setHostEpoch((epoch) => epoch + 1);
                            }}
                            className="rounded border border-border-divider px-3 py-1 text-xs text-text-secondary transition-colors hover:bg-surface-subtle"
                        >
                            {t("fileEditor.previewRetry")}
                        </button>
                    </div>
                ) : (
                    <WebPreviewHost
                        key={`${key}:${hostEpoch}`}
                        sessionKey={key}
                        url={url}
                        active
                        onError={handleOpenError}
                    />
                )}
            </div>
        </div>
    );
}

/**
 * Turn a typed backend error into a user-facing sentence.
 *
 * The codes are part of the command contract (`previewHost.ts`); anything
 * unrecognised shows the raw detail so a real engine failure is never hidden
 * behind a generic "failed".
 */
function describePreviewError(
    error: PreviewError,
    t: (key: string) => string,
): string {
    switch (error.code) {
        case "blocked_scheme":
            return t("fileEditor.previewErrorBlockedScheme");
        case "invalid_url":
            return t("fileEditor.previewErrorInvalidUrl");
        case "create_failed":
        case "main_window_missing":
            return `${t("fileEditor.previewErrorFailed")} (${error.code}: ${error.detail})`;
        case "unknown_preview":
        case "other":
        default:
            return error.detail || error.code;
    }
}