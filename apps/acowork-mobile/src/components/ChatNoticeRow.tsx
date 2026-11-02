/**
 * ChatNoticeRow — the mobile renderer for `system_notice` chat rows, the twin of
 * Desktop's `ChatNoticeEventRow`.
 *
 * A notice is a fact about the channel, never an utterance, so it paints as a
 * quiet centered line and carries no avatar. Three things still write one
 * (ADR-106 D12, after the reference-row consolidation):
 * - `delivery_failed`: a send this Chat owed the human and could not make —
 *   bodyless on the wire, so the wording belongs to the surface (ADR-105).
 * - `budget_exhausted` / `truncated` / `unavailable`: a limit the round hit.
 * - `session_choice` / `confirm_continue`: an addressing question (D6 levels 3
 *   and 5), empty-bodied with `meta.options`.
 *
 * Everything the runtime used to write as progress (dispatch / round-open
 * milestones, the router-fallback receipt, the card-resolution receipt) is gone
 * from the stream: the log records conversation, and a decision card is no
 * longer a chat row at all (D8) — its answer is an execution-side fact shown
 * inside the round's block.
 *
 * Mobile delta (§6.5): an addressing question collapses to one line, and its
 * option buttons open a BOTTOM SHEET. On a 390pt screen a centered strip of
 * small buttons is where a mis-tap sends a turn into the wrong task.
 */

import { useState } from 'react'
import { Sheet } from './ui'
import type { CardChoice } from '../lib/chatCardAnswer'
import type { ChatCandidateOption, ChatMessageRow } from '../lib/types'

/** Milestones the runtime stopped writing, kept invisible even when an old
 *  transcript still carries them — history says nothing about "queued" or "a
 *  round opened", and a retired card receipt must not resurrect a card that is
 *  now shown inside the block (D12). */
const RETIRED_NOTICES = new Set([
  'turn_dispatched',
  'turn_started',
  'turn_routed',
  'router_fallback',
  'interaction_resolved',
])

interface ChatNoticeRowProps {
  row: ChatMessageRow
  /** Answering an addressing card. Absent means the row is not answerable here:
   *  a read-only viewer never gets the action (ADR-106 D10 — only the person who
   *  spoke may decide where their words go). The choice becomes a re-delivery of
   *  the human's own utterance (see `chatCardAnswer`, D6). */
  onChoose?: (choice: CardChoice) => void
  /** The question already has a round: the buttons go quiet rather than
   *  disappearing, because a card that vanished on click would erase the record
   *  of what was decided — and a re-click would deliver the words twice. */
  answered?: boolean
}

export function ChatNoticeRow({ row, onChoose, answered }: ChatNoticeRowProps) {
  const [sheet, setSheet] = useState(false)
  const noticeType = row.meta?.notice_type ?? ''

  if (RETIRED_NOTICES.has(noticeType)) return null

  if (noticeType === 'session_choice' || noticeType === 'confirm_continue') {
    const confirm = noticeType === 'confirm_continue'
    const question = confirm ? '继续这个任务吗？' : '你指的是哪一个任务？'
    const options = row.meta?.options ?? []
    // Nothing to offer, or nobody allowed to offer it: the question is still a
    // fact about the transcript, so it stays as the plain line.
    if (!onChoose || options.length === 0) {
      return (
        <div className="chat-notice">
          <span className="chat-notice-pill">{question}</span>
        </div>
      )
    }
    return (
      <div className="chat-notice-card" data-testid="chat-card-options">
        <button
          type="button"
          className="chat-notice-card-btn"
          disabled={answered}
          onClick={() => setSheet(true)}
        >
          <span className="chat-notice-card-question">{question}</span>
          <span className="chat-notice-card-go">{answered ? '已回复' : '去选择'}</span>
        </button>
        {sheet ? (
          <ChooseSessionSheet
            question={question}
            confirm={confirm}
            options={options}
            allowNewTask={row.meta?.allow_new_task === true}
            onClose={() => setSheet(false)}
            onChoose={(choice) => {
              setSheet(false)
              onChoose(choice)
            }}
          />
        ) : null}
      </div>
    )
  }

  if (noticeType === 'delivery_failed') {
    return (
      <div className="chat-notice">
        <span className="chat-notice-pill">消息未能送达，请重试</span>
      </div>
    )
  }

  const text = row.body || row.meta?.status || ''
  if (!text) return null
  return (
    <div className="chat-notice">
      <span className="chat-notice-pill">{text}</span>
    </div>
  )
}

/** The sheet's one option row: a candidate session, or D6's always-offered
 *  escape. "开新任务" is put FIRST — a question must be answerable in one tap,
 *  and burying the escape below the suggestions would be pressing the human back
 *  into the machine's guess. */
function ChooseSessionSheet({
  question,
  confirm,
  options,
  allowNewTask,
  onClose,
  onChoose,
}: {
  question: string
  confirm: boolean
  options: ChatCandidateOption[]
  allowNewTask: boolean
  onClose: () => void
  onChoose: (choice: CardChoice) => void
}) {
  return (
    <Sheet open onClose={onClose} title={question}>
      {allowNewTask ? (
        <button type="button" className="chat-sheet-option is-primary" onClick={() => onChoose({ forceNew: true })}>
          开新任务
        </button>
      ) : null}
      {(options ?? []).map((option) => (
        <button
          key={option.session_id}
          type="button"
          className="chat-sheet-option"
          title={option.why || option.label || option.session_id}
          onClick={() => onChoose({ sessionId: option.session_id })}
        >
          {confirm ? '就继续这个' : option.label || option.session_id.slice(0, 8)}
        </button>
      ))}
    </Sheet>
  )
}
