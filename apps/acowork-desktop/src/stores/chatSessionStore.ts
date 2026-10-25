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
import { SessionAccessError } from "../lib/session-control";
import { useAuthStore } from "./authStore";

// ── Wire types (mirror `chat_sessions_api.rs` responses) ──────────────

/** ADR-100 D12: one display label for something an execution view's composer
 *  handed to the task alongside a turn. Labels only — no path, no id, nothing
 *  the chat side could try to open, because the attachment bytes stay in the
 *  execution session (design 27 §4.2.1). Mirrors `chat_sessions::ExecAttachmentLabel`. */
export interface ExecAttachmentLabel {
  filename: string;
  /** The ADR-046 item arm (`upload` / `file` / `attached_folder` / `selection`);
   *  absent when the sender did not say which arm it was. */
  kind?: string;
}

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
    /** Design 27 §5.3.1: the Agent instances an @-fan-out woke. Only that one
     *  broadcast `user_message` carries it, and it is what marks the row as an
     *  opened agent turn (see `chatTurnState.opensAgentTurn`). */
    mentions?: string[];
    status?: string;
    repo?: string;
    /** A PM task dispatch (`pm_task` origin): the task id, rendered as a
     *  chip on the group-chat bubble so the human text stays clean (the
     *  claim/submit playbook is model-facing only, never on this row). */
    task_id?: string;
    notice_type?: string;
    /** ADR-099: reserved producer label (`chat_router`). No writer emits it
     *  yet — the router's plain-text answer says "nothing to open" by
     *  carrying an empty `agent_session_id` (design 29 §7.2), which is what
     *  the existing drill-down already keys on. */
    origin?: string;
    /** Routing-resolved execution session that HANDLED a request — set on
     *  the `turn_started` pointer and `interaction_request` card. */
    agent_session_id?: string;
    /** ADR-100: the routing decision stamped on the turn pointer — "new" when
     *  this turn starts a fresh task, "continue" when it reuses an
     *  existing one. Drives the two-state `turn_started` reaction copy. */
    turn_mode?: "new" | "continue";
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
    /** ADR-100 D12: labels of the execution session's own attached items on a
     *  `user_message` sent from an execution view's composer. Present only on
     *  that head row; never resolvable from here (see [`ExecAttachmentLabel`]). */
    exec_attachments?: ExecAttachmentLabel[];
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
  /** Group owner for a `user_group` (ADR-098 governance) — the founder is
   *  the first owner; absent for every other kind. */
  owner?: string;
  /** `participant_id -> unread count` (design 27 §4.6). Only the list
   *  endpoint carries it; reading a session's messages clears mine
   *  server-side. */
  unread?: Record<string, number>;
  /** Sidebar meta line for a `project_group` row — the last message's text.
   *  Derived server-side on the list read (mirrors the one-to-one chats'
   *  `last_message_preview`); absent for other kinds and for an empty group. */
  last_message_preview?: string;
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
    throw new ChatRequestError(resp.status, `${method} ${path} → ${resp.status} ${text.slice(0, 200)}`);
  }
  if (method === "DELETE") return undefined as T;
  return resp.json() as Promise<T>;
}

/** A failed `req` that remembers its status, so a caller can tell a terminal
 *  denial from a transient one without parsing the message text. Every existing
 *  handler catches these as a plain `Error`, so nothing changes for them. */
class ChatRequestError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ChatRequestError";
  }
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

/** One paging page — the newest window, an older/newer step, and a locate
 *  jump all fetch exactly this many rows (ADR-097 locate math assumes it). */
const PAGE = 50;

/** One page of a Chat session. `offset` counts BACK from the newest row,
 *  so `offset=0` is the newest window in ascending order and
 *  `offset=loaded` is the next-older page (same semantics as the runtime's
 *  `read_messages`). */
function fetchChatMessages(id: string, offset: number, limit = PAGE): Promise<ChatMessagesPage> {
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
 *  execution session — at creation, and re-applied (from its next turn) when
 *  a mapped session is reused. `undefined` = use the
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

/** ADR-100 D12: post one turn from an EXECUTION view's composer into the Chat
 *  session that execution session is bound to, pinned straight back to that
 *  same session (`forExecutionSessionId`).
 *
 *  Same channel as `sendChatMessage`, and deliberately not the session-scoped
 *  write: the durable `user_message` row lands under the human's own identity
 *  before delivery resolves (design 27 §5.1), so the Chat shows a real question
 *  rather than a result the Runtime transcribed on the user's behalf. The pin
 *  is what makes it safe to leave the Chat surface — the receiving Agent skips
 *  its Chat Router, so the turn still runs in the session the human is looking
 *  at, and the continuation echo threads under this row (`in_reply_to`), which
 *  is how the two surfaces read as one round.
 *
 *  `messageId` is the id the composer already minted for its optimistic bubble:
 *  the Chat row, the execution session's JSONL row and the bubble then reconcile
 *  on one id instead of duplicating. `command` / `attachedItems` are the ADR-046
 *  turn facts the session-scoped write would have carried; the Gateway honors
 *  them on this pinned path only, so the send loses no capability. Attachment
 *  BYTES stay in the execution session — the Chat row records labels only. */
export async function sendPinnedChatMessage(params: {
  me: string;
  agentId: string;
  chatSessionId: string;
  text: string;
  messageId: string;
  executionSessionId: string;
  command?: string;
  attachedItems?: unknown[];
}): Promise<void> {
  try {
    await req<void>("POST", "/api/intents/chat-message", {
      chat_session_id: params.chatSessionId,
      from_user: params.me,
      text: params.text,
      in_reply_to: "",
      message_id: params.messageId,
      for_execution_session: params.executionSessionId,
      // ADR-100 D12: which Agent owns this execution session. Ignored on a 1:1
      // chat (the Gateway resolves its single agent participant) and required
      // when the bound chat is a group — a PM task's session is bound to the
      // project group it was dispatched from, and a group has several agents, so
      // without it the Gateway has no target and refuses the send. It is a claim
      // the Gateway checks against this chat's participants, not a trust.
      pinned_agent: params.agentId,
      ...(params.command ? { command: params.command } : {}),
      ...(params.attachedItems && params.attachedItems.length > 0
        ? { attached_items: params.attachedItems }
        : {}),
    });
  } catch (e) {
    // A deleted Chat session outlives the execution session it was bound to
    // (design 27 §5.5.5), and that reads as a 404 — terminal, and NOT "the
    // Gateway is down". Surfacing it as the shared access-error type is what
    // lets the execution composer say why, exactly as the session-scoped path
    // does; every other status travels unchanged.
    if (e instanceof ChatRequestError && (e.status === 401 || e.status === 403 || e.status === 404)) {
      throw new SessionAccessError(e.status, params.agentId, e.message);
    }
    throw e;
  }
}

/** Archive-only group message: one `user_message` row, no delivery.
 *  (Agents stay silent unless @-mentioned — design 27 §5.3.) */
function postGroupMessage(
  chatSessionId: string,
  text: string,
  attachmentIds?: string[],
): Promise<void> {
  return req<void>("POST", `/api/chat-sessions/${encodeURIComponent(chatSessionId)}/messages`, {
    body: text,
    to: "",
    attachment_ids: attachmentIds ?? [],
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
  attachmentIds?: string[],
): Promise<void> {
  return req<void>("POST", "/api/intents/mentions", {
    chat_session_id: chatSessionId,
    from_user: me,
    targets,
    text,
    attachment_ids: attachmentIds ?? [],
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

// ── Durable card projections (design 27 §5.5.4) ──────────────────────
// The chat window is a pure event log — there is no read-time turn
// projection any more. "Still running" is expressed by the next round's own
// `turn_started` event row, not a live mirror (that was the removed status
// card, whose two truths could not stay in sync).

/**
 * §5.5.4: the cards that are no longer answerable.
 *
 * A card closes when a later row answers it — a human reply, or the
 * Runtime's `interaction_resolved` notice. It ALSO closes when the turn that
 * produced it has already written its terminal `agent_message`: the run is
 * over, so the request behind the card is dead even if nobody stamped a
 * closure on it. That second rule is not decoration — it is what makes the
 * pre-closure transcripts (and any write-back that died before the notice
 * landed) render correctly, instead of showing a permanent idle status card
 * with a button that can never work.
 *
 * Two facts keep the rule from closing a live card early:
 * - `status: "waiting"` is the outcome the Runtime writes when a turn ends
 *   *because* a card is still open (§5.5.4 pause): it releases the slot and
 *   expects an answer, so such a row does not close anything;
 * - the terminal row must belong to the same execution session as the card
 *   (a group fan-out answers one trigger from several agents — one agent
 *   finishing says nothing about another agent's live request). Missing
 *   session ids fall back to trigger-only matching so older rows still heal.
 */
export function closedCardIds(rows: ChatMessageRow[]): Set<string> {
  const closed = new Set(rows.map((r) => r.meta?.in_reply_to).filter((s): s is string => !!s));
  const terminals = rows
    .map((r, i) => ({ r, i }))
    .filter(
      ({ r }) =>
        r.kind === "agent_message" &&
        !!r.meta?.in_reply_to &&
        (r.meta?.status ?? "completed") !== "waiting",
    );
  rows.forEach((card, i) => {
    if (card.kind !== "interaction_request") return;
    const mid = card.meta?.message_id;
    if (!mid || closed.has(mid)) return;
    const sid = card.meta?.agent_session_id;
    const finished = terminals.some(
      ({ r, i: t }) =>
        // Strictly after the card: a terminal row written *before* it belongs
        // to an earlier round and cannot speak for this request.
        t > i &&
        r.meta?.in_reply_to === card.meta?.in_reply_to &&
        (!sid || !r.meta?.sender_session_id || r.meta.sender_session_id === sid),
    );
    if (finished) closed.add(mid);
  });
  return closed;
}

/** Cards a human actually answered — a `user_message` addressed to the card.
 *  Narrower than [`closedCardIds`]: it exists so a surface can tell "you
 *  answered this" from "this request is just no longer open". */
export function humanAnsweredCardIds(rows: ChatMessageRow[]): Set<string> {
  return new Set(
    rows
      .filter((r) => r.kind === "user_message")
      .map((r) => r.meta?.in_reply_to)
      .filter((s): s is string => !!s),
  );
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
  /** Older pages, ascending, prepended by `loadOlder` / a mid-history jump. */
  older: ChatMessageRow[];
  /** Newest segment of the loaded span; the tail when `atTail`. */
  tail: ChatMessageRow[];
  total: number;
  /** Inclusive physical-line positions the loaded span `[older, tail]`
   *  covers — the row cursor that lets the window sit in the MIDDLE of a
   *  conversation (ADR-097 locate), not just flush against the newest page.
   *  `firstPos` is the oldest loaded row's position, `lastPos` the newest.
   *  An empty window is the degenerate range `firstPos = 0, lastPos = -1`. */
  firstPos: number;
  lastPos: number;
  loadingOlder: boolean;
  loadingNewer: boolean;
  /** `seq` of a row the panels must scroll to + flash (set by
   *  `jumpToMessage`, cleared by the panel once located or after the fade). */
  pendingLocateSeq: number | null;
}

interface ChatSessionStore {
  byAgent: Record<string, AgentChatState>;
  windows: Record<string, Window>;
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
    attachmentIds?: string[],
  ): Promise<boolean>;
  setMode(agentId: string, mode: "chat" | "exec"): void;
  /** Persist the Chat surface's full seed draft to the session row,
   *  optimistically showing it locally; reverts on a failed write. */
  saveDraft(agentId: string, draft: ChatSessionConfig): Promise<void>;
  refresh(chatSessionId: string, force?: boolean): Promise<void>;
  loadOlder(chatSessionId: string): Promise<void>;
  /** Page back toward the newest from a mid-history window (no-op at tail).
   *  Enabled by ADR-097 locate: a jump can land in the middle, and the reader
   *  must be able to walk back down without losing the loaded span above. */
  loadNewer(chatSessionId: string): Promise<void>;
  /** Jump the window to a search hit's physical line so the panels scroll to
   *  and flash it (ADR-097). `linePos` is the hit's `line_pos`, `seq` its row
   *  identity (set as `pendingLocateSeq` for the panel effect to consume). */
  jumpToMessage(chatSessionId: string, linePos: number, seq: number): Promise<void>;
  /** Clear a pending locate once the panel has scrolled to it. */
  clearLocate(chatSessionId: string): void;
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
  total: 0,
  firstPos: 0,
  lastPos: -1,
  loadingOlder: false,
  loadingNewer: false,
  pendingLocateSeq: null,
});

/** Whether the loaded span reaches the newest row. Only an at-tail window may
 *  be reset to the newest page by a background `refresh` — a mid-history window
 *  (after a locate jump) keeps the reader's place. */
const atTail = (w: Window): boolean => w.lastPos >= w.total - 1;

export const useChatSessionStore = create<ChatSessionStore>((set, get) => ({
  byAgent: {},
  openGroupIds: {},
  windows: {},
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
      await get().refresh(def.session_id, true);
    } catch (e) {
      console.warn("[chatSessionStore] openAgent failed:", e);
      set((s) => ({
        byAgent: { ...s.byAgent, [agentId]: { chatSessionId: null, resolved: true, mode: "exec" } },
      }));
    }
  },

  openGroup: async (chatSessionId) => {
    set((s) => ({ openGroupIds: { ...s.openGroupIds, [chatSessionId]: true } }));
    await get().refresh(chatSessionId, true);
  },

  sendGroupText: async (chatSessionId, text, targets, inReplyTo, attachmentIds) => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) return false;
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: true } }));
    let ok = false;
    try {
      if (inReplyTo) {
        // Card answer: the intent channel routes it to the card's agent
        // (the gateway resolves the card row; groups accepted per §5.5.4).
        // Card replies never carry attachments (mirrors the 1:1 channel).
        if (attachmentIds?.length) throw new Error("card replies cannot include attachments");
        await sendChatMessage(me, chatSessionId, text, inReplyTo);
      } else if (targets.length > 0) {
        await fanoutMentions(me, chatSessionId, text, targets, attachmentIds);
      } else {
        await postGroupMessage(chatSessionId, text, attachmentIds);
      }
      ok = true;
    } catch (e) {
      console.warn("[chatSessionStore] group send failed:", e);
    }
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: false } }));
    if (ok) await get().refresh(chatSessionId, true);
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
    if (mode === "chat" && chatSessionId) void get().refresh(chatSessionId, true);
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

  refresh: async (chatSessionId, force = false) => {
    try {
      const page = await fetchChatMessages(chatSessionId, 0);
      const cur = get().windows[chatSessionId];
      // A reader parked mid-history (after a locate jump) must not be yanked
      // back to the newest page by a doorbell wake — only an at-tail window,
      // or an explicit `force` (open / send / tab flip), resets to the tail.
      // We still fold in the new `total` so the "newer" affordance stays true.
      if (cur && !force && !atTail(cur)) {
        set((s) => ({
          windows: { ...s.windows, [chatSessionId]: { ...cur, total: page.total } },
        }));
        return;
      }
      const n = page.messages.length;
      set((s) => ({
        windows: {
          ...s.windows,
          [chatSessionId]: {
            ...emptyWindow(),
            tail: page.messages,
            total: page.total,
            firstPos: page.total - n,
            lastPos: page.total - 1,
          },
        },
      }));
    } catch (e) {
      console.warn("[chatSessionStore] refresh failed:", e);
    }
  },

  loadOlder: async (chatSessionId) => {
    const w = get().windows[chatSessionId];
    // `offset` counts back from the newest; the oldest loaded row sits at
    // `firstPos`, so the next-older page starts `total - firstPos` rows up.
    if (!w || w.loadingOlder || w.firstPos <= 0) return;
    set((s) => ({
      windows: { ...s.windows, [chatSessionId]: { ...w, loadingOlder: true } },
    }));
    try {
      const page = await fetchChatMessages(chatSessionId, w.total - w.firstPos);
      const cur = get().windows[chatSessionId] ?? w;
      set((s) => ({
        windows: {
          ...s.windows,
          [chatSessionId]: {
            ...cur,
            older: [...page.messages, ...cur.older],
            firstPos: cur.firstPos - page.messages.length,
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

  loadNewer: async (chatSessionId) => {
    const w = get().windows[chatSessionId];
    if (!w || w.loadingNewer || atTail(w)) return;
    set((s) => ({
      windows: { ...s.windows, [chatSessionId]: { ...w, loadingNewer: true } },
    }));
    try {
      // Fetch the block just above `lastPos`: it ends at position
      // `min(total-1, lastPos + PAGE)`, so `offset` leaves exactly that many
      // newer rows to append (`take`). `older` is untouched — the span grows
      // downward toward the tail.
      const topPos = Math.min(w.total - 1, w.lastPos + PAGE);
      const take = topPos - w.lastPos;
      const page = await fetchChatMessages(chatSessionId, w.total - 1 - topPos, take);
      const cur = get().windows[chatSessionId] ?? w;
      set((s) => ({
        windows: {
          ...s.windows,
          [chatSessionId]: {
            ...cur,
            tail: [...cur.tail, ...page.messages],
            lastPos: cur.lastPos + page.messages.length,
            loadingNewer: false,
          },
        },
      }));
    } catch (e) {
      console.warn("[chatSessionStore] loadNewer failed:", e);
      const cur = get().windows[chatSessionId] ?? w;
      set((s) => ({
        windows: { ...s.windows, [chatSessionId]: { ...cur, loadingNewer: false } },
      }));
    }
  },

  jumpToMessage: async (chatSessionId, linePos, seq) => {
    let total = get().windows[chatSessionId]?.total ?? 0;
    if (total <= 0) {
      // No window yet (e.g. the dialog opened before the first refresh): pull
      // one page so `total` is authoritative before doing the locate math.
      await get().refresh(chatSessionId, true);
      total = get().windows[chatSessionId]?.total ?? 0;
    }
    if (total <= 0) return;
    // Center the target: aim to land it mid-viewport by pulling `LEAD` rows
    // above it, clamped so the page never runs off the newest end.
    const lead = Math.floor(PAGE / 2);
    let topPos = Math.min(total - 1, linePos + lead);
    try {
      let page = await fetchChatMessages(chatSessionId, total - 1 - topPos);
      // The response is authoritative: a row landing between our count read
      // and the fetch shifts every offset (they count back from the newest
      // row), so re-anchor once instead of describing a different span.
      if (page.total !== total) {
        total = page.total;
        topPos = Math.min(total - 1, linePos + lead);
        page = await fetchChatMessages(chatSessionId, total - 1 - topPos);
      }
      const n = page.messages.length;
      if (n === 0) return;
      const bottom = topPos - n + 1;
      set((s) => ({
        windows: {
          ...s.windows,
          [chatSessionId]: {
            ...emptyWindow(),
            tail: page.messages,
            total,
            firstPos: bottom,
            lastPos: topPos,
            pendingLocateSeq: seq,
          },
        },
      }));
    } catch (e) {
      console.warn("[chatSessionStore] jumpToMessage failed:", e);
    }
  },

  clearLocate: (chatSessionId) => {
    const w = get().windows[chatSessionId];
    if (!w || w.pendingLocateSeq == null) return;
    set((s) => ({
      windows: { ...s.windows, [chatSessionId]: { ...w, pendingLocateSeq: null } },
    }));
  },

  sendText: async (chatSessionId, text, inReplyTo, sessionConfig, attachmentIds) => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) return false;
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: true } }));
    let ok = false;
    try {
      if (inReplyTo && attachmentIds?.length) throw new Error("card replies cannot include attachments");
      await sendChatMessage(me, chatSessionId, text, inReplyTo, sessionConfig, attachmentIds);
      ok = true;
    } catch (e) {
      console.warn("[chatSessionStore] send failed:", e);
    } finally {
      set((s) => ({ sending: { ...s.sending, [chatSessionId]: false } }));
    }
    // The durable row exists before the delivery returns (§5.1), so one
    // re-read right after the response is enough — the doorbell covers
    // everything the sender's own device did not render. `force`: sending
    // re-claims the tail even if the reader had parked mid-history.
    if (ok) await get().refresh(chatSessionId, true);
    return ok;
  },

  upload: (chatSessionId, file) => uploadChatAttachment(chatSessionId, file),

  wake: async () => {
    // The doorbell names nothing (no subscription ACL on the broker). The
    // chat session is pinned — even while the exec surface is shown its
    // in-flight turn drives the chat tab's status, so refresh regardless of
    // `mode`.
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
