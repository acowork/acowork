import { useEffect, useState, type RefObject } from "react";

/**
 * Answers one question: **is a native preview webview currently covered by an
 * HTML overlay (modal, dropdown, context menu, lightbox)?**
 *
 * The fact that forces this hook: a child webview is a native surface. It is
 * composited *above* the HTML layer, so no `z-index` can put a React portal on
 * top of it — an open dialog would simply be hidden behind the live page, and
 * clicks on the dialog would land in the page. The only cure is to hide the
 * webview while something covers it (`preview_set_visible(label, false)`).
 *
 * Detection is a hit test rather than an instrumentation of every overlay:
 * portals are created in ~15 components, and any new one would silently
 * regress. `document.elementFromPoint` is unaffected by the native surface
 * (it queries the DOM, not the compositor), so sampling a few points inside the
 * placeholder rect tells us whether the topmost HTML element there is still our
 * own anchor.
 *
 * Failure direction is deliberate: uncertainty resolves to "covered", which
 * hides the webview — the app stays usable and nothing steals input. A dialog
 * that *fails* to be detected would be invisible, which is unrecoverable.
 *
 * Known limitation (documented in ADR-118): overlays with `pointer-events:
 * none` are invisible to `elementFromPoint` and will not trigger hiding. No
 * such overlay is used for input-blocking surfaces in this app.
 */
export function useNativeOverlayCoverage(
    ref: RefObject<HTMLElement | null>,
    enabled: boolean,
): boolean {
    const [covered, setCovered] = useState(false);

    useEffect(() => {
        if (!enabled) {
            setCovered(false);
            return;
        }
        const detect = () => typeof document.elementFromPoint === "function";
        if (!detect()) return; // jsdom / very old engines: assume uncovered

        const element = ref.current;
        if (!element) return;

        let frame = 0;
        let lastCheck = 0;

        const sample = () => {
            lastCheck = Date.now();
            const target = ref.current;
            if (!target) return;
            const rect = target.getBoundingClientRect();
            if (rect.width < 1 || rect.height < 1) {
                setCovered(false);
                return;
            }
            // Centre plus the four inset corners: covers both a full-screen
            // backdrop and a small dropdown sitting over one corner.
            const probes: ReadonlyArray<readonly [number, number]> = [
                [0.5, 0.5],
                [0.06, 0.06],
                [0.94, 0.06],
                [0.06, 0.94],
                [0.94, 0.94],
            ];
            for (const [fx, fy] of probes) {
                const hit = document.elementFromPoint(
                    rect.left + rect.width * fx,
                    rect.top + rect.height * fy,
                );
                if (hit && hit !== target && !target.contains(hit) && !hit.contains(target)) {
                    setCovered(true);
                    return;
                }
            }
            setCovered(false);
        };

        const schedule = (force = false) => {
            if (frame) return;
            frame = requestAnimationFrame(() => {
                frame = 0;
                // Chat streaming and resize drags mutate the DOM continuously;
                // a 100 ms floor keeps the hit test off the hot path.
                if (!force && Date.now() - lastCheck < 100) return;
                sample();
            });
        };

        const onViewportChange = () => schedule(true);
        const onScroll = () => schedule();

        const resizeObserver = new ResizeObserver(onViewportChange);
        resizeObserver.observe(element);
        const mutationObserver = new MutationObserver(onScroll);
        mutationObserver.observe(document.body, { childList: true, subtree: true });
        window.addEventListener("resize", onViewportChange);
        window.addEventListener("scroll", onScroll, true);

        sample();

        return () => {
            if (frame) cancelAnimationFrame(frame);
            resizeObserver.disconnect();
            mutationObserver.disconnect();
            window.removeEventListener("resize", onViewportChange);
            window.removeEventListener("scroll", onScroll, true);
        };
    }, [ref, enabled]);

    return covered;
}