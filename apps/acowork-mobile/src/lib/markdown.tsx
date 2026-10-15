/**
 * The Markdown subset the chat bubbles render (design §4.3).
 *
 * Why hand-rolled instead of `marked`/`react-markdown`: every renderer in
 * that family's default configuration emits HTML, and this text is
 * UNTRUSTED — it comes out of an agent's mouth, or a stranger's DM. A
 * `dangerouslySetInnerHTML` path here is an XSS hole with the Gateway's own
 * credentials in reach. This module returns React nodes only, so there is no
 * HTML to escape and no sanitizer to keep correct. It is also ~150 lines and
 * has no dependency.
 *
 * Supported: headings, fenced code, inline code, bold, italic, links
 * (http/https/mailto only), unordered/ordered lists, blockquotes,
 * GFM tables, line breaks. Anything else renders literally, which is the
 * safe default.
 */

import type { ReactNode } from 'react'

/** Schemes a chat link may use. Everything else renders as plain text. */
const SAFE_URL = /^(https?:|mailto:)/i

/**
 * Inline spans. One alternation, scanned left to right: a match consumes its
 * region, so `` `**not bold**` `` inside code stays literal.
 *
 * `_italic_` is deliberately NOT supported — it would eat the underscores in
 * `user_name` and every other identifier a chat log contains. `*italic*` is
 * enough, and Markdown's own emphasis rule agrees on ambiguity.
 */
const INLINE = /`([^`\n]+)`|\*\*([^*\n]+)\*\*|\*([^*\n]+)\*|\[([^\]\n]*)\]\(([^()\s]*)\)/g

/**
 * GFM table separator — `:---`, `---:`, `:---:`, or plain `---`, joined by
 * `|` with optional surrounding pipes. Requires at least one `-` per cell
 * (so a row of dashes alone doesn't trigger) and at least one cell that
 * contains `-` (so a blank line following a pipe-y sentence doesn't
 * trigger either).
 */
const TABLE_SEP = /^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)+\|?\s*$/

function inline(text: string, keyBase: string): ReactNode[] {
  const out: ReactNode[] = []
  let last = 0
  let n = 0
  // A fresh regex per call: `INLINE` is /g, and sharing one across calls
  // leaks `lastIndex` between them — the classic "works alone, drops the
  // first match in a loop" bug.
  const re = new RegExp(INLINE.source, 'g')
  let m: RegExpExecArray | null
  while ((m = re.exec(text))) {
    if (!m[0].length) re.lastIndex++ // zero-width guard
    const at = m.index
    if (at > last) out.push(text.slice(last, at))
    const [all, code, bold, italic, linkText, linkHref] = m
    const key = `${keyBase}-${n++}`
    if (code !== undefined) out.push(<code key={key} className="md-icode">{code}</code>)
    else if (bold !== undefined) out.push(<strong key={key}>{inline(bold, key)}</strong>)
    else if (italic !== undefined) out.push(<em key={key}>{inline(italic, key)}</em>)
    else if (linkHref !== undefined && SAFE_URL.test(linkHref))
      out.push(
        <a key={key} href={linkHref} className="md-link" target="_blank" rel="noreferrer">
          {inline(linkText ?? '', key)}
        </a>,
      )
    else out.push(all)
    last = at + all.length
  }
  if (last < text.length) out.push(text.slice(last))
  return out
}

/** GFM table separator recogniser — kept strict so unrelated `-`-heavy prose
 *  (a horizontal rule under a header, etc.) is not misclassified. */
function isTableSeparator(line: string | undefined): line is string {
  return typeof line === 'string' && /\|/.test(line) && TABLE_SEP.test(line)
}

/** Split a pipe-delimited row into trimmed cell strings, tolerating missing
 *  leading / trailing pipes (GFM allows both `| a | b |` and `a | b`). */
function parseCells(line: string): string[] {
  let s = line.trim()
  if (s.startsWith('|')) s = s.slice(1)
  if (s.endsWith('|') && s.length > 1) s = s.slice(0, -1)
  return s.split('|').map((c) => c.trim())
}

/** Render one parsed GFM table. The outer `.md-table-wrap` is a horizontal
 *  scroller — agent tables are often wider than the 390pt iPhone viewport
 *  and a fixed-width table would either clip or shrink the font size. */
function renderTable(header: string[], rows: string[][], k: number): ReactNode {
  return (
    <div key={`t${k}`} className="md-table-wrap">
      <table className="md-table">
        <thead>
          <tr>
            {header.map((c, i) => (
              <th key={i} className="md-th">
                {inline(c, `t${k}h${i}`)}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, ri) => (
            <tr key={ri} className="md-tr">
              {row.map((c, ci) => (
                <td key={ci} className="md-td">
                  {inline(c, `t${k}r${ri}c${ci}`)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

export function renderMarkdown(src: string): ReactNode[] {
  const lines = src.replace(/\r\n?/g, '\n').split('\n')
  const blocks: ReactNode[] = []
  let para: string[] = []
  let list: { ordered: boolean; items: string[] } | null = null
  let fence: string[] | null = null
  let key = 0

  const flushPara = () => {
    if (!para.length) return
    const parts = para.join('\n').split('\n')
    blocks.push(
      <p key={`p${key++}`} className="md-p">
        {parts.map((l, i) => (
          <span key={i}>
            {inline(l, `p${key}i${i}`)}
            {i < parts.length - 1 ? <br /> : null}
          </span>
        ))}
      </p>,
    )
    para = []
  }

  const flushList = () => {
    if (!list) return
    const items = list.items.map((it, i) => <li key={i}>{inline(it, `l${key}i${i}`)}</li>)
    blocks.push(
      list.ordered ? (
        <ol key={`l${key++}`} className="md-list">{items}</ol>
      ) : (
        <ul key={`l${key++}`} className="md-list">{items}</ul>
      ),
    )
    list = null
  }

  // Index-based loop so we can peek the next line for GFM table detection.
  // A `for (const raw of lines)` would force us to either build a mutable
  // iterator or scan the array twice per line — both uglier than this.
  let i = 0
  while (i < lines.length) {
    const raw = lines[i]!

    // Fence toggling first: inside a fence, nothing else is parsed.
    const fenceMark = /^\s*```/.test(raw)
    if (fenceMark) {
      if (fence) {
        blocks.push(
          <pre key={`c${key++}`} className="md-code">
            <code>{fence.join('\n')}</code>
          </pre>,
        )
        fence = null
      } else {
        flushPara()
        flushList()
        fence = []
      }
      i++
      continue
    }
    if (fence) {
      fence.push(raw)
      i++
      continue
    }

    if (!raw.trim()) {
      flushPara()
      flushList()
      i++
      continue
    }
    let m: RegExpExecArray | null
    if ((m = /^(#{1,6})\s+(.*)$/.exec(raw))) {
      flushPara()
      flushList()
      const level = Math.min(6, m[1]!.length + 2) // # -> h3: bubbles have no h1
      const Tag = (`h${level}`) as 'h3'
      blocks.push(<Tag key={`h${key++}`} className="md-h">{inline(m[2]!, `h${key}`)}</Tag>)
      i++
      continue
    }
    if ((m = /^\s*>\s?(.*)$/.exec(raw))) {
      flushPara()
      flushList()
      blocks.push(<blockquote key={`q${key++}`} className="md-quote">{inline(m[1]!, `q${key}`)}</blockquote>)
      i++
      continue
    }
    if ((m = /^\s*[-*+]\s+(.*)$/.exec(raw))) {
      flushPara()
      if (!list || list.ordered) {
        flushList()
        list = { ordered: false, items: [] }
      }
      list.items.push(m[1]!)
      i++
      continue
    }
    if ((m = /^\s*\d+[.)]\s+(.*)$/.exec(raw))) {
      flushPara()
      if (!list || !list.ordered) {
        flushList()
        list = { ordered: true, items: [] }
      }
      list.items.push(m[1]!)
      i++
      continue
    }

    // GFM table: header row containing a pipe, followed by a separator
    // line. We then consume body rows until a blank line (or end of input
    // or any line lacking a pipe). A stray single-line pipe (e.g. "use
    // foo | bar") does NOT trigger because it requires a separator
    // neighbour — that's what makes the false-positive rate low.
    if (raw.includes('|') && isTableSeparator(lines[i + 1])) {
      flushPara()
      flushList()
      const header = parseCells(raw)
      i += 2 // skip header + separator
      const rows: string[][] = []
      while (i < lines.length && lines[i]!.trim() !== '' && lines[i]!.includes('|')) {
        rows.push(parseCells(lines[i]!))
        i++
      }
      blocks.push(renderTable(header, rows, key++))
      continue
    }

    flushList()
    para.push(raw)
    i++
  }
  if (fence) {
    // Unterminated fence (a truncated reply): render what we have as code.
    blocks.push(
      <pre key={`c${key++}`} className="md-code">
        <code>{fence.join('\n')}</code>
      </pre>,
    )
  }
  flushPara()
  flushList()
  return blocks
}
