/**
 * Chat detail — the busiest screen in the app, and the one carrying the
 * two invariants that are easiest to get wrong.
 *
 * 1. MULTI-SESSION (ADR-086). An agent has many sessions and the user
 *    switches between them constantly, so the switcher is the navigation
 *    bar TITLE, not a tab strip: a horizontal tab bar cannot fit on a
 *    390pt screen once real session titles are shown, and it is not the
 *    IM idiom. Tapping the title opens an Action Sheet listing every
 *    session.
 *
 * 2. READ-ONLY (ADR-076 §决策 4). `can_write === false` renders a banner,
 *    removes the composer, and disables the visibility control — the
 *    control stays visible and disabled rather than disappearing, so the
 *    user can see the session is shared and understand why they cannot
 *    post. The permission is never re-derived from `visibility`.
 *
 * 3. FOREGROUND POLLING (v1.1 §8). The screen owns the poll loop's
 *    lifetime: start on mount / session switch, stop on unmount. Only the
 *    session on screen is ever polled; results appear as complete messages
 *    when the session returns to idle — no streaming, ever, in v1.
 */

import { Fragment, useCallback, useEffect, useState, type ReactNode } from 'react'
import { Clock } from 'lucide-react'
import { useAgentStore } from '../../stores/agentStore'
import { isActiveStatus, useChatStore } from '../../stores/chatStore'
import { useNavStore } from '../../stores/navStore'
import { useAuthStore } from '../../stores/authStore'
import { useSessionReadOnly } from '../../lib/session-write-access'
import { setSessionVisibility } from '../../lib/api'
import { useForegroundRefresh } from '../../lib/foreground'
import { useStickToBottom } from '../../lib/stickToBottom'
import { Banner, ListRow, Sheet } from '../../components/ui'
import { Bubble } from '../../components/Bubble'
import { ExploreBlock } from '../../components/ExploreBlock'
import { AuthImage } from '../../components/AuthImage'
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from '../../lib/builtinIcons'
import { daySeparator, timeGapSeparator } from '../../lib/time'
import { AgentDrawer } from './AgentDrawer'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import type { ChatMessage, LiveStatus } from '../../lib/types'

/** Deterministic builtin icon picker (same algorithm as ChatListScreen). */
function pickIconId(seed: string): string | null {
  if (BUILTIN_ICON_IDS.length === 0 || !seed) return null
  let h = 0
  for (let i = 0; i < seed.length; i++) h = (seed.charCodeAt(i) + (h << 5) - h) | 0
  return BUILTIN_ICON_IDS[Math.abs(h) % BUILTIN_ICON_IDS.length] ?? null
}

/** Fold consecutive tool_call / tool_result / thought messages into groups. */
type RenderItem = { type: 'msg'; msg: ChatMessage } | { type: 'explore'; items: ChatMessage[] }
function foldToolCalls(messages: ChatMessage[]): RenderItem[] {
  const out: RenderItem[] = []
  let toolBuffer: ChatMessage[] = []
  const isProcess = (t: string | undefined) =>
    t === 'tool_call' || t === 'tool_result' || t === 'thought'
  const flush = () => {
    if (toolBuffer.length > 0) {
      out.push({ type: 'explore', items: toolBuffer })
    }
    toolBuffer = []
  }
  for (const m of messages) {
    const payload = (m.payload ?? {}) as { type?: string }
    if (isProcess(payload.type)) {
      toolBuffer.push(m)
    } else {
      flush()
      out.push({ type: 'msg', msg: m })
    }
  }
  flush()
  return out
}

/** §8: one status line, not a token stream. */
function statusText(s: LiveStatus): string {
  switch (s.status) {
    case 'llm_awaiting_first_chunk':
      return '正在思考…'
    case 'thinking':
      return '正在思考…'
    case 'llm_streaming':
      return '正在生成回复…'
    case 'tool_executing':
      return '正在执行工具…'
    case 'waiting_approval':
      return '等待审批：工具执行'
    case 'paused':
      return '已暂停 — 请在桌面端继续'
    case 'errored':
      return `出错：${(s as { detail?: { message?: string } }).detail?.message ?? '会话异常终止'}`
    default:
      return ''
  }
}

export function ChatDetailScreen() {
  const agentId = useChatStore((s) => s.selectedAgentId)
  const state = useChatStore((s) => (agentId ? s.agentStates[agentId] : undefined))
  const openSession = useChatStore((s) => s.openSession)
  const createSession = useChatStore((s) => s.createSession)
  const startWatching = useChatStore((s) => s.startWatching)
  const stopWatching = useChatStore((s) => s.stopWatching)
  const sendMessage = useChatStore((s) => s.sendMessage)
  const decideApproval = useChatStore((s) => s.decideApproval)
  const answerQuestion = useChatStore((s) => s.answerQuestion)
  const pop = useNavStore((s) => s.pop)
  const deleteSession = useChatStore((s) => s.deleteSession)
  const loadMoreSessions = useAgentStore((s) => s.loadMoreSessions)
  const sessionsHasMore = useAgentStore((s) => (agentId ? s.sessionsHasMore[agentId] ?? true : true))

  const agent = useAgentStore((s) => (agentId ? s.agents[agentId]?.info : undefined))
  const sessions = useAgentStore((s) => (agentId ? s.agents[agentId]?.sessions : undefined)) ?? []
  const activeId = state?.activeSessionId ?? null
  const readOnly = useSessionReadOnly(agentId, activeId)
  const baseUrl = useAuthStore((s) => s.baseUrl)
  const me = useAuthStore((s) => s.me)

  // Build avatar nodes for bubbles
  const agentAvatar: ReactNode = (() => {
    if (!agent) return <span>{'?'}</span>
    const picked = pickIconId(agent.name)
    const fallbackSrc = (agent.icon && BUILTIN_ICONS[agent.icon]) ? BUILTIN_ICONS[agent.icon] : BUILTIN_ICONS[picked ?? '']
    const fallbackEl = fallbackSrc ? <img src={fallbackSrc} alt="" /> : <span>{agent.name.charAt(0)}</span>
    if (agent.avatar && agentId) {
      const url = `${baseUrl}/api/agents/${encodeURIComponent(agentId)}/avatar-file?path=${encodeURIComponent(agent.avatar)}`
      return <AuthImage src={url} fallback={fallbackEl} />
    }
    return fallbackEl
  })()
  const userAvatar: ReactNode = (() => {
    if (!me) return <span>{'?'}</span>
    const name = me.display_name || me.username
    const picked = pickIconId(name)
    const fallbackSrc = BUILTIN_ICONS[picked ?? '']
    const fallbackEl = fallbackSrc ? <img src={fallbackSrc} alt="" /> : <span>{name.charAt(0)}</span>
    if (me.avatar) {
      const url = `${baseUrl}/api/users/${encodeURIComponent(me.user_id)}/avatar-file?path=${encodeURIComponent(me.avatar)}`
      return <AuthImage src={url} fallback={fallbackEl} />
    }
    return fallbackEl
  })()

  const [switcherOpen, setSwitcherOpen] = useState(false)
  const [drawerOpen, setDrawerOpen] = useState(false)
  /** The session whose manage sheet is open — actions bind to it, not to the
   *  active one, so deleting row A never silently deletes the open row B. */
  const [manageId, setManageId] = useState<string | null>(null)
  const [draft, setDraft] = useState('')
  const active = sessions.find((s) => s.session_id === activeId)
  // Follow rule: a refresh only scrolls to the bottom while the user is
  // already at it. A session switch (forceKey) re-pins, and so does a send
  // (`stickNow` below) — scrolling up must survive the 2 s snapshot poll.
  const { ref: scrollRef, stickNow } = useStickToBottom(
    [state?.messages.length, state?.status],
    activeId,
  )

  // §8 watch lifetime: exactly the foreground session, exactly while on screen.
  useEffect(() => {
    if (agentId && activeId) startWatching(agentId, activeId)
    return () => stopWatching()
  }, [agentId, activeId, startWatching, stopWatching])

  // §11.3: iOS freezes timers in the background, so the data on screen is
  // simply old on return. Reload rather than waiting for the next tick — and
  // go through `openSession`, the one path that re-points the Runtime and
  // re-reads history atomically, instead of a partial refresh that could
  // leave the two halves out of step.
  useForegroundRefresh(
    useCallback(() => {
      if (!agentId) return
      void useAgentStore.getState().refreshSessions(agentId)
      if (activeId) void openSession(agentId, activeId)
    }, [agentId, activeId, openSession]),
  )

  const onBack = useCallback(() => {
    stopWatching()
    if (!pop()) return
  }, [pop, stopWatching])

  const send = async () => {
    const text = draft.trim()
    if (!text || !agentId || !activeId || readOnly) return
    stickNow()
    const ok = await sendMessage(agentId, activeId, text)
    if (ok) setDraft('')
  }

  const approvalRequestId =
    state?.status?.status === 'waiting_approval'
      ? state.status.detail?.request_id || state?.approvalDetail?.requestId
      : undefined
  const approval = state?.approvalDetail && approvalRequestId === state.approvalDetail.requestId ? state.approvalDetail : null
  const question = state?.question ?? null
  const [answerDraft, setAnswerDraft] = useState('')

  // No session yet (first open of a fresh agent): create on first send.
  const sendFirst = async () => {
    const text = draft.trim()
    if (!text || !agentId) return
    stickNow()
    const sid = await createSession(agentId)
    if (!sid) return
    const ok = await sendMessage(agentId, sid, text)
    if (ok) setDraft('')
  }

  return (
    <EdgeSwipe onBack={onBack} onDrawer={() => setDrawerOpen(true)}>
      <div className="screen">
        {/* Three balanced zones: back (left) · centered title · session-history
            (right). The switcher is triggered by the right-hand history icon
            (Desktop's SessionTabBar uses the same clock glyph), not the title,
            so the title reads as a label and the bar has clear thirds. */}
        <header className="navbar navbar--chat">
          <button type="button" className="navbar-back" onClick={onBack} aria-label="返回">
            <svg width="11" height="18" viewBox="0 0 12 20" fill="none" style={{ display: 'block' }}>
              <path d="M10 2L2 10l8 8" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"/>
            </svg>
          </button>
          <span className="navbar-title">{active?.title ?? agent?.name ?? '会话'}</span>
          <button
            type="button"
            className="navbar-history"
            onClick={() => setSwitcherOpen(true)}
            aria-label="会话历史"
          >
            <Clock size={22} aria-hidden />
          </button>
        </header>

        {readOnly ? (
          <Banner tone="info">
            只读 · 来自 {active?.owner_id ?? '其他用户'} 的共享会话
          </Banner>
        ) : null}

        <div className="scroll scroll-chat" ref={scrollRef}>
          {state?.loading ? <div className="empty-state">载入中…</div> : null}
          {state?.loaded && state.messages.length === 0 && !isActiveStatus(state.status) ? (
            <div className="empty-state">还没有消息</div>
          ) : null}
          {state?.messages.length ? (() => {
            const folded = foldToolCalls(state.messages)
            const userName = me?.display_name || me?.username || '我'
            const lastMsgOf = (it: RenderItem): ChatMessage =>
              it.type === 'msg' ? it.msg : it.items[it.items.length - 1]!
            // Desktop cohesion: fold consecutive same-role items into one
            // "turn". The avatar + name/role header render ONCE per turn and
            // the turn's bubbles / explore blocks stack tightly inside it. A
            // role change, day change or >5-min gap breaks a turn (so the
            // avatar re-appears for the new turn).
            type Turn = { isUser: boolean; items: RenderItem[]; firstTs: number; prevTs: number | null; key: string }
            const turns: Turn[] = []
            folded.forEach((item, i) => {
              const firstMsg = item.type === 'msg' ? item.msg : item.items[0]!
              const isUser = firstMsg.role === 'user'
              const prevMsg = i > 0 ? lastMsgOf(folded[i - 1]!) : null
              const prevTs = prevMsg ? prevMsg.created_at : null
              const breakHere = !prevMsg
                || prevMsg.role !== firstMsg.role
                || !!daySeparator(prevTs, firstMsg.created_at)
                || !!timeGapSeparator(prevTs, firstMsg.created_at)
              if (!breakHere) {
                turns[turns.length - 1]!.items.push(item)
              } else {
                turns.push({
                  isUser,
                  items: [item],
                  firstTs: firstMsg.created_at,
                  prevTs,
                  key: item.type === 'explore' ? `explore-${item.items[0]!.id}` : item.msg.id,
                })
              }
            })
            return turns.map((turn) => {
              const sep = daySeparator(turn.prevTs, turn.firstTs) ?? timeGapSeparator(turn.prevTs, turn.firstTs)
              const avatar = turn.isUser ? userAvatar : agentAvatar
              const name = turn.isUser ? userName : (agent?.name ?? '助手')
              const role = turn.isUser ? null : (agent?.role ?? null)
              return (
                <Fragment key={turn.key}>
                  {sep ? <div className="time-sep">{sep}</div> : null}
                  <div className={`msg-group${turn.isUser ? ' msg-group-self' : ' msg-group-peer'}`}>
                    <div className="msg-avatar">{avatar}</div>
                    <div className="msg-main">
                      <div className="msg-sender">
                        <span className="msg-sender-name">{name}</span>
                        {role ? <span className="msg-sender-role">{role}</span> : null}
                      </div>
                      {turn.items.map((item) =>
                        item.type === 'explore'
                          ? <ExploreBlock key={`explore-${item.items[0]!.id}`} items={item.items} />
                          : <Bubble key={item.msg.id} msg={item.msg} />,
                      )}
                    </div>
                  </div>
                </Fragment>
              )
            })
          })() : null}
          {/* Failed draft: kept visible with an explicit retry (edge states). */}
          {state?.pendingSend ? (
            <div className="msg-group msg-group-self">
              <div className="msg-avatar">{userAvatar}</div>
              <div className="msg-main">
                <div
                  className={`bubble bubble-self bubble-raw bubble-failed${state.pendingSend.state === 'sending' ? ' is-sending' : ''}`}
                >
                  {state.pendingSend.content}
                </div>
                {state.pendingSend.state === 'failed' ? (
                  <button
                    type="button"
                    className="retry-link"
                    onClick={() => agentId && activeId && void sendMessage(agentId, activeId)}
                  >
                    发送失败 · 重试
                  </button>
                ) : null}
              </div>
            </div>
          ) : null}
          {/* v1.1 §8: status line instead of streaming output. */}
          {state?.status && isActiveStatus(state.status) ? (
            <div className="status-line">
              <span className="status-pulse" aria-hidden />
              <span>{statusText(state.status)}</span>
            </div>
          ) : null}
          {/* Approval card: WS channel carries tool details (§8.1); the
              polling channel only knows a request is pending (§8.3). */}
          {!readOnly && approvalRequestId && agentId && activeId ? (
            <div className="list-section" role="group" aria-label="工具审批">
              <ListRow
                label={<span className="row-title">{approval ? `允许执行 ${approval.toolName}？` : '允许该工具执行？'}</span>}
                hint={approval ? approval.action || approval.riskLevel || '工具详情' : '工具详情请在桌面端查看'}
                value={
                  <span style={{ display: 'inline-flex', gap: 8 }}>
                    <button
                      type="button"
                      className="retry-link"
                      style={{ color: 'var(--color-tint)' }}
                      onClick={() => void decideApproval(agentId, activeId, approvalRequestId, false)}
                    >
                      拒绝
                    </button>
                    <button
                      type="button"
                      className="retry-link"
                      style={{ color: 'var(--color-success)' }}
                      onClick={() => void decideApproval(agentId, activeId, approvalRequestId, true)}
                    >
                      允许
                    </button>
                  </span>
                }
              />
            </div>
          ) : null}
          {/* AskQuestion card — WS channel only (polling cannot see it, §8.3). */}
          {!readOnly && question && agentId && activeId ? (
            <div className="list-section" role="group" aria-label="助手提问">
              <div className="question-card">
                <div className="question-text">{question.question}</div>
                {question.options.length > 0 ? (
                  <div className="question-options">
                    {question.options.map((opt) => (
                      <button
                        key={opt}
                        type="button"
                        className="question-option"
                        onClick={() => void answerQuestion(agentId, activeId, question.requestId, opt)}
                      >
                        {opt}
                      </button>
                    ))}
                  </div>
                ) : null}
                <div className="question-answer">
                  <input
                    className="input-field"
                    placeholder="输入回答"
                    aria-label="回答输入框"
                    value={answerDraft}
                    onChange={(e) => setAnswerDraft(e.target.value)}
                  />
                  <button
                    type="button"
                    className="retry-link"
                    style={{ color: 'var(--color-tint)' }}
                    disabled={!answerDraft.trim()}
                    onClick={() => {
                      void answerQuestion(agentId, activeId, question.requestId, answerDraft.trim())
                      setAnswerDraft('')
                    }}
                  >
                    回答
                  </button>
                </div>
              </div>
            </div>
          ) : null}
        </div>

        {/* Read-only sessions render NO composer at all. */}
        {readOnly ? null : (
          <div className="composer">
            <textarea
              className="composer-input"
              rows={1}
              placeholder={activeId ? '发送消息' : '发送第一条消息将自动新建会话'}
              aria-label="消息输入框"
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
            />
            <button
              type="button"
              className="composer-send"
              aria-label="发送"
              disabled={!draft.trim() || state?.pendingSend?.state === 'sending'}
              onClick={() => void (activeId ? send() : sendFirst())}
            >
              发送
            </button>
          </div>
        )}

        <Sheet open={switcherOpen} onClose={() => setSwitcherOpen(false)} title={agent?.name}>
          {sessions.map((s) => {
            const isReadOnly = s.can_write === false
            return (
              <div key={s.session_id} className="sheet-row">
                <ListRow
                  onClick={() => {
                    setSwitcherOpen(false)
                    if (s.session_id !== activeId) void openSession(agentId!, s.session_id)
                  }}
                  label={
                    <span className="row-title">
                      {s.visibility === 'public' ? <span aria-label="公开">🌐</span> : null}
                      {s.visibility === 'private' ? <span aria-label="私有">🔒</span> : null}
                      {s.title}
                      {isReadOnly ? <span className="badge">只读</span> : null}
                    </span>
                  }
                  value={s.session_id === activeId ? '✓' : undefined}
                  hint={s.last_message ?? undefined}
                />
                {/* §5.4: delete and visibility live behind a per-row menu,
                    not on the row itself — a mis-tap on a chat list must
                    never destroy a conversation. */}
                <button
                  type="button"
                  className="sheet-row-more"
                  aria-label={`管理 ${s.title}`}
                  onClick={() => {
                    setSwitcherOpen(false)
                    setManageId(s.session_id)
                  }}
                >
                  ⋯
                </button>
              </div>
            )
          })}
          <ListRow
            label="＋ 新建会话"
            onClick={() => {
              setSwitcherOpen(false)
              void createSession(agentId!)
            }}
          />
          {sessionsHasMore ? (
            <ListRow label="加载更多" onClick={() => void loadMoreSessions(agentId!)} />
          ) : null}
        </Sheet>

        {/* Per-session action sheet (§5.4). */}
        <Sheet open={!!manageId} onClose={() => setManageId(null)} title="会话操作">
          {(() => {
            const target = sessions.find((s) => s.session_id === manageId)
            if (!target) return null
            const writable = target.can_write !== false
            const isPublic = target.visibility !== 'private'
            return (
              <>
                <ListRow
                  disabled={!writable}
                  hint={writable ? undefined : '只读会话无法修改'}
                  label={isPublic ? '🔒 设为私有' : '🌐 设为公开'}
                  onClick={() => {
                    setManageId(null)
                    void setSessionVisibility(agentId!, target.session_id, isPublic ? 'private' : 'public').then(() =>
                      useAgentStore.getState().refreshSessions(agentId!),
                    )
                  }}
                />
                <ListRow
                  destructive
                  disabled={!writable}
                  hint={writable ? undefined : '只读会话无法删除'}
                  label="删除会话"
                  onClick={() => {
                    setManageId(null)
                    void deleteSession(agentId!, target.session_id)
                  }}
                />
              </>
            )
          })()}
        </Sheet>

        {agentId ? (
          <AgentDrawer open={drawerOpen} agentId={agentId} onClose={() => setDrawerOpen(false)} />
        ) : null}
      </div>
    </EdgeSwipe>
  )
}
