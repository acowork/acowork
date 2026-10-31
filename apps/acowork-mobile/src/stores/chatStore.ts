/**
 * Per-agent VIEW state. The session list itself is owned by `agentStore` —
 * see the note there on why a second copy would break the `can_write` gate.
 *
 * The invariant this file exists to protect: a session switch is a
 * THREE-STEP ATOMIC MIGRATION, never a local page swap. Selecting a session
 * must (1) move the UI, (2) tell the Runtime `open_session` so it
 * re-points its active session on the wire, and (3) reload history over
 * HTTP. Skipping (2) leaves the Runtime streaming into the previous
 * session while the user reads the new one.
 *
 * Read-only sessions deliberately skip (2): `open_session` flips the
 * session's global Active/Closed lifecycle state, and a viewer has no
 * business opening someone else's session platform-wide. See ADR-086.
 */

import { create } from 'zustand'
import { useAgentStore } from './agentStore'
import type { ChatMessage, LiveStatus, MessagesWindow, SessionInfo, SessionSnapshot } from '../lib/types'
import {
  startWatching, stopWatching, setRealtimeDeps, pollNetworkState, POLL_INTERVAL_MS,
} from '../lib/realtime'
import type { RealtimeEvent } from '../lib/proto-wire'

export type { RealtimeEvent }
export { pollNetworkState, POLL_INTERVAL_MS }

export interface AgentViewState {
  /** The session the user is looking at. May be a read-only view. */
  activeSessionId: string | null
  messages: ChatMessage[]
  /** True while history is loading, to keep the bubble list stable. */
  loading: boolean
  /** False once a load failed; the UI must not pretend a session is empty. */
  loaded: boolean
  /** v1.2 §8: last known status of the foreground session (null = idle/unknown). */
  status: LiveStatus | null
  /** Server `message_count` at the moment the current `messages` were
   *  loaded; null until the first snapshot primes it. Drives reloads. */
  loadedCount: number | null
  /** ADR-050 window cursor (RAW entry math, ported from the Desktop chat
   *  store): the cache covers raw entries [messageOffset,
   *  messageOffset + messageLimit) of messageTotal. Null until the first
   *  window lands. `messageOffset > 0` is "there is earlier history". */
  messageOffset: number | null
  messageLimit: number | null
  messageTotal: number | null
  /** True while a "load earlier" page is in flight (edge-trigger guard). */
  isLoadingMore: boolean
  /** True while a send is in flight or failed; carries the draft for retry.
   *  `command` is the wire-side skill name the user picked at send time
   *  (see `lib/api.ts sendMessage`); the retry path restores it so a
   *  "skill-anchored" message replays the same skill, instead of silently
   *  dropping it. Empty string = no skill (matches the wire default). */
  pendingSend: { content: string; command: string; state: 'sending' | 'failed' } | null
  /** Which realtime channel is currently feeding this view (§8.0). */
  channel: 'ws' | 'polling' | null
  /** AskQuestion card — WS-channel only (polling cannot see the question). */
  question: { requestId: string; question: string; options: string[] } | null
  /** Tool details for the approval card (WS-channel enrichment). */
  approvalDetail: { requestId: string; toolName: string; action: string; riskLevel: string } | null
}

/** Statuses where the agent is doing work — the poll loop keeps running
 *  and the UI shows a status line instead of streaming (v1.1 §8). */
export function isActiveStatus(s: LiveStatus | null): boolean {
  if (!s) return false
  switch (s.status) {
    case 'llm_awaiting_first_chunk':
    case 'thinking':
    case 'llm_streaming':
    case 'tool_executing':
    case 'waiting_approval':
    case 'paused':
      return true
    default:
      return false
  }
}

interface ChatStore {
  agentStates: Record<string, AgentViewState>
  /** The agent whose conversation detail is on screen; null = the IM list. */
  selectedAgentId: string | null

  selectAgent: (agentId: string | null) => void

  /**
   * The only supported way to change session. Returns ok=false on failure
   * so the caller can revert the UI instead of leaving it lying.
   */
  openSession: (agentId: string, sessionId: string) => Promise<boolean>

  /** Create a session and open it. Defaults to Private (ADR-076). */
  createSession: (agentId: string, title?: string) => Promise<string | null>

  deleteSession: (agentId: string, sessionId: string) => Promise<boolean>

  /** v1.2 §8: foreground watch — start when a session is on screen. */
  startWatching: (agentId: string, sessionId: string) => void
  stopWatching: () => void

  /** Send (or retry the failed draft). `retry` reuses the pending content. */
  sendMessage: (agentId: string, sessionId: string, content?: string, command?: string) => Promise<boolean>

  /**
   * ADR-050 scroll-up pagination (the Desktop `loadBefore` equivalent):
   * fetch the page of raw entries immediately BEFORE the cached window and
   * merge it in. No-op when there is no cursor, no earlier history, or a
   * page is already in flight.
   */
  loadEarlier: (agentId: string) => Promise<void>

  /** Approve/deny the tool the session is waiting on (generic v1 card). */
  decideApproval: (agentId: string, sessionId: string, requestId: string, approved: boolean) => Promise<void>

  /** Answer an AskQuestion card (WS channel only, §8.3). */
  answerQuestion: (agentId: string, sessionId: string, requestId: string, answer: string) => Promise<boolean>
}

/** Exported for window-model tests (see chat-window.test.ts) — same shape. */
export const emptyView = (): AgentViewState => ({
  activeSessionId: null,
  messages: [],
  loading: false,
  loaded: false,
  status: null,
  loadedCount: null,
  messageOffset: null,
  messageLimit: null,
  messageTotal: null,
  isLoadingMore: false,
  pendingSend: null,
  channel: null,
  question: null,
  approvalDetail: null,
})

/** Page size for the scroll-up window fetches (matches the initial tail load). */
const WINDOW_LIMIT = 100

/** Injected by `lib/api.ts`; kept out of the store so tests stay pure. */
export interface ChatTransport {
  openSession(agentId: string, sessionId: string): Promise<void>
  /** ADR-050: no `opts.offset` = tail window (`tail=true`); with it = the
   *  forward page [offset, offset+limit). Returns the server-echoed cursor. */
  fetchMessages(agentId: string, sessionId: string, opts?: { offset?: number; limit?: number }): Promise<MessagesWindow>
  fetchSessions(agentId: string, page: number): Promise<{ items: SessionInfo[]; hasMore: boolean }>
  createSession(agentId: string, title?: string): Promise<SessionInfo>
  deleteSession(agentId: string, sessionId: string): Promise<void>
  /** v1.1 §8: the poll primitive — live status + authoritative message
   *  count of ONE foreground session. The count (not a status transition)
   *  drives history reloads, so a turn that starts and finishes between
   *  two ticks still lands. */
  fetchSnapshot(agentId: string, sessionId: string): Promise<SessionSnapshot>
  /** Send a user message; resolves with the client message id.
   *
   *  An execution-view send always goes to this session — the Chat binding is
   *  not an input here. Where the turn's OUTPUT goes (the bound chat's card and
   *  echo rows) is the Runtime's business, not the composer's. */
  send(
    agentId: string,
    sessionId: string,
    content: string,
    command?: string,
  ): Promise<string>
  /** v1 approval decision (generic card; tool details ride the WS channel). */
  approval(agentId: string, sessionId: string, requestId: string, approved: boolean): Promise<void>
  /** AskQuestion answer (§8.3): POST .../answer with the card's request id. */
  answer(agentId: string, sessionId: string, requestId: string, answer: string): Promise<void>
}

let transport: ChatTransport | null = null

/** Wire the real HTTP transport once, at app boot. */
export function setChatTransport(t: ChatTransport): void {
  transport = t
  // The polling-fallback primitive travels with the transport, so a test
  // that swaps transports never polls through a stale one.
  setRealtimeDeps({
    snapshot: (agentId, sessionId) => t.fetchSnapshot(agentId, sessionId),
  })
}

export const useChatStore = create<ChatStore>((set, get) => ({
  agentStates: {},
  selectedAgentId: null,

  selectAgent: (agentId) => set({ selectedAgentId: agentId }),

  openSession: async (agentId, sessionId) => {
    if (!transport) return false
    const prev = get().agentStates[agentId] ?? emptyView()
    // Read the row from the ONE owner of the session list, so the read-only
    // decision below uses the same `can_write` the UI renders.
    const target = useAgentStore.getState().getSession(agentId, sessionId)
    const readOnly = target?.can_write === false

    // Step 1: move the UI. Optimistic — reverted below if the load fails.
    // The polled status, cards and pending draft belong to the PREVIOUS
    // session; carrying them over would show a stale "thinking…" line or a
    // dead approval card.
    set((s) => ({
      agentStates: {
        ...s.agentStates,
        [agentId]: {
          ...prev, activeSessionId: sessionId, loading: true,
          status: null, loadedCount: null, pendingSend: null,
          question: null, approvalDetail: null,
        },
      },
    }))

    try {
      // Step 2: tell the Runtime, skipped for read-only sessions.
      if (!readOnly) await transport.openSession(agentId, sessionId)
      // Step 3: reload history — a fresh tail window, cursor and all (the
      // previous session's cached pages are gone with the old view anyway).
      const w = await transport.fetchMessages(agentId, sessionId)
      set((s) => ({
        agentStates: {
          ...s.agentStates,
          // Merge onto step 1 (not the stale `prev`): the cleared status and
          // pending draft belong to THIS session's view, not the old one.
          // `loadedCount: null` re-arms the freshness baseline — the first
          // tick after this load primes it from the server's count.
          [agentId]: { ...(s.agentStates[agentId] ?? emptyView()), messages: w.messages, messageOffset: w.offset, messageLimit: w.limit, messageTotal: w.total, loading: false, loaded: true, loadedCount: null },
        },
      }))
      return true
    } catch {
      // Revert to the previous session rather than showing a half-open state.
      // The revert must restore the PREVIOUS session id too: leaving the new
      // id active with the old messages would let the poll loop reload the
      // wrong session and paint over the failure.
      set((s) => ({
        agentStates: { ...s.agentStates, [agentId]: { ...prev, loading: false } } ,
      }))
      return false
    }
  },

  createSession: async (agentId, title) => {
    if (!transport) return null
    const created = await transport.createSession(agentId, title)
    useAgentStore.getState().mergeSessions(agentId, [created])
    await get().openSession(agentId, created.session_id)
    return created.session_id
  },

  deleteSession: async (agentId, sessionId) => {
    if (!transport) return false
    await transport.deleteSession(agentId, sessionId)
    const remaining = useAgentStore.getState().removeSession(agentId, sessionId)
    set((s) => {
      const prev = s.agentStates[agentId] ?? emptyView()
      // Deleting the session the user is looking at must not leave a dangling
      // active id pointing at history that is gone.
      const wasActive = prev.activeSessionId === sessionId
      return {
        agentStates: {
          ...s.agentStates,
          [agentId]: {
            ...prev,
            ...(wasActive
              ? { activeSessionId: remaining[0]?.session_id ?? null, messages: [], loaded: false }
              : {}),
          },
        },
      }
    })
    return true
  },

  /* ---------------- v1.2 §8: foreground watch ----------------
   *
   * The channel lives in `lib/realtime` (WS on relay, polling elsewhere);
   * this store owns only the RELOAD RULE: history is reloaded whenever the
   * authoritative `message_count` differs from the count the current view
   * was loaded at, or on an active → idle transition. Events from the WS
   * channel carry what polling cannot see — approval details and the
   * AskQuestion card (§8.1/§8.3).
   */

  startWatching: (agentId, sessionId) => {
    startWatching(agentId, sessionId, {
      onSnapshot: (snap) => applySnapshot(get, set, agentId, sessionId, snap),
      onEvent: (ev) => handleEvent(get, set, agentId, sessionId, ev),
      onChannel: (ch) => {
        set((s) => ({
          agentStates: {
            ...s.agentStates,
            [agentId]: { ...(s.agentStates[agentId] ?? emptyView()), channel: ch },
          },
        }))
      },
    })
  },

  stopWatching: () => {
    stopWatching()
  },

  sendMessage: async (agentId, sessionId, content, command) => {
    if (!transport) return false
    const view = get().agentStates[agentId] ?? emptyView()
    const text = (content ?? view.pendingSend?.content ?? '').trim()
    if (!text) return false
    // Retry path: when `content` is omitted we read the failed draft back
    // out of `pendingSend`, and we have to read `command` from there too
    // so the skill the user originally picked rides the retry. Otherwise
    // a "skill-anchored" message silently drops the skill on retry and
    // the agent gets a bare content replay.
    const cmd = command ?? view.pendingSend?.command ?? ''
    set((s) => ({
      agentStates: {
        ...s.agentStates,
        [agentId]: { ...(s.agentStates[agentId] ?? emptyView()), pendingSend: { content: text, command: cmd, state: 'sending' } },
      },
    }))
    try {
      // The server persists the user entry under this exact id, so the
      // window merge (mergeMessageWindow) collapses the optimistic bubble
      // into the authoritative copy instead of doubling it.
      // `command` rides the wire as the active skill name — see api.ts.
      //
      // One entry point: the session on screen. The bound Chat (if any) never
      // receives a copy of these words, so there is no second surface to keep
      // the turn on and no pin to ask for one.
      const mid = await transport.send(agentId, sessionId, text, cmd)
      // Optimistic bubble: the authoritative copy arrives with the idle
      // reload and merges over this entry; this keeps the echo instant.
      set((s) => {
        const v = s.agentStates[agentId] ?? emptyView()
        return {
          agentStates: {
            ...s.agentStates,
            [agentId]: {
              ...v,
              pendingSend: null,
              messages: [
                ...v.messages,
                { id: mid, role: 'user', content: text, created_at: Date.now(), kind: 'text' },
              ],
            },
          },
        }
      })
      // The turn may already be over by the time we get here; watch at least
      // once so the status line and the final reload are correct.
      get().startWatching(agentId, sessionId)
      return true
    } catch {
      set((s) => ({
        agentStates: {
          ...s.agentStates,
          [agentId]: { ...(s.agentStates[agentId] ?? emptyView()), pendingSend: { content: text, command: cmd, state: 'failed' } },
        },
      }))
      return false
    }
  },

  decideApproval: async (agentId, sessionId, requestId, approved) => {
    if (!transport) return
    await transport.approval(agentId, sessionId, requestId, approved)
    // The card is dismissed by the status leaving waiting_approval; clear the
    // WS-side detail immediately so a redelivered retained event cannot
    // re-arm a decided card.
    set((s) => ({
      agentStates: {
        ...s.agentStates,
        [agentId]: { ...(s.agentStates[agentId] ?? emptyView()), approvalDetail: null },
      },
    }))
  },

  answerQuestion: async (agentId, sessionId, requestId, answer) => {
    if (!transport) return false
    try {
      await transport.answer(agentId, sessionId, requestId, answer)
      set((s) => ({
        agentStates: {
          ...s.agentStates,
          [agentId]: { ...(s.agentStates[agentId] ?? emptyView()), question: null },
        },
      }))
      return true
    } catch {
      return false
    }
  },

  loadEarlier: async (agentId) => {
    if (!transport) return
    const view = get().agentStates[agentId]
    const sid = view?.activeSessionId
    if (!view || !sid || view.isLoadingMore) return
    // No cursor yet, or the cached window already reaches the oldest entry:
    // there is nothing earlier to fetch.
    if (view.messageOffset === null || view.messageOffset <= 0) return
    const offset = Math.max(0, view.messageOffset - WINDOW_LIMIT)
    set((s) => ({
      agentStates: { ...s.agentStates, [agentId]: { ...(s.agentStates[agentId] ?? emptyView()), isLoadingMore: true } },
    }))
    try {
      const w = await transport.fetchMessages(agentId, sid, { offset, limit: WINDOW_LIMIT })
      set((s) => {
        const cur = s.agentStates[agentId]
        // The session may have moved on while the page was in flight.
        if (!cur || cur.activeSessionId !== sid) return {}
        return { agentStates: { ...s.agentStates, [agentId]: { ...cur, ...applyWindow(cur, w) } } }
      })
    } catch {
      /* the next top-edge scroll retries */
    } finally {
      set((s) => ({
        agentStates: { ...s.agentStates, [agentId]: { ...(s.agentStates[agentId] ?? emptyView()), isLoadingMore: false } },
      }))
    }
  },
}))

/* ---------------- realtime handler internals (v1.2 §8) ---------------- */

type Get = () => ChatStore
type Set = (fn: (s: ChatStore) => Partial<ChatStore>) => void

/**
 * Content equality for the poll loop's status (the serde tagged union).
 * Every HTTP tick parses a fresh object, and screens key their follow
 * effects on the stored reference — replacing the old one with a
 * content-equal clone re-fired them on a 2 s timer (the "list snaps back
 * to bottom" bug). `detail` is small and both sides came from the same
 * server serializer, so a serialization compare is stable and exhaustive.
 */
function sameStatus(a: LiveStatus | null, b: LiveStatus | null): boolean {
  if (a === b) return true
  if (!a || !b) return false
  if (a.status !== b.status) return false
  const da = (a as { detail?: unknown }).detail
  const db = (b as { detail?: unknown }).detail
  if (da === db) return true
  return JSON.stringify(da ?? null) === JSON.stringify(db ?? null)
}

/**
 * Ported from the Desktop chat store (ADR-050): reconcile a server window
 * with the local cache. Server entries win on id collision (an optimistic
 * bubble carrying the server id collapses into the authoritative copy);
 * cache entries outside the window are preserved (earlier pages already
 * loaded); the result is sorted by timestamp — Array.sort is stable, so
 * rows sharing a millisecond keep server-before-cache order.
 */
export function mergeMessageWindow(cache: ChatMessage[], server: ChatMessage[]): ChatMessage[] {
  const byId = new Map<string, ChatMessage>()
  for (const m of server) byId.set(m.id, m)
  for (const m of cache) if (!byId.has(m.id)) byId.set(m.id, m)
  return [...byId.values()].sort((a, b) => a.created_at - b.created_at)
}

/**
 * Merge a fetched window into the view, or adopt it verbatim when the
 * cache cannot be reconciled (empty, or NOT contiguous with the cached
 * range). The Desktop store warns why this matters: unioning cursor math
 * over a sparse cache fabricates a `[0, total)` cursor, sets hasOlder
 * false, and permanently hides the missing head. Never merge across a
 * gap — replace, and let scroll-up re-page from the honest cursor.
 */
export function applyWindow(
  view: AgentViewState,
  w: MessagesWindow,
): Pick<AgentViewState, 'messages' | 'messageOffset' | 'messageLimit' | 'messageTotal'> {
  const prevStart = view.messageOffset
  const prevLimit = view.messageLimit
  if (view.messages.length === 0 || prevStart === null || prevLimit === null) {
    return { messages: w.messages, messageOffset: w.offset, messageLimit: w.limit, messageTotal: w.total }
  }
  const prevEnd = prevStart + prevLimit
  const contiguous = w.offset <= prevEnd && prevStart <= w.offset + w.limit
  if (!contiguous) {
    return { messages: w.messages, messageOffset: w.offset, messageLimit: w.limit, messageTotal: w.total }
  }
  const offset = Math.min(prevStart, w.offset)
  const end = Math.max(prevEnd, w.offset + w.limit)
  return {
    messages: mergeMessageWindow(view.messages, w.messages),
    messageOffset: offset,
    messageLimit: end - offset,
    // The count only ever grows (append-only JSONL); a page response may
    // lag a concurrent tick's total, so keep the larger observation.
    messageTotal: Math.max(view.messageTotal ?? 0, w.total),
  }
}

/**
 * Apply one snapshot (from either channel). The §8 reload rule:
 * the history is reloaded whenever the authoritative `message_count`
 * differs from the count the current view was loaded at, or when the
 * session transitions active → idle. The count check is stateless — a turn
 * that starts and finishes between two observations still changes it —
 * which closes the fast-reply race a transition-only rule had.
 */
async function applySnapshot(
  get: Get, set: Set,
  agentId: string, sessionId: string,
  snap: SessionSnapshot,
): Promise<void> {
  if (!transport) return
  const view = get().agentStates[agentId]
  if (!view || view.activeSessionId !== sessionId) return
  // Unchanged content keeps the stored reference, so downstream effects
  // that key on identity do not see a no-op tick as a change.
  const status = sameStatus(view.status, snap.status) ? view.status : snap.status
  const was = isActiveStatus(view.status)
  const now = isActiveStatus(status)
  const countChanged =
    snap.messageCount !== null && view.loadedCount !== null && snap.messageCount !== view.loadedCount
  const erroredNew = status?.status === 'errored' && view.status?.status !== 'errored'
  // Reload lands only on a QUIET observation (not active). Mid-turn the
  // raw-entry count changes on every tick, and each tail reload would
  // replace the view with the newest 100 RAW entries — inside a long tool
  // turn that window is a run of thought/tool lines and pushes every
  // older text bubble out of the window ("history shrinks to one explore
  // block"). While active the UI shows the status line (§8), so waiting
  // for the active→idle settle loses nothing but a stale mid-turn tail.
  const shouldReload = !now && (was || countChanged || erroredNew)
  if (shouldReload) {
    try {
      const w = await transport.fetchMessages(agentId, sessionId)
      set((s) => {
        const cur = s.agentStates[agentId] ?? emptyView()
        return {
          agentStates: {
            ...s.agentStates,
            [agentId]: { ...cur, ...applyWindow(cur, w), status, loadedCount: snap.messageCount },
          },
        }
      })
    } catch {
      /* a failed reload retries on the next observation */
    }
  } else {
    // Prime the count baseline on the first snapshot after a history load, so
    // the next change is detectable without trusting a transition.
    const loadedCount = view.loadedCount === null ? snap.messageCount : view.loadedCount
    // A no-change tick must not publish a new view object at all: rewrites
    // with identical content are what turned the poll into scroll hijacks
    // and needless re-renders.
    if (status === view.status && loadedCount === view.loadedCount) return
    set((s) => ({
      agentStates: {
        ...s.agentStates,
        [agentId]: {
          ...(s.agentStates[agentId] ?? emptyView()),
          status,
          loadedCount,
        },
      },
    }))
  }
}

/**
 * WS-channel events (§8.1). Turn-end signals force the same reload the count
 * rule would do; card events carry details polling cannot see.
 */
async function handleEvent(
  get: Get, set: Set,
  agentId: string, sessionId: string,
  ev: RealtimeEvent,
): Promise<void> {
  if (!transport) return
  const view = get().agentStates[agentId]
  if (!view || view.activeSessionId !== sessionId) return
  switch (ev.kind) {
    case 'done':
    case 'stopped':
    case 'error': {
      try {
        const [w, snap] = await Promise.all([
          transport.fetchMessages(agentId, sessionId),
          transport.fetchSnapshot(agentId, sessionId),
        ])
        set((s) => {
          const cur = s.agentStates[agentId] ?? emptyView()
          return {
            agentStates: {
              ...s.agentStates,
              [agentId]: { ...cur, ...applyWindow(cur, w), loadedCount: snap.messageCount, status: snap.status },
            },
          }
        })
      } catch {
        /* the count rule on the next observation retries */
      }
      break
    }
    case 'tool_approval_needed': {
      set((s) => ({
        agentStates: {
          ...s.agentStates,
          [agentId]: {
            ...(s.agentStates[agentId] ?? emptyView()),
            status: { status: 'waiting_approval', detail: { request_id: ev.requestId } },
            approvalDetail: { requestId: ev.requestId, toolName: ev.toolName, action: ev.action, riskLevel: ev.riskLevel },
          },
        },
      }))
      break
    }
    case 'ask_question': {
      const card = parseQuestion(ev.questionJson)
      set((s) => ({
        agentStates: {
          ...s.agentStates,
          [agentId]: {
            ...(s.agentStates[agentId] ?? emptyView()),
            question: { requestId: ev.requestId, question: card.question, options: card.options },
          },
        },
      }))
      break
    }
    default:
      break
  }
}

/** AskQuestionPayload.question_json: `{question, options?}` — tolerant parse. */
export function parseQuestion(json: string): { question: string; options: string[] } {
  try {
    const o = JSON.parse(json) as Record<string, unknown>
    const q = typeof o.question === 'string' ? o.question : typeof o.text === 'string' ? o.text : ''
    const opts = Array.isArray(o.options) ? o.options.map(String) : []
    return { question: q || json, options: opts }
  } catch {
    return { question: json, options: [] }
  }
}

