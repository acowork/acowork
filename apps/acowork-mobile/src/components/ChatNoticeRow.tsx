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
import { RETIRED_NOTICES } from '../lib/retiredNotices'

interface ChatNoticeRowProps {
  row: ChatMessageRow
  /** Answering an addressing card. Absent means the row is not answerable here:
   *  a read-only viewer never gets the action (ADR-106 D10 — only the person who
   *  spoke may decide where their words go). The choice becomes a re-delivery of
   *  the human's own utterance (see `chatCardAnswer`, D6). */
  onChoose?: (choice: CardChoice) => void
}

export function ChatNoticeRow({ row, onChoose }: ChatNoticeRowProps) {
  const [sheet, setSheet] = useState(false)
  const noticeType = row.meta?.notice_type ?? ''

  if (RETIRED_NOTICES.has(noticeType)) return null

  if (noticeType === 'session_choice' || noticeType === 'confirm_continue') {
    const confirm = noticeType === 'confirm_continue'
    const question = confirm ? '继续这个任务吗？' : '你指的是哪一个任务？'
    // The service's projection (design 31 §6): the options the question offers,
    // whether a round already answered it, and which session took the answer.
    // `meta.options` is no longer read by any surface — a card whose payload
    // never reached the client rendered as a sentence with no buttons, and
    // nothing could tell that it was a bug.
    const card = row.delivery?.question
    const options = card?.options ?? []
    const answered = card?.answered === true
    // A card with nothing to offer is retired chrome, not a question: the rows
    // that reach a surface like this are pre-projection leftovers, and a pill
    // asking "继续这个任务吗？" with nothing to tap is noise in the transcript.
    if (options.length === 0) return null

    const labelOf = (option: ChatCandidateOption) =>
      option.label?.trim() || option.session_id.slice(0, 8)

    // Settled: paint nothing (ADR-106 D14). A card is a question; once a round
    // answers it, the question is an intermediate state of that turn and the
    // round's block already says which session took the words.
    //
    // `!answerable` leaves by the same door: the card names a row that is not
    // in the conversation, so no tap can land (`origin_gone`/422) and it is not
    // a live question at all — rows an older build wrote, dropped at render
    // time rather than listed as dead options.
    if (answered || card?.answerable === false) return null

    // `confirm_continue` proposes exactly ONE session, so the collapsed row has
    // to name it: "继续这个任务吗？" over an anonymous "去选择" asked the human to
    // continue a task they could not identify without opening the sheet. The
    // multi-candidate question keeps its names in the sheet, where the picks are.
    const only = options.length === 1 ? options[0] : undefined
    const proposed = confirm && only ? labelOf(only) : ''
    return (
      <div className="chat-notice-card" data-testid="chat-card-options">
        <div className="chat-notice-card-main">
          <span className="chat-notice-card-question">{question}</span>
          {proposed ? <span className="chat-notice-card-proposal">{proposed}</span> : null}
          {!onChoose ? <span className="chat-notice-card-hint">只有发话的人可以回答</span> : null}
        </div>
        {onChoose ? (
          <button
            type="button"
            className="chat-notice-card-go"
            onClick={() => setSheet(true)}
          >
            {options.length === 1 ? '确认' : '选择'}
            <span aria-hidden="true">›</span>
          </button>
        ) : null}
        {sheet && onChoose ? (
          <ChooseSessionSheet
            question={question}
            options={options}
            allowNewTask={card?.allow_new_task === true}
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
  options,
  allowNewTask,
  onClose,
  onChoose,
}: {
  question: string
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
      {(options ?? []).map((option) => {
        // An answer must NAME what it continues. `confirm` used to paint a fixed
        // "就继续这个", so the sheet presented one anonymous task and the human
        // had to pick it blind — the payload carries the candidate's own title
        // (and, for `confirm_continue`, the router's reason); this is where they
        // belong. The fixed wording survives only as the no-label fallback.
        const label = option.label?.trim() || option.session_id.slice(0, 8)
        const why = option.why?.trim()
        return (
          <button
            key={option.session_id}
            type="button"
            className="chat-sheet-option"
            title={why ? `${label} — ${why}` : label}
            onClick={() => onChoose({ sessionId: option.session_id })}
          >
            <span className="chat-sheet-option-body">
              <span className="chat-sheet-option-label">
                <span className={`chat-sheet-option-dot is-${option.status ?? 'unknown'}`} aria-hidden="true" />
                {label}
              </span>
              {why ? <span className="chat-sheet-option-why">{why}</span> : null}
            </span>
            <span className="chat-sheet-option-go" aria-hidden="true">
              ›
            </span>
          </button>
        )
      })}
    </Sheet>
  )
}
