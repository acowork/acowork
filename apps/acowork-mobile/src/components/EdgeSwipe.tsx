/**
 * Edge-swipe gestures. Direction is resolved ONCE on the first move and
 * then locked, because a chat screen offers two different horizontal
 * gestures (right = back, left = drawer) and letting the direction flip
 * mid-drag makes both feel broken.
 *
 * Vertical intent wins: if the first movement is more vertical than
 * horizontal, the gesture is handed to the scroll container and this
 * handler never engages.
 *
 * Horizontal scroll wins too: if the touch starts inside an ancestor
 * that actually overflows horizontally (e.g. a GFM table wrap), this
 * handler bails entirely so the browser's own scroll handles the motion.
 * Otherwise any horizontal drag on a wide table would race the 8 px
 * threshold and fire back / drawer the instant the user tried to read
 * across columns.
 */

import { useRef, useEffect, type ReactNode } from 'react'

const COMMIT_RATIO = 0.38
const COMMIT_VELOCITY = 0.35 // px per ms

export interface EdgeGesture {
  /** 'back' fires on right-swipe; 'drawer' on left-swipe. */
  onBack?: () => void
  onDrawer?: () => void
  enabled?: boolean
  children: ReactNode
}

/** Walk up from `target` looking for an ancestor whose `overflow-x` is
 *  auto/scroll AND whose content actually overflows. The second clause is
 *  important: many `overflow-x: auto` wrappers sit on the page that have
 *  `scrollWidth === clientWidth` at rest, and yielding those to the
 *  browser would block the user's intent to swipe-back from a static
 *  region. */
function findHorizontalScrollAncestor(el: Element | null): Element | null {
  let cur: Element | null = el
  while (cur) {
    const style = getComputedStyle(cur)
    const overflowX = style.overflowX
    if (
      (overflowX === 'auto' || overflowX === 'scroll') &&
      cur.scrollWidth > cur.clientWidth + 1
    ) {
      return cur
    }
    cur = cur.parentElement
  }
  return null
}

export function EdgeSwipe({ onBack, onDrawer, enabled = true, children }: EdgeGesture) {
  const start = useRef<{ x: number; y: number; t: number; dir: 'left' | 'right' | null } | null>(null)

  useEffect(() => {
    if (!enabled) return
    const el = document.getElementById('screen-root')
    if (!el) return

    const onStart = (e: TouchEvent) => {
      const t = e.touches[0]
      if (!t) return
      // Touch originated inside a horizontally-scrollable region — hand
      // the gesture to the browser. Yields a wider scope than vertical
      // intent (vertical intent only resolves on first move; this one
      // opts out immediately at touchstart because we KNOW scrolling
      // belongs to the descendant, not to us).
      if (findHorizontalScrollAncestor(e.target as Element | null)) {
        start.current = null
        return
      }
      start.current = { x: t.clientX, y: t.clientY, t: Date.now(), dir: null }
    }

    const onMove = (e: TouchEvent) => {
      const s = start.current
      const t = e.touches[0]
      if (!s || !t) return
      const dx = t.clientX - s.x
      const dy = t.clientY - s.y

      if (s.dir === null) {
        if (Math.abs(dx) < 8 && Math.abs(dy) < 8) return
        // Vertical intent: bail out and let the list scroll.
        if (Math.abs(dy) > Math.abs(dx)) {
          start.current = null
          return
        }
        s.dir = dx > 0 ? 'right' : 'left'
      }
      if (s.dir === 'right') onBack?.()
      else onDrawer?.()
      start.current = null
    }

    const onEnd = () => {
      start.current = null
    }

    el.addEventListener('touchstart', onStart, { passive: true })
    el.addEventListener('touchmove', onMove, { passive: true })
    el.addEventListener('touchend', onEnd, { passive: true })
    return () => {
      el.removeEventListener('touchstart', onStart)
      el.removeEventListener('touchmove', onMove)
      el.removeEventListener('touchend', onEnd)
    }
  }, [enabled, onBack, onDrawer])

  return <>{children}</>
}

export { COMMIT_RATIO, COMMIT_VELOCITY }
