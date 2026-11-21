import { useCallback, useEffect, useRef, useState } from "react";
import { cn } from "../../lib/utils";
import {
    closePreview,
    navigatePreview,
    openPreview,
    parsePreviewError,
    setPreviewBounds,
    setPreviewVisible,
    type PreviewBounds,
    type PreviewError,
} from "../../lib/previewHost";
import { isParkedBounds, sameBounds, toPreviewBounds } from "../../lib/previewGeometry";
import { usePreviewSessionStore } from "../../stores/previewSessionStore";
import { useNativeOverlayCoverage } from "../../hooks/useNativeOverlayCoverage";

interface WebPreviewHostProps {
    /**
     * Stable identity of the owning surface (an editor tab id). Remounting with
     * the same key re-attaches to the live session instead of reloading the page.
     */
    sessionKey: string;
    /** The URL this surface *wants* displayed. Changes navigate the session. */
    url: string;
    /** Whether this surface is the visible tab. Inactive hosts keep their page loaded. */
    active: boolean;
    /** Reports creation/navigation failures to the chrome (it owns the error card). */
    onError?: (error: PreviewError) => void;
    className?: string;
}

/**
 * How many times a tab may silently rebuild its webview after the backend
 * reported the session gone. Eviction happens at most once per tab in practice;
 * a limit turns a pathological recreate loop into an error card instead of an
 * IPC storm.
 */
const MAX_AUTO_RECREATES = 2;

/**
 * Owns the lifecycle and geometry of one native preview webview.
 *
 * Responsibilities, in the order they matter:
 *
 *   1. **Measure** — the placeholder div is the source of truth for where the
 *      native surface belongs; `ResizeObserver` pushes integer bounds.
 *   2. **Create / re-attach** — a session is created once per `sessionKey` and
 *      then survives mounts, so switching tabs does not reload the page.
 *   3. **Visibility** — visible only when the tab is active, the rect is real,
 *      and no HTML overlay covers it (a native surface cannot be layered under
 *      a portal; see `useNativeOverlayCoverage`).
 *
 * This component renders an empty div and nothing else: everything visual for a
 * preview lives in the surrounding chrome, because HTML cannot be painted over
 * the webview.
 */
export function WebPreviewHost({
    sessionKey,
    url,
    active,
    onError,
    className,
}: WebPreviewHostProps) {
    const anchorRef = useRef<HTMLDivElement | null>(null);
    const boundsRef = useRef<PreviewBounds | null>(null);
    const [label, setLabel] = useState<string | null>(null);
    const [measureTick, setMeasureTick] = useState(0);
    const [reopenToken, setReopenToken] = useState(0);
    const reopenCountRef = useRef(0);
    const covered = useNativeOverlayCoverage(anchorRef, active);

    const attach = usePreviewSessionStore((s) => s.attach);
    const detach = usePreviewSessionStore((s) => s.detach);
    const setSourceUrl = usePreviewSessionStore((s) => s.setSourceUrl);

    // ── 1. Measure the placeholder ────────────────────────────────────────
    useEffect(() => {
        const element = anchorRef.current;
        if (!element) return;
        const measure = () => {
            const next = toPreviewBounds(element.getBoundingClientRect());
            const previous = boundsRef.current;
            if (previous && sameBounds(previous, next)) return;
            boundsRef.current = next;
            setMeasureTick((tick) => tick + 1);
        };
        measure();
        const observer = new ResizeObserver(measure);
        observer.observe(element);
        window.addEventListener("resize", measure);
        return () => {
            observer.disconnect();
            window.removeEventListener("resize", measure);
        };
    }, []);

    /**
     * Backend lost the session (LRU eviction, or a webview closed under us).
     * Forget the binding and rebuild once; past the limit, report instead of
     * looping.
     */
    const recreate = useCallback(
        (error: PreviewError) => {
            if (reopenCountRef.current >= MAX_AUTO_RECREATES) {
                onError?.(error);
                return;
            }
            reopenCountRef.current += 1;
            detach(sessionKey);
            setLabel(null);
            setReopenToken((token) => token + 1);
        },
        [detach, onError, sessionKey],
    );

    // ── 2. Create or re-attach the session ────────────────────────────────
    useEffect(() => {
        let cancelled = false;
        const store = usePreviewSessionStore.getState();
        const existing = store.labels[sessionKey];

        const fail = (error: unknown) => {
            const parsed = parsePreviewError(error);
            if (parsed.code === "unknown_preview") {
                recreate(parsed);
                return;
            }
            onError?.(parsed);
        };

        void (async () => {
            if (existing) {
                setLabel(existing);
                const sourceUrl = usePreviewSessionStore.getState().sourceUrls[sessionKey];
                if (sourceUrl === url) return;
                try {
                    await navigatePreview(existing, url);
                    if (cancelled) return;
                    setSourceUrl(sessionKey, url);
                } catch (error) {
                    if (!cancelled) fail(error);
                }
                return;
            }

            const bounds = boundsRef.current ?? { x: 0, y: 0, width: 0, height: 0 };
            try {
                const state = await openPreview(url, bounds);
                if (cancelled) {
                    // Unmounted while the webview was being created: close it so
                    // a stray native surface cannot outlive its React owner.
                    void closePreview(state.label).catch(() => {});
                    return;
                }
                attach(sessionKey, state);
                setSourceUrl(sessionKey, url);
                reopenCountRef.current = 0;
                setLabel(state.label);
            } catch (error) {
                if (!cancelled) fail(error);
            }
        })();

        return () => {
            cancelled = true;
        };
        // `attach`/`setSourceUrl` are stable store actions and `recreate` is
        // memoised on `sessionKey`; the disabled rule would otherwise recreate a
        // session on every parent re-render.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [sessionKey, url, reopenToken]);

    // ── 3. Push geometry + visibility ─────────────────────────────────────
    const wantVisible = active && !covered && !!label;
    useEffect(() => {
        if (!label) return;
        const bounds = boundsRef.current;
        if (!bounds) return;
        const visible = wantVisible && !isParkedBounds(bounds);
        let cancelled = false;
        void (async () => {
            try {
                await setPreviewBounds(label, bounds);
                if (cancelled) return;
                await setPreviewVisible(label, visible);
            } catch (error) {
                if (cancelled) return;
                const parsed = parsePreviewError(error);
                if (parsed.code === "unknown_preview") recreate(parsed);
            }
        })();
        return () => {
            cancelled = true;
        };
        // `measureTick` re-runs this effect when the rect changed but no other
        // dependency did.
    }, [label, wantVisible, measureTick, recreate]);

    // ── 4. Park on unmount — never destroy: the tab may come back ─────────
    useEffect(() => {
        return () => {
            if (label) void setPreviewVisible(label, false).catch(() => {});
        };
    }, [label]);

    return <div ref={anchorRef} className={cn("pointer-events-none absolute inset-0", className)} />;
}