/**
 * Chat-session store — the mobile twin of Desktop's `chatSessionStore`
 * (design 27 §6.1/§6.5, plan Wave 5.5). The agent chat's PRIMARY surface is
 * a Chat session; execution sessions live behind the status card.
 *
 * Deliberate mobile deltas from the desktop version:
 * - NO transient streaming text. Mobile v1 renders complete messages only
 *   (design doc 25 §8: "no streaming, ever, in v1"); the in-flight turn is
 *   carried by the status card alone.
 * - NO chat-tab set. The pair has EXACTLY ONE Chat session (WeChat model,
 *   design 27 §5.4.4) — the screen opens the pair's default session and
 *   there is no second-session affordance on any client; execution sessions
 *   are reached through the status card / history, same rule as desktop's
 *   pinned chat tab.
 * - Freshness reuses the ONE doorbell the app already subscribes:
 *   `acowork/users/{me}/chat/changed` fans out here through
 *   `wakeAgentChat()` next to `wakeUserChat()` (main.tsx). Content-free
 *   signal → HTTP re-read, never a patched row.
 *
 * `projectInFlight` is copied verbatim from the desktop store — same read
 * projection, same tests (design 27 §6.2).
 */

import { create } from 'zustand'
import {
  fetchChatMessages,
  fetchDefaultChatSession,
  sendChatMessage,
} from '../lib/api'
import { useAuthStore } from './authStore'
import type { ChatMessageRow, ChatSessionInfo } from '../lib/types'

const PAGE = 50

/** The in-flight turn, projected from the rows — no send-time bookkeeping. */
export interface InFlightTurn {
  /** meta.message_id of the unanswered human `user_message`. */
  triggerId: string
  /** execution session id from the `turn_started` pointer, if it arrived. */
  execSid: string | null
}

interface AgentChatState {
  /** The pair's default Chat session id; null until resolved or failed. */
  sessionId: string | null
  /** True once `openAgent` resolved (or failed) — the screen's loading gate. */
  resolved: boolean
}

interface Window {
  /** Older pages, ascending, prepended by `loadOlder`. */
  older: ChatMessageRow[]
  /** Newest window, replaced wholesale on refresh. */
  tail: ChatMessageRow[]
  tailLoaded: number
  total: number
  loadingOlder: boolean
}

interface ChatSessionStore {
  byAgent: Record<string, AgentChatState>
  windows: Record<string, Window>
  inFlight: Record<string, InFlightTurn | null>
  sending: Record<string, boolean>

  openAgent(agentId: string): Promise<void>
  refresh(chatSessionId: string): Promise<void>
  loadOlder(chatSessionId: string): Promise<void>
  sendText(chatSessionId: string, text: string, inReplyTo?: string): Promise<boolean>
  /** The doorbell reaction: re-read the one session that can be on screen. */
  wake(): Promise<void>
}

/**
 * Read-time turn projection (§6.2): walk the window from the newest row
 * backwards — a trailing `user_message` (with `turn_started` pointers
 * allowed in between) means the turn has not produced its chat-side
 * terminal row yet, so it is in flight. Any agent-side row (terminal
 * `agent_message`, answered `interaction_request` card, failure receipt)
 * ends the turn. Position-based on purpose: a card answer continues the
 * ORIGINAL turn's terminal row (`in_reply_to` = the first trigger), which
 * an id-matching rule would never connect to the answer message.
 *
 * A trailing `user_message` counts as in flight only once its
 * `turn_started` pointer is in the window: the pointer is written by the
 * Runtime for every turn that actually runs, so a group message that
 * @-nobody (no intent, no pointer) never shows a false "in progress" —
 * at the cost of a sub-second quiet gap between send and pointer on 1:1.
 *
 * `system_notice` rows are bookkeeping, never a turn's start or end: the walk
 * skips every one of them (a `turn_started` pointer, an `interaction_resolved`
 * closure, anything added later), otherwise a notice landing after the live
 * trigger would read as "the round already finished".
 */
export function projectInFlight(rows: ChatMessageRow[]): InFlightTurn | null {
  const findPointer = (trigger: string): ChatMessageRow | undefined => {
    for (let i = rows.length - 1; i >= 0; i--) {
      const p = rows[i]!
      if (
        p.kind === 'system_notice' &&
        p.meta?.notice_type === 'turn_started' &&
        p.meta?.in_reply_to === trigger
      )
        return p
    }
    return undefined
  }
  for (let i = rows.length - 1; i >= 0; i--) {
    const r = rows[i]!
    if (r.kind === 'system_notice') continue
    if (r.kind === 'user_message') {
      const triggerId = r.meta?.message_id ?? ''
      if (!triggerId) return null
      let pointer = findPointer(triggerId)
      if (!pointer && r.meta?.in_reply_to) {
        // Card answer: the turn continues under the ORIGINAL trigger, whose
        // pointer is two hops away (answer → card row → its trigger).
        const card = rows.find(
          (x) => x.kind === 'interaction_request' && x.meta?.message_id === r.meta?.in_reply_to,
        )
        if (card?.meta?.in_reply_to) pointer = findPointer(card.meta.in_reply_to)
      }
      return pointer ? { triggerId, execSid: pointer.meta?.agent_session_id || null } : null
    }
    return null
  }
  return null
}

const emptyWindow = (): Window => ({ older: [], tail: [], tailLoaded: 0, total: 0, loadingOlder: false })

export const useChatSessionStore = create<ChatSessionStore>((set, get) => ({
  byAgent: {},
  windows: {},
  inFlight: {},
  sending: {},

  openAgent: async (agentId) => {
    const existing = get().byAgent[agentId]
    if (existing?.resolved) return
    try {
      const session: ChatSessionInfo = await fetchDefaultChatSession(agentId)
      set((s) => ({
        byAgent: { ...s.byAgent, [agentId]: { sessionId: session.session_id, resolved: true } },
      }))
      await get().refresh(session.session_id)
    } catch (e) {
      // Failed → the screen shows its own error; never spin forever.
      console.warn(`openAgent(${agentId}) failed:`, e)
      set((s) => ({
        byAgent: { ...s.byAgent, [agentId]: { sessionId: null, resolved: true } },
      }))
    }
  },

  refresh: async (chatSessionId) => {
    try {
      const page = await fetchChatMessages(chatSessionId, 0, PAGE)
      // The newest window, ascending; older pages already held stay put.
      set((s) => ({
        windows: {
          ...s.windows,
          [chatSessionId]: {
            ...(s.windows[chatSessionId] ?? emptyWindow()),
            tail: page.messages,
            tailLoaded: page.messages.length,
            total: page.total,
          },
        },
        inFlight: { ...s.inFlight, [chatSessionId]: projectInFlight(page.messages) },
      }))
    } catch {
      // hidden 404 / transient — keep the rendered window
    }
  },

  loadOlder: async (chatSessionId) => {
    const w = get().windows[chatSessionId]
    if (!w || w.loadingOlder || w.tailLoaded >= w.total) return
    set((s) => ({
      windows: { ...s.windows, [chatSessionId]: { ...w, loadingOlder: true } },
    }))
    try {
      const page = await fetchChatMessages(chatSessionId, w.tailLoaded, PAGE)
      set((s) => {
        const cur = s.windows[chatSessionId]
        if (!cur) return s
        return {
          windows: {
            ...s.windows,
            [chatSessionId]: {
              ...cur,
              older: [...page.messages, ...cur.older],
              tailLoaded: cur.tailLoaded + page.messages.length,
              loadingOlder: false,
            },
          },
        }
      })
    } catch {
      set((s) => {
        const cur = s.windows[chatSessionId]
        return cur ? { windows: { ...s.windows, [chatSessionId]: { ...cur, loadingOlder: false } } } : s
      })
    }
  },

  sendText: async (chatSessionId, text, inReplyTo) => {
    const me = useAuthStore.getState().me?.user_id
    if (!me || get().sending[chatSessionId]) return false
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: true } }))
    let ok = false
    try {
      await sendChatMessage(me, chatSessionId, text, inReplyTo)
      ok = true
    } catch (e) {
      console.warn('chat send failed:', e)
    } finally {
      set((s) => ({ sending: { ...s.sending, [chatSessionId]: false } }))
    }
    // The durable row exists before the delivery returns (§5.1), so one
    // re-read right after the response is enough — the doorbell covers
    // everything the sender's own device did not render.
    if (ok) await get().refresh(chatSessionId)
    return ok
  },

  wake: async () => {
    // The doorbell names nothing (no subscription ACL on the broker), and on
    // mobile at most one agent chat is ever on screen — refresh its session.
    for (const st of Object.values(get().byAgent)) {
      if (st.sessionId) await get().refresh(st.sessionId)
    }
  },
}))

/** Presence fan-out target: the chat/changed doorbell also wakes agent chats. */
export function wakeAgentChat(): void {
  void useChatSessionStore.getState().wake()
}
