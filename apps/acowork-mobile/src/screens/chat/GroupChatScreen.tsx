/**
 * Group Chat session — the mobile surface for a `project_group` / `user_group`
 * (design 27 §4.4 / §5.3, §6.5), the multi-party twin of `AgentChatScreen`.
 *
 * Same view model as the 1:1 surface — a durable event log of four row kinds
 * (ADR-106 D1), rounds folded into per-session blocks, cards answered inside
 * the block (D8) — and the same window machinery (`chatSessionStore.windows`
 * is keyed by session id, so paging / refresh / the doorbell are shared
 * verbatim). What is different is the party structure:
 *
 *  - every row wears ITS speaker: name + avatar resolved from the session
 *    roster (agents off the local registry, humans off the account directory),
 *    own rows right-aligned;
 *  - the first level of the `@` is real here: the mention picker lists every
 *    participant, `@Agent` is a delivery target (it wakes), `@person` is a
 *    mention that wakes nothing (§5.3.1). The client resolves `@Name` text
 *    into the targets list — the Gateway never parses free text;
 *  - every send takes the intent channel: an un-addressed send is the
 *    positive instruction ADR-105 (default responder, or archive), a
 *    mention send fans out one turn per target. A block's in-place reply
 *    is the level-1 branch of the same entry;
 *  - the second level of the `@` opens per mentioned AGENT (the runs to
 *    quote as material are that agent's), same sheet as the 1:1 surface,
 *    `surface='group'`.
 *
 * Mobile deltas (§6.5): the picker and the member list are BOTTOM SHEETS
 * (no hover, no anchored popup); the transcript is not virtualized; no
 * upload in the composer yet (parity with the 1:1 mobile surface).
 *
 * Freshness: the same content-free `chat/changed` doorbell (main.tsx wakes
 * this window through `chatSessionStore.wake()` and re-reads the group list
 * through `projectGroupStore`); reading the window marks the session read
 * server-side, so the inbox badge clears on the next list re-read.
 */

import { Fragment, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { AtSign, Quote, Send, Users } from 'lucide-react'
import { useNavStore } from '../../stores/navStore'
import { useAuthStore } from '../../stores/authStore'
import { useAgentStore } from '../../stores/agentStore'
import { useChatStore } from '../../stores/chatStore'
import { useProjectGroupStore } from '../../stores/projectGroupStore'
import {
  useChatSessionStore,
  type ChatAddress,
} from '../../stores/chatSessionStore'
import { AuthImage } from '../../components/AuthImage'
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from '../../lib/builtinIcons'
import { daySeparator, timeGapSeparator } from '../../lib/time'
import { useStickToBottom } from '../../lib/stickToBottom'
import { Banner, ListRow, Sheet } from '../../components/ui'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import { AttachmentViewer } from '../../components/AttachmentViewer'
import { ChatAttachmentCapsule } from '../../components/ChatAttachmentCapsule'
import { ChatExecutionBlock } from '../../components/ChatExecutionBlock'
import { ChatNoticeRow } from '../../components/ChatNoticeRow'
import { DeliveryAlertLine } from '../../components/DeliveryAlertLine'
import { ChatAddressSheet, type AddressPick } from '../../components/ChatAddressSheet'
import {
  answerWithChoice,
  groupCardAddress,
  mayAnswerCard,
  type CardChoice,
} from '../../lib/chatCardAnswer'
import { fetchCardAnswer } from '../../lib/api'
import { turnInflight, resolveExpectedResponders } from '../../lib/chatTurnInflight'
import { buildTranscriptItems, type TranscriptItem } from '../../lib/chatTranscriptGroups'
import {
  applyMention,
  buildChatAddress,
  detectMention,
  filterMentionCandidates,
  resolveMentions,
  rowToTargets,
  type GroupAddressPick,
  type MentionCandidate,
} from '../../lib/groupMentions'
import { renderMarkdown } from '../../lib/markdown'
import type { ChatAttachment } from '../../lib/chatAttachment'
import type { ChatMessageRow } from '../../lib/types'

function pickIconId(seed: string): string | null {
  if (BUILTIN_ICON_IDS.length === 0 || !seed) return null
  let h = 0
  for (let i = 0; i < seed.length; i++) h = (seed.charCodeAt(i) + (h << 5) - h) | 0
  return BUILTIN_ICON_IDS[Math.abs(h) % BUILTIN_ICON_IDS.length] ?? null
}

function rowKeyOf(row: ChatMessageRow): string {
  return `${row.seq}-${row.meta?.message_id ?? ''}`
}

function itemOf(item: TranscriptItem): ChatMessageRow {
  return item.kind === 'run' ? item.anchorRow : item.row
}

interface ReplyTarget {
  id: string
  quote: string
}

/** What a participant id resolves to on this surface. */
interface ParticipantWho {
  name: string
  isAgent: boolean
  avatarUrl: string | null
  builtinAvatarId: string | null
}

export function GroupChatScreen() {
  const pop = useNavStore((s) => s.pop)
  const push = useNavStore((s) => s.push)
  const activeGroupId = useProjectGroupStore((s) => s.activeGroupId)
  const group = useProjectGroupStore((s) =>
    s.groups.find((g) => g.session_id === s.activeGroupId) ?? null,
  )
  const me = useAuthStore((s) => s.me)
  const baseUrl = useAuthStore((s) => s.baseUrl)
  const agents = useAgentStore((s) => s.agentList)
  const users = useAgentStore((s) => s.users)

  const win = useChatSessionStore((s) => (activeGroupId ? s.windows[activeGroupId] : undefined))
  const sending = useChatSessionStore((s) => (activeGroupId ? s.sending[activeGroupId] ?? false : false))
  const inflightTurn = useChatSessionStore((s) => (activeGroupId ? s.inflight[activeGroupId] : undefined))

  const [draft, setDraft] = useState('')
  const [replyTo, setReplyTo] = useState<ReplyTarget | null>(null)
  /** The mention picker's open state + the typed query it filters by. */
  const [picker, setPicker] = useState<{ query: string; start: number } | null>(null)
  const [pickerOpen, setPickerOpen] = useState(false)
  /** Per-target second-level picks (agent instance id -> what was chosen for
   *  this draft). Keyed by the instance, not the draft: several mentioned
   *  agents each carry their own address. */
  const [addresses, setAddresses] = useState<Record<string, GroupAddressPick>>({})
  /** The agent whose run list the second-level sheet is open for. */
  const [addressStage, setAddressStage] = useState<{ instanceId: string; name: string } | null>(null)
  const [focusBlockSessionId, setFocusBlockSessionId] = useState<string | null>(null)
  const [transientHint, setTransientHint] = useState<string | null>(null)
  const [membersOpen, setMembersOpen] = useState(false)
  const [viewing, setViewing] = useState<ChatAttachment | null>(null)
  const containerRef = useRef<HTMLDivElement | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement | null>(null)

  useEffect(() => {
    if (activeGroupId) void useChatSessionStore.getState().openGroup(activeGroupId)
  }, [activeGroupId])

  /* ── participant naming ───────────────────────────────────────────── */

  const nameOf = useCallback(
    (id: string): ParticipantWho => {
      const agent = agents.find((a) => a.id === id)
      if (agent) {
        return {
          name: agent.name || id,
          isAgent: true,
          avatarUrl: agent.avatar ? `${baseUrl}/api/agents/${encodeURIComponent(id)}/avatar-file?path=${encodeURIComponent(agent.avatar)}` : null,
          builtinAvatarId: agent.icon ?? null,
        }
      }
      if (me && id === me.user_id) {
        return {
          name: me.display_name || me.username || id.slice(0, 8),
          isAgent: false,
          avatarUrl: me.avatar ? `${baseUrl}/api/users/${encodeURIComponent(id)}/avatar-file?path=${encodeURIComponent(me.avatar)}` : null,
          builtinAvatarId: me.builtin_avatar ?? null,
        }
      }
      const user = users.find((u) => u.id === id)
      if (user) {
        return {
          name: user.display_name || id.slice(0, 8),
          isAgent: false,
          avatarUrl: user.avatar ? `${baseUrl}/api/users/${encodeURIComponent(id)}/avatar-file?path=${encodeURIComponent(user.avatar)}` : null,
          builtinAvatarId: user.builtin_avatar ?? null,
        }
      }
      return { name: id.slice(0, 8), isAgent: false, avatarUrl: null, builtinAvatarId: null }
    },
    [agents, users, me, baseUrl],
  )

  const participantAvatar = useCallback(
    (id: string): ReactNode => {
      const who = nameOf(id)
      const picked = pickIconId(who.name)
      const fallbackSrc =
        (who.builtinAvatarId && BUILTIN_ICONS[who.builtinAvatarId]) || BUILTIN_ICONS[picked ?? '']
      const fallbackEl = fallbackSrc ? <img src={fallbackSrc} alt="" /> : <span>{who.name.charAt(0)}</span>
      if (who.avatarUrl) return <AuthImage src={who.avatarUrl} fallback={fallbackEl} />
      return fallbackEl
    },
    [nameOf],
  )

  /* ── mention candidates ───────────────────────────────────────────── */

  /** The roster minus me. `identity` tags the entry when the server carried
   *  it; an untagged id is resolved against the local registry (same rule as
   *  Desktop's picker). */
  const candidates = useMemo<MentionCandidate[]>(() => {
    if (!group?.participants) return []
    return group.participants
      .filter((p) => p.id !== me?.user_id)
      .map((p) => {
        const who = nameOf(p.id)
        return { id: p.id, name: who.name, isAgent: p.identity ? p.identity === 'agent' : who.isAgent }
      })
      .filter((p) => p.name)
  }, [group, me, nameOf])

  /** Only agents become delivery targets (design 27 §5.3.1). */
  const agentParticipants = useMemo(() => candidates.filter((c) => c.isAgent), [candidates])

  const pickerRows = useMemo(
    () => filterMentionCandidates(candidates, picker?.query ?? ''),
    [candidates, picker],
  )

  /* ── the window ───────────────────────────────────────────────────── */

  const rows = useMemo(() => (win ? [...win.older, ...win.tail] : []), [win])
  const turnLive = turnInflight(rows, inflightTurn)
  const composerLocked = sending || turnLive

  // The turn header's face: a claim naming exactly ONE agent this device can
  // resolve wears that agent's name; anything else keeps the centered line
  // (ADR-106 revision 2026-11-08 — the same rule the 1:1 surface applies).
  const turnHeaderId: string | null = (() => {
    const ids = inflightTurn?.responders ?? []
    if (ids.length !== 1) return null
    const who = nameOf(ids[0]!)
    return who.isAgent ? ids[0]! : null
  })()

  useEffect(() => {
    if (!activeGroupId || !inflightTurn) return
    if (turnInflight(rows, inflightTurn)) return
    useChatSessionStore.getState().settleTurn(activeGroupId, inflightTurn.messageId)
  }, [rows, inflightTurn, activeGroupId])

  const layout = useMemo(() => buildTranscriptItems(rows, rowKeyOf), [rows])
  const items = layout.items
  const { ref: scrollRef, stickNow, atBottom } = useStickToBottom([rows.length, activeGroupId], activeGroupId, () => {
    if (activeGroupId) void useChatSessionStore.getState().loadOlder(activeGroupId)
  })

  // messageId → sender, for the `→` chip on an echo row (the row that answers
  // a round takes its counterparty from the row it continues).
  const senderById = useMemo(() => {
    const m = new Map<string, string>()
    for (const r of rows) if (r.meta?.message_id) m.set(r.meta.message_id, r.from)
    return m
  }, [rows])

  /* ── the write path ───────────────────────────────────────────────── */

  /** The one send entry of this surface. `targets` are the mentioned agents
   *  the text still carries; an empty list is the ADR-105 instruction, never
   *  a missing one. */
  const deliver = (
    text: string,
    opts: {
      inReplyTo?: string
      attachmentIds?: string[]
      address?: ChatAddress
      /** The level-1 reply's addressed row, as the window has it (`null` =
       *  not loaded — an unresolved fact, not an absent one). */
      repliedRow?: ChatMessageRow | null
    } = {},
  ) => {
    if (!activeGroupId) return
    const targets = opts.inReplyTo
      ? []
      : resolveMentions(text, agentParticipants)
    const responders = resolveExpectedResponders({
      mentions: targets,
      repliedRow: opts.inReplyTo ? opts.repliedRow : undefined,
      defaultResponder: group?.default_responder ?? null,
    })
    setDraft('')
    setReplyTo(null)
    setAddresses({})
    setPicker(null)
    setPickerOpen(false)
    setFocusBlockSessionId(null)
    stickNow()
    void useChatSessionStore
      .getState()
      .sendGroupText(activeGroupId, text, targets, opts.inReplyTo, opts.attachmentIds, opts.address, responders)
  }

  const send = () => {
    const text = draft.trim()
    if (!text || composerLocked || !activeGroupId) return
    // A level-1 reply already names its session; a picked address must not
    // ride that send (ADR-106 D6 — the two are contradictory instructions).
    const targets = resolveMentions(text, agentParticipants)
    const address = replyTo ? undefined : buildChatAddress(targets, addresses)
    const repliedRow = replyTo
      ? rows.find((r) => r.meta?.message_id === replyTo.id) ?? null
      : undefined
    deliver(text, { inReplyTo: replyTo?.id, address, repliedRow })
  }

  /** Answering an addressing card: re-deliver the human's OWN words, now
   *  addressed, and make sure the card's author is among the targets (the
   *  original message may have named nobody — ADR-105). */
  const answerCard = async (card: ChatMessageRow, choice: CardChoice) => {
    const cardId = card.meta?.message_id
    if (!cardId || !activeGroupId) return
    const lookup = await fetchCardAnswer(activeGroupId, cardId)
    if (!lookup.ok) {
      if (lookup.reason === 'gone') setTransientHint('这张卡片已无法回答')
      return
    }
    const answer = answerWithChoice(lookup.facts, choice)
    if (!answer) return
    const mentioned = resolveMentions(answer.text, agentParticipants)
    const targets = mentioned.includes(answer.agentId) ? mentioned : [...mentioned, answer.agentId]
    const addr = groupCardAddress(answer)
    setDraft('')
    setReplyTo(null)
    setAddresses({})
    stickNow()
    void useChatSessionStore
      .getState()
      .sendGroupText(
        activeGroupId,
        answer.text,
        targets,
        undefined,
        answer.attachmentIds,
        addr,
        resolveExpectedResponders({ mentions: targets }),
      )
  }

  /** A `pause` card's continue: address the composer at that round. */
  const replyToRound = (rowId: string, quote: string) => {
    setReplyTo({ id: rowId, quote })
    setAddresses({})
  }

  const handleReplyInPlace = (rowId: string, text: string) => {
    const repliedRow = rows.find((r) => r.meta?.message_id === rowId) ?? null
    deliver(text, { inReplyTo: rowId, repliedRow })
  }

  const handleLocateRef = (execSessionId: string) => {
    setAddressStage(null)
    const scope = containerRef.current
    if (!scope || !execSessionId) {
      setTransientHint('任务块不在已加载历史')
      return
    }
    const el = scope.querySelector<HTMLElement>(`[data-session$="|${execSessionId}"]`)
    if (!el) {
      setTransientHint('任务块不在已加载历史')
      return
    }
    el.scrollIntoView({ behavior: 'smooth', block: 'center' })
    setFocusBlockSessionId(execSessionId)
  }

  useEffect(() => {
    if (!transientHint) return
    const id = setTimeout(() => setTransientHint(null), 2600)
    return () => clearTimeout(id)
  }, [transientHint])

  const openExec = (instanceId: string, execSid: string) => {
    if (!instanceId || !execSid) return
    void useChatStore.getState().openSession(instanceId, execSid).then(() => push('chat/detail'))
  }

  /* ── composer: the `@` picker ─────────────────────────────────────── */

  const syncMention = (value: string, caret: number) => {
    const found = detectMention(value, caret)
    setPicker(found)
    if (found) setPickerOpen(true)
  }

  const openPicker = () => {
    if (pickerOpen) {
      setPickerOpen(false)
      setPicker(null)
      return
    }
    const ta = textareaRef.current
    const caret = ta?.selectionStart ?? draft.length
    setPicker({ query: '', start: caret })
    setPickerOpen(true)
  }

  const pickCandidate = (c: MentionCandidate) => {
    const ta = textareaRef.current
    const caret = ta?.selectionStart ?? draft.length
    const start = picker?.start ?? caret
    const next = applyMention(draft, caret, start, c.name)
    setDraft(next.value)
    setPicker(null)
    setPickerOpen(false)
    requestAnimationFrame(() => {
      ta?.focus()
      ta?.setSelectionRange(next.caret, next.caret)
    })
    // The second level belongs to an agent only: @-ing a person wakes nothing,
    // so asking "which of their tasks" would promise a delivery that cannot
    // happen.
    if (c.isAgent) setAddressStage({ instanceId: c.id, name: c.name })
  }

  // Chips follow the draft: an address whose `@` the human deleted stops
  // being an instruction, so it stops being shown (and being sent).
  const addressRows = useMemo(() => {
    const mentioned = new Set(resolveMentions(draft, agentParticipants))
    return Object.entries(addresses)
      .filter(([id]) => mentioned.has(id))
      .map(([id, pick]) => ({ instanceId: id, name: nameOf(id).name, pick }))
  }, [addresses, draft, agentParticipants, nameOf])

  if (!activeGroupId) {
    return (
      <EdgeSwipe onBack={() => pop()}>
        <div className="screen">
          <header className="navbar">
            <button type="button" className="navbar-back" onClick={() => pop()} aria-label="返回">返回</button>
            <span className="navbar-title">群聊</span>
            <span className="navbar-trail" />
          </header>
          <div className="empty-state">未选择群聊，请返回重试</div>
        </div>
      </EdgeSwipe>
    )
  }

  return (
    <EdgeSwipe onBack={() => pop()}>
      <div className="screen">
        <header className="navbar">
          <button type="button" className="navbar-back" onClick={() => pop()} aria-label="返回">返回</button>
          <span className="navbar-title">{group?.title ?? '群聊'}</span>
          <span className="navbar-trail">
            <button type="button" className="navbar-action" onClick={() => setMembersOpen(true)} aria-label="成员">
              成员
            </button>
          </span>
        </header>

        {!group ? <Banner tone="info">群聊信息加载中…</Banner> : null}

        <div
          className="scroll scroll-chat"
          ref={(el) => {
            containerRef.current = el
            if (typeof scrollRef === 'function') scrollRef(el)
          }}
        >
          {win?.loadingOlder ? <div className="time-sep">载入更早消息…</div> : null}
          {rows.length === 0 ? (
            <div className="empty-state">还没有消息，发一条开始群聊</div>
          ) : null}
          {items.map((item, i) => {
            const row = itemOf(item)
            const prev = i > 0 ? itemOf(items[i - 1]!) : null
            const sep = prev ? daySeparator(prev.ts, row.ts) ?? timeGapSeparator(prev.ts, row.ts) : null
            return (
              <Fragment key={rowKeyOf(row)}>
                {sep ? <div className="time-sep">{sep}</div> : null}
                <GroupItemView
                  item={item}
                  hasFollowUp={i < items.length - 1}
                  chatSessionId={activeGroupId}
                  meId={me?.user_id ?? ''}
                  nameOf={nameOf}
                  avatarOf={participantAvatar}
                  senderById={senderById}
                  onJumpToExec={openExec}
                  onReplyToRound={replyToRound}
                  onChooseCard={
                    item.kind === 'row' && mayAnswerCard(item.row, me?.user_id ?? '')
                      ? answerCard
                      : undefined
                  }
                  onOpenAttachment={setViewing}
                  onLocateRef={handleLocateRef}
                  focusBlockSessionId={focusBlockSessionId ?? undefined}
                  onReplyInPlace={handleReplyInPlace}
                />
              </Fragment>
            )
          })}
          {composerLocked ? (
            turnLive && turnHeaderId ? (
              <div className="chat-turn-header" role="status">
                <div className="msg-avatar">{participantAvatar(turnHeaderId)}</div>
                <div className="msg-sender">
                  <span className="msg-sender-name">{nameOf(turnHeaderId).name}</span>
                  <span className="chat-turn-header-status">正在处理…</span>
                </div>
              </div>
            ) : (
              <div className="chat-notice">
                <span className="chat-notice-pill">正在处理…</span>
              </div>
            )
          ) : null}
          {atBottom === false && rows.length > 0 ? (
            <button type="button" className="chat-scroll-pill" onClick={stickNow} aria-label="回到底部">
              回到底部 ↓
            </button>
          ) : null}
        </div>

        {replyTo || addressRows.length > 0 ? (
          <div className="composer-chips">
            {replyTo ? (
              <span className="composer-chip">
                <span className="composer-chip-text">回复 {replyTo.quote.slice(0, 30)}</span>
                <button type="button" className="composer-chip-clear" onClick={() => setReplyTo(null)} aria-label="取消回复">
                  ×
                </button>
              </span>
            ) : null}
            {addressRows.map(({ instanceId, name, pick }) => (
              <span key={instanceId} className="composer-chip">
                <span className="composer-chip-text">
                  @{name}
                  {pick.forceNew ? ' · 开新任务' : pick.refs?.length ? ` · 引用 ${pick.refs.length} 个任务` : ''}
                </span>
                <button
                  type="button"
                  className="composer-chip-clear"
                  onClick={() =>
                    setAddresses((prev) => {
                      const next = { ...prev }
                      delete next[instanceId]
                      return next
                    })
                  }
                  aria-label="移除寻址"
                >
                  ×
                </button>
              </span>
            ))}
          </div>
        ) : null}

        {transientHint ? <div className="composer-locate-hint" role="status">{transientHint}</div> : null}

        {pickerOpen && pickerRows.length > 0 ? (
          <div className="group-mention-picker" role="listbox" aria-label="选择提及对象">
            {pickerRows.map((c) => (
              <button key={c.id} type="button" role="option" aria-selected="false" className="group-mention-row" onClick={() => pickCandidate(c)}>
                <span className="group-mention-avatar">{participantAvatar(c.id)}</span>
                <span className="group-mention-name">{c.name}</span>
                {c.isAgent ? <span className="group-mention-tag">Agent</span> : null}
              </button>
            ))}
          </div>
        ) : null}

        <div className="composer">
          <button
            type="button"
            className="composer-address"
            aria-label="提及成员"
            aria-expanded={pickerOpen}
            disabled={!group || candidates.length === 0}
            onClick={openPicker}
          >
            <AtSign className="composer-send-icon" aria-hidden />
          </button>
          <textarea
            ref={textareaRef}
            className="composer-input"
            rows={1}
            placeholder="发送消息（@ 提及成员）"
            aria-label="消息输入框"
            value={draft}
            disabled={composerLocked}
            onChange={(e) => {
              setDraft(e.target.value)
              syncMention(e.target.value, e.target.selectionStart ?? e.target.value.length)
            }}
          />
          <button
            type="button"
            className="composer-send"
            aria-label="发送"
            title={turnLive ? '上一条消息正在处理中' : '发送'}
            disabled={!draft.trim() || composerLocked || !activeGroupId}
            onClick={() => void send()}
          >
            <Send className="composer-send-icon" aria-hidden />
          </button>
        </div>

        {addressStage && activeGroupId ? (
          <ChatAddressSheet
            instanceId={addressStage.instanceId}
            chatSessionId={activeGroupId}
            surface="group"
            onPick={(pick: AddressPick) => {
              setAddresses((prev) => {
                const next = { ...prev }
                if (pick.refs?.length || pick.forceNew) {
                  next[addressStage.instanceId] = pick
                } else {
                  delete next[addressStage.instanceId]
                }
                return next
              })
              setAddressStage(null)
            }}
            onLocate={(sid) => handleLocateRef(sid)}
            onClose={() => setAddressStage(null)}
          />
        ) : null}

        {membersOpen ? (
          <Sheet open onClose={() => setMembersOpen(false)} title={`成员 · ${group?.participants?.length ?? 0}`}>
            {(group?.participants ?? []).map((p) => {
              const who = nameOf(p.id)
              return (
                <ListRow
                  key={p.id}
                  avatar={participantAvatar(p.id)}
                  label={
                    <span className="row-title">
                      {who.name}
                      {who.isAgent ? <span className="group-mention-tag">Agent</span> : null}
                    </span>
                  }
                  hint={p.id === me?.user_id ? '我' : undefined}
                />
              )
            })}
            {group && !group.participants?.length ? <div className="chat-sheet-hint">成员名单加载中</div> : null}
            <div className="chat-sheet-hint">
              <Users style={{ width: 12, height: 12, verticalAlign: -2, marginRight: 4 }} aria-hidden />
              群成员随项目成员同步；增删成员请在桌面端项目页操作。
            </div>
          </Sheet>
        ) : null}

        {viewing ? (
          <AttachmentViewer chatSessionId={activeGroupId} attachment={viewing} onClose={() => setViewing(null)} />
        ) : null}
      </div>
    </EdgeSwipe>
  )
}

/* ------------------------------------------------------------------ */
/* Row rendering — the multi-party transcript                          */
/* ------------------------------------------------------------------ */

interface GroupRowProps {
  row: ChatMessageRow
  chatSessionId: string
  meId: string
  nameOf: (id: string) => ParticipantWho
  avatarOf: (id: string) => ReactNode
  senderById: Map<string, string>
  onJumpToExec: (agentInstanceId: string, execSid: string) => void
  onReplyToRound: (rowId: string, quote: string) => void
  onChooseCard?: (card: ChatMessageRow, choice: CardChoice) => void
  onOpenAttachment: (attachment: ChatAttachment) => void
  onLocateRef?: (sessionId: string) => void
  focusBlockSessionId?: string
  onReplyInPlace?: (rowId: string, text: string) => void
}

function GroupRowView(props: GroupRowProps) {
  const { row, meId, nameOf, avatarOf, senderById, onChooseCard, onOpenAttachment, onLocateRef } = props

  if (row.kind === 'execution_round') {
    return (
      <div className="chat-notice">
        <span className="chat-notice-pill">本轮执行已不可用</span>
      </div>
    )
  }

  if (row.kind === 'system_notice') {
    const noticeType = row.meta?.notice_type
    const choose =
      (noticeType === 'session_choice' || noticeType === 'confirm_continue') && onChooseCard
        ? (choice: CardChoice) => onChooseCard(row, choice)
        : undefined
    return <ChatNoticeRow row={row} onChoose={choose} />
  }

  if (row.kind === 'interaction_request') return null

  if (row.kind === 'user_message' || row.kind === 'agent_message') {
    const isSelf = row.from === meId
    const who = nameOf(row.from)
    // Who this row is addressed to: the mentions the sender named, the `to`
    // the write carried, or — for an echo answering a row — that row's
    // sender. Empty = the row carries no address, no chip.
    const triggerFrom = row.meta?.in_reply_to ? senderById.get(row.meta.in_reply_to) : undefined
    const toTargets = rowToTargets(row, triggerFrom)
    return (
      <div className={`msg-group${isSelf ? ' msg-group-self' : ' msg-group-peer'}`}>
        <div className="msg-header">
          <div className="msg-avatar">{avatarOf(row.from)}</div>
          <div className="msg-sender">
            <span className="msg-sender-name">{who.name}</span>
          </div>
        </div>
        <div className="msg-stack">
          {toTargets.length > 0 && !isSelf ? (
            <div className="chat-ref-chips">
              {toTargets.map((id) => (
                <span key={id} className="chat-ref-chip">
                  <ArrowTo />
                  <span className="chat-ref-chip-text">{nameOf(id).name}</span>
                </span>
              ))}
            </div>
          ) : null}
          <div className={`bubble${isSelf ? ' bubble-self' : ' bubble-peer'}`}>
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
            {row.meta?.exec_attachments && row.meta.exec_attachments.length > 0 ? (
              <div className="chat-attachments">
                {row.meta.exec_attachments.map((l, i) => (
                  <span key={`exec-${i}-${l.filename}`} className="chat-attachment-chip chat-attachment-label">
                    {l.filename}
                  </span>
                ))}
              </div>
            ) : null}
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
            {row.delivery?.alert ? (
              <DeliveryAlertLine alert={row.delivery.alert} align={isSelf ? 'end' : 'start'} />
            ) : null}
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="chat-notice">
      <span className="chat-notice-pill">{row.body}</span>
    </div>
  )
}

/** The `→` glyph of the addressed-to chip, kept inline so the chip reads
 *  "to 名字" without pulling another icon import into the row. */
function ArrowTo() {
  return <span aria-hidden style={{ fontSize: 11, lineHeight: 1 }}>→</span>
}

interface GroupItemProps extends Omit<GroupRowProps, 'row'> {
  item: TranscriptItem
  hasFollowUp: boolean
}

export function GroupItemView({ item, hasFollowUp, ...rowProps }: GroupItemProps) {
  if (item.kind === 'run') {
    const who = rowProps.nameOf(item.instanceId)
    return (
      <ChatExecutionBlock
        block={item}
        chatSessionId={rowProps.chatSessionId}
        showHeader
        hasFollowUp={hasFollowUp}
        agentAvatar={rowProps.avatarOf(item.instanceId)}
        agentName={who.name}
        onJumpToExec={rowProps.onJumpToExec}
        onReplyToRound={rowProps.onReplyToRound}
        focusInput={!!rowProps.focusBlockSessionId && item.execSessionId === rowProps.focusBlockSessionId}
        onReplyInPlace={rowProps.onReplyInPlace}
      />
    )
  }
  return <GroupRowView {...rowProps} row={item.row} />
}
