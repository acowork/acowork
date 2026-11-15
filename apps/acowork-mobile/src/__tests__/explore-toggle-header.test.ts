/**
 * The execution card's header is ONE line, always.
 *
 * jsdom applies no stylesheet, so the CSS source IS the spec here (same
 * reason as `button-vocabulary.test.ts`).
 *
 * Why: the header row is a flex row whose items were sized by their content.
 * A flex item without `min-width: 0` shrinks to its MIN-content width, so a
 * long run name wrapped onto several lines — the card then grew TALLER as the
 * window got NARROWER. The fix is the same one the desktop card got: the
 * items may shrink past their content (`min-width: 0`) and the text shortens
 * with an ellipsis (`text-overflow: ellipsis` + `white-space: nowrap`)
 * instead of wrapping.
 */
import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const __dirname = dirname(fileURLToPath(import.meta.url))
const appCss = readFileSync(resolve(__dirname, '..', 'styles', 'app.css'), 'utf8')

function rule(selector: string): string {
  const m = appCss.match(new RegExp(`\\${selector}\\s*\\{[^}]*\\}`))
  expect(m, `${selector} rule exists in app.css`).not.toBeNull()
  return m![0]
}

describe('the execution card header never wraps', () => {
  it('the header is a flex row', () => {
    expect(rule('.explore-toggle')).toContain('display: flex')
  })

  it('the label shrinks and shortens with an ellipsis', () => {
    const label = rule('.explore-toggle-label')
    expect(label).toContain('min-width: 0')
    expect(label).toContain('overflow: hidden')
    expect(label).toContain('text-overflow: ellipsis')
    expect(label).toContain('white-space: nowrap')
    // It may shrink, but it must not grow past the room the summary needs.
    expect(label).not.toMatch(/flex:\s*1/)
  })

  it('the summary truncates the same way', () => {
    const summary = rule('.explore-toggle-summary')
    expect(summary).toContain('min-width: 0')
    expect(summary).toContain('overflow: hidden')
    expect(summary).toContain('text-overflow: ellipsis')
    expect(summary).toContain('white-space: nowrap')
  })

  /** The desktop card's rule, copied: a run with positive evidence it is still
   *  going wears the round count + live state word as a FILLED capsule, so the
   *  shape itself says "working". An idle run keeps the bare gray text. */
  it('the working capsule is a filled pill with white text', () => {
    const capsule = rule('.explore-toggle-live')
    expect(capsule).toContain('background: var(--color-tint-strong)')
    expect(capsule).toContain('color: var(--color-on-accent)')
    expect(capsule).toContain('border-radius: 999px')
    // It shrinks and shortens like the rest of the header — a capsule that
    // cannot shrink would wrap the header onto two lines.
    expect(capsule).toContain('min-width: 0')
    expect(capsule).not.toMatch(/flex:\s*none/)
  })

  it("the capsule's own text truncates instead of wrapping", () => {
    const text = rule('.explore-toggle-live-text')
    expect(text).toContain('min-width: 0')
    expect(text).toContain('overflow: hidden')
    expect(text).toContain('text-overflow: ellipsis')
    expect(text).toContain('white-space: nowrap')
  })
})
