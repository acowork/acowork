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
import { Clock, Send, Settings2, Brain, Briefcase, Sparkles, Cpu, ChevronRight, Wrench, Image as ImageIcon, Check } from 'lucide-react'
import { useAgentStore } from '../../stores/agentStore'
import { isActiveStatus, useChatStore } from '../../stores/chatStore'
import { useNavStore } from '../../stores/navStore'
import { useAuthStore } from '../../stores/authStore'
import { useSessionReadOnly } from '../../lib/session-write-access'
import { setSessionVisibility, fetchWorkspaces, attachWorkspace, type WorkspaceEntry, listKeys, fetchProviderModels, fetchSkills, patchSessionConfig, fetchSessionConfig, type VaultKeyEntry, type ProviderModel, type SkillEntry, type SessionConfigSnapshot } from '../../lib/api'
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
  const loadEarlier = useChatStore((s) => s.loadEarlier)
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
  /** Composer settings sheet (model / reasoning / workspace / skill).
   *  `composerMenu` is the parent; one of `modelOpen | effortOpen |
   *  workspaceOpen | skillOpen` is the sub-sheet. Only one level is open
   *  at a time — picking a row on the parent swaps it for the sub. */
  const [composerMenu, setComposerMenu] = useState(false)
  const [modelOpen, setModelOpen] = useState(false)
  const [effortOpen, setEffortOpen] = useState(false)
  const [workspaceOpen, setWorkspaceOpen] = useState(false)
  const [skillOpen, setSkillOpen] = useState(false)
  /** Snapshot of the active session's effective config — backs the "current"
   *  badges on the model / effort / workspace rows so the picker is not a
   *  mystery box. Refreshed on session change and after every picker write. */
  const [configSnap, setConfigSnap] = useState<SessionConfigSnapshot | null>(null)
  // Workspace list, fetched once per agent — the parent menu's
  // "工作区选择" trail needs to render the *name* of the attached
  // workspace (`configSnap.workspace_id` is just an id, see api.ts
  // SessionConfigSnapshot). Keeping the list in the parent avoids
  // round-tripping the picked name up from the sub-sheet.
  const [workspaces, setWorkspaces] = useState<WorkspaceEntry[]>([])
  // Resolves `configSnap.workspace_id` to the human label the user
  // picked in the vault (`alias` first, then `path`, then the id as a
  // last resort). Used as the parent menu's "工作区选择" trail text so
  // the user sees the actual workspace name after attach, not the
  // bare "已附加".
  const attachedWorkspaceLabel = (() => {
    const id = configSnap?.workspace_id
    if (!id) return null
    const ws = workspaces.find((w) => w.id === id)
    return ws?.alias?.trim() || ws?.path || id
  })()
  /** The skill selected via the composer skill picker; rides the next
   *  message's `command` field and is cleared after send (desktop
   *  parity: clearActiveSkill in chatStore). Held in local state instead
   *  of the chat store because no other surface of the app needs to know. */
  const [activeSkill, setActiveSkill] = useState<SkillEntry | null>(null)
  const active = sessions.find((s) => s.session_id === activeId)
  // Follow rule: a refresh only scrolls to the bottom while the user is
  // already at it. A session switch (forceKey) re-pins, and so does a send
  // (`stickNow` below) — scrolling up must survive the 2 s snapshot poll.
  // Scrolling into the top band pages in earlier history (ADR-050
  // loadEarlier, the Desktop loadBefore equivalent); the store guards
  // re-entrancy and stops at offset 0.
  const { ref: scrollRef, stickNow } = useStickToBottom(
    [state?.messages.length, state?.status],
    activeId,
    () => {
      if (agentId) void loadEarlier(agentId)
    },
  )

  // §8 watch lifetime: exactly the foreground session, exactly while on screen.
  useEffect(() => {
    if (agentId && activeId) startWatching(agentId, activeId)
    return () => stopWatching()
  }, [agentId, activeId, startWatching, stopWatching])

  // Pull the session's effective config whenever the active session
  // changes — the model / effort pickers badge "current" against this
  // snapshot so the user knows what they're switching from. A failure
  // (rare, ADR-087 access denied) is silent — the pickers still work,
  // they just lack the "current" label.
  useEffect(() => {
    if (!agentId || !activeId) {
      setConfigSnap(null)
      return
    }
    let alive = true
    fetchSessionConfig(agentId, activeId)
      .then((c) => { if (alive) setConfigSnap(c) })
      .catch(() => { if (alive) setConfigSnap(null) })
    return () => { alive = false }
  }, [agentId, activeId])

  // Workspace list mirrors the per-agent scope of `fetchWorkspaces`
  // (the server resolves it against the runtime's known workspaces, so
  // it never crosses agent boundaries). Refreshed on agent switch.
  useEffect(() => {
    if (!agentId) {
      setWorkspaces([])
      return
    }
    let alive = true
    fetchWorkspaces(agentId)
      .then((list) => { if (alive) setWorkspaces(list ?? []) })
      .catch(() => { if (alive) setWorkspaces([]) })
    return () => { alive = false }
  }, [agentId])

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
    // The active skill (set via the composer skill picker) rides the next
    // message as `command`. Clear it after the send so the user has to
    // re-pick if they want the same skill on the next turn — desktop
    // parity (clearActiveSkill in chatStore). command="", not undefined,
    // so the wire field is sent explicitly even when no skill is active.
    const skillName = activeSkill?.name
    const ok = await sendMessage(agentId, activeId, text, skillName)
    if (ok) {
      setDraft('')
      setActiveSkill(null)
    }
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
          {state?.isLoadingMore ? <div className="time-sep">载入更早消息…</div> : null}
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
              const role = turn.isUser
                ? (me?.role === 'admin' ? '管理员' : me?.role === 'user' ? '用户' : null)
                : (agent?.role ?? null)
              return (
                <Fragment key={turn.key}>
                  {sep ? <div className="time-sep">{sep}</div> : null}
                  <div className={`msg-group${turn.isUser ? ' msg-group-self' : ' msg-group-peer'}`}>
                    <div className="msg-header">
                      <div className="msg-avatar">{avatar}</div>
                      <div className="msg-sender">
                        <span className="msg-sender-name">{name}</span>
                        {role ? <span className="msg-sender-role">{role}</span> : null}
                      </div>
                    </div>
                    <div className="msg-stack">
                      {turn.items.map((item) =>
                        item.type === 'explore'
                          ? <ExploreBlock key={`explore-${item.items[0]!.id}`} items={item.items} />
                          : <Bubble key={item.msg.id} msg={item.msg} literal={turn.isUser} />,
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
              <div className="msg-header">
                <div className="msg-avatar">{userAvatar}</div>
              </div>
              <div className="msg-stack">
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
            {/* Settings cog sits on the LEFT, mirroring Desktop's
             *  bottom-of-input toolbar (Model / Reasoning / Workspace
             *  / Skill are leading cluster). Hidden before the user
             *  has any session — the "send first message creates a
             *  session" path has nothing to configure. */}
            {activeId ? (
              <button
                type="button"
                className="composer-settings"
                aria-label="会话设置"
                onClick={() => setComposerMenu(true)}
              >
                <Settings2 className="composer-send-icon" aria-hidden />
              </button>
            ) : null}
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
              <Send className="composer-send-icon" aria-hidden />
            </button>
          </div>
        )}

        <Sheet open={switcherOpen} onClose={() => setSwitcherOpen(false)} title={agent?.name}>
          {/* High-frequency action. Sits at the TOP of the sheet so a tap
           * on the history button → tap on "新建会话" is the only path,
           * with no scroll required even when the session list is long.
           * The hairline below separates the "actions" group from the
           * "history" group (same iOS grouped-list idiom used elsewhere
           * in the app). Desktop keeps these two side-by-side at the
           * top level; mobile folds them into a sub-sheet but the
           * action is never more than one tap away from the entry. */}
          <ListRow
            label="＋ 新建会话"
            onClick={() => {
              setSwitcherOpen(false)
              void createSession(agentId!)
            }}
          />
          <div className="menu-sep" />
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

        {/* Composer settings (cog button → this sheet). Four sub-sheets
         *  mirror Desktop's bottom-of-input toolbar: Model / Reasoning /
         *  Workspace / Skill. The trail text on each row reflects the
         *  current value (`configSnap`) so the menu reads as a status
         *  surface, not a blank list. Tapping a row opens the matching
         *  sub-sheet — only one level open at a time. */}
        <Sheet open={composerMenu} onClose={() => setComposerMenu(false)} title="会话设置">
          <div className="menu-section-title">输入选项</div>
          <button
            type="button"
            className="menu-row"
            onClick={() => { setModelOpen(true) }}
          >
            <span className="menu-row-icon"><Cpu className="h-4 w-4" /></span>
            <span className="menu-row-label">模型选择</span>
            <span className="menu-row-trail">{configSnap?.model ?? '默认'}</span>
            <span className="menu-row-arrow"><ChevronRight className="h-4 w-4" /></span>
          </button>
          <button
            type="button"
            className="menu-row"
            onClick={() => { setEffortOpen(true) }}
          >
            <span className="menu-row-icon"><Brain className="h-4 w-4" /></span>
            <span className="menu-row-label">推理深度</span>
            <span className="menu-row-trail">{effortLabel(configSnap?.reasoning_effort)}</span>
            <span className="menu-row-arrow"><ChevronRight className="h-4 w-4" /></span>
          </button>
          <button
            type="button"
            className="menu-row"
            onClick={() => { setWorkspaceOpen(true) }}
          >
            <span className="menu-row-icon"><Briefcase className="h-4 w-4" /></span>
            <span className="menu-row-label">工作区选择</span>
            <span className="menu-row-trail">{attachedWorkspaceLabel ?? '未附加'}</span>
            <span className="menu-row-arrow"><ChevronRight className="h-4 w-4" /></span>
          </button>
          <button
            type="button"
            className="menu-row"
            onClick={() => { setSkillOpen(true) }}
          >
            <span className="menu-row-icon"><Sparkles className="h-4 w-4" /></span>
            <span className="menu-row-label">技能选择</span>
            <span className="menu-row-trail">{activeSkill ? activeSkill.name : '无'}</span>
            <span className="menu-row-arrow"><ChevronRight className="h-4 w-4" /></span>
          </button>
          {activeSkill ? (
            <div className="menu-subsection menu-hint-row">
              <span className="menu-hint-row-text">
                下条消息将以技能「{activeSkill.name}」发送；发送后自动清除。
              </span>
              <button
                type="button"
                className="btn btn-sm"
                onClick={() => setActiveSkill(null)}
              >
                清除
              </button>
            </div>
          ) : null}
        </Sheet>

        {/* Model picker — real. Loads providers + configured models on
         *  open, intersecting each provider's `models[]` allowlist with
         *  `fetchProviderModels` so the user sees exactly what they
         *  picked in the vault, not the catalog. Tapping a row PATCHes
         *  the session config and refreshes the parent menu's trail. */}
        <ComposerModelSheet
          open={modelOpen}
          agentId={agentId ?? null}
          sessionId={activeId}
          current={configSnap}
          onClose={() => {
            setModelOpen(false)
            // Refresh the parent menu's badge after a write.
            if (agentId && activeId) void fetchSessionConfig(agentId, activeId).then(setConfigSnap).catch(() => undefined)
          }}
        />

        {/* Reasoning effort picker — real. Builds the offered-effort list
         *  from the current model's `default_reasoning_effort` and the
         *  universal set (`off | low | medium | high`), then PATCHes the
         *  chosen value to the session config. */}
        <ComposerEffortSheet
          open={effortOpen}
          agentId={agentId ?? null}
          sessionId={activeId}
          current={configSnap}
          onClose={() => {
            setEffortOpen(false)
            if (agentId && activeId) void fetchSessionConfig(agentId, activeId).then(setConfigSnap).catch(() => undefined)
          }}
        />

        {/* Workspace picker — wired. Reuses the same fetch + attach pair
         *  the AgentDrawer "工作区" tab uses; tapping a row attaches that
         *  workspace to the active session. */}
        <ComposerWorkspaceSheet
          open={workspaceOpen}
          agentId={agentId ?? null}
          sessionId={activeId}
          current={configSnap}
          onClose={() => setWorkspaceOpen(false)}
        />

        {/* Skill picker — real. Loads the agent's skills on open; tapping
         *  a row sets the local active-skill flag, which rides the next
         *  message as the wire `command` field (resolved by the agent's
         *  SkillRegistry — see lib/api.ts sendMessage). */}
        <ComposerSkillSheet
          open={skillOpen}
          agentId={agentId ?? null}
          current={activeSkill}
          onPick={(s) => {
            setActiveSkill(s)
            setSkillOpen(false)
          }}
          onClear={() => {
            setActiveSkill(null)
            setSkillOpen(false)
          }}
        />

        {agentId ? (
          <AgentDrawer open={drawerOpen} agentId={agentId} onClose={() => setDrawerOpen(false)} />
        ) : null}
      </div>
    </EdgeSwipe>
  )
}

/** The workspace sub-sheet inside the composer settings menu. Loads the
 *  agent's workspace list on open and lets the user attach one to the
 *  active session. Identical wiring to AgentDrawer's WorkspaceTab — both
 *  feed off `fetchWorkspaces` + `attachWorkspace` — but the surface is a
 *  plain Sheet so the user reads it as part of the composer flow, not as
 *  an agent-settings drawer. */
function ComposerWorkspaceSheet({
  open,
  agentId,
  sessionId,
  current,
  onClose,
}: {
  open: boolean
  agentId: string | null
  sessionId: string | null
  /** Picker mirrors the model/effort pattern: the row whose id matches
   *  `current.workspace_id` is rendered with the same `is-selected` tint
   *  and a trailing `Check`, so the user sees which workspace the
   *  session is actually attached to before they pick another. */
  current: SessionConfigSnapshot | null
  onClose: () => void
}) {
  const [items, setItems] = useState<WorkspaceEntry[] | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [note, setNote] = useState<string | null>(null)

  // Re-fetch every time the sheet opens; the workspace list can grow on
  // another device (the desktop app) and we want this picker to show the
  // freshest set when the user opens it. `alive` mirrors ComposerModelSheet
  // / ComposerSkillSheet so a quick close-then-reopen doesn't fire
  // setState on a returned-null component.
  useEffect(() => {
    if (!open || !agentId) return
    let alive = true
    setLoading(true)
    setError(null)
    fetchWorkspaces(agentId)
      .then((list) => { if (alive) setItems(list) })
      .catch((e) => { if (alive) setError(e instanceof Error ? e.message : '加载失败') })
      .finally(() => { if (alive) setLoading(false) })
    return () => { alive = false }
  }, [open, agentId])

  return (
    <Sheet open={open} onClose={onClose} title="工作区选择">
      {note ? <div className="menu-subsection">{note}</div> : null}
      {loading ? <ListRow label="载入中…" /> : null}
      {error ? <ListRow label={error} destructive /> : null}
      {items && items.length === 0 ? (
        <ListRow label="该 Agent 暂无工作区" hint="在桌面端添加" />
      ) : null}
      {items?.map((ws) => {
        const id = ws.id
        if (!id) return null
        const label = ws.alias?.trim() || ws.path || id
        const access = ws.access === 'read-only' ? '只读' : '读写'
        const isCurrent = current?.workspace_id === id
        return (
          <ListRow
            key={id}
            label={label}
            value={
              <span className="cap-icons">
                <span>{access}</span>
                {isCurrent ? <Check size={14} className="cap-icon-check" /> : null}
              </span>
            }
            selected={isCurrent}
            onClick={() => {
              if (!agentId || !sessionId) {
                setNote('当前会话未打开无法附加工作区')
                return
              }
              attachWorkspace(agentId, sessionId, id)
                .then(() => {
                  // Close the sub so the user lands back on the parent
                  // menu (which re-fetches the snapshot on its onClose
                  // and updates the trail from "未附加" → "已附加" as the
                  // visible confirmation that the attach took). Keeping
                  // `note` for the error path only.
                  onClose()
                })
                .catch((e) => setNote(e instanceof Error ? e.message : '附加失败'))
            }}
          />
        )
      })}
    </Sheet>
  )
}

/** Human label for the reasoning-effort enum the backend understands.
 *  Centralised so the menu's "current" trail and the sub-sheet rows
 *  agree on the same string per value. */
const EFFORT_LABELS: Record<string, string> = {
  off: '关闭',
  low: '低',
  medium: '中',
  high: '高',
  minimal: '最小',
}
function effortLabel(v: string | null | undefined): string {
  if (!v) return '默认'
  return EFFORT_LABELS[v] ?? v
}

/** The four canonical effort values, ordered low → high. We always offer
 *  this set; the model picker can hide the picker entirely when the
 *  selected model has `reasoning: false` in its capability row, but the
 *  backend is the source of truth and will silently accept whatever we
 *  send — so it's safe to surface this list even before we've confirmed
 *  the model has reasoning. */
const EFFORT_OPTIONS = ['off', 'low', 'medium', 'high'] as const

/** Model sub-sheet. Intersects each vault key's `models[]` allowlist with
 *  the per-provider capability catalog so the list is exactly what the
 *  user would see in Desktop's ModelMenu. Tapping a row PATCHes the
 *  session config and closes the sheet; the parent re-pulls config on
 *  close so the "current" trail stays in sync. */
function ComposerModelSheet({
  open,
  agentId,
  sessionId,
  current,
  onClose,
}: {
  open: boolean
  agentId: string | null
  sessionId: string | null
  current: SessionConfigSnapshot | null
  onClose: () => void
}) {
  const [keys, setKeys] = useState<VaultKeyEntry[] | null>(null)
  const [catalog, setCatalog] = useState<Record<string, ProviderModel[]>>({})
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)

  // Re-load on every open. The vault can grow (the user adds a new key
  // in desktop); a stale list would silently miss the new provider.
  useEffect(() => {
    if (!open || !agentId) return
    let alive = true
    setLoading(true)
    setError(null)
    listKeys()
      .then(async (ks) => {
        if (!alive) return
        setKeys(ks)
        // Fire one capability fetch per unique provider; the per-account
        // catalog is identical so a 3-key provider doesn't trip 3 calls.
        const providers = [...new Set(ks.map((k) => k.provider))]
        const results = await Promise.all(providers.map((p) => fetchProviderModels(p)))
        if (!alive) return
        const map: Record<string, ProviderModel[]> = {}
        providers.forEach((p, i) => { map[p] = results[i]?.models ?? [] })
        setCatalog(map)
      })
      .catch((e) => setError(e instanceof Error ? e.message : '加载失败'))
      .finally(() => { if (alive) setLoading(false) })
    return () => { alive = false }
  }, [open, agentId])

  if (!open) return null

  // Build the displayed list: one row per (provider, account, model)
  // triple, intersected with the provider capability catalog so we don't
  // show an id the user hasn't enabled. Then group by provider so the
  // picker mirrors Desktop's flat ModelMenu (provider as section header,
  // models stacked under it) instead of repeating the provider on every
  // row.
  type Row = { provider: string; accountId: string | null; modelId: string; cap: ProviderModel | null }
  const rows: Row[] = []
  keys?.forEach((k) => {
    const ids = (k.models && k.models.length > 0) ? k.models : (k.default_model ? [k.default_model] : [])
    const caps = catalog[k.provider] ?? []
    for (const id of ids) {
      rows.push({ provider: k.provider, accountId: k.account_id ?? null, modelId: id, cap: caps.find((m) => m.id === id) ?? null })
    }
  })

  // Strip the leading "provider/" from a model id only when the tail is
  // actually longer (e.g. "openai/gpt-4o" → "gpt-4o"). Same rule as
  // Desktop's ModelMenu (ChatPanel.tsx ~L3610).
  const displayName = (id: string): string => {
    const slash = id.indexOf('/')
    if (slash < 0) return id
    const prefix = id.slice(0, slash)
    const tail = id.slice(slash + 1)
    return tail.length > prefix.length ? tail : id
  }

  // Group rows by provider. Sort each bucket by display name; sort
  // buckets by their first row's provider so the order is stable across
  // re-opens (matches Desktop's locale-compare).
  const groups = (() => {
    const m = new Map<string, Row[]>()
    for (const r of rows) {
      const bucket = m.get(r.provider)
      if (bucket) bucket.push(r)
      else m.set(r.provider, [r])
    }
    const cmp = (a: string, b: string) => a.toLowerCase().localeCompare(b.toLowerCase())
    return Array.from(m.entries())
      .map(([provider, rs]) => [
        provider,
        [...rs].sort((a, b) => cmp(displayName(a.modelId), displayName(b.modelId))),
      ] as const)
      .sort(([a], [b]) => cmp(a, b))
  })()

  return (
    <Sheet open={open} onClose={onClose} title="模型选择">
      {loading ? <ListRow label="载入中…" /> : null}
      {error ? <ListRow label={error} destructive /> : null}
      {!loading && groups.length === 0 ? (
        <ListRow
          label="尚未配置任何模型"
          hint="在桌面端的设置中配置 API 密钥和模型"
          value={<span className="badge">桌面端</span>}
          disabled
        />
      ) : null}
      {groups.map(([provider, pRows], gIdx) => (
        <div key={provider}>
          {gIdx > 0 ? <div className="menu-sep" /> : null}
          <div className="menu-section-title">{provider}</div>
          {pRows.map((r) => {
            const isCurrent = current?.model === r.modelId && current?.provider === r.provider
            return (
              <ListRow
                key={`${r.provider}:${r.accountId ?? ''}:${r.modelId}`}
                label={displayName(r.modelId)}
                selected={isCurrent}
                value={
                  <span className="cap-icons">
                    {r.cap?.tool_call ? <Wrench size={12} className="cap-icon" /> : null}
                    {r.cap?.reasoning ? <Brain size={12} className="cap-icon-reasoning" /> : null}
                    {r.cap?.input_modalities?.includes('image') ? <ImageIcon size={12} className="cap-icon-image" /> : null}
                    {isCurrent ? <Check size={14} className="cap-icon-check" /> : null}
                  </span>
                }
                disabled={busy !== null}
                onClick={() => {
                  if (!agentId || !sessionId) return
                  setBusy(r.modelId)
                  patchSessionConfig(agentId, sessionId, {
                    model: r.modelId,
                    provider: r.provider,
                    account_id: r.accountId ?? undefined,
                  })
                    .then(() => onClose())
                    .catch((e) => setError(e instanceof Error ? e.message : '切换失败'))
                    .finally(() => setBusy(null))
                }}
              />
            )
          })}
        </div>
      ))}
    </Sheet>
  )
}

/** Reasoning-effort sub-sheet. Universal set, current selection badged.
 *  Writes one field; PATCH semantics leave everything else untouched. */
function ComposerEffortSheet({
  open,
  agentId,
  sessionId,
  current,
  onClose,
}: {
  open: boolean
  agentId: string | null
  sessionId: string | null
  current: SessionConfigSnapshot | null
  onClose: () => void
}) {
  const [busy, setBusy] = useState<string | null>(null)
  // Mirror ComposerModelSheet: surface a PATCH failure as a row, not
  // silent. Without this the user taps a row, the busy lock drops, and
  // nothing else happens — they think the tap didn't register.
  const [error, setError] = useState<string | null>(null)
  if (!open) return null
  return (
    <Sheet open={open} onClose={onClose} title="推理深度">
      {error ? <ListRow label={error} destructive /> : null}
      {EFFORT_OPTIONS.map((v) => {
        const isCurrent = current?.reasoning_effort === v
        return (
          <ListRow
            key={v}
            label={effortLabel(v)}
            value={isCurrent ? <Check size={14} className="cap-icon-check" /> : undefined}
            selected={isCurrent}
            disabled={busy !== null}
            onClick={() => {
              if (!agentId || !sessionId) return
              setError(null)
              setBusy(v)
              patchSessionConfig(agentId, sessionId, { reasoning_effort: v })
                .then(() => onClose())
                .catch((e) => setError(e instanceof Error ? e.message : '切换失败'))
                .finally(() => setBusy(null))
            }}
          />
        )
      })}
    </Sheet>
  )
}

/** Skill sub-sheet. Renders the agent's skills as a list of rows; tap to
 *  pick one as the active skill for the next message. The "clear" row at
 *  the top drops the active skill without picking a new one. */
function ComposerSkillSheet({
  open,
  agentId,
  current,
  onPick,
  onClear,
}: {
  open: boolean
  agentId: string | null
  current: SkillEntry | null
  onPick: (s: SkillEntry) => void
  onClear: () => void
}) {
  const [skills, setSkills] = useState<SkillEntry[] | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!open || !agentId) return
    let alive = true
    setLoading(true)
    setError(null)
    fetchSkills(agentId)
      .then((r) => { if (alive) setSkills(r.skills ?? []) })
      .catch((e) => setError(e instanceof Error ? e.message : '加载失败'))
      .finally(() => { if (alive) setLoading(false) })
    return () => { alive = false }
  }, [open, agentId])

  if (!open) return null
  const enabled = skills?.filter((s) => s.enabled !== false) ?? []
  return (
    <Sheet open={open} onClose={onClear} title="技能选择">
      {loading ? <ListRow label="载入中…" /> : null}
      {error ? <ListRow label={error} destructive /> : null}
      {current ? (
        <ListRow
          label={`清除当前选择「${current.name}」`}
          onClick={onClear}
        />
      ) : null}
      {!loading && enabled.length === 0 ? (
        <ListRow
          label="该 Agent 暂无技能"
          hint="在桌面端添加技能包"
          value={<span className="badge">桌面端</span>}
          disabled
        />
      ) : null}
      {enabled.map((s) => (
        <ListRow
          key={s.name}
          label={s.name}
          hint={s.description ?? undefined}
          value={current?.name === s.name ? <span className="badge">当前</span> : undefined}
          onClick={() => onPick(s)}
        />
      ))}
    </Sheet>
  )
}
