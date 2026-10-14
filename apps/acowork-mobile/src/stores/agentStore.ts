/**
 * The agent + contact directory, and the SINGLE OWNER of the session list.
 *
 * Why the session list lives here and not in `chatStore`: two stores both
 * holding `sessions` is how `can_write` silently stops being enforced —
 * the composer would consult one array while the permission hook consults
 * the other, and the read-only gate would fail open. One list, one owner.
 *
 * `chatStore` keeps only per-agent *view* state (which session is open, its
 * messages, loading flags) and reads the session list from here.
 */

import { create } from 'zustand'
import { fetchLastMessage, fetchSessions, startAgent as startAgentApi } from '../lib/api'
import type { AgentSummary, NodeRow, SessionInfo, UserSummary } from '../lib/types'

/** Wake-up confirmation: the Runtime's `alive` verdict lands a moment after
 *  `/start` returns (over MQTT), so poll the directory until it flips or a
 *  bounded budget runs out. Cheap (directory-only) and self-terminating. */
const START_POLL_MS = 1_500
/** Runtime `SessionStatus` variants that mean "the agent is mid-turn" —
 *  the exact set Desktop's `isProcessing` (ADR-014) treats as non-idle.
 *  `errored` is deliberately OUT: an error is surfaced in the thread, not
 *  as a breathing dot claiming work is in flight. */
const WORKING_STATUSES = new Set([
  'llm_awaiting_first_chunk',
  'thinking',
  'llm_streaming',
  'tool_executing',
  'waiting_approval',
  'paused',
])
const START_POLL_LIMIT = 8

/** Injected by `lib/api.ts` wiring at boot; keeps the store HTTP-free. */
export interface DirectorySource {
  loadAgents(): Promise<AgentSummary[]>
  loadUsers(): Promise<UserSummary[]>
  loadNodes(): Promise<NodeRow[]>
}

let directory: DirectorySource | null = null

export function setDirectorySource(d: DirectorySource): void {
  directory = d
}

interface AgentStore {
  agents: Record<string, { info: AgentSummary; sessions: SessionInfo[] }>
  /** agentId -> sessionIds currently mid-turn, fed by the presence socket's
   *  `.../sessions/+/state` subscription (parity with Desktop's
   *  `busyAgentIds`). The inbox's breathing working dot reads this. */
  busySessions: Record<string, Set<string>>
  agentList: AgentSummary[]
  users: UserSummary[]
  /** Node topology for the inbox's per-node grouping. The server already
   *  filtered by can_view (ADR-087); the inbox renders ONLINE nodes only and
   *  hides an agent whose node is not in that set (no fallback bucket). */
  nodes: NodeRow[]
  /** True while the first directory load is in flight (list skeleton). */
  directoryLoading: boolean
  /**
   * Pagination cursor, owned HERE rather than in `chatStore` because the
   * session list lives here: a cursor that outlives the list it indexes, or
   * a page-1 load that forgets to reset it, shows "加载更多" on a one-page
   * agent forever.
   */
  sessionPage: Record<string, number>
  sessionsHasMore: Record<string, boolean>
  /** Ids of agents whose wake-up (`/start`) is in flight — the row shows a
   *  spinner and re-taps are deduped until it lands. Owned HERE because the
   *  agent list lives here and the row renders from this store. */
  startingAgentIds: Set<string>

  /** Pull agents + contacts; failure is reported, never swallowed silently. */
  refreshDirectory(): Promise<boolean>
  /** Page 1 of one agent's sessions, REPLACING the cached page, plus the
   *  IM preview of its most recent session. */
  refreshSessions(agentId: string): Promise<void>
  /** The next page, appended. No-op once the server says there is none. */
  loadMoreSessions(agentId: string): Promise<void>
  /**
   * The inbox load (§2.3): directory, then one session page per agent, then
   * a preview for each. Bounded fan-out — a 30-agent gateway on a phone
   * connection must not open 30 sockets at once.
   */
  refreshInbox(): Promise<boolean>

  /**
   * Wake a stopped agent (tap an offline row). No-op unless the row is
   * currently `offline` and not already starting — so a double tap cannot
   * fire two `/start` calls. Marks the row spinner, issues the start, then
   * polls the directory until the agent reports online (or the budget runs
   * out). Never navigates: the user re-taps once it is up.
   */
  startAgent(agentId: string): Promise<void>

  /**
   * Apply one pushed presence verdict (v1.2 §8.1). `status` is already mapped
   * to the directory's vocabulary at the transport boundary, so the store
   * never sees a wire field name.
   *
   * A verdict for an unlisted agent is ignored rather than invented — the
   * status topic carries no name, role or avatar to render a row from.
   */
  applyPresence(agentId: string, status: NonNullable<AgentSummary['status']>): void

  setAgents: (list: AgentSummary[]) => void
  setUsers: (list: UserSummary[]) => void
  setNodes: (list: NodeRow[]) => void
  /** Pull the node topology. Failure keeps the previous snapshot — a
   *  transient blip must not collapse the grouped inbox into an empty list
   *  (mirrors the Desktop's `fetchNodes` resilience). */
  fetchNodes(): Promise<void>
  /** Prepend/merge a page of session history into the owning agent's slot. */
  mergeSessions: (agentId: string, items: SessionInfo[]) => void
  /** Fleet session-state push (`acowork/agents/+/sessions/+/state` over the
   *  presence socket). The inbox's working dot reads `busySessions` — the
   *  same signal Desktop's sidebar derives `busyAgentIds` from. */
  applySessionState(agentId: string, sessionId: string, status: string): void
  /** Replace an agent's session list with a freshly loaded page 1. */
  setSessions: (agentId: string, items: SessionInfo[]) => void
  /** Fill the preview text of one session (the server has no such field). */
  setPreview: (agentId: string, sessionId: string, preview: string) => void
  /** Remove a session and return the remaining list, for `chatStore`. */
  removeSession: (agentId: string, sessionId: string) => SessionInfo[]
  /** Read a single session row, the one place callers should get `can_write`. */
  getSession: (agentId: string, sessionId: string) => SessionInfo | undefined
}

export const useAgentStore = create<AgentStore>((set, get) => ({
  agents: {},
  busySessions: {},
  agentList: [],
  users: [],
  nodes: [],
  directoryLoading: false,
  sessionPage: {},
  sessionsHasMore: {},
  startingAgentIds: new Set<string>(),

  refreshDirectory: async () => {
    if (!directory) return false
    set({ directoryLoading: true })
    try {
      const [agents, users] = await Promise.all([directory.loadAgents(), directory.loadUsers()])
      get().setAgents(agents)
      get().setUsers(users)
      return true
    } catch {
      return false
    } finally {
      set({ directoryLoading: false })
    }
  },

  refreshSessions: async (agentId) => {
    try {
      const { items, hasMore } = await fetchSessions(agentId, 1)
      get().setSessions(agentId, items)
      set((s) => ({ sessionPage: { ...s.sessionPage, [agentId]: 1 }, sessionsHasMore: { ...s.sessionsHasMore, [agentId]: hasMore } }))
      // The preview is the last message of the newest session. The backend's
      // SessionSummary has no such field, so it costs one extra tail=1 call —
      // only for the top row, which is the only one the inbox shows.
      const top = items[0]
      if (top && (top.message_count ?? 0) > 0) {
        fetchLastMessage(agentId, top.session_id)
          .then((p) => {
            if (p) get().setPreview(agentId, top.session_id, p)
          })
          .catch(() => undefined)
      }
    } catch {
      /* a stale list beats an error screen for a preview fetch */
    }
  },

  loadMoreSessions: async (agentId) => {
    if ((get().sessionsHasMore[agentId] ?? true) === false) return
    const page = (get().sessionPage[agentId] ?? 1) + 1
    try {
      const { items, hasMore } = await fetchSessions(agentId, page)
      get().mergeSessions(agentId, items)
      set((s) => ({ sessionPage: { ...s.sessionPage, [agentId]: page }, sessionsHasMore: { ...s.sessionsHasMore, [agentId]: hasMore } }))
    } catch {
      /* leave the cursor alone so the row can be tapped again */
    }
  },

  refreshInbox: async () => {
    const ok = await get().refreshDirectory()
    // Node topology refresh rides on the inbox load: entering the list
    // re-reads /api/nodes, so a node that was offline and has come back
    // reappears on the next visit (the no-MQTT realtime substitute).
    void get().fetchNodes()
    const ids = get().agentList.map((a) => a.id)
    // 4 at a time: enough to feel instant on LTE, polite enough that a
    // large gateway cannot starve the phone's socket budget.
    const BATCH = 4
    for (let i = 0; i < ids.length; i += BATCH) {
      await Promise.all(ids.slice(i, i + BATCH).map((id) => get().refreshSessions(id)))
    }
    return ok
  },

  startAgent: async (agentId) => {
    const s = get()
    // Dedup gate + a guard against starting something not known offline.
    if (s.startingAgentIds.has(agentId)) return
    if (s.agentList.find((a) => a.id === agentId)?.status !== 'offline') return
    set((st) => ({ startingAgentIds: new Set(st.startingAgentIds).add(agentId) }))
    try {
      await startAgentApi(agentId)
      let online = false
      for (let i = 0; i < START_POLL_LIMIT; i++) {
        await new Promise((r) => setTimeout(r, START_POLL_MS))
        await get().refreshDirectory()
        const info = get().agentList.find((a) => a.id === agentId)
        if (info && info.status !== 'offline') {
          online = true
          break
        }
      }
      // Sessions only became listable once the Runtime was up (the offline
      // load returned nothing). Load them now so the row's title/preview
      // appears immediately instead of only after a leave-and-return.
      if (online) await get().refreshSessions(agentId)
    } catch {
      /* start rejected (permission / node down): leave it offline, the
         spinner clears, and the row can be tapped again */
    } finally {
      set((st) => {
        const next = new Set(st.startingAgentIds)
        next.delete(agentId)
        return { startingAgentIds: next }
      })
    }
  },

  setSessions: (agentId, items) =>
    set((s) => {
      const slot = s.agents[agentId] ?? { info: { id: agentId, name: agentId, node_id: '' }, sessions: [] }
      // Keep a preview already fetched this session: the server cannot give
      // it back, and dropping it would blank the inbox row on every refresh.
      const previews = new Map(slot.sessions.map((x) => [x.session_id, x.last_message]))
      return {
        agents: {
          ...s.agents,
          [agentId]: {
            ...slot,
            sessions: items.map((x) => ({ ...x, last_message: x.last_message ?? previews.get(x.session_id) ?? null })),
          },
        },
      }
    }),

  setPreview: (agentId, sessionId, preview) =>
    set((s) => {
      const slot = s.agents[agentId]
      if (!slot) return s
      return {
        agents: {
          ...s.agents,
          [agentId]: {
            ...slot,
            sessions: slot.sessions.map((x) => (x.session_id === sessionId ? { ...x, last_message: preview } : x)),
          },
        },
      }
    }),

  applySessionState: (agentId, sessionId, status) =>
    set((s) => {
      const working = WORKING_STATUSES.has(status)
      const cur = s.busySessions[agentId]
      if (!working) {
        if (!cur || !cur.has(sessionId)) return s
        const nextSet = new Set(cur)
        nextSet.delete(sessionId)
        const next = { ...s.busySessions }
        if (nextSet.size === 0) delete next[agentId]
        else next[agentId] = nextSet
        return { busySessions: next }
      }
      if (cur?.has(sessionId)) return s
      const nextSet = new Set(cur ?? [])
      nextSet.add(sessionId)
      return { busySessions: { ...s.busySessions, [agentId]: nextSet } }
    }),

  setAgents: (list) =>
    set((s) => {
      // Preserve already-fetched session pages when the directory refreshes,
      // so a reconnect does not wipe the session switcher's list.
      const agents: AgentStore['agents'] = {}
      for (const a of list) {
        agents[a.id] = { info: a, sessions: s.agents[a.id]?.sessions ?? [] }
      }
      return { agentList: list, agents }
    }),

  applyPresence: (agentId, status) =>
    set((s) => {
      const slot = s.agents[agentId]
      // An agent that just went offline cannot be working; drop its stale
      // session states so a mid-turn socket death cannot breathe forever.
      const dropBusy = status === 'offline' && s.busySessions[agentId] !== undefined
      const busyPatch = () => {
        if (!dropBusy) return {}
        const next = { ...s.busySessions }
        delete next[agentId]
        return { busySessions: next }
      }
      if (!slot) return busyPatch()
      if (slot.info.status === status) return dropBusy ? busyPatch() : s
      const info = { ...slot.info, status }
      // A directory refresh can still overwrite this with a verdict polled
      // before the transition — that is bounded (the next poll, and the
      // status topic's own heartbeat, both re-assert it within seconds).
      return {
        ...busyPatch(),
        agentList: s.agentList.map((a) => (a.id === agentId ? info : a)),
        agents: { ...s.agents, [agentId]: { ...slot, info } },
      }
    }),

  setUsers: (list) => set({ users: list }),

  setNodes: (list) => set({ nodes: list }),

  fetchNodes: async () => {
    if (!directory) return
    try {
      get().setNodes(await directory.loadNodes())
    } catch {
      /* keep the previous snapshot — a blip must not collapse the grouped inbox */
    }
  },

  mergeSessions: (agentId, items) =>
    set((s) => {
      const slot = s.agents[agentId] ?? { info: { id: agentId, name: agentId, node_id: '' }, sessions: [] }
      const known = new Set(slot.sessions.map((x) => x.session_id))
      return {
        agents: {
          ...s.agents,
          [agentId]: { ...slot, sessions: [...slot.sessions, ...items.filter((i) => !known.has(i.session_id))] },
        },
      }
    }),

  removeSession: (agentId, sessionId) => {
    const remaining = (get().agents[agentId]?.sessions ?? []).filter((s) => s.session_id !== sessionId)
    set((s) => ({
      agents: {
        ...s.agents,
        [agentId]: { ...(s.agents[agentId] ?? { info: { id: agentId, name: agentId, node_id: '' } }), sessions: remaining },
      },
    }))
    return remaining
  },

  getSession: (agentId, sessionId) => get().agents[agentId]?.sessions.find((s) => s.session_id === sessionId),
}))
