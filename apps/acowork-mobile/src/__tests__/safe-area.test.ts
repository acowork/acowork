/**
 * Resolution rules for the status-bar / navigation-bar clearance
 * (`lib/safeArea.ts`). Driven as a pure function plus one jsdom check that
 * the result really lands on `:root` — the bug being fixed is a value that
 * never reaches the stylesheet.
 */
import { describe, expect, it } from 'vitest'
import { publishInsets, resolveInset } from '../lib/safeArea'

describe('resolveInset', () => {
  it('takes the native clearance when env() reports nothing (notch-less Android)', () => {
    // HUAWEI Mate 10: `env()` is 0 in every direction, the status bar is 72
    // physical px at dpr 3.
    expect(resolveInset(0, 72, 3)).toBe(24)
  })

  it('keeps env() when the platform answers nothing (iOS)', () => {
    // Command returns zeros off Android; WKWebView already got it right.
    expect(resolveInset(47, 0, 3)).toBe(47)
  })

  it('is zero in a plain browser, where there is no system bar at all', () => {
    expect(resolveInset(0, 0, 1)).toBe(0)
  })

  it('prefers the larger input when both report a value', () => {
    // A cutout taller than the dispatched inset must not shrink the gap.
    expect(resolveInset(30, 72, 3)).toBe(30)
    expect(resolveInset(20, 120, 3)).toBe(40)
  })

  it('never trusts a broken scale factor', () => {
    // dpr 0 would divide to Infinity; dpr < 1 would inflate the bar.
    expect(resolveInset(0, 72, 0)).toBe(72)
    expect(resolveInset(0, 72, 0.5)).toBe(72)
  })

  it('reads junk as no clearance', () => {
    expect(resolveInset(Number.NaN, 72, 3)).toBe(24)
    expect(resolveInset(0, Number.NaN, 3)).toBe(0)
    expect(resolveInset(-10, -20, 3)).toBe(0)
  })
})

describe('publishInsets', () => {
  it('writes an inline custom property that outranks tokens.css', () => {
    publishInsets({ top: 24, bottom: 40, left: 0, right: 0 })
    const s = document.documentElement.style
    expect(s.getPropertyValue('--safe-top')).toBe('24px')
    expect(s.getPropertyValue('--safe-bottom')).toBe('40px')
    s.removeProperty('--safe-top')
    s.removeProperty('--safe-bottom')
  })
})
