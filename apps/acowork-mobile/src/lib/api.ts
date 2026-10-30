/**
 * The real Gateway transport. Split from `chatStore` so the store stays a
 * pure state machine (and therefore testable without HTTP).
 *
 * BOUNDARY NOTE (ADR-009 §5): the mobile app NEVER reads the agent's
 * install_path, workspace, or private data off the local filesystem. Every
 * byte of agent-private content arrives through these HTTP calls. That is
 * not a stylistic preference — since ADR-055 `install_path` is node-local,
 * a filesystem shortcut would work on one machine and 5xx the moment the
 * Gateway and Node live on different hosts.
 *
 * AUTH (v1.1 §7): the Gateway is the only server; every call except
 * `/api/status` and `/api/auth/*` carries the Bearer access token. A 401
 * rotates the token pair exactly once (single-flight, so parallel 401s
 * cannot double-rotate and trip the Gateway's reuse detection) and replays
 * the request; a failed rotation reports `sessionExpired` and the app
 * returns to the login screen. Same ladder as Desktop's `authFetch`.
 */

import { newId } from './id'
import type { ChatTransport } from '../stores/chatStore'
import type {
  AccountMe,
  AgentRow,
  ChatMessage,
  ChatMessagesPage,
  ChatSessionInfo,
  ConversationEntry,
  DirectoryEntry,
  GatewayStatus,
  LiveStatus,
  MessagesPage,
  MessagesWindow,
  NodeRow,
  SessionDetail,
  SessionInfo,
  SessionSnapshot,
  SessionRow,
  SessionsPage,
  SessionVisibility,
  TokenPair,
  UserChatMessage,
  UserChatSummary,
} from './types'

/** Thrown for any non-2xx Gateway response; `status` drives the auth ladder. */
export class GatewayError extends Error {
  constructor(
    readonly status: number,
    readonly body: string,
  ) {
    super(`Gateway ${status}: ${body.slice(0, 200)}`)
    this.name = 'GatewayError'
  }
}

/** Everything the transport needs from the auth layer, injected at boot so
 *  this module never imports a store (keeps the dependency one-directional). */
export interface AuthBridge {
  baseUrl(): string
  accessToken(): string | null
  /** Rotate the pair. Resolve with the new access token, or null on failure. */
  refresh(): Promise<string | null>
  /** Rotation failed: the session is gone. */
  sessionExpired(): void
}

let auth: AuthBridge | null = null

export function setAuthBridge(a: AuthBridge): void {
  auth = a
}

/**
 * HTTP request timeouts. A WiFi/network switch makes a fetch to the old
 * Gateway IP hang at the OS TCP-connect layer (~20-30s) before it finally
 * errors, which is far too long to leave the user staring at a stale screen
 * before the reconnect UI appears. The Gateway is a same-LAN self-hosted
 * service, so a healthy request lands in <100ms and these short ceilings
 * never false-trigger:
 *   - PROBE: boot / connect / login / retry path — fail fast so `disconnected`
 *     shows within seconds of a network switch.
 *   - REQUEST: authenticated business calls once `ready` (a slightly longer
 *     leash for real payload work).
 */
const PROBE_TIMEOUT_MS = 5_000
const REQUEST_TIMEOUT_MS = 10_000

/** AbortSignal with a timeout, degrading to no signal when unsupported. */
function timeoutSignal(ms: number): AbortSignal | undefined {
  return typeof AbortSignal !== 'undefined' && 'timeout' in AbortSignal
    ? AbortSignal.timeout(ms)
    : undefined
}

/** Public for the onboarding screens (they probe before any bridge exists). */
export async function probeStatus(baseUrl: string): Promise<GatewayStatus> {
  const res = await fetch(`${trim(baseUrl)}/api/status`, {
    headers: { Accept: 'application/json' },
    signal: timeoutSignal(PROBE_TIMEOUT_MS),
  })
  if (!res.ok) throw new GatewayError(res.status, await res.text().catch(() => ''))
  return (await res.json()) as GatewayStatus
}

export async function loginRequest(baseUrl: string, username: string, password: string): Promise<TokenPair> {
  const res = await fetch(`${trim(baseUrl)}/api/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username, password }),
    signal: timeoutSignal(PROBE_TIMEOUT_MS),
  })
  if (!res.ok) throw new GatewayError(res.status, await res.text().catch(() => ''))
  return (await res.json()) as TokenPair
}

export async function refreshRequest(baseUrl: string, refreshToken: string): Promise<TokenPair> {
  const res = await fetch(`${trim(baseUrl)}/api/auth/refresh`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ refresh_token: refreshToken }),
    signal: timeoutSignal(REQUEST_TIMEOUT_MS),
  })
  if (!res.ok) throw new GatewayError(res.status, await res.text().catch(() => ''))
  return (await res.json()) as TokenPair
}

export async function logoutRequest(baseUrl: string, refreshToken: string): Promise<void> {
  await fetch(`${trim(baseUrl)}/api/auth/logout`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ refresh_token: refreshToken }),
    signal: timeoutSignal(REQUEST_TIMEOUT_MS),
  }).catch(() => undefined) // best-effort: a dead token is not an error here
}

/**
 * 方案③：凭桌面二维码里的一次性配对票据换取一套新 token。公开端点（调用方
 * 尚无 token pair），仿 loginRequest：直接 POST、不走 401 旋转阶梯。票据过期/
 * 已兑换会得到 401 GatewayError，由调用方回退到登录/重新扫码。
 */
export async function redeemPairing(baseUrl: string, pairingToken: string): Promise<TokenPair> {
  const res = await fetch(`${trim(baseUrl)}/api/auth/pair/redeem`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ pairing_token: pairingToken }),
    signal: timeoutSignal(PROBE_TIMEOUT_MS),
  })
  if (!res.ok) throw new GatewayError(res.status, await res.text().catch(() => ''))
  return (await res.json()) as TokenPair
}

function trim(u: string): string {
  return u.replace(/\/+$/, '')
}

/**
 * One authenticated request with the 401→refresh→replay ladder, returning the
 * raw `Response`.
 *
 * Split from `req` because not every authenticated GET is JSON: attachment
 * bytes are bearer-guarded too (a bare `<img src>` or `<a href>` cannot send
 * the header), so the bytes path needs the same rotation ladder without the
 * JSON envelope handling. One ladder, two readers.
 */
async function authedFetch(
  path: string,
  init: { method?: string; headers?: Record<string, string>; body?: BodyInit } = {},
): Promise<Response> {
  const a = auth
  if (!a) throw new Error('auth bridge not installed')
  const send = async (token: string | null): Promise<Response> => {
    const headers: Record<string, string> = { ...init.headers }
    if (token) headers.Authorization = `Bearer ${token}`
    return fetch(`${trim(a.baseUrl())}${path}`, {
      method: init.method ?? 'GET',
      headers,
      body: init.body,
      signal: timeoutSignal(REQUEST_TIMEOUT_MS),
    })
  }

  let res = await send(a.accessToken())
  if (res.status === 401) {
    // Single-flight lives in the bridge (authStore); awaiting it here also
    // dedupes PARALLEL 401s into one rotation. Re-send through the bridge
    // rather than a captured token: the rotation that answered our 401 may
    // itself have been triggered by an earlier request.
    const rotated = await a.refresh()
    if (!rotated) {
      a.sessionExpired()
      throw new GatewayError(401, 'session expired')
    }
    res = await send(a.accessToken())
  }
  return res
}

/**
 * One authenticated request with the 401→refresh→replay ladder.
 * `path` must start with `/api`.
 */
async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await authedFetch(path, {
    method,
    headers: {
      Accept: 'application/json',
      ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (!res.ok) throw new GatewayError(res.status, await res.text().catch(() => ''))
  if (res.status === 204) return undefined as T
  // Some accepted responses (e.g. 202 on send) carry no body.
  const text = await res.text()
  if (!text) return undefined as T
  return JSON.parse(text) as T
}

/**
 * `GET /api/chat-sessions/{id}/files/{attachment_id}` — one chat attachment's
 * bytes, or its thumbnail sidecar with `thumb`.
 *
 * Session-routed, not account-routed: a group chat has no single `userId` to
 * scope the account route by (the same reason Desktop's `downloadChatAttachment`
 * gained a session twin). `GatewayError` on any non-2xx so the caller can tell
 * "the blob is gone" (404, a migrated row) from "network down".
 */
export async function fetchChatAttachmentBlob(
  chatSessionId: string,
  attachmentId: string,
  opts?: { thumb?: boolean },
): Promise<Blob> {
  const q = opts?.thumb ? '?thumb=1' : ''
  const res = await authedFetch(
    `/api/chat-sessions/${encodeURIComponent(chatSessionId)}/files/${encodeURIComponent(attachmentId)}${q}`,
  )
  if (!res.ok) throw new GatewayError(res.status, await res.text().catch(() => ''))
  return res.blob()
}

/* ------------------------------------------------------------------ */
/* Directory                                                          */
/* ------------------------------------------------------------------ */

export async function fetchAgents(): Promise<AgentRow[]> {
  return req<AgentRow[]>('GET', '/api/agents')
}

/**
 * `GET /api/nodes` — the node topology for the inbox's per-node grouping.
 * The server already applies the ADR-087 `can_view` filter and prunes machine
 * metadata for non-managers, so the client renders this verbatim and never
 * derives visibility itself (ADR-087 D8 single source of truth).
 */
export async function fetchNodes(): Promise<NodeRow[]> {
  return req<NodeRow[]>('GET', '/api/nodes')
}

/**
 * `POST /api/agents/{id}/start` — wake a stopped agent (ADR-087: `start` is
 * `Op::Use`, so even a guest may wake an agent it can already see). Idempotent
 * server-side (fast 200 when already online). `agentId` is the INSTANCE id —
 * same key the directory row uses. Resolves when the command is accepted; the
 * Runtime's `alive` verdict arrives a moment later over MQTT, so callers that
 * must confirm startup re-read the directory.
 */
export async function startAgent(agentId: string): Promise<void> {
  // The handler extracts `Json<StartAgentRequest>` — a bodyless POST is a 415
  // (Unsupported Media Type). `StartAgentRequest` is `#[serde(default)]`, so
  // an explicit `{ dev_mode: false }` both sets Content-Type and parses.
  await req<void>('POST', `/api/agents/${agentId}/start`, { dev_mode: false })
}

export async function fetchDirectory(): Promise<DirectoryEntry[]> {
  const r = await req<{ users: DirectoryEntry[] }>('GET', '/api/users/directory')
  return r.users ?? []
}

export async function fetchMe(): Promise<AccountMe> {
  return req<AccountMe>('GET', '/api/auth/me')
}

/* ------------------------------------------------------------------ */
/* Sessions & messages                                                */
/* ------------------------------------------------------------------ */

/** ISO timestamps → epoch ms, 0 when unparseable (never NaN into a store). */
function isoToMs(ts: string | null | undefined): number {
  if (!ts) return 0
  const n = Date.parse(ts)
  return Number.isFinite(n) ? n : 0
}

export function mapSessionRow(r: SessionRow): SessionInfo {
  return {
    session_id: r.session_id,
    title: r.title ?? '新会话',
    can_write: r.can_write,
    visibility: r.visibility ?? 'public',
    updated_at: isoToMs(r.last_active_at),
    message_count: r.message_count,
  }
}

export async function fetchSessions(agentId: string, page: number): Promise<{ items: SessionInfo[]; hasMore: boolean }> {
  const r = await req<SessionsPage>('GET', `/api/agents/${agentId}/sessions?page=${page}&size=20`)
  return { items: (r.sessions ?? []).map(mapSessionRow), hasMore: (r.page ?? page) < (r.total_pages ?? page) }
}

/**
 * Conversation JSONL → chat bubbles. user/assistant render as text bubbles;
 * `tool_call` / `tool_result` / `thought` are emitted with a payload type tag
 * so `foldToolCalls` can group them into ExploreBlock. `system` / `compaction`
 * / `metadata.internal` entries are skipped.
 */
export function mapConversationEntries(entries: ConversationEntry[]): ChatMessage[] {
  const out: ChatMessage[] = []
  for (const e of entries) {
    if (e.kind === 'compaction') continue
    const internal = (e.metadata as { internal?: boolean } | undefined)?.internal === true
    if (internal) continue
    switch (e.role) {
      case 'user':
      case 'assistant':
        out.push({
          id: e.id,
          role: e.role,
          content: e.content,
          created_at: isoToMs(e.ts),
          kind: 'text',
        })
        break
      case 'tool_call': {
        const md = e.metadata as { tool_name?: string; tool_call_id?: string } | undefined
        out.push({
          id: e.id,
          role: 'assistant',
          content: e.content,
          created_at: isoToMs(e.ts),
          kind: 'text',
          payload: { type: 'tool_call', name: md?.tool_name ?? 'tool', toolCallId: md?.tool_call_id ?? '' },
        })
        break
      }
      case 'tool_result': {
        const md = e.metadata as { tool_name?: string; tool_call_id?: string } | undefined
        out.push({
          id: e.id,
          role: 'assistant',
          content: e.content,
          created_at: isoToMs(e.ts),
          kind: 'text',
          payload: { type: 'tool_result', name: md?.tool_name ?? 'tool', toolCallId: md?.tool_call_id ?? '' },
        })
        break
      }
      case 'thought':
        out.push({
          id: e.id,
          role: 'assistant',
          content: e.content,
          created_at: isoToMs(e.ts),
          kind: 'text',
          payload: { type: 'thought' },
        })
        break
      default:
        // system: not rendered
        break
    }
  }
  return out
}

/**
 * One page of conversation history, in the ADR-050 window model (same
 * contract the Desktop chat store runs on). See `MessagesWindow` in
 * `lib/types.ts` — offset/limit/total are raw-entry counts.
 */
export async function fetchMessages(
  agentId: string,
  sessionId: string,
  opts?: { offset?: number; limit?: number },
): Promise<MessagesWindow> {
  // ADR-050: `tail=true` anchors the window to the LAST `limit` raw entries
  // (initial load — the caller does not know `total` yet). A page fetch
  // passes an explicit forward offset instead: entries [offset, offset+limit).
  const params = new URLSearchParams()
  params.set('limit', String(opts?.limit ?? 100))
  if (opts?.offset === undefined) params.set('tail', 'true')
  else params.set('offset', String(opts.offset))
  const r = await req<MessagesPage>('GET', `/api/agents/${agentId}/sessions/${sessionId}/messages?${params}`)
  return {
    messages: mapConversationEntries(r.messages ?? []),
    offset: r.offset ?? 0,
    limit: r.limit ?? 0,
    total: r.total ?? 0,
  }
}

export async function fetchSessionDetail(agentId: string, sessionId: string): Promise<SessionDetail> {
  return req<SessionDetail>('GET', `/api/agents/${agentId}/sessions/${sessionId}`)
}

/** `live_state.status` — the only field the poll loop consumes (§8). */
export function extractStatus(d: SessionDetail): LiveStatus | null {
  const raw = d.live_state?.status
  if (!raw || typeof raw !== 'object' || typeof (raw as LiveStatus).status !== 'string') return null
  return raw as LiveStatus
}

/**
 * The poll primitive: status + the authoritative `meta.message_count`.
 * The count makes freshness detection stateless — a turn that starts and
 * finishes between two ticks still changes the count, so no transition can
 * be missed (v1.1 §8; fixes the fast-reply reload race).
 */
export function extractSnapshot(d: SessionDetail): SessionSnapshot {
  return { status: extractStatus(d), messageCount: d.meta?.message_count ?? null }
}

/**
 * Unverified JWT payload claims — CLIENT-SIDE USE ONLY (MQTT CONNECT
 * username + expiry check). The broker verifies the token itself; nothing
 * security-relevant is decided from these claims.
 */
export function tokenClaims(jwt: string): { sub?: string; exp?: number } {
  try {
    const part = jwt.split('.')[1]
    if (!part) return {}
    const json = JSON.parse(atob(part.replace(/-/g, '+').replace(/_/g, '/'))) as Record<string, unknown>
    return {
      sub: typeof json.sub === 'string' ? json.sub : undefined,
      exp: typeof json.exp === 'number' ? json.exp : undefined,
    }
  } catch {
    return {}
  }
}

/** Sends a user message; resolves with the client-generated message id so
 *  the store can key the optimistic bubble and its retry.
 *
 *  `command` is the same wire field the desktop chat panel uses for the
 *  active skill name (SendMessageBody.command is documented as "slash-
 *  command name only; the Runtime resolves it against the agent's
 *  SkillRegistry, client-supplied instructions are ignored"). When the
 *  composer skill picker selects one, mobile passes the skill name here so
 *  the next user message arrives with `command: "weekly-report"` and the
 *  runtime injects that skill's instructions. Absent = no skill.
 *
 *  `pinnedChat` (ADR-100 D12) switches the ENTRY POINT, not the destination:
 *  the turn is posted to the Chat session this execution session came from,
 *  pinned back to this session, so the human's message lands as a durable
 *  `user_message` row under the human's own identity (the session-scoped write
 *  records it in the execution transcript only) and the Agent skips its Chat
 *  Router. The id minted here is the one row both surfaces reconcile on.
 *  `undefined` = the session has no Chat binding, so the write below is its
 *  only path. */
export async function sendMessage(
  agentId: string,
  sessionId: string,
  content: string,
  command?: string,
  pinnedChat?: { me: string; chatSessionId: string },
): Promise<string> {
  const messageId = newId()
  if (pinnedChat) {
    await req<void>('POST', '/api/intents/chat-message', {
      chat_session_id: pinnedChat.chatSessionId,
      from_user: pinnedChat.me,
      text: content,
      in_reply_to: '',
      message_id: messageId,
      for_execution_session: sessionId,
      // ADR-100 D12: the Agent that owns this execution session. Only consulted
      // when the bound chat is a group (a project group has several agents, so
      // the Gateway cannot pick a target on its own); the Gateway verifies the
      // claim against that chat's participants.
      pinned_agent: agentId,
      ...(command ? { command } : {}),
    })
    return messageId
  }
  await req('POST', `/api/agents/${agentId}/sessions/${sessionId}/messages`, {
    content,
    message_id: messageId,
    command: command ?? '',
  })
  return messageId
}

export async function sendApproval(agentId: string, sessionId: string, requestId: string, approved: boolean): Promise<void> {
  await req('POST', `/api/agents/${agentId}/sessions/${sessionId}/approval`, {
    request_id: requestId,
    approved,
  })
}

/** Answer an `ask_user_question` prompt (Desktop parity: lib/session-control). */
export async function sendAnswer(agentId: string, sessionId: string, requestId: string, answer: string): Promise<void> {
  await req('POST', `/api/agents/${agentId}/sessions/${sessionId}/answer`, {
    request_id: requestId,
    answer,
  })
}

/* ------------------------------------------------------------------ */
/* User-to-user chat (ADR-076 §决策 8)                                 */
/* ------------------------------------------------------------------ */

/**
 * Canonical 1:1 chat id: the two user ids sorted and joined by `__`.
 * Mirrors `acowork-user::chat::chat_id` — the sort is a bytewise compare on
 * the server, and ids are ASCII UUIDs, so JS's default sort agrees. A
 * non-canonical id is a 404 on the server, never a different conversation.
 */
export function userChatId(a: string, b: string): string {
  return [a, b].sort()[0] === a ? `${a}__${b}` : `${b}__${a}`
}

/**
 * The per-account wake-up topic — the client-side twin of
 * `acowork_core::mqtt_proto::user_chat_changed_topic`, keyed by account id only
 * (never the thread, so a subscription cannot name who I talk to).
 *
 * It carries an EMPTY payload: the embedded broker has no subscription-level
 * ACL, so anything naming the peer or the `chat_id` would be readable by every
 * other account — `chat_id` is literally the two account ids joined, i.e. the
 * social graph. The topic is a doorbell; the HTTP read below is the truth.
 */
export function userChatChangedTopic(me: string): string {
  return `acowork/users/${me}/chat/changed`
}

export async function fetchUserChats(me: string): Promise<UserChatSummary[]> {
  const r = await req<{ chats: UserChatSummary[] }>('GET', `/api/users/${me}/chats`)
  return r.chats ?? []
}

/**
 * One page of a 1:1 chat; `offset` counts BACK from the newest message, so
 * the page is always the newest `limit` items in chronological order.
 *
 * `total` is the thread's full message count and is the reason this returns
 * the envelope instead of the array: the wake-up signal is empty by design
 * (no `chat_id`, no body — the embedded broker has no subscription ACL), so
 * the difference between the server's count and what we hold is the ONLY way
 * a client learns that something arrived and how much of it. That is what
 * lets a refresh append a tail instead of replacing the window the user is
 * reading.
 *
 * `null` means the server gave no count (older build): the caller must fall
 * back to replacing the whole window rather than guessing a delta.
 */
export async function fetchUserMessages(
  me: string,
  chatId: string,
  limit = 50,
): Promise<{ messages: UserChatMessage[]; total: number | null }> {
  const r = await req<{ messages: UserChatMessage[] | null; total?: number }>(
    'GET',
    `/api/users/${me}/chats/${chatId}/messages?offset=0&limit=${limit}`,
  )
  const messages = r.messages ?? []
  return { messages, total: typeof r.total === 'number' ? r.total : null }
}

/** Returns the stored message so the caller can append it without a refetch. */
export async function sendUserMessage(me: string, chatId: string, body: string): Promise<UserChatMessage> {
  return req<UserChatMessage>('POST', `/api/users/${me}/chats/${chatId}/messages`, { body })
}

export async function markUserChatRead(me: string, chatId: string): Promise<void> {
  await req<void>('POST', `/api/users/${me}/chats/${chatId}/read`)
}

/* ------------------------------------------------------------------ */
/* Chat sessions — the unified surface (design 27 §6.5, plan Wave 5.5)  */
/* ------------------------------------------------------------------ */

/**
 * The caller's default 1:1 Chat session with `agentId`, created on first
 * call (idempotent server-side: `GET /api/chat-sessions/default?pair=`).
 * The agent is addressed by instance id — the same value the inbox lists.
 */
export async function fetchDefaultChatSession(agentId: string): Promise<ChatSessionInfo> {
  return req<ChatSessionInfo>(
    'GET',
    `/api/chat-sessions/default?pair=${encodeURIComponent(agentId)}`,
  )
}

/**
 * One page of a Chat session. `offset` counts BACK from the newest row, so
 * `offset=0` is the newest window in ascending order and `offset=loaded`
 * is the next-older page — identical to Desktop's `chatSessionStore` paging.
 */
export async function fetchChatMessages(
  chatSessionId: string,
  offset: number,
  limit = 50,
): Promise<ChatMessagesPage> {
  return req<ChatMessagesPage>(
    'GET',
    `/api/chat-sessions/${encodeURIComponent(chatSessionId)}/messages?offset=${offset}&limit=${limit}`,
  )
}

/**
 * Post one turn into a Chat session. Goes through the intent channel
 * (`POST /api/intents/chat-message`), NOT a session-scoped write: the
 * durable row lands before delivery resolves (design 27 §5.1), so the
 * caller re-reads after the response regardless of the delivery outcome.
 * `inReplyTo` is a card's `message_id` when answering a decision card.
 */
export async function sendChatMessage(
  me: string,
  chatSessionId: string,
  text: string,
  inReplyTo?: string,
): Promise<void> {
  await req<void>('POST', '/api/intents/chat-message', {
    chat_session_id: chatSessionId,
    from_user: me,
    text,
    in_reply_to: inReplyTo ?? '',
  })
}

/* ------------------------------------------------------------------ */
/* Session list extras (§2.3 preview, §5.4 visibility)                  */
/* ------------------------------------------------------------------ */

/**
 * The IM-style preview: the last message of one session. `tail` is a BOOL on
 * this endpoint (`?tail=1` is a 400 — serde will not parse an int as bool),
 * so the window is `tail=true&limit=1`, the cheapest call available — the
 * inbox fires it
 * only for each agent's most recent session, fire-and-forget, so a slow
 * agent never blocks the list (§2.3).
 */
export async function fetchLastMessage(agentId: string, sessionId: string): Promise<string> {
  const r = await req<MessagesPage>(
    'GET',
    `/api/agents/${agentId}/sessions/${sessionId}/messages?tail=true&limit=1`,
  )
  const entries = r.messages ?? []
  const last = entries[entries.length - 1]
  return last?.content ?? ''
}

/** `PUT /sessions/{id}/visibility` — one-tap 🌐/🔒 (§5.4). */
export async function setSessionVisibility(agentId: string, sessionId: string, visibility: SessionVisibility): Promise<void> {
  await req<void>('PUT', `/api/agents/${agentId}/sessions/${sessionId}/visibility`, { visibility })
}

/* ── Composer settings — model / reasoning / skill (mobile parity) ──────── */

/** One row of `GET /api/providers`. Provider + accounts + configured model IDs.
 *  Same shape as Desktop's `VaultKeyEntry` (only the fields mobile needs). */
export interface VaultKeyEntry {
  provider: string
  /** Optional human label (e.g. "OpenAI · work"); absent when not set. */
  alias?: string | null
  /** Multi-account support — the same provider may have several API keys. */
  account_id?: string | null
  /** Provider-level model allow-list (overrides `default_model`). */
  models?: string[] | null
  /** Fallback single model when no `models` list is configured. */
  default_model?: string | null
  /** Per-model capability overrides; `default_reasoning_effort` drives the
   *  effort sub-picker's first selection. */
  model_capabilities?: Record<string, { default_reasoning_effort?: string | null }> | null
  /** Last 4 chars of the key — shown in the picker so the user can pick
   *  the right account by preview without exposing the full secret. */
  key_preview?: string | null
}

/** One row from `GET /api/models/{provider}`. The model picker renders
 *  the `id` and uses `reasoning` to decide whether the effort sub-picker
 *  should appear. */
export interface ProviderModel {
  id: string
  tool_call?: boolean | null
  reasoning?: boolean | null
  input_modalities?: string[] | null
  default_reasoning_effort?: string | null
}

/** `GET /api/providers` — providers + accounts + configured model IDs. The
 *  model picker iterates the result, calls `fetchProviderModels` for
 *  each provider, and intersects with `entry.models` to get the actual
 *  selectable list. */
export async function listKeys(): Promise<VaultKeyEntry[]> {
  const r = await req<{ keys?: VaultKeyEntry[] } | VaultKeyEntry[]>('GET', '/api/providers')
  return Array.isArray(r) ? r : (r.keys ?? [])
}

/** `GET /api/models/{provider}` — capabilities for one provider.
 *  Resolves with `{ models: [] }` on failure so a missing catalog row
 *  doesn't kill the whole picker (the row simply has no capabilities). */
export async function fetchProviderModels(provider: string): Promise<{ models: ProviderModel[] }> {
  try {
    return await req<{ models: ProviderModel[] }>('GET', `/api/models/${encodeURIComponent(provider)}`)
  } catch {
    return { models: [] }
  }
}

/** One row from `GET /api/agents/{id}/skills`. The skill picker renders
 *  the name; description is shown in the row's secondary line. */
export interface SkillEntry {
  name: string
  description?: string | null
  /** Whether the skill is currently enabled on the agent. */
  enabled?: boolean | null
  /** Optional metadata the runtime exposes (e.g. version). */
  version?: string | null
}

/** `GET /api/agents/{id}/skills?page=1&size=100` — list skills. Mobile
 *  only needs the name and enabled flag to power the picker; we still
 *  surface description in the row for context. */
export async function fetchSkills(agentId: string): Promise<{ skills: SkillEntry[]; total: number }> {
  return req<{ skills: SkillEntry[]; total: number }>(
    'GET',
    `/api/agents/${encodeURIComponent(agentId)}/skills?page=1&size=100`,
  )
}

/** `PUT /api/agents/{id}/sessions/{sid}/config` — partial config update.
 *  Mobile uses this for the model picker (model/provider/account_id) and
 *  the reasoning-effort picker (reasoning_effort). Absent fields are
 *  untouched, so a one-field PATCH is a single round-trip. */
export async function patchSessionConfig(
  agentId: string,
  sessionId: string,
  patch: {
    model?: string
    provider?: string
    account_id?: string
    reasoning_effort?: string
  },
): Promise<void> {
  await req<void>('PUT', `/api/agents/${agentId}/sessions/${sessionId}/config`, patch)
}

/** `GET /api/agents/{id}/sessions/{sid}/config` — full snapshot of the
 *  session's effective config (backend resolves the persisted → provider
 *  default → null chain, so the value here is what the runtime will use
 *  for the next message). Mobile uses this to badge the "current"
 *  selection in the model / effort pickers so the user sees what they're
 *  switching from before they tap a row. */
export interface SessionConfigSnapshot {
  model?: string | null
  provider?: string | null
  account_id?: string | null
  reasoning_effort?: string | null
  temperature?: number | null
  context_window?: number | null
  title?: string | null
  workspace_id?: string | null
}

export async function fetchSessionConfig(agentId: string, sessionId: string): Promise<SessionConfigSnapshot> {
  return req<SessionConfigSnapshot>('GET', `/api/agents/${agentId}/sessions/${sessionId}/config`)
}

export const httpChatTransport: ChatTransport = {
  async openSession(agentId, sessionId) {
    await req('POST', `/api/agents/${agentId}/sessions/${sessionId}/open`)
  },

  async fetchMessages(agentId, sessionId, opts) {
    return fetchMessages(agentId, sessionId, opts)
  },

  async fetchSessions(agentId, page) {
    return fetchSessions(agentId, page)
  },

  async fetchSnapshot(agentId, sessionId) {
    try {
      return extractSnapshot(await fetchSessionDetail(agentId, sessionId))
    } catch (e) {
      // 404 = the session is unknown to this runtime (never opened, or the
      // runtime restarted without it). That is idle, not an outage — the
      // poll loop must not raise the offline banner on it.
      if (e instanceof GatewayError && e.status === 404) return { status: null, messageCount: null }
      throw e
    }
  },

  async send(agentId, sessionId, content, command, pinnedChat) {
    return sendMessage(agentId, sessionId, content, command, pinnedChat)
  },

  async approval(agentId, sessionId, requestId, approved) {
    return sendApproval(agentId, sessionId, requestId, approved)
  },

  async answer(agentId, sessionId, requestId, answer) {
    return sendAnswer(agentId, sessionId, requestId, answer)
  },

  async createSession(agentId, title) {
    // visibility: 'private' is the on-disk default per ADR-076
    // create_frontend_session — a new session is nobody's until shared.
    // The create response only carries {session_id, created_at, owner,
    // visibility}; `can_write` is NOT in it, so the store re-reads the
    // authoritative row from page 1 of the list instead of trusting a
    // client-side guess (invariant: backend `can_write` is the only gate).
    const created = await req<{ session_id: string }>('POST', `/api/agents/${agentId}/sessions`, {
      visibility: 'private',
    })
    const { items } = await fetchSessions(agentId, 1)
    const row = items.find((x) => x.session_id === created.session_id)
    if (!row) throw new GatewayError(500, 'created session missing from list')
    // The title is set after creation via the config plane; v1 keeps the
    // runtime default and lets the first user message rename it (Desktop
    // parity), so `title` is only used for the optimistic label.
    void title
    return row
  },

  async deleteSession(agentId, sessionId) {
    await req('DELETE', `/api/agents/${agentId}/sessions/${sessionId}`)
  },
}

/* ------------------------------------------------------------------ */
/* Agent settings drawer (§4.2) — all read-through reverse proxies      */
/* ------------------------------------------------------------------ */

/** `GET /api/agents/{id}/status` — the Runtime's own view of itself. */
export interface AgentStatus {
  agent_id: string
  matches: boolean
  work_dir: string
  pid: number
  latest_session: string | null
  embed_dim: number | null
}

export async function fetchAgentStatus(agentId: string): Promise<AgentStatus> {
  return req<AgentStatus>('GET', `/api/agents/${agentId}/status`)
}

/** `GET .../memory/stats` — counts only; full browsing is Desktop-only (§4.2). */
export interface MemoryStats {
  total_nodes: number
  by_type: Record<string, number>
  by_status: Record<string, number>
  index_health: string
  stored_dim: number
}

export async function fetchMemoryStats(agentId: string): Promise<MemoryStats> {
  return req<MemoryStats>('GET', `/api/agents/${agentId}/memory/stats`)
}

/** `GET .../builtin-tools` — `{agent_id, tools:[{name, enabled}]}`. */
export interface BuiltinTool {
  name: string
  enabled?: boolean
}

export async function fetchBuiltinTools(agentId: string): Promise<BuiltinTool[]> {
  const r = await req<{ tools: BuiltinTool[] }>('GET', `/api/agents/${agentId}/builtin-tools`)
  return r.tools ?? []
}

/**
 * `GET .../workspaces` — the agent's attached directories. Entries are
 * `serde_json::Value` on the server (verbatim passthrough), so every field
 * is optional here and the UI must render what exists, not a fixed schema.
 */
export interface WorkspaceEntry {
  id?: string
  /** The user's own label for this directory; absent until they set one. */
  alias?: string | null
  path?: string
  /** Not `read_only: bool` — the server serialises the mode as a string. */
  access?: 'read-only' | 'read-write'
  added_at?: string
  last_active?: string | null
  select_count?: number
}

export async function fetchWorkspaces(agentId: string): Promise<WorkspaceEntry[]> {
  const r = await req<{ workspaces: WorkspaceEntry[] }>('GET', `/api/agents/${agentId}/workspaces`)
  return r.workspaces ?? []
}

/** `PUT .../sessions/{sid}/workspace` — "Add to Chat" (§4.2). */
export async function attachWorkspace(agentId: string, sessionId: string, workspaceId: string): Promise<void> {
  await req<void>('PUT', `/api/agents/${agentId}/sessions/${sessionId}/workspace`, {
    workspace_id: workspaceId,
  })
}

/** `GET .../config` — the manifest/config view; keys are not a mobile contract. */
export async function fetchAgentConfig(agentId: string): Promise<Record<string, unknown>> {
  return req<Record<string, unknown>>('GET', `/api/agents/${agentId}/config`)
}

/* ------------------------------------------------------------------ */
/* PM: projects & tasks (§9) — proxied to acowork-pm under /api/pm     */
/* ------------------------------------------------------------------ */

/**
 * `X-Actor` is injected by the Gateway from the bearer token
 * (`pm_proxy.rs`), and a client-supplied one is DROPPED. So the mobile app
 * sends nothing but the token and cannot claim to be another principal.
 */

export type PmTaskStatus = 'pending' | 'in_progress' | 'submitted' | 'done' | 'rejected' | 'cancelled'

/** `GET /api/pm/projects` row. */
export interface PmProject {
  id: string
  title: string
  description?: string
  status: 'active' | 'archived' | 'completed'
  created_at: string
  updated_at: string
  members?: { instance_id: string }[]
}

/** `TaskResponse`: the flattened `Task` plus the derived board fields. */
export interface PmTask {
  id: string
  project_id: string
  title: string
  description?: string
  type: string
  status: PmTaskStatus
  review_status: 'not_required' | 'pending' | 'approved' | 'rejected'
  priority: 'low' | 'normal' | 'high' | 'urgent'
  assignee?: string | null
  /** RFC3339 wake-up time; the PM dispatch scanner pushes the task when due. */
  start_at?: string | null
  due_at?: string | null
  created_by: string
  created_at: string
  updated_at: string
  claimed_at?: string | null
  submitted_at?: string | null
  /** Design 27 §5.4.2: set once the scanner pushed the wake-up intent. */
  dispatched_at?: string | null
  /** Consecutive failed dispatch attempts (bounded backoff, then abandoned). */
  dispatch_attempts?: number
  /** Last dispatch failure reason; shown on the task surface (D3). */
  dispatch_error?: string | null
  result?: { text: string; attachment_ids?: string[] } | null
  is_blocked?: boolean
  blocked_by?: string[]
  depth: number
  parent_id?: string | null
}

export async function fetchProjects(): Promise<PmProject[]> {
  return req<PmProject[]>('GET', '/api/pm/projects')
}

/** Returns a bare array — the PM service does not wrap its list envelopes. */
export async function fetchProjectTasks(projectId: string): Promise<PmTask[]> {
  return req<PmTask[]>('GET', `/api/pm/projects/${projectId}/tasks`)
}

export async function fetchTask(taskId: string): Promise<PmTask> {
  return req<PmTask>('GET', `/api/pm/tasks/${taskId}`)
}

export async function fetchTaskChildren(taskId: string): Promise<PmTask[]> {
  return req<PmTask[]>('GET', `/api/pm/tasks/${taskId}/children`)
}

/** 任务流转 (§2.1: the board is read-only, the transitions are not). */
export const pmClaimTask = (tid: string) => req<PmTask>('POST', `/api/pm/tasks/${tid}/claim`)
export const pmSubmitTask = (tid: string, text: string) =>
  req<PmTask>('POST', `/api/pm/tasks/${tid}/submit`, { text, attachment_ids: [] })
export const pmReviewTask = (tid: string, approved: boolean) =>
  req<PmTask>('POST', `/api/pm/tasks/${tid}/review`, { approved })

/* ------------------------------------------------------------------ */
/* Doc library (acowork-doc, §10)                                      */
/* ------------------------------------------------------------------ */
//
// The Gateway reverse-proxies `/api/doc/*` onto the doc service and STRIPS
// the `/api/doc` prefix, and the service mounts its routes with no `/api`
// base of their own — so the mobile path is `/api/doc/tree`, not
// `/api/doc/api/tree`. (Desktop agrees: `src/lib/doc-api.ts` fetches
// `${gw}/api/doc${path}` with `path` starting at `/docs`.) A doubled `api`
// returns 404 with an empty body, which is easy to mistake for "doc service
// is down".

/** One level of the tree: the service returns children only, never a
 *  recursive tree, so a directory is a screen rather than an expander. */
export interface DocTreeNode {
  dir_id: string
  name: string
  path: string
  files: DocMeta[]
  dirs: DirMeta[]
}

export interface DocMeta {
  doc_id: string
  name: string
  version: number
  created_at: string
  updated_at: string
  deleted: boolean
  import?: { instance_id: string; workspace_path: string } | null
}

export interface DirMeta {
  dir_id: string
  name: string
  updated_at: string
  deleted: boolean
}

export interface DocContent {
  meta: DocMeta
  content: string
  path: string
}

export type DocRequestStatus = 'pending' | 'approved' | 'rejected' | 'expired'

export interface DocUpdateRequest {
  request_id: string
  doc_id: string
  path: string
  base_version: number
  content: string
  submitted_by: string
  status: DocRequestStatus
  created_at: string
  reviewed_at?: string | null
  review_note?: string | null
}

export interface DocSearchHit {
  doc_id: string
  name: string
  path: string
  snippet: string
  score: number
}

const DOC = '/api/doc'

export const fetchDocTree = (dirId?: string): Promise<DocTreeNode> =>
  req<DocTreeNode>('GET', dirId ? `${DOC}/tree?dir_id=${encodeURIComponent(dirId)}` : `${DOC}/tree`)

export const fetchDoc = (docId: string): Promise<DocContent> =>
  req<DocContent>('GET', `${DOC}/docs/${encodeURIComponent(docId)}`)

export const fetchDocRequests = (status?: DocRequestStatus): Promise<DocUpdateRequest[]> =>
  req<DocUpdateRequest[]>('GET', status ? `${DOC}/requests?status=${status}` : `${DOC}/requests`)

export const fetchDocRequest = (id: string): Promise<DocUpdateRequest> =>
  req<DocUpdateRequest>('GET', `${DOC}/requests/${encodeURIComponent(id)}`)

/**
 * Approving merges the submitted content into the document, so the reviewer
 * is part of the body and must be the human's own id. An agent submitting a
 * change cannot be the one accepting it — which is the entire reason this
 * queue exists instead of letting the agent write the doc directly.
 */
/**
 * Approve and reject do NOT share an envelope: approve answers
 * `ApproveDto { request, doc_version }` — the caller needs the version the
 * merge produced — while reject answers the bare request. Unwrapping is done
 * by the store, which only ever wants the request.
 */
export interface DocApproveResult {
  request: DocUpdateRequest
  doc_version: number
}

export const approveDocRequest = (id: string, reviewedBy: string): Promise<DocApproveResult> =>
  req<DocApproveResult>('POST', `${DOC}/requests/${encodeURIComponent(id)}/approve`, {
    reviewed_by: reviewedBy,
  })

export const rejectDocRequest = (id: string, reviewedBy: string): Promise<DocUpdateRequest> =>
  req<DocUpdateRequest>('POST', `${DOC}/requests/${encodeURIComponent(id)}/reject`, {
    reviewed_by: reviewedBy,
  })

export const searchDocs = (keyword: string, limit = 20): Promise<DocSearchHit[]> =>
  req<DocSearchHit[]>('GET', `${DOC}/search?keyword=${encodeURIComponent(keyword)}&limit=${limit}`)
