/**
 * ChatAddressSheet — the second level of the `@` on mobile (ADR-106 D7), the
 * twin of Desktop's `ChatAddressPicker`.
 *
 * The first level answers "which agent"; this one answers "which of its runs".
 * It is deliberately not a session browser: the rows come from `GET …/sessions`
 * narrowed by `chat_session_id` / `surface`, where the narrowing IS the Runtime's
 * candidate rule for this chat. A picker built off any other list would offer a
 * session the turn could not legally be delivered to — and the app's other list
 * is the execution session index, which is exactly the mistake this rule exists
 * to prevent. No row cap and no freshness window: the paging IS the window, so
 * the human taps back as far as they want instead of wondering why a session
 * they remember is missing.
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

import { useEffect, useRef, useState } from 'react'
import { fetchChatSessions } from '../lib/api'
import { formatListTime } from '../lib/time'
import { Sheet } from './ui'
import type { SessionInfo } from '../lib/types'

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
  /** Part of the address, not a filter: the listing is narrowed per chat. */
  chatSessionId: string
  onPick: (pick: AddressPick) => void
  onClose: () => void
}

export function ChatAddressSheet({ instanceId, chatSessionId, onPick, onClose }: ChatAddressSheetProps) {
  const [rows, setRows] = useState<SessionInfo[] | null>(null)
  const [hasMore, setHasMore] = useState(false)
  const [failed, setFailed] = useState(false)
  const [attempt, setAttempt] = useState(0)
  const [loadingMore, setLoadingMore] = useState(false)
  // One generation counter for both the reload and the append: a "加载更多"
  // that answers after the sheet was reloaded (retry, chat switch) belongs to a
  // dead request and must not repaint what is on screen.
  const generation = useRef(0)
  const page = useRef(1)

  useEffect(() => {
    const gen = ++generation.current
    setFailed(false)
    setRows(null)
    setHasMore(false)
    page.current = 1
    fetchChatSessions(instanceId, chatSessionId, 'direct', 1)
      .then((res) => {
        if (gen !== generation.current) return
        setRows(res.items)
        setHasMore(res.hasMore)
      })
      .catch(() => {
        if (gen === generation.current) setFailed(true)
      })
  }, [instanceId, chatSessionId, attempt])

  const loadMore = () => {
    const gen = generation.current
    setLoadingMore(true)
    fetchChatSessions(instanceId, chatSessionId, 'direct', page.current + 1)
      .then((res) => {
        if (gen !== generation.current) return
        page.current += 1
        setLoadingMore(false)
        setHasMore(res.hasMore)
        // A session created between the two reads can appear on both pages;
        // a duplicate row would be a second key AND a second tap target for the
        // same run.
        setRows((prev) => {
          const seen = new Set((prev ?? []).map((s) => s.session_id))
          return [...(prev ?? []), ...res.items.filter((s) => !seen.has(s.session_id))]
        })
      })
      .catch(() => {
        if (gen === generation.current) setLoadingMore(false)
      })
  }

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

      {!failed && rows === null ? <div className="chat-sheet-hint">载入中…</div> : null}

      {failed || rows === null
        ? null
        : rows.map((s) => (
            <button
              key={s.session_id}
              type="button"
              className="chat-sheet-option"
              data-testid="address-session"
              title={s.session_id}
              onClick={() => onPick({ sessionId: s.session_id, label: s.title })}
            >
              <span className="chat-sheet-option-label">{s.title}</span>
              {/* The listing carries no run status (that arrives over the live
                  push, which a closed sheet is not subscribed to), so the row
                  says when the work last moved — same slot, honest content. */}
              <span className="chat-sheet-option-value">{formatListTime(s.updated_at)}</span>
            </button>
          ))}

      {rows && rows.length > 0 ? (
        <button
          type="button"
          className="chat-sheet-option is-quiet"
          data-testid="address-none"
          onClick={() => onPick({})}
        >
          不指定 — 由 agent 决定
        </button>
      ) : null}

      {hasMore ? (
        <button
          type="button"
          className="chat-sheet-option"
          data-testid="address-more"
          disabled={loadingMore}
          onClick={loadMore}
        >
          {loadingMore ? '载入中…' : '更早的任务'}
        </button>
      ) : null}

      {rows && rows.length === 0 && !failed ? (
        <div className="chat-sheet-hint">这里还没有可继续的任务</div>
      ) : null}
    </Sheet>
  )
}
