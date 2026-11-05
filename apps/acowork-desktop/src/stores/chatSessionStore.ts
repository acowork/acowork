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
 *   Render-perf L1+L2+L3 (docs/plan/zh/chat-transcript-render-perf-plan.md)
 *   executed the OTHER half of the v1 assumption: the transcript is now
 *   memoized (L1), windowed through the shared `VirtualRowList` shell —
 *   only viewport rows are in the DOM (L2), and the doorbell path is
 *   reference-disciplined: an unchanged re-read sets nothing, and a changed
 *   one reuses the row objects it already had (L3). Do NOT write new chat
 *   surfaces against the old "full re-render per doorbell" cost model.
 *   Streaming itself remains unbuilt (the plan's deliberate non-goal).
 *
 * Freshness is push-first: `acowork/users/{me}/chat/changed` (the same
 * doorbell the human↔human inbox rides, published by the user service
 * after every append) fans out here from `chatStore`'s single MQTT
 * listener. Content-free signal → HTTP re-read, never a patched row.
 */

import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import { toWireAttachedItems, workspaceRefToAttachedItem } from "../lib/types";
import type { WorkspaceRef } from "../lib/types";
import {
  TURN_INFLIGHT_DEADLINE_MS,
  type InflightTurn,
} from "../lib/chatTurnInflight";
import { useAuthStore } from "./authStore";

// ── Wire types (mirror `chat_sessions_api.rs` responses) ──────────────

/** ADR-100: one display label for something the Chat-surface composer handed
 *  to the task alongside a turn (the execution surface's own sends never reach
 *  this code — Rev. 8 / D14 writes them to their own session, not to chat).
 *  Mirrors `acowork_core::protocol::AttachmentLabel`, the same projection the
 *  Chat Router renders into its prompt: the chip and the prompt describe one
 *  attachment with one shape.
 *
 *  `absPath` is the one field that is not display: a workspace arm ships the
 *  path its chip opens (ADR-100 D12 Rev. 2026-11), and nothing else may use it
 *  — the router's prompt line still prints the name and line range only. The
 *  upload arms carry no path and no id, because their bytes stay in the run
 *  that received them (design 27 §4.2.1). */
export interface ExecAttachmentLabel {
  filename: string;
  /** The ADR-046 item arm (`attached_file` / `image_upload` / …); absent on a
   *  row written before the projection carried one. Drives the glyph — never
   *  text shown to the user. */
  kind?: string;
  /** Upload arms only: document format (`pdf`, `png`). */
  format?: string;
  /** Upload arms only: blob size in bytes. */
  sizeBytes?: number;
  /** `attached_selection` only. */
  startLine?: number;
  endLine?: number;
  /** Workspace arms only (`attached_file` / `attached_selection`): the path the
   *  chip opens. Resolved client-side against the Agent's workspaces; a path
   *  that resolves nowhere is a visible "file not found", never a silent click. */
  absPath?: string;
}

/** One durable chat row. Mirrors `chat_sessions_api::MessageRow`. */
/**
 * What happened to one row, as the service computed it (design 31 §6).
 *
 * The Chat service projects this onto every row it serves, so no surface has to
 * work it out. A surface that scans the transcript for a row whose
 * `meta.in_reply_to` matches an id is deriving business state — that scan is
 * what kept a send claiming "working on it" for 300 s after a card landed with
 * an empty coordinate, and it existed in four places with three different
 * rules. Render this verbatim instead.
 */
export interface ChatAnswerRef {
  message_id: string;
  kind: string;
}

/** Where a turn went: coordinates only. The round's content is read from the
 *  execution session, never copied here (design 31 §3.2 R3). */
export interface ChatRoundTarget {
  agent_instance_id: string;
  agent_session_id: string;
  round_id: string;
  /** `true` = this turn opened the session, `false` = it carried on, absent =
   *  the delivery never said (ADR-106 D3 forbids faking it). */
  opened_new?: boolean;
}

/** An addressing card's own state: the answers it offers and whether the human
 *  has already taken one. */
export interface ChatQuestionState {
  /** A round now answers the message this card asked about — the re-delivery
   *  leaves the reference row, so the answer is the round's existence. */
  answered: boolean;
  options: Array<{
    session_id: string;
    label?: string;
    status?: string;
    origin?: string;
    why?: string;
  }>;
  allow_new_task?: boolean;
}

export interface ChatDeliveryView {
  /** True when a row answers this one — the fact the surfaces used to compute
   *  by scanning. Absent delivery means "not projected yet" (an optimistic row
   *  this device inserted before the service answered), never "unanswered". */
  answered: boolean;
  answered_by?: ChatAnswerRef;
  /** Every round that answers this row, in log order. A group `@` fan-out opens
   *  one round per target against the same message, so this is a list: a single
   *  value would report the first target and lose the rest. */
  targets?: ChatRoundTarget[];
  question?: ChatQuestionState;
}

export interface ChatMessageRow {
  seq: number;
  ts: number;
  from: string;
  /** user_message | agent_message | execution_round | system_notice */
  kind: string;
  body: string;
  to: string;
  meta?: {
    message_id?: string;
    conversation_id?: string;
    in_reply_to?: string;
    /** Design 27 §5.3.1: the Agent instances an @-fan-out woke. Only that one
     *  broadcast `user_message` carries it. ADR-105's default responder is in
     *  `auto_routed_to`, never here — the human typed no name. */
    mentions?: string[];
    /** ADR-105: the group's default responder that took an UN-addressed
     *  message. Never rendered as a mention chip (the human typed no name),
     *  and no longer an input to any rendering: the wait line claims this
     *  device's OWN send (`inflight`), keyed by the row id its response
     *  carried — ADR-106 D9 / D14 superseded ADR-105 D4's evidence slot. */
    auto_routed_to?: string;
    status?: string;
    repo?: string;
    /** ADR-106 D13: the PM task this round works on, read off the routing
     *  contract by the Runtime. Rendered as a chip on the block header — the
     *  claim/submit playbook stays model-facing. */
    task_id?: string;
    notice_type?: string;
    /** ADR-106 D3: the round this reference row names. With
     *  `agent_session_id` + `agent_instance_id` it is the block's address. */
    round_id?: string;
    /** The instance whose session ran the round (ADR-106 D4). */
    agent_instance_id?: string;
    /** The execution session that ran the round — set on an
     *  `execution_round` reference row. */
    agent_session_id?: string;
    /** ADR-106 D6: the reference row opened a NEW session rather than
     *  continuing one; absent when the turn could not say. */
    opened_new?: boolean;
    /** The router's one-line statement of what the round was asked to do
     *  (ADR-106 D13), carried on the reference row for the drill tooltip. */
    assignment_brief?: string;
    /** On a `session_choice` / `confirm_continue` notice (ADR-106 D6 levels 3
     *  and 5): the sessions the human may pick, plus the always-offered escape
     *  (`allow_new_task`). Empty-bodied rows — the surfaces own the wording. */
    options?: Array<{
      session_id: string;
      label?: string;
      status?: string;
      origin?: string;
      why?: string;
    }>;
    allow_new_task?: boolean;
    /** The SENDER's execution session (design 27 `source_agent_session_id`) —
     *  legacy: pre-ADR-106 rows named the run that produced a reply. */
    sender_session_id?: string;
    /** ADR-100: display-only labels of the items the Chat-surface composer
     *  handed to this turn (an execution-view send writes to its own session
     *  instead — Rev. 8 / D14). Present only on the head row; never resolvable
     *  from here (see [`ExecAttachmentLabel`]). */
    exec_attachments?: ExecAttachmentLabel[];
    /** ADR-106 revision 2026-11-04: the `@session` reference material the
     *  human attached, pinned to their round by the Gateway at send time.
     *  One quadruple `{agent_instance_id, session_id, round_id, terminal}`
     *  per reference — coordinates only, never the body (the block renders
     *  the material from the execution side). Read to draw the source chips
     *  under a user bubble, and echoed back verbatim when a card-answer
     *  re-delivers the same utterance. */
    refs?: Array<{
      agent_instance_id: string;
      session_id: string;
      round_id: string;
      terminal: boolean;
    }>;
  };
  attachment_ids?: string[];
  /** The service's projection for this row (design 31 §6). Present on every row
   *  served by the read endpoint; absent on a row this device inserted
   *  optimistically before the write's response came back. */
  delivery?: ChatDeliveryView;
  /** Files attached to this row. `thumb` is the upload-time sidecar token
   *  (`"jpg"` / `"txt"`); absent when the service produced no preview, so the
   *  row falls back to the plain filename capsule. */
  attachments?: Array<{
    id: string;
    filename: string;
    mime: string;
    size: number;
    thumb?: string;
  }>;
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
  /** Unix seconds when the backing PM project was archived. Only a
   *  `project_group` can carry it, and it is the whole "项目群聊只能在项目
   *  删除之后删除" precondition: PM stamps it when it deletes the project
   *  (there is nothing left to re-create the channel), and until then the
   *  delete is refused server-side. */
  project_deleted_at?: number;
  /** ADR-105: the agent instance an un-addressed message in this group is
   *  delivered to; absent = such messages stay archive-only (the historical
   *  behaviour, and the default for a group of humans). */
  default_responder?: string;
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

/** ADR-106 D6/D7's explicit address - the fact the human supplied, which no
 *  client and no model may re-decide. It is sent only when the composer (or the
 *  answer to a choice card) actually carries one: an absent address is what puts
 *  a turn on the ladder's lower levels, and inventing one here would take the
 *  Router's `answer` act away from a message that needs it. */
export interface ChatAddress {
  /** Group form of the second-level `@`: agent instance id -> the session the
   *  human named for that agent. Keys must be inside `targets`. */
  targetSessions?: Record<string, string>;
  /** 1:1 form: the first level is degenerate there, so the pick is a single
   *  session id. */
  targetSession?: string;
  /** D6's cheap escape beside every suggestion: continue nothing, new work.
   *  1:1 form - there is exactly one recipient, so a flag is enough. */
  forceNew?: boolean;
  /** Group form of the same refusal, per agent instance id.
   *  It cannot be a flag: one utterance fans out to several agents, and refusing
   *  continuation for the one the human was answering must not quietly start a
   *  new session for the others (the Gateway drops ids that are not targets of
   *  this message, and drops any that already carry an address). */
  forceNewFor?: string[];
  /** The id of the row being RE-delivered. Answering a `session_choice` /
   *  `confirm_continue` card re-sends the human's own utterance, now addressed,
   *  and the backend dedupes on this id - so the transcript keeps one message
   *  instead of drawing the same sentence twice. */
  messageId?: string;
  /** ADR-106 revision 2026-11-04 (reference redrawing): the execution-session
   *  rounds the human attached as **material** via the second level of the `@`.
   *  A reference is not a delivery — it never becomes `targetSessions`. Each
   *  entry carries the coordinates the picker knows (`agentInstanceId` +
   *  `sessionId`); the round is pinned server-side at send time. A card-answer
   *  re-delivery instead echoes the ORIGINAL row's already-pinned `meta.refs`
   *  verbatim, so it fills `roundId`/`terminal` too and the Gateway must not
   *  re-decide the material of a sentence already written. */
  refs?: ChatRefPick[];
}

/** One `@session` reference as the client states it. `roundId`/`terminal` are
 *  only present on a card-answer echo — a freshly picked reference names just
 *  the agent and session and lets the Gateway pin the round. */
export interface ChatRefPick {
  agentInstanceId: string;
  sessionId: string;
  roundId?: string;
  terminal?: boolean;
}

/** `POST /api/intents/chat-message` response. `message_id` is the durable id
 *  the Gateway gave the human's row — the key every outcome row threads itself
 *  to under `meta.in_reply_to`, which is why a client claim cannot invent one.
 *  A delivery that failed answers 200 with `status: "failed"`: the row is
 *  already durable, so only the delivery did not land — and that path writes no
 *  receipt back into the Chat (the 1:1 arm of `deliver_chat_message`), so no
 *  claim may wait for a row that will never exist. */
interface ChatSendResponse {
  message_id: string;
  status: string;
  error_code?: string;
  error?: string;
}

/** `POST /api/intents/mentions` response: the ONE `user_message` row's id
 *  (what every outcome threads to), plus one delivery result per target it
 *  woke. `routed: "archived"` carries an empty `results` — nobody was asked. */
interface MentionSendResponse {
  message_id: string;
  routed?: string;
  results?: Array<{ target?: string; message_id?: string; status?: string }>;
}

/** Map the client's reference picks to the wire `refs` array (snake_case). A
 *  fresh pick sends `{agent_instance_id, session_id}`; a card-answer echo also
 *  carries `round_id`/`terminal` so the Gateway passes it through instead of
 *  re-pinning the material of a sentence already written. Empty is not sent —
 *  its absence is "the human attached no material" on the wire. */
function toWireRefs(
  refs?: ChatRefPick[],
): Array<Record<string, string | boolean>> | undefined {
  if (!refs || refs.length === 0) return undefined;
  return refs.map((r) => ({
    agent_instance_id: r.agentInstanceId,
    session_id: r.sessionId,
    ...(r.roundId ? { round_id: r.roundId } : {}),
    ...(r.terminal !== undefined ? { terminal: r.terminal } : {}),
  }));
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
 *  execution session — at creation, and never on a reuse: a session that
 *  already exists keeps its own model / reasoning / workspace (ADR-099's
 *  revision of 2026-10-28). `undefined` = use the
 *  agent's own defaults. */
function sendChatMessage(
  me: string,
  chatSessionId: string,
  text: string,
  inReplyTo?: string,
  sessionConfig?: ChatSessionConfig,
  attachmentIds?: string[],
  attachedItems?: unknown[],
  address?: ChatAddress,
): Promise<ChatSendResponse> {
  return req<ChatSendResponse>("POST", "/api/intents/chat-message", {
    chat_session_id: chatSessionId,
    from_user: me,
    text,
    in_reply_to: inReplyTo ?? "",
    ...(sessionConfig ? { session_config: sessionConfig } : {}),
    ...(attachmentIds && attachmentIds.length > 0 ? { attachment_ids: attachmentIds } : {}),
    ...(address?.targetSession ? { target_session: address.targetSession } : {}),
    ...(address?.forceNew ? { force_new: true } : {}),
    ...(address?.messageId ? { message_id: address.messageId } : {}),
    // ADR-046 workspace references picked with "Add to Chat". The Gateway
    // forwards them to whichever execution session takes this routed turn
    // (design 27 §4.2.1) — they are paths, not bytes, so nothing is copied
    // into the Chat store.
    ...(attachedItems && attachedItems.length > 0 ? { attached_items: attachedItems } : {}),
    // ADR-106 revision 2026-11-04: `@session` reference material. The Gateway
    // pins each round before writing the row, so a bare pick sends only
    // (agent, session); a card-answer echo arrives pre-pinned.
    ...(() => {
      const wireRefs = toWireRefs(address?.refs);
      return wireRefs ? { refs: wireRefs } : {};
    })(),
  });
}

/** The group send entry (design 27 §5.3.1, ADR-105): the Gateway writes the
 *  ONE `user_message` row and routes a turn per target — so a group send must
 *  NOT also post the message.
 *
 *  An EMPTY `targets` is the "nobody was addressed" case, and it is a real
 *  instruction, not a missing one: the Gateway resolves the group's default
 *  responder (or archives the row when the group has none). That is why the
 *  archive-only `POST /api/chat-sessions/{id}/messages` is no longer used by
 *  this surface — routing an un-addressed message is a server decision, not a
 *  client one, so every client inherits it. */
function fanoutMentions(
  me: string,
  chatSessionId: string,
  text: string,
  targets: string[],
  attachmentIds?: string[],
  address?: ChatAddress,
): Promise<MentionSendResponse> {
  const targetSessions = address?.targetSessions ?? {};
  // A refusal to continue is only ever sent for a target with no address: the
  // two are contradictory instructions, and the Gateway answers the pair with a
  // 400 rather than picking one.
  const forceNewFor = [
    ...new Set([
      ...(address?.forceNew ? targets : []),
      ...(address?.forceNewFor ?? []),
    ]),
  ].filter((t) => !targetSessions[t]);
  return req<MentionSendResponse>("POST", "/api/intents/mentions", {
    chat_session_id: chatSessionId,
    from_user: me,
    targets,
    text,
    attachment_ids: attachmentIds ?? [],
    // An empty map is not sent as `{}`-noise: the field is optional server-side
    // and its absence is what "the human named no session" means on the wire.
    ...(Object.keys(targetSessions).length > 0 ? { target_sessions: targetSessions } : {}),
    ...(forceNewFor.length > 0 ? { force_new_for: forceNewFor } : {}),
    ...(address?.messageId ? { message_id: address.messageId } : {}),
    // ADR-106 revision 2026-11-04: `@session` reference material (see the 1:1
    // entry). A reference is not a delivery, so it never overlaps the (now
    // answer-only) `target_sessions`.
    ...(() => {
      const wireRefs = toWireRefs(address?.refs);
      return wireRefs ? { refs: wireRefs } : {};
    })(),
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

/** Cache of resolved object URLs, keyed by session/attachment (+thumb). */
const objectUrlCache = new Map<string, string>();

/** One authenticated GET against the session's blob route. Shared by the
 *  object-URL path (`<img>`) and the text path (read-only preview), because
 *  both need the same bearer header and the same URL shape. */
async function fetchChatAttachmentBlob(
  chatSessionId: string,
  attachmentId: string,
  opts?: { thumb?: boolean },
): Promise<Blob> {
  const thumb = opts?.thumb === true;
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(
    `${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(chatSessionId)}/files/${encodeURIComponent(attachmentId)}${thumb ? "?thumb=1" : ""}`,
    { headers: token ? { Authorization: `Bearer ${token}` } : {} },
  );
  if (!resp.ok) throw new Error(`attachment → ${resp.status}`);
  return resp.blob();
}

/**
 * Read one attachment as text, for the read-only preview tab.
 *
 * Session-scoped on purpose: the group surface has no owning `userId` in
 * scope, and the blob route is participation-checked server-side, so the
 * session is the only address this surface legitimately holds. Callers gate
 * on `canPreviewAttachment` — the size cap is a main-thread concern and this
 * function does not enforce it.
 */
export async function fetchChatAttachmentText(
  chatSessionId: string,
  attachmentId: string,
): Promise<string> {
  return (await fetchChatAttachmentBlob(chatSessionId, attachmentId)).text();
}

/** Attachment download needs the bearer token, so `<img src>` cannot work
 *  — hand out object URLs the same way `user-chat-api.ts` does. `thumb`
 *  serves the upload-time thumbnail sidecar (a 4K screenshot costs a few
 *  KB instead of the full blob) — 404 when the attachment has no sidecar,
 *  callers fall back to the full download. */
export async function chatAttachmentObjectUrl(
  chatSessionId: string,
  attachmentId: string,
  opts?: { thumb?: boolean },
): Promise<string> {
  const thumb = opts?.thumb === true;
  const key = `${chatSessionId}/${attachmentId}${thumb ? "/thumb" : ""}`;
  const hit = objectUrlCache.get(key);
  if (hit) return hit;
  const blob = await fetchChatAttachmentBlob(chatSessionId, attachmentId, opts);
  const url = URL.createObjectURL(blob);
  objectUrlCache.set(key, url);
  return url;
}

/**
 * An object URL this caller owns and must revoke.
 *
 * Why not the cache above: `objectUrlCache` lives for the process and never
 * revokes (a correct trade for thumbnails, which are small and recur), but a
 * full-size blob handed to the image lightbox is neither — one per image
 * viewed, and nothing in the cache would ever give the bytes back. Same
 * fetch, no entry: pair it with `ImageLightboxRequest.onClose`.
 */
export async function createChatAttachmentObjectUrl(
  chatSessionId: string,
  attachmentId: string,
  opts?: { thumb?: boolean },
): Promise<string> {
  const blob = await fetchChatAttachmentBlob(chatSessionId, attachmentId, opts);
  return URL.createObjectURL(blob);
}

/** The service's answer to "what would answering this card send?" — the
 *  human's own words, the card's coordinates, and the original row's pinned
 *  `@session` material (design 31 §7.2, ADR-106 D6). No address: which session
 *  the words go to is the click, and only the surface knows it. */
export interface CardAnswerFacts {
  agentId: string;
  messageId: string;
  text: string;
  attachmentIds: string[];
  refs: ChatRefPick[];
}

/** Why a card could not be answered, when it could not be.
 *
 *  `already_answered` is not an error to report: a round answers the message
 *  the card asked about, so the question is spent and the projection's next
 *  read closes it. `gone` covers the three ways the row stopped being a card
 *  (a non-card id, an origin row that is no longer readable, a session the
 *  caller is not in) — the surface says so instead of clicking into nothing. */
export type CardAnswerLookup =
  | { ok: true; facts: CardAnswerFacts }
  | { ok: false; reason: "already_answered" | "gone" };

/**
 * Resolve a card answer on the server (design 31 §6 — one derivation point).
 *
 * Why not derive it here: the answer is a copy of a row that may be far outside
 * the loaded window, and a surface can only copy what it has loaded. Both
 * surfaces used to rebuild the utterance from their own window and returned
 * silently when the row was gone, which made a scrolled-away card's buttons dead.
 */
export async function fetchCardAnswer(
  chatSessionId: string,
  cardMessageId: string,
): Promise<CardAnswerLookup> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(
    `${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(chatSessionId)}/messages/${encodeURIComponent(cardMessageId)}/answer`,
    { headers: token ? { Authorization: `Bearer ${token}` } : {} },
  );
  if (resp.status === 409) return { ok: false, reason: "already_answered" };
  // 404 (not a participant / no such session) and 422 (not a card, origin
  // gone) are the same fact to a surface: this card can no longer be answered.
  if (resp.status === 404 || resp.status === 422) {
    return { ok: false, reason: "gone" };
  }
  if (!resp.ok) throw new Error(`card answer → ${resp.status}`);
  const v = (await resp.json()) as {
    agent_id?: string;
    message_id?: string;
    text?: string;
    attachment_ids?: string[];
    refs?: Array<{
      agent_instance_id?: string;
      session_id?: string;
      round_id?: string;
      terminal?: boolean;
    }>;
  };
  return {
    ok: true,
    facts: {
      agentId: v.agent_id ?? "",
      messageId: v.message_id ?? "",
      text: v.text ?? "",
      attachmentIds: v.attachment_ids ?? [],
      // Echoed back verbatim (the quadruples were pinned when the row was
      // written); only the names are translated to the picker's spelling.
      refs: (v.refs ?? []).flatMap((r) =>
        r.agent_instance_id && r.session_id
          ? [
              {
                agentInstanceId: r.agent_instance_id,
                sessionId: r.session_id,
                ...(r.round_id ? { roundId: r.round_id } : {}),
                ...(r.terminal !== undefined ? { terminal: r.terminal } : {}),
              },
            ]
          : [],
      ),
    },
  };
}

// ADR-106 D8: there is no chat-side card state to project. A round's pending
// decision is the round's own state, read off its projection and answered at
// the execution session's write entry — so the closed/answered card id sets this
// module used to export are gone, along with the `interaction_request` rows they
// indexed.

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
  /** The POST this device has in flight. Deliberately NOT the wait-line / lock
   *  signal on its own: a POST answers as soon as delivery lands (~150 ms),
   *  while the turn's first durable row can be a minute behind it — that window
   *  is `inflight`'s job. */
  sending: Record<string, boolean>;
  /** The turn this device sent and has not seen an outcome for, per Chat
   *  session — ADR-106 D9's first-person claim, restored to the whole window
   *  the old `pendingTurn(rows, …)` used to cover without ever reading the
   *  transcript's shape. Written from the send's own response (the id the
   *  Gateway gave the human's row), dropped when the surface sees a row answer
   *  it (`settleTurn`) or when the deadline passes (`expireTurn`). In-memory
   *  only: a reload has no send of its own to claim, and so shows no wait line
   *  — the deliberate D9 boundary. */
  inflight: Record<string, InflightTurn | undefined>;
  /** Stop claiming a turn because the transcript already answers it. */
  settleTurn(chatSessionId: string, messageId: string): void;
  /** Stop claiming a turn because its outcome rows never arrived. */
  expireTurn(chatSessionId: string, messageId: string): void;
  /** Cache of the server-side `session_config` draft, keyed by chat
   *  session id. The session row is the source of truth (design 27 §4.5);
   *  this slot is hydrated by `openAgent` and updated by `saveDraft`'s
   *  response — never an independent frontend state. */
  draft: Record<string, ChatSessionConfig>;
  /** Workspace references ("Add to Chat") pending on this Chat surface's
   *  composer, keyed by chat session id. The execution view keeps its own list
   *  on the execution session (`chatStore.attachedContext`); a reference goes to
   *  whichever composer is on screen (see `lib/addToChat`). `sendText` clears it
   *  on a send that landed, so a failed send keeps what the human attached. */
  attachedContext: Record<string, WorkspaceRef[]>;
  /** Land a reference on this Chat session's composer. Idempotent by `id`:
   *  attaching the same path twice is one chip, not two. */
  addAttachedContext(chatSessionId: string, ref: WorkspaceRef): void;
  removeAttachedContext(chatSessionId: string, id: string): void;

  openAgent(agentId: string): Promise<void>;
  /** Project group chats currently on screen (keyed by session id) —
   *  doorbell `wake()` re-reads these windows too. */
  openGroupIds: Record<string, true>;
  openGroup(chatSessionId: string): Promise<void>;
  /** Send into a group: every send takes the intent channel, which writes the
   *  row and fans out. `targets` = the agents the human @-ed; an EMPTY list is
   *  a positive instruction since ADR-105 — the Gateway delivers to the group's
   *  default responder, or archives when the group has none. `inReplyTo`
   *  addresses a round's block (ADR-106 D6 level 1): it rides the 1:1 channel,
   *  which the backend also accepts for group sessions and resolves to the
   *  round's own agent and execution session. `address` is the second-level `@`
   *  (D7) - the session the human named, or the refusal to continue anything. */
  sendGroupText(
    chatSessionId: string,
    text: string,
    targets: string[],
    inReplyTo?: string,
    attachmentIds?: string[],
    address?: ChatAddress,
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
    address?: ChatAddress,
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

/** Row content equality for the doorbell path (render-perf L3). The chat log
 *  is append-only and rows immutable (`append_message` dedups by
 *  `message_id` and returns the existing row; nothing rewrites a line), so
 *  this is only ever comparing two parses of the same bytes — a full
 *  `JSON.stringify` compare is exact, needs no maintenance as the row shape
 *  grows, and costs milliseconds against the re-render it prevents. */
const rowEquals = (a: ChatMessageRow, b: ChatMessageRow): boolean =>
  JSON.stringify(a) === JSON.stringify(b);

/** Structural sharing for a re-read newest page: keep the existing row
 *  object whenever the content is unchanged, so a doorbell that appended
 *  one row repaints ONE memoized row instead of handing every mounted row
 *  a fresh JSON identity (which no memo can survive). Keyed by `seq` —
 *  unique within this session, and the window only ever spans one session. */
function shareRowIdentities(
  prev: ChatMessageRow[],
  next: ChatMessageRow[],
): ChatMessageRow[] {
  if (prev.length === 0) return next;
  const bySeq = new Map<number, ChatMessageRow>();
  for (const r of prev) bySeq.set(r.seq, r);
  return next.map((r) => {
    const old = bySeq.get(r.seq);
    return old !== undefined && rowEquals(old, r) ? old : r;
  });
}

export const useChatSessionStore = create<ChatSessionStore>((set, get) => ({
  byAgent: {},
  openGroupIds: {},
  windows: {},
  sending: {},
  inflight: {},
  draft: {},
  attachedContext: {},

  settleTurn: (chatSessionId, messageId) => dropTurn(chatSessionId, messageId),

  expireTurn: (chatSessionId, messageId) => dropTurn(chatSessionId, messageId),

  addAttachedContext: (chatSessionId, ref) => {
    set((s) => {
      const list = s.attachedContext[chatSessionId] ?? [];
      if (list.some((c) => c.id === ref.id)) return {};
      return { attachedContext: { ...s.attachedContext, [chatSessionId]: [...list, ref] } };
    });
  },

  removeAttachedContext: (chatSessionId, id) => {
    set((s) => {
      const list = s.attachedContext[chatSessionId];
      if (!list) return {};
      return { attachedContext: { ...s.attachedContext, [chatSessionId]: list.filter((c) => c.id !== id) } };
    });
  },

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
    if (existing?.resolved) {
      return;
    }
    try {
      // One pair, one Chat session (WeChat model): the pair's default
      // session, idempotently created server-side by `ensure_pair`.
      const def = await fetchDefaultChatSession(agentId);
      set((s) => ({
        byAgent: {
          ...s.byAgent,
          // Re-read after the await: `setMode` landing while the fetch was in
          // flight wrote a placeholder entry, and the pre-await `existing`
          // snapshot would silently overwrite it back to "chat".
          [agentId]: { chatSessionId: def.session_id, resolved: true, mode: s.byAgent[agentId]?.mode ?? "chat" },
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

  sendGroupText: async (chatSessionId, text, targets, inReplyTo, attachmentIds, address) => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) return false;
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: true } }));
    let ok = false;
    try {
      // Which row this send owes a claim on, once the response says somebody
      // took it. `null` for a send nothing was delivered to.
      let claimed: string | null = null;
      if (inReplyTo) {
        // Reply to a round's block: the 1:1 channel resolves the reference row
        // into its agent and execution session (ADR-106 D6 level 1); group
        // sessions are accepted for exactly this case (design 27 §5.5.4). A
        // level-2 address cannot ride this branch - level 1 already named the
        // session, and the backend drops `force_new` wherever an address exists.
        const resp = await sendChatMessage(me, chatSessionId, text, inReplyTo, undefined, attachmentIds, undefined, address);
        if (resp.status === "delivered") claimed = resp.message_id;
      } else {
        // Every group send takes the one entry: `targets` carries the @-ed
        // agents, and an empty list asks the Gateway to apply the group's
        // default responder (ADR-105) or archive. Splitting the two cases
        // client-side is what used to make an un-addressed reply vanish.
        const resp = await fanoutMentions(me, chatSessionId, text, targets, attachmentIds, address);
        // One row, N deliveries: the claim is on the ROW's id, and any target
        // that took the turn owes that row an answer. `archived` (no target)
        // and an all-failed fan-out owe none — a failed target already wrote its
        // own receipt server-side, and an archived row is the whole outcome.
        if ((resp.results ?? []).some((r) => r.status === "delivered")) {
          claimed = resp.message_id;
        }
      }
      ok = true;
      if (claimed) armTurn(chatSessionId, claimed);
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
      // An unresolved agent has no entry yet. Dropping the call here is what
      // made a drill into an execution session land on the Chat surface
      // instead: `openAgent` was still in flight, so its later
      // `{ ..., mode: existing?.mode ?? "chat" }` had no `existing` to
      // preserve. Writing the placeholder keeps the requested surface.
      const st = s.byAgent[agentId] ?? {
        chatSessionId: null,
        resolved: false,
        mode,
      };
      chatSessionId = st.chatSessionId;
      return { byAgent: { ...s.byAgent, [agentId]: { ...st, mode } } };
    });
    // Coming back to the Chat surface: the doorbell only wakes sessions
    // the store knows about, so re-read once on the flip.
    if (mode === "chat" && chatSessionId) {
      void get().refresh(chatSessionId, true);
    }
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
        // Equivalence gate (render-perf L3): a doorbell that carries no new
        // row for THIS session must not produce a new store identity at all.
        if (page.total === cur.total) {
          return;
        }
        set((s) => ({
          windows: { ...s.windows, [chatSessionId]: { ...cur, total: page.total } },
        }));
        return;
      }
      // At-tail (or forced) re-read. The log is append-only with immutable
      // rows, so `total` + the last row's `seq` pins the newest page exactly:
      // unchanged on both means the window already holds these bytes — skip
      // the set entirely (no new `windows` identity, no virtualizer churn,
      // no re-render of a single mounted row). A forced refresh always lands
      // (the caller may be re-claiming the tail from a different shape).
      const lastOld = cur?.tail[cur.tail.length - 1];
      const lastNew = page.messages[page.messages.length - 1];
      if (
        cur &&
        !force &&
        cur.older.length === 0 &&
        cur.total === page.total &&
        cur.tail.length === page.messages.length &&
        (lastOld?.seq ?? -1) === (lastNew?.seq ?? -2)
      ) {
        return;
      }
      // Rows that survived are handed back their old object identities
      // (structural sharing): a doorbell that appended one row repaints one
      // memoized row, not every mounted one.
      const tail = shareRowIdentities(cur?.tail ?? [], page.messages);
      const n = tail.length;
      set((s) => ({
        windows: {
          ...s.windows,
          [chatSessionId]: {
            ...emptyWindow(),
            tail,
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

  sendText: async (chatSessionId, text, inReplyTo, sessionConfig, attachmentIds, address) => {
    const me = useAuthStore.getState().account?.user_id;
    if (!me) return false;
    set((s) => ({ sending: { ...s.sending, [chatSessionId]: true } }));
    let ok = false;
    try {
      // ADR-106 D6 level 1: a reply to a round's block is an ordinary
      // `chat-message` turn that also carries an address, so attachments and
      // workspace refs ride it exactly as they do on a plain message (the card
      // answers that used to own this branch are gone — D8 moved them to the
      // execution session's write entry).
      const refs = get().attachedContext[chatSessionId] ?? [];
      const attachedItems =
        refs.length === 0 ? undefined : toWireAttachedItems(refs.map(workspaceRefToAttachedItem));
      const resp = await sendChatMessage(me, chatSessionId, text, inReplyTo, sessionConfig, attachmentIds, attachedItems, address);
      ok = true;
      // The claim starts here, from the id the Gateway gave THIS row. A failed
      // delivery is not claimed: that arm writes no receipt into the Chat, so
      // a claim would wait for a row that cannot exist and lock the composer
      // for the whole deadline.
      if (resp.status === "delivered") armTurn(chatSessionId, resp.message_id);
      if (attachedItems) {
        set((s) => ({ attachedContext: { ...s.attachedContext, [chatSessionId]: [] } }));
      }
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

  wake: () => wakeCoalesced(get),
}));

// ── In-flight turn claims (ADR-106 D9, restored window) ────────────────
//
// Module-level rather than per-view on purpose: the claim belongs to the Chat
// session, so closing the tab must not strand a turn nothing is left to expire.
// One timer per session — a second send replaces the claim, and the timer names
// the messageId it may clear so a stale deadline cannot drop a fresher turn's
// claim. `TURN_INFLIGHT_DEADLINE_MS` is the backstop for the case the surface
// cannot see: a Runtime that died between delivery and round open writes no
// outcome row, and without a deadline the composer would stay locked for good.

const turnTimers = new Map<string, ReturnType<typeof setTimeout>>();

/** Claim the turn this device just sent, and arm its deadline. */
function armTurn(chatSessionId: string, messageId: string): void {
  if (!messageId) return;
  useChatSessionStore.setState((s) => ({
    inflight: { ...s.inflight, [chatSessionId]: { messageId, at: Date.now() } },
  }));
  const prev = turnTimers.get(chatSessionId);
  if (prev !== undefined) clearTimeout(prev);
  turnTimers.set(
    chatSessionId,
    setTimeout(() => {
      turnTimers.delete(chatSessionId);
      dropTurn(chatSessionId, messageId);
    }, TURN_INFLIGHT_DEADLINE_MS),
  );
}

/** Release the claim — only if `messageId` is still the one being claimed. */
function dropTurn(chatSessionId: string, messageId: string): void {
  const cur = useChatSessionStore.getState().inflight[chatSessionId];
  if (!cur || cur.messageId !== messageId) return;
  const timer = turnTimers.get(chatSessionId);
  if (timer !== undefined) {
    clearTimeout(timer);
    turnTimers.delete(chatSessionId);
  }
  useChatSessionStore.setState((s) => ({
    inflight: { ...s.inflight, [chatSessionId]: undefined },
  }));
}

// ── Doorbell wake coalescing (render-perf L3) ──────────────────────
// The doorbell is content-free — it says only "something changed
// somewhere" — and one agent turn rings it several times (pointer row,
// card row, terminal row, per participant). A trailing 250ms debounce
// turns a burst into ONE re-read round; the latency is invisible next to
// the HTTP round trip, and the transcript (not the badge — unread counts
// ride the session list) is the only thing delayed. Within a round the
// per-session re-reads run in PARALLEL (they were a serial `await` loop:
// N sessions cost N round trips); per-session ordering stays safe because
// `refresh` re-reads the store state only after its own fetch resolved.
const WAKE_DEBOUNCE_MS = 250;
let wakeTimer: ReturnType<typeof setTimeout> | null = null;

function wakeCoalesced(get: () => ChatSessionStore): Promise<void> {
  return new Promise<void>((resolve) => {
    if (wakeTimer !== null) clearTimeout(wakeTimer);
    wakeTimer = setTimeout(() => {
      wakeTimer = null;
      const s = get();
      const ids = new Set<string>();
      // The chat session is pinned — even while the exec surface is shown
      // its in-flight turn drives the chat tab's status, so refresh
      // regardless of `mode`. Open project group chats ride the same bell.
      for (const st of Object.values(s.byAgent)) {
        if (st.chatSessionId) ids.add(st.chatSessionId);
      }
      for (const sid of Object.keys(s.openGroupIds)) ids.add(sid);
      Promise.allSettled([...ids].map((id) => s.refresh(id))).then(() => resolve());
    }, WAKE_DEBOUNCE_MS);
  });
}

/** Presence fan-out target: the chat/changed doorbell also wakes agent
 *  chats. Called from `chatStore`'s single `user-chat-changed` listener
 *  (next to `useUserChatStore.wake`) — one registration site per webview. */
export function wakeAgentChat(): void {
  void useChatSessionStore.getState().wake();
}
