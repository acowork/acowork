import { useState, useCallback, useRef, useEffect, type KeyboardEvent } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { Loader2, ExternalLink, ArrowLeft, ArrowRight, RefreshCw } from "lucide-react";

interface UrlPreviewViewProps {
    url: string;
    fileName: string;
}

/**
 * Renders an external URL in a sandboxed iframe with:
 *  - Navigation toolbar (back, forward, refresh, editable URL bar, open-in-browser)
 *  - Loading spinner on initial load; after 5s shows a non-blocking hint
 *    for pages that block iframe embedding (e.g. Baidu, GitHub).
 *  - Internal links (including target="_blank") navigate within the iframe.
 *
 * URL bar is editable: user can type a URL and press Enter to navigate.
 *
 * Note: Many major websites (Baidu, GitHub, Google) block iframe embedding via
 * X-Frame-Options or CSP. In this case the iframe remains blank. The user can
 * use the "Open in browser" button or type a compatible URL.
 */
export function UrlPreviewView({ url, fileName }: UrlPreviewViewProps) {
    const { t } = useTranslation();
    const [loading, setLoading] = useState(true);
    const [showSlowHint, setShowSlowHint] = useState(false);
    const iframeRef = useRef<HTMLIFrameElement>(null);
    const [currentUrl, setCurrentUrl] = useState(url);
    const [urlInputValue, setUrlInputValue] = useState(url);
    const [isEditingUrl, setIsEditingUrl] = useState(false);
    const slowTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

    const clearSlowTimer = useCallback(() => {
        if (slowTimerRef.current) {
            clearTimeout(slowTimerRef.current);
            slowTimerRef.current = null;
        }
    }, []);

    // Reset when the outer URL changes (user clicked a different link in chat)
    useEffect(() => {
        setCurrentUrl(url);
        setUrlInputValue(url);
        setLoading(true);
        setShowSlowHint(false);
        setIsEditingUrl(false);
        clearSlowTimer();
        // Show "page is taking longer" hint after 5 seconds
        slowTimerRef.current = setTimeout(() => {
            setShowSlowHint(true);
        }, 5000);
        return clearSlowTimer;
    }, [url, clearSlowTimer]);

    const handleLoad = useCallback(() => {
        setLoading(false);
        setShowSlowHint(false);
        clearSlowTimer();

        // Try to sync the URL bar (same-origin only)
        try {
            const href = iframeRef.current?.contentWindow?.location.href;
            if (href && href !== currentUrl) {
                setCurrentUrl(href);
                if (!isEditingUrl) {
                    setUrlInputValue(href);
                }
            }
        } catch {
            // Cross-origin — can't read location; keep the original URL
        }
    }, [currentUrl, isEditingUrl, clearSlowTimer]);

    const handleRefresh = useCallback(() => {
        const iframe = iframeRef.current;
        if (!iframe) return;
        iframe.src = currentUrl;
        setLoading(true);
        setShowSlowHint(false);
        clearSlowTimer();
        slowTimerRef.current = setTimeout(() => {
            setShowSlowHint(true);
        }, 5000);
    }, [currentUrl, clearSlowTimer]);

    const navigateToUrl = useCallback((targetUrl: string) => {
        if (!targetUrl) return;
        const normalized = targetUrl.startsWith("http://") || targetUrl.startsWith("https://")
            ? targetUrl
            : `https://${targetUrl}`;
        setCurrentUrl(normalized);
        setUrlInputValue(normalized);
        setLoading(true);
        setShowSlowHint(false);
        setIsEditingUrl(false);
        clearSlowTimer();
        slowTimerRef.current = setTimeout(() => {
            setShowSlowHint(true);
        }, 5000);
    }, [clearSlowTimer]);

    const handleUrlKeyDown = useCallback((e: KeyboardEvent<HTMLInputElement>) => {
        if (e.key === "Enter") {
            navigateToUrl(urlInputValue);
        }
    }, [urlInputValue, navigateToUrl]);

    const handleUrlBlur = useCallback(() => {
        setIsEditingUrl(false);
        setUrlInputValue(currentUrl);
    }, [currentUrl]);

    return (
        <div className="flex h-full w-full flex-col">
            {/* ── Navigation Toolbar ── */}
            <div className="flex shrink-0 items-center gap-1 border-b border-border-divider bg-surface-subtle px-2 py-1.5">
                {/* Back */}
                <button
                    onClick={() => { try { iframeRef.current?.contentWindow?.history.back(); } catch {} }}
                    className="rounded p-1 text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
                    title={t("fileEditor.navBack")}
                >
                    <ArrowLeft className="h-3.5 w-3.5" />
                </button>

                {/* Forward */}
                <button
                    onClick={() => { try { iframeRef.current?.contentWindow?.history.forward(); } catch {} }}
                    className="rounded p-1 text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
                    title={t("fileEditor.navForward")}
                >
                    <ArrowRight className="h-3.5 w-3.5" />
                </button>

                {/* Refresh */}
                <button
                    onClick={handleRefresh}
                    className="rounded p-1 text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
                    title={t("fileEditor.navRefresh")}
                >
                    <RefreshCw className="h-3.5 w-3.5" />
                </button>

                {/* Editable URL bar — now an <input> that can be focused and edited */}
                <div className="mx-1 flex-1 overflow-hidden rounded bg-modal-surface">
                    <input
                        type="text"
                        value={isEditingUrl ? urlInputValue : currentUrl}
                        onChange={(e) => { setUrlInputValue(e.target.value); setIsEditingUrl(true); }}
                        onFocus={() => { setUrlInputValue(currentUrl); setIsEditingUrl(true); }}
                        onBlur={handleUrlBlur}
                        onKeyDown={handleUrlKeyDown}
                        className="w-full border-0 bg-transparent px-2 py-1 text-xs text-text-secondary outline-none placeholder-text-disabled focus:ring-0  dark:focus:text-zinc-100"
                        placeholder={currentUrl}
                    />
                </div>

                {/* Open in external browser */}
                <a
                    href={currentUrl}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="flex shrink-0 items-center gap-1 rounded px-2 py-1 text-xs text-info transition-colors hover:bg-info-surface"
                    title={t("fileEditor.openInBrowser")}
                >
                    <ExternalLink className="h-3.5 w-3.5" />
                </a>
            </div>

            {/* ── Iframe container ── */}
            <div className="relative flex-1">
                {/* Loading spinner */}
                {loading && (
                    <div className="absolute inset-0 z-10 flex flex-col items-center justify-center gap-3 bg-right-panel">
                        <Loader2 className="h-8 w-8 animate-spin text-text-tertiary" />
                        <span className="text-sm text-text-tertiary">{t("fileEditor.loadingUrl")}</span>
                    </div>
                )}

                {/* Slow-load hint (shows after 5s if page hasn't loaded) — non-blocking */}
                {showSlowHint && loading && (
                    <div className="absolute bottom-0 left-0 right-0 z-20 flex items-center justify-center gap-2 bg-warning-surface px-3 py-2 text-xs text-warning-quiet">
                        <span>{t("fileEditor.iframeBlockedHint")}</span>
                        <a
                            href={currentUrl}
                            target="_blank"
                            rel="noopener noreferrer"
                            className="inline-flex items-center gap-1 font-medium text-info underline underline-offset-2 hover:text-info"
                        >
                            {t("fileEditor.openInBrowser")}
                            <ExternalLink className="h-3 w-3" />
                        </a>
                    </div>
                )}

                {/* Iframe — allow-popups so target="_blank" links navigate or open in system browser */}
                <iframe
                    ref={iframeRef}
                    key={currentUrl}
                    src={currentUrl}
                    className="h-full w-full border-0 bg-modal-surface"
                    sandbox="allow-scripts allow-same-origin allow-forms allow-popups"
                    title={fileName}
                    onLoad={handleLoad}
                />
            </div>
        </div>
    );
}