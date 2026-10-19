/**
 * Chat-session store — the desktop primary Agent surface (design 27 §6,
 * plan Wave 5). Each (user, agent) pair has EXACTLY ONE Chat session —
 * the WeChat model: the conversation is the pinned first tab in the Agent
 * page's tab strip, titled with the agent's name, never closable.
 * Execution sessions are ordinary additional tabs in the SAME strip: they
 * open and close freely WITHOUT replacing the chat surface — `mode` only
 * picks which surface the tab strip shows, it never destroys the other's
 * tabs.
 *
 * Desktop deltas over the mobile twin (`acowork-mobile/src/stores/
 * chatSessionStore.ts`):
 * - Tab strip with execution sessions alongside the pinned chat tab.
 *   `mode` per agent: 'chat' (this store's surface) or 'exec' (the
 *   ChatPanel embedded in the Agent page shell).
 * - NO transient streaming text in v1: the in-flight turn is carried by
 *   the status card, and its live detail is one click away in the exec
 *   view. `ponytail:` ceiling — the chat view updates at doorbell
 *   cadence, not token cadence; upgrade path is folding the existing
 *   `chatAdapterStore` stream into the status card.
 *
 * Freshness is push-first: `acowork/users/{me}/chat/changed` (the same
 * doorbell the human↔human inbox rides, published by the user service
 * after every append) fans out here from `chatStore`'s single MQTT
 * listener. Content-free signal → HTTP re-read, never a patched row.
 */

import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import { useAuthStore } from "./authStore";

// ── Wire types (mirror `chat_sessions_api.rs` responses) ──────────────

/** One durable chat row. Mirrors `chat_sessions_api::MessageRow`. */
export interface ChatMessageRow {
  seq: number;
  ts: number;
  from: string;
  /** user_message | agent_message | interaction_request | system_notice */
  kind: string;
  body: string;
  to: string;
  meta?: {
    message_id?: string;
    conversation_id?: string;
    in_reply_to?: string;
    status?: string;
    repo?: string;
    notice_type?: string;
    /** Routing-resolved execution session that HANDLED a request — set on
     *  the `turn_started` pointer and `interaction_request` card. */
    agent_session_id?: string;
    /** The SENDER's execution session (design 27 `source_agent_session_id`) —
     *  on a 1:1 `agent_message` this is the exec session that produced the
     *  reply, so every agent message (finished or empty) can drill into it. */
    sender_session_id?: string;
    interaction?: {
      kind: string;
      request_id: string;
      question?: string;
      options?: string[];
    };
  };
  attachment_ids?: string[];
  attachments?: Array<{ id: string; filename: string; mime: string; size: number }>;
}

/** `GET /api/chat-sessions/...` session envelope. */
export interface ChatSessionInfo {
  session_id: string;
  kind: string;
  title: string;
  project_id?: string;
  /** The Chat surface's durable seed draft (design 27 §4.5) — the server
   *  row the store's `draft` slot mirrors. Absent = no draft ever saved. */
  session_config?: ChatSessionConfig;
  created_at: number;
  last_active_at: number;
  message_count: number;
  participants?: Array<{ id: string; identity?: string }>;
  /** `participant_id -> unread count` (design 27 §4.6). Only the list
   *  endpoint carries it; reading a session's messages clears mine
   *  server-side. */
  unread?: Record<string, number>;
}

export interface ChatMessagesPage {
  messages: ChatMessageRow[];
  total: number;
}

// ── HTTP layer ─────────────────────────────────────────────────────────
//
// Same shape as `user-chat-api.ts` (plain fetch against the Gateway), but
// these paths ARE identity-bearing: the Gateway's user proxy verifies the
// bearer token and injects the caller downstream, so every request here
// carries it. No token (local mode) → the proxy 503s → `openAgent` fails →
// AgentPage falls back to the old ChatPanel. That is also the rollback
// lever (runbook: intent-chat-session-rollback).

async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(`${getGatewayUrl()}${path}`, {
    method,
    headers: {
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
    },
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  });
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new Error(`${method} ${path} → ${resp.status} ${text.slice(0, 200)}`);
  }
  if (method === "DELETE") return undefined as T;
  return resp.json() as Promise<T>;
}

/** The caller's default 1:1 session with `agentId`, created on first call
 *  (idempotent server-side). */
function fetchDefaultChatSession(agentId: string): Promise<ChatSessionInfo> {
  return req<ChatSessionInfo>(
    "GET",
    `/api/chat-sessions/default?pair=${encodeURIComponent(agentId)}`,
  );
}

/** Whole-replace write of the Chat surface's seed draft (design 27 §4.5).
 *  Durable on the session row — NOT a frontend-local preference: the
 *  response carries the normalized value the store adopts as its cache.
 *  Participation is the permission server-side (hidden 404 for outsiders). */
function patchChatSessionConfig(
  chatSessionId: string,
  cfg: ChatSessionConfig,
): Promise<ChatSessionInfo> {
  return req<ChatSessionInfo>(
    "PATCH",
    `/api/chat-sessions/${encodeURIComponent(chatSessionId)}/session-config`,
    cfg,
  );
}

/** One page of a Chat session. `offset` counts BACK from the newest row,
 *  so `offset=0` is the newest window in ascending order and
 *  `offset=loaded` is the next-older page (same semantics as the runtime's
 *  `read_messages`). */
function fetchChatMessages(id: string, offset: number, limit = 50): Promise<ChatMessagesPage> {
  return req<ChatMessagesPage>(
    "GET",
    `/api/chat-sessions/${encodeURIComponent(id)}/messages?offset=${offset}&limit=${limit}`,
  );
}

/** Design 27 §4.5 initiator `session_config` suggestion carried on a chat
 *  message that opens a new execution turn. Mirrors the Runtime `SessionSeed`
 *  wire shape: string-valued keys, all optional. The Runtime clamps each to
 *  the target agent's allowed set at execution-session creation (§13-6) — a
 *  value outside that set degrades to the agent's default, never an error. */
export interface ChatSessionConfig {
  model?: string;
  provider?: string;
  workspace?: string;
  reasoning_effort?: string;
}

/** Post one turn into a Chat session. Goes through the intent channel
 *  (`POST /api/intents/chat-message`), NOT the session-scoped write: the
 *  durable row lands before delivery resolves (design 27 §5.1), and only
 *  this channel starts the Agent's turn. `inReplyTo` is a card's
 *  `message_id` when answering a decision card (§5.5.4).
 *
 *  `sessionConfig` is the initiator's `session_config` suggestion (design
 *  27 §4.5): model / workspace / reasoning-effort chosen on the Chat
 *  surface before sending. It rides the intent envelope and seeds the
 *  execution session at creation (or triggers a T3 rotation if a mapped
 *  session already runs under a different config). `undefined` = use the
 *  agent's own defaults. */
function sendChatMessage(
  me: string,
  chatSessionId: string,
  text: string,
  inReplyTo?: string,
  sessionConfig?: ChatSessionConfig,
  attachmentIds?: string[],
): Promise<void> {
  return req<void>("POST", "/api/intents/chat-message", {
    chat_session_id: chatSessionId,
    from_user: me,
    text,
    in_reply_to: inReplyTo ?? "",
    ...(sessionConfig ? { session_config: sessionConfig } : {}),
    ...(attachmentIds && attachmentIds.length > 0 ? { attachment_ids: attachmentIds } : {}),
  });
}

/** Archive-only group message: one `user_message` row, no delivery.
 *  (Agents stay silent unless @-mentioned — design 27 §5.3.) */
function postGroupMessage(chatSessionId: string, text: string): Promise<void> {
  return req<void>("POST", `/api/chat-sessions/${encodeURIComponent(chatSessionId)}/messages`, {
    body: text,
    to: "",
  });
}

/** @-fan-out in a group (design 27 §5.3.1): the Gateway writes the ONE
 *  `user_message` row carrying `mentions` and routes a turn to each
 *  target — so a mention send must NOT also post the message. */
function fanoutMentions(
  me: string,
  chatSessionId: string,
  text: string,
  targets: string[],
): Promise<void> {
  return req<void>("POST", "/api/intents/mentions", {
    chat_session_id: chatSessionId,
    from_user: me,
    targets,
    text,
  });
}

/** Resolved attachment metadata — wire `chat::Attachment`. */
export interface ChatAttachmentMeta {
  id: string;
  filename: string;
  mime: string;
  size: number;
  thumb?: string;
}

/** Upload one attachment into the session's `files/` dir (§4.2.1). */
async function uploadChatAttachment(
  chatSessionId: string,
  file: File,
): Promise<ChatAttachmentMeta> {
  const form = new FormData();
  form.append("file", file);
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(
    `${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(chatSessionId)}/files`,
    { method: "POST", headers: token ? { Authorization: `Bearer ${token}` } : {}, body: form },
  );
  if (!resp.ok) throw new Error(`upload → ${resp.status}`);
  return resp.json();
}

/** Attachment download needs the bearer token, so `<img src>` cannot work
 *  — hand out object URLs the same way `user-chat-api.ts` does. `thumb`
 *  serves the upload-time thumbnail sidecar (a 4K screenshot costs a few
 *  KB instead of the full blob) — 404 when the attachment has no sidecar,
 *  callers fall back to the full download. */
const objectUrlCache = new Map<string, string>();
export async function chatAttachmentObjectUrl(
  chatSessionId: string,
  attachmentId: string,
  opts?: { thumb?: boolean },
): Promise<string> {
  const thumb = opts?.thumb === true;
  const key = `${chatSessionId}/${attachmentId}${thumb ? "/thumb" : ""}`;
  const hit = objectUrlCache.get(key);
  if (hit) return hit;
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(
    `${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(chatSessionId)}/files/${encodeURIComponent(attachmentId)}${thumb ? "?thumb=1" : ""}`,
    { headers: token ? { Authorization: `Bearer ${token}` } : {} },
  );
  if (!resp.ok) throw new Error(`attachment → ${resp.status}`);
  const blob = await resp.blob();
  const url = URL.createObjectURL(blob);
  objectUrlCache.set(key, url);
  return url;
}

// ── Read-time turn projection (design 27 §6.2) ────────────────────────

/** The in-flight turn, projected from the rows — no send-time bookkeeping. */
export interface InFlightTurn {
  /** meta.message_id of the unanswered human `user_message`. */
  triggerId: string;
  /** execution session id from the `turn_started` pointer, if it arrived. */
  execSid: string | null;
}

/**
 * COPIED VERBATIM from the mobile twin — the two apps must project the
 * same state from the same rows (plan 5.5 drift guard). Do not "fix" one
 * side without the other; the tests are duplicated for the same reason.
 *
 * Walk the window from the newest row backwards — a trailing `user_message`
 * (with `turn_started` pointers allowed in between) means the turn has not
 * produced its chat-side terminal row yet, so it is in flight. Any agent-side
 * row (terminal `agent_message`, answered `interaction_request` card, failure
 * receipt) ends the turn. Position-based on purpose: a card answer continues
 * the ORIGINAL turn's terminal row (`in_reply_to` = the first trigger), which
 * an id-matching rule would never connect to the answer message.
 *
 * A trailing `user_message` counts as in flight only once its `turn_started`
 * pointer is in the window: the pointer is written by the Runtime for every
 * turn that actually runs, so a group message that @-nobody never shows a
 * false "in progress" — at the cost of a sub-second quiet gap between send
 * and pointer on 1:1.
 */
export function projectInFlight(rows: ChatMessageRow[]): InFlightTurn | null {
  const findPointer = (trigger: string): ChatMessageRow | undefined => {
    for (let i = rows.length - 1; i >= 0; i--) {
      const p = rows[i]!;
      if (
        p.kind === "system_notice" &&
        p.meta?.notice_type === "turn_started" &&
        p.meta?.in_reply_to === trigger
      )
        return p;
    }
    return undefined;
  };
  for (let i = rows.length - 1; i >= 0; i--) {
    const r = rows[i]!;
    if (r.kind === "system_notice" && r.meta?.notice_type === "turn_started") continue;
    if (r.kind === "user_message") {
      const triggerId = r.meta?.message_id ?? "";
      if (!triggerId) return null;
      let pointer = findPointer(triggerId);
      if (!pointer && r.meta?.in_reply_to) {
        // Card answer: the turn continues under the ORIGINAL trigger, whose
        // pointer is two hops away (answer → card row → its trigger).
        const card = rows.find(
          (x) => x.kind === "interaction_request" && x.meta?.message_id === r.meta?.in_reply_to,
        );
        if (card?.meta?.in_reply_to) pointer = findPointer(card.meta.in_reply_to);
      }
      return pointer ? { triggerId, execSid: pointer.meta?.agent_session_id || null } : null;
    }
    return null;
  }
  return null;
}

/** §5.5.4: a card is unresolved when no later row answers it. */
export function answeredCardIds(rows: ChatMessageRow[]): Set<string> {
  return new Set(rows.map((r) => r.meta?.in_reply_to).filter((s): s is string => !!s));
}

// ── Store ──────────────────────────────────────────────────────────────

interface AgentChatState {
  /** The pair's single Chat session id; null until resolved (or on
   *  failure — the Agent page then falls back to the legacy exec-only
   *  ChatPanel). */
  chatSessionId: string | null;
  /** True once `openAgent` resolved (or failed) — the view's loading gate. */
  resolved: boolean;
  /** Which surface the tab strip shows: 'chat' = this store's surface,
   *  'exec' = the ChatPanel. Pure view state — switching never closes the
   *  other surface's sessions. */
  mode: "chat" | "exec";
}

interface Window {
  /** Older pages, ascending, prepended by `loadOlder`. */
  older: ChatMessageRow[];
  /** Newest window, replaced wholesale on refresh. */
  tail: ChatMessageRow[];
  tailLoaded: number;
  total: number;
  loadingOlder: boolean;
}

interface ChatSessionStore {
  byAgent: Record<string, AgentChatState>;
  windows: Record<string, Window>;
  inFlight: Record<string, InFlightTurn | null>;
  sending: Record<string, boolean>;
  /** Cache of the server-side `session_config` draft, keyed by chat
   *  session id. The session row is the source of truth (design 27 §4.5);
   *  this slot is hydrated by `openAgent` and updated by `saveDraft`'s
   *  response — never an independent frontend state. */
  draft: Record<string, ChatSessionConfig>;

  openAgent(agentId: string): Promise<void>;
  /** Project group chats currently on screen (keyed by session id) —
   *  doorbell `wake()` re-reads these windows too. */
  openGroupIds: Record<string, true>;
  openGroup(chatSessionId: string): Promise<void>;
  /** Send into a group: @-targets fan out through the intent channel
   *  (which writes the row); without targets the row is archive-only.
   *  `inReplyTo` answers a card (routes through the 1:1 channel, which
   *  the backend also accepts for group sessions). */
  sendGroupText(
    chatSessionId: string,
    text: string,
    targets: string[],
    inReplyTo?: string,
  ): Promise<boolean>;
  setMode(agentId: string, mode: "chat" | "exec"): void;
  /** Persist the Chat surface's full seed draft to the session row,
   *  optimistically showing it locally; reverts on a failed write. */
  saveDraft(agentId: string, draft: ChatSessionConfig): Promise<void>;
  refresh(chatSessionId: string): Promise<void>;
  loadOlder(chatSessionId: string): Promise<void>;
  sendText(
    chatSessionId: string,
    text: string,
    inReplyTo?: string,
    sessionConfig?: ChatSessionConfig,
    attachmentIds?: string[],
  ): Promise<boolean>;
  upload(chatSessionId: string, file: File): Promise<ChatAttachmentMeta>;
  /** The doorbell reaction: re-read every session that could be on screen. */
  wake(): Promise<void>;
}

const emptyWindow = (): Window => ({
  older: [],
  tail: [],
  tailLoaded: 0,
  total: 0,
  loadingOlder: false,
});

export const useChatSessionStore = create<ChatSessionStore>((set, get) => ({
  byAgent: {},
  openGroupIds: {},
  windows: {},
  inFlight: {},
  sending: {},
  draft: {},

  openAgent: async (agentId) => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) {
      // Local mode: no account, no human surface. Mark resolved with no
      // chat session — AgentPage renders the old ChatPanel, i.e. pre-Wave-5
      // behaviour, without a feature flag.
      set((s) => ({
        byAgent: { ...s.byAgent, [agentId]: { chatSessionId: null, resolved: true, mode: "exec" } },
      }));
      return;
    }
    const existing = get().byAgent[agentId];
    if (existing?.resolved) return;
    try {
      // One pair, one Chat session (WeChat model): the pair's default
      // session, idempotently created server-side by `ensure_pair`.
      const def = await fetchDefaultChatSession(agentId);
      set((s) => ({
        byAgent: {
          ...s.byAgent,
          [agentId]: { chatSessionId: def.session_id, resolved: true, mode: existing?.mode ?? "chat" },
        },
        // Hydrate the draft cache from the durable session row (§4.5) —
        // absent on the wire means "no draft", shown as the agent's own
        // defaults by the toolbar chips.
        draft: { ...s.draft, [def.session_id]: def.session_config ?? {} },
      }));
      await get().refresh(def.session_id);
    } catch (e) {
      console.warn("[chatSessionStore] openAgent failed:", e);
      set((s) => ({
        byAgent: { ...s.byAgent, [agentId]: { chatSessionId: null, resolved: true, mode: "exec" } },
      }));
    }
  },

  openGroup: async (chatSessionId) => {
    set((s) => ({ openGroupIds: { ...s.openGroupIds, [chatSessionId]: true } }));
    await get().refresh(chatSessionId);
  },

  sendGroupText: async (chatSessionId, text, targets, inReplyTo) => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) return false;
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: true } }));
    let ok = false;
    try {
      if (inReplyTo) {
        // Card answer: the intent channel routes it to the card's agent
        // (the gateway resolves the card row; groups accepted per §5.5.4).
        await sendChatMessage(me, chatSessionId, text, inReplyTo);
      } else if (targets.length > 0) {
        await fanoutMentions(me, chatSessionId, text, targets);
      } else {
        await postGroupMessage(chatSessionId, text);
      }
      ok = true;
    } catch (e) {
      console.warn("[chatSessionStore] group send failed:", e);
    }
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: false } }));
    if (ok) await get().refresh(chatSessionId);
    return ok;
  },

  setMode: (agentId, mode) => {
    let chatSessionId: string | null = null;
    set((s) => {
      const st = s.byAgent[agentId];
      if (!st) return s;
      chatSessionId = st.chatSessionId;
      return { byAgent: { ...s.byAgent, [agentId]: { ...st, mode } } };
    });
    // Coming back to the Chat surface: the doorbell only wakes sessions
    // the store knows about, so re-read once on the flip.
    if (mode === "chat" && chatSessionId) void get().refresh(chatSessionId);
  },

  saveDraft: async (agentId, draft) => {
    const chatSessionId = get().byAgent[agentId]?.chatSessionId;
    if (!chatSessionId) return;
    const prev = get().draft[chatSessionId] ?? {};
    // Whole-replace on the wire: strip the undefined keys so an absent
    // field clears server-side ("no opinion" — the agent's defaults).
    const wire = Object.fromEntries(
      Object.entries(draft).filter(([, v]) => v !== undefined && v !== ""),
    ) as ChatSessionConfig;
    // Optimistic: the chips follow the pick immediately; the PATCH
    // response (server-normalized) is what the cache keeps. On failure
    // the cache reverts — a draft that never landed must not be shown.
    set((s) => ({ draft: { ...s.draft, [chatSessionId]: draft } }));
    try {
      const resp = await patchChatSessionConfig(chatSessionId, wire);
      set((s) => ({ draft: { ...s.draft, [chatSessionId]: resp.session_config ?? {} } }));
    } catch (e) {
      console.warn("[chatSessionStore] saveDraft failed:", e);
      set((s) => ({ draft: { ...s.draft, [chatSessionId]: prev } }));
    }
  },

  refresh: async (chatSessionId) => {
    try {
      const page = await fetchChatMessages(chatSessionId, 0);
      set((s) => ({
        windows: {
          ...s.windows,
          [chatSessionId]: {
            ...emptyWindow(),
            tail: page.messages,
            tailLoaded: page.messages.length,
            total: page.total,
          },
        },
        inFlight: { ...s.inFlight, [chatSessionId]: projectInFlight(page.messages) },
      }));
    } catch (e) {
      console.warn("[chatSessionStore] refresh failed:", e);
    }
  },

  loadOlder: async (chatSessionId) => {
    const w = get().windows[chatSessionId];
    if (!w || w.loadingOlder || w.tailLoaded + w.older.length >= w.total) return;
    set((s) => ({
      windows: { ...s.windows, [chatSessionId]: { ...w, loadingOlder: true } },
    }));
    try {
      const page = await fetchChatMessages(chatSessionId, w.tailLoaded + w.older.length);
      const cur = get().windows[chatSessionId] ?? w;
      set((s) => ({
        windows: {
          ...s.windows,
          [chatSessionId]: {
            ...cur,
            older: [...page.messages, ...cur.older],
            loadingOlder: false,
          },
        },
      }));
    } catch (e) {
      console.warn("[chatSessionStore] loadOlder failed:", e);
      const cur = get().windows[chatSessionId] ?? w;
      set((s) => ({
        windows: { ...s.windows, [chatSessionId]: { ...cur, loadingOlder: false } },
      }));
    }
  },

  sendText: async (chatSessionId, text, inReplyTo, sessionConfig, attachmentIds) => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) return false;
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: true } }));
    let ok = false;
    try {
      if (inReplyTo && attachmentIds?.length) throw new Error("card replies cannot include attachments");
      await sendChatMessage(me, chatSessionId, text, inReplyTo, sessionConfig, attachmentIds);
    } catch (e) {
      console.warn("[chatSessionStore] send failed:", e);
    } finally {
      set((s) => ({ sending: { ...s.sending, [chatSessionId]: false } }));
    }
    // The durable row exists before the delivery returns (§5.1), so one
    // re-read right after the response is enough — the doorbell covers
    // everything the sender's own device did not render.
    if (ok) await get().refresh(chatSessionId);
    return ok;
  },

  upload: (chatSessionId, file) => uploadChatAttachment(chatSessionId, file),

  wake: async () => {
    // The doorbell names nothing (no subscription ACL on the broker). The
    // chat session is pinned — even while the exec surface is shown its
    // in-flight turn drives the chat tab's breathing icon, so refresh
    // regardless of `mode`.
    for (const st of Object.values(get().byAgent)) {
      if (st.chatSessionId) await get().refresh(st.chatSessionId);
    }
    // Open project group chats ride the same doorbell.
    for (const sid of Object.keys(get().openGroupIds)) {
      await get().refresh(sid);
    }
  },
}));

/** Presence fan-out target: the chat/changed doorbell also wakes agent
 *  chats. Called from `chatStore`'s single `user-chat-changed` listener
 *  (next to `useUserChatStore.wake`) — one registration site per webview. */
export function wakeAgentChat(): void {
  void useChatSessionStore.getState().wake();
}
