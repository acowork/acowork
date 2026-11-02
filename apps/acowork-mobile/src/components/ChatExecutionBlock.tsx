/**
 * ChatExecutionBlock — one `execution_round` reference row, rendered as the run
 * it points at (ADR-106 D3/D4/D8/D9). The mobile twin of Desktop's
 * `components/chat/ChatExecutionBlock`.
 *
 * The chat holds no copy of the work. The row names a round (`instance|session|
 * round`) and everything shown here is read from that round's projection at
 * render time, through the Gateway's participant-checked block read (D10).
 *
 * The visual language is this app's execution-surface `ExploreBlock`: a hairline
 * card, a quiet header line stating the outcome, content below, tap the header to
 * fold. Design 27 §6.5's mobile deltas:
 * - the answer shows inline — folding a reply behind a tap would make reading it
 *   cost more here than on the surface that owns the same transcript;
 * - the decision card collapses to one line, and the buttons live in a BOTTOM
 *   SHEET: on a 390pt screen the tap that approves a tool call must not be
 *   reachable by accident. The line stays visible even when the block is folded,
 *   because a round parked on a question has to stay answerable.
 *
 * Three rules this component owns:
 * - "in progress" is positive evidence only — a `pending` projection (D9). An
 *   unreachable Runtime says "暂不可达" with a retry, never "still running";
 *   showing an outage as a live run lets a dead node rewrite history.
 * - the card a round is parked on lives HERE, in the block (D8). Answering it
 *   posts to the execution session's own write entry, where the owner check
 *   already runs; no chat row is written for the decision.
 * - a hole is stated as a hole (D5): inventing content would be writing history.
 */

import { memo, useState } from 'react'
import { sendAnswer, sendApproval } from '../lib/api'
import { blockRefOf, blockStatusText, isFailureStatus } from '../lib/chatBlocks'
import { renderMarkdown } from '../lib/markdown'
import { useChatBlock, useChatBlockStore } from '../stores/chatBlockStore'
import { useChatStore } from '../stores/chatStore'
import { Sheet } from './ui'
import type { BlockCard, ChatMessageRow } from '../lib/types'

interface ChatExecutionBlockProps {
  row: ChatMessageRow
  /** The Chat this row belongs to — the block read is participation-checked
   *  against it, so it is part of the address, not a filter (D10). */
  chatSessionId: string
  /** Drill into the execution session that owns this round. The block names the
   *  instance that hosts it, so the handler takes it — on this 1:1 surface that
   *  instance is the agent the screen is open on, but the row must not have to
   *  know that. */
  onJumpToExec: (agentInstanceId: string, execSid: string) => void
  /** Address the composer at this round (D6 level 1) — what a `pause` card's
   *  "continue" rides. `quote` is the question the human is answering. */
  onReplyToRound: (rowId: string, quote: string) => void
  /** A viewer who is not this round's answerer (`answerable_by`): the question
   *  shows, the buttons do not (D10's read/write split). */
  canAnswer?: boolean
}

export const ChatExecutionBlock = memo(function ChatExecutionBlock({
  row,
  chatSessionId,
  onJumpToExec,
  onReplyToRound,
  canAnswer = true,
}: ChatExecutionBlockProps) {
  const ref = blockRefOf(row.meta)
  const entry = useChatBlock(chatSessionId, ref)
  const reload = useChatBlockStore((s) => s.reload)
  const [open, setOpen] = useState(true)

  if (!ref || !entry) {
    // No coordinates, or the read has not landed: a line holding the block's
    // own place. A transcript that painted nothing here would let a missing run
    // look like a message nobody received.
    return <div className="chat-block-hole">{ref ? '正在加载本轮执行…' : '本轮执行已不可用'}</div>
  }

  if (entry.status === 'unreachable') {
    // The transcript's event line with a retry: the fact is "we could not read
    // this", which belongs to the transcript, not to a bubble.
    return (
      <div className="chat-block-hole">
        <span>执行所在的主机暂不可达</span>
        <button type="button" className="chat-block-hole-action" onClick={() => reload(chatSessionId, [ref])}>
          重试
        </button>
      </div>
    )
  }

  const projection = entry.projection
  if (entry.status === 'not_found' || !projection) {
    // The round names a session this chat cannot resolve. An explicit hole is
    // the honest answer (D5).
    return <div className="chat-block-hole">本轮执行已不可用</div>
  }

  const instanceId = entry.agent_instance_id
  const execSid = entry.agent_session_id
  // D9: `pending` is the ONLY positive evidence that a round is still running —
  // the Gateway answers it from a live read of a non-terminal projection. A
  // terminal block is `ok` and immutable, and an `unreachable` one is an outage,
  // which must never be drawn as a run in flight.
  const inProgress = entry.status === 'pending'
  const status = inProgress ? 'in_progress' : projection.status
  const answer = projection.answer?.body.trim() ?? ''
  const card = projection.card ?? null

  return (
    <div className="chat-block" data-testid="exec-block" data-ref={ref} data-chat={chatSessionId}>
      <button type="button" className="chat-block-head" onClick={() => setOpen((v) => !v)} aria-expanded={open}>
        <span
          className={`chat-block-glyph${inProgress ? ' is-live' : ''}${isFailureStatus(projection.status) ? ' is-danger' : ''}`}
          aria-hidden
        >
          {inProgress ? '◌' : isFailureStatus(projection.status) ? '!' : '⚙'}
        </span>
        <span className="chat-block-status">{blockStatusText(status)}</span>
        {row.meta?.task_id ? <span className="chat-block-task">{row.meta.task_id}</span> : null}
        {!open && answer ? <span className="chat-block-peek">{peek(answer)}</span> : null}
        <span className="chat-block-caret" aria-hidden>
          {open ? '⌄' : '›'}
        </span>
      </button>

      {open ? (
        <div className="chat-block-body">
          {answer ? (
            <div className="chat-block-answer">{renderMarkdown(answer)}</div>
          ) : inProgress ? (
            <div className="chat-block-working">处理中…</div>
          ) : null}
          {execSid ? (
            <button type="button" className="chat-block-jump" onClick={() => onJumpToExec(instanceId, execSid)}>
              查看执行
            </button>
          ) : null}
        </div>
      ) : null}

      {card && execSid ? (
        <BlockCardSurface
          card={card}
          canAnswer={canAnswer}
          instanceId={instanceId}
          execSid={execSid}
          rowId={row.meta?.message_id ?? ''}
          chatSessionId={chatSessionId}
          blockRef={ref}
          onReplyToRound={onReplyToRound}
        />
      ) : null}
    </div>
  )
})

/** One folded line of the answer's opening, so a collapsed block still says
 *  something — a card that only states its status would read as a run that
 *  produced nothing. */
function peek(answer: string): string {
  const collapsed = answer.replace(/\s+/g, ' ').trim()
  if (!collapsed) return ''
  return collapsed.length > 28 ? `${collapsed.slice(0, 28)}…` : collapsed
}

interface CardSurfaceProps {
  card: BlockCard
  canAnswer: boolean
  instanceId: string
  execSid: string
  /** The reference row's own id — the coordinate a continuation addresses (D6
   *  level 1). */
  rowId: string
  chatSessionId: string
  /** This block's ref, re-read once the answer lands so the answered card leaves
   *  the transcript on the decision, not on the next projection that happens to
   *  arrive. Named `blockRef` because `ref` is a reserved JSX attribute. */
  blockRef: string
  onReplyToRound: (rowId: string, quote: string) => void
}

/** The round's pending decision (D8), collapsed to one line with its buttons in
 *  a bottom sheet (§6.5).
 *
 *  A `pause` is not answered through a sheet: the turn already ended `waiting`,
 *  and the human's reply IS the next turn of this round (D6 level 1) — so the
 *  button addresses the composer instead. */
function BlockCardSurface({
  card,
  canAnswer,
  instanceId,
  execSid,
  rowId,
  chatSessionId,
  blockRef,
  onReplyToRound,
}: CardSurfaceProps) {
  const [sheet, setSheet] = useState(false)
  const [busy, setBusy] = useState(false)
  const [failed, setFailed] = useState(false)

  const post = async (run: () => Promise<void>) => {
    setBusy(true)
    setFailed(false)
    try {
      await run()
      // The execution view keeps its own copy of the pending request; clear it
      // only when it IS this request (see `dismissAnsweredCard`).
      useChatStore.getState().dismissAnsweredCard(instanceId, execSid, card.request_id)
      useChatBlockStore.getState().reload(chatSessionId, [blockRef])
      setSheet(false)
    } catch {
      // The card stays open: the round is still parked on the question, and a
      // silent failure would strand the human with a button that did nothing.
      setFailed(true)
    } finally {
      setBusy(false)
    }
  }

  if (card.kind === 'pause') {
    return canAnswer ? (
      <div className="chat-block-card">
        <div className="chat-block-card-static">
          <span className="chat-block-card-tag">步骤确认</span>
          <span className="chat-block-card-text">{truncate(card.prompt)}</span>
        </div>
        <button
          type="button"
          className="chat-block-continue"
          data-testid="block-reply"
          onClick={() => onReplyToRound(rowId, card.prompt ?? '')}
        >
          继续执行
        </button>
      </div>
    ) : (
      <div className="chat-block-card">
        <div className="chat-block-card-static">
          <span className="chat-block-card-tag">步骤确认</span>
          <span className="chat-block-card-text">{truncate(card.prompt)}</span>
        </div>
        <div className="chat-block-card-note">只有发起本轮的人可以回复</div>
      </div>
    )
  }

  const label = card.kind === 'approval' ? '需要审批' : 'Agent 询问'
  return (
    <div className="chat-block-card">
      {canAnswer ? (
        <button
          type="button"
          className="chat-block-card-btn"
          data-testid="block-card-open"
          onClick={() => setSheet(true)}
        >
          <span className="chat-block-card-tag">{label}</span>
          <span className="chat-block-card-text">{truncate(card.prompt)}</span>
          <span className="chat-block-card-go">去回复</span>
        </button>
      ) : (
        <>
          <div className="chat-block-card-static">
            <span className="chat-block-card-tag">{label}</span>
            <span className="chat-block-card-text">{truncate(card.prompt)}</span>
          </div>
          <div className="chat-block-card-note">只有发起本轮的人可以回复</div>
        </>
      )}
      {sheet ? (
        <CardSheet
          card={card}
          busy={busy}
          failed={failed}
          onClose={() => setSheet(false)}
          onApprove={(approved) => void post(() => sendApproval(instanceId, execSid, card.request_id, approved))}
          onAnswer={(answer) => void post(() => sendAnswer(instanceId, execSid, card.request_id, answer))}
        />
      ) : null}
    </div>
  )
}

/** The answer surface's bottom sheet (§6.5). Approval offers exactly two
 *  decisions; an `ask_question` offers its labels plus free text — the same
 *  content the desktop block's inline buttons carry, just not one tap from the
 *  thumb. */
function CardSheet({
  card,
  busy,
  failed,
  onClose,
  onApprove,
  onAnswer,
}: {
  card: BlockCard
  busy: boolean
  failed: boolean
  onClose: () => void
  onApprove: (approved: boolean) => void
  onAnswer: (answer: string) => void
}) {
  const [free, setFree] = useState('')
  const options = card.options ?? []
  return (
    <Sheet open onClose={onClose} title={card.kind === 'approval' ? '允许执行？' : '需要你的回答'}>
      {card.prompt ? <div className="chat-sheet-question">{renderMarkdown(card.prompt)}</div> : null}
      {card.kind === 'approval' ? (
        <div className="chat-sheet-row">
          <button type="button" className="chat-sheet-option is-primary" disabled={busy} onClick={() => onApprove(true)}>
            批准
          </button>
          <button type="button" className="chat-sheet-option" disabled={busy} onClick={() => onApprove(false)}>
            拒绝
          </button>
        </div>
      ) : (
        <>
          {options.map((opt) => (
            <button key={opt} type="button" className="chat-sheet-option" disabled={busy} onClick={() => onAnswer(opt)}>
              {opt}
            </button>
          ))}
          <div className="chat-sheet-free">
            <input
              className="chat-sheet-input"
              placeholder="自由回答"
              aria-label="自由回答"
              value={free}
              onChange={(e) => setFree(e.target.value)}
            />
            <button
              type="button"
              className="chat-sheet-send"
              disabled={busy || !free.trim()}
              onClick={() => {
                onAnswer(free.trim())
                setFree('')
              }}
            >
              发送
            </button>
          </div>
        </>
      )}
      {failed ? <div className="chat-sheet-failed">回复未能发送，请重试</div> : null}
    </Sheet>
  )
}

/** Bound a card's question to one line on the collapsed row — the sheet is where
 *  the whole question is read. */
function truncate(prompt?: string): string {
  const collapsed = (prompt ?? '').replace(/\s+/g, ' ').trim()
  return collapsed.length > 34 ? `${collapsed.slice(0, 34)}…` : collapsed
}
