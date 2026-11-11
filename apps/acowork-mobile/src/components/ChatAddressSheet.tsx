/**
 * ChatAddressSheet — the second level of the `@` on mobile (ADR-106 D7, redrawn
 * by the 2026-11-04 revision), the twin of Desktop's `ChatAddressPicker`.
 *
 * It used to answer "which run should this message be DELIVERED to". That is
 * what `target_sessions` was, and the revision retired the human `@` path off it:
 * a typed `@session` is now MATERIAL, not a delivery. So the sheet attaches one
 * to five of the agent's runs as quoted context; the round is NOT guessed here —
 * the Gateway pins it at send time — and the message still rides the ladder's
 * lower levels (the Router may answer it, or open new work).
 *
 * The list is deliberately not a session browser: the rows come from
 * `GET …/sessions` narrowed by `chat_session_id` / `surface`, where the narrowing
 * IS the Runtime's candidate rule for this chat. No row cap and no freshness
 * window: the paging IS the window.
 *
 * Rules the list keeps:
 * - **"开新任务" is always the first row.** D6's cheap escape stays one tap.
 * - **a run is TOGGLED, and the sheet stays open** so several can be picked —
 *   one click is not yet the answer; "完成" commits the selection.
 * - **"去续投" is a navigation, never an address.** It asks the host to locate
 *   that run's block and focus its in-place reply box; it closes nothing to a
 *   delivery and changes what a message carries not at all.
 * - **picking nothing is still an answer.** "不指定" attaches no material, which
 *   leaves the message on the ladder's lower levels.
 *
 * Presentation: a bottom sheet (§6.5), because a phone has no hover.
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import { fetchChatSessions } from '../lib/api'
import { formatListTime } from '../lib/time'
import { Sheet } from './ui'
import type { ChatRefPick } from '../stores/chatSessionStore'
import type { SessionInfo } from '../lib/types'

/** The ceiling the Gateway's `validate_refs` answers with a 400 — mirrored here
 *  so the human is stopped at the fifth pick instead of learning the limit from
 *  a rejected send. */
export const MAX_PICK_REFS = 5

/** What the human decided. `{}` = attach nothing (the ladder continues). */
export interface AddressPick {
  /** The runs attached as material. Fresh picks carry only the coordinate the
   *  Gateway needs to pin the round (`{agentInstanceId, sessionId}`). */
  refs?: ChatRefPick[]
  forceNew?: boolean
  /** Display name per `agent|session` key, as shown in the list — the composer's
   *  chips show the human's own words, not an id. */
  labels?: Record<string, string>
}

function refKey(agentInstanceId: string, sessionId: string): string {
  return `${agentInstanceId}|${sessionId}`
}

interface ChatAddressSheetProps {
  /** Whose runs these are. */
  instanceId: string
  /** Part of the address, not a filter: the listing is narrowed per chat. */
  chatSessionId: string
  /** Which surface asked. The Runtime's permission rule differs: a 1:1 chat
   *  lists this chat's runs plus the human's own hand-started ones; a group
   *  lists what the group may name (ADR-106 D7). The narrowing runs
   *  server-side, so the client states where it is asking from. */
  surface?: 'direct' | 'group'
  onPick: (pick: AddressPick) => void
  /** "去续投": ask the host to locate this run's block and focus its input. A
   *  navigation, so it is separate from the reference commit. */
  onLocate?: (sessionId: string, label?: string) => void
  onClose: () => void
}

export function ChatAddressSheet({ instanceId, chatSessionId, surface = 'direct', onPick, onLocate, onClose }: ChatAddressSheetProps) {
  const [rows, setRows] = useState<SessionInfo[] | null>(null)
  const [hasMore, setHasMore] = useState(false)
  const [failed, setFailed] = useState(false)
  const [attempt, setAttempt] = useState(0)
  const [loadingMore, setLoadingMore] = useState(false)
  // The live reference selection, keyed `agent|session` → the row's label. A
  // reference keeps the sheet open across picks, so the toggles live here until
  // "完成" commits them — a commit on every tap would close the list the human
  // is still reading.
  const [picked, setPicked] = useState<Record<string, string>>({})
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
    fetchChatSessions(instanceId, chatSessionId, surface, 1)
      .then((res) => {
        if (gen !== generation.current) return
        setRows(res.items)
        setHasMore(res.hasMore)
      })
      .catch(() => {
        if (gen === generation.current) setFailed(true)
      })
  }, [instanceId, chatSessionId, surface, attempt])

  const loadMore = () => {
    const gen = generation.current
    setLoadingMore(true)
    fetchChatSessions(instanceId, chatSessionId, surface, page.current + 1)
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

  const pickedCount = Object.keys(picked).length

  const toggleRef = useCallback(
    (session: SessionInfo) => {
      const key = refKey(instanceId, session.session_id)
      setPicked((prev) => {
        if (prev[key]) {
          const next = { ...prev }
          delete next[key]
          return next
        }
        if (Object.keys(prev).length >= MAX_PICK_REFS) return prev
        return { ...prev, [key]: session.title ?? '' }
      })
    },
    [instanceId],
  )

  // Commit the current reference selection and close. An empty selection means
  // "nothing attached" — the same answer as "不指定" / Escape.
  const commit = useCallback(() => {
    const entries = Object.entries(picked)
    if (entries.length === 0) {
      onPick({})
      return
    }
    const refs: ChatRefPick[] = entries.map(([key]) => {
      const sep = key.indexOf('|')
      return { agentInstanceId: key.slice(0, sep), sessionId: key.slice(sep + 1) }
    })
    const labels: Record<string, string> = {}
    for (const [key, label] of entries) if (label) labels[key] = label
    onPick({ refs, labels })
  }, [picked, onPick])

  return (
    <Sheet open onClose={onClose} title="引用哪些任务作为材料？">
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
        : rows.map((s) => {
            const key = refKey(instanceId, s.session_id)
            const checked = !!picked[key]
            // A full selection disables unchecked reference rows (the fifth is
            // the Gateway's ceiling); a checked one stays tappable to un-pick.
            const refDisabled = !checked && pickedCount >= MAX_PICK_REFS
            return (
              <div key={s.session_id} className="chat-sheet-ref-row">
                <button
                  type="button"
                  className={
                    'chat-sheet-option chat-sheet-ref' + (checked ? ' is-checked' : '')
                  }
                  data-testid="address-session"
                  title={s.session_id}
                  aria-pressed={checked}
                  disabled={refDisabled}
                  onClick={() => toggleRef(s)}
                >
                  <span className="chat-sheet-option-label">{s.title}</span>
                  {/* The listing carries no run status (that arrives over the live
                      push, which a closed sheet is not subscribed to), so the row
                      says when the work last moved — same slot, honest content. */}
                  <span className="chat-sheet-option-value">{formatListTime(s.updated_at)}</span>
                  <span className="chat-sheet-ref-mark">{checked ? '已引用' : '引用'}</span>
                </button>
                <button
                  type="button"
                  className="chat-sheet-option chat-sheet-continue"
                  data-testid="address-continue"
                  onClick={() => onLocate?.(s.session_id, s.title)}
                >
                  去续投
                </button>
              </div>
            )
          })}

      {rows && rows.length > 0 ? (
        <button
          type="button"
          className="chat-sheet-option is-quiet"
          data-testid="address-none"
          onClick={() => onPick({})}
        >
          不指定 — 不引用任何材料
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
        <div className="chat-sheet-hint">这里还没有可引用的任务</div>
      ) : null}

      {rows && rows.length > 0 ? (
        <button
          type="button"
          className="chat-sheet-option is-primary chat-sheet-done"
          data-testid="address-done"
          disabled={pickedCount === 0}
          onClick={commit}
        >
          完成（{pickedCount}/{MAX_PICK_REFS}）
        </button>
      ) : null}
    </Sheet>
  )
}
