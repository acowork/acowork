/**
 * GFM table parsing in renderMarkdown.
 *
 * The renderMarkdown helper is the single markdown entry point for chat
 * bubbles, doc bodies, and task descriptions. Tables were the most-requested
 * missing feature — the previous behaviour was to render the raw source
 * verbatim (markdown.tsx header documents this). These tests pin the new
 * behaviour so it can't silently regress to "raw source" again.
 *
 * The tree walks are the same shape as inbox-render.test.ts: collect
 * className + text + href across the React tree and assert on the
 * flattened result. No jsdom / no fixtures.
 */

import { describe, expect, it } from 'vitest'
import { renderMarkdown } from '../lib/markdown'

type TreeNode = string | { props?: { className?: unknown; children?: unknown }; type?: unknown }

function walk(nodes: unknown, out: { text: string; classes: string[]; tags: string[] }): void {
  if (nodes === null || nodes === undefined || nodes === false) return
  if (typeof nodes === 'string' || typeof nodes === 'number') {
    out.text += String(nodes)
    return
  }
  if (Array.isArray(nodes)) {
    for (const n of nodes) walk(n, out)
    return
  }
  const el = nodes as TreeNode
  if (typeof el !== 'object' || el === null) return
  const cls = el.props?.className
  if (typeof cls === 'string') out.classes.push(cls)
  if (typeof el.type === 'string') out.tags.push(el.type)
  walk(el.props?.children, out)
}

function render(src: string) {
  const out: { text: string; classes: string[]; tags: string[] } = {
    text: '',
    classes: [],
    tags: [],
  }
  walk(renderMarkdown(src), out)
  return out
}

describe('renderMarkdown tables', () => {
  it('renders a minimal table as a real <table> with header and body rows', () => {
    const src = '| Name | Age |\n| --- | --- |\n| Bob | 25 |\n| Eve | 30 |'
    const out = render(src)
    // Tree contains the expected elements
    expect(out.classes.some((c) => c.includes('md-table-wrap'))).toBe(true)
    expect(out.classes.some((c) => c.includes('md-table'))).toBe(true)
    expect(out.classes.filter((c) => c === 'md-th').length).toBe(2) // Name, Age
    expect(out.classes.filter((c) => c === 'md-td').length).toBe(4) // 2 cols × 2 rows
    expect(out.tags).toContain('table')
    expect(out.tags).toContain('thead')
    expect(out.tags).toContain('tbody')
    expect(out.tags).toContain('th')
    expect(out.tags).toContain('td')
    // Cell text survived
    expect(out.text).toContain('Name')
    expect(out.text).toContain('Age')
    expect(out.text).toContain('Bob')
    expect(out.text).toContain('Eve')
    expect(out.text).toContain('25')
    expect(out.text).toContain('30')
  })

  it('tolerates leading/trailing pipe being absent (GFM allows both styles)', () => {
    const out = render('Name | Age\n--- | ---\nBob | 25')
    expect(out.classes.some((c) => c === 'md-table')).toBe(true)
    expect(out.text).toContain('Name')
    expect(out.text).toContain('Bob')
  })

  it('does not misfire on a stray pipe inside a paragraph', () => {
    // No separator line follows, so this is text — raw pipes and bars stay.
    const out = render('use foo | bar\nnot a table')
    expect(out.classes.some((c) => c === 'md-table')).toBe(false)
    expect(out.text).toContain('foo | bar')
  })

  it('stops the table at the first blank line and resumes parsing', () => {
    const src = '| a | b |\n| - | - |\n| 1 | 2 |\n\nparagraph after'
    const out = render(src)
    expect(out.classes.some((c) => c === 'md-table')).toBe(true)
    // The trailing paragraph becomes a <p>, not part of the table
    expect(out.classes.some((c) => c === 'md-p')).toBe(true)
  })

  it('keeps cell content inline-formatted (bold/code/link survive inside cells)', () => {
    const src = '| key | value |\n| --- | --- |\n| `cfg` | see [docs](https://example.com) |'
    const out = render(src)
    expect(out.classes.some((c) => c === 'md-icode')).toBe(true)
    expect(out.classes.some((c) => c === 'md-link')).toBe(true)
    expect(out.text).toContain('cfg')
  })
})
