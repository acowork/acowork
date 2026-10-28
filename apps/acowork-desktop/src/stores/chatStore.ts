import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ChatMessage, ContextUsageInfo, TokenUsage, ToolApprovalNeededEvent, PaginatedMessages, ConversationEntry, SessionStatus, SessionInfo, AskQuestionEvent, EventClearedEvent, ModelEntry, TodoItem, AttachedItem, ProviderAccount, VaultKeyEntry, AgentLifecycleState } from "../lib/types";
import { toWireAttachedItems } from "../lib/types";
import { isAtTail } from "../lib/paginationUtils";
import { useAgentStore } from "./agentStore";
import { useGatewayStore, stopGatewayDeathWatch } from "./gatewayStore";
import { useUserProfileStore } from "./userProfileStore";
import { useAuthStore } from "./authStore";
import { wakeAgentChat, sendPinnedChatMessage } from "./chatSessionStore";
import { useProjectGroupStore } from "./projectGroupStore";
import { useWorkspaceStore } from "./workspaceStore";
import { releaseAdapterSession, clearOptimisticEntries, clearAllOptimisticEntries, ingestStreamDelta, ingestRecordComplete, getChatAdapterSession } from "../components/chat/chatAdapterStore";
import { getGatewayUrl } from "../lib/config";
import { emitAgentConfigRefresh } from "../lib/refresh";
import { sessionConfigToPatch, deriveSessionTitle, type SessionConfigInput } from "../lib/sessionConfigMapper";
import { resolveDefaultReasoningEffort } from "../lib/modelCapabilities";
import { with503Retry } from "../lib/httpRetry";
import * as sessionControl from "../lib/session-control";
import { verifyAgentHealth, fetchProviderModels } from "../lib/gateway-api";
import i18n from "../i18n";
import { showToast } from "../components/common/ToastProvider";
import { log } from "../lib/logger";
import type { LlmAvailability } from "../lib/llmAvailability";
import { llmAvailabilityFromWire } from "../lib/llmAvailability";

// ---------------------------------------------------------------------------
// ADR-067: context-usage section merge.
//
// The runtime computes per-section byte sizes only in `process_llm_response_usage`
// (it has a `ContextBuilder`). The retained `session_state` snapshot and the
// `fetchSessionState` HTTP pull build `context_usage` from persisted tokens and
// therefore carry no `sections`. Without this merge, the last session_state
// to arrive would overwrite the sections delivered by the context_usage chunk,
// and the input-box popover breakdown would always render 0%.
//
// The same holds for `iteration` (per-session lifetime LLM-call count):
// session_state / fetchSessionState carry it only from the persisted meta,
// which can briefly lag the live `context_usage` push — preserving the last
// known value keeps the "Iterations" display from glitching to the
// assistant-message fallback between pushes.
function mergeContextUsage(
  prev: ContextUsageInfo | null | undefined,
  next: ContextUsageInfo,
): ContextUsageInfo {
  const preserved: Partial<ContextUsageInfo> = {};
  if (prev?.sections && !next.sections) {
    preserved.sections = prev.sections;
  }
  if (prev?.iteration != null && next.iteration == null) {
    preserved.iteration = prev.iteration;
  }
  if (Object.keys(preserved).length === 0) return next;
  return { ...next, ...preserved };
}

// ---------------------------------------------------------------------------
// ADR-050 C2: the per-session active stream tracker, throttle timestamps,
// and the 500ms cadence timer all moved to `chatAdapterStore` (see
// `apps/acowork-desktop/src/components/chat/chatAdapterStore.ts`).
// chatStore no longer mutates streaming state — it forwards stream_delta
// and record_complete to the adapter via `ingestStreamDelta` /
// `ingestRecordComplete` and lets the adapter own the live surface.
// ---------------------------------------------------------------------------



// ── messages[] cache: grows continuously, no sliding window ──
//
// The messages array grows as the user scrolls up (loadBefore prepends)
// or as new messages arrive via streaming (append).  No trimming is
// performed — the cache holds all messages that have been loaded.
// Memory is released when the session is closed or switched away.
// At ~593 messages / 3 MB, this is well within acceptable limits.
//
// Counted in raw-entry units (same as backend `PaginatedMessages.limit`).
// A single display group can occupy multiple slots here, which is fine:
// the cache only bounds memory, the UI rendering handles folding.

// ── Message window merge ──────────────────────────────────────────────
//
// Single source of truth for "what should `messages[]` look like after a
// new HTTP `/sessions/{sid}/messages` response lands?"
//
// Invariants (P0-1, P0-3 from the messages review):
//
//   1. The HTTP response is the authoritative timestamp-ordered window.
//      Any cache entry whose id appears in the response MUST be replaced
//      by the response copy (backend wins for content; ties impossible in
//      practice but we dedupe defensively).
//
//   2. Cache entries with ids NOT in the response are preserved (they
//      belong to a different window — e.g. older entries loaded via
//      `loadPrevPage`). They keep their relative timestamp order.
//
//   3. The final array is sorted by `timestamp` ascending. `Array.sort`
//      is stable in V8 since ES2019, so rows with identical `timestamp`
//      keep their input order — important when attachment system rows and
//      their owning user entry share an ISO millisecond.
//
//   4. No path may wipe `messages[]` based on a failed/aborted HTTP
//      request. Only the response handler may overwrite the window, and
//      only with merged content (never an empty reset).
//
// ADR-050 C2: the optimistic user-message overlay no longer lives in
// chatStore — it is owned by `chatAdapterStore`.  The HTTP response
// handler is responsible for calling `clearOptimisticEntries(agentId,
// sessionId, confirmedIds)` with the set of server-echoed ids so the
// adapter can drop the now-confirmed entries from its overlay.  The
// merge function itself stays a pure `(cache, server) => messages`
// transformation; the overlay is a separate concern.
//
// `mergeMessageWindow` is exported (pure function) so the regression tests
// in `chatStore.test.ts` can pin the contract without needing a Zustand
// store or fetch mock.
export interface MergedMessageWindow {
  /** Sorted, deduped message array ready to write into `messages[]`. */
  messages: ChatMessage[];
}

export function mergeMessageWindow(
  cache: ChatMessage[],
  server: ChatMessage[],
): MergedMessageWindow {
  // Step 1: server is authoritative for its ids. Build a Map<id, msg>
  // starting from server entries so duplicates in `server` itself are
  // resolved in favor of the first occurrence (server is already ts-
  // ordered, so the first occurrence is the oldest; ties are harmless).
  const byId = new Map<string, ChatMessage>();
  for (const m of server) byId.set(m.id, m);

  // Step 2: cache contributes any entry whose id is NOT in the server
  // window. These are older or otherwise out-of-window rows that the
  // server didn't echo back this round (e.g. a paginated window that
  // doesn't overlap the cache's older entries).
  for (const m of cache) {
    if (!byId.has(m.id)) byId.set(m.id, m);
  }

  // Step 3: stable sort by timestamp ascending. The merge Map preserves
  // insertion order on iteration, and Array.sort is stable, so rows
  // sharing an ISO millisecond keep the layered order
  // (server → cache).
  const messages = Array.from(byId.values()).sort(
    (a, b) => a.timestamp - b.timestamp,
  );

  return { messages };
}

/**
 * Write a single user message into the optimistic overlay.
 *
 * ADR-050 C2: this helper is removed.  The optimistic overlay is no
 * longer owned by chatStore — `chatAdapterStore.ingestOptimisticUserMessage`
 * receives the same entry list.  chatStore keeps no per-session
 * optimistic state and the merge function no longer takes an overlay
 * parameter.
 */

// ── ADR-035 C2: assistant activeStream safety valve ──
//
// ADR-050 C2: the active-stream tracker moved to `chatAdapterStore`,
// which now owns the `ASSISTANT_LINE_SAFETY_CAP` /
// `ASSISTANT_REPLYING_LINE_THRESHOLD` constants and the per-session
// stream bookkeeping.  chatStore no longer mutates streaming state.

// ── MQTT connection watchdog (P0 of unified-diagnostics plan) ─────────
//
// ADR-036 + ADR-065: the Rust eventloop is the source-of-truth for
// MQTT connection state. The frontend's `mqttConnected` boolean is the
// consumer of that truth. But historically the consumer could fall
// behind: the `mqtt-status` event could be emitted BEFORE the JS
// `listen()` call resolved (wake-recovery webview reload, listener
// registration race) and a 1s/30-attempts polling fallback was the only
// lifeline — after 30 seconds the polling stopped, leaving the frontend
// stuck.
//
// The watchdog below extends the polling fallback to:
//   1. Run forever (5s interval, only while not connected): armed on
//      init when not connected, re-armed by any non-connected
//      `mqtt-status` event after a connected episode, and stopped the
//      moment `connected` is observed.
//   2. Detect Rust-vs-frontend state inconsistency and self-heal.
//   3. Promote a connecting episode stuck past
//      `STUCK_FORCE_RECONNECT_MS` to `stale` (plan §3.1.1) and
//      auto-trigger `force_reconnect_mqtt`, restarting the episode
//      clock so a still-stuck session retries on the next window.
//
// The 5-state `effectiveConnection` is the UI-facing summary that the
// input box placeholder consumes. It collapses the raw
// {known, connected, connecting, reconnecting, reason} snapshot into a
// single value the user can act on: "Connected / Connecting /
// Reconnecting / Stale (auto-recovering) / Disconnected / Idle".

/** UI-facing MQTT connection state. Computed, never persisted. */
export type ConnectionStatus =
  | "connected"
  | "connecting"
  | "reconnecting"
  | "stale"
  | "disconnected"
  | "idle";

/** One row of the in-memory transition ring buffer (P0 of plan §3.1.2 P0-4). */
export interface TransitionLogEntry {
  timestamp: number;
  from: ConnectionStatus;
  to: ConnectionStatus;
  reason?: string | null;
}

/**
 * Raw MQTT liveness snapshot — the Rust eventloop is the source of
 * truth (ADR-036 / ADR-065). `connecting` / `reconnecting` are the
 * transient flags `mqtt_status_to_payload` attaches; every consumer
 * must TRANSPORT them verbatim, never collapse them into the
 * `connected` boolean (doing so renders a connecting client as
 * `disconnected` — review finding B-2).
 */
export interface MqttStatusSnapshot {
  known: boolean;
  connected: boolean;
  connecting?: boolean;
  reconnecting?: boolean;
  reason?: string | null;
}

/**
 * Compute `effectiveConnection` from the raw MQTT snapshot the Rust
 * side returns (or emits via `mqtt-status`).
 *
 * Pure function (no clock dependency) — the stale-upgrade rule lives in
 * `applyConnectionTransition` instead, so this remains unit-testable in
 * isolation.
 */
export function computeEffectiveConnection(
  snapshot: MqttStatusSnapshot | null,
): ConnectionStatus {
  if (!snapshot || !snapshot.known) return "idle";
  if (snapshot.connected) return "connected";
  if (snapshot.connecting) return "connecting";
  if (snapshot.reconnecting) return "reconnecting";
  return "disconnected";
}

/** Maximum number of transition log entries retained for diagnostics UI. */
const TRANSITION_LOG_CAPACITY = 20;

/** Polling interval for the watchdog (runs only while not connected). */
const WATCHDOG_INTERVAL_MS = 5_000;

/** Deadline for the whole connecting episode (plan §3.1.1 / D-5):
 *  `connecting` / `reconnecting` older than this is promoted to
 *  `stale` and gets one `force_reconnect_mqtt`; the episode clock is
 *  then restarted so a still-stuck session retries every window.
 *  Exported so the input-area status banner (P0-7) can compute the
 *  "X seconds until auto-recovery" countdown UI without hardcoding 30s
 *  in two places. */
export const STUCK_FORCE_RECONNECT_MS = 30_000;

/** A `connecting` / `reconnecting` / `stale` status means a connection
 *  attempt is in flight — the shared episode clock keeps running while
 *  the machine stays inside this family. */
function isConnectingFamily(status: ConnectionStatus): boolean {
  return status === "connecting" || status === "reconnecting" || status === "stale";
}

/**
 * Advance the shared connecting-episode clock (pure). The clock starts
 * when the machine enters the connecting family, survives moves WITHIN
 * the family (connecting → reconnecting → stale, or back), and resets
 * to `null` the moment the connection settles (connected /
 * disconnected / idle). `applyConnectionTransition` owns the module
 * state next to `_mqttPollHandle`; exported for unit tests.
 */
export function updateConnectingSince(
  prev: number | null,
  nextEffective: ConnectionStatus,
  now: number = Date.now(),
): number | null {
  if (!isConnectingFamily(nextEffective)) return null;
  return prev ?? now;
}

/** Build a TransitionLogEntry from a status change. Caps the buffer. */
export function appendTransition(
  log: TransitionLogEntry[],
  from: ConnectionStatus,
  to: ConnectionStatus,
  reason: string | null,
): TransitionLogEntry[] {
  if (from === to) return log;
  const entry: TransitionLogEntry = { timestamp: Date.now(), from, to, reason: reason ?? null };
  const next = [...log, entry];
  if (next.length > TRANSITION_LOG_CAPACITY) {
    return next.slice(next.length - TRANSITION_LOG_CAPACITY);
  }
  return next;
}

/** Dedupe adjacent identical error strings — prevents banner flicker on
 *  repeated transient failures. */
export function dedupeError(prev: string | null, next: string | null): string | null {
  if (!next) return null;
  if (prev === next) return prev;
  return next;
}

// ── Sender info helpers ────────────────────────────────────────────────

function getAgentSenderInfo(agentId: string): { senderDisplayName?: string; senderRole?: string } {
  const store = useAgentStore.getState();
  const agent = store.agents[agentId]?.meta;
  return {
    // ADR-009 §V-Q: name is server-side — no localStorage overlay.
    senderDisplayName: agent?.display_name ?? agent?.name,
    senderRole: agent?.role,
  };
}

function getUserSenderInfo(): { senderDisplayName?: string } {
  try {
    const profile = useUserProfileStore.getState().profile;
    return { senderDisplayName: profile.displayName };
  } catch {
    return { senderDisplayName: i18n.t("common.me") };
  }
}

// ---------------------------------------------------------------------------
// Per-session chat state — each session owns an independent instance
// ---------------------------------------------------------------------------

/** State for a single conversation session within an agent. */
export interface QueuedMessage {
  content: string;
  attachedItems: AttachedItem[];
}

/**
 * ADR-099 (design 27 §5.5.5): an inbound intent routed into a session while it
 * was already busy, mirrored from the backend `session_state` push. Unlike the
 * frontend-only `queuedMessages` (drafts the user parked locally), this is the
 * real server-side queue — e.g. a chat-forwarded message waiting for the
 * running execute session to pick it up.
 */
export interface PendingIntent {
  message_id: string;
  from: string;
  preview: string;
  from_chat: boolean;
}

interface SessionChatState {
  messages: ChatMessage[];
  tokenUsage: TokenUsage | null;
  contextUsage: ContextUsageInfo | null;
  /**
   * One-shot locate request (ADR-081 §4.2 conversation hits): when set
   * and matching the active session, ChatPanel scrolls the message list
   * to `targetArrayIdx` (index into `messages[]`) and briefly
   * highlights the block. Cleared after the highlight timeout — it is a
   * transient hint, not persisted state.
   */
  pendingLocate: {
    sessionId: string;
    messageIndex: number;
    /** Resolved index into `messages[]` after window alignment. */
    targetArrayIdx: number;
  } | null;
  /**
   * Pagination window coordinates returned by the last /messages HTTP
   * response. Both `messageOffset` and `messageLimit` are measured in
   * **raw entries** (one JSONL line each — a single user / assistant /
   * thought / tool_call / tool_result row), exactly like the backend
   * `PaginatedMessages` response. Display-group collapsing
   * (think + tool_call + tool_result → one chip) is the `MessageBlock`
   * abstraction local to `ChatPanel.tsx` and never reaches this state.
   *
   * ADR-050: **forward (oldest-end) semantics**:
   *
   * - `messageOffset == 0`               → window anchored at the OLDEST entry.
   * - `messageOffset + messageLimit >= messageTotal` → window touches the
   *   NEWEST entry (the cache "is at the tail").
   * - `messageOffset > 0`                → there are older entries beyond the
   *   window's left edge (scroll-up can load them).
   * - `messageOffset + messageLimit < messageTotal` → there are newer entries
   *   beyond the window's right edge (scroll-down can load them).
   *
   * Initial load sets `messageOffset = 0` and `messageLimit = 0`.
   * See `PaginatedMessages` in `lib/types.ts` for the request/response shape.
   */
  messageOffset: number;
  messageLimit: number;
  messageTotal: number;
  /**
   * Cache-integrity flag: true when `messages[]` is known to be a
   * PARTIAL, non-authoritative slice of the conversation.
   *
   * Set by `clearSessionMessages` (which wipes `messages[]` and resets
   * the pagination cursor).  After a clear, the array can only be
   * partially repopulated by background events (e.g. `record_complete`
   * writes for a session the user switched away from mid-stream) — the
   * older history (including the first user message) never comes back
   * until a real HTTP window load.  Cleared back to false by any
   * successful `loadSessionMessages` (the merge makes the cache
   * authoritative for the fetched window again).
   *
   * Consumers must gate their "cache already has data → skip reload"
   * shortcuts on `!messagesStale`; otherwise a partial cache is
   * mistaken for a complete one and the conversation renders with
   * missing messages until the next clear+reload cycle.
   */
  messagesStale: boolean;
  /**
   * Cursor-integrity flag: true once an HTTP `/messages` window has landed
   * for the CURRENT cache, i.e. `messageOffset` / `messageLimit` /
   * `messageTotal` describe a real server-reported window.
   *
   * This is a different fact from `messagesStale`, and the two are NOT
   * interchangeable:
   *
   * - `messagesStale` answers "is the array a complete slice of the
   *   conversation?" — cleared only by a full window load.
   * - `windowAuthoritative` answers "do the pagination coordinates mean
   *   anything yet?" — a cache can hold rows and still have no window.
   *
   * That second case is normal and must be expected: `record_complete`
   * appends live tail rows to `messages[]` for a session whose history was
   * never fetched (a running session right after the Desktop restarts, a
   * session the user has not opened yet). Those rows are real data, but they
   * carry no coverage claim — while the flag is false the store must NOT
   * treat `messageOffset` as the left edge of what it holds. The cursor
   * union math in `loadSessionMessages` is only valid between two
   * authoritative windows; over a live-seeded array it fabricates a
   * `[0, total)` window and permanently hides the head (`hasOlder` false,
   * scroll-up dead — regression t-f9a9fbc7).
   *
   * Set by: a successful `loadSessionMessages` response.
   * Cleared by: every path that invalidates the window
   * (`clearMessages` / `clearSessionMessages` / `clearSessionState` /
   * `trimMessagesTo`). Default false, which is why a freshly created session
   * state adopts the server's window verbatim on its first load.
   *
   * Invariant: every writer that sets `messagesStale: true` also clears this
   * flag, so "stale" implies "not authoritative" today. The two are kept
   * separate because they answer different questions (see above) and because
   * the union branch's `|| messagesStale` arm is defence in depth for a future
   * writer that forgets. If a third state ever appears, collapse both into a
   * single `windowState: "none" | "stale" | "authoritative"` discriminator
   * rather than growing a fourth boolean.
   */
  windowAuthoritative: boolean;
  pendingApproval: Record<string, ToolApprovalNeededEvent>;
  pendingQuestions: AskQuestionEvent[];
  isLoadingSession: boolean;
  /** Owner load-sequence of `isLoadingSession` (0 = no owner). Set when
   *  a load flips `isLoadingSession: true`; every settle point of
   *  `loadSessionMessages` checks ownership before clearing, so a
   *  superseded / aborted load can never orphan the spinner. */
  loadOwnerSeq: number;
  loadError: string | null;
  /** ADR-014/021: Session lifecycle status from backend (sole source of truth for "sending" state) */
  sessionStatus: SessionStatus | null;
  /** Last accessed timestamp — used for LRU eviction */
  lastAccessed: number;
  /** Per-session todo list (from todo_write tool) */
  todos: TodoItem[];
  /** Per-session queued messages (typed during streaming, sent when agent becomes idle) */
  queuedMessages: QueuedMessage[];
  /**
   * ADR-099 (design 27 §5.5.5): intents routed into this session while it was
   * busy, mirrored from the backend `session_state` push. Shown in the
   * execution pending queue alongside the local `queuedMessages`.
   */
  pendingIntents: PendingIntent[];
  /** Per-session selected model */
  model: string | null;
  /** Per-session selected provider */
  provider: string | null;
  /**
   * Multi-account: the `account_id` of `provider` this session uses.
   * `null` = the provider's first account (legacy / single-key sessions).
   */
  providerAccountId: string | null;
  /**
   * Per-session title — the live mirror of the backend config `title`.
   * Like `model` / `provider`, it is fed ONLY through `sessionConfigToPatch`
   * (HTTP `fetchSessionConfig` + MQTT `session_config`), so it lands whether
   * or not the session has a row in `agentStore.sessions[]`. It is the
   * display source for the session you are currently viewing; the list /
   * sidebar fall back to the `sessions[]` snapshot for never-opened rows.
   */
  title: string | null;
  /** Current model chars/token ratio from API calibration */
  ratio: number | null;
  /** Per-session reasoning effort override (frontend display only, source of truth is Runtime) */
  reasoningEffort: string | null;
  /** Per-session temperature override (from Runtime, persisted in JSONL metadata) */
  temperature: number | null;
  /**
   * ADR-074: per-session context window override (mirror of the backend
   * `ConversationSession.context_window`). `null` = no override — the
   * session inherits the per-agent chain. Named distinctly from
   * `agentStore.contextWindow` (the agent-level setting) on purpose.
   * Presence ⟺ override exists; cleared via `0` / `null` on PUT.
   */
  sessionContextWindow: number | null;
  /** Context compaction in progress (both manual and auto triggers) */
  isCompacting: boolean;
  /** File tree expanded directory paths (persisted per-session) */
  treeExpandedPaths: string[];
  /** Files/directories/selection attached to chat context (persistent until manually removed) */
  attachedContext: Array<{
    id: string;
    type: "file" | "directory" | "selection";
    name: string;
    absPath: string;
    /** Line range for selection type (1-based, inclusive) */
    startLine?: number;
    endLine?: number;
  }>;
  /** ADR-025: Whether more undelivered data exists (batch catch-up signal) */
  hasMoreIncremental: boolean;
  /** ADR-021: Per-session AbortController for cancelling in-flight loadSessionMessages */
  abortController: AbortController | null;
  /** ADR-021: Per-session load sequence number to prevent race conditions */
  loadSequence: number;
  /** Whether the agent is currently in reasoning/thinking phase */
  isReasoning: boolean;
  /**
   * ADR-038: Session lifecycle readiness. Set true when the Runtime
   * publishes `session_opened` (the session is confirmed Active in memory).
   * Set false initially and on `session_not_opened`. Input box / send button
   * gate on `isSessionReady` so the user can never send a `chat_message`
   * to a Closed or NotFound session — the gate is enforced client-side
   * and the Runtime also enforces it server-side via state guard.
   */
  /** Per-session loading-more flag. Guards against concurrent pagination
   *  requests (scroll-up / ensureLatestInCache) for THIS session only -
   *  other sessions are unaffected. Previously a global boolean which
   *  caused cross-session blocking. */
  isLoadingMore: boolean;
  /** ADR-045: Per-tool progress heartbeat state.
   *  Keyed by tool_call_id. Entry is created on first `tool_progress`
   *  event (5s after tool start) and removed when the matching
   *  `tool_result` record arrives. Absence of an entry means the tool
   *  completed in <5s — keep the pre-ADR-045 UX (breathing dot). */
  toolProgress: Record<string, { elapsedMs: number; timeoutMs: number }>;
  /**
   * Server-side error from MQTT `error` event.  NOT persisted to JSONL
   * by the backend, so it must NOT enter the `messages` array (which is
   * a sliding window over JSONL).  Rendered as a dismissible banner in
   * ChatPanel, same pattern as RetryWaitBanner / DebugPausedBanner.
   */
  serverError: { content: string; errorDetail?: string; errorType?: string; timestamp: number } | null;
  isSessionReady: boolean;
}

const DEFAULT_SESSION_STATE: SessionChatState = {
  messages: [],
  pendingLocate: null,
  tokenUsage: null,
  contextUsage: null,
  messageOffset: 0,
  messageLimit: 0,
  messageTotal: 0,
  messagesStale: false,
  windowAuthoritative: false,
  pendingApproval: {},
  pendingQuestions: [],
  isLoadingSession: false,
  loadOwnerSeq: 0,
  loadError: null,
  sessionStatus: null,
  lastAccessed: 0,
  todos: [],
  queuedMessages: [],
  pendingIntents: [],
  model: null,
  provider: null,
  providerAccountId: null,
  title: null,
  ratio: null,
  reasoningEffort: null,
  temperature: null,
  sessionContextWindow: null,
  isCompacting: false,
  treeExpandedPaths: [],
  attachedContext: [],
  hasMoreIncremental: false,
  abortController: null,
  loadSequence: 0,
  isReasoning: false,
  isSessionReady: false,
  isLoadingMore: false,
  toolProgress: {} as Record<string, { elapsedMs: number; timeoutMs: number }>,
  serverError: null,
};

// ---------------------------------------------------------------------------
// Per-agent state — owns session states, WebSocket, model info
// ---------------------------------------------------------------------------

/** State for a single agent — contains all session states + agent-level resources. */
interface AgentState {
  /** Per-session chat states — the core of session isolation */
  sessionStates: Record<string, SessionChatState>;
  /** Currently active session ID for this agent */
  activeSessionId: string | null;
  /** ADR-015: All session IDs that are open as tabs (ordered, max 32) */
  openSessionIds: string[];
  /** Last loaded session ID — prevents redundant reload */
  lastLoadedSessionId: string | null;
  /** Session init in progress */
  isSessionInitLoading: boolean;
  /** ADR-012: Agent's preferred model — set on every model_switch, inherited by new sessions */
  preferredModel: string | null;
  /** ADR-012: Agent's preferred provider */
  preferredProvider: string | null;
  /**
   * LLM availability for THIS agent, mirrored from its own
   * `SessionConfig.llm_availability` retained MQTT topic. Per-agent (not
   * global) because the Desktop subscribes to many agents at once and any
   * one of them flashing MISSING during a reconnect race must not put a
   * banner over every other agent's chat.
   */
  llmAvailability: LlmAvailability;
  /**
   * Cold-start tab restore latch (see `adoptRuntimeTabs`): true once this
   * agent's tab strip has been projected from the Runtime's `Active` set in
   * THIS app launch.
   *
   * Lives in the in-memory agent state — deliberately NOT persisted. It is a
   * client-side one-shot guard, not data: the sessions themselves stay
   * Runtime truth, and a relaunch must start unreconciled so the projection
   * runs again. Wiping agent state (logout, agent reset) clears it too, which
   * is exactly the "ask the backend again" condition.
   */
  tabsRestoredFromRuntime: boolean;
}

const DEFAULT_AGENT_STATE: AgentState = {
  sessionStates: {},
  activeSessionId: null,
  openSessionIds: [],
  lastLoadedSessionId: null,
  isSessionInitLoading: false,
  preferredModel: null,
  preferredProvider: null,
  llmAvailability: "unspecified",
  tabsRestoredFromRuntime: false,
};

const MAX_CACHED_SESSIONS = 32;

// ── Single-flight "open a session" ─────────────────────────────────────
// The start orchestrator, the selectAgent resolver, session-tab clicks
// and the session-switch effect can all target the same session within
// the same tick. Without coalescing, the second call re-sends
// `open_session` and restarts `loadSessionMessages`, aborting the first
// load's in-flight fetch. One open per (agentId, sessionId) is shared
// by every caller; the map entry lives only while the open runs.
const inflightOpens = new Map<string, Promise<void>>();

// ── Single-flight "jump to the tail" ───────────────────────────────────
// Render-loop and open/switch paths can request the same session's tail
// reload in the same tick. Sharing the promise (instead of returning
// early on `isLoadingMore`) keeps every caller honest: each one waits
// for the reload it asked about.
const inflightLatest = new Map<string, Promise<void>>();

// ── Single-flight transcript realignment (ADR-050 §16.3) ──────────────
const inflightRealign = new Map<string, Promise<unknown>>();

// ── Single flight: model inventory ─────────────────────────────────────
// `availableModels` / `providerAccounts` are read by BOTH composer
// surfaces (the execution ChatPanel and the Chat-session AgentChatView,
// whose model / reasoning buttons resolve their capability off this list
// — see lib/modelCapabilities.ts). A Gateway reconnect and a `models-added`
// signal can land in the same tick; share one fetch.
//
// Coalescing alone is not enough: the vault snapshot is read at the very
// start of a run, so a `models-added` (a real vault write) landing
// mid-flight — while the slow per-provider capability fetches still run —
// would resolve against a pre-write snapshot and hide the new provider.
// A module-level `models-added` listener counts vault mutations; the load
// loop re-runs whenever a mutation landed after its snapshot was taken.
let inflightModels: Promise<void> | null = null;
let vaultMutationSeq = 0;
if (typeof window !== "undefined") {
  window.addEventListener("models-added", () => {
    vaultMutationSeq += 1;
  });
}

/** Test-only: clear the model single-flight / vault-mutation module state.
 *  A test that asserts mid-flight leaves the lock held; without this the
 *  next test in the same file would collide with the abandoned run. */
export function __resetModelInventoryFlightForTests(): void {
  inflightModels = null;
  vaultMutationSeq = 0;
}

/** Field-wise equality for a model inventory row — every field the
 *  builders below produce is compared. */
function sameModelEntry(a: ModelEntry, b: ModelEntry): boolean {
  return (
    a.name === b.name &&
    a.provider === b.provider &&
    a.tool_call === b.tool_call &&
    a.reasoning === b.reasoning &&
    a.default_reasoning_effort === b.default_reasoning_effort &&
    ((a.input_modalities === undefined && b.input_modalities === undefined) ||
      a.input_modalities?.join(",") === b.input_modalities?.join(","))
  );
}

/** Order-sensitive list equality — the inventory order is deterministic
 *  (vault key order), so a re-read that changed nothing must compare
 *  equal element by element. */
function sameModelEntries(a: ModelEntry[], b: ModelEntry[]): boolean {
  return a.length === b.length && a.every((m, i) => sameModelEntry(m, b[i]));
}

function sameProviderAccounts(
  a: Record<string, ProviderAccount[]>,
  b: Record<string, ProviderAccount[]>,
): boolean {
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  for (const p of ka) {
    const la = a[p];
    const lb = b[p];
    if (!lb || la.length !== lb.length) return false;
    for (let i = 0; i < la.length; i++) {
      if (
        la[i].accountId !== lb[i].accountId ||
        la[i].alias !== lb[i].alias ||
        la[i].preview !== lb[i].preview
      ) {
        return false;
      }
    }
  }
  return true;
}

/**
 * `fetchProviderModels` with a deadline. The underlying fetch has no
 * timeout of its own, and a hanging provider endpoint behind the Gateway
 * would keep `Promise.allSettled` pending forever — with the single-flight
 * lock held, EVERY later refresh (including `models-added` after the user
 * saves a new provider) would degrade to a no-op until a page reload.
 * On timeout the fetch counts as a failure, so the provider keeps its
 * previous capabilities instead of being wiped.
 */
async function fetchProviderModelsWithTimeout(
  provider: string,
  timeoutMs = 10_000,
): Promise<Awaited<ReturnType<typeof fetchProviderModels>>> {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    return await fetchProviderModels(provider, undefined, ctrl.signal);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Re-fetch the tail page when the authoritative record count says our view
 * has diverged from the transcript.
 *
 * WHY THIS EXISTS: the live event path is deliberately role-incomplete —
 * `user` records are never pushed (the sender renders them optimistically,
 * and the Runtime persists them under the frontend-generated id precisely
 * so a re-fetch can dedupe by id; see `append_message_with_id` in
 * core/acowork-runtime/src/agent/loop_.rs).  MQTT delivery is also
 * best-effort, so a `record_complete` can be lost (ADR-035).  The invariant
 * "every persisted record eventually reaches every client" therefore cannot
 * rest on the event path alone — it needs a reconciliation keyed on DATA.
 * The retained `session_state` snapshot already carries the authoritative
 * `message_count`, so divergence is detectable without new plumbing.
 *
 * The rule is deliberately NOT keyed on the agent lifecycle: "a turn ended"
 * is only a proxy for "records appeared", and a proxy silently stops
 * covering sources it was never told about.  Keying on the count covers
 * remote-origin user records, lost events, reconnect gaps, and any future
 * record source with one condition instead of a case per source.
 *
 * Safe by construction against the duplicate-bubble regression this design
 * guards: `loadSessionMessages` merges through `mergeMessageWindow`, which
 * dedupes by message id, so the sender's optimistic entry collapses into
 * the server copy instead of doubling.
 *
 * Not gated on visibility: background caches are maintained live by
 * `record_complete`, and `ensureLatestInCache` short-circuits on the LOCAL
 * `messageTotal` — so a diverged background session would look tail-covered
 * on open and never heal.  Realigning here keeps the local count honest.
 */
function realignOnDivergence(
  agentId: string,
  sessionId: string,
  serverTotal: number,
): void {
  if (!Number.isFinite(serverTotal) || serverTotal <= 0) return;
  const key = `${agentId}:${sessionId}`;
  if (inflightRealign.has(key)) return;
  const ss = useChatStore.getState().agentStates[agentId]?.sessionStates[sessionId];
  if (!ss) return;
  // A load is already in flight for this session (pagination / open /
  // jump-to-tail).  `loadSessionMessages` aborts the previous controller,
  // so firing now would throw away the page that load was fetching.
  if (ss.isLoadingMore || ss.isLoadingSession) return;
  // Same at-tail expression as the `record_complete` handler: a reader
  // parked mid-history is never yanked to the tail — their `scrollToBottom`
  // already loads the newest page.  A stale cache's cursor is untrustworthy.
  const atTail =
    isAtTail(ss.messageOffset, ss.messageLimit, ss.messageTotal) && !ss.messagesStale;
  if (!atTail) return;
  // No divergence.  A transiently HIGHER local total is normal: the
  // optimistic send bumps it before the record is persisted.
  if (serverTotal <= ss.messageTotal) return;

  log.info(
    `[ChatStore] transcript diverged (server=${serverTotal} local=${ss.messageTotal}), ` +
      `realigning tail for ${sessionId}`,
  );
  const promise = useChatStore
    .getState()
    .loadSessionMessages(agentId, sessionId)
    .finally(() => {
      inflightRealign.delete(key);
    });
  inflightRealign.set(key, promise);
}

// ── Spinner ownership ────────────────────────────────────────────────
// `isLoadingSession` is owned by the load that raised it (`loadOwnerSeq`).
// Every discard path of `loadSessionMessages` funnels through this check,
// so a superseded or aborted load — including an `abortSessionLoad` that
// bumps the seq without starting a replacement — can never orphan the
// "Loading conversation..." overlay: when it is the last recorded owner,
// nobody newer exists to clear the flag, so it clears itself.
function clearSpinnerIfOwner(agentId: string, sessionId: string, seq: number): void {
  const ss = useChatStore.getState().agentStates[agentId]?.sessionStates[sessionId];
  if (!ss || !ss.isLoadingSession || ss.loadOwnerSeq !== seq) return;
  useChatStore.setState((state) =>
    updateSessionState(state, agentId, sessionId, { isLoadingSession: false }),
  );
}

// ---------------------------------------------------------------------------
// Helper functions for state access
// ---------------------------------------------------------------------------

function getAgentState(state: ChatStore, agentId: string): AgentState {
  return state.agentStates[agentId] ?? DEFAULT_AGENT_STATE;
}

function getSessionState(state: ChatStore, agentId: string, sessionId: string): SessionChatState {
  const agent = state.agentStates[agentId];
  if (!agent) return DEFAULT_SESSION_STATE;
  return agent.sessionStates[sessionId] ?? DEFAULT_SESSION_STATE;
}

/** Build initial session state, inheriting agent's preferred model (ADR-012). */
function makeInitialSessionState(agent: AgentState): SessionChatState {
  return {
    ...DEFAULT_SESSION_STATE,
    model: agent.preferredModel,
    provider: agent.preferredProvider,
    providerAccountId: null,
  };
}

/** Produce a new agentStates patch that merges `patch` into the agent's current state */
function updateAgentState(
  state: ChatStore,
  agentId: string,
  patch: Partial<AgentState>,
): { agentStates: Record<string, AgentState> } {
  const current = getAgentState(state, agentId);
  return {
    agentStates: {
      ...state.agentStates,
      [agentId]: { ...current, ...patch },
    },
  };
}

/** Produce a new agentStates patch that merges `patch` into a specific session's state */
function updateSessionState(
  state: ChatStore,
  agentId: string,
  sessionId: string,
  patch: Partial<SessionChatState>,
): { agentStates: Record<string, AgentState> } {
  const agent = getAgentState(state, agentId);
  const currentSession = agent.sessionStates[sessionId] ?? DEFAULT_SESSION_STATE;
  return {
    agentStates: {
      ...state.agentStates,
      [agentId]: {
        ...agent,
        sessionStates: {
          ...agent.sessionStates,
          [sessionId]: { ...currentSession, ...patch, lastAccessed: Date.now() },
        },
      },
    },
  };
}

/**
 * ADR-050 C5 reconciliation: replace optimistic entries once the backend
 * persists a send.
 *
 * `sendMessage` inserts the user message + attachment system entries into
 * `messages[]` with `_isOptimistic: true` before the backend has persisted
 * them. The ONLY mechanism that drops `_isOptimistic` is
 * `mergeMessageWindow` id-dedupe during an HTTP refresh — the server copy
 * (same id, no flag) wins. Nothing scheduled that refresh after a send, so
 * a message with attachments kept its "waiting for server confirmation"
 * spinner until the session was reopened.
 *
 * This schedules a short bounded tail-refresh (`loadSessionMessages`). Each
 * landing replaces whatever optimistic entries the server has echoed back;
 * we stop as soon as none remain. The bounded retries cover the backend's
 * asynchronous JSONL writer flush (`Conversation::append_message_with_id`
 * is non-blocking) and the publish → process → persist pipeline.
 */
const SEND_RECONCILE_DELAYS_MS = [300, 800, 1600];

export function scheduleSendReconciliation(agentId: string, sessionId: string, attempt = 0): void {
  if (attempt >= SEND_RECONCILE_DELAYS_MS.length) return;
  globalThis.setTimeout(() => {
    const store = useChatStore.getState();
    const ss = store.getSessionState(agentId, sessionId);
    // Session evicted or every optimistic entry already confirmed — done.
    if (!ss || !ss.messages.some((m) => m._isOptimistic)) return;
    store.loadSessionMessages(agentId, sessionId).then(
      () => {
        const after = useChatStore.getState().getSessionState(agentId, sessionId);
        if (after?.messages.some((m) => m._isOptimistic)) {
          scheduleSendReconciliation(agentId, sessionId, attempt + 1);
        }
      },
      () => scheduleSendReconciliation(agentId, sessionId, attempt + 1),
    );
  }, SEND_RECONCILE_DELAYS_MS[attempt]);
}

/** Evict oldest/unused sessions when cache exceeds MAX_CACHED_SESSIONS.
// NOTE: kept as a reference implementation for future eviction needs;
// currently dead because (a) `openSessionIds` has no cap (tab strip scrolls
// horizontally; `syncActiveTabs` prunes it back to the sessions the Runtime
// still reports Active, and only `openSession` ever adds one),
// and (b) `closeTab` reuses the `sessionStates[id]` slot by flipping
// `isSessionReady = false` rather than deleting it (preserves scroll
// position on re-open).  If a caller ever needs to bound memory growth from
// huge history loads, call this from the appropriate action — its semantics
// have not changed.
*/
export function _evictStaleSessions(
  state: ChatStore,
  agentId: string,
  protectSessionId?: string,
): { agentStates: Record<string, AgentState> } {
  const agent = getAgentState(state, agentId);
  const sessionIds = Object.keys(agent.sessionStates);
  if (sessionIds.length <= MAX_CACHED_SESSIONS) return { agentStates: state.agentStates };

  // Sort by lastAccessed ascending (oldest first)
  const sorted = sessionIds.sort((a, b) =>
    (agent.sessionStates[a]?.lastAccessed ?? 0) - (agent.sessionStates[b]?.lastAccessed ?? 0)
  );

  const toEvict = sorted
    .filter((id) => !agent.openSessionIds.includes(id) && id !== protectSessionId)
    .slice(0, sessionIds.length - MAX_CACHED_SESSIONS);

  if (toEvict.length === 0) return { agentStates: state.agentStates };

  const newSessionStates = { ...agent.sessionStates };
  for (const id of toEvict) {
    delete newSessionStates[id];
    // Release the evicted session's adapter-side live state
    // (activeStream tracker, throttle timestamps, optimisticEntries).
    releaseAdapterSession(agentId, id);
  }

  return {
    agentStates: {
      ...state.agentStates,
      [agentId]: { ...agent, sessionStates: newSessionStates },
    },
  };
}

// ---------------------------------------------------------------------------
// ChatStore — global fields + per-agent agentStates
// ---------------------------------------------------------------------------

interface ChatStore {
  agentStates: Record<string, AgentState>;

  // ---- Global fields (not per-agent) ----
  /**
   * Whether the MQTT connection to the Gateway broker is currently healthy.
   *
   * ADR-036: this field is the *consumer* output of the Rust
   * `mqtt-status` event.  The Rust eventloop is the source-of-truth;
   * the frontend never writes to this flag except for the initial
   * `false` default and the cleanup-on-dispose.
   */
  mqttConnected: boolean;
  /**
   * Most recent disconnect reason reported by the Rust eventloop, or
   * `null` while connected / before the first connection.  Surfaced in
   * the UI status bar so the user can tell *why* the input box went
   * disabled.  See ADR-036.
   */
  lastMqttError: string | null;
  /**
   * UI-facing summary of the raw MQTT connection snapshot, computed
   * from the Rust eventloop's last known state + `lastMqttError`.
   *
   * - `connected`    — Rust is in `Connected` state, frontend in sync.
   * - `connecting`   — Rust is in `Connecting` state (initial handshake).
   * - `reconnecting` — Rust is in `Reconnecting` state (post-disconnect retry).
   * - `stale`        — non-connected state has persisted past
   *                    `STALE_GRACE_MS` without recovery, OR the Rust-vs-frontend
   *                    state is inconsistent. Auto-recovery in progress.
   * - `disconnected` — Rust is in a hard `Disconnected` state with a reason.
   * - `idle`         — Rust client not yet initialized (cold start before
   *                    `connect_mqtt`, or after `disposeMqttListener`).
   *
   * See `docs/plan/zh/desktop-unified-diagnostics.md` §3.1 P0 for
   * the rationale and recovery-time contract.
   */
  effectiveConnection: ConnectionStatus;
  /** Timestamp (ms) when the current connecting episode started (the
   *  shared clock behind `updateConnectingSince`). `null` whenever the
   *  connection is settled (`connected` / `disconnected` / `idle`).
   *  Drives both the watchdog's stuck-detection and the banner's
   *  "X seconds until auto-reconnect" countdown. */
  staleSince: number | null;
  /** Ring buffer of the last `TRANSITION_LOG_CAPACITY` connection state
   *  transitions, for the diagnostics panel's "recent events" UI. */
  transitionLog: TransitionLogEntry[];
  /**
   * Monotonically incremented counter, bumped on every retained
   * `bootstrap-state` snapshot the Rust eventloop forwards from the
   * Gateway's `acowork/global/bootstrap` topic (ADR-059). The Gateway
   * republishes its aggregated `BootstrapState` whenever any subsystem
   * transitions — including the per-node online/offline signals that
   * drive the remote-mode sidebar's green/gray dot. Subscribing to this
   * counter lets the sidebar refetch `/api/nodes` in real time, replacing
   * the previous 30s polling fallback (a kill used to take up to 30s to
   * flip the dot gray, plus a UI focus to "force" it).
   */
  bootstrapVersion: number;
  /**
   * "The Gateway's agent inventory may have changed" counter. Bumped on
   * two events, both of which make the AgentList sidebar's cached
   * `GET /api/agents` result suspect:
   *
   * 1. Every `acowork/desktop/inventory` signal the Gateway emits when
   *    its aggregated `installed_agents` table mutates (a Node finishes
   *    an install/uninstall, a Node replays its retained inventory on
   *    reconnect, or HTTP `DELETE /api/agents/{id}` drops an entry).
   * 2. Every MQTT transition into `connected` — i.e. the initial
   *    subscribe and every reconnect. The signal is a live,
   *    **non-retained** message (see the Gateway's
   *    `mqtt/inventory_notifier.rs` for why), so a change that lands
   *    while the Desktop is disconnected would otherwise be lost. The
   *    connection edge is the catch-up: it also closes the window
   *    between the sidebar's mount fetch and the first subscribe.
   *
   * The sidebar refetches on every bump, so this replaces the previous
   * 30 s `setInterval` polling fallback that left the sidebar stale
   * until a tab-switch remount or the next poll tick.
   */
  inventoryVersion: number;
  availableModels: ModelEntry[];
  /**
   * Accounts (API keys) per provider, for the model picker's account level.
   * Keyed by provider id; a provider absent here (or with one entry) does not
   * get an account level in the menu.
   */
  providerAccounts: Record<string, ProviderAccount[]>;

  // ---- Actions ----
  sendMessage: (content: string, agentId: string, command?: string, attachedItems?: AttachedItem[]) => Promise<void>;
  stopCurrentMessage: (agentId: string) => Promise<void>;
  sendStop: (agentId: string) => void;
  /** Clear session state for a specific agent's active session */
  clearMessages: (agentId: string) => void;
  /** Clear a specific session's state */
  clearSessionState: (agentId: string, sessionId: string) => void;
  /** Clear every cached session's runtime state for an agent (used on
   *  agent stop / Gateway disconnect so the messages / pending approvals /
   *  tool progress / abort controllers release their object refs and can
   *  be GC'd). Selection (`activeSessionId`, `openSessionIds`) is NOT
   *  touched so the next `fetchLatestSession → openSession` can re-hydrate
   *  the user's place. */
  clearAgentSessions: (agentId: string) => void;
  /** Remove a session's cached state (e.g. on session delete) */
  removeSessionState: (agentId: string, sessionId: string) => void;
  trimMessagesTo: (agentId: string, count: number) => void;
  setCurrentModel: (model: string, provider: string, agentId: string, accountId?: string) => void;
  /** ADR-076 §决策 4: switch the session's workspace over authenticated HTTP. */
  setSessionWorkspace: (agentId: string, sessionId: string, workspaceId: string) => void;
  /** Optimistically set a session's live title (manual rename). Writes the
   *  per-session overlay that the open tab / status card read — same home as
   *  model/provider. Empty / whitespace-only titles are ignored: the overlay
   *  shadows the list snapshot for readers, so a blank must never land. */
  setSessionTitleLocal: (agentId: string, sessionId: string, title: string) => void;
  /** Seed a provisional title for a session that has none yet (a Chat-Router
   *  exec session), derived from the triggering user message. No-op once any
   *  title exists — live overlay OR the `fetchSessions` snapshot (both are
   *  backend-authoritative and the Runtime generates a title only once, so a
   *  snapshot title would never be re-pushed to heal a clobber). */
  seedOptimisticTitle: (agentId: string, sessionId: string, userMessage: string) => void;
  /**
   * Load the shared model inventory: the configured providers (Tauri
   * vault) plus each provider's model capabilities (Gateway), into
   * `availableModels` / `providerAccounts`.
   *
   * WHY IT LIVES HERE AND NOT IN A COMPONENT: both composer surfaces read
   * this list (the execution ChatPanel's model / workspace menus and the
   * Chat-session AgentChatView's model / reasoning buttons), but only one
   * of the two is mounted at a time (`AgentPage` renders the Chat surface
   * OR the exec surface per `mode`). Loading it from the exec composer
   * made it a side effect of that composer mounting — on a desktop restart
   * that reconnects to an already-running agent, `alive` is true from the
   * first render, the exec composer never mounts, and the Chat composer's
   * model / reasoning buttons had no inventory to resolve against.
   * `AppLayout` now triggers the load (see the effect there) instead.
   *
   * Single-flight with compensation: concurrent callers share one fetch, but
   * a vault write (`models-added`) landing while a load runs forces one
   * re-read — the running load's snapshot predates the write. Writes that
   * leave the content unchanged reuse the previous array/object identity,
   * per the identity-preserving-store-updates rule.
   */
  loadAvailableModels: () => Promise<void>;
  /** Set per-session reasoning effort override (auto/off/low/medium/high) */
  setReasoningEffort: (effort: string, agentId: string) => void;
  /**
   * ADR-074: set / clear the per-session context window override.
   * `null` (or 0) = clear → session inherits the per-agent chain.
   * Written via HTTP `PUT /sessions/{sid}/config` (the Gateway-proxied
   * `SessionConfigDelta` path) — no new MQTT control command (D3).
   */
  setSessionContextWindow: (window: number | null, agentId: string) => void;
  continueExecution: (agentId: string, sessionId?: string) => Promise<void>;
  resolveApproval: (agentId: string) => void;
  /** Resolve a specific approval by tool_call_id, removing it from the pending map.
   *  `sessionId` targets a specific execution session (the Chat surface drives
   *  a non-foreground exec session); absent falls back to the active session. */
  resolveApprovalByToolCallId: (agentId: string, toolCallId: string, sessionId?: string) => void;
  resolveQuestion: (agentId: string, requestId: string, sessionId?: string) => void;
  /**
   * Load messages for a session via HTTP pagination.
   * - `offset === undefined` (default): initial load — fetches the newest `limit` raw entries.
   * - `offset > 0`: paginated load — fetches `limit` raw entries older than the current window.
   *
   * Both `offset` and `limit` are in **raw-entry units** (one JSONL line
   * each — see `PaginatedMessages` in `lib/types.ts`). Display-group
   * collapsing is handled locally in `ChatPanel.messageBlocks` (the
   * `MessageBlock` strict intermediate layer) and is never transmitted.
   *
   * Direction is **derived** from `sessionState.messageOffset` after the response
   * lands (see `SessionChatState` for the rules); callers don't need to specify it.
   *
   * No sliding window — the messages array grows continuously and is released
   * on closeSession or session switch.
   *
   * Returns the window coordinates returned by the server, or `undefined` if
   * the response was discarded (stale sequence / aborted).
   */
  loadSessionMessages: (
    agentId: string,
    sessionId: string,
    offset?: number,
    limit?: number,
    /** When true, REPLACE the cached window with the fetched page instead of merging. */
    replaceCache?: boolean,
  ) => Promise<{ offset: number; limit: number; total: number } | undefined>;
  /**
   * Open a session and locate a specific message (ADR-081 conversation
   * hit). `messageIndex` is the raw JSONL line number — the same unit
   * the backend paginates by. Aligns the load window so the target is
   * in `messages[]`, then sets `pendingLocate` for ChatPanel to scroll
   * + highlight. Resolves after the session is open (even if locate
   * could not be resolved — the user still lands in the session).
   */
  locateMessage: (agentId: string, sessionId: string, messageIndex: number) => Promise<void>;
  abortSessionLoad: (agentId: string, sessionId: string) => void;
  /**
   * One-shot jump to the latest page (offset=0).
   *
   * Used by all "navigate to the bottom" scenarios:
   *  - Initial session mount / session switch
   *  - Scroll-to-bottom button click
   *  - Initial state where the cache is parked at an older window
   *
   */
  ensureLatestInCache: (agentId: string, sessionId: string) => Promise<void>;
  /**
   * Release the messages array for a session to free memory.
   * Called on closeSession and session switch.  The session state
   * is preserved (offset/limit/total) so the metadata can be used
   * to decide whether to reload from scratch or from a specific page.
   */
  clearSessionMessages: (agentId: string, sessionId: string) => void;
  /**
   * ADR-038: Strict UI-only switch — set which session is in the foreground.
   *
   * Contract: does NOT modify `openSessionIds` and does NOT communicate with
   * the backend. Used when the user toggles between already-open tabs. The
   * caller MUST have already ensured the session is in `openSessionIds`
   * (typically via `openSession`). If the session is not in `openSessionIds`
   * we log a warning and no-op — the caller should be calling `openSession`.
   */
  setActiveTab: (agentId: string, sessionId: string) => void;
  /**
   * Apply session metadata (model/provider/workspace_id) from activate_session response.
   * Sets the session's model/provider and agent's preferredModel, plus syncs workspaceStore.
   */
  applySessionMeta: (agentId: string, sessionId: string, meta: { model?: string | null; provider?: string | null; workspace_id?: string | null }) => void;
  /**
   * Get the active session ID for an agent */
  getActiveSessionId: (agentId: string) => string | null;
  /**
   * ADR-014: Get session state for reading from external stores */
  getSessionState: (agentId: string, sessionId: string) => SessionChatState;
  /**
   * ADR-014: Update session status from backend (Pull repair) */
  updateSessionStatus: (agentId: string, sessionId: string, status: SessionStatus) => void;
  /**
   * ADR-014: Batch update session statuses — single set() call to avoid O(n) re-renders */
  batchUpdateSessionStatuses: (agentId: string, statuses: Map<string, SessionStatus>) => void;
  /**
   * ADR-038: Open a session — full UI + backend activation.
   *
   * Three side effects, all idempotent:
   *  1. UI: prepend to `openSessionIds` and set as `activeSessionId`.
   *  2. Backend: send MQTT `open_session` — Runtime transitions Closed/NotFound → Active
   *     and acks via `session_opened` (or errors via `session_not_opened`).
   *  3. Local cache: HTTP-load conversation history for the session.
   *
   * After this returns, the UI shows the session and the user can interact,
   * but `isSessionReady` flips true only when `session_opened` arrives.
   * The input box should remain disabled until that event lands (or fire a
   * `session_not_opened` toast with a reopen affordance).
   */
  openSession: (agentId: string, sessionId: string, opts?: { prepend?: boolean }) => Promise<void>;
  /**
   * Cold-start restore, step 1: may this agent's tab strip still be restored
   * in this app launch?
   *
   * True while the agent's strip is empty AND nothing has been restored for
   * it yet. Both halves matter: an empty strip is how a fresh process looks
   * (the Desktop keeps no session state of its own), and the one-shot latch
   * keeps a later re-entry — agent stop/start, a switch back — from
   * resurrecting tabs the user closed in the meantime.
   */
  needsTabRestore: (agentId: string) => boolean;
  /**
   * Cold-start restore, step 2: project the Runtime's `Active` session set
   * onto the tab strip (ADR-038).
   *
   * WHY THIS IS A PROJECTION AND NOT A RESTORE FROM LOCAL STATE: the set of
   * sessions a user has open lives in the Runtime — `open_session` adds to it,
   * `close_session` removes from it, and `GET /sessions?active=true` reports
   * it. So after the Desktop restarts, the honest recovery is to ask, not to
   * remember: persisting the tab list client-side would be a second copy of
   * backend truth that drifts (and the Desktop owns no state by convention).
   *
   * `rows` are the `is_active` sessions the caller fetched. Tabs are opened
   * for those the caller may write (`can_write !== false`) — a session
   * resident only because someone else has it open is not ours to hold. Only
   * the newest one goes through the full `openSession` path (backend ack +
   * message load); the rest are added as strip entries, because they are
   * already `Active` server-side and loading N conversations would fetch N
   * histories to paint one tab bar.
   *
   * `rows === null` means the query FAILED (a transient 503 in the Runtime
   * boot window, say). That is not an answer, so it neither restores anything
   * nor latches: a later attempt — the user switching back to the agent — can
   * still restore. An empty array is a real answer ("nothing is active") and
   * does latch.
   *
   * Returns the foreground session id it opened, or null when there was
   * nothing to restore (already restored / failed query / no candidates).
   */
  adoptRuntimeTabs: (agentId: string, rows: SessionInfo[] | null) => Promise<string | null>;
  /** ADR-038: Close a session tab AND notify the backend.
   *
   * Three side effects:
   *  1. UI: remove from `openSessionIds`, activate a neighbor tab.
   *  2. Backend: send MQTT `close_session` so Runtime drops the in-memory
   *     session task. The JSONL + meta stay on disk; reopen is via
   *     `openSession`.
   *
   * Returns the new active sessionId (or null when no tabs remain).
   */
  closeTab: (agentId: string, sessionId: string) => Promise<string | null>;
  /** ADR-015: Get open session IDs for an agent */
  getOpenSessionIds: (agentId: string) => string[];
  /**
   * Sync the Desktop tab strip with the Runtime's Active session set.
   *
   * Called by `fetchSessions` after every list fetch (login, reconnect,
   * manual refresh). Prune-only: a session the Runtime no longer reports
   * Active loses its tab (closed elsewhere, idle-timeout). A tab is NEVER
   * added here — opening one is a user action, which is `openSession`'s job.
   * Adding here would race `closeTab`: it drops the id from `openSessionIds`
   * and repoints `activeSessionId` in one update while the Runtime's
   * `close_session` may still be in flight, so a fetch response listing the
   * session resurrected the tab the user had just closed.
   *
   * `activeSessionId` falls to the first survivor when it was pruned, or null
   * (chat surface takes over).
   *
   * Pure UI operation: no MQTT / HTTP is sent. The Runtime decides what is
   * still open; the strip is the user's subset of that, not a mirror.
   */
  syncActiveTabs: (agentId: string, activeIds: string[]) => void;
  /** ADR-032 C4c: Send a user-initiated compression action (tool results or summary).
   *  `compressType` is the proto `CompressType` enum value:
   *    1 = SUMMARY, 2 = TOOL_RESULTS. */
  sendCompressAction: (agentId: string, sessionId: string, compressType: number) => void;
  /** ADR-083: Cancel the in-flight context compaction (`compress_type = 3`).
   *  The Runtime aborts the distillation LLM call within ~500ms and leaves
   *  history untouched, so the user can switch model and retry. */
  cancelCompressAction: (agentId: string, sessionId: string) => void;
  /** ADR-045: Cancel an in-flight tool execution by tool_call_id.
   *  Publishes a `cancel_tool` control message to the Runtime.
   *  The Runtime maps toolCallId → pending tokio task and aborts it. */
  cancelTool: (agentId: string, sessionId: string, toolCallId: string) => void;
  /** Toggle a file tree directory expansion (per-session) */
  toggleTreeExpandedPath: (agentId: string, sessionId: string, relPath: string) => void;
  /**
   * Ensure all ancestor directories of `relPath` are expanded in the session's
   * file tree (additive merge with existing expansions). Idempotent. No-op for
   * `relPath === ""` or files directly at the workspace root.
   *
   * Example: for `src/components/Foo.tsx` this adds "src" and
   * "src/components" to `treeExpandedPaths` without removing other expansions.
   */
  expandTreeToPath: (agentId: string, sessionId: string, relPath: string) => void;
  /** Add a file/directory/selection to attached chat context */
  addAttachedContext: (agentId: string, sessionId: string, item: { id: string; type: "file" | "directory" | "selection"; name: string; absPath: string; startLine?: number; endLine?: number }) => void;
  /** Remove a file/directory from attached chat context */
  removeAttachedContext: (agentId: string, sessionId: string, id: string) => void;
  /** Clear all attached chat context for a session */
  clearAttachedContext: (agentId: string, sessionId: string) => void;
  /** Add a message to the per-session queue (typed during streaming) */
  addQueuedMessage: (agentId: string, sessionId: string, message: QueuedMessage) => void;
  /** Remove a single attachment entry from the in-memory message list.
   *
   *  Each ADR-046 attachment (file_upload / image_upload / attached_*)
   *  is rendered as a standalone system entry with its own `message.id`.
   *  Calling this with that id drops the entry from `messages[]`.
   *
   *  MVP scope: in-memory only. The corresponding JSONL line is NOT
   *  removed — a session reload will bring the attachment back, and
   *  runtime token accounting already reflects the attached item.
   *  Bidirectional sync (delete from JSONL + tell the runtime) is
   *  deferred to a follow-up; see ADR-046 follow-ups. */
  removeMessageAttachment: (agentId: string, sessionId: string, messageId: string) => void;
  /** Remove a message from the per-session queue by index */
  removeQueuedMessage: (agentId: string, sessionId: string, index: number) => void;
  /** Replace the entire per-session queue (e.g. after sending all) */
  setQueuedMessages: (agentId: string, sessionId: string, messages: QueuedMessage[]) => void;
  /** Dismiss the server-side error banner. */
  clearServerError: (agentId: string, sessionId: string) => void;
  /** ADR-047: Pull session state (status/ratio/todos/context_usage) from backend.
   *  Config fields (model/provider/workspace_id/reasoning_effort/temperature)
   *  are NOT applied here - use `fetchSessionConfig` or `loadSession`. */
  fetchSessionState: (agentId: string, sessionId: string) => Promise<void>;
  /** ADR-047: Pull session config (model/provider/workspace_id/reasoning_effort/
   *  temperature/title) from backend `GET /sessions/{sid}/config`. */
  fetchSessionConfig: (agentId: string, sessionId: string) => Promise<void>;
  /** ADR-047 §3.5.2: Combined cold-load function. Must be used in all
   *  switch/open/first-start scenarios. Encapsulates `Promise.all` of
   *  `fetchSessionState` + `fetchSessionConfig` so callers cannot
   *  accidentally skip one. */
  loadSession: (agentId: string, sessionId: string) => Promise<void>;
}

// ADR-033 / ADR-036: Initialize MQTT event listeners.
//
// We register TWO separate Tauri-event subscriptions:
//   - `agent-event`:  business message stream (status, chunks, sessions, ...)
//                     routed to `handleMessageEvent`.  This was the only
//                     listener before ADR-036 and is unchanged.
//   - `mqtt-status`:  connection liveness (`{connected: bool, reason?: str}`)
//                     emitted by the Rust eventloop on every CONNACK /
//                     DISCONNECT.  This is the *source-of-truth* path for
//                     `chatStore.mqttConnected`; we DO NOT set the flag in
//                     `initMqttListener` itself any more.
//
// Additionally we synchronously query `get_mqtt_status` after the listeners
// are registered.  That command returns the Rust-side last observed
// status, which lets us recover the initial state without racing the
// Tauri event between `connect_mqtt` returning and `listen` resolving.
//
// Both unlisten handles must be cleaned up together in
// `disposeMqttListener`.
let _mqttAgentEventUnlisten: (() => void) | null = null;
let _mqttStatusUnlisten: (() => void) | null = null;
// ADR-059: `bootstrap-state` listener — bumps `bootstrapVersion` on every
// retained snapshot. Held for `disposeMqttListener` cleanup.
let _bootstrapUnlisten: (() => void) | null = null;
// Inventory-change fanout handle — see the `inventory-changed` listener
// below. Paired with `bootstrap-state` for cleanup symmetry.
let _inventoryUnlisten: (() => void) | null = null;
// User-chat wake-up fanout handle — see the `user-chat-changed` listener
// below (design doc 25 §8).
let _userChatUnlisten: (() => void) | null = null;

/// Reentrancy guard for `initMqttListener`.
///
/// React StrictMode (dev) double-invokes `useEffect`, causing
/// `bootGateway` → `initMqttListener` to be called twice concurrently.
/// Without this guard, the second call cancels the first call's
/// listeners before they finish registering, causing `mqtt-status`
/// events to be lost.  The guard ensures the second call awaits the
/// first and returns immediately.
let _mqttInitPromise: Promise<void> | null = null;

/// Interval handle for the background status-polling watchdog.
///
/// P0 of unified-diagnostics: this is no longer a 30-attempt one-shot
/// fallback. It runs forever at `WATCHDOG_INTERVAL_MS` while the
/// connection is not in the `connected` state, and is responsible for:
///
///   1. Detecting Rust-vs-frontend state inconsistency (e.g. Rust reports
///      `connected: true` but the frontend still shows false because the
///      `mqtt-status` event was lost) and self-healing the frontend.
///   2. Detecting a stuck Connecting/Reconnecting state past
///      `STUCK_FORCE_RECONNECT_MS` and auto-triggering
///      `force_reconnect_mqtt` to break the deadlock.
///
/// The watchdog is a safety net, not the primary mechanism. The event
/// listener registered for `mqtt-status` is still authoritative; the
/// watchdog exists to handle the rare cases where the event is lost
/// (wake-recovery webview reload, listener registration race).
let _mqttPollHandle: ReturnType<typeof setInterval> | null = null;
/// Shared clock for the current connecting episode (see
/// `updateConnectingSince`). `null` while the connection is settled.
/// The watchdog resets it after each `force_reconnect_mqtt` so the
/// next episode gets a fresh `STUCK_FORCE_RECONNECT_MS` window.
let _connectingSince: number | null = null;

function stopMqttPoll(): void {
  if (_mqttPollHandle) {
    clearInterval(_mqttPollHandle);
    _mqttPollHandle = null;
  }
}

/**
 * Apply a status transition to the store. Centralizes the invariant:
 * any time `effectiveConnection` flips, the `transitionLog` is
 * appended to and the shared connecting-episode clock is advanced.
 * `mqttConnected` (the legacy boolean) is kept in sync for
 * backwards-compatible consumers.
 *
 * `fromWatchdog` is true when the call originates from the 5s polling
 * loop — the watchdog uses it to distinguish a Rust-driven transition
 * from a frontend-driven self-heal in the transition log.
 *
 * Stale-upgrade rule (plan §3.1.1 / D-5): if the freshly-computed
 * nextEffective is still "connecting" / "reconnecting" after
 * `STUCK_FORCE_RECONNECT_MS` on the connecting-episode clock, the
 * verdict is promoted to "stale" — the UI surfaces "we've been
 * waiting too long" and the watchdog fires `force_reconnect_mqtt` in
 * the same pass.
 */
function applyConnectionTransition(
  raw: MqttStatusSnapshot,
  options: { fromWatchdog?: boolean } = {},
): void {
  const state = useChatStore.getState();
  const prevEffective = state.effectiveConnection;
  const now = Date.now();
  let nextEffective = computeEffectiveConnection(raw);
  if (
    (nextEffective === "connecting" || nextEffective === "reconnecting") &&
    _connectingSince !== null &&
    now - _connectingSince >= STUCK_FORCE_RECONNECT_MS
  ) {
    nextEffective = "stale";
  }
  if (prevEffective === nextEffective) return;

  const nextError = dedupeError(
    state.lastMqttError,
    raw.connected ? null : raw.reason ?? null,
  );
  _connectingSince = updateConnectingSince(_connectingSince, nextEffective, now);
  const nextLog = appendTransition(
    state.transitionLog,
    prevEffective,
    nextEffective,
    raw.reason ?? null,
  );
  log.debug(
    `[mqtt-status] ${prevEffective} → ${nextEffective}` +
      (raw.reason ? ` (reason: ${raw.reason})` : "") +
      (options.fromWatchdog ? " [via watchdog]" : ""),
  );
  useChatStore.setState({
    mqttConnected: raw.connected,
    lastMqttError: nextError,
    effectiveConnection: nextEffective,
    staleSince: _connectingSince,
    transitionLog: nextLog,
    // Inventory catch-up: the inventory-change signal is live-only
    // (non-retained), so anything that happened while the Desktop was
    // not subscribed is invisible to us. Every transition into
    // `connected` means the Rust client just (re-)subscribed — refetch
    // `GET /api/agents` so the AgentList sidebar converges. This also
    // closes the mount-fetch-vs-first-subscribe window.
    ...(nextEffective === "connected"
      ? { inventoryVersion: state.inventoryVersion + 1 }
      : {}),
  });

  // The group / DM chat surface rides the same bridge. On the `down → up`
  // edge the doorbells that fired while we were unsubscribed are gone for
  // good (non-retained), so the sidebar lists + open chat windows re-read
  // once to converge — exactly like the `inventoryVersion` bump above. (This
  // replaces the retired user-chat `setChatPushChannel` catch-up: the unified
  // chat surface has no fallback poll loop, so the connect edge is the only
  // proactive trigger and the doorbell carries everything after it.)
  if (nextEffective === "connected") {
    void useProjectGroupStore.getState().fetchGroups();
    wakeAgentChat();
  }
}

/**
 * Apply a transition derived from a Rust snapshot polled via
 * `get_mqtt_status` (the watchdog path). Same effect as
 * `applyConnectionTransition` but tagged `fromWatchdog: true` so the
 * transition log surfaces "who" caused the change.
 *
 * F-3 fix: returns `recovered` when Rust reports `connected: true`
 * while the frontend still thinks otherwise — the caller stops the
 * watchdog on that signal.
 *
 * The snapshot is passed through VERBATIM (`{ ...snapshot }`): the
 * `connecting` / `reconnecting` transient flags must survive, because
 * collapsing them made a genuinely-connecting client render as
 * `disconnected` (frontend downgrade — review finding B-2).
 */
function applyWatchdogSnapshot(snapshot: MqttStatusSnapshot): { recovered: boolean } {
  const state = useChatStore.getState();
  const recovered =
    snapshot.connected === true && state.mqttConnected !== true;
  applyConnectionTransition({ ...snapshot }, { fromWatchdog: true });
  return { recovered };
}

function startMqttPoll(): void {
  stopMqttPoll();
  log.warn("[mqtt-watchdog] starting 5s polling watchdog");
  _mqttPollHandle = setInterval(async () => {
    // Stop the watchdog as soon as we're back to `connected` — it's a
    // safety net, not a permanent feature.
    const cur = useChatStore.getState();
    if (cur.effectiveConnection === "connected") {
      log.warn("[mqtt-watchdog] effectiveConnection=connected; stopping watchdog");
      stopMqttPoll();
      return;
    }
    log.warn(
      `[mqtt-watchdog] tick: effectiveConnection=${cur.effectiveConnection}, \
       mqttConnected=${cur.mqttConnected}, will query get_mqtt_status`,
    );

    // 1. Poll the Rust snapshot and apply it. This self-heals a
    //    Rust-vs-frontend inconsistency (F-3) and may promote a stuck
    //    `connecting` / `reconnecting` to `stale` (plan §3.1.1).
    try {
      const snap = await invoke<MqttStatusSnapshot>("get_mqtt_status");
      log.warn(
        "[mqtt-watchdog] get_mqtt_status returned:",
        JSON.stringify(snap),
      );
      if (snap.known) {
        const { recovered } = applyWatchdogSnapshot(snap);
        if (recovered) {
          log.warn(
            "[mqtt-watchdog] recovered Rust-vs-frontend inconsistency; stopping watchdog",
          );
          stopMqttPoll();
          return;
        }
      }
    } catch (err) {
      log.warn("[mqtt-watchdog] get_mqtt_status threw:", err);
      // Transient IPC failure - keep polling.
    }

    // 2. D-5: a `stale` verdict past the force-reconnect deadline gets
    //    a hard `force_reconnect_mqtt`. The stale-upgrade above runs on
    //    the SAME connecting-episode clock, so this branch is reached
    //    with no threshold race (`stale` also matches if a previous
    //    pass already promoted it). The clock is then reset so the next
    //    episode gets a fresh 30s window — periodic retry, not a
    //    single shot.
    const after = useChatStore.getState();
    if (
      after.effectiveConnection === "stale" &&
      _connectingSince !== null &&
      Date.now() - _connectingSince >= STUCK_FORCE_RECONNECT_MS
    ) {
      log.warn(
        "[mqtt-watchdog] connecting episode stuck for " +
          `${Math.round((Date.now() - _connectingSince) / 1000)}s; ` +
          "auto force_reconnect_mqtt",
      );
      try {
        await invoke("force_reconnect_mqtt");
      } catch (err) {
        log.warn("[mqtt-watchdog] force_reconnect_mqtt failed:", err);
      }
      // Reset the episode clock (and its store mirror) so the banner
      // countdown restarts and we don't double-fire within this window.
      _connectingSince = Date.now();
      useChatStore.setState({ staleSince: _connectingSince });
    }
  }, WATCHDOG_INTERVAL_MS);
}

export async function initMqttListener(): Promise<void> {
  // Reentrancy guard: if a previous init is still in flight (React
  // StrictMode double-call), await it and return instead of racing.
  if (_mqttInitPromise) {
    await _mqttInitPromise;
    return;
  }
  _mqttInitPromise = doInitMqttListener();
  try {
    await _mqttInitPromise;
  } finally {
    _mqttInitPromise = null;
  }
}

async function doInitMqttListener(): Promise<void> {
  // Unregister previous listeners and stop any ongoing poll.
  stopMqttPoll();
  if (_mqttAgentEventUnlisten) {
    _mqttAgentEventUnlisten();
    _mqttAgentEventUnlisten = null;
  }
  if (_mqttStatusUnlisten) {
    _mqttStatusUnlisten();
    _mqttStatusUnlisten = null;
  }

  _mqttAgentEventUnlisten = await listen("agent-event", (event) => {
    const data = event.payload as Record<string, unknown>;
    // ADR-073: every `agent-event` carries the INSTANCE identity in
    // `instance_id` — the store addressing key. Package `agent_id` is a
    // category attribute with display value only and is never used here.
    const agentId = data.instance_id as string;
    if (!agentId) {
      // Events without an instance_id are ignored at the store level
      return;
    }

    const store = useChatStore;
    handleMessageEvent(data, store.setState, store.getState, agentId);
  });

  // ADR-036: connection liveness is owned by the Rust eventloop.  We
  // consume `mqtt-status` events for real-time updates.  The payload
  // may include `connecting: true` or `reconnecting: true` for
  // transient states that should NOT trigger the disconnected banner.
  //
  // P0 of unified-diagnostics: every event now flows through
  // `applyConnectionTransition` so the transition log + effective
  // status stay in sync. When we transition to `connected`, the
  // watchdog (if running) stops itself on the next tick.
  _mqttStatusUnlisten = await listen<{
    connected: boolean;
    reason?: string;
    connecting?: boolean;
    reconnecting?: boolean;
  }>("mqtt-status", (event) => {
    const { connected, reason, connecting, reconnecting } = event.payload;
    log.warn(
      "[mqtt-status] received from Rust:",
      JSON.stringify({ connected, connecting, reconnecting, reason }),
    );
    applyConnectionTransition({
      known: true,
      connected,
      connecting: connecting || undefined,
      reconnecting: reconnecting || undefined,
      reason: reason ?? null,
    });
    if (connected) {
      stopMqttPoll();
    } else if (_mqttPollHandle === null) {
      // H-1 fix: a non-connected status arriving after a connected
      // episode means the session degraded at runtime — re-arm the
      // watchdog so a FOLLOW-UP lost event still gets self-healed.
      // Guarded so re-delivered transient events can't restart the
      // 5s tick phase mid-episode.
      startMqttPoll();
    }
  });

  // ADR-059: bootstrap snapshot fanout. The Gateway republishes its
  // aggregated `BootstrapState` on every subsystem transition (including
  // per-node online/offline signals). Bumping a version counter here is
  // the real-time trigger for the remote-mode sidebar to refetch
  // `/api/nodes` — a kill turns the dot gray within ~1s, a reboot
  // turns it green again within ~1s, no `setInterval` fallback needed.
  _bootstrapUnlisten = await listen("bootstrap-state", () => {
    useChatStore.setState((s) => ({ bootstrapVersion: s.bootstrapVersion + 1 }));
  });

  // Inventory-change fanout. The Gateway publishes a non-retained
  // `acowork/desktop/inventory` message whenever its
  // aggregated `installed_agents` table mutates (remote Node finishes
  // install / uninstall, Node replays its retained inventory on
  // reconnect, HTTP uninstall drops an entry). The Tauri
  // `inventory-changed` event is emitted by the Rust eventloop on each
  // delivery; bumping `inventoryVersion` here is the realtime trigger
  // for the AgentList sidebar to refetch `GET /api/agents`.
  //
  // The signal is live-only, so it cannot cover a change that happened
  // while the Desktop was disconnected. That catch-up lives in
  // `applyConnectionTransition` below, which bumps the same counter on
  // every transition into `connected`.
  _inventoryUnlisten = await listen("inventory-changed", () => {
    useChatStore.setState((s) => ({ inventoryVersion: s.inventoryVersion + 1 }));
  });

  // User-chat wake-up fanout (design doc 25 §8). `acowork-user` publishes an
  // empty doorbell on `acowork/users/{user_id}/chat/changed` after every send
  // and every read; the Rust client subscribes the `+` filter (the account can
  // change without a reconnect, so the subscription table stays static) and
  // forwards it with the account segment taken from the topic. The "is this
  // mine" test therefore belongs here, next to the only copy of the signed-in
  // id — a signal for another account must not start a re-read of this inbox.
  _userChatUnlisten = await listen<{ user_id: string }>(
    "user-chat-changed",
    (event) => {
      const me = useAuthStore.getState().account?.user_id;
      if (!me || event.payload.user_id !== me) return;
      // Design 27 Wave 5: the same doorbell lands on agent Chat sessions —
      // the user service publishes it after every append, human or agent.
      wakeAgentChat();
      // ...and on the sidebar's group + DM list (a new message bumps the
      // unread badge / preview, a new project's group appears without
      // reopening the app).
      void useProjectGroupStore.getState().fetchGroups();
    },
  );

  // Pull the *current* status from the Rust side so we don't miss the
  // initial state.  The source of truth is `DesktopMqttClient::session_state`
  // (a watch channel updated synchronously by the poll task).
  //
  // If the snapshot shows `connected: false` (client is in `Connecting`
  // or `Reconnecting` state), start the polling fallback to catch the
  // eventual `Connected` transition - the `mqtt-status` event for that
  // transition may have been emitted before this listener registered.
    try {
      const snapshot = await invoke<MqttStatusSnapshot>("get_mqtt_status");
      log.debug(
        "[initMqttListener] initial snapshot from Rust:",
        JSON.stringify(snapshot),
      );
      if (snapshot.known) {
        // Pass the snapshot through verbatim so the transient
        // `connecting` / `reconnecting` flags survive the init path
        // too (B-2: collapsing them rendered a connecting client as
        // `disconnected`).
        applyConnectionTransition({ ...snapshot, known: true });
      }
      // Start the watchdog whenever we are not yet connected.
      // It is harmless when the event stream is working and is a
      // lifeline when the initial event was lost during webview reload.
      if (!snapshot.connected) {
        log.warn(
          "[initMqttListener] initial snapshot not connected — starting watchdog",
        );
        startMqttPoll();
      }
    } catch (err) {
      // Tauri command not registered (older binary) or other transient
      // failure - fall back to the event stream alone.
      log.warn("get_mqtt_status failed; relying on mqtt-status events:", err);
    }
}

export function disposeMqttListener(): void {
  stopMqttPoll();
  // The death classifier watch polls /health every 3s while MQTT is
  // down — tear it down with the rest of the liveness machinery.
  stopGatewayDeathWatch();
  _connectingSince = null;
  _mqttInitPromise = null;
  if (_mqttAgentEventUnlisten) {
    _mqttAgentEventUnlisten();
    _mqttAgentEventUnlisten = null;
  }
  if (_mqttStatusUnlisten) {
    _mqttStatusUnlisten();
    _mqttStatusUnlisten = null;
  }
  if (_bootstrapUnlisten) {
    _bootstrapUnlisten();
    _bootstrapUnlisten = null;
  }
  if (_inventoryUnlisten) {
    _inventoryUnlisten();
    _inventoryUnlisten = null;
  }
  if (_userChatUnlisten) {
    _userChatUnlisten();
    _userChatUnlisten = null;
  }
  useChatStore.setState({
    mqttConnected: false,
    lastMqttError: null,
    effectiveConnection: "idle",
    staleSince: null,
  });
}

export const useChatStore = create<ChatStore>((set, get) => ({
  agentStates: {},
  mqttConnected: false,
  lastMqttError: null,
  effectiveConnection: "idle",
  staleSince: null,
  transitionLog: [],
  bootstrapVersion: 0,
  inventoryVersion: 0,
  availableModels: [],
  providerAccounts: {},

  getActiveSessionId: (agentId: string) => {
    return getAgentState(get(), agentId).activeSessionId;
  },

  // ADR-014: Get session state for reading from external stores
  getSessionState: (agentId: string, sessionId: string): SessionChatState => {
    return getSessionState(get(), agentId, sessionId);
  },

  // ADR-014: Update session status from backend (Pull repair)
  // Also creates SessionChatState entry if not cached (e.g. crash restart)
  updateSessionStatus: (agentId: string, sessionId: string, status: SessionStatus) => {
    set((state) => {
      const agent = getAgentState(state, agentId);
      const session = agent.sessionStates[sessionId];
      if (!session) {
        // Crash restart: create entry with backend status
        const updatedSessions = { ...agent.sessionStates, [sessionId]: { ...makeInitialSessionState(agent), sessionStatus: status, lastAccessed: Date.now() } };
        const updatedAgent = { ...agent, sessionStates: updatedSessions };
        return { agentStates: { ...state.agentStates, [agentId]: updatedAgent } };
      }
      return updateSessionState(state, agentId, sessionId, { sessionStatus: status });
    });
  },

  // ADR-014: Batch update — single set() call, O(1) re-render regardless of session count
  // Also creates SessionChatState entries for sessions not yet cached (e.g. crash restart)
  batchUpdateSessionStatuses: (agentId: string, statuses: Map<string, SessionStatus>) => {
    if (statuses.size === 0) return;
    set((state) => {
      const agent = getAgentState(state, agentId);
      const updatedSessions = { ...agent.sessionStates };
      for (const [sessionId, status] of statuses) {
        const session = updatedSessions[sessionId];
        if (session) {
          updatedSessions[sessionId] = { ...session, sessionStatus: status, lastAccessed: Date.now() };
        } else {
          // Crash restart: session not cached yet — create entry with backend status
          updatedSessions[sessionId] = {
            ...makeInitialSessionState(agent),
            sessionStatus: status,
                lastAccessed: Date.now(),
          };
        }
      }
      const updatedAgent = { ...agent, sessionStates: updatedSessions };
      return { agentStates: { ...state.agentStates, [agentId]: updatedAgent } };
    });
  },

  // ADR-038: Close a session tab AND notify the backend.
  //
  // Three side effects (all best-effort — MQTT publish errors are logged
  // but do not block UI cleanup, which is the source of truth for the
  // frontend tab strip):
  //  1. UI: remove from `openSessionIds`, activate a neighbor tab.
  //  2. Memory: clear the session's messages array (free memory).
  //  3. Backend: send MQTT `close_session` so Runtime drops the in-memory
  //     session task. The JSONL + meta stay on disk; reopen is via
  //     `openSession`.
  //  4. Reset `isSessionReady` on the evicted session so a later switch
  //     back must wait for a fresh `session_opened` ack.
  closeTab: async (agentId: string, sessionId: string): Promise<string | null> => {
    let newActiveId: string | null = null;
    set((state) => {
      const agent = getAgentState(state, agentId);
      const idx = agent.openSessionIds.indexOf(sessionId);
      if (idx === -1) return {}; // Not open — nothing to do

      const newOpenIds = agent.openSessionIds.filter((id) => id !== sessionId);

      // If closing the active tab, activate neighbor
      if (agent.activeSessionId === sessionId) {
        // Prefer right neighbor, then left
        const neighborIdx = Math.min(idx, newOpenIds.length - 1);
        newActiveId = newOpenIds[neighborIdx] ?? null;
      } else {
        newActiveId = agent.activeSessionId;
      }

      // ADR-038: drop readiness for the evicted session so a later switch
      // must wait for a fresh `session_opened` ack from the Runtime.
      const newSessionStates = { ...agent.sessionStates };
      const evictedState = newSessionStates[sessionId];
      if (evictedState) {
        newSessionStates[sessionId] = { ...evictedState, isSessionReady: false };
      }

      return updateAgentState(state, agentId, {
        openSessionIds: newOpenIds,
        activeSessionId: newActiveId,
        sessionStates: newSessionStates,
      });
    });

    // 2. Memory: clear the session's messages array to free memory.
    //    The session state metadata (offset/limit/total) is preserved
    //    so the next load can resume from the correct page.
    get().clearSessionMessages(agentId, sessionId);

    // 2. Backend: tell Runtime to release the session task.  `close_session`
    // is idempotent on the Runtime side (Closed → closed no-op), so it is
    // safe to fire even if the backend has already dropped the session.
    //
    // ADR-076 §决策 4: skip the call when this session is only shared *with*
    // us. Closing is write-gated (a bystander must not tear down the owner's
    // session), so the request would 404 — closing the tab is a purely local
    // act for a viewer.
    const canWrite = useAgentStore
      .getState()
      .agents[agentId]?.sessions.find((s) => s.session_id === sessionId)?.can_write;
    if (canWrite !== false) {
      try {
        await sessionControl.closeSession(agentId, sessionId);
      } catch (err) {
        log.warn("[chatStore] close_session HTTP failed (UI already cleaned up):", err);
      }
    }

    return newActiveId;
  },

  // ADR-015: Get open session IDs for reading
  getOpenSessionIds: (agentId: string): string[] => {
    return getAgentState(get(), agentId).openSessionIds;
  },

  syncActiveTabs: (agentId: string, activeIds: string[]) => {
    set((state) => {
      const agent = getAgentState(state, agentId);
      // Prune-only: remove tabs for sessions the Runtime no longer reports
      // as Active (closed by another user, idle-timeout, etc.). NEVER add
      // tabs here — that is `openSession`'s job. Adding here would race
      // with closeTab: the fetchSessions response may arrive before the
      // Runtime processes close_session, resurrecting a tab the user just
      // closed. The one place that ADDS in bulk is `adoptRuntimeTabs`, and
      // it is gated on an empty strip + a once-per-launch latch, so it can
      // only ever run at cold start.
      // The foreground tab survives regardless: a session opened from
      // history (or drilled into from a Chat reply) is legitimately not in
      // the Runtime's live set — a read-only one is never activated at all,
      // because `openSession` skips the backend `open_session` it may not
      // call. Pruning it dropped `activeSessionId` to null and parked the
      // panel on "Loading session…" forever (regression t-9764c5fa).
      const newOpenIds = agent.openSessionIds.filter(
        (id) => activeIds.includes(id) || id === agent.activeSessionId,
      );
      if (newOpenIds.length === agent.openSessionIds.length) return {}; // no change
      const patches: Partial<AgentState> = { openSessionIds: newOpenIds };
      // If the foreground tab was pruned, fall to the first survivor or null.
      if (agent.activeSessionId && !newOpenIds.includes(agent.activeSessionId)) {
        patches.activeSessionId = newOpenIds[0] ?? null;
      }
      return updateAgentState(state, agentId, patches);
    });
  },

  /** ADR-032 C4c: Send a user-initiated compression action to the Runtime.
   *  ADR-076 §决策 4: over the Gateway's authenticated HTTP API now, so the
   *  Runtime can check that the caller owns the session. */
  sendCompressAction: (agentId: string, sessionId: string, compressType: number) => {
    sessionControl
      .compressSession(agentId, sessionId, compressType)
      .catch((err: unknown) => log.warn("[ChatStore] compress action failed:", err));
  },

  /** ADR-083: Ask the Runtime to cancel the in-flight context compaction.
   *  Same endpoint as compression, with `compress_type = 3`
   *  (CompressType::CANCEL) — the Runtime fires the running compaction's
   *  cancel handle immediately and publishes `compaction_cancelled`.
   *
   *  ADR-076 §决策 4: over the Gateway's authenticated HTTP API, like every
   *  other session action. Publishing straight to the broker (as this first
   *  did) would skip the owner check the HTTP path performs. */
  cancelCompressAction: (agentId: string, sessionId: string) => {
    sessionControl
      .compressSession(agentId, sessionId, sessionControl.COMPRESS_CANCEL)
      .catch((err: unknown) => log.warn("[ChatStore] cancel compaction failed:", err));
  },

  /**
   * ADR-045: Cancel a single in-flight tool execution by tool_call_id.
   *
   * The Runtime owns the tool-dispatch task map; the UI only emits the intent.
   * The Runtime looks up the task by toolCallId and aborts it; the resulting
   * tool_result is published as normal (with `cancelled: true` flag if the
   * backend wants to surface it). The UI does NOT optimistically remove the
   * tool_progress entry — we wait for the matching tool_result so the chip
   * transitions through the normal lifecycle (running → cancelled → done).
   *
   * ADR-076 §决策 4: HTTP now (see `session-control.ts`).
   */
  cancelTool: (agentId: string, sessionId: string, toolCallId: string) => {
    sessionControl
      .cancelTool(agentId, sessionId, toolCallId)
      .catch((err: unknown) => log.warn("[ChatStore] cancel_tool failed:", err));
  },

  /**
   * ADR-038: Strict UI-only switch — set which session is in the foreground.
   *
   * Contract: does NOT modify `openSessionIds` and does NOT communicate with
   * the backend. Used when the user toggles between already-open tabs.
   *
   * Behaviour:
   *  - If the session is not in `openSessionIds`, we warn and no-op:
   *    the caller should be using `openSession` for first-open semantics.
   *  - If the session is already `activeSessionId`, no-op (idempotent).
   *  - Otherwise: flip `activeSessionId` to the target session.
   *
   * We do NOT touch per-session state here — `openSession` owns session-state
   * creation, and `session_created` / `session_opened` events refresh fields
   * like `model` / `provider`. `setActiveTab` is purely "which tab is shown".
   */
  setActiveTab: (agentId: string, sessionId: string) => {
    set((state) => {
      const agent = getAgentState(state, agentId);
      if (!agent.openSessionIds.includes(sessionId)) {
        log.warn(
          `[chatStore] setActiveTab: ${sessionId} not in openSessionIds — ` +
          `call openSession(agentId, sessionId) first`,
        );
        return {};
      }
      if (agent.activeSessionId === sessionId) return {};
      return updateAgentState(state, agentId, { activeSessionId: sessionId });
    });
  },

  /**
   * ADR-038: Open a session with full UI + backend activation.
   *
   * Three side effects (idempotent, best-effort):
   *  1. UI: prepend to `openSessionIds` if not already present,
   *     set as `activeSessionId`, ensure `sessionStates[sessionId]` exists.
   *  2. Backend: send MQTT `open_session`. Runtime will ack via `session_opened`
   *     (flipping `isSessionReady`) or `session_not_opened` (triggering toast).
   *  3. Local cache: HTTP-load conversation history for the session.
   *
   * The MQTT send is fire-and-forget: errors are logged but don't reject the
   * UI transition. If the user just stopped / restarted the agent, MQTT may
   * not be connected yet — the publish fails, but the UI is already correct.
   * The Runtime will publish `session_opened` once the connection is healthy.
   *
   * Single-flight: concurrent opens of the same session share one promise
   * (see `inflightOpens`).
   */
  openSession: async (agentId: string, sessionId: string, opts?: { prepend?: boolean }) => {
    // Single-flight: a second caller targeting the same session while it
    // is already opening shares that promise instead of re-sending
    // `open_session` and restarting the message load (which would abort
    // the first load's in-flight fetch).
    const key = `${agentId}:${sessionId}`;
    const existing = inflightOpens.get(key);
    if (existing) return existing;
    const promise = (async () => {
      // ponytail: diagnostic — anchor for when "Loading session..." clears.
      console.warn(
        `[chatStore] openSession start ${agentId} ${sessionId} @${performance.now().toFixed(0)}ms`,
      );
      // 1. UI: open the tab + activate + ensure session cache slot.
      set((state) => {
        const agent = getAgentState(state, agentId);
        const patches: Partial<AgentState> = { activeSessionId: sessionId };

        if (!agent.openSessionIds.includes(sessionId)) {
          // New sessions (just created) prepend to sit next to the pinned
          // chat tab; opening an existing session (history, drill) appends
          // so the tab order stays stable.
          patches.openSessionIds = opts?.prepend
            ? [sessionId, ...agent.openSessionIds]
            : [...agent.openSessionIds, sessionId];
        }

        // Lazy-create the session state entry so downstream consumers
        // (loadSessionMessages / fetchSessionState / etc.) don't have to
        // handle a missing entry.
        const newSessionStates = { ...agent.sessionStates };
        if (!newSessionStates[sessionId]) {
          newSessionStates[sessionId] = {
            ...makeInitialSessionState({ ...agent, ...patches }),
            lastAccessed: Date.now(),
          };
        } else {
          newSessionStates[sessionId] = {
            ...newSessionStates[sessionId],
            lastAccessed: Date.now(),
          };
        }
        patches.sessionStates = newSessionStates;

        return updateAgentState(state, agentId, patches);
      });

      // 2. Backend: tell Runtime to transition Closed → Active (or Active
      // no-op). ADR-076 §决策 4: over HTTP so the Gateway can authenticate
      // the caller. Best-effort — the Runtime treats a re-open as a no-op.
      //
      // A session shared *with* us (`can_write === false`) is **not**
      // activated, and the request is not even sent. `Active` / `Closed` is
      // per-session global state, so a viewer-activated session would be a
      // lifecycle change nobody can undo: the viewer may not close it (that
      // is write-gated on purpose) and the owner cannot see that anyone is
      // holding it. Read-only viewing does not need activation anyway —
      // history comes from `GET /messages` and events arrive via the
      // wildcard MQTT subscription whenever the session is genuinely Active
      // (i.e. while its owner has it open).
      const canWrite = useAgentStore
        .getState()
        .agents[agentId]?.sessions.find((s) => s.session_id === sessionId)?.can_write;
      if (canWrite !== false) {
        try {
          await sessionControl.openSession(agentId, sessionId);
        } catch (err) {
          log.warn("[chatStore] open_session failed:", err);
        }
      }

      // 3. Local cache: load conversation history + session config/state.
      // ADR-047 §3.5.2: openSession MUST fetch config + state alongside
      // messages so every caller gets a complete session snapshot without
      // relying on React useEffect to trigger loadSession indirectly.
      // Failures are non-fatal; the user can retry or wait for MQTT patches.
      try {
        await get().loadSessionMessages(agentId, sessionId);
      } catch (err) {
        log.warn("[chatStore] loadSessionMessages after openSession failed:", err);
      }
      try {
        await get().loadSession(agentId, sessionId);
      } catch (err) {
        log.warn("[chatStore] loadSession after openSession failed:", err);
      }
    })().finally(() => {
      inflightOpens.delete(key);
    });
    inflightOpens.set(key, promise);
    return promise;
  },

  needsTabRestore: (agentId: string) => {
    const agent = getAgentState(get(), agentId);
    return !agent.tabsRestoredFromRuntime && agent.openSessionIds.length === 0;
  },

  adoptRuntimeTabs: async (agentId: string, rows: SessionInfo[] | null) => {
    // A failed query is not an answer: leave the latch clear so a later
    // attempt can still restore once the Runtime is reachable.
    if (rows === null) return null;
    if (!get().needsTabRestore(agentId)) return null;
    // Latch before awaiting so a concurrent caller cannot adopt the same strip
    // twice; the adopt is one-shot per launch per agent by definition.
    set((state) => updateAgentState(state, agentId, { tabsRestoredFromRuntime: true }));

    // A session resident only because ANOTHER account holds it open is not
    // ours to show (`can_write === false`; ADR-076 §决策 4). `undefined`
    // (older Runtime) degrades to "ours" — same rule as the rest of the UI.
    const candidates = rows
      .filter((r) => r.is_active !== false && r.can_write !== false)
      .sort(
        (a, b) =>
          new Date(b.last_active_at ?? b.created_at).getTime() -
          new Date(a.last_active_at ?? a.created_at).getTime(),
      )
      .map((r) => r.session_id);
    if (candidates.length === 0) return null;

    const [foreground, ...rest] = candidates;
    log.info("[chatStore] restoring tabs from Runtime Active set", {
      agentId,
      foreground,
      tabs: candidates,
    });

    // Strip entries for the background ones: they are already Active in the
    // Runtime, so no `open_session` and no history load — just the tab. The
    // foreground goes through the full path below, which also makes the
    // activation idempotent on the server (`already_active`).
    //
    // NOTE: titles are deliberately NOT seeded here. A restored background tab
    // has no `sessionStates[id]` yet, so `SessionTabBar` falls back to the
    // `sessions[]` snapshot (its second tier) and shows "Untitled" if the row
    // is outside the fetched page. Copying the row's title into the live
    // overlay tier would fix that label, but the overlay outranks the snapshot
    // (`seedOptimisticTitle` relies on the opposite invariant), so a later
    // backend rename would be shadowed by a stale client copy. Titles stay the
    // snapshot's job — this method owns tabs only.

    // Appended oldest-first: the candidates arrive newest-first (that is how
    // the foreground was picked), and a tab strip reads chronologically left
    // to right.
    if (rest.length > 0) {
      const chronological = [...rest].reverse();
      set((state) => {
        const agent = getAgentState(state, agentId);
        const missing = chronological.filter((id) => !agent.openSessionIds.includes(id));
        if (missing.length === 0) return {};
        return updateAgentState(state, agentId, {
          openSessionIds: [...agent.openSessionIds, ...missing],
        });
      });
    }

    await get().openSession(agentId, foreground);
    return foreground;
  },

  /** Apply session metadata (model/provider/workspace_id) from activate_session response.
   *  Sets the session's model/provider and agent's preferredModel, plus syncs workspaceStore. */
  applySessionMeta: (
    agentId: string,
    sessionId: string,
    meta: { model?: string | null; provider?: string | null; workspace_id?: string | null },
  ) => {
    set((state) => {
      const sessionPatch: Partial<SessionChatState> = {};
      const agentPatch: Partial<AgentState> = {};
      if (typeof meta.model === "string" && meta.model) {
        sessionPatch.model = meta.model;
        agentPatch.preferredModel = meta.model;
      }
      if (typeof meta.provider === "string" && meta.provider) {
        sessionPatch.provider = meta.provider;
        agentPatch.preferredProvider = meta.provider;
      }
      if (Object.keys(sessionPatch).length === 0 && Object.keys(agentPatch).length === 0) return state;

      // Apply session and agent patches sequentially, carrying state forward
      let result = state;
      if (Object.keys(sessionPatch).length > 0) {
        const p = updateSessionState(result, agentId, sessionId, sessionPatch);
        result = { ...result, agentStates: p.agentStates };
      }
      if (Object.keys(agentPatch).length > 0) {
        const p = updateAgentState(result, agentId, agentPatch);
        result = { ...result, agentStates: p.agentStates };
      }
      return result;
    });
    // Sync workspace selection to workspaceStore
    if (typeof meta.workspace_id === "string" && meta.workspace_id) {
      useWorkspaceStore.getState().setSessionWorkspaceLocal(sessionId, meta.workspace_id as string);
    }
  },

  clearMessages: (agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;
    // ADR-050 C2: optimisticEntries + activeStream + throttle timestamps
    // live in chatAdapterStore; release them here too.
    clearAllOptimisticEntries(agentId, sessionId);
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        messages: [],
        tokenUsage: null,
        contextUsage: null,
        messageOffset: 0,
        messageLimit: 0,
        messageTotal: 0,
        messagesStale: true,
        // The window is gone: the next load adopts the server's cursor.
        windowAuthoritative: false,
        pendingApproval: {},
        loadError: null,
        hasMoreIncremental: false,
        abortController: null,
        loadSequence: 0,
        loadOwnerSeq: 0,
        serverError: null,
      }),
    }));
  },

  clearSessionState: (agentId: string, sessionId: string) => {
    clearAllOptimisticEntries(agentId, sessionId);
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        // Fully reset to DEFAULT_SESSION_STATE. Half-cleaned fields
        // (messages: [] but attachedContext: [...], toolProgress: {...})
        // would leave references alive in the store, preventing GC of the
        // attachments / tool-progress objects even after the Runtime is
        // gone. Resetting every object/array field explicitly lets the
        // previous session's contents (and its upload-file blobs) be
        // collected.
        messages: [],
        tokenUsage: null,
        contextUsage: null,
        messageOffset: 0,
        messageLimit: 0,
        messageTotal: 0,
        messagesStale: false,
        // Reset alongside the cursor — the fields above no longer describe
        // any server window, so the next load must adopt one verbatim.
        windowAuthoritative: false,
        pendingApproval: {},
        pendingQuestions: [],
        loadError: null,
        sessionStatus: null,
        todos: [],
        queuedMessages: [],
        pendingIntents: [],
        model: null,
        provider: null,
        providerAccountId: null,
        title: null,
        ratio: null,
        reasoningEffort: null,
        temperature: null,
        isCompacting: false,
        treeExpandedPaths: [],
        attachedContext: [],
        hasMoreIncremental: false,
        abortController: null,
        loadSequence: 0,
        loadOwnerSeq: 0,
        isReasoning: false,
        isSessionReady: false,
        isLoadingSession: false,
        isLoadingMore: false,
        toolProgress: {},
        serverError: null,
      }),
    }));
  },

  clearAgentSessions: (agentId) => {
    const agent = getAgentState(get(), agentId);
    for (const sid of Object.keys(agent.sessionStates)) {
      get().clearSessionState(agentId, sid);
    }
  },

  removeSessionState: (agentId: string, sessionId: string) => {
    releaseAdapterSession(agentId, sessionId);
    set((state) => {
      const agent = getAgentState(state, agentId);
      const newSessionStates = { ...agent.sessionStates };
      delete newSessionStates[sessionId];
      return updateAgentState(state, agentId, { sessionStates: newSessionStates });
    });
  },

  // ADR-033: connectStream removed — MQTT connection is managed by the Rust backend.
  // The frontend no longer creates WebSocket connections.

  sendMessage: async (content: string, agentId: string, command?: string, attachedItems?: AttachedItem[]) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;

    // Add user message to the active session's state
    // NOTE: Use crypto.randomUUID() so the ID survives round-trip to the backend
    // and back — loadSessionMessages() deduplicates by message ID, so the
    // optimistic render and the backend-persisted message must share the same ID.
    const userMsgId = `msg-${crypto.randomUUID()}`;

    // ADR-046: Build the unified `attached_items` payload from BOTH upload
    // payloads (`attachedItems`) AND workspace refs (`attachedContext`).
    // They are collected into one list FIRST so that every attachment gets
    // a `clientId` and an optimistic system entry. Without this step
    // workspace refs would only appear after the backend responds.
    const rawItems: AttachedItem[] = [...(attachedItems ?? [])];
    const ssAttachedContext = sessionId
      ? getSessionState(get(), agentId, sessionId).attachedContext
      : [];
    if (sessionId && ssAttachedContext.length > 0) {
      for (const ctx of ssAttachedContext) {
        if (ctx.type === "file") {
          rawItems.push({
            type: "attached_file",
            absPath: ctx.absPath,
            name: ctx.name,
          });
        } else if (ctx.type === "selection") {
          rawItems.push({
            type: "attached_selection",
            absPath: ctx.absPath,
            name: ctx.name,
            startLine: ctx.startLine ?? 1,
            endLine: ctx.endLine ?? ctx.startLine ?? 1,
          });
        } else if (ctx.type === "directory") {
          rawItems.push({
            type: "attached_folder",
            absPath: ctx.absPath,
            name: ctx.name,
          });
        }
      }
      // Clear workspace refs immediately so the next send starts fresh.
      // This must happen before the async MQTT publish to avoid races if
      // the user sends another message quickly.
      set((s) =>
        updateSessionState(s, agentId, sessionId, { attachedContext: [] }),
      );
    }

    const items: AttachedItem[] = rawItems.map((item) => {
      const clientId = `msg-${crypto.randomUUID()}`;
      return { ...item, clientId };
    });

    const now = Date.now();
    const userMsg: ChatMessage = {
      id: userMsgId,
      type: "user",
      content,
      timestamp: now,
      ...getUserSenderInfo(),
    };

    // Build optimistic system entries for each attachment so they appear
    // immediately in the chat list alongside the user text bubble.
    const optimisticAttachments: ChatMessage[] = items.map((item, idx) => {
      const metaType = item.type; // "file_upload" | "image_upload" | "attached_file" | "attached_selection" | "attached_folder"
      const meta: Record<string, unknown> = { type: metaType };
      // Copy all fields except `type` and `clientId` into metadata
      for (const [k, v] of Object.entries(item)) {
        if (k !== "type" && k !== "clientId" && v !== undefined) {
          meta[k] = v;
        }
      }
      return {
        id: item.clientId!,
        type: "system" as const,
        content: "",
        timestamp: now + 1 + idx, // +1ms per attachment so they sort after the user text
        metadata: meta,
        senderDisplayName: undefined,
        _isOptimistic: true as const,
      };
    });

    if (sessionId) {
      // ADR-050 post-C5 fix: user message is written directly into
      // messages[] (optimistic insert).  The id (crypto.randomUUID)
      // survives the round-trip to the backend, so when the HTTP refresh
      // returns the same message, mergeMessageWindow deduplicates by id
      // (server version wins).  This replaces the old liveBuffer.
      // pendingUserMessage path and ensures the user sees their message
      // immediately even in a fresh session where limit === 0.
      set((state) => {
        const ss = getSessionState(state, agentId, sessionId!);
        // Guard against duplicate (rapid double-send of same id).
        if (ss.messages.some(m => m.id === userMsgId)) return {};
        return updateSessionState(state, agentId, sessionId!, {
          messages: [...ss.messages, userMsg, ...optimisticAttachments],
          messageTotal: ss.messageTotal + 1 + optimisticAttachments.length,
          messageLimit: ss.messageLimit + 1 + optimisticAttachments.length,
        });
      });
      log.debug("[ChatStore:DEBUG] sendMessage optimistic insert to messages[]", {
        sid: sessionId,
        userMsgId,
        attachedItemCount: items.length,
      });
    }

    // ADR-046 §params: only `attached_items` survives. The legacy
    // `content_parts` / `attached_context` / `document_ids` MQTT params are
    // dropped — the runtime reads everything it needs from
    // `params_json.attached_items`.
    const params: Record<string, unknown> = {};
    if (items.length > 0) params.attached_items = toWireAttachedItems(items);
    const paramsJson = Object.keys(params).length > 0 ? JSON.stringify(params) : "";

    // Ids of the optimistic entries inserted above — used to roll them back
    // if the MQTT publish fails (the backend never accepted the message).
    const optimisticIds = new Set([userMsgId, ...items.map((i) => i.clientId ?? "")]);

    // ADR-100 D12: does this session have a Chat to file the message in? Two
    // halves must both hold — a binding stamped by the Runtime, and a signed-in
    // account (the Chat channel is identity-bearing; in local mode there is no
    // Chat surface at all, so the session-scoped write is the only path and an
    // unbound send is the correct fallback, not a degradation).
    const me = useAuthStore.getState().account?.user_id ?? "";
    const chatSessionId =
      sessionId &&
      useAgentStore
        .getState()
        .agents[agentId]?.sessions.find((s) => s.session_id === sessionId)?.chat_session_id;
    const pinnedChat = me !== "" && chatSessionId ? { me, chatSessionId } : undefined;

    try {
      // ADR-076 §决策 4: HTTP, not MQTT — the send is a user action, so the
      // Gateway authenticates the caller and the Runtime checks ownership.
      //
      // ADR-100 D12: when this execution session came from a Chat, the words
      // go through the Chat entry point instead, pinned back to this session.
      // The human's message then gets its own durable row under the human's own
      // identity (so the conversation reads as a question and an answer, not as
      // a result the Runtime transcribed) and the Agent skips its router, so the
      // turn still runs right here. Everything after the send is shared: the
      // bubble, the attachment entries and the reconcile all key on `userMsgId`,
      // which both paths land on the same id.
      if (pinnedChat) {
        await sendPinnedChatMessage({
          me: pinnedChat.me,
          agentId,
          chatSessionId: pinnedChat.chatSessionId,
          text: content,
          messageId: userMsgId,
          executionSessionId: sessionId!,
          command,
          attachedItems: items.length > 0 ? toWireAttachedItems(items) : undefined,
        });
      } else {
        await sessionControl.sendMessage(agentId, sessionId!, {
          message_id: userMsgId,
          content,
          command: command ?? "",
          params_json: paramsJson,
        });
      }
      log.debug("[ChatStore] Message sent via HTTP:", userMsgId);
      // Touch the user's last-interaction timestamp so the next Gateway
      // boot can pick this agent as the default (`/api/agents` sorts by
      // `last_interaction_at`). Best-effort — failure is logged but does
      // not roll back the sent message; the on-disk store is already
      // idempotent on consecutive touches.
      void fetch(`${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/interactions`, {
        method: "POST",
      }).catch((err: unknown) => {
        log.debug("[ChatStore] touch_interaction HTTP failed (non-fatal):", err);
      });
      // ADR-050 C5: reconcile the optimistic user message + attachment
      // entries with the server once the backend has persisted them. See
      // `scheduleSendReconciliation` — without this the attachment chips
      // kept their "waiting for server confirmation" spinner until the
      // session was reopened.
      if (sessionId) scheduleSendReconciliation(agentId, sessionId);
    } catch (error) {
      log.error("[ChatStore] HTTP message send failed:", error);
      // The backend never accepted this send — drop the optimistic entries
      // we just inserted so the user does not see a phantom message with a
      // perpetual spinner.
      if (sessionId) {
        set((state) => {
          const ss = getSessionState(state, agentId, sessionId!);
          const removed = ss.messages.filter(
            (m) => optimisticIds.has(m.id) && m._isOptimistic,
          ).length;
          if (removed === 0) return {};
          return updateSessionState(state, agentId, sessionId!, {
            messages: ss.messages.filter(
              (m) => !(optimisticIds.has(m.id) && m._isOptimistic),
            ),
            messageTotal: Math.max(0, ss.messageTotal - removed),
            messageLimit: Math.max(0, ss.messageLimit - removed),
          });
        });
      }
      // Transient client-side error - show a toast, don't pollute the
      // messages array (sliding window over JSONL). A Gateway denial
      // (403/404 `SessionAccessError`) is NOT a connection problem —
      // say "you lack permission", not "wait and retry".
      if (sessionControl.isAccessDenied(error)) {
        showToast({
          type: "error",
          message: (error as sessionControl.SessionAccessError).message,
        });
      } else {
        showToast({
          type: "error",
          message: i18n.t("chatPanel.sendFailedNotConnected"),
        });
      }
    }
  },

  stopCurrentMessage: async (agentId: string) => {
    log.debug("[ChatStore] Stopping current message for agent:", agentId);

    // ADR-076 §决策 4: HTTP now (was MQTT `stop`).
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (sessionId) {
      sessionControl
        .stopSession(agentId, sessionId)
        .catch((err: unknown) => log.warn("[ChatStore] stop failed:", err));

      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, {
          }),
      }));
    }
  },

  sendStop: (agentId: string) => {
    // ADR-076 §决策 4: HTTP now (was MQTT `stop`).
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (sessionId) {
      sessionControl
        .stopSession(agentId, sessionId)
        .catch((err: unknown) => log.warn("[ChatStore] sendStop failed:", err));
    }

    // Optimistic: immediately mark as stopping so the UI exits "working" state
    // without waiting for the backend Stopped/SessionStateChanged event.
    const activeSessionId = getAgentState(get(), agentId).activeSessionId;
    if (activeSessionId) {
      set((state) =>
        updateSessionState(state, agentId, activeSessionId, {}),
      );
    }
  },


  trimMessagesTo: (agentId: string, count: number) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;
    set((state) => {
      const session = getSessionState(state, agentId, sessionId);
      if (session.messages.length <= count) return {};
      return updateSessionState(state, agentId, sessionId, {
        messages: session.messages.slice(0, count),
        messageOffset: 0,
        messageLimit: 0,
        messageTotal: 0,
        // Local truncation makes the cache non-authoritative (tail lost) —
        // force a reload on the next re-entry so the conversation renders whole.
        messagesStale: true,
        // The slice keeps the OLDEST rows while the cursor was just zeroed,
        // so nothing about the current coordinates is trustworthy: the next
        // load adopts the server's window verbatim.
        windowAuthoritative: false,
      });
    });
  },

  setCurrentModel: (model: string, provider: string, agentId: string, accountId?: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    log.debug("[ChatStore:DEBUG] setCurrentModel called", { model, provider, agentId, accountId, sessionId });
    if (!sessionId) return;

    // Resolve new model's default reasoning effort from availableModels.
    // Lookup goes through resolveDefaultReasoningEffort so this stays in
    // sync with the MQTT model_confirmed handler.
    const defaultEffort = resolveDefaultReasoningEffort(get().availableModels, model, provider);

    // Update session model + reset reasoningEffort to new model's default.
    // `accountId` picks WHICH API key of this provider the session uses
    // (null = the provider's first account) — the Runtime persists it in the
    // session meta so the choice survives a restart.
    set((state) => updateSessionState(state, agentId, sessionId, {
      model,
      provider,
      providerAccountId: accountId ?? null,
      reasoningEffort: defaultEffort,
    }));
    // Update agent's default model (new sessions inherit this)
    set((state) => updateAgentState(state, agentId, { preferredModel: model, preferredProvider: provider }));

    // ADR-076 §决策 4: model switch is an authenticated HTTP write now
    // (was MQTT `model_switch`). The MQTT path could not be authorized — it
    // carries no identity — so any client on the broker could retarget
    // another account's session at a different model/provider.
    //
    // `provider` must travel with `model`: the Runtime rebuilds the
    // per-session Provider instance from it. Without it, switching to a
    // model served by a different provider (e.g. deepseek → minimax) would
    // keep the previous base_url and yield 401s.
    // `account_id` selects the API key when the provider holds several
    // (multi-account, was the MQTT payload's `account_id`).
    //
    // Capture the pre-switch values: a refused write (403 for a
    // view-only caller) must snap the picker back, or the UI would keep
    // showing a model that was never persisted.
    const prevAgent = getAgentState(get(), agentId);
    const prevSession = prevAgent.sessionStates[sessionId];
    const prev = {
      model: prevSession?.model ?? null,
      provider: prevSession?.provider ?? null,
      providerAccountId: prevSession?.providerAccountId ?? null,
      reasoningEffort: prevSession?.reasoningEffort ?? null,
      preferredModel: prevAgent.preferredModel,
      preferredProvider: prevAgent.preferredProvider,
    };
    sessionControl
      .patchSessionConfig(agentId, sessionId, { model, provider, account_id: accountId ?? undefined })
      .catch((err: unknown) => {
        log.warn("[ChatStore] model switch failed:", err);
        // Two separate `set`s — `updateSessionState` returns only the
        // patched `agentStates` slice, which is not a full `ChatStore`,
        // so the two helpers cannot be nested.
        set((state) =>
          updateSessionState(state, agentId, sessionId!, {
            model: prev.model,
            provider: prev.provider,
            providerAccountId: prev.providerAccountId,
            reasoningEffort: prev.reasoningEffort,
          }),
        );
        set((state) =>
          updateAgentState(state, agentId, {
            preferredModel: prev.preferredModel,
            preferredProvider: prev.preferredProvider,
          }),
        );
        showToast({
          type: "error",
          message: sessionControl.isAccessDenied(err)
            ? (err as sessionControl.SessionAccessError).message
            : i18n.t("chatPanel.modelSwitchFailed", { model }),
        });
      });
  },

  setSessionWorkspace: (agentId: string, sessionId: string, workspaceId: string) => {
    sessionControl
      .setSessionWorkspace(agentId, sessionId, workspaceId)
      .catch((err: unknown) => log.warn("[ChatStore] workspace switch failed:", err));
  },
  setReasoningEffort: (effort: string, agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;

    const previous =
      getAgentState(get(), agentId).sessionStates[sessionId]?.reasoningEffort ?? null;

    // Optimistically update frontend state (Runtime will confirm)
    set((state) => updateSessionState(state, agentId, sessionId, { reasoningEffort: effort }));

    // ADR-076 §决策 4: authenticated HTTP write (was MQTT `reasoning_effort`).
    sessionControl
      .patchSessionConfig(agentId, sessionId, { reasoning_effort: effort })
      .catch((err: unknown) => {
        log.warn("[ChatStore] reasoning effort failed:", err);
        // Snap the menu back — a refused write must not leave the
        // session claiming an effort level the Runtime never accepted.
        set((state) =>
          updateSessionState(state, agentId, sessionId, { reasoningEffort: previous }),
        );
        showToast({
          type: "error",
          message: sessionControl.isAccessDenied(err)
            ? (err as sessionControl.SessionAccessError).message
            : i18n.t("chatPanel.reasoningEffortFailed"),
        });
      });
  },
  setSessionContextWindow: (window: number | null, agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;

    const previous = getAgentState(get(), agentId).sessionStates[sessionId]?.sessionContextWindow ?? null;

    // Optimistically update frontend state. `null` = no override.
    set((state) => updateSessionState(state, agentId, sessionId, { sessionContextWindow: window }));

    // ADR-074 D3: write through the existing HTTP `PUT /sessions/{sid}/config`
    // (Gateway transparently proxies to Runtime) using `SessionConfigDelta`.
    // `0` (not `null`) is sent to CLEAR so the backend can distinguish
    // "clear" from "unchanged" (both deserialize to `None` on the wire).
    const body = window == null ? { context_window: 0 } : { context_window: window };
    const target = `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/config`;
    fetch(target, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    })
      .then(async (resp) => {
        if (!resp.ok) {
          const detail = await resp.text().catch(() => "");
          log.warn(`[ChatStore] setSessionContextWindow HTTP ${resp.status}: ${detail}`);
          // Roll back the optimistic update on failure (the MQTT retained
          // snapshot never arrives for a rejected PUT).
          set((state) => updateSessionState(state, agentId, sessionId, { sessionContextWindow: previous }));
        }
      })
      .catch((err: unknown) => {
        log.warn("[ChatStore] setSessionContextWindow failed:", err);
        set((state) => updateSessionState(state, agentId, sessionId, { sessionContextWindow: previous }));
      });
  },
  loadAvailableModels: () => {
    if (inflightModels) return inflightModels;
    inflightModels = (async () => {
      // Re-read while vault writes landed after the snapshot we read — the
      // classic race: connect edge starts the load, the user saves a new
      // provider in Harness mid-flight, `models-added` must not be swallowed.
      let mutationsAtSnapshot: number;
      do {
        const keys = await invoke<VaultKeyEntry[]>("list_keys");
        // Capture the mutation count AFTER the read: the result reflects
        // every write up to this point, so only later events demand a
        // re-run (the same read-after-version-check as cache validation —
        // capturing before the read would re-run for events the read saw).
        mutationsAtSnapshot = vaultMutationSeq;

        // Accounts per provider — drives the model menu's account level.
        // Identity-preserving write: the triggers here fire on every
        // Gateway connect edge, so unchanged content must not hand every
        // subscriber a fresh object (AGENTS.md, identity-preserving store
        // updates — these lists feed the memoized transcript surfaces'
        // parent components).
        const accountsByProvider: Record<string, ProviderAccount[]> = {};
        for (const key of keys) {
          if (!key.account_id) continue;
          (accountsByProvider[key.provider] ??= []).push({
            accountId: key.account_id,
            alias: key.alias?.trim() || key.provider,
            preview: key.key_preview ?? "",
          });
        }
        set((state) =>
          sameProviderAccounts(state.providerAccounts, accountsByProvider)
            ? state
            : { providerAccounts: accountsByProvider },
        );

        // Build (provider, configuredModelIds, modelCapabilities) tuples, skipping
        // empty entries. `models` is provider-level config shared by every account
        // of that provider, so collapse the per-account rows first — otherwise a
        // 3-key provider would refetch the same model list 3 times.
        const byProvider = new Map<string, VaultKeyEntry>();
        for (const key of keys) if (!byProvider.has(key.provider)) byProvider.set(key.provider, key);

        const entries = [...byProvider.values()].map((key) => ({
          provider: key.provider,
          modelIds: key.models?.length
            ? key.models
            : key.default_model
              ? [key.default_model]
              : [],
          modelCapabilities: key.model_capabilities,
        })).filter((e) => e.modelIds.length > 0);

        // Fetch capabilities for all providers in parallel, each behind a
        // deadline (a hung fetch would wedge the single-flight lock forever).
        const previousModels = get().availableModels;
        const results = await Promise.allSettled(
          entries.map((e) => fetchProviderModelsWithTimeout(e.provider)),
        );

        // A provider whose fetch FAILED (or timed out) keeps the
        // capabilities we last resolved for its models rather than being
        // rewritten with "no capability": one transient 500 must not make
        // the reasoning control disappear from the composers.
        const prevCapsByProvider = new Map<string, Map<string, ModelEntry>>();
        for (const m of previousModels) {
          let byName = prevCapsByProvider.get(m.provider);
          if (!byName) {
            byName = new Map();
            prevCapsByProvider.set(m.provider, byName);
          }
          byName.set(m.name, m);
        }

        const allModels: ModelEntry[] = [];
        entries.forEach((entry, i) => {
          const result = results[i];
          const apiModels =
            result.status === "fulfilled" ? (result.value.models ?? []) : null;
          const prev = prevCapsByProvider.get(entry.provider);
          for (const modelId of entry.modelIds) {
            const info = apiModels?.find((m) => m.id === modelId);
            const prevEntry = prev?.get(modelId);
            const fallback = apiModels === null ? prevEntry : undefined;
            allModels.push({
              name: modelId,
              provider: entry.provider,
              tool_call: info?.tool_call ?? fallback?.tool_call ?? undefined,
              reasoning: info?.reasoning ?? fallback?.reasoning ?? undefined,
              input_modalities:
                info?.input_modalities ?? fallback?.input_modalities ?? undefined,
              default_reasoning_effort:
                entry.modelCapabilities?.[modelId]?.default_reasoning_effort ??
                fallback?.default_reasoning_effort ??
                undefined,
            });
          }
        });

        // Deduplicate by model name + provider
        const uniqueModels = allModels.filter(
          (m, i, arr) =>
            arr.findIndex((x) => x.name === m.name && x.provider === m.provider) === i,
        );
        set((state) =>
          sameModelEntries(state.availableModels, uniqueModels)
            ? state
            : { availableModels: uniqueModels },
        );
      } while (vaultMutationSeq !== mutationsAtSnapshot);
    })()
      .catch(() => {
        // Vault unreadable (outside the desktop shell) or the Gateway is
        // not running — keep whatever inventory we already have.
      })
      .finally(() => {
        inflightModels = null;
      });
    return inflightModels;
  },
  continueExecution: async (agentId: string, sessionId?: string) => {
    try {
      const sid = sessionId ?? getAgentState(get(), agentId).activeSessionId;
      if (!sid) return;
      // ADR-076 §决策 4: HTTP now (was MQTT `continue_execution`).
      await sessionControl.continueSession(agentId, sid);
    } catch (error) {
      log.error("[ChatStore] Failed to send continue signal:", error);
    }
  },

  resolveApproval: (agentId: string) => {
    const sessionId = getAgentState(get(), agentId).activeSessionId;
    if (!sessionId) return;
    set((state) => updateSessionState(state, agentId, sessionId, { pendingApproval: {} }));
  },
  resolveApprovalByToolCallId: (agentId: string, toolCallId: string, sessionId?: string) => {
    const sid = sessionId ?? getAgentState(get(), agentId).activeSessionId;
    if (!sid) return;
    set((state) => {
      const prevPending = getSessionState(state, agentId, sid).pendingApproval;
      const nextPending = { ...prevPending };
      delete nextPending[toolCallId];
      return updateSessionState(state, agentId, sid, { pendingApproval: nextPending });
    });
  },
  resolveQuestion: (agentId: string, requestId: string, sessionId?: string) => {
    const sid = sessionId ?? getAgentState(get(), agentId).activeSessionId;
    if (!sid) return;
    set((state) => updateSessionState(state, agentId, sid, {
      pendingQuestions: (getSessionState(state, agentId, sid).pendingQuestions ?? []).filter(
        (q) => q.request_id !== requestId
      ),
    }));
  },

  loadSessionMessages: async (
    agentId: string,
    sessionId: string,
    offset?: number,
    limit: number = 50,
    replaceCache = false,
  ): Promise<{ offset: number; limit: number; total: number } | undefined> => {
    // ADR-021: Per-session abortController + loadSequence (no cross-session interference).
    const sessionState = getSessionState(get(), agentId, sessionId);
    const seq = sessionState.loadSequence + 1;

    const oldController = sessionState.abortController;
    if (oldController) {
      oldController.abort();
    }
    const controller = new AbortController();
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        loadSequence: seq,
        abortController: controller,
      }),
    }));

    // "Initial load" = the caller is fetching the session for the first time
    // (caller passes `offset === undefined`) AND the cache is empty.  This is
    // the only path that:
    //   - shows the "Loading conversation..." indicator
    //   - clears streaming buffers (activeStream entries that predate this
    //     load cycle and would otherwise be orphaned)
    //
    // ADR-050: forward offset semantics.  `offset === undefined` means
    // "from the tail end of the conversation" — backend interprets this
    // via the `tail=true` query parameter so the caller does not need to
    // know `total` before issuing the very first request.
    //
    // offset===0 with a non-empty cache is NOT initial — it's a user-initiated
    // "jump to the oldest page" (a deliberate scroll-to-top), since under
    // forward semantics offset=0 means the OLDEST entry.  We must NOT clear
    // streaming in that case (the agent may still be writing live), and we
    // must NOT show the loading overlay (the cache already has content to
    // render).
    const cacheIsEmpty = sessionState.messages.length === 0;
    const isInitialLoad = offset === undefined && cacheIsEmpty;
    if (isInitialLoad) {
      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, {
          isLoadingSession: true,
          loadError: null,
          // Record this load as the spinner's owner so every settle
          // point below can decide who may clear it.
          loadOwnerSeq: seq,
        }),
      }));
    }

    try {
      const params = new URLSearchParams();
      params.set("limit", String(limit));
      // ADR-050: initial load uses `?tail=true` so the backend can return
      // the latest `limit` entries without the caller having to know
      // `total` yet.  Subsequent pagination calls pass an explicit offset.
      if (offset === undefined) {
        params.set("tail", "true");
      } else {
        params.set("offset", String(offset));
      }
      // ADR-035 Phase 3: no HTTP incremental endpoint

      // Bug B v3 fix: chat endpoints proxy through the Runtime and 503
      // during the boot window between Gateway discovery and Runtime
      // HTTP port registration. `with503Retry` honours the Gateway's
      // Retry-After header so a transient 503 at session-load time
      // recovers transparently instead of flashing an error to the user.
      const resp = await with503Retry(
        () => fetch(
          `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/messages?${params}`,
          { signal: controller.signal },
        ),
        {
          tag: `ChatStore.loadSessionMessages(${agentId}/${sessionId})`,
          logger: log,
          signal: controller.signal,
        },
      );

      if (getSessionState(get(), agentId, sessionId).loadSequence !== seq) {
        log.debug(`[ChatStore] Discarding stale loadSessionMessages response (seq ${seq})`);
        clearSpinnerIfOwner(agentId, sessionId, seq);
        return;
      }

      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);

      const data = (await resp.json()) as PaginatedMessages;

      if (getSessionState(get(), agentId, sessionId).loadSequence !== seq) {
        log.debug(`[ChatStore] Discarding stale response after json parse (seq ${seq})`);
        clearSpinnerIfOwner(agentId, sessionId, seq);
        return;
      }

      const converted = mergeDocumentUploads(data.messages ?? [], agentId);
      const returnedOffset = data.offset;
      const returnedLimit = data.limit;
      const returnedTotal = data.total;

      set((state) => {
        const ss = getSessionState(state, agentId, sessionId);
        const prevOffset = ss.messageOffset;
        const prevLimit = ss.messageLimit;

        // Server window is the authoritative timestamp-ordered slice.
        // `mergeMessageWindow` reconciles it with the local cache and the
        // optimistic-user overlay in a single pass:
        //
        //   1. server entries win on id collision (defensive dedupe even
        //      though the backend never emits duplicates within a window).
        //   2. cache entries with non-overlapping ids are preserved
        //      (older rows from `loadBefore`, etc.).
        //   3. optimistic entries not yet echoed back are kept in the
        //      overlay and sorted by timestamp into the final array.
        //   4. final array is sorted by `timestamp` ascending — this is
        //      what the user perceives as "correct order" regardless of
        //      which window HTTP fetched.
        //
        // Pagination cursor math is unchanged: offset/limit continue to
        // be derived from the server window (server-authoritative).
        // ADR-050: under forward semantics, the direction comparison is
        //   returnedOffset > prevOffset → loading NEWER entries (cursor
        //     walked forward, larger offset = further from oldest)
        //   returnedOffset < prevOffset → loading OLDER entries (cursor
        //     walked back toward the oldest end, smaller offset)
        // The min/max expressions below are direction-agnostic — they
        // always extend the cached range to the union of old + new window.
        // ADR-050 C2: mergeMessageWindow is now (cache, server) → messages
        // (the optimistic overlay lives in chatAdapterStore, not here).
        // We also notify the adapter of which optimistic ids the server
        // confirmed so it can drop them from its overlay.
        const merged = replaceCache
          ? { messages: converted }
          : mergeMessageWindow(ss.messages, converted);
        const confirmedIds = new Set(
          converted.map((m) => m.id),
        );
        // Only run the side effect after the setState closure commits —
        // we don't want a setState on a different store to interleave
        // with this transition.
        queueMicrotask(() => {
          clearOptimisticEntries(agentId, sessionId, confirmedIds);
        });

        let finalOffset = returnedOffset;
        let finalLimit = returnedLimit;

        if (replaceCache) {
          // Jump / replace: the fetched page is the authoritative cursor.
          finalOffset = returnedOffset;
          finalLimit = returnedLimit;
        } else if (isInitialLoad) {
          // ADR-050 C2: drop any in-flight activeStream tracker so a
          // stale lineCount from a previous session incarnation cannot
          // bleed into the freshly-loaded view.  The tracker now lives
          // in chatAdapterStore; chatStore delegates the cleanup.
          releaseAdapterSession(agentId, sessionId);
          // Nothing to union with — the window is the server's window.
          finalOffset = returnedOffset;
          finalLimit = returnedLimit;
        } else if (!ss.windowAuthoritative || ss.messagesStale) {
          // The cached window is not trustworthy — the union math below is
          // only valid between two authoritative, contiguous windows, and
          // there are exactly two ways to not have one:
          //
          //  - `!windowAuthoritative`: no HTTP window has ever landed for
          //    this cache, so `messageOffset` is not "the left edge", it is
          //    simply unset. This is the live-seeded case: `record_complete`
          //    appends tail rows for a session whose history was never
          //    fetched (a session still running when the Desktop starts,
          //    opened later from the history list). The server answers the
          //    tail window at `total - limit`, the union math reads that as
          //    "loading NEWER", and it fabricates `[0, total)` over a 50-row
          //    array: `hasOlder` false, scroll-up dead until the cache gets
          //    cleared and reloaded (regression t-f9a9fbc7). `messagesStale`
          //    cannot catch it — appending a live row is legitimate, so
          //    nothing marked the cache unclean.
          //  - `messagesStale`: the cursor was reset by
          //    `clearSessionMessages` / `trimMessagesTo` and the array is a
          //    partial slice, possibly bumped by out-of-band writes.
          //
          // In both, ADOPT the server window's cursor verbatim.
          finalOffset = returnedOffset;
          finalLimit = returnedLimit;
        } else if (returnedOffset > prevOffset) {
          // Loading NEWER messages (scroll-down).  Window slid forward;
          // extend the cached range to include the newer entries the
          // server just returned.
          finalOffset = Math.min(returnedOffset, prevOffset);
          finalLimit = Math.max(
            returnedOffset + returnedLimit,
            prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
          ) - finalOffset;
        } else if (returnedOffset < prevOffset) {
          // Loading OLDER messages (scroll-up).  Window slid backward
          // (smaller offset under forward semantics); expand the cached
          // range to include the older entries the server just sent.
          finalOffset = Math.min(returnedOffset, prevOffset);
          finalLimit = Math.max(
            returnedOffset + returnedLimit,
            prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
          ) - finalOffset;
        } else {
          // returnedOffset === prevOffset: same window (a duplicate load
          // or a re-fetch of the same page).  No cursor math change.
          finalOffset = Math.min(returnedOffset, prevOffset);
          finalLimit = Math.max(
            returnedOffset + returnedLimit,
            prevOffset + (ss.messages.length > 0 ? prevLimit : 0),
          ) - finalOffset;
        }

        // Only the recorded spinner owner may clear the flag; a
        // superseding load still in flight owns it otherwise.
        const ownsSpinner = ss.loadOwnerSeq === seq;
        return updateSessionState(state, agentId, sessionId, {
          messages: merged.messages,
          messageOffset: finalOffset,
          messageLimit: finalLimit,
          messageTotal: returnedTotal,
          // A real HTTP window landed — the cache is authoritative for the
          // fetched window again (see the messagesStale doc).  hasOlder /
          // hasNewer drive any further pagination.
          messagesStale: false,
          // The coordinates above now describe a server-reported window, so
          // subsequent loads may union against them (see windowAuthoritative).
          windowAuthoritative: true,
          ...(ownsSpinner ? { isLoadingSession: false } : {}),
          loadError: null,
        });
      });

      return { offset: returnedOffset, limit: returnedLimit, total: returnedTotal };
    } catch (e: unknown) {
      if (getSessionState(get(), agentId, sessionId).loadSequence !== seq) {
        log.debug(`[ChatStore] Discarding stale error response (seq ${seq})`);
        clearSpinnerIfOwner(agentId, sessionId, seq);
        return;
      }
      if (e instanceof DOMException && e.name === "AbortError") {
        // P0-3: abort / cancellation must NEVER wipe the cached message
        // window. The previous implementation reset `messages: []` here,
        // which destroyed in-flight optimistic entries and caused the
        // "user message reappears as duplicate" bug. We only flip the
        // loading flags so the UI can re-attempt via the next scroll /
        // scheduleRefresh / setPinnedToBottom hook.
        log.debug(`[ChatStore] loadSessionMessages aborted (seq ${seq})`);
        set((state) => updateSessionState(state, agentId, sessionId, {
          // The spinner is cleared only when this load still owns it.
          ...(getSessionState(state, agentId, sessionId).loadOwnerSeq === seq
            ? { isLoadingSession: false }
            : {}),
          isLoadingMore: false,
        }));
        return;
      }
      log.error("[ChatStore] Failed to load session messages:", e);
      // P0-3: even on hard failures, do NOT reset `messages[]`. The
      // cache may already hold valid content from a previous window; we
      // only surface the error and clear loading flags so the retry
      // button in VirtualMessageList can fire `ensureLatestInCache`
      // without first losing the cached history.
      set((state) => updateSessionState(state, agentId, sessionId, {
        // The spinner is cleared only when this load still owns it.
        ...(getSessionState(state, agentId, sessionId).loadOwnerSeq === seq
          ? { isLoadingSession: false }
          : {}),
        isLoadingMore: false,
        loadError: `${i18n.t("chatPanel.sessionLoadFailed")}: ${e instanceof Error ? e.message : String(e)}`,
      }));
    } finally {
      const currentController = getSessionState(get(), agentId, sessionId).abortController;
      if (currentController === controller) {
        set((state) => ({
          ...updateSessionState(state, agentId, sessionId, { abortController: null }),
        }));
      }
    }
  },

  abortSessionLoad: (agentId: string, sessionId: string) => {
    const controller = getSessionState(get(), agentId, sessionId).abortController;
    if (controller) {
      controller.abort();
      set((state) => ({
        ...updateSessionState(state, agentId, sessionId, { abortController: null }),
      }));
    }
    set((state) => ({
      ...updateSessionState(state, agentId, sessionId, {
        loadSequence: getSessionState(state, agentId, sessionId).loadSequence + 1,
      }),
    }));
  },

  /** ADR-081 §4.2: open a session and locate one message (raw line). */
  locateMessage: async (agentId: string, sessionId: string, messageIndex: number) => {
    await get().openSession(agentId, sessionId);
    let st = getSessionState(get(), agentId, sessionId);
    // The initial tail window may not reach an old hit — reload a window
    // anchored at the target (replaceCache so messages[] lines up with
    // `messageOffset`; the window is contiguous by construction).
    const inWindow =
      messageIndex >= st.messageOffset &&
      messageIndex < st.messageOffset + st.messages.length;
    if (!inWindow) {
      try {
        await get().loadSessionMessages(agentId, sessionId, messageIndex, 50, true);
      } catch (err) {
        log.warn("[chatStore] locateMessage window load failed:", err);
        return; // session is open — user still lands there
      }
    }
    st = getSessionState(get(), agentId, sessionId);
    const targetArrayIdx = messageIndex - st.messageOffset;
    if (targetArrayIdx < 0 || targetArrayIdx >= st.messages.length) {
      log.warn("[chatStore] locateMessage out of bounds:", {
        sessionId,
        messageIndex,
        messageOffset: st.messageOffset,
        loaded: st.messages.length,
      });
      return;
    }
    set((state) =>
      updateSessionState(state, agentId, sessionId, {
        pendingLocate: { sessionId, messageIndex, targetArrayIdx },
      }),
    );
    // Transient highlight — clear it after the visual fade.
    window.setTimeout(() => {
      set((state) =>
        updateSessionState(state, agentId, sessionId, { pendingLocate: null }),
      );
    }, 5_000);
  },


  /**
   * ADR-050: One-shot jump to the latest page (tail of the conversation).
   *
   * Replaces the old `loadMoreNewerMessages` loop which had to issue N HTTP
   * requests to slide a cache from the middle to the tail.  After this call
   * the cache holds the newest page; `messageOffset + messageLimit ===
   * messageTotal`; the rendering layer's ensureRenderable effect then decides
   * if more data is needed to fill the viewport (same load path).
   *
   * No-op if the cache is already at the tail (the cache window's far edge
   * touches `total`) — the caller is expected to check before invoking.
   *
   * Single-flight: concurrent callers for the same session share one
   * reload (`inflightLatest`) and await it; a second caller no longer
   * returns empty-handed while the first reload is still in flight.
   */
  ensureLatestInCache: async (agentId: string, sessionId: string) => {
    const key = `${agentId}:${sessionId}`;
    const existing = inflightLatest.get(key);
    if (existing) return existing;
    const promise = (async () => {
      const sessionState = getSessionState(get(), agentId, sessionId);
      const { messageOffset, messageLimit, messageTotal, messages, messagesStale } = sessionState;
      // Already at the tail:
      //   - The cache window's far edge touches the end of the conversation
      //     (messageOffset + messageLimit >= messageTotal), AND
      //   - messageTotal > 0 (we know the conversation has data — guards against
      //     a freshly-initialized sessionState whose DEFAULT values are all 0 /
      //     empty, which would otherwise be mistaken for "already at tail"), AND
      //   - messages.length > 0 (the cache has at least some data at the tail).
      //   - !messagesStale — a stale cache's cursor (offset/limit/total) is
      //     NOT trustworthy: after clearSessionMessages, background
      //     record_complete writes directly append tail records and bump
      //     messageTotal/messageLimit (chatStore record_complete handler),
      //     making a PARTIAL slice look like it fully covers the tail
      //     (offset 0 + limit N == total N) while older history — typically
      //     the first user message — is still missing.  Short-circuiting on
      //     such a cache would swallow the reload that restores the missing
      //     head; we must NOT skip the fetch until a real HTTP window lands.
      const tailCovered =
        !messagesStale &&
        messageTotal > 0 &&
        messageOffset + messageLimit >= messageTotal &&
        messages.length > 0;
      if (tailCovered) return;
      set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: true }));
      try {
        // Forward semantics: the latest `limit` entries live at
        // `offset = max(0, total - limit)`.  When total === 0 (fresh session
        // that has never been loaded) we delegate to the initial-load path,
        // which uses `?tail=true` to grab the newest `limit` entries.
        // A stale cache's messageTotal is likewise untrustworthy (partial
        // tail write-backs), so treat it the same way and force a fresh
        // tail fetch instead of deriving an offset from the polluted cursor.
        const limit = 50;
        if (messagesStale || messageTotal === 0) {
          await get().loadSessionMessages(agentId, sessionId, undefined, limit);
        } else {
          const tailOffset = Math.max(0, messageTotal - limit);
          await get().loadSessionMessages(agentId, sessionId, tailOffset, limit);
        }

        // The bare tail page may fold to a LONE explore block (see
        // expandWindowToTurnBoundary) — pull older pages until the window
        // starts on a turn boundary.
        await expandWindowToTurnBoundary(agentId, sessionId, limit);
      } finally {
        set((state) => updateSessionState(state, agentId, sessionId, { isLoadingMore: false }));
      }
    })().finally(() => {
      inflightLatest.delete(key);
    });
    inflightLatest.set(key, promise);
    return promise;
  },

  /** Release the messages array for a session to free memory. */
  clearSessionMessages: (agentId: string, sessionId: string) => {
    clearAllOptimisticEntries(agentId, sessionId);
    set((state) => updateSessionState(state, agentId, sessionId, {
      messages: [],
      messageOffset: 0,
      messageLimit: 0,
      messageTotal: 0,
      // The cache is now a non-authoritative empty slice.  It can only be
      // partially repopulated by background record_complete writes (e.g. a
      // session the user switched away from mid-stream), so consumers must
      // NOT treat "has data" as "has complete data" until a full HTTP load
      // lands (loadSessionMessages clears this back to false).
      messagesStale: true,
      // Cursor reset: the next load adopts the server's window verbatim
      // instead of unioning on top of a window that no longer exists.
      windowAuthoritative: false,
    }));
  },

  toggleTreeExpandedPath: (agentId: string, sessionId: string, relPath: string) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      const current = ss.treeExpandedPaths;
      const idx = current.indexOf(relPath);
      const next = idx >= 0
        ? current.filter((p) => p !== relPath)
        : [...current, relPath];
      return updateSessionState(state, agentId, sessionId, { treeExpandedPaths: next });
    });
  },

  expandTreeToPath: (agentId, sessionId, relPath) => {
    if (!relPath) return;
    const parts = relPath.split("/");
    if (parts.length <= 1) return;
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      const current = ss.treeExpandedPaths;
      const set_ = new Set(current);
      let changed = false;
      for (let i = 0; i < parts.length - 1; i++) {
        const ancestor = parts.slice(0, i + 1).join("/");
        if (!set_.has(ancestor)) {
          set_.add(ancestor);
          changed = true;
        }
      }
      if (!changed) return state;
      return updateSessionState(state, agentId, sessionId, {
        treeExpandedPaths: Array.from(set_),
      });
    });
  },

  addAttachedContext: (agentId: string, sessionId: string, item: { id: string; type: "file" | "directory" | "selection"; name: string; absPath: string; startLine?: number; endLine?: number }) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      // Avoid duplicates
      if (ss.attachedContext.some((c) => c.id === item.id)) return {};
      return updateSessionState(state, agentId, sessionId, {
        attachedContext: [...ss.attachedContext, item],
      });
    });
  },

  removeAttachedContext: (agentId: string, sessionId: string, id: string) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      return updateSessionState(state, agentId, sessionId, {
        attachedContext: ss.attachedContext.filter((c) => c.id !== id),
      });
    });
  },

  clearAttachedContext: (agentId: string, sessionId: string) => {
    set((state) => updateSessionState(state, agentId, sessionId, { attachedContext: [] }));
  },

  addQueuedMessage: (agentId: string, sessionId: string, message: QueuedMessage) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      return updateSessionState(state, agentId, sessionId, {
        queuedMessages: [...ss.queuedMessages, message],
      });
    });
  },

  removeQueuedMessage: (agentId: string, sessionId: string, index: number) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      return updateSessionState(state, agentId, sessionId, {
        queuedMessages: ss.queuedMessages.filter((_, i) => i !== index),
      });
    });
  },

  removeMessageAttachment: (agentId: string, sessionId: string, messageId: string) => {
    set((state) => {
      const ss = getSessionState(state, agentId, sessionId);
      return updateSessionState(state, agentId, sessionId, {
        messages: ss.messages.filter((m) => m.id !== messageId),
      });
    });
  },

  setQueuedMessages: (agentId: string, sessionId: string, messages: QueuedMessage[]) => {
    set((state) => updateSessionState(state, agentId, sessionId, { queuedMessages: messages }));
  },

  clearServerError: (agentId: string, sessionId: string) => {
    set((state) => updateSessionState(state, agentId, sessionId, { serverError: null }));
  },

  // ADR-047: Pull session state (runtime telemetry only) from backend.
  //
  // The Runtime returns `{ session_id, meta, live_state }` where:
  //   - `meta` = state-level metadata (created_at, last_active_at, message_count)
  //   - `live_state` = runtime snapshot (status, ratio, todos, context_usage)
  //     or `null` when no live session is running.
  //
  // ADR-047: config fields (model, provider, workspace_id, reasoning_effort,
  // temperature, title) are NO LONGER included in this response. Use
  // `fetchSessionConfig` or `loadSession` to get config.
  // Errors are non-fatal - warns and returns without blocking startup.
  fetchSessionState: async (agentId: string, sessionId: string) => {
    try {
      // Bug B v3 fix: see `loadSessionMessages` for the rationale.
      // We pass the AbortSignal through `with503Retry` so the chat
      // panel can cancel the boot-window retry loop when the user
      // switches sessions or closes the panel.
      const resp = await with503Retry(
        () => fetch(
          `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}`,
        ),
        { tag: `ChatStore.fetchSessionState(${agentId}/${sessionId})`, logger: log },
      );
      if (!resp.ok) {
        log.warn(`[ChatStore] fetchSessionState HTTP ${resp.status} for session ${sessionId}`);
        return;
      }
      const data = await resp.json() as {
        session_id: string;
        meta: Record<string, unknown>;
        live_state: Record<string, unknown> | null;
      };
      const { meta, live_state: liveState } = data;
      if (!meta && !liveState) return;

      const sessionPatch: Partial<SessionChatState> = {};

      // From meta (state-level metadata only)
      if (meta) {
        if (typeof meta.message_count === "number") {
          sessionPatch.messageTotal = meta.message_count as number;
        }
      }

      // From live_state (runtime fields)
      if (liveState) {
        // Status: parse from JSON
        if (liveState.status && typeof liveState.status === "object") {
          sessionPatch.sessionStatus = liveState.status as SessionStatus;
        }
        // Model chars/token ratio from API calibration.
        if (typeof liveState.ratio === "number") sessionPatch.ratio = liveState.ratio as number;
        // Todo list
        if (liveState.todos && Array.isArray(liveState.todos)) {
          sessionPatch.todos = liveState.todos as TodoItem[];
        }
        // Context usage (token counts, window, percentage)
        if (liveState.context_usage && typeof liveState.context_usage === "object") {
          const prev = getAgentState(get(), agentId).sessionStates[sessionId]?.contextUsage;
          sessionPatch.contextUsage = mergeContextUsage(
            prev,
            liveState.context_usage as ContextUsageInfo,
          );
        }
      }

      if (Object.keys(sessionPatch).length > 0) {
        set((state) => updateSessionState(state, agentId, sessionId, sessionPatch));
      }
    } catch (e) {
      log.warn("[ChatStore] fetchSessionState failed:", e);
    }
  },

  // ADR-047: Pull session config from backend GET /sessions/{sid}/config.
  //
  // The Runtime returns a SessionConfigSnapshot JSON:
  //   { model, provider, workspace_id, reasoning_effort, temperature, title }
  // All fields are optional (null means "not set / no override").
  //
  // This is the authoritative config source - it reads from the in-memory
  // Arc<ConversationSession> which is mutated by apply_config().
  // fetchSessionState does NOT apply config fields; this function does.
  fetchSessionConfig: async (agentId: string, sessionId: string) => {
    try {
      // Bug B v3 fix: same 503 retry rationale as fetchSessionState.
      const resp = await with503Retry(
        () => fetch(
          `${getGatewayUrl()}/api/agents/${agentId}/sessions/${sessionId}/config`,
        ),
        { tag: `ChatStore.fetchSessionConfig(${agentId}/${sessionId})`, logger: log },
      );
      if (!resp.ok) {
        log.warn(`[ChatStore] fetchSessionConfig HTTP ${resp.status} for session ${sessionId}`);
        return;
      }
      const config = await resp.json() as SessionConfigInput & {
        workspace_id?: string | null;
        title?: string | null;
      };

      // ADR-047 P1: null/empty values clear stale config from the previous
      // session. The mapper is the single source of truth for both HTTP
      // and MQTT paths - see sessionConfigMapper.ts. Both paths use
      // clearOnNull: true because both deliver a full snapshot.
      const sessionPatch = sessionConfigToPatch(config, { clearOnNull: true });

      set((state) => updateSessionState(state, agentId, sessionId, sessionPatch));

      // Title is applied by the mapper above (`sessionConfigToPatch` reads
      // `config.title`), exactly like model/provider — no separate write path
      // into agentStore.

      // Workspace selection is owned by workspaceStore, not SessionChatState.
      if (typeof config.workspace_id === "string" && config.workspace_id) {
        useWorkspaceStore.getState().setSessionWorkspaceLocal(sessionId, config.workspace_id);
      }
    } catch (e) {
      log.warn("[ChatStore] fetchSessionConfig failed:", e);
    }
  },

  // ADR-047 section 3.5.2: Combined cold-load function.
  //
  // MUST be used in all switch/open/first-start scenarios.
  // Encapsulates Promise.all of fetchSessionState + fetchSessionConfig
  // so callers cannot accidentally skip one (which would cause config
  // vacuum and bounce-back bug recurrence).
  loadSession: async (agentId: string, sessionId: string) => {
    await Promise.all([
      get().fetchSessionState(agentId, sessionId),
      get().fetchSessionConfig(agentId, sessionId),
    ]);
  },

  setSessionTitleLocal: (agentId: string, sessionId: string, title: string) => {
    // Guard: the overlay is read live-first by the tab / status card, so an
    // empty write would shadow the authoritative `sessions[]` snapshot with
    // the "Untitled" fallback. Title changes are always non-empty.
    if (!title.trim()) return;
    set((state) => updateSessionState(state, agentId, sessionId, { title }));
  },

  seedOptimisticTitle: (agentId: string, sessionId: string, userMessage: string) => {
    if (getSessionState(get(), agentId, sessionId).title?.trim()) return; // earlier overlay wins
    // The `sessions[]` snapshot is equally authoritative: the backend
    // generates a session's title ONCE (first turn) and never re-pushes it,
    // so after a restart / `clearAgentSessions` wiped the overlay, a titled
    // snapshot row must block re-seeding — otherwise the truncated latest
    // message would shadow the real title with no later push to heal it.
    const snapshot = useAgentStore
      .getState()
      .agents[agentId]?.sessions.find((s) => s.session_id === sessionId)?.title;
    if (snapshot?.trim()) return;
    const derived = deriveSessionTitle(userMessage);
    if (!derived) return;
    set((state) => updateSessionState(state, agentId, sessionId, { title: derived }));
  },
}));

/**
 * Keep pulling older pages until the loaded window starts on a turn
 * boundary (its oldest raw entry is no longer part of an explore run).
 *
 * A page is measured in RAW entries, but the renderer folds consecutive
 * tool_call / tool_result / thought entries into ONE explore_group block
 * (`foldMessages`).  A single explore run can therefore span more raw
 * entries than one page — in which case a bare tail page folds to a LONE
 * explore block and every earlier message (notably the user message that
 * opened the turn) stays unloaded, so the chat appears to contain only the
 * explore block.
 *
 * Shared by every explicit "position the view at the tail" entry point:
 * `ensureLatestInCache` (session open / switch-back) and the adapter's
 * `scrollToBottom` (jump-to-latest button).  It is deliberately NOT wired
 * into `loadSessionMessages` itself: the routine tail refreshes
 * (`scheduleSendReconciliation`, the record_complete refresh) fire on a hot
 * path where expanding a mid-explore-run window would spam extra HTTP.
 */
export async function expandWindowToTurnBoundary(
  agentId: string,
  sessionId: string,
  limit: number,
): Promise<void> {
  const MAX_EXPAND_PAGES = 20; // ponytail: bound; a >1000-entry explore run is pathological
  for (let i = 0; i < MAX_EXPAND_PAGES; i++) {
    const cur = useChatStore.getState().getSessionState(agentId, sessionId);
    const first = cur.messages[0];
    if (!first || cur.messageOffset <= 0) break;
    if (
      first.type !== "tool_call" &&
      first.type !== "tool_result" &&
      first.type !== "thought"
    ) {
      break;
    }
    const prevOffset = Math.max(0, cur.messageOffset - limit);
    await useChatStore.getState().loadSessionMessages(agentId, sessionId, prevOffset, limit);
  }
}

// ── Conversation entry conversion ─────────────────────────────────────

/**
 * Convert an MQTT `record_complete` payload into a `ChatMessage` ready for
 * direct insertion into `messages[]`.
 *
 * ADR-050 post-C5 fix: record_complete carries the COMPLETE content for
 * assistant / thought / tool_call / tool_result records.  Instead of
 * waiting for an HTTP refresh to surface these, we write them directly
 * into `messages[]` so the user sees the finished message immediately.
 *
 * The MQTT payload mirrors the JSONL entry fields (role / message_id /
 * content / tool_name / tool_call_id / is_error / seq) but is NOT a
 * `ConversationEntry` - it lacks `ts` and `metadata`.  We synthesize a
 * `timestamp` from `Date.now()` (clamped to be strictly greater than the
 * last cached message's timestamp so `foldMessages` sort order is
 * preserved) and reconstruct the `toolData` / `toolStatus` fields from
 * the flat MQTT fields.
 */
function convertRecordCompleteToChatMessage(
  data: Record<string, unknown>,
  agentId: string,
  lastTimestamp: number,
  // ADR-050 post-C5 fix: when set, these are written onto thought
  // messages so the renderer (PairedExploreItem / StreamingSourceBlock)
  // can distinguish a completed thought (`endTime` set) from a still-
  // streaming one.  Without `endTime`, a thought written to messages[]
  // by `record_complete` was indistinguishable from an in-flight stream
  // (PairedExploreItem's `isStreaming && !item.msg.endTime` evaluated
  // true forever, so the block stayed expanded and never auto-folded).
  // The `startTime` is sourced from chatAdapterStore's liveBuffer
  // `thinkingStream` entry (set when the first `stream_delta` for that
  // messageId landed).  The `endTime` falls back to `Date.now()` if the
  // MQTT payload doesn't carry an explicit end timestamp.
  thoughtTiming?: { startTime?: number | null; endTime?: number | null },
): ChatMessage {
  const role = data.role as string;
  const msgId = data.message_id as string;
  const content = (data.content as string) ?? "";
  const toolName = data.tool_name as string | undefined;
  const toolCallId = data.tool_call_id as string | undefined;
  const isError = data.is_error as boolean | undefined;
  const seq = data.seq as number | undefined;

  const agentInfo = getAgentSenderInfo(agentId);

  const msg: ChatMessage = {
    id: msgId,
    type: (role === "think" ? "thought" : role) as ChatMessage["type"],
    content,
    // Clamp to lastTimestamp + 1 so the new entry sorts AFTER every
    // cached message in foldMessages' timestamp-ascending sort.
    timestamp: Math.max(Date.now(), lastTimestamp + 1),
    senderDisplayName: agentInfo.senderDisplayName,
    senderRole: agentInfo.senderRole,
  };
  if (seq != null) msg.seq = seq;

  if (role === "tool_call" || role === "tool_result") {
    msg.toolName = toolName;
    msg.toolCallId = toolCallId;
    msg.toolData = {
      tool_name: toolName,
      tool_call_id: toolCallId,
      is_error: isError,
    } as Record<string, unknown>;
    if (role === "tool_result") {
      msg.toolStatus = isError ? "error" : "success";
    }
  }

  // ADR-050 post-C5 fix: stamp startTime/endTime on completed thoughts so
  // the rendering layer can auto-fold them.  See comment on the
  // `thoughtTiming` parameter above for the root-cause analysis.
  if (role === "think" || role === "thought") {
    if (thoughtTiming?.startTime != null) msg.startTime = thoughtTiming.startTime;
    if (thoughtTiming?.endTime != null) msg.endTime = thoughtTiming.endTime;
    else msg.endTime = Date.now();
  }

  return msg;
}

/** Strip leading/trailing `<summary>...</summary>` tags from a compaction
 *  summary string (the LLM is instructed to wrap output in those tags;
 *  we don't need them in the UI). Returns the inner text trimmed. If the
 *  tags aren't present, returns the original input trimmed. */
function stripSummaryTags(text: string): string {
  const trimmed = text.trim();
  const match = trimmed.match(/^<summary>([\s\S]*?)<\/summary>$/i);
  return (match ? match[1] : trimmed).trim();
}

function convertConversationEntry(entry: ConversationEntry, agentId: string): ChatMessage | null {
  // Internal runtime entries (e.g. the output-budget nudge the Runtime
  // appends to history and persists for replay fidelity) are kept out of the
  // chat transcript. They are never broadcast over MQTT, so this only affects
  // the JSONL history-load path.
  if (entry.metadata?.internal === true) return null;

  // Legacy intent-rotation handoff note (mechanism retired — the Runtime
  // no longer writes these). Transcripts recorded while it existed carry
  // the row, and it was context for the model, never for the user: it
  // stays in the JSONL but must not become a chat bubble.
  if (entry.metadata?.rotation_handoff === true) return null;

  // Compaction events: rendered as a folded summary card. Mirrors the
  // backend `kind="compaction"` JSONL marker. Detected BEFORE role-based
  // mapping because the underlying role is "system" but we render it
  // distinctly.
  if (entry.kind === "compaction") {
    const meta = (entry.metadata ?? {}) as Record<string, unknown>;
    const agentInfo = getAgentSenderInfo(agentId);
    return {
      id: entry.id,
      type: "compaction",
      content: stripSummaryTags(entry.content),
      timestamp: new Date(entry.ts).getTime(),
      senderDisplayName: agentInfo.senderDisplayName,
      senderRole: agentInfo.senderRole,
      compactionMeta: {
        compacted_from_id: meta.compacted_from_id as string | undefined,
        compacted_to_id: meta.compacted_to_id as string | undefined,
        level: (meta.level as number) ?? 0,
        model: meta.model as string | undefined,
        before_tokens: (meta.before_tokens as number) ?? 0,
        after_tokens: (meta.after_tokens as number) ?? 0,
      },
    };
  }

  const base: ChatMessage = {
    id: entry.id,
    type: (entry.role === "think" ? "thought" : entry.role) as ChatMessage["type"],
    content: entry.content,
    timestamp: new Date(entry.ts).getTime(),
    isStreaming: entry.is_streaming,
  };

  if (entry.role === "user") {
    const userInfo = getUserSenderInfo();
    base.senderDisplayName = userInfo.senderDisplayName;
  } else if (entry.role === "assistant" || entry.role === "think" || entry.role === "thought" || entry.role === "tool_call" || entry.role === "tool_result") {
    const agentInfo = getAgentSenderInfo(agentId);
    base.senderDisplayName = agentInfo.senderDisplayName;
    base.senderRole = agentInfo.senderRole;
  }

  const meta = entry.metadata;
  if (!meta) return base;

  // ADR-046 §2.5: 5 attachment-type system entries. Each carries a
  // `metadata.type` discriminator matching the backend `AttachmentMeta`
  // serde tag. The raw metadata is stored on `ChatMessage.metadata` so
  // the renderer (MessageBubble) can read it without re-parsing.
  const ATTACHMENT_TYPES = new Set([
    "file_upload",
    "image_upload",
    "attached_file",
    "attached_selection",
    "attached_folder",
  ]);
  if (entry.role === "system" && typeof meta.type === "string" && ATTACHMENT_TYPES.has(meta.type)) {
    base.metadata = meta as Record<string, unknown>;
    return base;
  }

  if (entry.role === "tool_call" || entry.role === "tool_result") {
    base.toolName = meta.tool_name as string | undefined;
    // ADR-035: tool_call_id is the backend's authoritative pairing key
    // (see ExploreBlock.buildPairedItems). It MUST be loaded from JSONL so
    // that historical sessions pair tool_call ↔ tool_result correctly even
    // before any live MQTT events arrive.
    base.toolCallId = meta.tool_call_id as string | undefined;
    base.toolData = meta as Record<string, unknown>;
    if (entry.role === "tool_result") {
      base.toolStatus = meta.success === false ? "error" : "success";
    }
  }

  if (entry.role === "think" || entry.role === "thought") {
    base.startTime = (meta.startTime as number) ?? undefined;
    base.endTime = (meta.endTime as number) ?? undefined;
  }

  return base;
}

/**
 * Map raw ConversationEntry list to ChatMessage[] — replaces the legacy
 * `mergeDocumentUploads` shim. Post-ADR-046 the runtime no longer writes
 * a separate `document_upload` system entry or appends enriched document
 * text into user content; everything is already inline as
 * `metadata.attached_items` on the user entry itself, and `convertConversationEntry`
 * rehydrates that into `ChatMessage.metadata`. So this function is
 * now a thin map. Kept as a named helper so the callsite stays self-explanatory.
 */
function mergeDocumentUploads(entries: ConversationEntry[], agentId: string): ChatMessage[] {
  return entries
    .map((e) => convertConversationEntry(e, agentId))
    .filter((m): m is ChatMessage => m !== null);
}

// ── WebSocket event handler — routes by event.session_id ──────────────

const CONTENT_EVENT_TYPES = new Set([
  "done", "error", "tool_approval_needed", "ask_question",
  "context_usage", "session_state", "stopped", "todo_list_updated",
  "compacting_started", "compacting_ended", "compaction_cancelled",
  "model_confirmed", "reasoning_effort_confirmed",
  "reasoning_started", "reasoning_ended",
  "memory_updated", "skill_executed",
  "session_config", "session_state",
  "stream_delta", "record_complete",
  "tool_progress",
  "event_cleared",
]);


/**
 * Process a single MQTT agent-event payload. Exported for unit testing;
 * production code should route events through the MQTT listener.
 */
export function handleMessageEvent(
  data: Record<string, unknown>,
  set: (fn: Partial<ChatStore> | ((state: ChatStore) => Partial<ChatStore>)) => void,
  get: () => ChatStore,
  agentId: string,
) {
  const eventType = data.type as string;
  // For content events: route to the session specified by event.session_id
  // If no session_id in event, fall back to the agent's active session.
  // This is the core fix: events go directly to their owning session,
  // NOT filtered by currentSessionId. Background sessions receive their
  // events correctly; non-active sessions just don't get rendered.
  let sid: string | null = null;

  if (CONTENT_EVENT_TYPES.has(eventType)) {
    const eventSessionId = data.session_id as string | undefined;
    if (eventSessionId != null) {
      sid = eventSessionId;
    } else {
      // Backward compat: no session_id → use active session
      sid = getAgentState(get(), agentId).activeSessionId;
    }
    if (!sid) return;

    // Ensure the session state entry exists
    const agent = getAgentState(get(), agentId);
    if (!agent.sessionStates[sid]) {
      set((state) => ({
        ...updateSessionState(state, agentId, sid!, { lastAccessed: Date.now() }),
      }));
    }
  }

  switch (eventType) {
    case "connected":
      break;

    case "ack":
      break;

    case "stop_received":
      // Gateway acknowledges that the stop request was received and
      // forwarded to the Runtime.  This is NOT a state transition —
      // the Runtime may still be streaming.  The real "stopped" event
      // arrives later via the bridge channel after the Runtime actually
      // processes the interrupt.
      break;

    // ADR-050 C2: stream_delta absorption lives in `chatAdapterStore`.
    // chatStore no longer mutates per-session streaming state — the
    // adapter owns the activeStream tracker, throttle timestamps,
    // isThinking/isAssistantReplying flags, and the trailing preview
    // content.  This handler is a one-line forwarder that hands the
    // raw `lines` array to the adapter's ingest entry point.
    case "stream_delta": {
      if (!sid) break;
      const lines = (data.lines as Array<{role:string;message_id:string;line_no:number;content:string}>) ?? [];
      log.debug("[ChatStore:DEBUG] stream_delta RECEIVED (forwarded to adapter)", {
        sid,
        eventSessionId: data.session_id,
        msgId: lines[0]?.message_id,
        role: lines[0]?.role,
        lineCount: lines.length,
        seq: data.seq,
      });
      if (!lines.length) break;
      ingestStreamDelta(agentId, sid, lines);
      break;
    }

    case "record_complete": {
      if (!sid) break;
      log.debug("[ChatStore:DEBUG] record_complete RECEIVED", {
        sid,
        eventSessionId: data.session_id,
        msgId: data.message_id,
        role: data.role,
        seq: data.seq,
        contentLen: typeof data.content === 'string' ? data.content.length : 0,
      });
      const rawRole = data.role as string;
      const role = (rawRole === 'assistant' || rawRole === 'thought' || rawRole === 'tool_call' || rawRole === 'tool_result')
        ? rawRole as 'assistant' | 'thought' | 'tool_call' | 'tool_result'
        : 'assistant';
      const msgId = data.message_id as string;
      const toolCallId = (data.tool_call_id as string | undefined) ?? '';

      // ADR-050 post-C5 fix: record_complete carries the COMPLETE content.
      // Write it directly into messages[] when atTail so the user sees the
      // finished message immediately, without waiting for an HTTP refresh.
      // This closes the "tool_call/tool_result not visible during streaming"
      // gap and eliminates the need for scheduleRefresh.
      //
      // atTail uses the shared `isAtTail()` function (same definition as
      // chatListAdapter's buildSnapshot) so that `limit === 0` (fresh
      // session before initial HTTP load) is also treated as at-tail, and
      // the message will be deduped by mergeMessageWindow when the HTTP
      // response eventually lands.  The write is additionally gated on a
      // NON-STALE cache (see the `atTail` expression below): `limit === 0`
      // because the window is genuinely empty-yet-authoritative is fine; a
      // cache explicitly marked stale (post-clear) is not.
      {
        const ss = getSessionState(get(), agentId, sid);
        // Only write directly into messages[] when the cache is a
        // contiguous, server-authoritative tail window.  `messagesStale`
        // marks the opposite: after clearSessionMessages (session
        // switch-away) the cache is empty and the cursor is meaningless, so
        // appending here would bump messageLimit with offset pinned at 0 —
        // fabricating a cursor that claims to cover the whole conversation
        // while the array only holds the freshly streamed tail records.
        // The next HTTP window load would then union that lie into a bogus
        // `[0, total)` cursor (hasOlder=false) and hide the older history.
        // While stale we simply drop the write: the switch-back reload
        // restores the record from disk (it is already persisted).
        const atTail =
          isAtTail(ss.messageOffset, ss.messageLimit, ss.messageTotal) &&
          !ss.messagesStale;
        if (atTail) {
          const lastTs = ss.messages.length > 0
            ? ss.messages[ss.messages.length - 1].timestamp
            : 0;
          // ADR-050 post-C5 fix: when this record_complete closes a thought,
          // stamp startTime/endTime on the converted message so the renderer
          // can auto-fold the thought block.  startTime comes from the
          // adapter's liveBuffer thinkingStream entry (set when the matching
          // stream_delta first landed).  We read it from there rather than
          // from `thinkingStartTime`, because the latter is a legacy
          // projection that is only reset on the next thought's first delta
          // and can leak a stale value from a previous thought cycle.
          // endTime defaults to "now" — close enough for the duration display,
          // which is rounded to seconds.
          let thoughtTiming: { startTime?: number | null; endTime?: number | null } | undefined;
          if (role === "thought") {
            const adapter = getChatAdapterSession(agentId, sid);
            const liveThought = adapter.liveBuffer.thinkingStream;
            thoughtTiming = {
              startTime: liveThought?.id === msgId ? liveThought.startTime : null,
              endTime: Date.now(),
            };
          }
          const chatMsg = convertRecordCompleteToChatMessage(data, agentId, lastTs, thoughtTiming);
          set((state) => {
            const ss2 = getSessionState(state, agentId, sid!);
            // Dedup: if the id is already in messages[] (e.g. MQTT QoS 1
            // duplicate delivery, or HTTP refresh already landed), skip.
            if (ss2.messages.some(m => m.id === msgId)) return {};
            return updateSessionState(state, agentId, sid!, {
              messages: [...ss2.messages, chatMsg],
              messageTotal: ss2.messageTotal + 1,
              messageLimit: ss2.messageLimit + 1,
            });
          });
        }
      }

      // Clear the corresponding liveBuffer stream (thinkingStream /
      // assistantStream).  Only clears - does NOT push into
      // pendingRecordComplete (that concept is removed in the post-C5 fix).
      ingestRecordComplete(agentId, sid, { messageId: msgId, role });

      // ADR-045: Clean up toolProgress entry when a tool_result arrives.
      if (role === 'tool_result') {
        set((state) => {
          const agent = getAgentState(state, agentId);
          const ss = agent.sessionStates[sid!];
          if (!ss) return {};
          const keys = Object.keys(ss.toolProgress);
          if (keys.length === 0) return {};
          const updated = { ...ss.toolProgress };
          if (toolCallId && updated[toolCallId]) {
            delete updated[toolCallId];
          } else {
            // No exact match - clear any entry whose elapsedMs is already
            // >= 90% of its timeout (clearly stale, safe to drop).
            for (const k of keys) {
              const e = updated[k];
              if (e && e.elapsedMs / Math.max(e.timeoutMs, 1) >= 0.9) {
                delete updated[k];
              }
            }
            // If we still have nothing dropped and toolCallId was missing,
            // clear the single oldest entry (deterministic FIFO).
            if (toolCallId === '' && Object.keys(updated).length === keys.length) {
              let oldestId: string | null = null;
              let oldestElapsed = -1;
              for (const k of keys) {
                const e = updated[k];
                if (e && e.elapsedMs > oldestElapsed) {
                  oldestElapsed = e.elapsedMs;
                  oldestId = k;
                }
              }
              if (oldestId) delete updated[oldestId];
            }
          }
          return updateSessionState(state, agentId, sid!, {
            toolProgress: updated,
          });
        });
      }
      break;
    }

    case "done": {
      if (!sid) break;
      const usage = data.usage as TokenUsage | undefined;

      // ADR-025: The done event marks that the backend has finished
      // processing the run.  Do NOT start a fire-and-forget transcript
      // re-fetch here — that was the root cause of the missing-segments
      // bug, and the race still applies to any tail fetch:
      //
      // it increments loadSequence, aborts an in-flight load, and its own
      // response may arrive before the backend has flushed the last JSONL
      // lines.  The old incremental poll then saw `streaming: null` and
      // dropped ALL streaming placeholders — but their JSONL replacements
      // hadn't arrived yet, so the last few toolcall/thought/assistant
      // segments vanished.
      //
      // Transcript reconciliation is owned by `realignOnDivergence`
      // (ADR-050 §16.3), which is keyed on the authoritative
      // `message_count` in the session_state snapshot rather than on this
      // event, and merges by message id instead of replacing the window.
      set((state) => {
        const ss = getSessionState(state, agentId, sid!);
        return {
          ...updateSessionState(state, agentId, sid!, {
            tokenUsage: usage ?? ss.tokenUsage,
            isCompacting: false,
          }),
        };
      });
      break;
    }

    case "model_confirmed": {
      const confirmedModel = data.model as string;
      const confirmedProvider = data.provider as string | undefined;
      log.debug("[ChatStore] Model switch confirmed:", confirmedModel, confirmedProvider);
      if (confirmedModel && sid) {
        // Resolve new model's default reasoning effort
        // Same lookup as setCurrentModel -- see resolveDefaultReasoningEffort.
        const defaultEffort = resolveDefaultReasoningEffort(get().availableModels, confirmedModel, confirmedProvider ?? "");

        // Update session model (current session only)
        set((state) => updateSessionState(state, agentId, sid!, {
          model: confirmedModel,
          provider: confirmedProvider ?? "",
          reasoningEffort: defaultEffort,
        }));
        // Update agent's default model (new sessions inherit this)
        set((state) => updateAgentState(state, agentId, {
          preferredModel: confirmedModel,
          preferredProvider: confirmedProvider ?? null,
        }));
      }
      break;
    }

    case "reasoning_effort_confirmed": {
      const confirmedEffort = data.effort as string;
      log.debug("[ChatStore] Reasoning effort confirmed:", confirmedEffort);
      if (confirmedEffort && sid) {
        set((state) => updateSessionState(state, agentId, sid!, {
          reasoningEffort: confirmedEffort,
        }));
      }
      break;
    }

    case "reasoning_started": {
      if (!sid) break;
      set((state) => updateSessionState(state, agentId, sid!, { isReasoning: true }));
      break;
    }

    case "reasoning_ended": {
      if (!sid) break;
      set((state) => updateSessionState(state, agentId, sid!, { isReasoning: false }));
      break;
    }

    case "error": {
      if (!sid) break;
      // Backend sends user_message as content, plus detail and error_type
      const errorMsg = (data.content ?? data.message) as string;
      const errorDetail = (data.detail) as string | undefined;
      const errorType = (data.error_type) as string | undefined;
      log.error("[ChatStore] Server error:", errorMsg, errorDetail);
      // ADR-035 Phase 3: no polling
      // Server errors are NOT persisted to JSONL by the backend, so they
      // must NOT enter the messages array (sliding window over JSONL).
      // Store as a separate banner state instead.
      set((state) => ({
        ...updateSessionState(state, agentId, sid!, {
          serverError: {
            content: errorMsg as string,
            errorDetail: errorDetail || undefined,
            errorType: errorType || undefined,
            timestamp: Date.now(),
          },
          isCompacting: false,
        }),
      }));
      break;
    }

    case "stopped": {
      if (!sid) break;
      // ADR-035 Phase 3: no polling
      set((state) => ({
        ...updateSessionState(state, agentId, sid!, {
          isCompacting: false,
          }),
      }));
      break;
    }

    case "tool_approval_needed": {
      if (sid) {
        const approvalEvent = data as unknown as ToolApprovalNeededEvent;
        set((state) => {
          const agentState = state.agentStates[agentId];
          const prevPending = agentState?.sessionStates[sid]?.pendingApproval || {};
          const key = approvalEvent.tool_call_id || approvalEvent.request_id;
          const newPending = { ...prevPending, [key]: approvalEvent };
          return updateSessionState(state, agentId, sid, {
            pendingApproval: newPending,
          });
        });
      } else {
        log.warn("[ChatStore] tool_approval_needed dropped - session_id is null", {
          agentId,
          tool_call_id: data.tool_call_id,
          request_id: data.request_id,
        });
      }
      break;
    }

    case "ask_question":
      if (sid) {
        set((state) => {
          const current = getSessionState(state, agentId, sid).pendingQuestions ?? [];
          // Avoid duplicates if the same request_id arrives twice (MQTT QoS)
          if (current.some((q) => q.request_id === (data as unknown as AskQuestionEvent).request_id)) {
            return state;
          }
          return updateSessionState(state, agentId, sid, {
          pendingQuestions: [...current, data as unknown as AskQuestionEvent],
          });
        });
      }
      break;

    // Synthetic event from the Tauri MQTT bridge: the Runtime cleared a
    // retained blocking-event slot (ask_question / tool_approval_needed)
    // by publishing a zero-byte payload with retain=true. The MQTT broker
    // re-delivers the empty payload to subscribers, the bridge cannot
    // decode it as DataEnvelope, so it forwards it as `event_cleared`.
    // We drop every card of the cleared event_type because the retained
    // slot holds at most one in-flight blocking event per session —
    // the timeout path AND the user-answered path both arrive here, so
    // a stale card always disappears on its own.
    // ponytail: clears ALL cards of the event_type, not a specific
    // request_id — the ClearRetainedEvent carries no correlation id.
    // Correct while the loop is strictly sequential (one in-flight
    // blocking event per session); upgrade path: carry request_id in the
    // clear event when parallel blocking events are ever supported.
    case "event_cleared":
      if (sid) {
        const clearedType = (data as unknown as EventClearedEvent).event_type;
        if (clearedType === "ask_question") {
          set((state) => updateSessionState(state, agentId, sid, { pendingQuestions: [] }));
        } else if (clearedType === "tool_approval_needed") {
          set((state) => updateSessionState(state, agentId, sid, { pendingApproval: {} }));
        } else {
          // Unknown event_type — forward-compatible no-op.
          log.debug("[ChatStore] event_cleared with unknown event_type:", clearedType);
        }
      }
      break;

    case "memory_updated":
      log.debug("[WS] Memory updated event:", data);
      break;

    case "skill_executed":
      log.debug("[WS] Skill executed event:", data);
      break;

    case "compacting_started":
      if (sid) {
        set((state) => updateSessionState(state, agentId, sid, { isCompacting: true }));
      }
      break;

    case "compacting_ended":
      if (sid) {
        set((state) => updateSessionState(state, agentId, sid, { isCompacting: false }));
        // ADR-025: One-shot poll to fetch the compaction record. No
        // coordinates — backend delivery cursor handles it.
        get().loadSessionMessages(agentId, sid);  // ADR-035 Phase 3: no incremental
      }
      break;

    // ADR-083: the compaction ended without a summary (user cancel, deadline
    // expiry, or every distill tier failed). Reset the dual-state button —
    // `compacting_ended` is NOT sent on this path — and surface why.
    case "compaction_cancelled": {
      if (sid) {
        set((state) => updateSessionState(state, agentId, sid, { isCompacting: false }));
      }
      const reason = data.reason as string | undefined;
      const message =
        reason === "timeout"
          ? i18n.t("contextUsage.compactionTimeout")
          : reason === "failed"
            ? i18n.t("contextUsage.compactionFailed")
            : i18n.t("contextUsage.compactionCancelled");
      showToast({ type: reason === "failed" ? "error" : "info", message });
      break;
    }

    case "embedding_migration_progress": {
      // Forward migration progress from WebSocket to gatewayStore.
      // agentId comes from the per-agent WebSocket connection context.
      const processed = data.processed as number;
      const total = data.total as number;
      if (processed != null && total != null) {
        useGatewayStore.getState().updateMigrationProgress(agentId, processed, total);
      }
      break;
    }

    case "tool_progress": {
      if (sid) {
        const toolCallId = data.tool_call_id as string;
        const elapsedMs = data.elapsed_ms as number;
        const timeoutMs = data.timeout_ms as number;
        if (toolCallId && elapsedMs != null && timeoutMs != null) {
          set((state) => {
            const agent = getAgentState(state, agentId);
            const ss = agent.sessionStates[sid];
            if (!ss) return {};
            const updated = { ...ss.toolProgress };
            updated[toolCallId] = { elapsedMs, timeoutMs };
            return updateSessionState(state, agentId, sid, {
              toolProgress: updated,
            });
          });
        }
      }
      break;
    }

    case "context_usage": {
      if (sid) {
        // ADR-033: Runtime now publishes a fully-populated `ContextUsageInfo`
        // under `data.context_usage` (serialised from `ContextUsagePayload.context_usage`).
        // The top-level `input_tokens` / `output_tokens` / `total_*_tokens` fields
        // are legacy per-turn counts only and must NOT be used to populate
        // `ContextUsageInfo` — they lack `context_window` / `total_tokens` /
        // `usage_percent` / `usable_context`, which the StatusBar reads.
        const nested = (data as Record<string, unknown>).context_usage;
        const usage: ContextUsageInfo | null =
          nested && typeof nested === "object"
            ? (nested as ContextUsageInfo)
            : null;
        if (usage) {
          log.debug("[ChatStore] context_usage RECEIVED for agent:", agentId, usage);
          set((state) => updateSessionState(state, agentId, sid, { contextUsage: usage, isCompacting: false }));
        } else {
          log.warn(
            "[ChatStore] context_usage event missing nested context_usage payload — skipping StatusBar update to avoid undefined fields:",
            data,
          );
        }
      }
      break;
    }

    case "iteration_limit_paused": {
      // ADR-014: Pause UX is derived from session_state (Paused detail with
      // reason/message). This transient event is intentionally ignored —
      // the frontend no longer mirrors pause flags from separate channels.
      break;
    }

    case "loop_detected_paused": {
      // ADR-014: same as iteration_limit_paused — derived from session_state.
      break;
    }

    // Todo list updated — from todo_write built-in tool
    case "todo_list_updated": {
      if (sid) {
        const todos = data.todos as TodoItem[] | undefined;
        if (todos) {
          set((state) => updateSessionState(state, agentId, sid, { todos }));
        }
      }
      break;
    }

    // Session lifecycle (Runtime → Desktop, ADR-033 Phase 4).
    // Backend publishes `acowork/agents/{id}/sessions/{created|deleted}`
    // protobuf events whenever a session is created or destroyed (close/delete).
    // We refresh the session list so the UI reflects the change immediately,
    // and auto-activate a freshly created session so the user's "+" button click
    // lands them on the new chat without a follow-up click.
    case "session_created": {
      const newSessionId = data.session_id as string | undefined;
      if (!newSessionId) break;
      const beforeActive = getAgentState(get(), agentId).activeSessionId;
      log.info("[ChatStore] session_created received", {
        newSessionId,
        beforeActive,
        beforeOpenSessionIds: getAgentState(get(), agentId).openSessionIds,
      });
      const agentStore = useAgentStore.getState();
      // Refresh the list first so the new entry is rendered.
      agentStore.fetchSessions(agentId).catch((e) => {
        log.warn("[ChatStore] fetchSessions after session_created failed:", e);
      });
      // ADR-038: a freshly-created session is Active by definition (the
      // backend has just spawned the session task), so we use strict
      // `setActiveTab` (no openSessionIds mutation, no MQTT round-trip)
      // to flip the foreground tab.  Any caller that initiated the
      // creation must have already called `createSession`, which itself
      // routes through the same MQ-backed lifecycle — so the session is
      // guaranteed to be in `openSessionIds` and Active by the time
      // this event lands.
      agentStore.activateNewlyCreatedSession(newSessionId, agentId);
      // Diagnostic: log AFTER state so we can verify the activation ran.
      // setTimeout(0) ensures the synchronous set() inside openSession
      // has settled before we read state.
      setTimeout(() => {
        const after = getAgentState(useChatStore.getState(), agentId);
        log.info('[ChatStore] session_created handled', {
          newSessionId,
          afterActive: after.activeSessionId,
          afterOpenSessionIds: after.openSessionIds,
        });
      }, 0);
      break;
    }

    // ADR-038: Runtime ack that an OpenSession command succeeded.
    // Topic: acowork/agents/{id}/sessions/{sid}/opened (Retained).
    // Flips `isSessionReady` so the UI unlocks the input box and
    // applies the runtime's authoritative model/provider/last_active_at
    // (defensive — applySessionMeta already covers most of this from
    // session_state events, but having both paths makes the
    // contract observable in the store alone).
    case "session_opened": {
      const sid = data.session_id as string | undefined;
      if (!sid) break;
      const status = data.status as string | undefined;
      const model = typeof data.model === "string" && data.model ? data.model : null;
      const provider = typeof data.provider === "string" && data.provider ? data.provider : null;
      const lastActiveAt =
        typeof data.last_active_at === "string" && data.last_active_at ? data.last_active_at : null;
      log.debug(
        `[ChatStore] session_opened for ${agentId}/${sid}: status=${status ?? "?"}, ` +
        `model=${model ?? "?"}, provider=${provider ?? "?"}, last_active_at=${lastActiveAt ?? "?"}`,
      );
      set((state) => {
        const agent = getAgentState(state, agentId);
        const newSessionStates = { ...agent.sessionStates };
        const existing = newSessionStates[sid] ?? {
          ...makeInitialSessionState(agent),
          lastAccessed: Date.now(),
        };
        newSessionStates[sid] = {
          ...existing,
          isSessionReady: true,
          ...(model ? { model } : {}),
          ...(provider ? { provider } : {}),
          lastAccessed: Date.now(),
        };
        return updateAgentState(state, agentId, { sessionStates: newSessionStates });
      });
      break;
    }

    // ADR-038: Runtime error when the session is not Active.
    // Topic: acowork/agents/{id}/sessions/{sid}/not_opened.
    // We surface this to the UI as a warning toast with a one-click
    // "reopen" affordance so the user can recover from the rare case
    // where the frontend issued chat_message / model_switch without a
    // preceding open_session (e.g. an older client, or a bug elsewhere
    // in the lifecycle). The backend stays the source of truth — when
    // the contract is violated, we make it visible.
    case "session_not_opened": {
      const sid = data.session_id as string | undefined;
      if (!sid) break;
      const reason = (data.reason as string | undefined) ?? "session_closed";
      const attempted = (data.attempted_command as string | undefined) ?? "?";
      log.warn(
        `[ChatStore] session_not_opened for ${agentId}/${sid}: ` +
        `attempted=${attempted}, reason=${reason}`,
      );
      set((state) => {
        const agent = getAgentState(state, agentId);
        const existing = agent.sessionStates[sid];
        if (!existing) return {};
        return updateSessionState(state, agentId, sid, {
          isSessionReady: false,
        });
      });
      // Only toast if this is the active session — background tabs
      // hitting a state error don't deserve a modal callout.
      const activeSid = getAgentState(get(), agentId).activeSessionId;
      if (activeSid === sid) {
        showToast({
          type: "warning",
          message: `Session is not open (${reason}). Reopen it to continue.`,
          action: {
            label: "Reopen",
            onClick: () => {
              void useChatStore.getState().openSession(agentId, sid);
            },
          },
        });
      }
      break;
    }

    case "session_deleted": {
      // Refresh the session list so the deleted entry disappears from the UI.
      const deletedSessionId = data.session_id as string | undefined;
      if (!deletedSessionId) break;
      const agentStore = useAgentStore.getState();
      agentStore.fetchSessions(agentId).catch((e) => {
        log.warn("[ChatStore] fetchSessions after session_deleted failed:", e);
      });
      break;
    }

    // ── Session config (ADR-043: user-configurable fields only) ──
    case "session_config": {
      log.debug("[ChatStore:DEBUG] session_config RECEIVED", {
        sid,
        agentId,
        model_id: data.model_id,
        provider_id: data.provider_id,
        workspace_id: data.workspace_id,
        reasoning_effort: data.reasoning_effort,
        temperature: data.temperature,
        title: data.title,
      });
      if (!sid) break;
      // MQTT session_config envelope uses model_id / provider_id and encodes
      // "no override" as "" for strings / NaN for floats (prost can't
      // encode Option<T>). Normalize to the unified SessionConfigInput
      // shape and delegate to the mapper. clearOnNull: true because the
      // retained session_config is a full snapshot - every field is
      // always present, and a null/empty value means "session has no
      // override" which must clear any stale value from the UI.
      const mqttConfig: SessionConfigInput = {
        model: typeof data.model_id === "string" && data.model_id ? data.model_id : null,
        provider: typeof data.provider_id === "string" && data.provider_id ? data.provider_id : null,
        // Same key as the HTTP snapshot (`account_id`) — see
        // sessionConfigMapper.ts; only `model_id` / `provider_id` are renamed.
        account_id: typeof data.account_id === "string" && data.account_id ? data.account_id : null,
        reasoning_effort: typeof data.reasoning_effort === "string" && data.reasoning_effort ? data.reasoning_effort : null,
        temperature: typeof data.temperature === "number" && !Number.isNaN(data.temperature) ? data.temperature : null,
        // ADR-074: prost `optional uint64` presence → `number | null`.
        // `0` is invalid and never emitted; a missing field is null.
        context_window: typeof data.context_window === "number" && data.context_window > 0 ? data.context_window : null,
        // Title rides the same snapshot; the mapper treats it additively, so
        // it lands with or without a `sessions[]` row, and a not-yet-generated
        // empty value never clobbers an optimistic title.
        title: typeof data.title === "string" && data.title ? data.title : null,
      };
      const patch = sessionConfigToPatch(mqttConfig, { clearOnNull: true });
      if (Object.keys(patch).length > 0) {
        log.debug("[ChatStore:DEBUG] session_config applying patch", { sid, patch });
        set((state) => updateSessionState(state, agentId, sid!, patch));
      }
      // LLM availability is an agent-level (instance-level) signal, NOT
      // global: the Desktop subscribes to many agents on the same broker
      // and each Runtime publishes its own retained SessionConfig. A
      // per-agent field keeps one agent's reconnect race (see the
      // runtime `compute()` comment) from putting a misconfig banner
      // over every other agent's chat. `data.llm_availability` is the
      // protobuf wire field (i32 / enum string from JSON conversion).
      const nextAvail = llmAvailabilityFromWire(data.llm_availability);
      const curAvail = getAgentState(get(), agentId).llmAvailability;
      // `unspecified` means "this message carries no availability info"
      // (old runtime without the field, or a per-session config
      // re-publish that wasn't tagged). Never downgrade a known state
      // back to unspecified — that would hide the banner until the next
      // availability transition fires.
      if (nextAvail !== "unspecified" || curAvail === "unspecified") {
        if (nextAvail !== curAvail) {
          set((state) => updateAgentState(state, agentId, { llmAvailability: nextAvail }));
        }
      }
      // Workspace selection is owned by workspaceStore, not SessionChatState.
      if (typeof data.workspace_id === "string" && data.workspace_id) {
        log.debug("[ChatStore:DEBUG] session_config setting workspace", { sid, workspace_id: data.workspace_id });
        useWorkspaceStore.getState().setSessionWorkspaceLocal(sid, data.workspace_id);
      }
      break;
    }

    // ── Session state (ADR-043: runtime telemetry only) ──
    // Replaces the deleted "session_state" event. Carries status,
    // ratio, context_usage, message_count, tokens, and updated_at.
    // The retained topic ensures reconnecting clients immediately receive
    // the latest state.
    case "session_state": {
      if (sid) {
        const status = data.status as SessionStatus | undefined;
        if (status) {
          // ponytail: diagnostic — time the reducer to see if session_state
          // handling is what stalls the main thread on agent start.
          const __t0 = performance.now();
          const prev = getSessionState(get(), agentId, sid!);
          log.debug(
            `[ChatStore:DEBUG] session_state for ${agentId}/${sid}: ` +
            `prev=${prev.sessionStatus?.status} -> next=${status.status}, ` +
            `messageCount=${prev.messages.length}`,
          );
          set((state) => {
            const sessionPatch: Partial<SessionChatState> = { sessionStatus: status };

            // Runtime fields from SessionState proto.
            if (typeof data.ratio === "number") sessionPatch.ratio = data.ratio as number;
            if (data.context_usage && typeof data.context_usage === "object") {
              const prev = getSessionState(get(), agentId, sid)?.contextUsage;
              sessionPatch.contextUsage = mergeContextUsage(
                prev,
                data.context_usage as ContextUsageInfo,
              );
            }

            // ADR-099 (design 27 §5.5.5): mirror the backend pending-intent
            // queue so the execution UI can show chat-forwarded messages that
            // arrived while the session was busy. The push carries the full
            // current list, so always replace (empty list clears it).
            sessionPatch.pendingIntents = Array.isArray(data.pending_intents)
              ? (data.pending_intents as PendingIntent[])
              : [];

            // ADR-021: Start/stop polling based on status transitions.
            const prev = getSessionState(state, agentId, sid);

            // When status transitions TO Idle from non-Idle, clear pending flags.
            // ADR-050 C2: isAssistantReplying / isThinking / thinkingContent
            // live in chatAdapterStore now.  The adapter also owns the
            // activeStream tracker, so the "record_complete lost" recovery
            // path delegates to `releaseAdapterSession` to drop the stale
            // tracker; the HTTP realignment itself is owned by
            // `realignOnDivergence` below (see the ADR-035 note there).
            if (prev.sessionStatus?.status !== "idle" && status.status === "idle") {
              sessionPatch.pendingApproval = {};
              sessionPatch.pendingQuestions = [];

              // ADR-035 C2/O2: if activeStream still has unfrozen content at
              // idle, record_complete was lost (QoS edge case).  Drop the
              // stale tracker so it cannot re-inject half-frozen text.  The
              // bespoke tail re-fetch this branch used to fire is gone: a
              // lost record_complete leaves the local count short of the
              // server's, which is exactly the divergence
              // `realignOnDivergence` detects — one data-keyed rule instead
              // of a second lifecycle-keyed one.
              const adapter = getChatAdapterSession(agentId, sid);
              if (adapter.isThinking || adapter.isAssistantReplying) {
                log.warn(
                  `[ChatStore] ADR-035 C2: adapter still streaming at idle ` +
                  `(isThinking=${adapter.isThinking}, isAssistantReplying=${adapter.isAssistantReplying}) ` +
                  ` - record_complete likely lost, releasing tracker`,
                );
                releaseAdapterSession(agentId, sid);
              }
            }

            const sessionResult = updateSessionState(state, agentId, sid, sessionPatch);
            let agentStates = sessionResult.agentStates;
            // ponytail: diagnostic
            const __t1 = performance.now();
            if (__t1 - __t0 > 100) {
              console.warn(
                `[chatStore] session_state reducer took ${Math.round(__t1 - __t0)}ms ` +
                  `session=${sid} status=${status.status}`,
              );
            }
            return { agentStates };
          });

          // ADR-050 §16.3: data-driven transcript realignment.  Runs after
          // the reducer so the divergence test reads the patch we just
          // applied, and outside it because a fetch is a side effect.
          const serverTotal = data.message_count;
          if (typeof serverTotal === "number") {
            realignOnDivergence(agentId, sid, serverTotal);
          }
        }
      }
      break;
    }

    // ── Agent lifecycle: status, meta, config ──
    case "agent_status": {
      const aid = data.instance_id as string | undefined;
      const online = data.online as boolean | undefined;
      // ADR-085: `state` + `detail` ride the same status envelope —
      // zero new subscriptions. Unknown/missing state degrades to
      // "unspecified" (chat_mqtt.rs normalizes), never an error.
      const lifecycle = data.state as AgentLifecycleState | undefined;
      const detail = typeof data.detail === "string" ? data.detail : undefined;
      if (aid && online !== undefined) {
        // `alive` mirrors `online` — the MQTT payload is the protobuf
        // `DataEnvelope<AgentStatus>::online` (Sept 2026; auto-sleep was
        // retired so the only transitions are start/stop). The
        // `node_id` rides along for per-node grouping.
        useAgentStore.getState().updateAgentLiveness(aid, online, lifecycle, detail);
        // HTTP health double-check on MQTT disconnect (distributed liveness):
        // the Runtime may run on a remote node, and its MQTT connection can
        // drop (e.g. system sleep → KeepAlive timeout → Gateway marks the
        // agent offline) while the Runtime process itself stays alive.
        // Probe `/health` through the Gateway reverse-proxy; if the Runtime
        // answers 2xx it is alive, so override back to alive instead of
        // rendering it offline.
        if (!online) {
          // The trailing `.catch(() => {})` is defensive: in production
          // `verifyAgentHealth` always resolves (it has its own try/catch
          // around `fetch`), but if a future refactor accidentally lets a
          // rejection escape, we must not surface it as an unhandled
          // promise rejection — that would crash the desktop renderer.
          void verifyAgentHealth(aid)
            .then((alive) => {
              if (alive) {
                useAgentStore.getState().updateAgentLiveness(aid, true);
              }
            })
            .catch(() => {
              // Swallow — see comment above. The agent is correctly
              // marked offline; the chatStore will get another chance on
              // the next MQTT status tick or the next user action.
            });
        }
      }
      break;
    }

    case "agent_meta": {
      const aid = data.instance_id as string | undefined;
      if (aid) {
        // ADR-009 §V-Q: avatar / display name are resolved server-side
        // (Gateway `list_agents` merges the Runtime's per-instance
        // overrides). The Runtime meta payload carries no avatar, so
        // patching it here would clobber the effective value.
        useAgentStore.getState().patchAgentMeta(aid, {
          name: data.name as string | undefined,
          version: data.version as string | undefined,
        });
      }
      break;
    }

    case "agent_config": {
      const aid = data.instance_id as string | undefined;
      if (aid && typeof data.config_json === "string") {
        try {
          const config = JSON.parse(data.config_json);
          log.debug("[ChatStore] Agent config updated:", aid, config);
          // ADR-034 §7.6.4: Runtime republishes its merged AgentConfig
          // (manifest defaults + agent_config.json) as a retained MQTT
          // message on startup and after each PUT /api/agents/{id}/config.
          // The desktop Setup / Tools panels listen to
          // `acowork:refresh-agent-config` to re-fetch via the HTTP
          // reverse proxy. Without this emit, a Runtime-initiated
          // change (e.g. workspace_switch side effects) would silently
          // leave the Setup panel showing stale localStorage values.
          emitAgentConfigRefresh(aid);

          // Sync context_window to all cached session contextUsage.
          //
          // The agent_config MQTT event carries the full AgentConfig
          // (including context_window) as a retained message, published
          // immediately after PUT /api/agents/{id}/config.  However the
          // live contextUsage push (via session_state / messages/context_usage)
          // only fires when the agent loop runs.  Without this local sync,
          // the ContextUsageIcon, status bar, and RightPanel keep showing
          // the old context_window until the user sends a message and
          // triggers the loop.
          //
          // We update context_window and recalculate the derived fields
          // (usage_percent, usable_context) as a best-effort approximation.
          // The next agent-loop context_usage push will overwrite these
          // with the backend's exact computation (which accounts for model
          // capabilities and output token reservation).
          //
          // ADR-074 §1.4: sessions with a per-session override must be
          // SKIPPED here — their effective window comes from the session
          // override, not the agent window, and overwriting it would break
          // the "agent change never overrides a set session value" invariant.
          if (typeof config.context_window === "number" && config.context_window > 0) {
            const newWindow = config.context_window;
            set((state) => {
              const agent = state.agentStates[aid];
              if (!agent) return {};
              let changed = false;
              const updatedSessions = { ...agent.sessionStates };
              for (const [sid, sess] of Object.entries(updatedSessions)) {
                // ADR-074 §1.4 (blanket-sync fix): never clobber a session
                // that has its own override — it keeps its own window and
                // the runtime pushes the session-effective value.
                if (sess.sessionContextWindow != null) continue;
                if (!sess.contextUsage) continue;
                const cu = sess.contextUsage;
                const total = cu.total_tokens ?? 0;
                updatedSessions[sid] = {
                  ...sess,
                  contextUsage: {
                    ...cu,
                    context_window: newWindow,
                    usage_percent: Math.min(100, Math.round((total / newWindow) * 100)),
                    usable_context: Math.max(0, newWindow - total),
                  },
                };
                changed = true;
              }
              return changed ? updateAgentState(state, aid, { sessionStates: updatedSessions }) : {};
            });
          }
        } catch (e) {
          log.warn("[ChatStore] Failed to parse agent_config JSON:", e);
        }
      }
      break;
    }

    case "sidecar_status": {
      // Sidecar status is currently a debug feature without agent_id routing.
      // The listener at line 609 filters out events without agent_id, so this
      // branch is reached only if agent_id is added to the Rust forwarding.
      log.debug("[ChatStore] Sidecar status:", {
        kind: data.kind,
        endpoint: data.endpoint,
        ready: data.ready,
      });
      break;
    }

    case "memory_node_update": {
      log.debug("[ChatStore] Memory node update:", {
        node_id: data.node_id,
        instance_id: data.instance_id,
      });
      break;
    }

    default:
      log.debug("[ChatStore] Unknown event type:", eventType, data);
  }
}
