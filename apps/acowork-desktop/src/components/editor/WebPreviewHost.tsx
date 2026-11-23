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
    type PreviewState,
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
 * Creates in flight, per session key.
 *
 * The create effect cannot claim the store binding before its `await`, so two
 * mounts of the same key both see "no session yet" and both ask the backend to
 * open one. React's `StrictMode` — on in `main.tsx` — mounts every effect twice
 * in development, which makes that not a rare race but the *normal* path. The
 * second native webview then fails (`WebView2` refuses concurrent controller
 * creation: `ERROR_INVALID_STATE`), and both attempts leave the panel unusable.
 *
 * Sharing the promise makes the create idempotent without making the effect
 * synchronous: the second caller waits on the first one's result instead of
 * opening a second webview.
 */
const inFlightCreates = new Map<string, Promise<PreviewState>>();

/**
 * Live hosts per session key — the ownership rule for a session.
 *
 * A session belongs to its `sessionKey` while at least one host mounted for that
 * key is alive. A create can resolve after the host that started it is gone
 * (tab closed, or a `StrictMode` unmount), and by then `FileEditorPanel`'s tab
 * disposer has already run and found nothing to close: the session would outlive
 * every owner, keeping a webview alive behind a key nobody can address twice
 * (the backend only reaps it on eviction). So the creating host asks this map
 * whether the key still has an owner before deciding to destroy what it made.
 *
 * A counter, not the store's `labels`, because the store binding is only written
 * *after* a create resolves — which is exactly the moment being decided.
 */
const liveHosts = new Map<string, number>();

function enterHost(sessionKey: string): void {
    liveHosts.set(sessionKey, (liveHosts.get(sessionKey) ?? 0) + 1);
}

function leaveHost(sessionKey: string): void {
    const remaining = (liveHosts.get(sessionKey) ?? 1) - 1;
    if (remaining > 0) liveHosts.set(sessionKey, remaining);
    else liveHosts.delete(sessionKey);
}

function openOnce(sessionKey: string, url: string, bounds: PreviewBounds): Promise<PreviewState> {
    const pending = inFlightCreates.get(sessionKey);
    if (pending) return pending;
    const created = openPreview(url, bounds).finally(() => inFlightCreates.delete(sessionKey));
    inFlightCreates.set(sessionKey, created);
    return created;
}

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
        enterHost(sessionKey);
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
                const state = await openOnce(sessionKey, url, bounds);
                if (cancelled) {
                    // This mount went away while its create was in flight. If the
                    // key still has a live host (a `StrictMode` twin, or the tab
                    // re-mounted), that host adopts the session on the same
                    // promise — closing it here would yank the page out from
                    // under it. With no host left, this session has no owner at
                    // all: destroy it, or the tab disposer can never find it.
                    if (!liveHosts.has(sessionKey)) {
                        void closePreview(state.label).catch(() => {});
                    }
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
            leaveHost(sessionKey);
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