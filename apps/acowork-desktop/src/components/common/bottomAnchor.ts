/**
 * Bottom-anchor decision for `VirtualMessageList`'s jump-to-bottom pin.
 *
 * `scrollToBottom()` arms the anchor, and while armed every commit re-pins
 * `scrollTop` to the (still growing) bottom so the viewport rides the async
 * layout — a single `scrollToIndex` cannot land on the bottom because
 * `estimateSize` underestimates never-measured blocks by hundreds of px.
 *
 * The anchor must disarm as soon as the USER scrolls away, otherwise it
 * fights a deliberate scroll-up.  The trap (two live repros, 2026-09-25) is
 * that the browser also clamps `scrollTop` downward when the CONTENT shrinks
 * — an over-estimated block getting measured shorter, a window
 * replacement/eviction, the "Loading more..." row appearing and vanishing —
 * and that clamp looks exactly like a 16px user scroll-up.  Disarming there
 * abandoned the jump mid-way and left the pane a screen above the bottom.
 *
 * So: only an upward move at a height that did NOT shrink counts as intent.
 */

export const BOTTOM_PIN_RELEASE_PX = 16;

export type BottomPinAction = "pin" | "disarm";

export function decideBottomPin(input: {
  /** Current container scrollTop. */
  scrollTop: number;
  /** Current max scrollTop (scrollHeight - clientHeight). */
  max: number;
  /** The scrollTop this anchor pinned on the previous commit. */
  lastPinTop: number;
}): BottomPinAction {
  const { scrollTop, max, lastPinTop } = input;
  if (max >= lastPinTop && scrollTop < lastPinTop - BOTTOM_PIN_RELEASE_PX) {
    return "disarm";
  }
  return "pin";
}
