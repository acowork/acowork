/**
 * Agent Chat session — the PRIMARY agent surface on mobile (design 27 §6.1,
 * plan Wave 5.5). Same view model as Desktop's `AgentChatView`: durable
 * chat rows only (user_message / agent_message / interaction_request /
 * system_notice). The differences are presentation-only, per §6.5:
 *
 *  - the in-flight turn's status card is a ONE-LINE summary; tapping it
 *    expands into the execution session view (the existing ChatDetailScreen
 *    — the read path enforces visibility, hidden 404 shows its own error);
 *  - a decision card collapses to one line; answering opens a BOTTOM SHEET
 *    (mis-tap safety on a 390pt screen), and every answer goes back through
 *    the same chat-message channel with `in_reply_to` (§5.5.4).
 *
 * Deliberately NOT here: agent thinking, tool calls, execution process —
 * those live behind the status card, not in the chat stream (§6).
 *
 * Freshness: the `chat/changed` doorbell (content-free) fans out into
 * `wakeAgentChat()` → HTTP re-read; plus an immediate refresh on mount and
 * on foreground return. No streaming, ever, in v1 (design doc 25 §8).
 */

import { Fragment, useEffect, useMemo, useState, type ReactNode } from 'react'
import { Send } from 'lucide-react'
import { useNavStore } from '../../stores/navStore'
import { useAuthStore } from '../../stores/authStore'
import { useAgentStore } from '../../stores/agentStore'
import { useChatStore } from '../../stores/chatStore'
import { useChatSessionStore } from '../../stores/chatSessionStore'
import { AuthImage } from '../../components/AuthImage'
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from '../../lib/builtinIcons'
import { daySeparator, timeGapSeparator } from '../../lib/time'
import { useStickToBottom } from '../../lib/stickToBottom'
import { Banner, Sheet } from '../../components/ui'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import { renderMarkdown } from '../../lib/markdown'
import type { ChatMessageRow } from '../../lib/types'

function pickIconId(seed: string): string | null {
  if (BUILTIN_ICON_IDS.length === 0 || !seed) return null
  let h = 0
  for (let i = 0; i < seed.length; i++) h = (seed.charCodeAt(i) + (h << 5) - h) | 0
  return BUILTIN_ICON_IDS[Math.abs(h) % BUILTIN_ICON_IDS.length] ?? null
}

/**
 * COPIED VERBATIM from the desktop store's `closedCardIds` — the two apps
 * must project the same state from the same rows (§5.5.4, plan 5.5 drift
 * guard).
 *
 * A card is no longer answerable when a later row answers it (a human reply
 * or the Runtime's `interaction_resolved` notice) OR when the turn that
 * produced it already wrote its terminal `agent_message`: the run is over,
 * so the request behind the card is dead even if nobody stamped a closure on
 * it — which is what heals the pre-closure transcripts instead of leaving an
 * idle status card with a button that can never work.
 *
 * `status: "waiting"` (a turn that ended *because* a card stayed open) does
 * not close anything, and a terminal row only speaks for the card of its own
 * execution session — a group fan-out answers one trigger from several
 * agents. Missing session ids fall back to trigger-only matching.
 */
export function closedCardIds(rows: ChatMessageRow[]): Set<string> {
  const closed = new Set(rows.map((r) => r.meta?.in_reply_to).filter((s): s is string => !!s))
  const terminals = rows
    .map((r, i) => ({ r, i }))
    .filter(
      ({ r }) =>
        r.kind === 'agent_message' &&
        !!r.meta?.in_reply_to &&
        (r.meta?.status ?? 'completed') !== 'waiting',
    )
  rows.forEach((card, i) => {
    if (card.kind !== 'interaction_request') return
    const mid = card.meta?.message_id
    if (!mid || closed.has(mid)) return
    const sid = card.meta?.agent_session_id
    const finished = terminals.some(
      ({ r, i: t }) =>
        t > i &&
        r.meta?.in_reply_to === card.meta?.in_reply_to &&
        (!sid || !r.meta?.sender_session_id || r.meta.sender_session_id === sid),
    )
    if (finished) closed.add(mid)
  })
  return closed
}

/** Cards a human actually answered — narrows `closed` for the wording. */
export function humanAnsweredCardIds(rows: ChatMessageRow[]): Set<string> {
  return new Set(
    rows
      .filter((r) => r.kind === 'user_message')
      .map((r) => r.meta?.in_reply_to)
      .filter((s): s is string => !!s),
  )
}

export function AgentChatScreen() {
  const pop = useNavStore((s) => s.pop)
  const push = useNavStore((s) => s.push)
  const agentId = useChatStore((s) => s.selectedAgentId)
  const agent = useAgentStore((s) => (agentId ? s.agents[agentId]?.info : undefined))
  const me = useAuthStore((s) => s.me)
  const baseUrl = useAuthStore((s) => s.baseUrl)

  const st = useChatSessionStore((s) => (agentId ? s.byAgent[agentId] : undefined))
  const sessionId = st?.sessionId ?? null
  const win = useChatSessionStore((s) => (sessionId ? s.windows[sessionId] : undefined))
  const inFlight = useChatSessionStore((s) => (sessionId ? s.inFlight[sessionId] ?? null : null))
  const sending = useChatSessionStore((s) => (sessionId ? s.sending[sessionId] ?? false : false))

  const [draft, setDraft] = useState('')
  /** The decision card currently open in the bottom sheet (§6.5). */
  const [answering, setAnswering] = useState<ChatMessageRow | null>(null)

  useEffect(() => {
    if (agentId) void useChatSessionStore.getState().openAgent(agentId)
  }, [agentId])

  const rows = useMemo(() => (win ? [...win.older, ...win.tail] : []), [win])
  const closed = useMemo(() => closedCardIds(rows), [rows])
  const answeredHuman = useMemo(() => humanAnsweredCardIds(rows), [rows])

  const { ref: scrollRef } = useStickToBottom([rows.length, sessionId], sessionId, () => {
    if (sessionId) void useChatSessionStore.getState().loadOlder(sessionId)
  })

  const send = (text: string, inReplyTo?: string) => {
    const trimmed = text.trim()
    if (!trimmed || sending || !sessionId) return
    setDraft('')
    setAnswering(null)
    void useChatSessionStore.getState().sendText(sessionId, trimmed, inReplyTo)
  }

  /** §6.5: the status card expands INTO the execution session view — the
   *  existing screen, reached the same way the inbox reached it before the
   *  primary surface moved to the Chat session. */
  const openExec = (execSid: string) => {
    if (!agentId) return
    void useChatStore.getState().openSession(agentId, execSid).then(() => push('chat/detail'))
  }

  /** History entry: the old inbox tap path (pick the newest execution
   *  session), kept reachable from the navbar while multi-session UX for
   *  chat sessions is still desktop-only (§6.1). */
  const openHistory = async () => {
    if (!agentId) return
    await useAgentStore.getState().refreshSessions(agentId)
    const list = useAgentStore.getState().agents[agentId]?.sessions ?? []
    const newest = list.slice().sort((x, y) => (y.updated_at ?? 0) - (x.updated_at ?? 0))[0]
    if (newest) await useChatStore.getState().openSession(agentId, newest.session_id)
    push('chat/detail')
  }

  const agentAvatar: ReactNode = (() => {
    if (!agent) return <span>{'?'}</span>
    const picked = pickIconId(agent.name)
    const fallbackSrc = agent.icon && BUILTIN_ICONS[agent.icon] ? BUILTIN_ICONS[agent.icon] : BUILTIN_ICONS[picked ?? '']
    const fallbackEl = fallbackSrc ? <img src={fallbackSrc} alt="" /> : <span>{agent.name.charAt(0)}</span>
    if (agent.avatar && agentId) {
      const url = `${baseUrl}/api/agents/${encodeURIComponent(agentId)}/avatar-file?path=${encodeURIComponent(agent.avatar)}`
      return <AuthImage src={url} fallback={fallbackEl} />
    }
    return fallbackEl
  })()
  const myName = me?.display_name || me?.username || '我'
  const meAvatar: ReactNode = (() => {
    if (!me) return <span>{'?'}</span>
    const picked = pickIconId(myName)
    const fallbackSrc = me.builtin_avatar && BUILTIN_ICONS[me.builtin_avatar] ? BUILTIN_ICONS[me.builtin_avatar] : BUILTIN_ICONS[picked ?? '']
    const fallbackEl = fallbackSrc ? <img src={fallbackSrc} alt="" /> : <span>{myName.charAt(0)}</span>
    if (me.avatar) {
      const url = `${baseUrl}/api/users/${encodeURIComponent(me.user_id)}/avatar-file?path=${encodeURIComponent(me.avatar)}`
      return <AuthImage src={url} fallback={fallbackEl} />
    }
    return fallbackEl
  })()

  if (!agentId) {
    return (
      <EdgeSwipe onBack={() => pop()}>
        <div className="screen">
          <header className="navbar">
            <button type="button" className="navbar-back" onClick={() => pop()} aria-label="返回">返回</button>
            <span className="navbar-title">会话</span>
            <span className="navbar-trail" />
          </header>
          <div className="empty-state">未选择 Agent，请返回重试</div>
        </div>
      </EdgeSwipe>
    )
  }

  const failed = st?.resolved && !sessionId

  return (
    <EdgeSwipe onBack={() => pop()}>
      <div className="screen">
        <header className="navbar">
          <button type="button" className="navbar-back" onClick={() => pop()} aria-label="返回">返回</button>
          <span className="navbar-title">{agent?.name ?? agentId}</span>
          <span className="navbar-trail">
            <button type="button" className="navbar-action" onClick={() => void openHistory()} aria-label="历史会话">
              历史
            </button>
          </span>
        </header>

        {failed ? <Banner tone="error">无法打开与该 Agent 的会话，请返回重试</Banner> : null}

        <div className="scroll scroll-chat" ref={scrollRef}>
          {win?.loadingOlder ? <div className="time-sep">载入更早消息…</div> : null}
          {!st?.resolved ? <div className="empty-state">载入中…</div> : null}
          {st?.resolved && !failed && rows.length === 0 && !inFlight ? (
            <div className="empty-state">还没有消息，发一条开始对话</div>
          ) : null}
          {rows.map((r, i) => {
            const prev = i > 0 ? rows[i - 1]! : null
            const sep = prev ? daySeparator(prev.ts, r.ts) ?? timeGapSeparator(prev.ts, r.ts) : null
            return (
              <Fragment key={`${r.seq}-${r.meta?.message_id ?? ''}`}>
                {sep ? <div className="time-sep">{sep}</div> : null}
                <RowView
                  row={r}
                  isSelf={r.from === me?.user_id}
                  selfAvatar={meAvatar}
                  selfName={myName}
                  agentAvatar={agentAvatar}
                  agentName={agent?.name ?? 'Agent'}
                  closed={!!(r.meta?.message_id && closed.has(r.meta.message_id))}
                  answered={!!(r.meta?.message_id && answeredHuman.has(r.meta.message_id))}
                  inFlightTrigger={inFlight?.triggerId ?? null}
                  onOpenExec={openExec}
                  onAnswerCard={(card) => setAnswering(card)}
                />
              </Fragment>
            )
          })}
        </div>

        <div className="composer">
          <textarea
            className="composer-input"
            rows={1}
            placeholder="发送消息"
            aria-label="消息输入框"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
          />
          <button
            type="button"
            className="composer-send"
            aria-label="发送"
            disabled={!draft.trim() || sending || !sessionId}
            onClick={() => send(draft)}
          >
            <Send className="composer-send-icon" aria-hidden />
          </button>
        </div>

        {answering ? (
          <AnswerSheet
            card={answering}
            closed={!!(answering.meta?.message_id && closed.has(answering.meta.message_id))}
            answered={!!(answering.meta?.message_id && answeredHuman.has(answering.meta.message_id))}
            onClose={() => setAnswering(null)}
            onAnswer={(text) => send(text, answering.meta?.message_id)}
          />
        ) : null}
      </div>
    </EdgeSwipe>
  )
}

/* ------------------------------------------------------------------ */
/* Row rendering — the four durable kinds (design 27 §4.2)              */
/* ------------------------------------------------------------------ */

interface RowProps {
  row: ChatMessageRow
  isSelf: boolean
  selfAvatar: ReactNode
  selfName: string
  agentAvatar: ReactNode
  agentName: string
  /** The card can no longer be answered (§5.5.4). */
  closed: boolean
  /** ...and a human did answer it — otherwise it just ended. */
  answered: boolean
  inFlightTrigger: string | null
  onOpenExec: (execSid: string) => void
  onAnswerCard: (card: ChatMessageRow) => void
}

function RowView(props: RowProps) {
  const { row, isSelf, selfAvatar, selfName, agentAvatar, agentName, closed, answered, inFlightTrigger, onOpenExec, onAnswerCard } = props
  if (row.kind === 'user_message' || row.kind === 'agent_message') {
    const fromAgent = row.kind === 'agent_message'
    return (
      <div className={`msg-group${isSelf ? ' msg-group-self' : ' msg-group-peer'}`}>
        <div className="msg-header">
          <div className="msg-avatar">{isSelf ? selfAvatar : fromAgent ? agentAvatar : '?'}</div>
          <div className="msg-sender">
            <span className="msg-sender-name">{isSelf ? selfName : agentName}</span>
          </div>
        </div>
        <div className="msg-stack">
          <div className={`bubble${isSelf ? ' bubble-self' : ' bubble-peer'}`}>
            <div className="bubble-text">{renderMarkdown(row.body)}</div>
            {row.attachments && row.attachments.length > 0 ? (
              <div className="chat-attachments">
                {row.attachments.map((a) => (
                  <span key={a.id} className="chat-attachment-chip">📎 {a.name}</span>
                ))}
              </div>
            ) : null}
          </div>
        </div>
      </div>
    )
  }

  if (row.kind === 'interaction_request') {
    // §6.5: collapsed to one line on mobile; the answer surface is a sheet.
    const q = row.meta?.interaction?.question || row.body
    return (
      <div className="chat-card-line">
        <button
          type="button"
          className="chat-card-line-btn"
          disabled={closed}
          onClick={() => onAnswerCard(row)}
        >
          <span className="chat-card-tag">{row.meta?.interaction?.kind === 'approval' ? '审批' : '提问'}</span>
          <span className="chat-card-text">{q.length > 34 ? `${q.slice(0, 34)}…` : q}</span>
          <span className="chat-card-state">{closed ? (answered ? '已回复' : '已结束') : '去回复'}</span>
        </button>
      </div>
    )
  }

  if (row.kind === 'system_notice') {
    if (row.meta?.notice_type === 'turn_started') {
      // §6.2: the pointer row renders the status card ONLY while its turn
      // is the in-flight one; once the terminal row lands the card is gone.
      if (row.meta?.in_reply_to !== inFlightTrigger) return null
      const execSid = row.meta?.agent_session_id ?? ''
      return (
        <div className="chat-card-line">
          <button
            type="button"
            className="chat-card-line-btn chat-card-status"
            disabled={!execSid}
            onClick={() => execSid && onOpenExec(execSid)}
          >
            <span className="chat-card-spinner" aria-hidden />
            <span className="chat-card-text">Agent 处理中{execSid ? ' · 点击查看执行过程' : ''}</span>
          </button>
        </div>
      )
    }
    return row.body ? (
      <div className="chat-notice">
        <span className="chat-notice-pill">{row.body}</span>
      </div>
    ) : null
  }

  // Unknown kind (forward-compat): a plain notice line, never a blank hole.
  return (
    <div className="chat-notice">
      <span className="chat-notice-pill">{row.body}</span>
    </div>
  )
}

/* ------------------------------------------------------------------ */
/* Decision card bottom sheet (§6.5)                                    */
/* ------------------------------------------------------------------ */

interface SheetProps {
  card: ChatMessageRow
  /** The card can no longer be answered (§5.5.4) — the sheet is read-only. */
  closed: boolean
  /** ...and a human did answer it — otherwise it just ended. */
  answered: boolean
  onClose: () => void
  onAnswer: (text: string) => void
}

function AnswerSheet({ card, closed, answered, onClose, onAnswer }: SheetProps) {
  const [free, setFree] = useState('')
  const interaction = card.meta?.interaction
  const kindLabel = interaction?.kind === 'approval' ? '允许执行？' : '需要你的回答'
  return (
    <Sheet open onClose={onClose} title={kindLabel}>
      <div className="chat-sheet-question">{interaction?.question || card.body}</div>
      {closed ? (
        <div className="chat-sheet-answered">{answered ? '该请求已被回复（可能在其他设备上）' : '该请求已结束，无法再回复'}</div>
      ) : (
        <>
          {(interaction?.options ?? []).map((opt) => (
            <button key={opt} type="button" className="chat-sheet-option" onClick={() => onAnswer(opt)}>
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
              disabled={!free.trim()}
              onClick={() => onAnswer(free.trim())}
            >
              发送
            </button>
          </div>
        </>
      )}
    </Sheet>
  )
}
