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
import { useCallback, useLayoutEffect, useRef } from 'react'

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
  private onScroll = () => {
    if (!this.el) return
    const was = this.pinned
    this.pinned = isNearBottom(this.el)
    // The auto-scroll a `refresh()` performs while already pinned has `was`
    // true, so it never re-fires — this only reports a user-driven arrival.
    if (this.pinned && !was) this.onBecamePinned?.()
    // Top-edge pagination trigger: fires on USER scrolls into the band; a
    // programmatic jump to the bottom lands far from the top, so the
    // mount-time scroll-to-bottom cannot spuriously trip it.
    if (this.el.scrollTop <= TOP_TRIGGER_PX) this.onNearTop?.()
  }

  /** Callback-ref binding: attach the currently mounted container (null detaches). */
  bind(el: ScrollEl | null, onNearTop?: () => void, onBecamePinned?: () => void): void {
    if (this.el === el) {
      this.onNearTop = onNearTop
      this.onBecamePinned = onBecamePinned
      return
    }
    if (this.el) this.el.removeEventListener('scroll', this.onScroll)
    this.el = el
    this.onNearTop = onNearTop
    this.onBecamePinned = onBecamePinned
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
      this.pinned = true
    }
    if (this.pinned) this.el.scrollTop = this.el.scrollHeight
  }

  /** Re-claim the follow (the send path calls this before the echo lands). */
  stickNow(): void {
    if (!this.el) return
    this.pinned = true
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
  /** Read the follow state without subscribing to it: an arrival effect needs
   *  to know whether the new bubble landed in view, and a scroll is not a
   *  render, so making this reactive would re-introduce the hijack. */
  isPinned: () => boolean
} {
  const ctrl = useRef<FollowController | null>(null)
  if (!ctrl.current) ctrl.current = new FollowController()
  // Keep the latest callback without re-creating the ref binding.
  const topRef = useRef(onNearTop)
  topRef.current = onNearTop
  const bottomRef = useRef(onBecamePinned)
  bottomRef.current = onBecamePinned

  // Callback ref, not a ref object: screens swap the scroll container across
  // early-return branches, and the listener must always sit on the element
  // that is actually mounted.
  const ref = useCallback((el: HTMLDivElement | null) => {
    ctrl.current!.bind(el, () => topRef.current?.(), () => bottomRef.current?.())
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

  const isPinned = useCallback(() => ctrl.current!.pinned, [])

  return { ref, stickNow, isPinned }
}
