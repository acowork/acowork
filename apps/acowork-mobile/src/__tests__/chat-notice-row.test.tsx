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
 * the human, so it collapses to one card whose picks open a sheet (§6.5). Once
 * the question has a round it settles into a receipt naming the session that
 * took the answer — the record survives, nothing is left to press, and a second
 * click cannot deliver the same words twice (D6). Its options, its answered-ness
 * and the session that answered come from the service's projection
 * (`row.delivery.question`, design 31 §6), which is what these cases feed it.
 */
import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { ChatNoticeRow } from '../components/ChatNoticeRow'
import type { ChatCandidateOption, ChatMessageRow } from '../lib/types'

function notice(
  noticeType: string,
  meta: ChatMessageRow['meta'] = {},
  body = '',
  delivery?: ChatMessageRow['delivery'],
): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: 'ag',
    kind: 'system_notice',
    body,
    to: 'u1',
    meta: { message_id: `n-${noticeType}`, notice_type: noticeType, ...meta },
    ...(delivery ? { delivery } : {}),
  } as ChatMessageRow
}

/** What the service projects onto a card (design 31 §6). The surface reads
 *  this and nothing else: `meta.options` is the durable payload, not the
 *  surface's input. */
function card(
  options: ChatCandidateOption[] = OPTIONS,
  {
    answered = false,
    allowNewTask = true,
    answeredSession,
  }: { answered?: boolean; allowNewTask?: boolean; answeredSession?: string } = {},
): ChatMessageRow['delivery'] {
  return {
    answered: false,
    question: {
      answered,
      ...(answeredSession ? { answered_session: answeredSession } : {}),
      options,
      ...(allowNewTask ? { allow_new_task: true } : {}),
    },
  }
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
    const html = render(notice('session_choice', {}, '', card()), {
      onChoose: choose,
    })
    expect(html).toContain('data-testid="chat-card-options"')
    expect(html).toContain('你指的是哪一个任务？')
    // Several candidates: the row asks, the sheet carries the labelled picks, and
    // the action is a button with its own hit area — a box that only holds text
    // reads as a disabled field.
    expect(html).toContain('选择')
    expect(html).toContain('chat-notice-card-go')
  })

  it('a confirm_continue asks about the one session it proposes', () => {
    const html = render(notice('confirm_continue', {}, '', card([OPTIONS[0]!])), {
      onChoose: choose,
    })
    expect(html).toContain('继续这个任务吗？')
  })

  it('the card names the one session a confirm_continue would continue', () => {
    // The card proposes exactly ONE session, so the collapsed row has to name it:
    // "继续这个任务吗？ / 去选择" over an anonymous proposal asked the human to
    // continue a task they could not identify without opening the sheet. The
    // payload carries the candidate's own title — it belongs on the row.
    const html = render(
      notice(
        'confirm_continue',
        {},
        '',
        card([{ session_id: 's-9', label: '发了个用户消息给SSE.kevin的chat session' }]),
      ),
      { onChoose: choose },
    )
    expect(html).toContain('继续这个任务吗？')
    expect(html).toContain('发了个用户消息给SSE.kevin的chat session')
  })

  it('names an untitled proposal by its session id rather than nothing', () => {
    const html = render(
      notice('confirm_continue', {}, '', card([{ session_id: 's-abcdef12' }])),
      { onChoose: choose },
    )
    expect(html).toContain('s-abcdef')
  })

  it('a multi-candidate question keeps its names in the sheet, not the row', () => {
    // `session_choice` offers several sessions; putting all titles on a 390pt
    // row is where a mis-tap sends a turn into the wrong task, so the row stays
    // the question and the sheet carries the labelled picks.
    const html = render(notice('session_choice', {}, '', card()), { onChoose: choose })
    expect(html).toContain('你指的是哪一个任务？')
    expect(html).not.toContain('整理季度报表')
  })

  it('an answered question settles into a receipt naming where it went', () => {
    // The service says the question is settled AND which session ran the answer
    // (`question.answered_session`). The record survives, but as one line: the
    // human's question after clicking is "did it go where I meant", and a dead
    // button answers that with nothing.
    const html = render(
      notice('session_choice', {}, '', card(OPTIONS, { answered: true, answeredSession: 's-2' })),
      { onChoose: choose },
    )
    expect(html).toContain('data-testid="chat-card-answered"')
    expect(html).toContain('已交给 核对发票')
    expect(html).not.toContain('chat-notice-card-go')
  })

  it('says a new task was started when the answer names no candidate', () => {
    const html = render(
      notice(
        'confirm_continue',
        {},
        '',
        card([OPTIONS[0]!], { answered: true, answeredSession: 's-new' }),
      ),
      { onChoose: choose },
    )
    expect(html).toContain('已开新任务')
  })

  it('a bystander gets the question and the reason it is not theirs (D10)', () => {
    // The question is a fact about the channel, the click is the speaker's. So a
    // read-only viewer sees the card — and a line saying why it offers no action,
    // rather than a dead button or a bare sentence with no buttons at all.
    const html = render(notice('session_choice', {}, '', card()))
    expect(html).toContain('data-testid="chat-card-options"')
    expect(html).toContain('你指的是哪一个任务？')
    expect(html).toContain('只有发话的人可以回答')
    expect(html).not.toContain('chat-notice-card-go')
  })

  it('a question with nothing to offer paints nothing', () => {
    // Pre-projection leftovers carry no options: nothing to choose, nothing
    // chosen. A pill asking "你指的是哪一个任务？" with no way to answer reads as
    // the agent talking to itself in the middle of the transcript.
    expect(render(notice('session_choice', {}, '', card([])), { onChoose: choose })).toBe('')
    expect(render(notice('confirm_continue'))).toBe('')
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
