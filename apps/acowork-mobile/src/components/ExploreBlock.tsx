/**
 * ExploreBlock — groups consecutive tool_call / tool_result / thought messages
 * into a single collapsible section, matching the Desktop's ExploreBlock UX.
 *
 * tool_call and its paired tool_result are merged into one row (keyed by
 * toolCallId), so the expanded list is as compact as the desktop.
 */
import { t as tr } from '../i18n'
import { useState } from 'react'
import type { ChatMessage } from '../lib/types'

type Payload = { type?: string; name?: string; toolCallId?: string }

type PairedItem =
  | { kind: 'thought'; msg: ChatMessage }
  | { kind: 'tool'; call: ChatMessage; result?: ChatMessage }

function buildPairedItems(items: ChatMessage[]): PairedItem[] {
  // Index tool_results by toolCallId
  const resultsById = new Map<string, ChatMessage>()
  for (const m of items) {
    const p = (m.payload ?? {}) as Payload
    if (p.type === 'tool_result' && p.toolCallId) {
      resultsById.set(p.toolCallId, m)
    }
  }

  const consumed = new Set<string>()
  const paired: PairedItem[] = []

  for (const m of items) {
    const p = (m.payload ?? {}) as Payload
    if (p.type === 'thought') {
      paired.push({ kind: 'thought', msg: m })
    } else if (p.type === 'tool_call') {
      let result: ChatMessage | undefined
      if (p.toolCallId) {
        const candidate = resultsById.get(p.toolCallId)
        if (candidate && !consumed.has(candidate.id)) {
          consumed.add(candidate.id)
          result = candidate
        }
      }
      paired.push({ kind: 'tool', call: m, result })
    } else if (p.type === 'tool_result') {
      // Skip if consumed by a tool_call pairing
      if (p.toolCallId && consumed.has(m.id)) continue
      // Orphan result (no matching call) — show standalone
      paired.push({ kind: 'tool', call: m })
    }
  }
  return paired
}

export function ExploreBlock({ items }: { items: ChatMessage[] }) {
  const [open, setOpen] = useState(false)

  const paired = buildPairedItems(items)
  const stepCount = paired.length

  // Tool names for summary (only tool_call items)
  const toolNames = paired
    .filter((p): p is Extract<PairedItem, { kind: 'tool' }> => p.kind === 'tool')
    .map((p) => {
      const pl = (p.call.payload ?? {}) as Payload
      return pl.name ?? 'tool'
    })

  // Deduplicate for summary (e.g. "bash ×5")
  const counts = new Map<string, number>()
  for (const n of toolNames) counts.set(n, (counts.get(n) ?? 0) + 1)
  const summary = Array.from(counts.entries())
    .map(([name, count]) => (count > 1 ? `${name} ×${count}` : name))
    .join(', ')

  return (
    <div className="explore-wrap">
      <button
        type="button"
        className="explore-toggle"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
      >
        <span className="explore-toggle-icon">&#9881;</span>
        <span className="explore-toggle-label">{tr('已探索 {{n}} 步', { n: stepCount })}</span>
        <span className="explore-toggle-summary">{summary}</span>
        <span className="explore-toggle-caret" aria-hidden>
          {open ? (
            <svg width="14" height="14" viewBox="0 0 14 14" fill="none" style={{ display: 'block' }}>
              <path d="M3 5l4 4 4-4" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          ) : (
            <svg width="14" height="14" viewBox="0 0 14 14" fill="none" style={{ display: 'block' }}>
              <path d="M5 3l4 4-4 4" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
          )}
        </span>
      </button>
      {open ? (
        <div className="explore-body">
          {paired.map((p) => {
            if (p.kind === 'thought') {
              return (
                <div key={p.msg.id} className="explore-item">
                  <span className="explore-item-name">{'\uD83D\uDCAD'} {tr('思考')}</span>
                  <span className="explore-item-args">{p.msg.content?.slice(0, 80)}</span>
                </div>
              )
            }
            // tool (call + optional result merged)
            const pl = (p.call.payload ?? {}) as Payload
            const name = pl.name ?? 'tool'
            const preview = p.result?.content?.slice(0, 60) ?? p.call.content?.slice(0, 60) ?? ''
            return (
              <div key={p.call.id} className="explore-item">
                <span className="explore-item-name">{'\u2699'} {name}</span>
                <span className="explore-item-args">{preview}</span>
              </div>
            )
          })}
        </div>
      ) : null}
    </div>
  )
}
