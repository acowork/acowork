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
import { isStaleParkedFact, type SessionStatusFact } from '../lib/sessionAttention'

/** Wake-up confirmation: the Runtime's `alive` verdict lands a moment after
 *  `/start` returns (over MQTT), so poll the directory until it flips or a
 *  bounded budget runs out. Cheap (directory-only) and self-terminating. */
const START_POLL_MS = 1_500
/**
 * Drop the session facts a freshly loaded listing proves stale, for one agent.
 *
 * The push half (`.../sessions/+/state`) is RETAINED, so the last status a
 * Runtime published survives the process that published it — a Runtime killed
 * mid-approval, or a session closed while its card was open, leaves a red dot
 * that no answer can ever clear. The listing's `is_active` is what contradicts
 * it (see `lib/sessionAttention.isStaleParkedFact`), so every page load doubles
 * as the repair pass. Rows the Runtime never reported residency for are left
 * exactly as they are.
 *
 * Why no session is exempt here, unlike Desktop's repair pass: this sweep runs
 * when the inbox is entered, and the inbox is a list — no session is on screen
 * to have a card open in. The one ordering that could wrongly retire a fresh
 * fact (a push landing between the listing snapshot and its response) costs a
 * dot that the next push re-establishes, while the ordering it defends against
 * — a retained publish from a Runtime that has since died — never heals on its
 * own. The asymmetry is deliberate: retire on the stronger evidence.
 *
 * Returns the input reference when nothing changes: an inbox refresh happens on
 * every entry, and a new object identity per refresh would repaint the list.
 */
function pruneStaleFacts(
  all: Record<string, Record<string, SessionStatusFact> | undefined>,
  agentId: string,
  items: SessionInfo[],
): Record<string, Record<string, SessionStatusFact> | undefined> {
  const cur = all[agentId]
  if (!cur) return all
  const stale = items.filter((i) => isStaleParkedFact(i, cur[i.session_id])).map((i) => i.session_id)
  if (stale.length === 0) return all
  const nextSessions = { ...cur }
  for (const sid of stale) delete nextSessions[sid]
  const next = { ...all }
  if (Object.keys(nextSessions).length === 0) delete next[agentId]
  else next[agentId] = nextSessions
  return next
}

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
  /**
   * agentId -> sessionId -> the session's lifecycle FACT, fed by the presence
   * socket's fleet-wide `.../sessions/+/state` subscription (parity with
   * Desktop's `chatStore.agentStates[*].sessionStates[*].sessionStatus`).
   *
   * The store keeps the fact and nothing derived from it. "is working" and
   * "is waiting for you" are two readings of the same fact — with different
   * rules (a 429 retry is working but not waiting) — and they live in
   * `lib/sessionAttention`, which the inbox applies with `useMemo`. Caching a
   * derived `Set` here is what made the two dots capable of disagreeing.
   */
  sessionStatuses: Record<string, Record<string, SessionStatusFact> | undefined>
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
   *  presence socket). Records the FACT; the inbox derives both its breathing
   *  working dot and its red attention dot from it (`lib/sessionAttention`),
   *  the same rule Desktop's sidebar applies. */
  applySessionState(agentId: string, sessionId: string, fact: SessionStatusFact): void
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
  sessionStatuses: {},
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
        sessionStatuses: pruneStaleFacts(s.sessionStatuses, agentId, items),
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

  applySessionState: (agentId, sessionId, fact) =>
    set((s) => {
      const cur = s.sessionStatuses[agentId]
      const prev = cur?.[sessionId]
      // Identity-preserving: a re-published identical status (the state topic is
      // retained and replayed on every reconnect) must not repaint the inbox.
      if (prev && prev.status === fact.status && prev.reason === fact.reason && prev.retry === fact.retry) {
        return s
      }
      // A session that goes idle holds no obligation, so its entry is dropped
      // rather than kept as a tombstone — the map only ever holds "something is
      // true right now", which is what both dots read.
      if (fact.status === 'idle') {
        if (!cur || !prev) return s
        const nextSessions = { ...cur }
        delete nextSessions[sessionId]
        const next = { ...s.sessionStatuses }
        if (Object.keys(nextSessions).length === 0) delete next[agentId]
        else next[agentId] = nextSessions
        return { sessionStatuses: next }
      }
      return {
        sessionStatuses: {
          ...s.sessionStatuses,
          [agentId]: { ...(cur ?? {}), [sessionId]: fact },
        },
      }
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
      // An agent that just went offline cannot be mid-turn nor be holding a
      // card open; drop its stale session facts so a mid-turn socket death
      // cannot breathe — or badge — forever.
      const dropStale = status === 'offline' && s.sessionStatuses[agentId] !== undefined
      const statusPatch = () => {
        if (!dropStale) return {}
        const next = { ...s.sessionStatuses }
        delete next[agentId]
        return { sessionStatuses: next }
      }
      if (!slot) return statusPatch()
      if (slot.info.status === status) return dropStale ? statusPatch() : s
      const info = { ...slot.info, status }
      // A directory refresh can still overwrite this with a verdict polled
      // before the transition — that is bounded (the next poll, and the
      // status topic's own heartbeat, both re-assert it within seconds).
      return {
        ...statusPatch(),
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
        sessionStatuses: pruneStaleFacts(s.sessionStatuses, agentId, items),
        agents: {
          ...s.agents,
          [agentId]: { ...slot, sessions: [...slot.sessions, ...items.filter((i) => !known.has(i.session_id))] },
        },
      }
    }),

  removeSession: (agentId, sessionId) => {
    const remaining = (get().agents[agentId]?.sessions ?? []).filter((s) => s.session_id !== sessionId)
    set((s) => {
      // A deleted session holds no obligation: its fact would otherwise keep a
      // red dot on an agent no longer running it.
      const cur = s.sessionStatuses[agentId]
      let sessionStatuses = s.sessionStatuses
      if (cur && cur[sessionId]) {
        const nextSessions = { ...cur }
        delete nextSessions[sessionId]
        const next = { ...s.sessionStatuses }
        if (Object.keys(nextSessions).length === 0) delete next[agentId]
        else next[agentId] = nextSessions
        sessionStatuses = next
      }
      return {
        sessionStatuses,
        agents: {
          ...s.agents,
          [agentId]: { ...(s.agents[agentId] ?? { info: { id: agentId, name: agentId, node_id: '' } }), sessions: remaining },
        },
      }
    })
    return remaining
  },

  getSession: (agentId, sessionId) => get().agents[agentId]?.sessions.find((s) => s.session_id === sessionId),
}))
