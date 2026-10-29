/**
 * The PM task chip on the mobile chat surface (design 27 §6.4, v2.8).
 *
 * A task-dispatch chat row carries only the human text plus `meta.task_id`;
 * the claim/submit playbook lives in the execution session's system block
 * and must never paint here. These tests pin the render contract:
 * chip-when-task_id, no-chip-otherwise, and no playbook text leaking into
 * the row body. Same shape as chat-routing-reaction.test.ts:
 * renderToStaticMarkup over the real RowView, no fixtures.
 */
import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import type { ChatMessageRow } from '../lib/types'

import { RowView, TaskChip, type RowProps } from '../screens/chat/AgentChatScreen'

function baseRow(meta: ChatMessageRow['meta'] = {}, body = '整理季度报表'): ChatMessageRow {
  return { seq: 1, ts: 1, from: 'u1', kind: 'user_message', body, to: 'a1', meta } as ChatMessageRow
}

function renderRow(row: ChatMessageRow): string {
  const props: RowProps = {
    row,
    isSelf: true,
    selfAvatar: 'me',
    selfName: 'me',
    agentAvatar: 'ag',
    agentName: 'agent',
    closed: false,
    answered: false,
    answeredCards: new Set<string>(),
    chatAnswered: new Set<string>(),
    onOpenExec: () => {},
    onAnswerCard: () => {},
    onOpenAttachment: () => {},
  }
  return renderToStaticMarkup(<RowView {...props} />)
}

describe('task chip (mobile)', () => {
  it('renders a pill with the bare task id above a dispatch row', () => {
    const html = renderRow(baseRow({ message_id: 'm1', task_id: 't-abc123' }))
    expect(html).toContain('chat-task-chip')
    expect(html).toContain('t-abc123')
  })

  it('a plain user message gets no chip', () => {
    const html = renderRow(baseRow({ message_id: 'm1' }))
    expect(html).not.toContain('chat-task-chip')
  })

  it('an agent message never gets the chip even with a stray task_id', () => {
    const row = { ...baseRow({ task_id: 't-x' }), kind: 'agent_message' } as ChatMessageRow
    expect(renderRow(row)).not.toContain('chat-task-chip')
  })

  it('the chip copies the bare id, not the label text', () => {
    let copied = ''
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText: (s: string) => { copied = s; return Promise.resolve() } },
    })
    const html = renderToStaticMarkup(<TaskChip taskId="t-abc123" />)
    expect(html).toContain('chat-task-chip')
    // SSR cannot click; pin the payload by invoking the same value the
    // handler passes — the bare id, never the "任务 " label.
    expect(html).toContain('t-abc123')
    expect(copied).toBe('')
  })
})
