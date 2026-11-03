/**
 * `system_notice` rows on the mobile chat surface — the twin of Desktop's
 * `ChatNoticeEventRow` tests (ADR-106 D6/D8/D12).
 *
 * Two contracts these cases hold:
 *   1. a notice is a fact about the channel, so it paints as a quiet centered
 *      line and never as agent speech;
 *   2. the milestones the runtime stopped writing (dispatch / round-open /
 *      router fallback) and the card-resolution receipt stay INVISIBLE even in
 *      an old transcript — painting them would resurrect state that now belongs
 *      to the round's block.
 *
 * An addressing question is the exception worth pinning: it is a question FOR
 * the human, so it collapses to one line whose buttons open a sheet (§6.5), and
 * once the question has a round the buttons go quiet rather than vanishing —
 * disappearing on click would erase the record of what was decided, and a
 * re-click would deliver the same words twice (D6).
 */
import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { ChatNoticeRow } from '../components/ChatNoticeRow'
import type { ChatMessageRow } from '../lib/types'

function notice(noticeType: string, meta: ChatMessageRow['meta'] = {}, body = ''): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: 'ag',
    kind: 'system_notice',
    body,
    to: 'u1',
    meta: { message_id: `n-${noticeType}`, notice_type: noticeType, ...meta },
  } as ChatMessageRow
}

function render(row: ChatMessageRow, props: Partial<Parameters<typeof ChatNoticeRow>[0]> = {}): string {
  return renderToStaticMarkup(<ChatNoticeRow row={row} {...props} />)
}

const OPTIONS = [
  { session_id: 's-1', label: '整理季度报表', why: '最近活跃' },
  { session_id: 's-2', label: '核对发票', why: '标题匹配' },
]

describe('retired milestones stay history (D12)', () => {
  it.each([
    'turn_dispatched',
    'turn_started',
    'turn_routed',
    'router_fallback',
    'interaction_resolved',
  ])('%s paints nothing, with or without a body', (noticeType) => {
    expect(render(notice(noticeType, {}, '旧转录留下的正文'))).toBe('')
    expect(render(notice(noticeType))).toBe('')
  })
})

describe('addressing questions (D6 levels 3 and 5)', () => {
  const choose = () => {}

  it('a session_choice offers the pick and names the question', () => {
    const html = render(notice('session_choice', { options: OPTIONS, allow_new_task: true }), {
      onChoose: choose,
    })
    expect(html).toContain('data-testid="chat-card-options"')
    expect(html).toContain('你指的是哪一个任务？')
    expect(html).toContain('去选择')
  })

  it('a confirm_continue asks about the one session it proposes', () => {
    const html = render(notice('confirm_continue', { options: [OPTIONS[0]!] }), { onChoose: choose })
    expect(html).toContain('继续这个任务吗？')
  })

  it('an answered question goes quiet instead of disappearing', () => {
    const html = render(notice('session_choice', { options: OPTIONS, allow_new_task: true }), {
      onChoose: choose,
      answered: true,
    })
    expect(html).toContain('已回复')
    expect(html).toContain('disabled')
    expect(html).toContain('你指的是哪一个任务？')
  })

  it('a bystander gets the question without the action (D10)', () => {
    const html = render(notice('session_choice', { options: OPTIONS, allow_new_task: true }))
    expect(html).not.toContain('data-testid="chat-card-options"')
    expect(html).toContain('chat-notice-pill')
    expect(html).toContain('你指的是哪一个任务？')
  })

  it('a question with nothing to offer stays a plain line', () => {
    const html = render(notice('session_choice', { options: [] }), { onChoose: choose })
    expect(html).not.toContain('data-testid="chat-card-options"')
  })
})

describe('the facts about the channel', () => {
  it('a failed delivery is worded by the surface, not read off the wire', () => {
    const html = render(notice('delivery_failed'))
    expect(html).toContain('消息未能送达，请重试')
    expect(html).toContain('chat-notice-pill')
  })

  it('a limit notice carries its own text, and an empty one paints nothing', () => {
    expect(render(notice('budget_exhausted', {}, '上下文预算已用尽'))).toContain('上下文预算已用尽')
    expect(render(notice('unknown_type'))).toBe('')
  })

  it('a notice with no type but a status still says what happened', () => {
    const html = render(notice('', { status: 'truncated' }))
    expect(html).toContain('truncated')
  })
})
