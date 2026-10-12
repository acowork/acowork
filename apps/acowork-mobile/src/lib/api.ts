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
  ConversationEntry,
  DirectoryEntry,
  GatewayStatus,
  LiveStatus,
  MessagesPage,
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
 * One authenticated request with the 401→refresh→replay ladder.
 * `path` must start with `/api`.
 */
async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const a = auth
  if (!a) throw new Error('auth bridge not installed')
  const send = async (token: string | null): Promise<Response> => {
    const headers: Record<string, string> = { Accept: 'application/json' }
    if (body !== undefined) headers['Content-Type'] = 'application/json'
    if (token) headers.Authorization = `Bearer ${token}`
    return fetch(`${trim(a.baseUrl())}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
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
  if (!res.ok) throw new GatewayError(res.status, await res.text().catch(() => ''))
  if (res.status === 204) return undefined as T
  // Some accepted responses (e.g. 202 on send) carry no body.
  const text = await res.text()
  if (!text) return undefined as T
  return JSON.parse(text) as T
}

/* ------------------------------------------------------------------ */
/* Directory                                                          */
/* ------------------------------------------------------------------ */

export async function fetchAgents(): Promise<AgentRow[]> {
  return req<AgentRow[]>('GET', '/api/agents')
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

export async function fetchMessages(agentId: string, sessionId: string): Promise<ChatMessage[]> {
  // tail=true, limit=100: the initial window is the most recent entries —
  // older pages arrive by explicit "load earlier" (v1.1 §4.3).
  const r = await req<MessagesPage>('GET', `/api/agents/${agentId}/sessions/${sessionId}/messages?tail=true&limit=100`)
  return mapConversationEntries(r.messages ?? [])
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
 *  the store can key the optimistic bubble and its retry. */
export async function sendMessage(agentId: string, sessionId: string, content: string): Promise<string> {
  const messageId = newId()
  await req('POST', `/api/agents/${agentId}/sessions/${sessionId}/messages`, {
    content,
    message_id: messageId,
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

export async function fetchUserChats(me: string): Promise<UserChatSummary[]> {
  const r = await req<{ chats: UserChatSummary[] }>('GET', `/api/users/${me}/chats`)
  return r.chats ?? []
}

/** One page of a 1:1 chat; `offset` counts BACK from the newest message. */
export async function fetchUserMessages(me: string, chatId: string, limit = 50): Promise<UserChatMessage[]> {
  const r = await req<{ messages: UserChatMessage[] }>(
    'GET',
    `/api/users/${me}/chats/${chatId}/messages?offset=0&limit=${limit}`,
  )
  return r.messages ?? []
}

/** Returns the stored message so the caller can append it without a refetch. */
export async function sendUserMessage(me: string, chatId: string, body: string): Promise<UserChatMessage> {
  return req<UserChatMessage>('POST', `/api/users/${me}/chats/${chatId}/messages`, { body })
}

export async function markUserChatRead(me: string, chatId: string): Promise<void> {
  await req<void>('POST', `/api/users/${me}/chats/${chatId}/read`)
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

export const httpChatTransport: ChatTransport = {
  async openSession(agentId, sessionId) {
    await req('POST', `/api/agents/${agentId}/sessions/${sessionId}/open`)
  },

  async fetchMessages(agentId, sessionId) {
    return fetchMessages(agentId, sessionId)
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

  async send(agentId, sessionId, content) {
    return sendMessage(agentId, sessionId, content)
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
  due_at?: string | null
  created_by: string
  created_at: string
  updated_at: string
  claimed_at?: string | null
  submitted_at?: string | null
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
