/**
 * CollapsibleChatContent — the fold an over-long chat bubble wears.
 *
 * The mobile port of Desktop's `chat/CollapsibleChatContent.test.tsx`; the
 * failure list it guards is theirs, verbatim:
 *   1. a long answer that swallows the transcript. The fold exists to show
 *      the first eight lines and say that there is more; an unclamped bubble
 *      under every row turns a 390pt conversation into a wall of text, and
 *      the reader can no longer page through the shape of the conversation;
 *   2. a fold that cannot be opened — a chevron clipped by the very overflow
 *      it controls is a control the reader cannot reach;
 *   3. a live refresh that snaps the content shut under the reader's finger.
 *      A session keeps talking; the answer grows; the reader's choice to read
 *      the whole thing must survive that. The ONE exception is content that
 *      no longer needs a fold: there is nothing to expand, so the resting
 *      shape is the whole content again;
 *   4. a page jump on expand. The clip animates its ceiling instead of
 *      snapping between two heights.
 *
 * The environment here has no layout (jsdom), so the fold takes its
 * measurement fallback path — the length estimate. That is a deliberate
 * subject of these tests: the DOM-less path must still decide "more to read"
 * correctly, or the fold would never appear in a headless environment and
 * the browser path would be the only one ever exercised.
 *
 * Mobile has no @testing-library dependency; the fold needs a live tree
 * (state + effects), so these tests use the same createRoot harness the
 * block tests use.
 */
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { CollapsibleChatContent } from '../components/CollapsibleChatContent'
import {
  RUN_ANSWER_COLLAPSED_LINES,
  collapsedHeightPx,
  isOverflowing,
  resolveExpanded,
  resolveLineHeightPx,
} from '../lib/chatBubbleFold'

let host: HTMLDivElement
let root: Root

beforeEach(() => {
  ;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})

afterEach(() => {
  act(() => root.unmount())
  host.remove()
})

/** (Re)render the fold over `text` — a second call on a mounted root is the
 *  live refresh the fold must survive. */
function mountFold(text: string): HTMLElement {
  act(() =>
    root.render(
      <CollapsibleChatContent text={text}>
        <div>{text}</div>
      </CollapsibleChatContent>,
    ),
  )
  return host
}

function click(el: HTMLElement | null): void {
  act(() => {
    ;(el as HTMLButtonElement).click()
  })
}

/** Content long enough to be folded: many lines of text. */
function longText(lines = 40): string {
  return Array.from({ length: lines }, (_, i) => `line ${i + 1}`).join('\n')
}

/** The clip's own element — the box whose ceiling is the fold. */
function clip(container: HTMLElement): HTMLElement {
  return container.querySelector('[data-chat-fold-clip]') as HTMLElement
}

/** The toggle, if the fold painted one. */
function toggle(container: HTMLElement): HTMLElement | null {
  return container.querySelector('[data-chat-fold-toggle]')
}

describe('CollapsibleChatContent', () => {
  it('shows short content whole, with nothing to expand', () => {
    const container = mountFold('short answer')
    expect(container.textContent).toContain('short answer')
    // No chevron: there is nothing more to read, so a fold control would be
    // a control that does nothing.
    expect(toggle(container)).toBeNull()
  })

  it('folds long content and offers a way to read the rest', () => {
    const container = mountFold(longText())
    const btn = toggle(container)
    expect(btn).not.toBeNull()
    expect(btn?.getAttribute('aria-expanded')).toBe('false')
    // The fold is a real ceiling, not a marker.
    expect(clip(container).style.maxHeight).not.toBe('')
  })

  it('the chevron opens the whole content and closes it again', () => {
    const container = mountFold(longText())
    click(toggle(container))
    expect(toggle(container)?.getAttribute('aria-expanded')).toBe('true')
    // The expanded ceiling is taller than the folded one.
    const expanded = parseFloat(clip(container).style.maxHeight)
    click(toggle(container))
    const collapsed = parseFloat(clip(container).style.maxHeight)
    expect(collapsed).toBeLessThan(expanded)
    expect(toggle(container)?.getAttribute('aria-expanded')).toBe('false')
  })

  it('the toggle carries a reachable hit area and a label', () => {
    const container = mountFold(longText())
    const btn = toggle(container)!
    // A chevron alone is a small target; the control is a padded, full-width
    // button the thumb can aim at (the skin lives in app.css).
    expect(btn.className).toContain('chat-fold-toggle')
    expect(btn.getAttribute('aria-label')).toBe('展开全文')
    click(btn)
    expect(toggle(container)?.getAttribute('aria-label')).toBe('收起全文')
  })

  it('a live refresh does not snap an expanded answer shut', () => {
    const container = mountFold(longText())
    click(toggle(container))
    expect(toggle(container)?.getAttribute('aria-expanded')).toBe('true')
    // The session keeps talking: the answer grows.
    mountFold(longText(60))
    expect(toggle(container)?.getAttribute('aria-expanded')).toBe('true')
  })

  it('content that no longer needs a fold returns to the resting shape', () => {
    const container = mountFold(longText())
    click(toggle(container))
    expect(toggle(container)?.getAttribute('aria-expanded')).toBe('true')
    // The content shrank to something that fits: there is nothing to expand,
    // so a stale "expanded" flag would be a lie the reader can act on.
    mountFold('short')
    expect(toggle(container)).toBeNull()
  })

  it('the chevron sits outside the clip that hides the rest of the content', () => {
    const container = mountFold(longText())
    const clipEl = clip(container)
    const btn = toggle(container)!
    // A control inside the clipped box would be cut off with the text it
    // controls.
    expect(clipEl.contains(btn)).toBe(false)
  })
})

describe('chatBubbleFold', () => {
  it('counts eight lines as the ceiling', () => {
    expect(RUN_ANSWER_COLLAPSED_LINES).toBe(8)
    expect(collapsedHeightPx(20)).toBe(160)
  })

  it("reads the browser's pixel line-height, and the shapes a hand-written style uses", () => {
    expect(resolveLineHeightPx('19.9px', 14)).toBeCloseTo(19.9)
    expect(resolveLineHeightPx('1.5', 14)).toBeCloseTo(21)
    expect(resolveLineHeightPx('1.5em', 14)).toBeCloseTo(21)
    expect(resolveLineHeightPx('normal', 14)).toBeCloseTo(14 * 1.6)
    expect(resolveLineHeightPx(null, 14)).toBeCloseTo(14 * 1.6)
  })

  it("a rounding difference is not 'more text'", () => {
    expect(isOverflowing(160, 160)).toBe(false)
    expect(isOverflowing(161, 160)).toBe(false)
    expect(isOverflowing(162, 160)).toBe(true)
  })

  it('an expansion survives a refresh, but only while there is something to expand', () => {
    expect(resolveExpanded(true, true)).toBe(true)
    expect(resolveExpanded(false, true)).toBe(false)
    expect(resolveExpanded(true, false)).toBe(false)
  })
})
