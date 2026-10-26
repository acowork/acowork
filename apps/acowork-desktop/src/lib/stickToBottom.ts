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
 * The bit is written by a scroll event, a thread switch, or a send — never by
 * binding a container: at startup the page lands after the container mounts,
 * and a fresh (or re-attached) node sits at `scrollTop === 0` below taller
 * content, which is a layout fact, not a reader decision (see `bind`).
 *
 * The mobile copy also carries a top-trigger band for upward pagination; the
 * desktop inbox renders one newest page and has no `load earlier`, so that
 * half is left behind rather than stubbed out.
 */
import { useCallback, useLayoutEffect, useRef, useState } from "react";

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
  /**
   * The surface's "take me to the bottom" mechanism, when a bare
   * `scrollTop = scrollHeight` write is not one. It is not, once the list is
   * virtualized: the total height is an estimate that converges as rows get
   * measured, and on a session switch the virtualizer may not even own its
   * scroll element yet (React 19 attaches the parent's ref after the child's
   * layout effects). A single write then lands at the top (nothing sized) or
   * mid-list (estimate overshot) and nothing re-pins afterwards. A
   * virtualized surface hands in its list handle's `scrollToBottom`, which
   * ARMS a convergence loop — every later commit re-pins until the layout
   * settles. Returning true means "positioned or armed"; returning false
   * means the delegate could not take responsibility (e.g. an empty list)
   * and the raw write below stays as the fallback.
   */
  private goToBottom: (() => boolean) | null = null;
  /** Fired on the unpinned -> pinned flip only. The unread badge has to go
   *  away when the reader scrolls down to meet their messages, and that is a
   *  scroll, not a render: without this the screen cannot see it. */
  private onBecamePinned: (() => void) | undefined = undefined;
  /** Fired on EVERY pinned/unpinned flip (both directions, programmatic
   *  re-pin included). The screen mirrors this into a reactive flag so it can
   *  show a "back to bottom" affordance while the reader is scrolled up —
   *  `pinned` itself is a plain field, so reading it never re-renders. */
  private onPinnedChange: ((pinned: boolean) => void) | undefined = undefined;
  private onScroll = () => {
    if (!this.el) return;
    const was = this.pinned;
    const next = isNearBottom(this.el);
    this.setPinned(next);
    // An arrival is a SCROLL, never a programmatic re-pin: only the reader's
    // own scroll-down clears the unread badge, so the receipt fires here and
    // nowhere else (a `refresh`/`stickNow` re-pin or a container adopt must
    // not fake one).
    if (next && !was) this.onBecamePinned?.();
  };

  /** Single mutation point for `pinned`: keeps `onBecamePinned` firing only
   *  on an arrival (unpinned -> pinned, so a scroll-up or an already-pinned
   *  auto-scroll never emits a receipt) while `onPinnedChange` reports every
   *  transition. No-op when the bit is unchanged. */
  private setPinned(next: boolean): void {
    if (this.pinned === next) return;
    this.pinned = next;
    this.onPinnedChange?.(next);
  }

  /** Install (or clear) the bottom-positioning mechanism. Called by the hook
   *  binding when the surface has a virtualized list to delegate to. */
  setGoToBottom(fn: (() => boolean) | null): void {
    this.goToBottom = fn;
  }

  /** The single bottom-write point: prefer the surface's mechanism, fall
   *  back to the raw write. Never call `el.scrollTop =` anywhere else. */
  private pinToBottom(): void {
    if (this.goToBottom?.() === true) return;
    if (this.el) this.el.scrollTop = this.el.scrollHeight;
  }

  /** Callback-ref binding: attach the currently mounted container (null detaches). */
  bind(
    el: ScrollEl | null,
    onBecamePinned?: () => void,
    onPinnedChange?: (pinned: boolean) => void,
  ): void {
    if (this.el === el) {
      this.onBecamePinned = onBecamePinned;
      this.onPinnedChange = onPinnedChange;
      return;
    }
    if (this.el) this.el.removeEventListener("scroll", this.onScroll);
    this.el = el;
    this.onBecamePinned = onBecamePinned;
    this.onPinnedChange = onPinnedChange;
    // The follow bit is deliberately NOT re-derived from geometry here: a
    // container's scroll offset carries no reader intent.
    //   - The normal startup order is container-mounts-then-page-arrives, so
    //     the commit that binds already has (or shortly gets) content taller
    //     than the viewport under an untouched `scrollTop === 0`. Reading that
    //     as "scrolled up" vetoes the follow for the whole session and leaves
    //     the reader staring at the OLDEST row of the newest page.
    //   - A callback ref whose identity changes per render is detached with
    //     `null` and re-attached to the SAME node, so such an adopt would
    //     fire on every render, not just on a remount.
    // Intent is still tracked exactly where it exists: a real scroll event
    // (`onScroll`) — which the browser also dispatches for a programmatic
    // `scrollIntoView` locate jump — plus a view switch in `refresh` and the
    // send path's `stickNow`.
    if (el) {
      el.addEventListener("scroll", this.onScroll, { passive: true });
      // A container attached while the reader is still following is positioned
      // on arrival: screens swap the container across early-return branches, so
      // the page can well have landed BEFORE this node existed, and the refresh
      // that carried it ran against the previous container (or none). A reader
      // who had scrolled up (`pinned === false`) is left exactly where they are.
      if (this.pinned) this.pinToBottom();
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
      this.setPinned(true);
    }
    if (this.pinned) this.pinToBottom();
  }

  /** Re-claim the follow (the send path calls this before the echo lands). */
  stickNow(): void {
    if (!this.el) return;
    this.setPinned(true);
    this.pinToBottom();
  }
}

export function useStickToBottom(
  deps: readonly unknown[],
  forceKey?: string | null,
  onBecamePinned?: () => void,
  /** Number of prepended (older) rows currently shown. When this increases the
   *  commit inserted content ABOVE the viewport, so the hook compensates
   *  `scrollTop` by the height delta to keep the reader exactly where they
   *  were — the newly loaded rows and the "load earlier" control move up out
   *  of view and are reached again by scrolling up. This replaces the browser's
   *  native `overflow-anchor`, which mis-anchors to the top control (it keeps
   *  the button pinned and shoves the reading position down). Pass `undefined`
   *  on surfaces with no upward pagination. The caller must also set
   *  `overflow-anchor: none` on the viewport so the two mechanisms do not both
   *  compensate. */
  prependSignal?: number,
  /** The surface's bottom-positioning mechanism — see `FollowController`
   *  `goToBottom`. A virtualized transcript hands in its list handle's
   *  `scrollToBottom` (which arms measurement convergence); without it a
   *  single `scrollTop = scrollHeight` write lands short on a list whose
   *  total height is still an estimate. */
  goToBottom?: () => boolean,
): {
  ref: (el: HTMLDivElement | null) => void;
  stickNow: () => void;
  /** Read the follow state without subscribing to it: an arrival effect needs
   *  to know whether the new bubble landed in view, and a scroll is not a
   *  render, so making this reactive would re-introduce the hijack. */
  isPinned: () => boolean;
  /** Reactive mirror of `isPinned` — drives the "back to bottom" affordance.
   *  True while the reader is at (or has never left) the tail; flips false the
   *  moment they scroll up, so a screen can show the arrow WITHOUT any content
   *  change re-yanking the viewport (the scroll-hijack the rule forbids). */
  atBottom: boolean;
} {
  const ctrl = useRef<FollowController | null>(null);
  if (!ctrl.current) ctrl.current = new FollowController();
  // Keep the latest callback without re-creating the ref binding.
  const pinnedRef = useRef(onBecamePinned);
  pinnedRef.current = onBecamePinned;
  // The bottom-positioning mechanism is kept in a ref too: the controller holds
  // one stable wrapper across renders and always calls the latest delegate.
  const goBottomRef = useRef(goToBottom);
  goBottomRef.current = goToBottom;
  ctrl.current.setGoToBottom(goToBottom ? () => goBottomRef.current!() : null);
  // Reactive follow flag: the controller reports every pinned flip into this
  // state so the arrow can appear/disappear. Defaults to pinned (a fresh view
  // opens at the tail), matching the controller's own initial bit.
  const [atBottom, setAtBottom] = useState(true);

  // Callback ref, not a ref object: screens swap the scroll container across
  // early-return branches, and the listener must always sit on the element
  // that is actually mounted. `elRef` mirrors it so the prepend-anchoring
  // layout effect can read/write the scroll offset directly.
  const elRef = useRef<HTMLDivElement | null>(null);
  const ref = useCallback((el: HTMLDivElement | null) => {
    elRef.current = el;
    ctrl.current!.bind(el, () => pinnedRef.current?.(), setAtBottom);
  }, [setAtBottom]);

  // Prepend anchoring: remember the last committed scroll height so the next
  // time `prependSignal` grows we can shift the viewport by exactly the height
  // added above it.
  const lastScrollHeightRef = useRef(0);
  const lastPrependRef = useRef<number | undefined>(undefined);

  useLayoutEffect(() => {
    const el = elRef.current;
    if (!el) return;
    const prevPrepend = lastPrependRef.current;
    const prevHeight = lastScrollHeightRef.current;
    if (
      prependSignal !== undefined &&
      prevPrepend !== undefined &&
      prependSignal > prevPrepend &&
      prevHeight > 0
    ) {
      const delta = el.scrollHeight - prevHeight;
      // Only shift when the container actually grew (content added above the
      // viewport); a transient shrink (images settling) must not pull the
      // reader up. Runs before paint, so there is no visible flash.
      if (delta > 0) el.scrollTop += delta;
    }
    lastPrependRef.current = prependSignal;
    lastScrollHeightRef.current = el.scrollHeight;
  });

  // The follow runs before paint, in the layout phase: the newest page arrives
  // in the same commit as its rows, so a passive effect would first paint the
  // reader the top of that page (the oldest row) and only then jump — the flash
  // the startup bug looks like. Keeping it a layout effect also leaves
  // `useLocateMessage` (passive, declared after this hook at the call site) as
  // the last writer, so a locate jump still wins over the follow.
  useLayoutEffect(() => {
    ctrl.current!.refresh(forceKey);
    // `deps` is the caller's trigger list — fixed length per call site.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [forceKey, ...deps]);

  const stickNow = useCallback(() => {
    ctrl.current!.stickNow();
  }, []);

  const isPinned = useCallback(() => ctrl.current!.pinned, []);

  return { ref, stickNow, isPinned, atBottom };
}
