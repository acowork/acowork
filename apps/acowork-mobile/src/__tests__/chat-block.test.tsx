/**
 * The round's block on the mobile chat surface (ADR-106 D3/D4/D8/D9/D13).
 *
 * Why this file exists: the transcript no longer carries the work — one
 * bodyless `execution_round` row names a round, and everything shown for it is
 * read from the execution side. The two failure modes these cases pin are the
 * ones the decision was written against:
 *
 *   1. inventing liveness — drawing "执行中" from anything other than a
 *      `pending` block read, which lets an unreachable host look like a dead
 *      node still working (D9);
 *   2. painting a hole as content — a missing round must say it is missing
 *      (D5), because inventing an answer would be writing history.
 *
 * The block store is MOCKED at its read hook: these cases are about what one
 * entry implies for the transcript (and static markup runs no effects, so the
 * real store would never have been read at all), not about batching — that is
 * `chatBlockStore.test.ts`.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { RowView, type RowProps } from '../screens/chat/AgentChatScreen'
import { ChatExecutionBlock } from '../components/ChatExecutionBlock'
import { blockRefOf, blockStatusText, isFailureStatus, truncateLabel } from '../lib/chatBlocks'
import type { ChatBlockEntry, ChatMessageRow } from '../lib/types'

const store = vi.hoisted(() => ({
  entry: undefined as ChatBlockEntry | undefined,
  useChatBlock: vi.fn(),
  reload: vi.fn(),
}))

vi.mock('../stores/chatBlockStore', () => ({
  useChatBlock: (chatId: string, ref?: string | null) => store.useChatBlock(chatId, ref),
  useChatBlockStore: (selector: (s: { reload: typeof store.reload }) => unknown) =>
    selector({ reload: store.reload }),
}))

const REF = 'ag|s1|r1'

function refRow(meta: ChatMessageRow['meta'] = {}): ChatMessageRow {
  return {
    seq: 3,
    ts: 1,
    from: 'ag',
    kind: 'execution_round',
    body: '',
    to: 'u1',
    meta: {
      message_id: `round:${REF}`,
      agent_instance_id: 'ag',
      agent_session_id: 's1',
      round_id: 'r1',
      ...meta,
    },
  } as ChatMessageRow
}

/** The one entry the rendered block reads. `undefined` = the read has not landed. */
function seed(entry?: ChatBlockEntry): void {
  store.entry = entry
}

function entryOf(over: Partial<ChatBlockEntry> = {}): ChatBlockEntry {
  return {
    ref: REF,
    agent_instance_id: 'ag',
    agent_session_id: 's1',
    round_id: 'r1',
    status: 'ok',
    projection: {
      round_id: 'r1',
      terminal: true,
      status: 'completed',
      answer: { body: '报表已整理好。', ts: '1' },
      card: null,
    },
    ...over,
  }
}

function renderRow(row: ChatMessageRow, over: Partial<RowProps> = {}): string {
  const props: RowProps = {
    row,
    chatSessionId: 'cs-1',
    isSelf: false,
    selfAvatar: 'me',
    selfName: 'me',
    agentAvatar: 'ag',
    agentName: 'agent',
    onJumpToExec: () => {},
    onReplyToRound: () => {},
    onOpenAttachment: () => {},
    ...over,
  }
  return renderToStaticMarkup(<RowView {...props} />)
}

beforeEach(() => {
  store.entry = undefined
  store.useChatBlock.mockImplementation((_chatId: string, ref?: string | null) => (ref ? store.entry : undefined))
  store.reload.mockClear()
})

describe('the block read (D3/D4/D5)', () => {
  it('a ref is the row coordinate triple — and nothing without all three parts', () => {
    expect(blockRefOf({ agent_instance_id: 'ag', agent_session_id: 's1', round_id: 'r1' })).toBe(REF)
    expect(blockRefOf({ agent_instance_id: 'ag', agent_session_id: 's1' })).toBeNull()
    expect(blockRefOf({ agent_instance_id: 'ag', round_id: 'r1' })).toBeNull()
    expect(blockRefOf({ agent_session_id: 's1', round_id: 'r1' })).toBeNull()
    expect(blockRefOf(undefined)).toBeNull()
  })

  it('a terminal read paints the answer inline, in the chat session it belongs to', () => {
    seed(entryOf())
    const html = renderRow(refRow())
    expect(html).toContain('data-testid="exec-block"')
    expect(html).toContain(`data-ref="${REF}"`)
    expect(html).toContain('data-chat="cs-1"')
    expect(html).toContain(blockStatusText('completed'))
    expect(html).toContain('报表已整理好')
  })

  it('a row with no coordinates says the round is unavailable, not empty', () => {
    expect(renderRow(refRow({ round_id: undefined as never }))).toContain('本轮执行已不可用')
  })

  it('a read that has not landed holds the line, so a run is not a missing message', () => {
    seed(undefined)
    expect(renderRow(refRow())).toContain('正在加载本轮执行…')
  })

  it('not_found is an explicit hole (D5)', () => {
    seed(entryOf({ status: 'not_found', projection: undefined }))
    expect(renderRow(refRow())).toContain('本轮执行已不可用')
  })
})

describe('liveness is positive evidence only (D9)', () => {
  it('pending is the one status that may read "执行中"', () => {
    seed(
      entryOf({
        status: 'pending',
        source: 'runtime',
        projection: { round_id: 'r1', terminal: false, status: 'in_progress' },
      }),
    )
    const html = renderRow(refRow())
    expect(html).toContain('执行中')
    expect(html).toContain('处理中…')
  })

  it('an unreachable read is an outage with a retry, never a run in flight', () => {
    seed(entryOf({ status: 'unreachable', projection: undefined, detail: 'connection refused' }))
    const html = renderRow(refRow())
    expect(html).toContain('执行所在的主机暂不可达')
    expect(html).toContain('重试')
    expect(html).not.toContain('执行中')
    expect(html).not.toContain('处理中')
  })

  it('a failed round is worded as a fault, an abort is not', () => {
    expect(blockStatusText('failed')).toBe('本轮执行失败')
    expect(blockStatusText('aborted')).toBe('本轮执行已中断')
    expect(isFailureStatus('failed')).toBe(true)
    expect(isFailureStatus('timeout')).toBe(true)
    expect(isFailureStatus('aborted')).toBe(false)
  })
})

describe('the decision lives in the block (D8)', () => {
  it('an approval is one collapsed line that opens the sheet', () => {
    seed(
      entryOf({
        status: 'pending',
        projection: {
          round_id: 'r1',
          terminal: false,
          status: 'waiting',
          card: { kind: 'approval', request_id: 'rq-1', prompt: '允许执行 bash？' },
        },
      }),
    )
    const html = renderRow(refRow())
    expect(html).toContain('data-testid="block-card-open"')
    expect(html).toContain('需要审批')
    expect(html).toContain('允许执行 bash？')
  })

  it('a pause is answered by the composer, so it addresses the round (D6 level 1)', () => {
    seed(
      entryOf({
        status: 'pending',
        projection: {
          round_id: 'r1',
          terminal: false,
          status: 'waiting',
          card: { kind: 'pause', request_id: 'rq-1', prompt: '下一步要改配置' },
        },
      }),
    )
    const html = renderRow(refRow())
    expect(html).toContain('data-testid="block-reply"')
    expect(html).toContain('继续执行')
    expect(html).not.toContain('data-testid="block-card-open"')
  })

  it('a viewer who is not the round answerer gets the question, not the buttons (D10)', () => {
    const card = {
      kind: 'ask_question',
      request_id: 'rq-1',
      prompt: '选哪个格式？',
      options: ['CSV', 'XLSX'],
    }
    seed(
      entryOf({
        projection: { round_id: 'r1', terminal: true, status: 'waiting', card },
      }),
    )
    // The block's own gate: RowView hands it the answerer's coordinates, and a
    // surface that resolved the viewer as somebody else must still show what the
    // round is waiting for — with the reason the buttons are gone.
    const html = renderToStaticMarkup(
      <ChatExecutionBlock
        row={refRow()}
        chatSessionId="cs-1"
        onJumpToExec={() => {}}
        onReplyToRound={() => {}}
        canAnswer={false}
      />,
    )
    expect(html).not.toContain('data-testid="block-card-open"')
    expect(html).toContain('选哪个格式？')
    expect(html).toContain('只有发起本轮的人可以回复')
    const answerer = renderToStaticMarkup(
      <ChatExecutionBlock
        row={refRow()}
        chatSessionId="cs-1"
        onJumpToExec={() => {}}
        onReplyToRound={() => {}}
      />,
    )
    expect(answerer).toContain('data-testid="block-card-open"')
    expect(answerer).not.toContain('只有发起本轮的人可以回复')
  })

  it('the task id rides the block header, never a bubble chip (D13)', () => {
    seed(entryOf())
    const html = renderRow(refRow({ task_id: 't-abc' }))
    expect(html).toContain('chat-block-task')
    expect(html).toContain('t-abc')
    expect(html).not.toContain('chat-task-chip')
  })
})

describe('the retired shapes stay retired (D8/D12)', () => {
  it('a legacy card row paints nothing — the decision is shown in the block', () => {
    const row = {
      seq: 4,
      ts: 2,
      from: 'ag',
      kind: 'interaction_request',
      body: '允许执行 bash？',
      to: 'u1',
      meta: { message_id: 'card-1', in_reply_to: 'm1' },
    } as ChatMessageRow
    expect(renderRow(row)).toBe('')
  })

  it('a bodyless Router answer says so instead of collapsing into an empty bubble', () => {
    const row = {
      seq: 5,
      ts: 3,
      from: 'ag',
      kind: 'agent_message',
      body: '',
      to: 'u1',
      meta: { message_id: 'm2' },
    } as ChatMessageRow
    expect(renderRow(row)).toContain('（无内容）')
  })
})

describe('truncateLabel', () => {
  it('collapses whitespace and bounds one line', () => {
    expect(truncateLabel('  a\n b  ')).toBe('a b')
    expect(truncateLabel('')).toBe('')
    expect(truncateLabel('x'.repeat(80), 34)).toBe(`${'x'.repeat(34)}…`)
  })
})
