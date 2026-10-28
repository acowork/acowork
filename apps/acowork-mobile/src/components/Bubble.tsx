/**
 * One chat bubble, shared by the agent and the human thread (design §4.3).
 *
 * The avatar / sender-name row is owned by the screen (ChatDetailScreen) so
 * that plain bubbles and ExploreBlocks align under the same left edge —
 * mirroring the desktop's grouped layout. This component renders only the
 * bubble body.
 */

import { useState } from 'react'
import { renderMarkdown } from '../lib/markdown'
import type { ChatMessage } from '../lib/types'

/** Seconds or milliseconds → milliseconds. A ts under 1e11 is seconds. */
export function toMs(ts: number): number {
  return ts > 0 && ts < 1e11 ? ts * 1000 : ts
}

/** `payload` as written by `mapConversationEntries`. */
type ToolPayload = { type?: string; name?: string }

/** `literal` renders the body as plain text instead of markdown. Human input
 *  is literal text, markdown is agent speech (the house grammar); a caller
 *  whose rows are ALL human — the user↔user DM — passes `literal` for both
 *  sides, and the execution detail passes it for the user's own turns. Without
 *  it `**bold**` renders bold here but literally on the desktop user rows, and
 *  a `> quote` picks up `.md-quote`'s secondary gray inside the white-text
 *  accent bubble (`renderMarkdown` declares no color of its own, so the quote
 *  rule is the whole of the drift). */
export function Bubble({ msg, literal = false }: { msg: ChatMessage; literal?: boolean }) {
  const tool = (msg.payload ?? {}) as ToolPayload
  if (tool.type === 'tool_call') return <ToolRow msg={msg} name={tool.name ?? 'tool'} />
  const self = msg.role === 'user'
  return (
    <div className={`bubble${self ? ' bubble-self' : ' bubble-peer'}`}>
      <div className={`bubble-text${literal ? ' bubble-raw' : ''}`}>
        {literal ? msg.content : renderMarkdown(msg.content)}
      </div>
    </div>
  )
}

/** Collapsed tool call: one line, arguments behind a tap. */
function ToolRow({ msg, name }: { msg: ChatMessage; name: string }) {
  const [open, setOpen] = useState(false)
  return (
    <button type="button" className="tool-row" onClick={() => setOpen((v) => !v)} aria-expanded={open}>
      <span className="tool-row-icon" aria-hidden>⚙</span>
      <span className="tool-row-name">调用 {name}</span>
      <span className="tool-row-caret">{open ? '收起' : '详情'}</span>
      {open && msg.content ? <span className="tool-row-body">{msg.content.slice(0, 2000)}</span> : null}
    </button>
  )
}
