/**
 * The new pure logic behind the inbox and the message renderer.
 *
 * The markdown tests are the point of this file. `renderMarkdown` consumes
 * text written by an agent or by a stranger in a DM, and it returns React
 * nodes — so the one thing it must never do is let input become markup.
 */

import { describe, expect, it } from 'vitest'
import { renderMarkdown } from '../lib/markdown'
import { userChatId } from '../lib/api'
import { formatListTime, daySeparator } from '../lib/time'
import { hasUnread, markSeen, clearUnread } from '../lib/unread'

/** Flatten the node tree to text, and collect every `href` it produced. */
function walk(nodes: unknown, texts: string[], hrefs: string[]): void {
  if (nodes === null || nodes === undefined || nodes === false) return
  if (typeof nodes === 'string' || typeof nodes === 'number') {
    texts.push(String(nodes))
    return
  }
  if (Array.isArray(nodes)) {
    for (const n of nodes) walk(n, texts, hrefs)
    return
  }
  const el = nodes as { props?: { href?: string; children?: unknown } }
  if (el.props?.href !== undefined) hrefs.push(el.props.href)
  walk(el.props?.children, texts, hrefs)
}

function render(src: string): { text: string; hrefs: string[] } {
  const texts: string[] = []
  const hrefs: string[] = []
  walk(renderMarkdown(src), texts, hrefs)
  return { text: texts.join(''), hrefs }
}

describe('renderMarkdown', () => {
  it('renders headings, emphasis and inline code as text, not markup', () => {
    const { text } = render('# Title\n\n**bold** and *ital* and `x=1`')
    expect(text).toContain('Title')
    expect(text).toContain('bold')
    expect(text).toContain('ital')
    expect(text).toContain('x=1')
    // The markers themselves must be gone.
    expect(text).not.toContain('**')
    expect(text).not.toContain('`')
    expect(text).not.toContain('#')
  })

  it('never emits raw HTML — an <img onerror> payload stays literal text', () => {
    const evil = '<img src=x onerror="alert(1)"><script>alert(2)</script>'
    const { text, hrefs } = render(evil)
    expect(hrefs).toEqual([])
    // Rendered as text nodes, the payload survives as characters. That is
    // the proof nothing was parsed into an element.
    expect(text).toContain('<img src=x onerror="alert(1)">')
    expect(text).not.toContain('undefined')
  })

  it('blocks a javascript: link but keeps an https: one', () => {
    const bad = render('[click](javascript:alert(1))')
    expect(bad.hrefs).toEqual([])
    expect(bad.text).toContain('click') // the label stays, the target dies

    const good = render('[docs](https://example.com/a?b=1)')
    expect(good.hrefs).toEqual(['https://example.com/a?b=1'])
  })

  it('keeps code fences literal, including markdown inside them', () => {
    const { text } = render('```ts\nconst a = "**not bold**"\n```')
    expect(text).toContain('const a = "**not bold**"')
  })

  it('parses lists and drops the bullet markers', () => {
    const { text } = render('- one\n- two\n\n1. first\n2. second')
    expect(text).toContain('one')
    expect(text).toContain('two')
    expect(text).toContain('first')
    expect(text).not.toContain('- one')
  })

  it('survives an unterminated fence (a truncated reply)', () => {
    const { text } = render('here is the code:\n```\nfn main() {}')
    expect(text).toContain('fn main() {}')
  })

  it('does not treat underscores in identifiers as emphasis', () => {
    const { text } = render('call user_name_id now')
    expect(text).toContain('user_name_id')
  })
})

describe('userChatId', () => {
  it('is canonical regardless of argument order', () => {
    expect(userChatId('u_b', 'u_a')).toBe('u_a__u_b')
    expect(userChatId('u_a', 'u_b')).toBe('u_a__u_b')
  })

  it('matches the server rule: sorted bytewise, joined by __', () => {
    // The Gateway rejects a non-canonical id with 404, so a wrong sort here
    // is a thread that silently never loads.
    expect(userChatId('9', '10')).toBe('10__9')
  })
})

describe('unread dot', () => {
  it('stays silent for a session this device has never opened', () => {
    clearUnread()
    expect(hasUnread('a', 's1', 5)).toBe(false)
  })

  it('lights up only when the count grew past what was seen', () => {
    clearUnread()
    markSeen('a', 's1', 5)
    expect(hasUnread('a', 's1', 5)).toBe(false)
    expect(hasUnread('a', 's1', 6)).toBe(true)
    expect(hasUnread('a', 's1', 4)).toBe(false)
  })

  it('treats a missing count as no news, not as unread', () => {
    clearUnread()
    markSeen('a', 's2', 3)
    expect(hasUnread('a', 's2', undefined)).toBe(false)
    expect(hasUnread('a', 's2', 0)).toBe(false)
  })
})

describe('time labels', () => {
  it('shows a clock today, 昨天 yesterday, and a date beyond a week', () => {
    const now = Date.now()
    expect(formatListTime(now)).toMatch(/^\d{2}:\d{2}$/)
    expect(formatListTime(now - 86_400_000)).toBe('昨天')
    expect(formatListTime(now - 30 * 86_400_000)).toMatch(/\d+\/\d+/)
    expect(formatListTime(0)).toBe('')
    expect(formatListTime(undefined)).toBe('')
  })

  it('emits a day separator only when the day actually changed', () => {
    const now = Date.now()
    expect(daySeparator(now, now)).toBeNull()
    // The label carries the clock as well since 5b1f2f41 (WeChat puts the time
    // on the separator), so assert the day word, not the whole string — the
    // exact clock depends on the runner's locale.
    expect(daySeparator(now, now - 86_400_000)).toMatch(/^昨天 /)
    expect(daySeparator(null, now)).toMatch(/^今天 /)
  })
})
