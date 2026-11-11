/**
 * Chat-session store — the mobile twin of Desktop's `chatSessionStore`
 * (design 27 §6.1/§6.5, plan Wave 5.5). The agent chat's PRIMARY surface is
 * a Chat session — a durable EVENT LOG; execution sessions live behind an
 * event row's "view run" drill, never as a live status mirror.
 *
 * Deliberate mobile deltas from the desktop version:
 * - NO transient streaming text. Mobile v1 renders complete messages only
 *   (design doc 25 §8: "no streaming, ever, in v1").
 * - NO chat-tab set. The pair has EXACTLY ONE Chat session (WeChat model,
 *   design 27 §5.4.4) — the screen opens the pair's default session and
 *   there is no second-session affordance on any client; execution sessions
 *   are reached through an event row's drill / history, same rule as
 *   desktop's pinned chat tab.
 * - Freshness reuses the ONE doorbell the app already subscribes:
 *   `acowork/users/{me}/chat/changed` fans out here through
 *   `wakeAgentChat()` next to `wakeUserChat()` (main.tsx). Content-free
 *   signal → HTTP re-read, never a patched row.
 */

import { create } from 'zustand'
import {
  fanoutMentions,
  fetchChatMessages,
  fetchDefaultChatSession,
  sendChatMessage,
} from '../lib/api'
import { TURN_INFLIGHT_DEADLINE_MS, type InflightTurn } from '../lib/chatTurnInflight'
import { useAuthStore } from './authStore'
import type { ChatMessageRow, ChatSessionInfo } from '../lib/types'

const PAGE = 50

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
  /** The POST this device has in flight. Deliberately NOT the wait-line / lock
   *  signal on its own: a POST answers as soon as delivery lands (~150 ms),
   *  while the turn's first durable row can be a minute behind it — that window
   *  is `inflight`'s job. */
  sending: Record<string, boolean>
  /** The turn this device sent and has not seen an outcome for, per Chat
   *  session — ADR-106 D9's first-person claim. Written from the send's own
   *  response (the id the Gateway gave the human's row), dropped when the
   *  surface sees a row answer it (`settleTurn`) or when the deadline passes
   *  (`expireTurn`). In-memory only: a reload — or returning to this screen —
   *  has no send of its own to claim, and so shows no wait line. */
  inflight: Record<string, InflightTurn | undefined>
  /** Stop claiming a turn because the transcript already answers it. */
  settleTurn(chatSessionId: string, messageId: string): void
  /** Stop claiming a turn because its outcome rows never arrived. */
  expireTurn(chatSessionId: string, messageId: string): void

  openAgent(agentId: string): Promise<void>
  /** Groups currently on screen (keyed by session id) — the doorbell wakes
   *  these windows too. The twin of Desktop's `openGroupIds`. */
  openGroupIds: Record<string, true>
  /** Open a group's transcript. The window is keyed by the session id, so
   *  the paging / refresh machinery is shared with the 1:1 surface verbatim. */
  openGroup(chatSessionId: string): Promise<void>
  /** Send into a group: every send takes the intent channel, which writes the
   *  ONE row and routes per target (design 27 §5.3.1). `targets` carries the
   *  @-ed agents; an EMPTY list is the positive instruction ADR-105 — the
   *  Gateway delivers to the group's default responder, or archives when the
   *  group has none. `inReplyTo` (a block's in-place reply, or a round's
   *  reference row) takes the level-1 `chat-message` branch instead, which the
   *  backend also accepts for group sessions (design 27 §5.5.4). */
  sendGroupText(
    chatSessionId: string,
    text: string,
    targets: string[],
    inReplyTo?: string,
    attachmentIds?: string[],
    address?: ChatAddress,
    /** See Desktop's `sendGroupText` — the ids this send is expected to reach,
     *  read off the ladder's inputs at send time (ADR-106 revision 2026-11-08). */
    responders?: string[],
  ): Promise<boolean>
  refresh(chatSessionId: string): Promise<void>
  loadOlder(chatSessionId: string): Promise<void>
  sendText(
    chatSessionId: string,
    text: string,
    inReplyTo?: string,
    attachmentIds?: string[],
    address?: ChatAddress,
    /** Agent instances this send is expected to reach (ADR-106 revision
     *  2026-11-08) — recorded on the claim so the surface can render the
     *  turn header. For a 1:1 chat this is its peer. */
    responders?: string[],
  ): Promise<boolean>
  /** The doorbell reaction: re-read the one session that can be on screen. */
  wake(): Promise<void>
}

const emptyWindow = (): Window => ({ older: [], tail: [], tailLoaded: 0, total: 0, loadingOlder: false })

/** ADR-106 D6/D7's explicit address — the fact the human supplied, which no
 *  client and no model may re-decide. On the 1:1 surface the first level of
 *  the `@` is degenerate: the pick is a single session id, or the refusal to
 *  continue anything. On the group surface the first level is the mention
 *  list, so the same fields take their per-agent forms (`targetSessions`,
 *  `forceNewFor`). It is sent only when the composer (or the answer to a
 *  choice card) actually carries one — an absent address is what puts a turn on
 *  the ladder's lower levels. */
export interface ChatAddress {
  /** 1:1 form of level 2: the session the human named for this agent. */
  targetSession?: string
  /** Group form of level 2 (design 27 §5.3.1): agent instance id -> the
   *  session named for THAT agent. Keys must be inside the send's targets.
   *  The `@`-revision 2026-11-04 retired the human path off this field for
   *  typed picks (they are `refs` now); it survives on the addressing-card
   *  answer, where the human genuinely chooses a delivery. */
  targetSessions?: Record<string, string>
  /** D6's cheap escape beside every suggestion: continue nothing, new work. */
  forceNew?: boolean
  /** Group form of the same refusal, per agent instance id: one utterance
   *  fans out to several agents, and refusing continuation for one must not
   *  quietly start a new session for the others. */
  forceNewFor?: string[]
  /** The id of the row being RE-delivered. Answering a `session_choice` /
   *  `confirm_continue` card re-sends the human's own utterance, now addressed,
   *  and the backend dedupes on this id — so the transcript keeps one message
   *  instead of drawing the same sentence twice. */
  messageId?: string
  /** ADR-106 revision 2026-11-04: the `@session` runs attached as quoted
   *  material for this utterance. A typed `@` is MATERIAL now, not a delivery:
   *  the picker hands back the picked coordinates, which ride the send as
   *  `refs` and are pinned to their round by the Gateway. 1:1 form: every ref
   *  names this agent's own session. Absent = no material. */
  refs?: ChatRefPick[]
}

/** One reference the human attached: the run quoted as material. `roundId`/
 *  `terminal` are only present on a card-answer echo — a fresh pick sends just
 *  the `{agent, session}` coordinate and lets the Gateway pin the round. */
export interface ChatRefPick {
  agentInstanceId: string
  sessionId: string
  roundId?: string
  terminal?: boolean
}

export const useChatSessionStore = create<ChatSessionStore>((set, get) => ({
  byAgent: {},
  openGroupIds: {},
  windows: {},
  sending: {},
  inflight: {},

  settleTurn: (chatSessionId, messageId) => dropTurn(chatSessionId, messageId),

  expireTurn: (chatSessionId, messageId) => dropTurn(chatSessionId, messageId),

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

  openGroup: async (chatSessionId) => {
    set((s) => ({ openGroupIds: { ...s.openGroupIds, [chatSessionId]: true } }))
    await get().refresh(chatSessionId)
  },

  sendGroupText: async (chatSessionId, text, targets, inReplyTo, attachmentIds, address, responders) => {
    const me = useAuthStore.getState().me?.user_id
    if (!me || get().sending[chatSessionId]) return false
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: true } }))
    let ok = false
    try {
      // Which row this send owes a claim on, once the response says somebody
      // took it. `null` for a send nothing was delivered to.
      let claimed: string | null = null
      if (inReplyTo) {
        // Reply to a round's block: the 1:1 channel resolves the reference row
        // into its agent and execution session (ADR-106 D6 level 1); group
        // sessions are accepted for exactly this case (design 27 §5.5.4). A
        // level-2 address cannot ride this branch — level 1 already named the
        // session, and the backend drops `force_new` wherever an address exists.
        const resp = await sendChatMessage(me, chatSessionId, text, inReplyTo, attachmentIds, address)
        if (resp.status === 'delivered') claimed = resp.message_id
      } else {
        // Every other group send takes the one entry: `targets` carries the
        // @-ed agents, and an empty list asks the Gateway to apply the group's
        // default responder (ADR-105) or archive. Splitting the two cases
        // client-side is what used to make an un-addressed reply vanish.
        const resp = await fanoutMentions(me, chatSessionId, text, targets, attachmentIds, address)
        // One row, N deliveries: the claim is on the ROW's id, and any target
        // that took the turn owes that row an answer. `archived` (no target)
        // and an all-failed fan-out owe none — a failed target already wrote
        // its own receipt server-side, and an archived row is the whole outcome.
        if ((resp.results ?? []).some((r) => r.status === 'delivered')) {
          claimed = resp.message_id
        }
      }
      ok = true
      if (claimed) armTurn(chatSessionId, claimed, responders)
    } catch (e) {
      console.warn('[chatSessionStore] group send failed:', e)
    } finally {
      set((s) => ({ sending: { ...s.sending, [chatSessionId]: false } }))
    }
    if (ok) await get().refresh(chatSessionId)
    return ok
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

  sendText: async (chatSessionId, text, inReplyTo, attachmentIds, address, responders) => {
    const me = useAuthStore.getState().me?.user_id
    if (!me || get().sending[chatSessionId]) return false
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: true } }))
    let ok = false
    try {
      const resp = await sendChatMessage(me, chatSessionId, text, inReplyTo, attachmentIds, address)
      ok = true
      // The claim starts here, from the id the Gateway gave THIS row. A failed
      // delivery is not claimed: that arm writes no receipt into the Chat, so a
      // claim would wait for a row that cannot exist and lock the composer for
      // the whole deadline.
      if (resp.status === 'delivered') armTurn(chatSessionId, resp.message_id, responders)
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
    // mobile at most one agent chat and one group are ever on screen — refresh
    // what the store knows is open.
    for (const st of Object.values(get().byAgent)) {
      if (st.sessionId) await get().refresh(st.sessionId)
    }
    for (const groupId of Object.keys(get().openGroupIds)) {
      await get().refresh(groupId)
    }
  },
}))

/** Presence fan-out target: the chat/changed doorbell also wakes agent chats. */
export function wakeAgentChat(): void {
  void useChatSessionStore.getState().wake()
}

// ── The in-flight turn claim (ADR-106 D9) ──────────────────────────────
//
// Kept in the store rather than in the screen's state because it is a fact
// about the SEND, not about what is mounted: navigating to another screen and
// back must not resurrect a wait line the transcript never answered, and the
// deadline must still run while no Chat surface is on screen. The screen only
// READS the claim and hands it back once a row answers it (`settleTurn`).
//
// A release is matched against the messageId it may clear so a stale deadline
// cannot drop a fresher turn's claim. `TURN_INFLIGHT_DEADLINE_MS` is the
// backstop for the case the surface cannot see: a Runtime that died between
// delivery and round open writes no outcome row, and without a deadline the
// composer would stay locked for good.

const turnTimers = new Map<string, ReturnType<typeof setTimeout>>()

/** Claim the turn this device just sent, and arm its deadline. */
function armTurn(chatSessionId: string, messageId: string, responders?: string[]): void {
  if (!messageId) return
  useChatSessionStore.setState((s) => ({
    inflight: {
      ...s.inflight,
      [chatSessionId]: {
        messageId,
        at: Date.now(),
        ...(responders && responders.length > 0 ? { responders } : {}),
      },
    },
  }))
  const prev = turnTimers.get(chatSessionId)
  if (prev !== undefined) clearTimeout(prev)
  turnTimers.set(
    chatSessionId,
    setTimeout(() => {
      turnTimers.delete(chatSessionId)
      dropTurn(chatSessionId, messageId)
    }, TURN_INFLIGHT_DEADLINE_MS),
  )
}

/** Release the claim — only if `messageId` is still the one being claimed. */
function dropTurn(chatSessionId: string, messageId: string): void {
  const cur = useChatSessionStore.getState().inflight[chatSessionId]
  if (!cur || cur.messageId !== messageId) return
  const timer = turnTimers.get(chatSessionId)
  if (timer !== undefined) {
    clearTimeout(timer)
    turnTimers.delete(chatSessionId)
  }
  useChatSessionStore.setState((s) => ({
    inflight: { ...s.inflight, [chatSessionId]: undefined },
  }))
}
