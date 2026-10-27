/**
 * Agent Chat session — the PRIMARY agent surface on mobile (design 27 §6.1,
 * plan Wave 5.5). Same view model as Desktop's `AgentChatView`: durable
 * chat rows only (user_message / agent_message / interaction_request /
 * system_notice). The differences are presentation-only, per §6.5:
 *
 *  - turn state is DERIVED, never stored (ADR-100 revision): while the newest
 *    message is unanswered the stream shows one centered shimmer line, and a
 *    turn that ended without a reply (stopped / failed / timed out) leaves a
 *    centered outcome line with a drill into the execution session view (the
 *    existing ChatDetailScreen — the read path enforces visibility, hidden 404
 *    shows its own error);
 *  - a decision card collapses to one line; answering opens a BOTTOM SHEET
 *    (mis-tap safety on a 390pt screen), and every answer goes back through
 *    the same chat-message channel with `in_reply_to` (§5.5.4).
 *
 * Deliberately NOT here: agent thinking, tool calls, execution process —
 * those live behind the event row's drill, not in the chat stream (§6).
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

/** Cards answered on THIS surface — a `user_message` row addressed to the
 *  card. The chat-side subset of [`humanAnsweredCardIds`]: the surface uses
 *  it to suppress the Runtime's duplicate `interaction_resolved` notice, so a
 *  chat-side answer is recorded exactly once (by its own event line). Mirrors
 *  the desktop `chatAnsweredCardIds`. */
export function chatAnsweredCardIds(rows: ChatMessageRow[]): Set<string> {
  return new Set(
    rows
      .filter((r) => r.kind === 'user_message')
      .map((r) => r.meta?.in_reply_to)
      .filter((s): s is string => !!s),
  )
}

/** Resolutions that mean a human decided. Everything else (timeout / stopped /
 *  turn_ended / cancelled / unanswerable / superseded) closes a card without
 *  claiming an answer. Mirrors the desktop `DECISION_RESOLUTIONS`. */
const DECISION_RESOLUTIONS = new Set(['answered', 'approved', 'rejected'])

/** Cards a human actually answered. Two shapes carry that fact:
 *  - a `user_message` addressed to the card (answered on THIS surface);
 *  - the Runtime's `interaction_resolved` notice with a decision resolution
 *    ("answered"/"approved"/"rejected") — answered on the execution surface,
 *    where the reply never rides the chat answer channel. A non-decision
 *    resolution means nobody answered, so it stays out.
 *  Narrower than [`closedCardIds`]: it exists so a surface can tell "you
 *  answered this" from "this request is just no longer open". Mirrors the
 *  desktop `humanAnsweredCardIds`. */
export function humanAnsweredCardIds(rows: ChatMessageRow[]): Set<string> {
  const answered = chatAnsweredCardIds(rows)
  for (const r of rows) {
    const mid = r.meta?.in_reply_to
    if (
      mid &&
      r.kind === 'system_notice' &&
      r.meta?.notice_type === 'interaction_resolved' &&
      DECISION_RESOLUTIONS.has(r.meta?.resolution ?? '')
    ) {
      answered.add(mid)
    }
  }
  return answered
}

/**
 * COPIED VERBATIM from the desktop `chatTurnState` module — both apps must
 * infer the live turn state from the same durable evidence (§5.5.5 drift
 * guard).
 *
 * The chat log records conversation, not progress: the dispatch / round-open
 * milestones are runtime facts and no longer rows. "A turn is in flight" is
 * derived instead: the newest human message has no terminal reply and no open
 * decision card after it.
 */
const TERMINAL_STATUSES = new Set(['completed', 'failed', 'aborted', 'timeout', 'waiting'])

/** Notice types retired by the ADR-100 revision — never painted, even when an
 * old transcript still carries them. Mirrors the desktop renderer. */
const RETIRED_NOTICES = new Set(['turn_dispatched', 'turn_started'])

export function isTerminalReply(row: ChatMessageRow): boolean {
  return row.kind === 'agent_message' && TERMINAL_STATUSES.has(row.meta?.status ?? '')
}

/** `failed` / `timeout` are errors; `aborted` is a user action, not a fault. */
export function isFailureStatus(status?: string): boolean {
  return status === 'failed' || status === 'timeout'
}

/** A terminal reply with no text: the outcome IS the message. */
export function isEmptyTerminalReply(row: ChatMessageRow): boolean {
  return isTerminalReply(row) && !row.body.trim() && !(row.attachments && row.attachments.length > 0)
}

/**
 * The routing reaction row (ADR-100 D11): the agent's immediate in-chat
 * answer to "where did my message go" — "启动新任务：X" / "继续之前的任务：X".
 * A `system_notice` by producer, but it renders as an agent bubble (avatar +
 * name): a user message must always be followed by an agent message — that
 * exchange IS the chat. Mirrors the desktop `chatTurnState` renderer.
 */
export function isTurnRoutedNotice(row: ChatMessageRow): boolean {
  return row.kind === 'system_notice' && row.meta?.notice_type === 'turn_routed'
}

/** Label for the centered outcome line of an empty terminal reply. */
export function terminalStatusText(status?: string): string {
  switch (status) {
    case 'aborted':
      return '本轮执行已中断'
    case 'timeout':
      return '本轮执行超时'
    case 'failed':
      return '本轮执行失败'
    case 'waiting':
      return '本轮等待你回复'
    default:
      return '本轮没有产生回复'
  }
}

/** Bound a raw trigger body used as the task name in the routing reaction,
 *  mirroring the desktop `truncateTaskName` — a long first message must not
 *  wrap the bubble. */
function truncateTaskName(body: string): string {
  const collapsed = body.trim().replace(/\s+/g, ' ')
  if (!collapsed) return ''
  return collapsed.length > 40 ? `${collapsed.slice(0, 40)}…` : collapsed
}

/**
 * Did this human row actually ask an Agent something? (Same rule as the desktop
 * `chatTurnState.opensAgentTurn`.) `/api/intents/chat-message` files its
 * `user_message` with `to` = the agent it delivered to, and the §5.3.1 mention
 * fan-out files one broadcast row (`to` empty) carrying `meta.mentions`.
 * Archive-only rows — a `user_user` DM, an un-@'d group remark — store `to: ""`
 * and wake nobody, so they must not hold a waiting line open: no agent will ever
 * answer a conversation that contains no agent.
 */
export function opensAgentTurn(row: ChatMessageRow): boolean {
  if (row.to?.trim()) return true
  return (row.meta?.mentions?.length ?? 0) > 0
}

/**
 * Is the newest human message still waiting for its answer?
 *
 * `sending` covers the HTTP round trip before the human row lands. Afterwards
 * the turn is over as soon as a terminal reply, an OPEN decision card, or a
 * newer human message that opens its own turn follows the trigger — the same
 * rule the desktop uses. Rows that opened no turn are skipped rather than
 * treated as a superseder. The routing reaction row (ADR-100 D11) does NOT close
 * the wait: the run is still in flight, and the live line is the only thing
 * between the reaction and the final answer.
 */
export function pendingTurn(
  rows: ChatMessageRow[],
  sending: boolean,
  closedCards: ReadonlySet<string>,
): boolean {
  if (sending) return true
  for (let i = rows.length - 1; i >= 0; i--) {
    const row = rows[i]!
    if (row.kind !== 'user_message' || !opensAgentTurn(row)) continue
    const trigger = row.meta?.message_id
    return !!trigger && !answeredAfter(rows, i, trigger, closedCards)
  }
  return false
}

function answeredAfter(
  rows: ChatMessageRow[],
  fromIndex: number,
  trigger: string,
  closedCards: ReadonlySet<string>,
): boolean {
  for (let i = fromIndex + 1; i < rows.length; i++) {
    const row = rows[i]!
    if (row.kind === 'agent_message' && row.meta?.in_reply_to === trigger && isTerminalReply(row)) {
      return true
    }
    if (row.kind === 'interaction_request' && row.meta?.in_reply_to === trigger) {
      const cardId = row.meta?.message_id
      if (!cardId || !closedCards.has(cardId)) return true
    }
  }
  return false
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
  // The narrower "answered on THIS surface" half — what tells the Runtime's
  // duplicate `interaction_resolved` notice to stay silent.
  const answeredHere = useMemo(() => chatAnsweredCardIds(rows), [rows])
  // message_id → body, so the routing reaction bubble (ADR-100 D11) can name
  // the task off its trigger message — the mobile twin of the desktop index.
  const bodyById = useMemo(() => {
    const m = new Map<string, string>()
    for (const r of rows) {
      if (r.meta?.message_id) m.set(r.meta.message_id, r.body)
    }
    return m
  }, [rows])
  // Card rows only, so an answer can name the interaction kind it answers
  // (ask_question/approval render as an event line, pause keeps its message
  // form) and quote the question it answers — the mobile twin of the desktop
  // `cardKindById` / `cardQuoteById` pair. Card-only on purpose: `in_reply_to`
  // also points at plain messages, and quoting one would paint a decision
  // quote where no decision was asked.
  const cardKindById = useMemo(() => {
    const m = new Map<string, string>()
    for (const r of rows) {
      if (r.kind !== 'interaction_request' || !r.meta?.message_id) continue
      m.set(r.meta.message_id, r.meta.interaction?.kind ?? '')
    }
    return m
  }, [rows])
  const cardQuoteById = useMemo(() => {
    const m = new Map<string, string>()
    for (const r of rows) {
      if (r.kind !== 'interaction_request' || !r.meta?.message_id) continue
      m.set(r.meta.message_id, r.meta.interaction?.question || r.body)
    }
    return m
  }, [rows])
  // Derived live state — one aggregated line while the newest message waits.
  const waiting = pendingTurn(rows, sending, closed)

  const { ref: scrollRef, stickNow, atBottom } = useStickToBottom([rows.length, sessionId], sessionId, () => {
    if (sessionId) void useChatSessionStore.getState().loadOlder(sessionId)
  })

  const send = (text: string, inReplyTo?: string) => {
    const trimmed = text.trim()
    if (!trimmed || sending || !sessionId) return
    setDraft('')
    setAnswering(null)
    // Sending re-claims the follow (the IM convention), before the echo lands.
    stickNow()
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
          {st?.resolved && !failed && rows.length === 0 ? (
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
                  answeredCardKind={
                    r.meta?.in_reply_to ? cardKindById.get(r.meta.in_reply_to) : undefined
                  }
                  replyQuote={r.meta?.in_reply_to ? cardQuoteById.get(r.meta.in_reply_to) : undefined}
                  answeredCards={answeredHuman}
                  chatAnswered={answeredHere}
                  onOpenExec={openExec}
                  onAnswerCard={(card) => setAnswering(card)}
                  triggerBody={r.meta?.in_reply_to ? bodyById.get(r.meta.in_reply_to) : undefined}
                />
              </Fragment>
            )
          })}
          {/* One aggregated "the agent has your message" line, derived from the
              newest unanswered row — it disappears the moment a terminal row
              lands. */}
          {waiting ? (
            <div className="chat-turn-status" role="status">
              <span className="chat-turn-status-text chat-turn-status-text-live">
                {agent?.name ? `${agent.name}正在处理…` : '正在处理…'}
              </span>
            </div>
          ) : null}
          {/* Back-to-bottom arrow — visible only while the reader has scrolled
              up off the tail (and there is content). Tapping is the explicit
              act that re-claims the follow, the same convention the "N new
              messages" pill encodes on the human thread. `position: sticky`
              rather than an overlay, for the same reason as that pill: it only
              ever sits below the reader's position, so showing it cannot shift
              what they are looking at. */}
          {atBottom === false && rows.length > 0 ? (
            <button
              type="button"
              className="chat-scroll-pill"
              onClick={stickNow}
              aria-label="回到底部"
            >
              回到底部 ↓
            </button>
          ) : null}
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

export interface RowProps {
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
  onOpenExec: (execSid: string) => void
  onAnswerCard: (card: ChatMessageRow) => void
  /** Body of the message that triggered this row's turn (`meta.in_reply_to`).
   *  Names the task in the routing reaction bubble (ADR-100 D11), the mobile
   *  twin of the desktop `triggerBody` — mobile has no session-title store
   *  read here, so the trigger body is the name. */
  triggerBody?: string
  /** Interaction kind of the card this row answers (`ask_question` /
   *  `approval` / `pause`), resolved through the parent's card-only index. An
   *  ask/approval answer is a step of the run, not an utterance, so it renders
   *  as a centered event line; a PAUSE answer keeps reading as a message. */
  answeredCardKind?: string
  /** Question text of the card this row answers — the one-line quote above the
   *  answer bubble (card-only, so a reply to a plain message paints nothing). */
  replyQuote?: string
  /** Cards a human answered on either surface (`humanAnsweredCardIds`). Paired
   *  with `chatAnswered`, tells an exec-surface decision from a nobody-answered
   *  closure. */
  answeredCards: ReadonlySet<string>
  /** Cards answered on THIS surface (`chatAnsweredCardIds`): their answer row
   *  already states the outcome, so the Runtime's duplicate resolution notice
   *  must stay silent. */
  chatAnswered: ReadonlySet<string>
}

/** Mobile twin of the desktop task chip (design 27 §6.4, v2.8): a PM
 *  dispatch row shows its task id as a pill; tap copies the bare id. The
 *  claim/submit playbook never reaches this surface — the row body is the
 *  human text alone. */
export function TaskChip({ taskId }: { taskId: string }) {
  const [copied, setCopied] = useState(false)
  const onCopy = () => {
    navigator.clipboard
      ?.writeText(taskId)
      .then(() => {
        setCopied(true)
        setTimeout(() => setCopied(false), 1500)
      })
      .catch(() => {
        /* clipboard unavailable — the id is still visible as the label */
      })
  }
  return (
    <button type="button" className="chat-task-chip" onClick={onCopy} title="复制任务 ID">
      任务 {taskId}
      {copied ? ' ✓' : ''}
    </button>
  )
}

/** Bound a quoted question so one line stays one line. Longer than
 *  `truncateTaskName` because this is the question itself, not a label for it.
 *  The mobile twin of the desktop `ReplyQuote.truncateQuote`. */
export function truncateQuote(text: string | undefined): string {
  const collapsed = (text ?? '').trim().replace(/\s+/g, ' ')
  if (!collapsed) return ''
  return collapsed.length > 60 ? `${collapsed.slice(0, 60)}…` : collapsed
}

/** One line above an answer bubble naming the question it answers — the mobile
 *  twin of the desktop `ReplyQuote`. Card-only: the parent resolves it through
 *  a card-only index, so a reply to a plain message paints nothing. It rides
 *  ABOVE the bubble, never inside it: the own bubble is accent-filled with
 *  white text, and a tertiary-gray quote inside it would be unreadable. */
export function ReplyQuote({ quote }: { quote?: string }) {
  const text = truncateQuote(quote)
  if (!text) return null
  return (
    <div className="chat-reply-quote" title={quote}>
      <span className="chat-reply-quote-mark" aria-hidden>↳</span>
      <span className="chat-reply-quote-text">{text}</span>
    </div>
  )
}

export function RowView(props: RowProps) {
  const { row, isSelf, selfAvatar, selfName, agentAvatar, agentName, closed, answered, onOpenExec, onAnswerCard, triggerBody, answeredCardKind, replyQuote, answeredCards, chatAnswered } = props
  const routed = isTurnRoutedNotice(row)
  const execSid =
    row.kind === 'agent_message' || routed
      ? row.meta?.agent_session_id || row.meta?.sender_session_id || ''
      : ''
  // ADR-100 D11: the routing reaction — runtime-written, but it speaks as the
  // agent: avatar + name + bubble, never a centered system line. A user
  // message must always be followed by an agent message — that exchange IS
  // the chat, and it lands within seconds of the send, long before the answer.
  if (routed) {
    const task = truncateTaskName(triggerBody ?? '')
    const text =
      row.meta?.turn_mode === 'new' ? `启动新任务：${task}` : `继续之前的任务：${task}`
    return (
      <div className="msg-group msg-group-peer">
        <div className="msg-header">
          <div className="msg-avatar">{agentAvatar}</div>
          <div className="msg-sender">
            <span className="msg-sender-name">{agentName}</span>
          </div>
        </div>
        <div className="msg-stack">
          <div className="bubble bubble-peer">
            <div className="bubble-text">{text}</div>
          </div>
          {execSid ? (
            <button type="button" className="chat-bubble-drill" onClick={() => onOpenExec(execSid)}>
              查看执行
            </button>
          ) : null}
        </div>
      </div>
    )
  }
  // A decision answered on THIS surface. The card interaction is a step of
  // the run, not an utterance — so it states itself as a centered event line,
  // the sibling of the unanswered note the card leaves behind. PAUSE answers
  // are excluded: that text IS the new turn's prompt and must read as a
  // message (so it falls through to the user bubble below, quote included).
  if (
    row.kind === 'user_message' &&
    answeredCardKind &&
    answeredCardKind !== 'pause' &&
    !(row.attachments && row.attachments.length > 0)
  ) {
    return (
      <div className="chat-turn-status">
        <span className="chat-turn-status-text" title={row.body}>
          已回复：{truncateQuote(row.body)}
        </span>
      </div>
    )
  }
  // A terminal reply with nothing in it IS its outcome — a centered status
  // line with the drill into the run, never a bubble in the agent's voice.
  if (row.kind === 'agent_message' && isEmptyTerminalReply(row)) {
    return (
      <div className="chat-turn-status">
        <span
          className={`chat-turn-status-text${isFailureStatus(row.meta?.status) ? ' chat-turn-status-text-danger' : ''}`}
        >
          {terminalStatusText(row.meta?.status)}
        </span>
        {execSid ? (
          <button type="button" className="chat-turn-status-drill" onClick={() => onOpenExec(execSid)}>
            查看执行
          </button>
        ) : null}
      </div>
    )
  }
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
          {/* Design 27 §6.4 (v2.8): a PM dispatch row's task id as a pill,
              above the bubble — the row body is human text only. */}
          {row.kind === 'user_message' && row.meta?.task_id ? (
            <TaskChip taskId={row.meta.task_id} />
          ) : null}
          {/* What this answer answers (card-only). The card itself is gone once
              answered (§5.5.4 records the answer here, not on a receipt), so a
              PAUSE answer names its question with this one line above the
              bubble. */}
          {row.kind === 'user_message' ? <ReplyQuote quote={replyQuote} /> : null}
          <div className={`bubble${isSelf ? ' bubble-self' : ' bubble-peer'}`}>
            {/* Human input is literal text, markdown is agent speech — the house
                grammar the desktop surfaces apply to their user rows. Routing a
                `user_message` through the markdown renderer would make
                `**bold**` render bold in one surface while reading literally in
                another, and would pick up the md sub-styles (`.md-quote`'s
                secondary gray) that are unreadable on the accent-filled bubble's
                white text. */}
            <div className={`bubble-text${row.kind === 'user_message' ? ' bubble-raw' : ''}`}>
              {row.kind === 'user_message' ? row.body : renderMarkdown(row.body)}
            </div>
            {row.attachments && row.attachments.length > 0 ? (
              <div className="chat-attachments">
                {row.attachments.map((a) => (
                  <span key={a.id} className="chat-attachment-chip">📎 {a.name}</span>
                ))}
              </div>
            ) : null}
            {/* ADR-100 D12: a turn sent from an execution view carries that run's
                attached items as labels only — the file belongs to the execution
                session, not to this chat, so there is nothing to open. They still
                paint, because a task sent with a file and no words must not read
                as an empty bubble. */}
            {row.meta?.exec_attachments && row.meta.exec_attachments.length > 0 ? (
              <div className="chat-attachments">
                {row.meta.exec_attachments.map((l, i) => (
                  <span key={`exec-${i}-${l.filename}`} className="chat-attachment-chip chat-attachment-label">
                    {l.filename}
                  </span>
                ))}
              </div>
            ) : null}
          </div>
          {/* Every agent reply with a resolved execution session drills into it,
              finished or empty alike — the mobile twin of the desktop
              "View run" affordance. */}
          {fromAgent && execSid ? (
            <button type="button" className="chat-bubble-drill" onClick={() => onOpenExec(execSid)}>
              查看执行
            </button>
          ) : null}
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
    const noticeType = row.meta?.notice_type ?? ''
    // The milestones are retired (ADR-100 revision): the runtime stopped
    // writing them, and an old transcript's copies must not paint either —
    // state belongs to the live surface, not to history.
    if (RETIRED_NOTICES.has(noticeType)) return null
    // A card's closure. A nobody-answered close stays on the CARD (its state
    // line reads "已结束"), so this row is silent for it. A human decision made
    // on the EXECUTION surface never arrives here as a message row — this
    // notice is the transcript's only record of it, and it says so as a
    // centered event line, not a bubble (a card interaction is a step of the
    // run, not an utterance). The chat-side decision is suppressed: its own
    // answer row already stated the outcome.
    if (noticeType === 'interaction_resolved') {
      const cardId = row.meta?.in_reply_to ?? ''
      if (cardId && answeredCards.has(cardId) && !chatAnswered.has(cardId)) {
        return (
          <div className="chat-turn-status">
            <span className="chat-turn-status-text">已在执行会话中回复</span>
          </div>
        )
      }
      return null
    }
    // The router-fallback receipt is keyed by notice_type (bodyless row);
    // the surface owns the wording.
    if (noticeType === 'router_fallback') {
      return (
        <div className="chat-notice">
          <span className="chat-notice-pill">已自动分配任务</span>
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
