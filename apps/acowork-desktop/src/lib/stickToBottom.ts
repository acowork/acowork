/**
 * The chat follow rule — desktop twin of the mobile `lib/stickToBottom.ts`.
 *
 * A message list must only follow new content while the user is AT the
 * bottom. Reading history upward is an explicit act, and any refresh — a
 * wake-up signal, a poll tick, a re-render with a fresh object — yanking the
 * viewport back down destroys that position.
 *
 * `InboxPanel` used to run `bottomRef.current?.scrollIntoView()` on
 * `[messages.length, activePeerId]`. With the store now re-reading the thread
 * on every `acowork/users/{me}/chat/changed` signal (design doc 25 §8), that
 * effect became a signal-driven scroll hijack: the more people you talk to,
 * the less often you can read anything above the last bubble.
 *
 * The rule lives in a React-free `FollowController` so it is unit-testable
 * without mounting a renderer, and the hook below is the thin binding screens
 * use:
 *
 *   - pinned (gap from bottom <= STICK_PX): refresh() scrolls to the bottom;
 *   - unpinned: nothing moves the list until the user scrolls back down,
 *     switches thread (`forceKey` changed), or sends a message (`stickNow` —
 *     the IM convention that sending re-claims the follow).
 *
 * The mobile copy also carries a top-trigger band for upward pagination; the
 * desktop inbox renders one newest page and has no `load earlier`, so that
 * half is left behind rather than stubbed out.
 */
import { useCallback, useEffect, useRef } from "react";

/** Distance from the bottom that still counts as "at the bottom". */
export const STICK_PX = 80;

export function isNearBottom(el: {
  scrollHeight: number;
  scrollTop: number;
  clientHeight: number;
}): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= STICK_PX;
}

/** The scroll-container surface the controller touches (HTMLDivElement fits). */
export interface ScrollEl {
  scrollHeight: number;
  scrollTop: number;
  clientHeight: number;
  addEventListener(type: "scroll", fn: () => void, opts?: unknown): void;
  removeEventListener(type: "scroll", fn: () => void): void;
}

export class FollowController {
  /** Public for tests: the one bit this whole rule reduces to. */
  pinned = true;
  private el: ScrollEl | null = null;
  private lastKey: string | null | undefined = undefined;
  /** Fired on the unpinned -> pinned flip only. The unread badge has to go
   *  away when the reader scrolls down to meet their messages, and that is a
   *  scroll, not a render: without this the screen cannot see it. */
  private onBecamePinned: (() => void) | undefined = undefined;
  private onScroll = () => {
    if (!this.el) return;
    const was = this.pinned;
    this.pinned = isNearBottom(this.el);
    // The auto-scroll a `refresh()` performs while already pinned has `was`
    // true, so it never re-fires — this only reports a user-driven arrival.
    if (this.pinned && !was) this.onBecamePinned?.();
  };

  /** Callback-ref binding: attach the currently mounted container (null detaches). */
  bind(el: ScrollEl | null, onBecamePinned?: () => void): void {
    if (this.el === el) {
      this.onBecamePinned = onBecamePinned;
      return;
    }
    if (this.el) this.el.removeEventListener("scroll", this.onScroll);
    this.el = el;
    this.onBecamePinned = onBecamePinned;
    if (el) {
      el.addEventListener("scroll", this.onScroll, { passive: true });
      // Adopt whatever position a (re)mounted container sits at.
      this.pinned = isNearBottom(el);
    }
  }

  /**
   * Called by the screen on every content change.
   * @param forceKey identity of what is being viewed (peer id); a change
   *                 re-pins even if the user scrolled up before it.
   */
  refresh(forceKey?: string | null): void {
    if (!this.el) return;
    if (forceKey !== this.lastKey) {
      this.lastKey = forceKey ?? null;
      this.pinned = true;
    }
    if (this.pinned) this.el.scrollTop = this.el.scrollHeight;
  }

  /** Re-claim the follow (the send path calls this before the echo lands). */
  stickNow(): void {
    if (!this.el) return;
    this.pinned = true;
    this.el.scrollTop = this.el.scrollHeight;
  }
}

export function useStickToBottom(
  deps: readonly unknown[],
  forceKey?: string | null,
  onBecamePinned?: () => void,
): {
  ref: (el: HTMLDivElement | null) => void;
  stickNow: () => void;
  /** Read the follow state without subscribing to it: an arrival effect needs
   *  to know whether the new bubble landed in view, and a scroll is not a
   *  render, so making this reactive would re-introduce the hijack. */
  isPinned: () => boolean;
} {
  const ctrl = useRef<FollowController | null>(null);
  if (!ctrl.current) ctrl.current = new FollowController();
  // Keep the latest callback without re-creating the ref binding.
  const pinnedRef = useRef(onBecamePinned);
  pinnedRef.current = onBecamePinned;

  // Callback ref, not a ref object: screens swap the scroll container across
  // early-return branches, and the listener must always sit on the element
  // that is actually mounted.
  const ref = useCallback((el: HTMLDivElement | null) => {
    ctrl.current!.bind(el, () => pinnedRef.current?.());
  }, []);

  useEffect(() => {
    ctrl.current!.refresh(forceKey);
    // `deps` is the caller's trigger list — fixed length per call site.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [forceKey, ...deps]);

  const stickNow = useCallback(() => {
    ctrl.current!.stickNow();
  }, []);

  const isPinned = useCallback(() => ctrl.current!.pinned, []);

  return { ref, stickNow, isPinned };
}
