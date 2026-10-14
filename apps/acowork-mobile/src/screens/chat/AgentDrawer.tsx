/**
 * Agent settings drawer — the mobile counterpart of Desktop's RightPanel
 * (design §4.2), opened by a left swipe on the chat detail screen.
 *
 * Six sections: 状态 / 工作区 / 记忆 / 工具 / 配置 / 会话. Each is loaded on
 * demand, only when its tab is selected: the drawer is opened to check one
 * thing, and pre-fetching six endpoints to display one of them is both
 * slower and louder on a phone connection.
 *
 * THE WORKSPACE RULE: this section offers "Add to Chat" and nothing else.
 * There is no file preview and no filesystem read — mobile never touches
 * `install_path`, which since ADR-055 lives on another machine. Every byte
 * here arrives over the Gateway reverse proxy, which is also what makes the
 * panel work at all when the Node is remote (ADR-009 §5).
 *
 * Everything is read-only except two things the design explicitly allows:
 * session visibility (§5.4) and the workspace attach.
 */

import { useCallback, useEffect, useState } from 'react'
import {
  attachWorkspace,
  fetchAgentConfig,
  fetchAgentStatus,
  fetchBuiltinTools,
  fetchMemoryStats,
  fetchWorkspaces,
  type AgentStatus,
  type BuiltinTool,
  type MemoryStats,
  type WorkspaceEntry,
} from '../../lib/api'
import { useAgentStore } from '../../stores/agentStore'
import { useChatStore } from '../../stores/chatStore'
import { ListRow, Banner } from '../../components/ui'
import type { SessionInfo } from '../../lib/types'

const TABS = [
  { id: 'status', label: '状态' },
  { id: 'workspace', label: '工作区' },
  { id: 'memory', label: '记忆' },
  { id: 'tools', label: '工具' },
  { id: 'config', label: '配置' },
  { id: 'sessions', label: '会话' },
] as const

type TabId = (typeof TABS)[number]['id']

export function AgentDrawer({ open, agentId, onClose }: { open: boolean; agentId: string; onClose: () => void }) {
  const [tab, setTab] = useState<TabId>('status')
  const info = useAgentStore((s) => s.agents[agentId]?.info)

  // Unmounted when closed: the per-tab fetches below then never run for a
  // drawer nobody is looking at.
  if (!open) return null

  return (
    <>
      <div className="drawer-backdrop" onClick={onClose} role="presentation" />
      <aside className="drawer" role="dialog" aria-modal="true" aria-label={`${info?.name ?? 'Agent'} 设置`}>
        <header className="drawer-head">
          <div className="drawer-title">{info?.name ?? agentId}</div>
          <button type="button" className="drawer-close" onClick={onClose} aria-label="关闭">
            关闭
          </button>
        </header>
        <div className="drawer-tabs" role="tablist" aria-label="设置分区">
          {TABS.map((t) => (
            <button
              key={t.id}
              type="button"
              role="tab"
              aria-selected={t.id === tab}
              className={`drawer-tab${t.id === tab ? ' is-active' : ''}`}
              onClick={() => setTab(t.id)}
            >
              {t.label}
            </button>
          ))}
        </div>
        <div className="drawer-body">
          {tab === 'status' ? <StatusTab agentId={agentId} /> : null}
          {tab === 'workspace' ? <WorkspaceTab agentId={agentId} /> : null}
          {tab === 'memory' ? <MemoryTab agentId={agentId} /> : null}
          {tab === 'tools' ? <ToolsTab agentId={agentId} /> : null}
          {tab === 'config' ? <ConfigTab agentId={agentId} /> : null}
          {tab === 'sessions' ? <SessionsTab agentId={agentId} onClose={onClose} /> : null}
        </div>
      </aside>
    </>
  )
}

/** One fetch-per-tab: shared skeleton so a failure reads the same everywhere. */
function useTabData<T>(loader: () => Promise<T>, deps: unknown[]): { data: T | null; error: string | null } {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const load = useCallback(() => {
    loader()
      .then((d) => {
        setData(d)
        setError(null)
      })
      .catch((e) => setError(e instanceof Error ? e.message : '加载失败'))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps)
  useEffect(load, [load])
  return { data, error }
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="list-section" role="group" aria-label={title}>
      <div className="list-section-title">{title}</div>
      {children}
    </div>
  )
}

function Row({ k, v }: { k: string; v: React.ReactNode }) {
  return <ListRow label={k} value={v} />
}

function ErrBanner({ msg }: { msg: string }) {
  return (
    <div style={{ padding: 'var(--space-3)' }}>
      <Banner tone="warning">{msg} · 该 Agent 可能不在此节点上，或 Runtime 尚未就绪</Banner>
    </div>
  )
}

function StatusTab({ agentId }: { agentId: string }) {
  const info = useAgentStore((s) => s.agents[agentId]?.info)
  const { data, error } = useTabData<AgentStatus>(() => fetchAgentStatus(agentId), [agentId])
  const st: AgentStatus | null = data
  return (
    <>
      <Section title="运行状态">
        <Row k="状态" v={info?.status === 'offline' ? '已停止' : '运行中'} />
        <Row k="生命周期" v={info?.status ?? '未知'} />
        {st ? <Row k="进程" v={String(st.pid)} /> : null}
        {st ? <Row k="工作目录" v={<span className="mono">{tail(st.work_dir)}</span>} /> : null}
      </Section>
      {error ? <ErrBanner msg="状态不可得" /> : null}
    </>
  )
}

/** A long Windows/Unix path is unreadable in a 320pt panel: show the end. */
function tail(p: string): string {
  const parts = p.split(/[\/]/).filter(Boolean)
  return parts.length > 2 ? `…/${parts.slice(-2).join('/')}` : p
}

function WorkspaceTab({ agentId }: { agentId: string }) {
  const { data, error } = useTabData<WorkspaceEntry[]>(() => fetchWorkspaces(agentId), [agentId])
  const activeId = useChatStore((s) => s.agentStates[agentId]?.activeSessionId ?? null)
  const [note, setNote] = useState<string | null>(null)

  const addToChat = async (ws: WorkspaceEntry) => {
    if (!activeId) {
      setNote('当前会话未打开，无法切换工作区')
      return
    }
    const id = ws.id
    if (!id) {
      setNote('该工作区没有可用 id')
      return
    }
    try {
      await attachWorkspace(agentId, activeId, id)
      setNote('已加入当前会话')
    } catch (e) {
      setNote(e instanceof Error ? e.message : '切换失败')
    }
  }

  return (
    <>
      <Section title="工作区">
        {(data ?? []).map((w, i) => {
          const id = w.id ?? ''
          // `alias` is the human's own label; without one the directory name
          // is the only thing that means anything to them — a workspace id
          // (`ws-e272622f`) looks like an error.
          const label = w.alias || (w.path ? tail(w.path) : id) || '未命名'
          const readOnly = w.access === 'read-only'
          return (
            <ListRow
              key={id || i}
              label={<span className="row-title">{label}</span>}
              hint={w.path ? w.path : undefined}
              value={
                id && !readOnly ? (
                  <button type="button" className="retry-link" style={{ color: 'var(--color-tint)' }} onClick={() => void addToChat(w)}>
                    加入会话
                  </button>
                ) : (
                  <span className="badge">只读</span>
                )
              }
            />
          )
        })}
        {!data && !error ? <ListRow label="载入中…" /> : null}
        {data && data.length === 0 ? <ListRow label="没有附加工作区" hint="在桌面端添加" /> : null}
      </Section>
      {note ? (
        <div style={{ padding: '0 var(--space-4)' }}>
          <Banner tone="info">{note}</Banner>
        </div>
      ) : null}
      {/* §4.2: file contents are read by the agent in the conversation, never
          previewed here. The note tells the user where that boundary is. */}
      <div style={{ padding: 'var(--space-3) var(--space-4)' }}>
        <Banner tone="info">移动端不提供文件预览，请在对话中让 Agent 读取</Banner>
      </div>
      {error ? <ErrBanner msg="工作区不可得" /> : null}
    </>
  )
}

function MemoryTab({ agentId }: { agentId: string }) {
  const { data, error } = useTabData<MemoryStats>(() => fetchMemoryStats(agentId), [agentId])
  const byType = Object.entries(data?.by_type ?? {})
  return (
    <>
      <Section title="记忆统计">
        {data ? <Row k="节点总数" v={String(data.total_nodes)} /> : <ListRow label="载入中…" />}
        {byType.map(([k, v]) => (
          <Row key={k} k={`${k} 记忆`} v={String(v)} />
        ))}
        {data ? <Row k="索引状态" v={indexLabel(data.index_health)} /> : null}
      </Section>
      <Section title="检索">
        <ListRow label="浏览记忆节点" hint="需桌面端（节点树 + 嵌入详情）" value={<span className="badge">桌面端</span>} />
      </Section>
      {error ? <ErrBanner msg="记忆统计不可得" /> : null}
    </>
  )
}

function indexLabel(h: string): string {
  if (h === 'healthy') return '已同步'
  if (h === 'no_store') return '未建立'
  return h.startsWith('error') ? '异常' : h
}

function ToolsTab({ agentId }: { agentId: string }) {
  const { data, error } = useTabData<BuiltinTool[]>(() => fetchBuiltinTools(agentId), [agentId])
  return (
    <>
      <Section title="内置工具">
        {(data ?? []).map((t) => (
          <ListRow key={t.name} label={<span className="row-title">{t.name}</span>} value={t.enabled ? '已启用' : '已禁用'} />
        ))}
        {!data && !error ? <ListRow label="载入中…" /> : null}
      </Section>
      <div style={{ padding: 'var(--space-3) var(--space-4)' }}>
        <Banner tone="info">工具开关请在桌面端修改</Banner>
      </div>
      {error ? <ErrBanner msg="工具列表不可得" /> : null}
    </>
  )
}

function ConfigTab({ agentId }: { agentId: string }) {
  const { data, error } = useTabData<Record<string, unknown>>(() => fetchAgentConfig(agentId), [agentId])
  // The endpoint wraps the manifest: `{agent_id, matches, config: {…}}`.
  // Reading the top level would show two identity rows and none of the
  // settings the tab is about, so descend first and fall back to the whole
  // body for a shape that has no wrapper.
  const inner = (data?.['config'] ?? data) as Record<string, unknown> | undefined
  // The config object is the Runtime's, not a mobile contract: show only the
  // scalar fields, or a nested dump would fill the panel with JSON.
  const scalars = Object.entries(inner ?? {}).filter(([, v]) => v === null || typeof v !== 'object')
  return (
    <>
      <Section title="配置">
        {scalars.map(([k, v]) => (
          <Row key={k} k={k} v={typeof v === 'string' ? tail(v) : String(v ?? '')} />
        ))}
        {!data && !error ? <ListRow label="载入中…" /> : null}
      </Section>
      <div style={{ padding: 'var(--space-3) var(--space-4)' }}>
        <Banner tone="info">配置修改请在桌面端进行</Banner>
      </div>
      {error ? <ErrBanner msg="配置不可得" /> : null}
    </>
  )
}

function SessionsTab({ agentId, onClose }: { agentId: string; onClose: () => void }) {
  const sessions = useAgentStore((s) => s.agents[agentId]?.sessions) ?? []
  const activeId = useChatStore((s) => s.agentStates[agentId]?.activeSessionId ?? null)
  const openSession = useChatStore((s) => s.openSession)
  const loadMore = useAgentStore((s) => s.loadMoreSessions)
  const hasMore = useAgentStore((s) => s.sessionsHasMore[agentId] ?? true)

  return (
    <>
      <Section title={`会话 · ${sessions.length}`}>
        {sessions.map((s: SessionInfo) => (
          <ListRow
            key={s.session_id}
            onClick={() => {
              onClose()
              if (s.session_id !== activeId) void openSession(agentId, s.session_id)
            }}
            label={<span className="row-title">{s.title}</span>}
            value={
              s.session_id === activeId ? (
                <span className="badge">当前</span>
              ) : s.can_write === false ? (
                <span className="badge">只读</span>
              ) : undefined
            }
          />
        ))}
        {sessions.length === 0 ? <ListRow label="该 Agent 暂无会话" /> : null}
        {hasMore && sessions.length > 0 ? <ListRow label="加载更多会话" onClick={() => void loadMore(agentId)} /> : null}
      </Section>
      <div style={{ padding: 'var(--space-3) var(--space-4)' }}>
        <Banner tone="info">多会话是业务事实：移动端与桌面端共用同一份会话列表</Banner>
      </div>
    </>
  )
}
