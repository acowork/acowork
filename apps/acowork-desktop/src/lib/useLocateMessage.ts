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
): number | null {
  const [highlightSeq, setHighlightSeq] = useState<number | null>(null);
  const timerRef = useRef<number | null>(null);

  useEffect(() => {
    if (pendingLocateSeq == null) return;
    const target =
      getContainer()?.querySelector<HTMLElement>(`[data-seq="${pendingLocateSeq}"]`) ?? null;
    if (!target) {
      // `jumpToMessage` only stamps a row it just fetched into the window, so
      // a miss means the window moved on (a wake reset it to the tail). Drop
      // the stale stamp rather than strand a pending highlight.
      clearLocate();
      return;
    }
    setHighlightSeq(pendingLocateSeq);
    target.scrollIntoView({ block: "center" });
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
