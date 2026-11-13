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
  /**
   * Whether the Runtime holds this session in memory right now (ADR-038:
   * present in the in-memory session map; `close_session` removes the key).
   *
   * It is the staleness check the status push cannot do alone: a turn parked on
   * an approval card is a loop blocked mid-iteration, so it is necessarily
   * resident — and a row that says it is NOT, while the inbox still shows a red
   * dot for it, is reporting a retained publish from a Runtime that has since
   * died or closed the session. Absent on an older Runtime, which proves
   * nothing (see `isStaleParkedFact`).
   */
  is_active?: boolean
  /**
   * ADR-100: the Chat session this execution session came from, when it did
   * (design 27 §8.2). History, not a routing flag: ADR-100 Rev. 8 (D14)
   * canceled the chat mirror, so the composer always writes to this session
   * (`POST /sessions/{sid}/messages`) and the chat gains no row. The binding
   * still says which chat the session belongs to. Absent = it came from no
   * chat, which is also what an older Runtime returns.
   */
  chat_session_id?: string | null
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
  /** The human rendering of this turn, when the Runtime has one. The bubble
   *  paints it in place of `content` (see `Bubble`). */
  display?: IntentDisplay
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
  /** ADR-114: the account system is always on; this is its runtime state. */
  account_state: 'uninitialized' | 'needs_setup' | 'ready'
  registration_open: boolean
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
  /** ADR-038 residency — see `SessionInfo.is_active`. Absent on an older Runtime. */
  is_active?: boolean
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

/** The human rendering of one agent-origin intent turn (design 27 §5.4.5).
 *
 *  A turn has two texts: `content` is the machine envelope the model reads
 *  (`[intent:from:action] {params}` + the reply coordinates), and this is the
 *  same turn written for a person. The Runtime owns the values — no client reads
 *  `metadata.intent` itself. Absent on human turns and on rows written before the
 *  projection existed, where `content` is what a person reads. */
export interface IntentDisplay {
  /** Wire kind the display came from (`request` / `response`). */
  origin: string
  /** The action the sender named (`analyze`, `notify`, …). */
  action: string
  /** One line: a block header, a round row, a list preview. */
  summary: string
  /** Markdown the reader gets in place of the envelope. */
  body: string
  /** The producer cut `body`; `content` still holds everything. */
  truncated?: boolean
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
  /** The same turn written for a human, when the Runtime has one. */
  display?: IntentDisplay
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

/* ------------------------------------------------------------------ */
/* Chat sessions (design 27; plan Wave 5.5 mobile alignment)           */
/* ------------------------------------------------------------------ */

/**
 * One durable row of a Chat session (`GET /api/chat-sessions/{id}/messages`).
 * Mirrors `chat_sessions_api::MessageRow` — the same view model Desktop's
 * `chatSessionStore` consumes; the mobile differences are presentation-only
 * (design 27 §6.5: the round's block collapses to its header line, a decision
 * card is answered from a bottom sheet).
 *
 * ADR-106 D3/D4: an `execution_round` row is a POINTER, not a copy — it has no
 * body, and everything shown for that round is read from the execution
 * projection (`GET /api/chat-blocks/{chatId}`). The transcript therefore carries
 * no card row any more (D8): a pending decision lives inside the block.
 */
/**
 * What happened to one row, as the service computed it (design 31 §6).
 *
 * The Chat service projects this onto every row it serves, so no surface has to
 * work it out — Desktop and Mobile read the same field. A surface that scans
 * the transcript for a row whose `meta.in_reply_to` matches an id is deriving
 * business state, and there were once four of those scans with three different
 * rules: a send could sit in "working on it" for 300 s after a card landed with
 * an empty coordinate.
 */
export interface ChatAnswerRef {
  message_id: string
  kind: string
}

/** Where a turn went: coordinates only. The round's content is read from the
 *  execution side, never copied here (design 31 §3.2 R3). */
export interface ChatRoundTarget {
  agent_instance_id: string
  agent_session_id: string
  round_id: string
  opened_new?: boolean
}

/** An addressing card's own state: the answers it offers and whether the human
 *  has already taken one. */
export interface ChatQuestionState {
  answered: boolean
  /** Whether an answer could still be *given* (as opposed to whether one was
   *  taken). A card asks about a row; when that row is not in the conversation
   *  — a card an older build wrote under a per-target delivery uuid — nothing
   *  can ever answer it, and the surface renders no card instead of a dead
   *  question whose tap 422s. Absent means the service did not say. */
  answerable?: boolean
  options: ChatCandidateOption[]
  allow_new_task?: boolean
}

export interface ChatDeliveryView {
  /** A row answers this one. Absent delivery means "not projected yet" (an
   *  optimistic row this device inserted), never "unanswered". */
  answered: boolean
  answered_by?: ChatAnswerRef
  /** Every round that answers this row, in log order. A group `@` fan-out opens
   *  one round per target against the same message, so this is a list: a single
   *  value would report the first target and lose the rest. */
  targets?: ChatRoundTarget[]
  question?: ChatQuestionState
  /** A send this row owed that failed — served **live**, never stored
   *  (ADR-105 D5, revision 2026-11-06). Gone after `expires_in` seconds. */
  alert?: ChatDeliveryAlert
}

/** A live delivery failure (`delivery.alert`): the machine token plus how long
 *  it is still worth painting. The wording belongs to the surface. */
export interface ChatDeliveryAlert {
  target: string
  error_code: string
  expires_in: number
}

export interface ChatMessageRow {
  seq: number
  ts: number
  from: string
  /** ADR-106 D1: user_message | agent_message | execution_round | system_notice */
  kind: string
  body: string
  to: string
  meta?: {
    message_id?: string
    conversation_id?: string
    /** ADR-106 D6 level 1: the reference row this turn continues, or, on a
     *  card/notice, the message the question was asked about. */
    in_reply_to?: string
    status?: string
    repo?: string
    notice_type?: string
    /** The group `@` fan-out: the participant ids this human row addressed
     *  (design 27 §5.3.1). The client resolves `@Name` text into this list at
     *  send time; the service stores it, and the surface paints a chip per id.
     *  Empty/absent = nobody was named (the default-responder or archive path). */
    mentions?: string[]
    /** ADR-106 D3: the round this reference row names. With `agent_session_id`
     *  + `agent_instance_id` it is the block's address — missing any of the
     *  three and there is no block to read. */
    round_id?: string
    /** The instance whose session ran the round (ADR-106 D4). */
    agent_instance_id?: string
    /** The execution session that ran the round — set on an `execution_round`
     *  reference row. */
    agent_session_id?: string
    /** ADR-106 D6: the round opened a NEW session rather than continuing one;
     *  absent when the turn could not say. */
    opened_new?: boolean
    /** The round's one-line name (ADR-106 D13) — a presentation hint, never a
     *  routing decision. From the Runtime's `reference_name`. */
    round_name?: string
    /** Legacy spelling of {@link round_name} on rows written before the rename.
     *  Read as a fallback, never written. */
    assignment_brief?: string
    /** ADR-106 D13: the PM task this round works on, read off the routing
     *  contract by the Runtime. Rendered as a chip on the block header. */
    task_id?: string
    /** On a `session_choice` / `confirm_continue` notice (ADR-106 D6 levels 3
     *  and 5): the sessions the human may pick, plus the always-offered escape
     *  (`allow_new_task`). Empty-bodied rows — the surfaces own the wording. */
    options?: ChatCandidateOption[]
    allow_new_task?: boolean
    /** ADR-100: display-only labels of the items the Chat-surface composer
     *  handed to this turn. Present only on the head row; never resolvable
     *  from here (see [`ExecAttachmentLabel`]). */
    exec_attachments?: ExecAttachmentLabel[]
    /** ADR-106 revision 2026-11-04: the `@session` reference material the human
     *  attached, pinned to its round by the Gateway at send time. One quadruple
     *  `{agent_instance_id, session_id, round_id, terminal}` per reference —
     *  coordinates only, never the body. Read to draw the source chips under a
     *  user bubble, echoed back verbatim when a card-answer re-delivers the same
     *  utterance. */
    refs?: Array<{
      agent_instance_id: string
      session_id: string
      round_id: string
      terminal: boolean
    }>
  }
  /** The service's projection for this row (design 31 §6). Present on every row
   *  the read endpoint serves; absent on a row this device inserted
   *  optimistically before the write's response came back. */
  delivery?: ChatDeliveryView
  attachment_ids?: string[]
  attachments?: Array<{ id: string; filename: string; mime: string; size: number }>
}

/* ------------------------------------------------------------------ */
/* Execution-block read (ADR-106 D3/D4/D5/D9)                           */
/* ------------------------------------------------------------------ */

/** The card a round is parked on (ADR-106 D8): what the round is waiting for,
 *  and what the human can answer with. Rendered inside the block, never as a
 *  chat row. */
export interface BlockCard {
  /** `approval` | `ask_question` | `pause`. */
  kind: string
  /** The coordinate the answer resolves against. */
  request_id: string
  /** The question, in the agent's own words. */
  prompt?: string
  /** Labels the human may pick; empty for a free-text answer. */
  options?: string[]
}

/** One round's projected content, verbatim from the execution session. */
/** The **human** rendering of an agent-origin turn (design 27 §5.4.5).
 *
 *  A round's `user.body` is the machine envelope (`[intent:…] {params}` plus the
 *  reply coordinates) because the model reads it; this is the same turn written
 *  for the reader. Declared once, above ({@link IntentDisplay}) — the block
 *  projection and a session-message entry are the same shape. */
export type RoundUserDisplay = IntentDisplay

export interface RoundProjection {
  round_id: string
  /** The Chat this round's session is bound to — checked server-side (D10). */
  chat_session_id?: string
  user?: {
    body: string
    ts: string
    /** What a human reads instead of `body`; see {@link RoundUserDisplay}. */
    display?: RoundUserDisplay | null
  } | null
  answer?: { body: string; ts: string } | null
  card?: BlockCard | null
  terminal: boolean
  /** `in_progress` / `completed` / `aborted` / `failed` / `waiting` /
   *  `interrupted` / `unknown`. */
  status: string
}

/** One entry of `GET /api/chat-blocks/{chatId}`'s answer, in request order. */
export interface ChatBlockEntry {
  ref: string
  agent_instance_id: string
  agent_session_id: string
  round_id: string
  /** `ok` renders the block; `pending` renders it live and may be re-read;
   *  `not_found` is an explicit hole; `unreachable` offers a retry (D9). */
  status: 'ok' | 'pending' | 'not_found' | 'unreachable'
  source?: 'cache' | 'runtime'
  projection?: RoundProjection
  detail?: string
}

/* ------------------------------------------------------------------ */
/* Addressing questions (ADR-106 D6 levels 3 and 5)                    */
/* ------------------------------------------------------------------ */

/** One offer on an addressing question (ADR-106 D6 levels 3 and 5). `why` is the
 *  runtime's reason for putting this session on the list — shown as the row's
 *  tooltip, never as the label: a human decides by name, not by inference score. */
export interface ChatCandidateOption {
  session_id: string
  label?: string
  status?: string
  origin?: string
  why?: string
}

/** `GET /api/chat-sessions/default` response envelope (the pair's session). */
export interface ChatSessionInfo {
  session_id: string
  /** `agent_user` (the 1:1 pair), `project_group` (a project's channel),
   *  `user_group` (a human-founded group), `user_user` (the DM twin). The
   *  group surfaces render the last two through the same panel (design 27 §4.4). */
  kind: string
  title: string
  project_id?: string
  created_at: number
  last_active_at: number
  message_count: number
  /** The session's roster as the user service knows it (PM-synced for a
   *  `project_group`, managed through the member routes for a `user_group`).
   *  `identity` tags each entry `agent` | `human`; absent on older servers,
   *  where the client resolves the id against its own directory instead. */
  participants?: Array<{ id: string; identity?: string }>
  /** `participant_id -> unread count` (design 27 §4.6). Only the list
   *  endpoint (`GET /api/chat-sessions/{me}`) fills it — the row a pair
   *  resolve returns without it. */
  unread?: Record<string, number>
  /** Sidebar meta line for a `project_group` row — the last message's text
   *  (the service owns the truncation). Mirrors the Desktop field. */
  last_message_preview?: string
  /** ADR-105: the agent instance an un-addressed message in this group is
   *  routed to. Absent = the group has no default and such a row archives. */
  default_responder?: string
  /** Group owner for a `user_group` (ADR-098 governance). */
  owner?: string
}

/** ADR-100 D12: one display label for something a composer handed to the task
 *  alongside a turn. Mirrors `acowork_core::protocol::AttachmentLabel` — the
 *  same projection the Chat Router renders into its prompt and the desktop
 *  renders into a chip.
 *
 *  `absPath` is the workspace arm's click coordinate (D12 Rev. 2026-11). It is
 *  read on the desktop, which resolves it against the Agent's workspaces; this
 *  side has no filesystem to open it with, so the chip here stays a plain
 *  label. The field is mirrored anyway so the two clients describe one shape
 *  instead of two. */
export interface ExecAttachmentLabel {
  filename: string
  /** The ADR-046 item arm (`attached_file` / `image_upload` / …); absent on a
   *  row written before the projection carried one. Drives the glyph — never
   *  text shown to the user. */
  kind?: string
  format?: string
  sizeBytes?: number
  startLine?: number
  endLine?: number
  /** Workspace arms only — nothing this client can open (see above). */
  absPath?: string
}

export interface ChatMessagesPage {
  messages: ChatMessageRow[]
  total: number
  offset: number
  limit: number
}
