/**
 * Consecutive-message grouping (the IM convention the transcript adopted):
 * a speaker run wears its avatar+name once, at its head. The layout pass
 * (`AgentChatScreen`'s item map) decides whether an item continues the
 * previous speaker; the views must honour that decision — header off, and
 * the tighter `msg-group-cont` rhythm — because a repeated header per message
 * was the loudest structural noise on the 390pt screen.
 *
 * Pinned here is the view-side contract; the speaker-run computation itself
 * lives in the map (same file) and is exercised through `showHeader`.
 */
import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { RowView, type RowProps } from '../screens/chat/AgentChatScreen'
import type { ChatMessageRow } from '../lib/types'

let seq = 0
function row(kind: string, body = 'x'): ChatMessageRow {
  seq += 1
  return { seq, ts: seq, from: kind === 'user_message' ? 'u1' : 'ag', kind, body, to: 'a1', meta: {} } as ChatMessageRow
}

function render(r: ChatMessageRow, showHeader?: boolean): string {
  const props: RowProps = {
    row: r,
    showHeader,
    chatSessionId: 'cs-1',
    isSelf: r.kind === 'user_message',
    selfAvatar: 'me',
    selfName: 'me',
    agentAvatar: 'ag',
    agentName: 'agent',
    onJumpToExec: () => {},
    onReplyToRound: () => {},
    onOpenAttachment: () => {},
  }
  return renderToStaticMarkup(<RowView {...props} />)
}

describe('consecutive-message grouping — view contract', () => {
  it('a head row keeps the avatar+name header and the turn rhythm', () => {
    const html = render(row('agent_message'))
    expect(html).toContain('msg-header')
    expect(html).toContain('msg-sender-name')
    expect(html).not.toContain('msg-group-cont')
  })

  it('an explicit showHeader=true is the same as the default', () => {
    expect(render(row('agent_message'), true)).toContain('msg-header')
  })

  it('a continuation row drops the header and rides the tighter rhythm', () => {
    const html = render(row('agent_message'), false)
    expect(html).not.toContain('msg-header')
    expect(html).not.toContain('msg-sender-name')
    expect(html).toContain('msg-group-cont')
    // the bubble itself still renders — only the header is suppressed.
    expect(html).toContain('bubble bubble-peer')
  })

  it('a self continuation keeps the self-side anchoring classes', () => {
    const html = render(row('user_message'), false)
    expect(html).toContain('msg-group-self')
    expect(html).toContain('msg-group-cont')
    expect(html).not.toContain('msg-header')
  })

  it('a notice row is never a speaker run — it renders its own line', () => {
    const html = render(row('system_notice', '提醒'), false)
    expect(html).toContain('chat-notice')
    expect(html).not.toContain('msg-group')
  })
})
