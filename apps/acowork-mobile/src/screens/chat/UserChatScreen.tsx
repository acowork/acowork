/**
 * A 1:1 thread with another human (design §2.3, ADR-076 §决策 8).
 *
 * Same visual grammar as the agent thread — turn grouping, avatar + name
 * header, bubbles, composer, edge-swipe back — but a much shorter list of
 * concerns: there is no session to switch, no `can_write` gate, no live
 * status, no approval card, and no tool/explore blocks. A human chat is a
 * flat log, so a "turn" here is just consecutive messages from the same
 * sender; the avatar and name render ONCE per turn and the bubbles stack
 * tightly beneath it (desktop cohesion).
 *
 * Freshness is push-first: the background channel subscribes
 * `acowork/users/{me}/chat/changed` and every signal re-reads this thread under
 * the store's `planPage` rule (design doc 25 §8). A 5 s poll only runs while that
 * subscribe is unavailable, plus an immediate refresh when the app returns to the
 * foreground (§11.3). Either way, a refresh never moves the viewport: see
 * `lib/stickToBottom.ts`.
 */

import { Fragment, useEffect, useState, type ReactNode } from 'react'
import { Send } from 'lucide-react'
import { useNavStore } from '../../stores/navStore'
import { useUserChatStore, setUserChatThreadOnScreen } from '../../stores/userChatStore'
import { useAuthStore } from '../../stores/authStore'
import { useAgentStore } from '../../stores/agentStore'
import { Bubble, toMs } from '../../components/Bubble'
import { AuthImage } from '../../components/AuthImage'
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from '../../lib/builtinIcons'
import { daySeparator, timeGapSeparator } from '../../lib/time'
import { useStickToBottom } from '../../lib/stickToBottom'
import { Banner, ListRow } from '../../components/ui'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import type { ChatMessage } from '../../lib/types'

/** Deterministic builtin icon picker (same algorithm as ChatListScreen). */
function pickIconId(seed: string): string | null {
  if (BUILTIN_ICON_IDS.length === 0 || !seed) return null
  let h = 0
  for (let i = 0; i < seed.length; i++) h = (seed.charCodeAt(i) + (h << 5) - h) | 0
  return BUILTIN_ICON_IDS[Math.abs(h) % BUILTIN_ICON_IDS.length] ?? null
}

export function UserChatScreen() {
  const peer = useUserChatStore((s) => s.activePeerId)
  const peerName = useUserChatStore((s) => s.activePeerName)
  const raw = useUserChatStore((s) => s.messages)
  const loading = useUserChatStore((s) => s.loading)
  const sending = useUserChatStore((s) => s.sending)
  const error = useUserChatStore((s) => s.error)
  const send = useUserChatStore((s) => s.send)
  const unseen = useUserChatStore((s) => s.unseenCount)
  const markSeen = useUserChatStore((s) => s.markSeen)
  const pop = useNavStore((s) => s.pop)
  const me = useAuthStore((s) => s.me)
  const baseUrl = useAuthStore((s) => s.baseUrl)
  const peerUser = useAgentStore((s) => (peer ? s.users.find((u) => u.id === peer) : undefined))

  const [draft, setDraft] = useState('')

  // A human message and an agent message render through the same bubble, so
  // the wire shape is lifted to the shared one here, not in the store.
  const messages: ChatMessage[] = raw.map((m, i) => ({
    id: `${m.ts}-${i}`,
    role: m.from === me?.user_id ? 'user' : 'assistant',
    content: m.body,
    created_at: toMs(m.ts),
    kind: 'text',
  }))

  // Follow rule: an arriving message must not hijack the viewport — it only
  // scrolls while the user is at the bottom. Switching peer re-pins; so does
  // sending (`stickNow`). Arriving while pinned means the reader is looking at
  // the spot it landed, so the badge is cleared on that render; arriving while
  // unpinned leaves it for the pill, and `onBecamePinned` clears it the moment
  // they scroll down to meet their messages (a scroll is not a render, so the
  // controller has to report it).
  const { ref: scrollRef, stickNow, isPinned } = useStickToBottom(
    [messages.length],
    peer,
    undefined,
    () => void markSeen(),
  )

  useEffect(() => {
    setUserChatThreadOnScreen(true)
    return () => setUserChatThreadOnScreen(false)
  }, [])

  useEffect(() => {
    if (unseen > 0 && isPinned()) void markSeen()
  }, [unseen, messages.length, isPinned, markSeen])

  const submit = async () => {
    const text = draft.trim()
    if (!text || sending) return
    stickNow()
    if (await send(text)) setDraft('')
  }

  // Avatars mirror ChatDetailScreen: custom file → declared builtin →
  // deterministic builtin → initial. There is no per-user avatar-file route
  // for a peer, so the AuthImage fetch falls back to the builtin on failure.
  const myName = me?.display_name || me?.username || '我'
  const meAvatar: ReactNode = (() => {
    if (!me) return <span>{'?'}</span>
    const picked = pickIconId(myName)
    const fallbackSrc = me.builtin_avatar && BUILTIN_ICONS[me.builtin_avatar]
      ? BUILTIN_ICONS[me.builtin_avatar]
      : BUILTIN_ICONS[picked ?? '']
    const fallbackEl = fallbackSrc ? <img src={fallbackSrc} alt="" /> : <span>{myName.charAt(0)}</span>
    if (me.avatar) {
      const url = `${baseUrl}/api/users/${encodeURIComponent(me.user_id)}/avatar-file?path=${encodeURIComponent(me.avatar)}`
      return <AuthImage src={url} fallback={fallbackEl} />
    }
    return fallbackEl
  })()
  const peerDisplayName = peerUser?.display_name || peerName || peer || ''
  const peerAvatar: ReactNode = (() => {
    const picked = pickIconId(peerDisplayName)
    const fallbackSrc = peerUser?.builtin_avatar && BUILTIN_ICONS[peerUser.builtin_avatar]
      ? BUILTIN_ICONS[peerUser.builtin_avatar]
      : BUILTIN_ICONS[picked ?? '']
    const fallbackEl = fallbackSrc
      ? <img src={fallbackSrc} alt="" />
      : <span>{(peerDisplayName || '?').charAt(0)}</span>
    if (peerUser?.avatar && peer) {
      const url = `${baseUrl}/api/users/${encodeURIComponent(peer)}/avatar-file?path=${encodeURIComponent(peerUser.avatar)}`
      return <AuthImage src={url} fallback={fallbackEl} />
    }
    return fallbackEl
  })()

  if (!peer) {
    // Deep-linked or the store was cleared (logout): say so, never spin.
    return (
      <EdgeSwipe onBack={() => pop()}>
        <div className="screen">
          <header className="navbar">
            <button type="button" className="navbar-back" onClick={() => pop()} aria-label="返回">返回</button>
            <span className="navbar-title">联系人</span>
            <span className="navbar-trail" />
          </header>
          <div className="empty-state">该联系人会话已关闭，请返回重试</div>
        </div>
      </EdgeSwipe>
    )
  }

  return (
    <EdgeSwipe onBack={() => pop()}>
      <div className="screen">
        <header className="navbar">
          <button type="button" className="navbar-back" onClick={() => pop()} aria-label="返回">返回</button>
          <span className="navbar-title">{peerName ?? peer}</span>
          <span className="navbar-trail" />
        </header>

        {!me ? (
          <Banner tone="error">
            <ListRow label="无法确定当前账号" hint="退出登录后重新登录即可恢复" />
          </Banner>
        ) : null}
        {error ? <Banner tone="warning">{error}</Banner> : null}

        <div className="scroll scroll-chat" ref={scrollRef}>
          {loading ? <div className="empty-state">载入中…</div> : null}
          {!loading && messages.length === 0 ? <div className="empty-state">还没有消息</div> : null}
          {messages.length
            ? (() => {
                // Fold consecutive same-sender messages into one turn (the
                // agent thread's cohesion model): the avatar + name render
                // once, the bubbles stack tightly. A sender change, day
                // change or >5-min gap breaks a turn.
                type Turn = { isUser: boolean; items: ChatMessage[]; firstTs: number; prevTs: number | null }
                const turns: Turn[] = []
                messages.forEach((m, i) => {
                  const isUser = m.role === 'user'
                  const prev = i > 0 ? messages[i - 1]! : null
                  const prevTs = prev ? prev.created_at : null
                  const breakHere = !prev
                    || prev.role !== m.role
                    || !!daySeparator(prevTs, m.created_at)
                    || !!timeGapSeparator(prevTs, m.created_at)
                  if (!breakHere) {
                    turns[turns.length - 1]!.items.push(m)
                  } else {
                    turns.push({ isUser, items: [m], firstTs: m.created_at, prevTs })
                  }
                })
                return turns.map((turn) => {
                  const sep = daySeparator(turn.prevTs, turn.firstTs) ?? timeGapSeparator(turn.prevTs, turn.firstTs)
                  const avatar = turn.isUser ? meAvatar : peerAvatar
                  const name = turn.isUser ? myName : peerDisplayName
                  return (
                    <Fragment key={turn.items[0]!.id}>
                      {sep ? <div className="time-sep">{sep}</div> : null}
                      <div className={`msg-group${turn.isUser ? ' msg-group-self' : ' msg-group-peer'}`}>
                        <div className="msg-avatar">{avatar}</div>
                        <div className="msg-main">
                          <div className="msg-sender">
                            <span className="msg-sender-name">{name}</span>
                          </div>
                          {turn.items.map((m) => (
                            <Bubble key={m.id} msg={m} />
                          ))}
                        </div>
                      </div>
                    </Fragment>
                  )
                })
              })()
            : null}
          {/* Unread arrivals while the reader was scrolled up: counted, never
              scrolled to. Tapping is the explicit act that re-claims the
              follow — the same convention as the peer IM apps. */}
          {unseen > 0 ? (
            <button
              type="button"
              className="chat-newpill"
              onClick={() => {
                stickNow()
                void markSeen()
              }}
            >
              {unseen} 条新消息
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
            disabled={!draft.trim() || sending || !me}
            onClick={() => void submit()}
          >
            <Send className="composer-send-icon" aria-hidden />
          </button>
        </div>
      </div>
    </EdgeSwipe>
  )
}
