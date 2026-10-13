/**
 * Chat tab root: the unified IM conversation stream.
 *
 * Desktop splits this into an AgentList and a UserList side by side. On a
 * 390pt-wide screen there is no room for two lists, and IM users expect
 * ONE inbox. So agents and contacts become two `ListSection` groups inside
 * a single scroll view — same data, one column, native grouping.
 *
 * Ordering is recency, active conversations first. A contact with no thread
 * yet is still listed (starting one is the point of the directory), sorted
 * below everyone who has messages.
 *
 * Two badges with two different truths (§8.5): a human thread has a
 * server-side read cursor, so its count is a NUMBER. An agent session has
 * none, so it gets a DOT and nothing more — inventing a number there would
 * be a claim this client cannot support.
 */

import { useEffect, useMemo, useState, type ReactNode } from 'react'
import { Monitor, Users } from 'lucide-react'
import { useAgentStore } from '../../stores/agentStore'
import { useChatStore } from '../../stores/chatStore'
import { useUserChatStore } from '../../stores/userChatStore'
import { useNavStore } from '../../stores/navStore'
import { useAuthStore } from '../../stores/authStore'
import { hasUnread } from '../../lib/unread'
import { formatListTime } from '../../lib/time'
import { ListSection, ListRow } from '../../components/ui'
import { AuthImage } from '../../components/AuthImage'
import { BUILTIN_ICONS, BUILTIN_ICON_IDS } from '../../lib/builtinIcons'
import type { NodeRow } from '../../lib/types'

/** Pick a stable builtin icon ID for a given seed (same as Desktop). */
function pickIconId(seed: string): string | null {
  if (BUILTIN_ICON_IDS.length === 0 || !seed) return null
  let h = 0
  for (let i = 0; i < seed.length; i++) h = (seed.charCodeAt(i) + (h << 5) - h) | 0
  return BUILTIN_ICON_IDS[Math.abs(h) % BUILTIN_ICON_IDS.length] ?? null
}

/** Render agent avatar: custom file → declared builtin → deterministic builtin. */
function AgentAvatar({ name, icon, avatar, agentId, baseUrl }: { name: string; icon?: string | null; avatar?: string | null; agentId: string; baseUrl: string }) {
  const picked = pickIconId(name)
  const fallbackSrc = (icon && BUILTIN_ICONS[icon]) ? BUILTIN_ICONS[icon] : BUILTIN_ICONS[picked ?? '']
  const fallbackEl = fallbackSrc
    ? <img src={fallbackSrc} alt="" />
    : <span>{name.charAt(0)}</span>

  if (avatar) {
    const url = `${baseUrl}/api/agents/${encodeURIComponent(agentId)}/avatar-file?path=${encodeURIComponent(avatar)}`
    return <AuthImage src={url} fallback={fallbackEl} />
  }
  return fallbackEl
}

/** Render user avatar: custom file → builtin icon → deterministic builtin. */
function UserAvatarEl({ name, avatar, builtinAvatar, baseUrl }: { name: string; avatar?: string | null; builtinAvatar?: string | null; baseUrl: string }) {
  const picked = pickIconId(name)
  const fallbackSrc = (builtinAvatar && BUILTIN_ICONS[builtinAvatar]) ? BUILTIN_ICONS[builtinAvatar] : BUILTIN_ICONS[picked ?? '']
  const fallbackEl = fallbackSrc
    ? <img src={fallbackSrc} alt="" />
    : <span>{name.charAt(0)}</span>

  if (avatar) {
    const url = `${baseUrl}/api/user/avatar-file?path=${encodeURIComponent(avatar)}`
    return <AuthImage src={url} fallback={fallbackEl} />
  }
  return fallbackEl
}

/** Group-header skeleton shared by every inbox section: leading glyph, then
 *  the name, then an optional trailing status dot. One shape for nodes and
 *  contacts so both read as the same kind of list entry. */
function GroupTitle({ icon, name, online }: { icon: ReactNode; name: string; online?: boolean }) {
  return (
    <span className="group-title">
      {icon}
      <span className="group-title-name">{name}</span>
      {online === undefined ? null : (
        <span
          className={`dot ${online ? 'dot-online' : 'dot-offline'}`}
          aria-label={online ? '在线' : '离线'}
        />
      )}
    </span>
  )
}

/** Group header for one node: a computer glyph before the name — a node is a
 *  machine hosting agents, not a chat peer — and its online dot after the
 *  name. The dot follows the row rather than the status the inbox filtered
 *  on, so a node that drops offline between refreshes is never shown as up. */
function NodeTitle({ node }: { node: NodeRow }) {
  return (
    <GroupTitle
      icon={<Monitor className="group-title-icon" aria-hidden />}
      name={node.node_name ?? node.node_id}
      online={node.online}
    />
  )
}

/** Group header for the contacts section. A contact list has no aggregate
 *  presence (each peer is online or not on its own), so no status dot. */
function ContactsTitle() {
  return <GroupTitle icon={<Users className="group-title-icon" aria-hidden />} name="联系人" />
}

export function ChatListScreen() {
  const agents = useAgentStore((s) => s.agentList)
  const users = useAgentStore((s) => s.users)
  const nodes = useAgentStore((s) => s.nodes)
  const sessionsByAgent = useAgentStore((s) => s.agents)
  const refreshInbox = useAgentStore((s) => s.refreshInbox)
  const loading = useAgentStore((s) => s.directoryLoading)
  const startingAgentIds = useAgentStore((s) => s.startingAgentIds)
  const openSession = useChatStore((s) => s.openSession)
  const push = useNavStore((s) => s.push)
  const baseUrl = useAuthStore((s) => s.baseUrl)

  const chats = useUserChatStore((s) => s.chats)
  const refreshChats = useUserChatStore((s) => s.refreshChats)

  const [query, setQuery] = useState('')

  // Entering the inbox re-pulls directory + sessions + previews (§8.5).
  useEffect(() => {
    void refreshInbox()
    void refreshChats()
  }, [refreshInbox, refreshChats])

  const q = query.trim().toLowerCase()
  const match = (name: string, sub?: string | null) =>
    !q || name.toLowerCase().includes(q) || (sub ?? '').toLowerCase().includes(q)

  // Visible-online nodes only. The server already dropped nodes this account
  // cannot view (ADR-087); `online` drops the offline ones. An agent whose
  // node is not in this map is hidden — no fallback bucket, because after the
  // backend's Node->Agent cascade fix a listed agent always has a viewable,
  // and here online, node.
  const onlineById = useMemo(
    () => new Map(nodes.filter((n) => n.online).map((n) => [n.node_id, n])),
    [nodes],
  )

  const agentRows = useMemo(() => {
    return agents
      .map((a) => {
        const sessions = sessionsByAgent[a.id]?.sessions ?? []
        // Newest by the server's own `last_active_at`, not by array order:
        // a page-1 refresh replaces the list, and its order is the server's
        // pagination order, which is not guaranteed to be recency.
        const recent = sessions.reduce<null | (typeof sessions)[number]>(
          (best, s) => (!best || (s.updated_at ?? 0) > (best.updated_at ?? 0) ? s : best),
          null,
        )
        return { a, recent }
      })
      .filter(({ a, recent }) => match(a.name, recent?.last_message ?? recent?.title))
      .filter(({ a }) => onlineById.has(a.node_id))
      .sort((x, y) => (y.recent?.updated_at ?? 0) - (x.recent?.updated_at ?? 0))
  }, [agents, sessionsByAgent, q, onlineById])

  // Bucket the (already recency-sorted) rows under their node, so within-group
  // order stays by recency; groups themselves sort by their newest member.
  const onlineGroups = useMemo(() => {
    const buckets = new Map<string, typeof agentRows>()
    for (const row of agentRows) {
      const arr = buckets.get(row.a.node_id)
      if (arr) arr.push(row)
      else buckets.set(row.a.node_id, [row])
    }
    const newest = (rows: typeof agentRows) => Math.max(0, ...rows.map((r) => r.recent?.updated_at ?? 0))
    return [...buckets.entries()]
      .map(([nodeId, rows]) => ({ node: onlineById.get(nodeId)!, rows }))
      .sort((x, y) => newest(y.rows) - newest(x.rows))
  }, [agentRows, onlineById])

  const chatByPeer = useMemo(() => new Map(chats.map((c) => [c.peer_user_id, c])), [chats])

  const userRows = useMemo(() => {
    return users
      .map((u) => ({ u, chat: chatByPeer.get(u.id) }))
      .filter(({ u, chat }) => match(u.display_name, chat?.last_message_preview))
      .sort((x, y) => (y.chat?.last_active_at ?? 0) - (x.chat?.last_active_at ?? 0))
  }, [users, chatByPeer, q])

  const openAgent = async (agentId: string, sessionId: string | null) => {
    // Offline → wake it in place, do NOT open a chat against a stopped
    // Runtime (that was the blank screen). Re-tap once the row goes online.
    const info = useAgentStore.getState().agentList.find((a) => a.id === agentId)
    if (info?.status === 'offline') {
      void useAgentStore.getState().startAgent(agentId)
      return
    }
    useChatStore.getState().selectAgent(agentId)
    // Load this agent's sessions on open. A just-started agent's cached list
    // is empty (the offline-time load returned nothing), so trusting the
    // click-time `recent` landed the first open on a null session → blank;
    // the second tap only worked because returning to the list had reloaded
    // them. `refreshSessions` re-reads page 1 and updates the store slot.
    await useAgentStore.getState().refreshSessions(agentId)
    const list = useAgentStore.getState().agents[agentId]?.sessions ?? []
    const newest = list.slice().sort((x, y) => (y.updated_at ?? 0) - (x.updated_at ?? 0))[0]
    const target = sessionId ?? newest?.session_id ?? null
    if (target) await openSession(agentId, target)
    push('chat/detail')
  }

  const openUser = (peerId: string, peerName: string) => {
    void useUserChatStore.getState().openChat(peerId, peerName)
    push('chat/user')
  }

  return (
    <div className="screen">
      <header className="navbar navbar-large">
        <h1 className="navbar-title">聊天</h1>
      </header>

      <div className="searchbar">
        <input
          className="searchbar-input"
          type="search"
          inputMode="search"
          placeholder="搜索 Agent 或联系人"
          aria-label="搜索"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
      </div>

      <div className="scroll">
        {loading && agents.length === 0 ? <div className="empty-state">载入中…</div> : null}
        {onlineGroups.map((g) => (
          <ListSection key={g.node.node_id} title={<NodeTitle node={g.node} />}>
            {g.rows.map(({ a, recent }) => (
              <ListRow
                key={a.id}
                avatar={<AgentAvatar name={a.name} icon={a.icon} avatar={a.avatar} agentId={a.id} baseUrl={baseUrl} />}
                avatarUnread={!!(recent && hasUnread(a.id, recent.session_id, recent.message_count))}
                onClick={() => void openAgent(a.id, recent?.session_id ?? null)}
                label={
                  <span className="row-title">
                    {a.name}
                    {startingAgentIds.has(a.id) ? (
                      <span className="dot-spinner" aria-label="正在启动" title="正在启动" />
                    ) : (
                      (() => {
                        // Presence from the directory's `alive`/`lifecycle`.
                        // A plain offline-only dot used to render transparent
                        // and never surfaced online; now all three states show.
                        const cls =
                          a.status === 'offline'
                            ? 'dot dot-offline'
                            : a.status === 'error'
                              ? 'dot dot-error'
                              : 'dot dot-online'
                        const text =
                          a.status === 'offline' ? '离线' : a.status === 'error' ? '异常' : '在线'
                        return <span className={cls} aria-label={text} title={text} />
                      })()
                    )}
                  </span>
                }
                value={formatListTime(recent?.updated_at)}
                hint={startingAgentIds.has(a.id) ? '正在启动…' : recent?.last_message || recent?.title || '暂无会话'}
              />
            ))}
          </ListSection>
        ))}
        {onlineGroups.length === 0 && !loading ? (
          <ListSection>
            <ListRow label="暂无 Agent" hint="在桌面端安装后出现在这里" />
          </ListSection>
        ) : null}

        <ListSection title={<ContactsTitle />}>
          {userRows.map(({ u, chat }) => (
            <ListRow
              key={u.id}
              avatar={<UserAvatarEl name={u.display_name} avatar={u.avatar} builtinAvatar={u.builtin_avatar} baseUrl={baseUrl} />}
              onClick={() => openUser(u.id, u.display_name)}
              label={
                <span className="row-title">
                  {u.display_name}
                  {u.online ? <span className="dot dot-online" aria-label="在线" /> : null}
                </span>
              }
              value={formatListTime(chat ? chat.last_active_at * 1000 : undefined)}
              hint={chat?.last_message_preview || '开始对话'}
              badge={chat && chat.unread_count > 0 ? chat.unread_count : undefined}
            />
          ))}
          {users.length === 0 && !loading ? (
            <ListRow label="暂无联系人" hint="在桌面端邀请成员后出现在这里" />
          ) : null}
        </ListSection>
      </div>
    </div>
  )
}
