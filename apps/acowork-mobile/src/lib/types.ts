/**
 * Wire types shared by the stores. Mirrors the fields the Gateway already
 * returns — this is a subset of Desktop's, not a redesign. Anything the
 * mobile app does not render is omitted, and absence of a field is never
 * interpreted as a permission.
 */

/** ADR-076 session visibility. Presentation-only; never a write gate. */
export type SessionVisibility = 'public' | 'private'

/** One row of an agent's session history. */
export interface SessionInfo {
  session_id: string
  title: string
  /** ADR-076 §决策 4: the single authority for write access. */
  can_write?: boolean
  visibility?: SessionVisibility
  owner_id?: string | null
  updated_at?: number
  /** Ids of the last few messages, used to build the IM-style preview. */
  last_message?: string | null
  /**
   * Server-side truth for "has this session moved?". Absent until a list load
   * fills it. No longer drives an unread dot (the list row shows only the
   * green working dot, like the Desktop sidebar), but it stays the change
   * detector for tail reloads — see chatStore's `messagesStale`.
   */
  message_count?: number
}

/** An installed agent, as shown in the IM conversation list. */
export interface AgentSummary {
  id: string
  name: string
  /** Hosting node (ADR-055 §6.5). The inbox groups agents by this; an agent
   *  whose node is not in the visible-online set is hidden (no fallback
   *  bucket — ADR-087 §4 Node->Agent cascade guarantees listed ⇒ viewable). */
  node_id: string
  role?: string | null
  /** Builtin icon ID (e.g. "icon-05"). */
  icon?: string | null
  /** Custom avatar file path (served via /api/agents/{id}/avatar-file?path=). */
  avatar?: string | null
  status?: 'idle' | 'busy' | 'error' | 'offline'
  /** Count of sessions this account has open/created. */
  session_count?: number
  unread?: number
}

/** A human contact. User-to-user chat is Gateway-side 1:1 with no session. */
export interface UserSummary {
  id: string
  display_name: string
  /** Login role (ADR-076 §决策 5). Optional because Gateway versions predating
   * the role-on-directory change will not return it — the IM header falls back
   * to omitting the badge in that case rather than guessing. */
  role?: 'user' | 'admin' | null
  /** Custom avatar file path. */
  avatar?: string | null
  /** Builtin icon ID (e.g. "icon-05"). */
  builtin_avatar?: string | null
  online?: boolean
  last_message?: string | null
  updated_at?: number
  unread?: number
}

/** A single chat bubble. */
export interface ChatMessage {
  id: string
  role: 'user' | 'assistant' | 'system'
  content: string
  created_at: number
  /** Only assistant messages can ask a question or request an approval. */
  kind?: 'text' | 'ask_question' | 'approval'
  payload?: unknown
}

/** The result of `chatStore.openSession` — kept for the screens' error UX. */
export type OpenSessionResult = { ok: true } | { ok: false; error: string }

/* ------------------------------------------------------------------ */
/* Gateway wire shapes (v1.1 §7/§8). Field names match the Rust       */
/* serializers exactly; optional-ness mirrors `skip_serializing_if`.  */
/* ------------------------------------------------------------------ */

/** `GET /api/status` — public, reachable before any login. */
export interface GatewayStatus {
  version: string
  agents_installed: number
  agents_running: number
  uptime_secs: number
  mqtt_port: number
  auth_mode: 'local' | 'multi_user'
  registration_open: boolean
  requires_setup: boolean
}

/** `POST /api/auth/login|refresh` — `TokenPair` (ADR-076 §决策 3). */
export interface TokenPair {
  access_token: string
  refresh_token: string
  token_type: string
  expires_in: number
}

/** `GET /api/auth/me` — the subset of `AccountView` the app renders. */
/**
 * `AccountView` (core/acowork-core/src/account.rs). Everything past `role`
 * is `skip_serializing_if = "Option::is_none"` on the server, so it is
 * optional here — but the live gateway also sends `""` for a profile field
 * the user never filled, which is present-but-meaningless. Read these
 * through `profileField()` rather than testing truthiness of the raw value.
 */
export interface AccountMe {
  user_id: string
  username: string
  display_name: string
  role: 'user' | 'admin'
  language: string
  timezone: string
  city?: string | null
  country?: string | null
  occupation?: string | null
  avatar?: string | null
  builtin_avatar?: string | null
  last_login_at?: string | null
  created_at?: string
}

/** `GET /api/users/directory` row. */
export interface DirectoryEntry {
  user_id: string
  username: string
  display_name: string
  /** Login role (ADR-076 §决策 5). */
  role: 'user' | 'admin'
  avatar?: string | null
  builtin_avatar?: string | null
}

/** `GET /api/agents` row — only the fields the IM list needs. */
export interface AgentRow {
  instance_id: string
  agent_id: string
  node_id: string
  name: string
  display_name?: string | null
  role?: string | null
  avatar?: string | null
  builtin_avatar?: string | null
  alive: boolean
  lifecycle: string
}

/** `GET /api/nodes` row — the node topology. The server already filters by
 *  `can_view` (ADR-087), so whatever arrives is viewable by this caller.
 *  Machine metadata (hostname/os/arch) is pruned for non-managers and is
 *  deliberately NOT modelled here — the inbox only needs identity + liveness. */
export interface NodeRow {
  node_id: string
  online: boolean
  /** Display name (renameable slug, ADR-075). Absent until the node reports
   *  its first info snapshot, or pruned from view — fall back to `node_id`. */
  node_name?: string | null
}

/** `GET .../sessions` row — `SessionSummary`. `can_write` is authoritative. */
export interface SessionRow {
  session_id: string
  title?: string | null
  created_at: string
  last_active_at: string
  message_count: number
  visibility?: SessionVisibility | null
  can_write: boolean
}

/** `GET .../sessions` envelope. */
export interface SessionsPage {
  sessions: SessionRow[]
  total_count: number
  total_pages: number
  page: number
  size: number
}

/** `GET .../messages` envelope — entries are raw `ConversationEntry` JSONL. */
export interface MessagesPage {
  session_id: string
  messages: ConversationEntry[]
  offset: number
  limit: number
  total: number
  count: number
}

/**
 * One mapped page of history in the ADR-050 window model (same contract
 * the Desktop chat store runs on): `offset`/`limit`/`total` are RAW JSONL
 * entry counts (each tool_call / tool_result / thought line included) and
 * must never be derived from the filtered bubble list.
 */
export interface MessagesWindow {
  messages: ChatMessage[]
  offset: number
  limit: number
  total: number
}

/** One conversation JSONL entry. Roles beyond user/assistant exist and are */
/* filtered or folded by `mapConversationEntries`.                          */
export interface ConversationEntry {
  id: string
  ts: string
  role: 'user' | 'assistant' | 'thought' | 'tool_call' | 'tool_result' | 'system'
  content: string
  metadata?: unknown
  kind?: string | null
}

/**
 * `live_state.status` — the serde form of the Runtime's `SessionStatus`
 * (`#[serde(tag = "status", content = "detail", rename_all = "snake_case")]`).
 * v1.1 §8: the foreground poll loop reads ONLY this field.
 */
export type LiveStatus =
  | { status: 'idle' }
  | { status: 'llm_awaiting_first_chunk' }
  | { status: 'thinking' }
  | { status: 'llm_streaming'; detail?: { message_id?: string | null } }
  | { status: 'tool_executing' }
  | { status: 'waiting_approval'; detail: { request_id: string } }
  | { status: 'paused'; detail?: Record<string, unknown> }
  | { status: 'errored'; detail?: { reason?: string; message?: string; recoverable?: boolean } }

/** `GET .../sessions/{sid}` — the panel-4 detail (meta + live state). */
export interface SessionDetail {
  session_id: string
  meta: { session_id: string; created_at: string; last_active_at: string; message_count: number }
  live_state: { status: LiveStatus | null } | null
}

/**
 * What the foreground poll loop reads each tick (v1.1 §8): the live status
 * plus the authoritative `meta.message_count`. The count — not a observed
 * status transition — is the freshness signal, so a turn that starts and
 * finishes between two ticks still triggers a history reload.
 */
export interface SessionSnapshot {
  status: LiveStatus | null
  messageCount: number | null
}

/* ------------------------------------------------------------------ */
/* User-to-user chat (ADR-076 §决策 8)                                 */
/* ------------------------------------------------------------------ */

/**
 * `GET /api/users/{me}/chats` row. `unread_count` is server-side truth, so
 * the contact badge shows a real number — unlike an agent session, where
 * the mobile app has no read cursor and the badge degrades to a dot (§8.5).
 */
export interface UserChatSummary {
  chat_id: string
  peer_user_id: string
  peer_display_name: string
  /** Unix seconds. */
  last_active_at: number
  last_message_preview: string
  unread_count: number
}

/** `ChatMessage` from the user service. `from` is server-forced, never client-supplied. */
export interface UserChatMessage {
  /**
   * Server-assigned, thread-monotonic sequence number (ADR-092 decision 1).
   * `planPage` diffs on it. Absent on a Gateway that predates ADR-092 — the
   * client then falls back to the `total`-delta rule (see `planPage`).
   */
  seq?: number
  /** Unix seconds. */
  ts: number
  from: string
  kind: string
  body: string
}
