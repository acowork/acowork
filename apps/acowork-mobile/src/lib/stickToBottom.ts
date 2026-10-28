/**
 * The chat follow rule (fix for "history snaps back to the bottom").
 *
 * A message list must only follow new content while the user is AT the
 * bottom. Reading history upward is an explicit act, and any refresh —
 * a poll tick, a WS state replay, a re-render with a fresh status object
 * — yanking the viewport back down destroys it.
 *
 * The rule lives in a React-free `FollowController` so it is unit-testable
 * without mounting a renderer (render-based tests in this workspace die on
 * the double-React-instance vitest issue that already breaks
 * `screens-smoke`); the hook below is the thin binding screens use:
 *
 *   - pinned (gap from bottom <= STICK_PX): refresh() scrolls to the bottom;
 *   - unpinned: nothing moves the list until the user scrolls back down,
 *     switches session/thread (`forceKey` changed), or sends a message
 *     (`stickNow` — the IM convention that sending re-claims the follow).
 *
 * The bit is written by a scroll event, a session switch, or a send — never by
 * binding a container: at startup the page lands after the container mounts,
 * and a fresh (or re-attached) node sits at `scrollTop === 0` below taller
 * content, which is a layout fact, not a reader decision (see `bind`).
 *
 * Screens used to `el.scrollTop = el.scrollHeight` unconditionally on
 * [messages.length, status]; combined with the 2 s snapshot poll that
 * stored a freshly parsed `status` object every tick, it became a timer
 * driven scroll hijack. The store now also keeps the status reference
 * stable when the content has not changed (chatStore.sameStatus); this
 * controller is what makes the remaining REAL refreshes respect user
 * position.
 */
import { useCallback, useLayoutEffect, useRef, useState } from 'react'

/** Distance from the bottom that still counts as "at the bottom". */
export const STICK_PX = 80

/** Distance from the TOP that counts as "the user wants earlier history".
 *  The screen's onNearTop hook fires on any scroll event landing inside
 *  this band (the store's loadEarlier guards re-entrancy). */
export const TOP_TRIGGER_PX = 120

export function isNearBottom(el: {
  scrollHeight: number
  scrollTop: number
  clientHeight: number
}): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= STICK_PX
}

/** The scroll-container surface the controller touches (HTMLDivElement fits). */
export interface ScrollEl {
  scrollHeight: number
  scrollTop: number
  clientHeight: number
  addEventListener(type: 'scroll', fn: () => void, opts?: unknown): void
  removeEventListener(type: 'scroll', fn: () => void): void
}

export class FollowController {
  /** Public for tests: the one bit this whole rule reduces to. */
  pinned = true
  private el: ScrollEl | null = null
  private lastKey: string | null | undefined = undefined
  private onNearTop: (() => void) | undefined = undefined
  /** Fired on the unpinned -> pinned flip only. The unread badge has to go
   *  away when the reader scrolls down to meet their messages, and that is a
   *  scroll, not a render: without this the screen cannot see it. */
  private onBecamePinned: (() => void) | undefined = undefined
  /** Fired on EVERY pinned/unpinned flip (both directions, programmatic
   *  re-pin included). The screen mirrors this into a reactive flag so it can
   *  show a "back to bottom" affordance while the reader is scrolled up —
   *  `pinned` itself is a plain field, so reading it never re-renders. */
  private onPinnedChange: ((pinned: boolean) => void) | undefined = undefined
  /** Last scroll offset seen by `onScroll` — the direction guard's baseline. */
  private lastTop = 0
  private onScroll = () => {
    if (!this.el) return
    const was = this.pinned
    if (isNearBottom(this.el)) {
      this.setPinned(true)
      // The auto-scroll a `refresh()` performs while already pinned has `was`
      // true, so it never re-fires — this only reports a user-driven arrival.
      if (!was) this.onBecamePinned?.()
    } else if (this.el.scrollTop < this.lastTop - 1) {
      // Away from the bottom AND a genuine upward move: the reader left.
      // Geometry alone cannot make this call — the scroll event from our own
      // bottom-pin write can be delivered after a later commit has grown the
      // content, and reads "far from the bottom" without anyone having moved.
      // Unpinning on that stale event abandoned the follow mid-convergence
      // (the agent-switch drift: the transcript landed at the bottom and then
      // drifted up as measurements settled). Our writes only ever move DOWN,
      // and a clamp from a content shrink lands AT the new bottom (the near
      // branch above), so a decrease can only come from the reader. A
      // deliberately downward programmatic scroll releases the follow through
      // the explicit `unpin()`. Same discipline as the desktop twin.
      this.setPinned(false)
    }
    this.lastTop = this.el.scrollTop
    // Top-edge pagination trigger: fires on USER scrolls into the band; a
    // programmatic jump to the bottom lands far from the top, so the
    // mount-time scroll-to-bottom cannot spuriously trip it.
    if (this.el.scrollTop <= TOP_TRIGGER_PX) this.onNearTop?.()
  }

  /** Single mutation point for `pinned`: `onPinnedChange` reports every flip
   *  (so the screen can show a "back to bottom" affordance), while
   *  `onBecamePinned` stays a scroll-only receipt. No-op when unchanged. */
  private setPinned(next: boolean): void {
    if (this.pinned === next) return
    this.pinned = next
    this.onPinnedChange?.(next)
  }

  /** Release the follow explicitly: a programmatic scroll that parks the
   *  reader mid-history moves DOWNWARD, which carries no signal the direction
   *  guard may read as intent. Nothing on mobile calls this yet — the locate
   *  jump that scrolls to an off-screen message is a desktop-only path today —
   *  but the guard cannot be escaped any other way for a downward move, so the
   *  escape hatch stays complete for that caller. */
  unpin(): void {
    this.setPinned(false)
  }

  /** Callback-ref binding: attach the currently mounted container (null detaches). */
  bind(
    el: ScrollEl | null,
    onNearTop?: () => void,
    onBecamePinned?: () => void,
    onPinnedChange?: (pinned: boolean) => void,
  ): void {
    if (this.el === el) {
      this.onNearTop = onNearTop
      this.onBecamePinned = onBecamePinned
      this.onPinnedChange = onPinnedChange
      return
    }
    if (this.el) this.el.removeEventListener('scroll', this.onScroll)
    this.el = el
    this.onNearTop = onNearTop
    this.onBecamePinned = onBecamePinned
    this.onPinnedChange = onPinnedChange
    if (el) {
      el.addEventListener('scroll', this.onScroll, { passive: true })
      // The follow bit is deliberately NOT re-derived from geometry here: a
      // container's scroll offset carries no reader intent. The startup order is
      // container-mounts-then-page-arrives, so reading the offset would take the
      // untouched `scrollTop === 0` under brand-new content for a scroll-up and
      // veto the follow for the whole session — the reader would open the
      // conversation on its OLDEST row. Intent is tracked where it exists: the
      // `onScroll` listener (a browser also fires it for a programmatic jump).
      //
      // What attaching DOES do is position a container that appears while the
      // reader is still following: screens swap the scroll container across
      // early-return branches, so the page can exist BEFORE this node does and
      // the refresh that carried it ran against no container at all. A reader
      // who had scrolled up (`pinned === false`) is left exactly where they are.
      if (this.pinned) el.scrollTop = el.scrollHeight
    }
  }

  /**
   * Called by the screen on every content/status change.
   * @param forceKey identity of what is being viewed (session id, peer id);
   *                 a change re-pins even if the user scrolled up before it.
   */
  refresh(forceKey?: string | null): void {
    if (!this.el) return
    if (forceKey !== this.lastKey) {
      this.lastKey = forceKey ?? null
      this.setPinned(true)
    }
    if (this.pinned) this.el.scrollTop = this.el.scrollHeight
  }

  /** Re-claim the follow (the send path calls this before the echo lands). */
  stickNow(): void {
    if (!this.el) return
    this.setPinned(true)
    this.el.scrollTop = this.el.scrollHeight
  }
}

export function useStickToBottom(
  deps: readonly unknown[],
  forceKey?: string | null,
  onNearTop?: () => void,
  onBecamePinned?: () => void,
): {
  ref: (el: HTMLDivElement | null) => void
  stickNow: () => void
  /** Release the follow explicitly — the exit for a programmatic scroll that
   *  parks the reader away from the bottom. The scroll-event unpin is
   *  direction-guarded (a stale bottom-pin event must not fake intent), so such
   *  a move has no other way out of the follow. No mobile screen scrolls
   *  programmatically today, so this is currently called only by tests; it is
   *  exposed alongside `stickNow` because that caller (a locate jump) is the
   *  natural next user of this controller. */
  unpin: () => void
  /** Read the follow state without subscribing to it: an arrival effect needs
   *  to know whether the new bubble landed in view, and a scroll is not a
   *  render, so making this reactive would re-introduce the hijack. */
  isPinned: () => boolean
  /** Reactive mirror of `isPinned` — true while the reader is at (or has never
   *  left) the tail. Flips false the moment they scroll up, so a screen can
   *  show a "back to bottom" arrow WITHOUT any content change re-yanking the
   *  viewport (the scroll hijack the rule forbids). */
  atBottom: boolean
} {
  const ctrl = useRef<FollowController | null>(null)
  if (!ctrl.current) ctrl.current = new FollowController()
  // Keep the latest callback without re-creating the ref binding.
  const topRef = useRef(onNearTop)
  topRef.current = onNearTop
  const bottomRef = useRef(onBecamePinned)
  bottomRef.current = onBecamePinned
  // Reactive follow flag: the controller reports every pinned flip into this
  // state so the arrow can appear/disappear. Defaults to pinned (a fresh view
  // opens at the tail), matching the controller's own initial bit.
  const [atBottom, setAtBottom] = useState(true)

  // Callback ref, not a ref object: screens swap the scroll container across
  // early-return branches, and the listener must always sit on the element
  // that is actually mounted.
  const ref = useCallback((el: HTMLDivElement | null) => {
    ctrl.current!.bind(
      el,
      () => topRef.current?.(),
      () => bottomRef.current?.(),
      setAtBottom,
    )
  }, [])

  // The follow runs before paint, in the layout phase: the newest page arrives
  // in the same commit as its bubbles, so a passive effect would first paint the
  // reader the top of that page (the oldest message) and only then jump.
  useLayoutEffect(() => {
    ctrl.current!.refresh(forceKey)
    // `deps` is the caller's trigger list — fixed length per call site.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [forceKey, ...deps])

  const stickNow = useCallback(() => {
    ctrl.current!.stickNow()
  }, [])

  const unpin = useCallback(() => {
    ctrl.current!.unpin()
  }, [])

  const isPinned = useCallback(() => ctrl.current!.pinned, [])

  return { ref, stickNow, unpin, isPinned, atBottom }
}
