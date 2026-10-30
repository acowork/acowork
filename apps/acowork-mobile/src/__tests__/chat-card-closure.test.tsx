/**
 * A chat card's closure — the mobile port of the desktop fix "tell 'a human
 * answered' from 'nobody answered', on either surface" (6778be37), plus the
 * human-input grammar (26655b36) and the pause-answer quote (37056c19).
 *
 * Why this test exists:
 *   A card answered on the EXECUTION surface used to reach the transcript as
 *   an `interaction_resolved` notice that the mobile surface dropped
 *   unconditionally, while `humanAnsweredCardIds` only counted a `user_message`
 *   row — so the card painted "已结束" (this request ended) minutes after the
 *   human had actually decided in the other window, and the transcript said
 *   nothing. The fact was already in the row (`meta.resolution`); these cases
 *   pin that the two projections split along it and that the surface renders
 *   the right one:
 *
 *     1. `humanAnsweredCardIds` = "a human decided" (a local answer row OR a
 *        decision resolution on the notice); a non-decision resolution closes
 *        without claiming an answer;
 *     2. `chatAnsweredCardIds` = "decided on THIS surface" (the notice dedup);
 *     3. an ask_question/approval answer renders as a centered event line,
 *        while a PAUSE answer keeps its message form (with the question quote);
 *     4. the exec-surface decision paints one event line — the transcript's
 *        only record of it;
 *     5. human input is literal text, markdown is agent speech.
 *
 * Driven through `renderToStaticMarkup` over the real `RowView` — same shape
 * as `task-chip.test.tsx`.
 */
import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import type { ChatMessageRow } from '../lib/types'
import { Bubble } from '../components/Bubble'
import {
  RowView,
  chatAnsweredCardIds,
  humanAnsweredCardIds,
  truncateQuote,
  type RowProps,
} from '../screens/chat/AgentChatScreen'

let seq = 0
function row(kind: string, meta: ChatMessageRow['meta'] = {}, body = ''): ChatMessageRow {
  seq += 1
  return { seq, ts: seq, from: 'u1', kind, body, to: 'a1', meta } as ChatMessageRow
}

function renderRow(r: ChatMessageRow, meta: Partial<RowProps> = {}): string {
  const props: RowProps = {
    row: r,
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
    ...meta,
  }
  return renderToStaticMarkup(<RowView {...props} />)
}

describe('card closure projections', () => {
  it('chatAnsweredCardIds counts only a row answered on this surface', () => {
    const rows = [
      row('user_message', { message_id: 'ans-1', in_reply_to: 'card-1' }, '允许'),
      row('system_notice', {
        message_id: 'n-1',
        in_reply_to: 'card-2',
        notice_type: 'interaction_resolved',
        resolution: 'approved',
      }),
    ]
    expect([...chatAnsweredCardIds(rows)]).toEqual(['card-1'])
  })

  it('humanAnsweredCardIds accepts a decision resolution from the execution surface', () => {
    for (const resolution of ['answered', 'approved', 'rejected']) {
      const rows = [
        row('system_notice', {
          message_id: 'n-1',
          in_reply_to: 'card-1',
          notice_type: 'interaction_resolved',
          resolution,
        }),
      ]
      expect(humanAnsweredCardIds(rows).has('card-1')).toBe(true)
    }
  })

  it('a non-decision resolution closes the card without claiming an answer', () => {
    for (const resolution of ['timeout', 'cancelled', 'stopped', 'turn_ended', 'unanswerable', 'superseded']) {
      const rows = [
        row('system_notice', {
          message_id: 'n-1',
          in_reply_to: 'card-1',
          notice_type: 'interaction_resolved',
          resolution,
        }),
      ]
      expect(humanAnsweredCardIds(rows).has('card-1')).toBe(false)
    }
  })
})

describe('the answer moves tiers (6778be37)', () => {
  it('an ask_question/approval answer renders as a centered event line, not a bubble', () => {
    const html = renderRow(
      row('user_message', { message_id: 'ans-1', in_reply_to: 'card-1' }, '允许'),
      { answeredCardKind: 'approval' },
    )
    expect(html).toContain('chat-turn-status')
    expect(html).toContain('已回复：允许')
    expect(html).not.toContain('bubble-self')
  })

  it('a PAUSE answer keeps its message form and names its question above the bubble', () => {
    const html = renderRow(
      row('user_message', { message_id: 'ans-1', in_reply_to: 'card-1' }, '继续'),
      { answeredCardKind: 'pause', replyQuote: '允许执行 bash？' },
    )
    expect(html).toContain('bubble-self')
    expect(html).toContain('chat-reply-quote')
    expect(html).toContain('允许执行 bash？')
    expect(html).not.toContain('chat-turn-status')
  })

  it('an answer to a plain message gets no quote (card-only index)', () => {
    const html = renderRow(row('user_message', { message_id: 'ans-1', in_reply_to: 'msg-1' }, '好的'), {
      replyQuote: undefined,
    })
    expect(html).not.toContain('chat-reply-quote')
  })
})

describe('the execution-surface decision notice', () => {
  const notice = (resolution: string) =>
    row('system_notice', {
      message_id: 'n-1',
      in_reply_to: 'card-1',
      notice_type: 'interaction_resolved',
      resolution,
    })

  it('paints one event line when a human decided in the other window', () => {
    const html = renderRow(notice('answered'), { answeredCards: new Set(['card-1']) })
    expect(html).toContain('已在执行会话中回复')
  })

  it('stays silent when THIS surface already recorded the answer', () => {
    const html = renderRow(notice('answered'), {
      answeredCards: new Set(['card-1']),
      chatAnswered: new Set(['card-1']),
    })
    expect(html).not.toContain('已在执行会话中回复')
  })

  it('says nothing for a nobody-answered closure (the card owns that note)', () => {
    const html = renderRow(notice('timeout'))
    expect(html).not.toContain('已在执行会话中回复')
    expect(html).not.toContain('chat-turn-status')
  })
})

describe('human input is literal text, markdown is agent speech (26655b36)', () => {
  it('a user message renders literally — no markdown parsing', () => {
    const html = renderRow(row('user_message', { message_id: 'm1' }, '**不是粗体**'))
    expect(html).toContain('**不是粗体**')
    expect(html).not.toContain('<strong>')
    expect(html).toContain('bubble-raw')
  })

  it('an agent message still renders through the markdown shell', () => {
    const html = renderRow(row('agent_message', { message_id: 'm2' }, '**是粗体**'))
    expect(html).toContain('<strong>')
    expect(html).not.toContain('**是粗体**')
  })
})

describe('truncateQuote', () => {
  it('collapses whitespace and bounds to one line', () => {
    expect(truncateQuote('  a\n b  ')).toBe('a b')
    expect(truncateQuote(undefined)).toBe('')
    expect(truncateQuote('x'.repeat(80)).length).toBe(61)
  })
})

describe('Bubble: human input is literal text', () => {
  // The two human-authored surfaces (user↔user DM, execution detail) share
  // `Bubble`; both sides of the DM are human, so the caller says so rather
  // than the role deciding.
  const msg = (role: 'user' | 'assistant', content: string) => ({
    id: 'm1',
    role,
    content,
    created_at: '2026-10-24T00:00:00.000Z',
  }) as never

  it('renders markdown as prose when the caller says agent speech', () => {
    const html = renderToStaticMarkup(<Bubble msg={msg('assistant', '**粗体**')} />)
    expect(html).toContain('<strong>')
  })

  it('keeps the raw source when the caller says literal (both DM sides)', () => {
    const html = renderToStaticMarkup(<Bubble msg={msg('user', '**不是粗体**')} literal />)
    expect(html).toContain('**不是粗体**')
    expect(html).not.toContain('<strong>')
    expect(html).toContain('bubble-raw')
  })
})
