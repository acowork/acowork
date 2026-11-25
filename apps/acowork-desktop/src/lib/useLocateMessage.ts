/**
 * Locate-and-flash for the chat-session surfaces (AgentChatView /
 * GroupChatPanel), mirroring the execution view's `pendingLocate` interaction
 * (ADR-097 / ADR-081 §4.2). `chatSessionStore.jumpToMessage` fetches a window
 * around a search hit and stamps `pendingLocateSeq`; this hook scrolls that row
 * into view, flashes it, and clears the stamp after the fade.
 *
 * The consumer must render each message row with `data-seq={row.seq}` on a DOM
 * node inside the scroll container, and call this AFTER `useStickToBottom` so
 * the center-scroll wins over the follow-to-bottom on the jump commit.
 *
 * The list handle comes FIRST, mounted row or not (render-perf L2): a
 * `scrollIntoView` lands against the row heights measured SO FAR, and on a
 * window that was just replaced the rows above the target then measure in
 * taller than their estimate and push it back out of view — the "it loaded the
 * page but not the row" first click. The handle's key scroll re-centres across
 * the commits that follow (`VirtualRowList`). `scrollIntoView` stays as the
 * fallback for surfaces without a virtualized handle, and only when NEITHER
 * path can place the row is the stamp stale.
 */
import { useEffect, useRef, useState } from "react";

/** How long the located row stays highlighted before fading. */
const LOCATE_FADE_MS = 5000;

export function useLocateMessage(
  /** Read the mounted scroll container (the panel's viewport element). */
  getContainer: () => HTMLElement | null,
  /** The `seq` to locate, from the window's `pendingLocateSeq`. */
  pendingLocateSeq: number | null,
  /** Clear the stamp once located (or when the row is genuinely absent). */
  clearLocate: () => void,
  /** Virtualization fallback: center-scroll the row by seq through the
   *  list handle. Return false when the seq is not in the loaded window. */
  scrollToSeq?: (seq: number) => boolean,
  /** Fired when the locate scroll is issued (either path). A locate parks
   *  the reader mid-history ON PURPOSE — the surface must take them out of
   *  the follow-to-bottom here. The follow rule's scroll-event unpin is
   *  direction-guarded (a virtualized pane's stale event must not fake
   *  intent), so a downward jump would otherwise keep the follow alive and
   *  the persistent anchor would ride the next commit straight back to the
   *  tail, losing the locate. */
  onLocate?: () => void,
): number | null {
  const [highlightSeq, setHighlightSeq] = useState<number | null>(null);
  const timerRef = useRef<number | null>(null);

  useEffect(() => {
    if (pendingLocateSeq == null) return;
    const target =
      getContainer()?.querySelector<HTMLElement>(`[data-seq="${pendingLocateSeq}"]`) ?? null;
    if (scrollToSeq?.(pendingLocateSeq)) {
      // Off-viewport in a virtualized window: the handle moved the pane (and
      // keeps it centred while measurements settle); the row mounts on the
      // next commit and paints the flash from there.
      setHighlightSeq(pendingLocateSeq);
      onLocate?.();
    } else if (target) {
      setHighlightSeq(pendingLocateSeq);
      onLocate?.();
      target.scrollIntoView({ block: "center" });
    } else {
      // `jumpToMessage` only stamps a row it just fetched into the window, so
      // a miss here means the window moved on (a wake reset it to the tail).
      // Drop the stale stamp rather than strand a pending highlight.
      clearLocate();
      return;
    }
    if (timerRef.current) window.clearTimeout(timerRef.current);
    timerRef.current = window.setTimeout(() => {
      setHighlightSeq(null);
      clearLocate();
    }, LOCATE_FADE_MS);
    return () => {
      if (timerRef.current) window.clearTimeout(timerRef.current);
    };
    // Intentionally keyed only on the stamp: the container query is read fresh
    // per jump, and re-running on every render would re-fire the scroll.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pendingLocateSeq]);

  return highlightSeq;
}
