/**
 * Agent Chat session — the PRIMARY agent surface on mobile (design 27 §6.1,
 * plan Wave 5.5). Same view model as Desktop's `AgentChatView`: a durable event
 * log of four row kinds (ADR-106 D1) where a run is a POINTER row
 * (`execution_round`) whose content is read from the execution side at render
 * time (D3/D4) — the Chat keeps no copy of the work, and a pending decision is
 * shown inside the round's block, never as a chat row (D8). The storage is per
 * round, the RENDERING is per execution SESSION: the layout layer
 * (`lib/chatTranscriptGroups`) folds a session's rows into one block, the same
 * way one run's tool calls fold into a single `ExploreBlock`. The differences
 * here are presentation-only, per §6.5:
 *
 *  - the block's header line is the compact shape, its answer stays inline: a
 *    reply folded behind a tap here would cost more to read than on the surface
 *    that owns the same transcript;
 *  - a decision card collapses to one line and its buttons live in a BOTTOM
 *    SHEET (mis-tap safety on a 390pt screen), and the second level of the `@`
 *    is a sheet too, not a popup anchored to the composer;
 *  - answering an addressing card RE-DELIVERS the human's own words, now
 *    addressed (D6) — the same channel, the same row id, so the transcript
 *    keeps one message.
 *
 * "In progress" is positive evidence only (D9): the single waiting line this
 * surface may paint is about a send THIS device made, keyed by the id its own
 * response carried, and it ends where the transcript answers that id — the
 * round's reference row, the router's reply, or a receipt. A transcript SHAPE
 * ("the newest human row has no answer") is still not evidence, so a dead node
 * never keeps looking busy; and the same flag that shows the line locks the
 * composer, because this Chat has no queue to absorb a second send racing a
 * router still deciding the first. A round's liveness rides inside its block,
 * off the `pending` projection read — a transcript shape must never stand in
 * for liveness.
 *
 * Deliberately NOT here: agent thinking, tool calls, execution process — those
 * live behind the block's drill into the execution session view, not in the
 * chat stream (§6).
 *
 * Freshness: the `chat/changed` doorbell (content-free) fans out into
 * `wakeAgentChat()` → HTTP re-read; plus an immediate refresh on mount and
 * on foreground return. No streaming, ever, in v1 (design doc 25 §8).
 */

import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { AtSign, Quote, Send } from 'lucide-react'
import { useNavStore } from '../../stores/navStore'
import { useAuthStore } from '../../stores/authStore'
import { useAgentStore } from '../../stores/agentStore'
import { useChatStore } from '../../stores/chatStore'
import { useChatSessionStore, type ChatAddress } from '../../stores/chatSessionStore'
import { AuthImage } from '../../components/AuthImage'
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from '../../lib/builtinIcons'
import { daySeparator, timeGapSeparator } from '../../lib/time'
import { useStickToBottom } from '../../lib/stickToBottom'
import { Banner } from '../../components/ui'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import { AttachmentViewer } from '../../components/AttachmentViewer'
import { ChatAttachmentCapsule } from '../../components/ChatAttachmentCapsule'
import { ChatExecutionBlock } from '../../components/ChatExecutionBlock'
import { ChatNoticeRow } from '../../components/ChatNoticeRow'
import { ChatAddressSheet, type AddressPick } from '../../components/ChatAddressSheet'
import { answerWithChoice, directCardAddress, mayAnswerCard, type CardChoice } from '../../lib/chatCardAnswer'
import { fetchCardAnswer } from '../../lib/api'
import { turnInflight } from '../../lib/chatTurnInflight'
import { buildTranscriptItems, type TranscriptItem } from '../../lib/chatTranscriptGroups'
import { indexByMessageId, noticeAnchorOf, type NoticeAnchor } from '../../lib/chatNoticeAnchor'
import { renderMarkdown } from '../../lib/markdown'
import type { ChatAttachment } from '../../lib/chatAttachment'
import type { ChatMessageRow } from '../../lib/types'

function pickIconId(seed: string): string | null {
  if (BUILTIN_ICON_IDS.length === 0 || !seed) return null
  let h = 0
  for (let i = 0; i < seed.length; i++) h = (seed.charCodeAt(i) + (h << 5) - h) | 0
  return BUILTIN_ICON_IDS[Math.abs(h) % BUILTIN_ICON_IDS.length] ?? null
}

/** The render key of a transcript row. A folded block is keyed by its ANCHOR row
 *  — the newest of its session — so the list key tracks the item the reader sees,
 *  not one of the rows hidden inside it. */
function rowKeyOf(row: ChatMessageRow): string {
  return `${row.seq}-${row.meta?.message_id ?? ''}`
}

/** The row an item is positioned by: a block sits where its newest round is. */
function itemOf(item: TranscriptItem): ChatMessageRow {
  return item.kind === 'run' ? item.anchorRow : item.row
}

/** A pending continuation of one round: the reference row's id (what the send
 *  files as `in_reply_to`, ADR-106 D6 level 1) and the question the human is
 *  answering, shown in the composer chip. A card no longer has a chat row of
 *  its own, so the block is what hands this over. */
interface ReplyTarget {
  id: string
  quote: string
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
  // The turn this device sent and has not seen an outcome for. `sending` alone
  // only covers the POST (~150 ms measured); the claim carries the wait through
  // to the row the turn actually produces.
  const inflightTurn = useChatSessionStore((s) => (sessionId ? s.inflight[sessionId] : undefined))

  const [draft, setDraft] = useState('')
  /** D6 level 1: this draft continues one named round. */
  const [replyTo, setReplyTo] = useState<ReplyTarget | null>(null)
  /** The second level of the `@` (D7). On this surface the first level is
   *  degenerate — one agent — so the human names only the task. `addressStage`
   *  is "the sheet is open", `address` is "what was chosen for this draft". */
  const [addressStage, setAddressStage] = useState(false)
  const [address, setAddress] = useState<AddressPick | null>(null)
  /** ADR-106 revision 2026-11-04: the run whose block the `@` sheet's "去续投"
   *  (or a source chip) just located — that block opens + focuses its in-place
   *  reply row. Cleared on the next send so a stale focus never re-fires. */
  const [focusBlockSessionId, setFocusBlockSessionId] = useState<string | null>(null)
  /** A best-effort action that could not be carried out says so once, briefly
   *  (a locate that found no block in the loaded window; a card that can never
   *  be answered again). No toast primitive exists on mobile, so it rides a
   *  transient pill above the composer. */
  const [transientHint, setTransientHint] = useState<string | null>(null)
  // The scroll container, kept in a ref object alongside the hook's callback
  // ref, so a locate can find a block's DOM node (mobile is not virtualized —
  // every loaded row is in the tree).
  const containerRef = useRef<HTMLDivElement | null>(null)
  /** The uploaded file whose viewer is open. Held here, above the list, so a
   *  poll tick that re-renders (or re-keys) a row cannot close the overlay. */
  const [viewing, setViewing] = useState<ChatAttachment | null>(null)

  useEffect(() => {
    if (agentId) void useChatSessionStore.getState().openAgent(agentId)
  }, [agentId])

  const rows = useMemo(() => (win ? [...win.older, ...win.tail] : []), [win])
  // message_id → row, for the receipts that name the message they are about: a
  // delivery failure is placed in that message's column instead of floating
  // center (design 31 §6 leaves this join to the presentation layer).
  const rowByMessageId = useMemo(() => indexByMessageId(rows), [rows])
  const turnLive = turnInflight(rows, inflightTurn)
  // ONE state drives the line and the lock: a wait line with an unlocked
  // composer invites the second send this Chat cannot absorb, and a locked
  // composer with no line reads as a broken button.
  const composerLocked = sending || turnLive
  // Hand the claim back as soon as a row answers it, so nothing keeps
  // asserting a turn the transcript has already settled.
  useEffect(() => {
    if (!sessionId || !inflightTurn) return
    if (turnInflight(rows, inflightTurn)) return
    useChatSessionStore.getState().settleTurn(sessionId, inflightTurn.messageId)
  }, [rows, inflightTurn, sessionId])
  // The fold (ADR-106 D3/D4): the reference rows of ONE execution session become
  // ONE block item, and the question rows they were built from fold inside it.
  // Memoized on the window, so a composer keystroke keeps every item identity
  // that `ItemView`'s memo needs.
  const layout = useMemo(() => buildTranscriptItems(rows, rowKeyOf), [rows])
  const items = layout.items
  const { ref: scrollRef, stickNow, atBottom } = useStickToBottom([rows.length, sessionId], sessionId, () => {
    if (sessionId) void useChatSessionStore.getState().loadOlder(sessionId)
  })

  /** The one write path of this surface. No trim guard: a re-delivered message
   *  may legitimately be attachment-only, and the composer gates itself. */
  const deliver = (text: string, inReplyTo?: string, attachmentIds?: string[], addr?: ChatAddress) => {
    if (!sessionId) return
    setDraft('')
    setReplyTo(null)
    setAddress(null)
    setFocusBlockSessionId(null)
    // Sending re-claims the follow (the IM convention), before the echo lands.
    stickNow()
    void useChatSessionStore.getState().sendText(sessionId, text, inReplyTo, attachmentIds, addr)
  }

  const send = () => {
    const text = draft.trim()
    if (!text || composerLocked || !sessionId) return
    // A level-1 reply already names its session, so the address the human picked
    // for a plain message must not ride that send — the two are contradictory
    // instructions and the refusal belongs to the backend, not to a guess here.
    const chosen = replyTo ? null : address
    // ADR-106 revision 2026-11-04: a typed `@session` is now material, never a
    // delivery — the composer carries `refs` (the picked coordinates) and the
    // Gateway pins each round at send time. "new task" is still `forceNew`.
    const wire: ChatAddress | undefined = chosen?.refs?.length
      ? { refs: chosen.refs }
      : chosen?.forceNew
        ? { forceNew: true }
        : undefined
    deliver(text, replyTo?.id, undefined, wire)
  }

  /** Answering an addressing card (ADR-106 D6): re-send the human's OWN words
   *  with the address they just chose, keyed by the original row's id so the
   *  transcript keeps one message instead of narrating the decision twice. */
  const answerCard = async (card: ChatMessageRow, choice: CardChoice) => {
    const cardId = card.meta?.message_id
    if (!cardId || !sessionId) return
    const lookup = await fetchCardAnswer(sessionId, cardId)
    if (!lookup.ok) {
      // A spent card (`already_answered`) closes itself on the next projection
      // read; `gone` is the one the human has to be told about, because those
      // buttons will never work again.
      if (lookup.reason === 'gone') setTransientHint('这张卡片已无法回答')
      return
    }
    const answer = answerWithChoice(lookup.facts, choice)
    if (!answer) return
    deliver(answer.text, undefined, answer.attachmentIds, directCardAddress(answer))
  }

  /** A `pause` card's continue is not an answer of its own: the human's words
   *  make the NEXT turn of that round, so this only addresses the composer. */
  const replyToRound = (rowId: string, quote: string) => {
    setReplyTo({ id: rowId, quote })
    setAddress(null)
  }

  /** ADR-106 revision 2026-11-04: "去续投" / a source-chip click locate the run's
   *  block. Mobile is not virtualized, so the block's DOM node is found by the
   *  execution session it names and scrolled into view; the block then opens +
   *  focuses its in-place reply row. A run with no block in the loaded window
   *  answers with a transient hint and no auto-paging (v1). */
  const handleLocateRef = (execSessionId: string) => {
    setAddressStage(false)
    const scope = containerRef.current
    if (!scope || !execSessionId) {
      setTransientHint('任务块不在已加载历史')
      return
    }
    const el = scope.querySelector<HTMLElement>(
      `[data-session$="|${execSessionId}"]`,
    )
    if (!el) {
      setTransientHint('任务块不在已加载历史')
      return
    }
    el.scrollIntoView({ behavior: 'smooth', block: 'center' })
    setFocusBlockSessionId(execSessionId)
  }

  // Auto-dismiss the pill; the action already parked wherever it could.
  useEffect(() => {
    if (!transientHint) return
    const id = setTimeout(() => setTransientHint(null), 2600)
    return () => clearTimeout(id)
  }, [transientHint])

  /** The block's in-place reply sends straight down the level-1 branch: the
   *  text continues the freshest round the block names, exactly as a card
   *  answer does — the claim and the wait line fall out of `deliver`. */
  const handleReplyInPlace = (rowId: string, text: string) => {
    deliver(text, rowId)
  }

  /** §6.5: the block's drill — the existing execution-session screen, reached
   *  the same way the inbox reached it before the primary surface moved to the
   *  Chat session. The block names the instance that hosts the round, so this
   *  takes it rather than assuming it is the agent on screen. */
  const openExec = (instanceId: string, execSid: string) => {
    if (!instanceId || !execSid) return
    void useChatStore.getState().openSession(instanceId, execSid).then(() => push('chat/detail'))
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

        <div
          className="scroll scroll-chat"
          ref={(el) => {
            containerRef.current = el
            if (typeof scrollRef === 'function') scrollRef(el)
          }}
        >
          {win?.loadingOlder ? <div className="time-sep">载入更早消息…</div> : null}
          {!st?.resolved ? <div className="empty-state">载入中…</div> : null}
          {st?.resolved && !failed && rows.length === 0 ? (
            <div className="empty-state">还没有消息，发一条开始对话</div>
          ) : null}
          {items.map((item, i) => {
            const row = item.kind === 'run' ? item.anchorRow : item.row
            const prev = i > 0 ? itemOf(items[i - 1]!) : null
            const sep = prev ? daySeparator(prev.ts, row.ts) ?? timeGapSeparator(prev.ts, row.ts) : null
            return (
              <Fragment key={rowKeyOf(row)}>
                {sep ? <div className="time-sep">{sep}</div> : null}
                <ItemView
                  item={item}
                  hasFollowUp={i < items.length - 1}
                  chatSessionId={sessionId ?? ''}
                  isSelf={row.from === me?.user_id}
                  selfAvatar={meAvatar}
                  selfName={myName}
                  agentAvatar={agentAvatar}
                  agentName={agent?.name ?? 'Agent'}
                  onJumpToExec={openExec}
                  onReplyToRound={replyToRound}
                  // The card asks the person who spoke, and in local mode there
                  // is exactly one person but no id to compare against.
                  onChooseCard={
                    item.kind === 'row' && mayAnswerCard(item.row, me?.user_id ?? '')
                      ? answerCard
                      : undefined
                  }
                  onOpenAttachment={setViewing}
                  onLocateRef={handleLocateRef}
                  focusBlockSessionId={focusBlockSessionId ?? undefined}
                  noticeAnchor={
                    item.kind === 'row' && item.row.kind === 'system_notice'
                      ? noticeAnchorOf(item.row, rowByMessageId, me?.user_id ?? '')
                      : undefined
                  }
                  onReplyInPlace={handleReplyInPlace}
                />
              </Fragment>
            )
          })}
          {/* D9: the only "working on it" line this surface may paint is about a
              send of its own — never about the transcript's shape. It ends at the
              first row that answers the send (the round's reference row, the
              router's reply, a receipt), so it cannot fake liveness for a node
              that is gone; and the round's own liveness rides inside its block,
              off the `pending` projection read. */}
          {composerLocked ? (
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

        {/* What this draft carries. Each instruction is shown because one the
            surface swallowed would be indistinguishable from one never given —
            and a reference is material the human attached, shown as its own chip
            so it can be dropped one at a time (ADR-106 D7, rev 2026-11-04). */}
        {replyTo || address ? (
          <div className="composer-chips">
            {replyTo ? (
              <span className="composer-chip">
                <span className="composer-chip-text">
                  回复 {replyTo.quote.slice(0, 30)}
                </span>
                <button
                  type="button"
                  className="composer-chip-clear"
                  onClick={() => setReplyTo(null)}
                  aria-label="取消回复"
                >
                  ×
                </button>
              </span>
            ) : address ? (
              <>
                {address.forceNew ? (
                  <span className="composer-chip">
                    <span className="composer-chip-text">新任务</span>
                    <button
                      type="button"
                      className="composer-chip-clear"
                      onClick={() => setAddress(null)}
                      aria-label="清除地址"
                    >
                      ×
                    </button>
                  </span>
                ) : null}
                {(address.refs ?? []).map((r) => {
                  const key = `${r.agentInstanceId}|${r.sessionId}`
                  const label = address.labels?.[key] || r.sessionId
                  return (
                    <span key={key} className="composer-chip composer-ref-chip">
                      <Quote className="composer-ref-icon" aria-hidden />
                      <span className="composer-chip-text">{label}</span>
                      <button
                        type="button"
                        className="composer-chip-clear"
                        onClick={() => {
                          const rest = (address.refs ?? []).filter(
                            (x) => `${x.agentInstanceId}|${x.sessionId}` !== key,
                          )
                          const labels = { ...(address.labels ?? {}) }
                          delete labels[key]
                          setAddress(
                            rest.length > 0
                              ? { refs: rest, labels, forceNew: address.forceNew }
                              : address.forceNew
                                ? { forceNew: true }
                                : null,
                          )
                        }}
                        aria-label="移除引用"
                      >
                        ×
                      </button>
                    </span>
                  )
                })}
              </>
            ) : null}
          </div>
        ) : null}

        {transientHint ? (
          <div className="composer-locate-hint" role="status">
            {transientHint}
          </div>
        ) : null}

        <div className="composer">
          <button
            type="button"
            className="composer-address"
            aria-label="指定任务"
            disabled={!sessionId || !!replyTo}
            onClick={() => setAddressStage(true)}
          >
            <AtSign className="composer-send-icon" aria-hidden />
          </button>
          <textarea
            className="composer-input"
            rows={1}
            placeholder="发送消息"
            aria-label="消息输入框"
            value={draft}
            disabled={composerLocked}
            onChange={(e) => setDraft(e.target.value)}
          />
          <button
            type="button"
            className="composer-send"
            aria-label="发送"
            title={turnLive ? '上一条消息正在处理中' : '发送'}
            disabled={!draft.trim() || composerLocked || !sessionId}
            onClick={send}
          >
            <Send className="composer-send-icon" aria-hidden />
          </button>
        </div>

        {addressStage && sessionId ? (
          <ChatAddressSheet
            instanceId={agentId}
            chatSessionId={sessionId}
            onPick={(pick) => {
              setAddress(pick.refs?.length || pick.forceNew ? pick : null)
              setAddressStage(false)
            }}
            onLocate={(sid) => handleLocateRef(sid)}
            onClose={() => setAddressStage(false)}
          />
        ) : null}

        {/* An uploaded file's viewer. Desktop parity: the same attachment the
            chat received is tappable on both surfaces. Only rendered with a
            session id, because the bytes are fetched through the session route
            (a group chat has no single account to scope them by). */}
        {viewing && sessionId ? (
          <AttachmentViewer
            chatSessionId={sessionId}
            attachment={viewing}
            onClose={() => setViewing(null)}
          />
        ) : null}
      </div>
    </EdgeSwipe>
  )
}

/* ------------------------------------------------------------------ */
/* Item rendering — the folded transcript (ADR-106 D1/D3/D4)            */
/* ------------------------------------------------------------------ */

export interface RowProps {
  row: ChatMessageRow
  /** The Chat this row belongs to: the block read is participation-checked
   *  against it (ADR-106 D10), so it is part of the address, not a filter. */
  chatSessionId: string
  isSelf: boolean
  selfAvatar: ReactNode
  selfName: string
  agentAvatar: ReactNode
  agentName: string
  onJumpToExec: (agentInstanceId: string, execSid: string) => void
  onReplyToRound: (rowId: string, quote: string) => void
  /** Answer an addressing card (D6 levels 3 and 5) by re-delivering the
   *  utterance it asks about, now addressed. Handed only to the person the card
   *  asked (D10) — `undefined` for anyone else. */
  onChooseCard?: (card: ChatMessageRow, choice: CardChoice) => void
  /** A tap on an uploaded file's capsule. The parent owns the viewer so one
   *  overlay serves every row in the list (and outlives re-renders of it). */
  onOpenAttachment: (attachment: ChatAttachment) => void
  /** ADR-106 revision 2026-11-04: click a reference source chip (or the `@`
   *  sheet's "去续投") to locate that run's block. A navigation, not an address —
   *  it never changes what a message carries. */
  onLocateRef?: (sessionId: string) => void
  /** Which execution session the sheet just asked to continue: the matching
   *  block opens + focuses its in-place reply row. */
  focusBlockSessionId?: string
  /** Column a channel receipt belongs in (see `lib/chatNoticeAnchor`). Absent
   *  for every non-notice row and for a receipt whose subject is not in the
   *  loaded window. */
  noticeAnchor?: NoticeAnchor
  /** The block's in-place reply sends straight down the level-1 branch. */
  onReplyInPlace?: (rowId: string, text: string) => void
}

export function RowView(props: RowProps) {
  const {
    row,
    isSelf,
    selfAvatar,
    selfName,
    agentAvatar,
    agentName,
    onChooseCard,
    onOpenAttachment,
    onLocateRef,
    noticeAnchor,
  } = props

  if (row.kind === 'execution_round') {
    // A reference row that names no round (a missing coordinate) addresses
    // nothing, so it has no block to fold into and keeps its own explicit hole
    // (ADR-106 D3). Every row that DOES name a round is drawn by the session's
    // `ChatExecutionBlock` and reaches it as a `run` item — not as a row.
    return <div className="chat-notice"><span className="chat-notice-pill">本轮执行已不可用</span></div>
  }

  if (row.kind === 'system_notice') {
    // Delivery receipts, the budget receipt, and the addressing questions
    // (ADR-106 D6): quiet centered lines, never agent speech. The routing
    // milestones and the card-resolution receipt this branch used to paint are
    // gone from the stream.
    const noticeType = row.meta?.notice_type
    const choose =
      (noticeType === 'session_choice' || noticeType === 'confirm_continue') && onChooseCard
        ? (choice: CardChoice) => onChooseCard(row, choice)
        : undefined
    return <ChatNoticeRow row={row} anchor={noticeAnchor} onChoose={choose} />
  }

  if (row.kind === 'interaction_request') {
    // A run's pending decision is no longer a chat row (ADR-106 D8): it shows
    // inside the round's block, where answering it also belongs. Old transcripts
    // still carry these rows and the log keeps recording them — neither may
    // resurrect a decision that is answered somewhere else now, so this paints
    // nothing.
    return null
  }

  if (row.kind === 'user_message' || row.kind === 'agent_message') {
    const fromAgent = row.kind === 'agent_message'
    // A message that continues a round (`in_reply_to` = the reference row) needs
    // no quote above it: the block it answers is the row directly beneath.
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
            {/* Only the Chat Router's own answer is written as `agent_message`
                now (ADR-106 D12) — a plain-text reply to something that opened
                no run. A bodyless row with no attachments has nothing to say;
                the placeholder keeps it from collapsing into an empty bubble. */}
            {row.body || !row.attachments?.length ? (
              <div className={`bubble-text${row.kind === 'user_message' ? ' bubble-raw' : ''}`}>
                {row.body ? (
                  row.kind === 'user_message' ? (
                    row.body
                  ) : (
                    renderMarkdown(row.body)
                  )
                ) : (
                  <span className="bubble-empty">（无内容）</span>
                )}
              </div>
            ) : null}
            {row.attachments && row.attachments.length > 0 ? (
              <div className="chat-attachments">
                {row.attachments.map((a) => (
                  <ChatAttachmentCapsule key={a.id} attachment={a} onOpen={onOpenAttachment} />
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
            {/* ADR-106 revision 2026-11-04: the `@session` material attached to
                this utterance, read off the row's pinned `meta.refs` (no new
                read). Source chips under the bubble — the human sees what they
                quoted and one tap jumps to that run's block. */}
            {isSelf && row.meta?.refs && row.meta.refs.length > 0 ? (
              <div className="chat-ref-chips">
                {row.meta.refs.map((r) => (
                  <button
                    key={`${r.agent_instance_id}|${r.session_id}|${r.round_id}`}
                    type="button"
                    className="chat-ref-chip"
                    onClick={() => onLocateRef?.(r.session_id)}
                    title={r.session_id}
                  >
                    <Quote className="chat-ref-chip-icon" aria-hidden />
                    <span className="chat-ref-chip-text">{r.session_id}</span>
                  </button>
                ))}
              </div>
            ) : null}
          </div>
        </div>
      </div>
    )
  }

  // Unknown kind (forward-compat): a plain notice line, never a blank hole.
  return (
    <div className="chat-notice">
      <span className="chat-notice-pill">{row.body}</span>
    </div>
  )
}

export interface ItemProps extends Omit<RowProps, 'row'> {
  item: TranscriptItem
  /** Something follows this block in the transcript — `ExploreBlock`'s
   *  follow-up condition, the one that auto-collapses a settled run. */
  hasFollowUp: boolean
}

/** One item of the folded transcript: an execution session's block, or a row.
 *  The layout layer (`lib/chatTranscriptGroups`) decides the fold; this is where
 *  it becomes a component. */
export function ItemView({ item, hasFollowUp, ...rowProps }: ItemProps) {
  if (item.kind === 'run') {
    return (
      <ChatExecutionBlock
        block={item}
        chatSessionId={rowProps.chatSessionId}
        // A block always starts a new turn, so it wears the avatar and name a
        // bubble wears — the run speaks as the agent, not as the transcript.
        showHeader
        hasFollowUp={hasFollowUp}
        agentAvatar={rowProps.agentAvatar}
        agentName={rowProps.agentName}
        onJumpToExec={rowProps.onJumpToExec}
        onReplyToRound={rowProps.onReplyToRound}
        focusInput={
          !!rowProps.focusBlockSessionId &&
          item.execSessionId === rowProps.focusBlockSessionId
        }
        onReplyInPlace={rowProps.onReplyInPlace}
      />
    )
  }
  return <RowView {...rowProps} row={item.row} />
}
