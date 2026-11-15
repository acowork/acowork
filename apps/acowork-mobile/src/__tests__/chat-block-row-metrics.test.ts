/**
 * The execution card's rows and the composer keep ONE line each.
 *
 * jsdom applies no stylesheet, so the CSS source IS the spec here (same
 * reason as `button-vocabulary.test.ts` and `explore-toggle-header.test.ts`).
 *
 * Why: two global rules were making the layout read taller and misaligned.
 *
 * 1. `button { min-height: var(--touch-min) }` (44px, `tokens.css`) applies to
 *    EVERY button, including a list line like `.chat-run-row`. That row holds
 *    12px text, so 44px left ~10px of blank above and below it and the row
 *    read TALLER than the card's own title banner. The row must size to its
 *    content.
 *
 * 2. A flex row with `align-items: flex-end` puts the TOPS of unequal children
 *    out of line. The reply box was 34px against a 32px button, and the
 *    composer's textarea 40px against a 44px send button — in both cases the
 *    two halves of one control read as two rows.
 */
import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const __dirname = dirname(fileURLToPath(import.meta.url))
const appCss = readFileSync(resolve(__dirname, '..', 'styles', 'app.css'), 'utf8')
const tokensCss = readFileSync(resolve(__dirname, '..', 'styles', 'tokens.css'), 'utf8')

function rule(css: string, selector: string): string {
  const m = css.match(new RegExp(`\\${selector}\\s*\\{[^}]*\\}`))
  expect(m, `${selector} rule exists`).not.toBeNull()
  return m![0]
}

describe('the execution card rows keep one line', () => {
  it('the global touch minimum really is 44px (the reason the row was tall)', () => {
    expect(tokensCss).toContain('--touch-min: 44px')
  })

  it('the round row sizes to its content, not to the touch minimum', () => {
    expect(rule(appCss, '.chat-run-row')).toContain('min-height: auto')
  })

  it('the reply row centers its box and its button', () => {
    expect(rule(appCss, '.chat-block-reply')).toContain('align-items: center')
  })

  it('the reply box and its button are the same height', () => {
    const input = rule(appCss, '.chat-block-reply-input')
    const send = rule(appCss, '.chat-block-reply-send')
    const inputH = input.match(/min-height:\s*(\d+)px/)
    const sendH = send.match(/min-height:\s*(\d+)px/)
    expect(inputH, 'the reply box declares a height').not.toBeNull()
    expect(sendH, 'the reply button declares a height').not.toBeNull()
    expect(inputH![1]).toBe(sendH![1])
  })
})

describe('the composer is one row', () => {
  it('the composer centers its textarea and its send button', () => {
    expect(rule(appCss, '.composer')).toContain('align-items: center')
  })

  it('the textarea is as tall as the send button', () => {
    const inputH = rule(appCss, '.composer-input').match(/min-height:\s*(\d+)px/)
    const sendH = rule(appCss, '.composer-send').match(/height:\s*(\d+)px/)
    expect(inputH, 'the composer textarea declares a height').not.toBeNull()
    expect(sendH, 'the send button declares a height').not.toBeNull()
    expect(inputH![1]).toBe(sendH![1])
  })
})
