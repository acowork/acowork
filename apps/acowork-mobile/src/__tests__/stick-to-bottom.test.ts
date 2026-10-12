/**
 * Follow-rule tests for `FollowController` (the scroll-hijack fix): the
 * list follows new content only while the user is pinned at the bottom;
 * a scroll-up survives refreshes; session switches and sends re-claim
 * the follow. Driven through a fake container — no renderer needed
 * (render-based tests die on this workspace's double-React vitest issue).
 */
import { describe, expect, it } from 'vitest'
import { FollowController, isNearBottom, STICK_PX, type ScrollEl } from '../lib/stickToBottom'

/**
 * Stand-in for a scroll container: only the surface the controller touches.
 * `scrollTop` clamps like a real browser, so the max is
 * scrollHeight - clientHeight = 1000 - 300 = 700.
 */
class FakeEl implements ScrollEl {
  scrollHeight = 1000
  clientHeight = 300
  private top = 0
  private fns: (() => void)[] = []
  get scrollTop(): number {
    return this.top
  }
  set scrollTop(v: number) {
    this.top = Math.max(0, Math.min(v, this.scrollHeight - this.clientHeight))
  }
  get bottom(): number {
    return this.scrollHeight - this.clientHeight
  }
  addEventListener(_type: 'scroll', fn: () => void): void {
    this.fns.push(fn)
  }
  removeEventListener(_type: 'scroll', fn: () => void): void {
    this.fns = this.fns.filter((f) => f !== fn)
  }
  /** A user scroll: move (clamped) and fire the listeners, like a browser. */
  userScrollTo(top: number): void {
    this.scrollTop = top
    this.fns.forEach((f) => f())
  }
  /** Content growth (new messages): the follow decision runs against it. */
  grow(by: number): void {
    this.scrollHeight += by
  }
}

function bound(): { c: FollowController; el: FakeEl } {
  const c = new FollowController()
  const el = new FakeEl()
  c.bind(el)
  return { c, el }
}

describe('FollowController', () => {
  it('a first refresh re-pins by the view key and follows to the bottom', () => {
    const { c, el } = bound()
    c.refresh('s1')
    expect(el.scrollTop).toBe(el.bottom)
    // new content lands while pinned — follow
    el.grow(200)
    c.refresh('s1')
    expect(el.scrollTop).toBe(el.bottom)
  })

  it('a user scroll-up survives refreshes', () => {
    const { c, el } = bound()
    c.refresh('s1')
    el.userScrollTo(0) // far from the bottom: gap 700 > STICK_PX
    el.grow(200)
    c.refresh('s1')
    el.grow(200)
    c.refresh('s1')
    expect(el.scrollTop).toBe(0)
  })

  it('returning near the bottom re-pins the follow', () => {
    const { c, el } = bound()
    c.refresh('s1')
    el.userScrollTo(0)
    c.refresh('s1')
    expect(el.scrollTop).toBe(0)
    el.userScrollTo(el.bottom - STICK_PX) // exactly at the threshold: pinned again
    el.grow(200)
    c.refresh('s1')
    expect(el.scrollTop).toBe(el.bottom)
  })

  it('switching the view key re-pins even after a scroll-up', () => {
    const { c, el } = bound()
    c.refresh('s1')
    el.userScrollTo(0)
    c.refresh('s2')
    expect(el.scrollTop).toBe(el.bottom)
  })

  it('stickNow re-pins and keeps following (the send path)', () => {
    const { c, el } = bound()
    c.refresh('s1')
    el.userScrollTo(0)
    c.stickNow()
    expect(el.scrollTop).toBe(el.bottom)
    el.grow(200)
    c.refresh('s1')
    expect(el.scrollTop).toBe(el.bottom)
  })

  it('bind(null) detaches: refresh becomes a no-op and old scrolls stop mattering', () => {
    const { c, el } = bound()
    c.refresh('s1')
    c.bind(null)
    el.userScrollTo(0) // would unpin if the listener were still attached
    expect(() => c.refresh('s1')).not.toThrow()
    expect(el.scrollTop).toBe(0) // nothing moved
  })

  it('rebinding adopts the new container position (no scroll-up is lost)', () => {
    const { c } = bound()
    c.refresh('s1')
    const el2 = new FakeEl()
    el2.scrollTop = 0 // a remounted DOM node starts at the top of overflowing content
    c.bind(el2)
    expect(c.pinned).toBe(false)
    el2.grow(200)
    c.refresh('s1') // same key: no force — the adoption stands
    expect(el2.scrollTop).toBe(0)
  })
})

describe('isNearBottom', () => {
  it('uses the STICK_PX threshold inclusive', () => {
    expect(isNearBottom({ scrollHeight: 1000, scrollTop: 700, clientHeight: 300 })).toBe(true)
    expect(isNearBottom({ scrollHeight: 1000, scrollTop: 700 - STICK_PX, clientHeight: 300 })).toBe(true)
    expect(isNearBottom({ scrollHeight: 1000, scrollTop: 700 - STICK_PX - 1, clientHeight: 300 })).toBe(false)
  })

  it('treats a non-overflowing container as at-bottom', () => {
    expect(isNearBottom({ scrollHeight: 200, scrollTop: 0, clientHeight: 300 })).toBe(true)
  })
})
