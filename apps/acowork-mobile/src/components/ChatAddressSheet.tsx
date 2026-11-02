/**
 * ChatAddressSheet — the second level of the `@` on mobile (ADR-106 D7), the
 * twin of Desktop's `ChatAddressPicker`.
 *
 * The first level answers "which agent"; this one answers "which of its runs".
 * It is deliberately not a session browser: the rows come from the Runtime's
 * candidate filter (`GET …/chat-candidates`), which IS the permission for what
 * this human may address in THIS chat. A picker built off any other list would
 * offer a session the turn could not legally be delivered to — and on mobile the
 * only other list the app holds is the execution session index, which is exactly
 * the mistake this rule exists to prevent.
 *
 * Two rules the list keeps:
 * - **"开新任务" is always the first row.** D6's cheap escape must be answerable
 *   in one tap; burying it below the suggestions presses the human back into the
 *   machine's guess.
 * - **picking nothing is still an answer.** "不指定" means "no session named",
 *   which leaves the message on the ladder's lower levels — where the Router may
 *   answer it as chat instead of routing it. An address is an instruction, so the
 *   absence of one has to stay reachable.
 *
 * Presentation: a bottom sheet (§6.5) rather than a popup anchored to the
 * composer, because a phone has no hover and no room for a floating list.
 */

import { useEffect, useState } from 'react'
import { fetchChatCandidates } from '../lib/api'
import { candidateStatusText } from '../lib/chatBlocks'
import { Sheet } from './ui'
import type { ChatCandidate } from '../lib/types'

/** What the human decided. `{}` = no address at all (the ladder continues). */
export interface AddressPick {
  sessionId?: string
  forceNew?: boolean
  /** Display name of the named session — the chip that records the address on
   *  the composer needs the human's own words, not an id. */
  label?: string
}

interface ChatAddressSheetProps {
  /** Whose runs these are. */
  instanceId: string
  /** Part of the address, not a filter: the candidate set is per chat. */
  chatSessionId: string
  onPick: (pick: AddressPick) => void
  onClose: () => void
}

export function ChatAddressSheet({ instanceId, chatSessionId, onPick, onClose }: ChatAddressSheetProps) {
  const [candidates, setCandidates] = useState<ChatCandidate[] | null>(null)
  const [failed, setFailed] = useState(false)
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    let alive = true
    setFailed(false)
    setCandidates(null)
    fetchChatCandidates(instanceId, chatSessionId, 'direct')
      .then((rows) => {
        if (alive) setCandidates(rows)
      })
      .catch(() => {
        if (alive) setFailed(true)
      })
    return () => {
      alive = false
    }
  }, [instanceId, chatSessionId, attempt])

  return (
    <Sheet open onClose={onClose} title="这条消息投给哪个任务？">
      <button
        type="button"
        className="chat-sheet-option is-primary"
        data-testid="address-new"
        onClick={() => onPick({ forceNew: true })}
      >
        开新任务
      </button>

      {failed ? (
        <button type="button" className="chat-sheet-option" onClick={() => setAttempt((a) => a + 1)}>
          读不到本会话的任务列表 · 重试
        </button>
      ) : null}

      {!failed && candidates === null ? <div className="chat-sheet-hint">载入中…</div> : null}

      {failed || candidates === null
        ? null
        : candidates.map((c) => (
            <button
              key={c.session_id}
              type="button"
              className="chat-sheet-option"
              data-testid="address-session"
              title={c.session_id}
              onClick={() => onPick({ sessionId: c.session_id, label: c.label })}
            >
              <span className="chat-sheet-option-label">{c.label}</span>
              <span className="chat-sheet-option-value">{candidateStatusText(c.status)}</span>
            </button>
          ))}

      {candidates && candidates.length > 0 ? (
        <button
          type="button"
          className="chat-sheet-option is-quiet"
          data-testid="address-none"
          onClick={() => onPick({})}
        >
          不指定 — 由 agent 决定
        </button>
      ) : null}

      {candidates && candidates.length === 0 && !failed ? (
        <div className="chat-sheet-hint">这里还没有可继续的任务</div>
      ) : null}
    </Sheet>
  )
}
