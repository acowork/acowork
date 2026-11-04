/**
 * The session's block on the mobile chat surface (ADR-106 D3/D4/D5/D8/D9/D13).
 *
 * Why this file exists: the transcript no longer carries the work — one
 * bodyless `execution_round` row names a round, everything shown for it is read
 * from the execution side, and the LAYOUT folds a session's rows into one block.
 * The failure modes these cases pin are the ones the decision was written
 * against:
 *
 *   1. one block per ROW — a long-lived run became a stack of near-identical
 *      cards, which is neither the transcript's nor the execution surface's
 *      grammar (D3/D4);
 *   2. inventing liveness — drawing "执行中" from anything other than a
 *      `pending` block read, which lets an unreachable host look like a dead
 *      node still working (D9);
 *   3. painting a hole as content — a missing round must say it is missing (D5),
 *      because inventing an answer would be writing history.
 *
 * The block store is MOCKED at its read hook: these cases are about what one
 * entry implies for the transcript (and static markup runs no effects, so the
 * real store would never have been read at all), not about batching — that is
 * `chatBlockStore.test.ts`.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { createRoot, type Root } from 'react-dom/client'
import { ItemView, RowView, type ItemProps, type RowProps } from '../screens/chat/AgentChatScreen'
import { ChatExecutionBlock } from '../components/ChatExecutionBlock'
import { blockRefOf, blockStatusText, isFailureStatus, truncateLabel } from '../lib/chatBlocks'
import { buildTranscriptItems, type RunBlock } from '../lib/chatTranscriptGroups'
import type { ChatBlockEntry, ChatMessageRow } from '../lib/types'

const store = vi.hoisted(() => ({
  /** The foreground session's live status, as the chat store would hold it. */
  live: null as { status: string } | null,
  /** The one entry the rendered rounds read, keyed by their ref. */
  entry: undefined as ChatBlockEntry | undefined,
  entries: undefined as Record<string, ChatBlockEntry> | undefined,
  useChatBlocks: vi.fn(),
  reload: vi.fn(),
}))

// The live line is read off the chat store, but a static render never reaches a
// subscription: the store is stood in for, so a case can say what the foreground
// session is doing. Everything else about the store stays real.
vi.mock('../stores/chatStore', async (importOriginal) => {
  const orig = await importOriginal<typeof import('../stores/chatStore')>()
  const useChatStore = (selector: (s: unknown) => unknown) =>
    selector({
      agentStates: store.live
        ? { ag: { activeSessionId: 's1', status: store.live } }
        : {},
    })
  return {
    ...orig,
    useChatStore: Object.assign(useChatStore, {
      getState: orig.useChatStore.getState,
      setState: orig.useChatStore.setState,
    }),
  }
})

vi.mock('../stores/chatBlockStore', () => ({
  useChatBlocks: (chatId: string, refs: readonly string[]) =>
    store.useChatBlocks(chatId, refs),
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
      message_id: 'round:r1',
      agent_instance_id: 'ag',
      agent_session_id: 's1',
      round_id: 'r1',
      ...meta,
    },
  } as ChatMessageRow
}

/** The human's own row that opened a round: folded into the block as its
 *  question line, never painted twice. */
function triggerRow(id: string, body: string, seq = 2): ChatMessageRow {
  return {
    seq,
    ts: 0,
    from: 'u1',
    kind: 'user_message',
    body,
    to: 'ag',
    meta: { message_id: id },
  } as ChatMessageRow
}

/** `undefined` = the read has not landed. */
function seed(entry?: ChatBlockEntry): void {
  store.entry = entry
  store.entries = undefined
}

/** Every ref of the rendered block resolves to the same entry. */
function seedEntries(entries: Record<string, ChatBlockEntry>): void {
  store.entries = entries
  store.entry = undefined
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

function rowProps(over: Partial<RowProps> = {}): RowProps {
  return {
    row: refRow(),
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
}

function renderRow(row: ChatMessageRow, over: Partial<RowProps> = {}): string {
  return renderToStaticMarkup(<RowView {...rowProps({ row, ...over })} />)
}

/** Render the transcript the surface renders: the layout folds it, so a block is
 *  reached through an item, never by hand. */
function renderRows(rows: ChatMessageRow[], over: Partial<ItemProps> = {}): string {
  const layout = buildTranscriptItems(rows, (r) => `k:${r.meta?.message_id}`)
  return renderToStaticMarkup(
    <>
      {layout.items.map((item, i) => (
        <ItemView
          key={item.kind === 'run' ? item.anchorRow.seq : item.row.seq}
          item={item}
          hasFollowUp={i < layout.items.length - 1}
          {...rowProps(over as Partial<RowProps>)}
          {...over}
        />
      ))}
    </>,
  )
}

/** The single block a set of rows folds into — fails loudly when it is not one. */
function onlyBlock(rows: ChatMessageRow[]): RunBlock {
  const run = buildTranscriptItems(rows, (r) => `k:${r.meta?.message_id}`).items.find(
    (i) => i.kind === 'run',
  )
  if (!run || run.kind !== 'run') throw new Error('expected exactly one run item')
  return run
}

/** One block, opened: rendered away from the transcript's follow rule, so the
 *  assertions are about what a round shows rather than whether the block folds. */
function renderBlock(block: RunBlock, over: Partial<ItemProps> = {}): string {
  return renderToStaticMarkup(
    <ItemView
      item={block}
      hasFollowUp={false}
      {...rowProps(over as Partial<RowProps>)}
      {...over}
    />,
  )
}

beforeEach(() => {
  store.entry = undefined
  store.entries = undefined
  store.useChatBlocks.mockImplementation((_chatId: string, refs: readonly string[]) => {
    if (store.entries) return store.entries
    const ref = refs[0]
    return ref && store.entry ? { [ref]: store.entry } : {}
  })
  store.reload.mockClear()
})

describe('the fold: one block per execution session (D3/D4)', () => {
  it('the rounds of ONE session fold into a single block anchored at the newest', () => {
    const rows = [
      triggerRow('u1', '帮我整理报表'),
      refRow({ in_reply_to: 'u1' }),
      triggerRow('u2', '再按月份汇总', 4),
      refRow({ round_id: 'r2', in_reply_to: 'u2', message_id: 'round:r2' }),
    ]
    const blocks = buildTranscriptItems(rows, (r) => `k:${r.meta?.message_id}`).items.filter(
      (i) => i.kind === 'run',
    )
    expect(blocks).toHaveLength(1)
    const block = onlyBlock(rows)
    expect(block.rounds.map((r) => r.ref)).toEqual(['ag|s1|r1', 'ag|s1|r2'])
    // The block sits where the session's most recent work is, not where it began.
    expect(block.anchorRow.meta?.round_id).toBe('r2')
    seedEntries({
      'ag|s1|r1': entryOf({ ref: 'ag|s1|r1' }),
      'ag|s1|r2': entryOf({ ref: 'ag|s1|r2', round_id: 'r2' }),
    })
    const html = renderRows(rows)
    // One block, two rounds — and the human's own rows are folded INSIDE it, so
    // the transcript holds no standalone bubble for them (D6's re-delivery rule
    // is the precedent for "one sentence, one place").
    expect(html.match(/data-testid="exec-block"/g)).toHaveLength(1)
    expect(html).toContain('第 1 轮')
    expect(html).toContain('第 2 轮')
    expect(html).not.toContain('bubble-raw')
    // Only the newest round shows its pair: that is what keeps a long run short.
    expect(html.match(/chat-run-question/g)).toHaveLength(1)
  })

  it('two execution sessions in one chat stay two blocks, not one merged run', () => {
    const rows = [
      triggerRow('u1', '任务甲'),
      refRow({ in_reply_to: 'u1' }),
      refRow({ agent_session_id: 's2', round_id: 'r9', in_reply_to: 'u1', message_id: 'round:s2' }),
    ]
    const items = buildTranscriptItems(rows, (r) => `k:${r.meta?.message_id}`).items
    const runs = items.filter((i) => i.kind === 'run') as RunBlock[]
    expect(runs).toHaveLength(2)
    // The fan-out trigger belongs to the FIRST block; the second must not render
    // the same sentence again.
    expect(runs[0]!.rounds[0]!.showUserLine).toBe(true)
    expect(runs[1]!.rounds[0]!.showUserLine).toBe(false)
    seedEntries({
      'ag|s1|r1': entryOf({ ref: 'ag|s1|r1' }),
      'ag|s2|r9': entryOf({ ref: 'ag|s2|r9', agent_session_id: 's2', round_id: 'r9' }),
    })
    const html = renderBlock(runs[0]!) + renderBlock(runs[1]!)
    // The question line is rendered once across the two blocks: the second owns
    // the answer only.
    expect(html.match(/chat-run-question/g)).toHaveLength(1)
  })

  /** ADR-106 presentation-contract amendment: the round that OPENED the
   *  session pairs with its own question bubble — "question, then block" —
   *  and the block's line leads with the router's brief instead. */
  it('an opener round keeps its trigger as its own bubble', () => {
    const rows = [
      triggerRow('u1', '修复分页不满屏', 1),
      refRow({ in_reply_to: 'u1', opened_new: true, assignment_brief: 'Fix the pagination bug' }),
      triggerRow('u2', 'also add tests', 4),
      { ...refRow({ round_id: 'r2', message_id: 'round:r2', in_reply_to: 'u2', opened_new: false }), seq: 5 },
    ]
    const { items, keyOfSeq } = buildTranscriptItems(rows, (r) => `k:${r.meta?.message_id}`)
    // The opener's question stands alone; the continuation stays folded — one
    // bubble, then the block that answers both.
    expect(items.map((i) => i.kind)).toEqual(['row', 'run'])
    const block = items[1] as RunBlock
    const [opener, continuation] = block.rounds
    expect(opener?.questionOnTranscript).toBe(true)
    expect(opener?.showUserLine).toBe(false)
    // The block still holds the trigger row for the header's title derivation.
    expect(opener?.triggerRow?.body).toBe('修复分页不满屏')
    expect(continuation?.questionOnTranscript).toBeUndefined()
    expect(continuation?.showUserLine).toBe(true)
    // Locate: the opener's seq points at its OWN bubble, the continuation's
    // trigger still points at the block that draws it.
    expect(keyOfSeq.get(1)).toBe('k:u1')
    expect(keyOfSeq.get(4)).toBe('k:round:r2')

    seedEntries({
      'ag|s1|r1': entryOf({
        projection: {
          round_id: 'r1',
          terminal: true,
          status: 'completed',
          user: { body: 'Fix the pagination bug\n\n[user message]\n修复分页不满屏', ts: '1' },
          answer: { body: 'done', ts: '2' },
          card: null,
        },
      }),
      'ag|s1|r2': entryOf({ ref: 'ag|s1|r2', round_id: 'r2' }),
    })
    const html = renderRows(rows)
    // The human's words stay on the transcript as their own bubble…
    expect(html).toContain('bubble-raw')
    // …and the opener's block line leads with the brief, not the delivered
    // prompt text (which would carry the `[user message]` tail verbatim).
    expect(html).toContain('>Fix the pagination bug<')
    expect(html).not.toContain('[user message]')
    // Only the continuation round renders a question panel inside the block.
    expect(html.match(/chat-run-question/g)).toHaveLength(1)
  })

  it('a reference row with no coordinates is a hole of its own, not a block', () => {
    expect(renderRow(refRow({ round_id: undefined as never }))).toContain('本轮执行已不可用')
    expect(renderRow(refRow({ round_id: undefined as never }))).not.toContain('exec-block')
  })
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
    const html = renderRows([triggerRow('u1', '帮我整理报表'), refRow({ in_reply_to: 'u1' })])
    expect(html).toContain('data-testid="exec-block"')
    expect(html).toContain('data-session="ag|s1"')
    expect(html).toContain(blockStatusText('completed'))
    expect(html).toContain('报表已整理好')
    expect(html).toContain('帮我整理报表')
  })

  it('a read that has not landed holds the line, so a run is not a missing message', () => {
    seed(undefined)
    expect(renderRows([refRow()])).toContain('正在加载本轮执行…')
  })

  it('not_found is an explicit hole (D5)', () => {
    seed(entryOf({ status: 'not_found', projection: undefined }))
    expect(renderRows([refRow()])).toContain('本轮执行已不可用')
  })

  it('a round whose answer is gone reports the outcome, not a blank box', () => {
    seed(
      entryOf({
        projection: { round_id: 'r1', terminal: true, status: 'failed' },
      }),
    )
    expect(renderRows([refRow()])).toContain('本轮执行失败')
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
    const html = renderRows([refRow()])
    expect(html).toContain('执行中')
    // The retired per-round "处理中…" line is gone: the block header carries the
    // session's state, and a round carries its own read.
    expect(html).not.toContain('处理中')
  })

  it('an unreachable read is an outage with a retry, never a run in flight', () => {
    seed(entryOf({ status: 'unreachable', projection: undefined, detail: 'connection refused' }))
    const html = renderRows([refRow()])
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

/**
 * What the block's header claims (t-1b7bdb21) — the failure this suite was
 * extended for:
 *  1. the human's own sentence vanished from the transcript: the round that owns
 *     a folded chat question sat outside the recent-rounds window, so the only
 *     place the sentence was ever drawn was hidden — and the visible rows read
 *     as the agent answering questions nobody asked here;
 *  2. the state word answered "is ANY round of this session pending" instead of
 *     "how did the round you are reading end": a run parked on a card read as
 *     "执行中" (it is waiting for the human) and a settled newest round could
 *     still look live. On a 390pt screen the header is the only thing a folded
 *     block shows, so a wrong word there is the whole story.
 */
describe("what the block's header claims (t-1b7bdb21)", () => {
  it("never windows away the round that carries this Chat's own question", () => {
    // One chat question opened round 1; the human then kept nudging from the
    // execution surface, so rounds 2 and 3 answer sentences this Chat never saw.
    const rows = [
      triggerRow('u1', '帮我整理报表'),
      refRow({ in_reply_to: 'u1' }),
      refRow({ round_id: 'r2', message_id: 'round:r2' }),
      refRow({ round_id: 'r3', message_id: 'round:r3' }),
    ]
    const block = onlyBlock(rows)
    seedEntries({
      'ag|s1|r1': entryOf({ ref: 'ag|s1|r1' }),
      'ag|s1|r2': entryOf({ ref: 'ag|s1|r2', round_id: 'r2' }),
      'ag|s1|r3': entryOf({ ref: 'ag|s1|r3', round_id: 'r3' }),
    })
    const html = renderBlock(block)
    // All three rounds are listed, so nothing sits behind "查看更早".
    expect(html).toContain('第 1 轮')
    expect(html).toContain('第 3 轮')
    expect(html).not.toContain('查看更早')
    expect(html).toContain('帮我整理报表')
  })

  it('a settled block reads 已完成 even when an older read still says pending', () => {
    const rows = [triggerRow('u1', 'q'), refRow({ in_reply_to: 'u1' }), refRow({ round_id: 'r2', message_id: 'round:r2' })]
    seedEntries({
      'ag|s1|r1': entryOf({ ref: 'ag|s1|r1', status: 'pending', projection: { round_id: 'r1', terminal: false, status: 'in_progress' } }),
      'ag|s1|r2': entryOf({ ref: 'ag|s1|r2', round_id: 'r2' }),
    })
    const html = renderBlock(onlyBlock(rows))
    // The newest round is the block's claim; an older read that never resolves
    // must not keep the whole run looking live.
    expect(html).toContain(blockStatusText('completed'))
    expect(html).not.toContain('执行中')
  })

  it('a round parked on a card reads as waiting for the human, not as running', () => {
    seed(
      entryOf({
        status: 'pending',
        projection: {
          round_id: 'r1',
          terminal: false,
          status: 'waiting',
          card: { kind: 'approval', request_id: 'q1', prompt: 'Approve `bash`?' },
        },
      }),
    )
    const html = renderRows([refRow()])
    expect(html).toContain(blockStatusText('waiting'))
    expect(html).not.toContain('执行中')
  })

  it('a newest read that is a hole says so instead of claiming a state', () => {
    seed(entryOf({ status: 'not_found', projection: undefined }))
    const html = renderRows([refRow()])
    expect(html).toContain(blockStatusText('unknown'))
    expect(html).not.toContain('执行中')
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
    const html = renderRows([refRow()])
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
    const html = renderRows([refRow()])
    expect(html).toContain('data-testid="block-reply"')
    expect(html).toContain('继续执行')
    expect(html).not.toContain('data-testid="block-card-open"')
  })

  it('a parked card stays reachable while the block is folded (§6.5)', () => {
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
    // `hasFollowUp` is what starts a block folded: a parked round must still be
    // answerable under it, so the card sits outside the folded body.
    const folded = renderToStaticMarkup(
      <ChatExecutionBlock
        block={onlyBlock([refRow()])}
        chatSessionId="cs-1"
        hasFollowUp
        agentAvatar="ag"
        agentName="agent"
        onJumpToExec={() => {}}
        onReplyToRound={() => {}}
      />,
    )
    // The initial state is `!hasFollowUp`, so a folded block shows the card and
    // no round list.
    expect(folded).not.toContain('第 1 轮')
    expect(folded).toContain('data-testid="block-card-open"')
  })

  it('a viewer who is not the round answerer gets the question, not the buttons (D10)', () => {
    const card = {
      kind: 'ask_question',
      request_id: 'rq-1',
      prompt: '选哪个格式？',
      options: ['CSV', 'XLSX'],
    }
    seed(entryOf({ projection: { round_id: 'r1', terminal: true, status: 'waiting', card } }))
    // The block's own gate: the surface hands it the viewer's standing, and a
    // surface that resolved the viewer as somebody else must still show what the
    // round is waiting for — with the reason the buttons are gone.
    const html = renderToStaticMarkup(
      <ChatExecutionBlock
        block={onlyBlock([refRow()])}
        chatSessionId="cs-1"
        agentAvatar="ag"
        agentName="agent"
        onJumpToExec={() => {}}
        onReplyToRound={() => {}}
        canAnswer={false}
      />,
    )
    const answerer = renderToStaticMarkup(
      <ChatExecutionBlock
        block={onlyBlock([refRow()])}
        chatSessionId="cs-1"
        agentAvatar="ag"
        agentName="agent"
        onJumpToExec={() => {}}
        onReplyToRound={() => {}}
      />,
    )
    expect(html).not.toContain('data-testid="block-card-open"')
    expect(html).toContain('选哪个格式？')
    expect(html).toContain('只有发起本轮的人可以回复')
    expect(answerer).toContain('data-testid="block-card-open"')
    expect(answerer).not.toContain('只有发起本轮的人可以回复')
  })

  it('the task id rides the block header (D13)', () => {
    seed(entryOf())
    const html = renderRows([refRow({ task_id: 't-abc' })])
    expect(html).toContain('explore-toggle-summary')
    expect(html).toContain('t-abc')
    expect(html).not.toContain('chat-task-chip')
  })

  it('the block header names the run and counts its rounds', () => {
    seedEntries({
      'ag|s1|r1': entryOf({ ref: 'ag|s1|r1' }),
      'ag|s1|r2': entryOf({ ref: 'ag|s1|r2', round_id: 'r2' }),
    })
    const html = renderRows([
      triggerRow('u1', '帮我整理一份季度报表'),
      refRow({ in_reply_to: 'u1' }),
      refRow({ round_id: 'r2', message_id: 'round:r2' }),
    ])
    expect(html).toContain('2 轮')
    // Untitled run: the opening question is the name, so a block and the tab
    // that later shows its session do not name the same run two ways.
    expect(html).toContain('帮我整理一份季度报表')
  })
})

describe('the live line the SESSION owns (D9)', () => {
  afterEach(() => {
    store.live = null
  })

  it('a busy session speaks in the header even after the block reads settle', () => {
    seed(entryOf())
    store.live = { status: 'tool_executing' }
    const html = renderRows([triggerRow('u1', '帮我整理报表'), refRow({ in_reply_to: 'u1' })])
    // `session_state` is the freshest fact on this surface: the round it is busy
    // with may not have reached this Chat as a reference row yet.
    expect(html).toContain('执行工具中')
    // It may not manufacture liveness though: the read is terminal, so the block
    // stays settled — no live marker, and the answer is still its own word.
    expect(html).not.toContain('is-live')
  })

  it('an idle session leaves the header to the block own outcome', () => {
    seed(entryOf())
    store.live = { status: 'idle' }
    const html = renderRows([triggerRow('u1', '帮我整理报表'), refRow({ in_reply_to: 'u1' })])
    expect(html).toContain('已完成')
    expect(html).not.toContain('is-live')
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

// The block's in-place reply (ADR-106 rev 2026-11-04). A static render proves
// presence/absence; the send and the focus rising edge need a live tree, so this
// block renders `ChatExecutionBlock` in jsdom (the file already mocks the block +
// chat stores). It is NOT hover-gated like the desktop twin — a phone has no
// hover — it lives at the bottom of the OPEN body and vanishes when folded.
describe('in-place reply row (ADR-106 rev 2026-11-04)', () => {
  let host: HTMLDivElement
  let root: Root

  beforeEach(() => {
    ;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true
    host = document.createElement('div')
    document.body.append(host)
    root = createRoot(host)
    seed(entryOf())
  })

  afterEach(() => {
    act(() => root.unmount())
    host.remove()
    store.entry = undefined
    store.entries = undefined
  })

  function renderExec(block: RunBlock, over: { focusInput?: boolean; hasFollowUp?: boolean; onSend?: (rowId: string, text: string) => void } = {}) {
    act(() =>
      root.render(
        <ChatExecutionBlock
          block={block}
          chatSessionId="cs-1"
          agentAvatar="ag"
          agentName="agent"
          hasFollowUp={over.hasFollowUp ?? false}
          onJumpToExec={() => {}}
          onReplyToRound={() => {}}
          canAnswer
          focusInput={over.focusInput ?? false}
          onReplyInPlace={over.onSend ?? (() => {})}
        />,
      ),
    )
  }

  const box = () => host.querySelector<HTMLTextAreaElement>('textarea.chat-block-reply-input')

  it('shows the reply row at the bottom of an open block', () => {
    renderExec(onlyBlock([refRow()]))
    expect(box()).not.toBeNull()
  })

  it('a folded block never shows an input (§6.5)', () => {
    renderExec(onlyBlock([refRow()]), { hasFollowUp: true })
    expect(host.querySelector('.chat-block-reply')).toBeNull()
  })

  it('typing and sending continues the freshest round straight down level 1', () => {
    // The freshest round's reference row id is the coordinate a level-1 reply
    // answers — not the block's `ag|s1|r1` ref string.
    const onSend = vi.fn()
    renderExec(onlyBlock([refRow({ message_id: 'round:ag|s1|r1' })]), { onSend })
    const el = box()!
    const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value')!.set!
    act(() => {
      setter.call(el, 'continue please')
      el.dispatchEvent(new Event('input', { bubbles: true }))
    })
    const send = host.querySelector<HTMLButtonElement>('[data-testid="block-reply-send"]')!
    expect(send.disabled).toBe(false)
    act(() => send.click())
    expect(onSend).toHaveBeenCalledWith('round:ag|s1|r1', 'continue please')
    // The box stays for the next continuation, so it clears rather than vanishes.
    expect(box()!.value).toBe('')
  })

  it('a locate (去续投) rising edge focuses the row once, not on every re-render', () => {
    const block = onlyBlock([refRow()])
    renderExec(block, { focusInput: false })
    expect(document.activeElement).not.toBe(box())
    // The `@` sheet located this run: the flag rises and the box takes focus.
    renderExec(block, { focusInput: true })
    expect(document.activeElement).toBe(box())
    // A held signal (re-render, still true) must not yank focus back mid-typing.
    const el = box()!
    el.blur()
    renderExec(block, { focusInput: true })
    expect(document.activeElement).not.toBe(el)
  })
})
