import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { BUILTIN_ICON_IDS } from "../components/common/UserAvatar";
import type { AgentInfo, AgentDetail, SessionInfo, SessionStatus, NodeInfo, AgentLifecycleState, LatestSessionResult } from "../lib/types";
import { instanceIdOf, isProcessing } from "../lib/types";
import { getGatewayUrl } from "../lib/config";
import { fetchNodes as fetchNodesApi } from "../lib/gateway-api";
import { useChatStore } from "./chatStore";
import { useAuthStore } from "./authStore";
import { useWorkspaceStore } from "./workspaceStore";
import { useFileTreeStore } from "./fileTree";
import { useProjectGroupStore } from "./projectGroupStore";
import { useChatSessionStore } from "./chatSessionStore";
import { log } from "../lib/logger";
import { with503Retry } from "../lib/httpRetry";
import * as sessionControl from "../lib/session-control";

// ── One-shot "agent came online" waiters (event-driven, no polling) ──────
// startAgent 等待 Runtime 上线：MQTT `agent_status online` → chatStore →
// `updateAgentLiveness(alive=true)` → 触发这里注册的回调。超时由调用方
// (startAgent) 负责 reject。key 为 instance id。
const onlineWaiters = new Map<string, Array<() => void>>();

function notifyAgentOnline(agentId: string): void {
  const waiters = onlineWaiters.get(agentId);
  if (waiters) {
    onlineWaiters.delete(agentId);
    for (const fn of waiters) fn();
  }
}

// ── Single-flight "resolve the active session" chain ─────────────────────
// The start orchestrator (lib/agent-start.ts) and the selectAgent /
// fetchAgents re-bind paths used to run their own copies of
// resolve → open concurrently, aborting each other's message loads. One
// in-flight resolution per agent is now shared by every caller; the map
// entry lives only while the chain runs.
const inflightResolve = new Map<string, Promise<ResolveActiveSessionResult>>();

/** ADR-085 D4 + ADR-076 §决策 4: terminal outcome of `resolveActiveSession`. */
export type ResolveActiveSessionResult = "noop" | "opened" | "created" | "unavailable";

// ══════════════════════════════════════════════════════════════════════════
// AgentProfile types (moved from agentProfileStore.ts)
// ══════════════════════════════════════════════════════════════════════════

export interface AgentProfileSettings {
  /** @deprecated ADR-017 — avatar is now server-side (agent_config.json).
   *  Kept for backward compat with existing localStorage profiles. */
  avatarIconId?: string | null;
  modelId?: string;
  providerId?: string;
  maxTokens?: number;
  maxIterations?: number;
  maxSessions?: number;
  systemPrompt?: string;
  shellApprovalThreshold?: string;
  approvalTimeoutSecs?: number;
  /** Per-agent LLM temperature override (0.0–2.0).
   *  Undefined = use manifest default or system default (0.3). */
  temperature?: number;
  /** Per-agent context window cap in tokens (0 = no limit).
   *  Undefined = use manifest default or system default (200K). */
  contextWindow?: number;
  globalMaxTokens?: number;
  activeModel?: string;
  activeProvider?: string;
  /** ADR-061: minimum compression ratio for context compaction levels 1-7
   *  (0.05–0.95, expressed as the SAVED share). 0.90 = compress until at
   *  most 10% remains (e.g. 200K → 20K). Undefined = use built-in default (0.90). */
  compressionRatioThreshold?: number;
  /** Per-agent LLM session language override (BCP 47, e.g. `"zh-CN"`,
   *  `"en"`). Undefined = follow the global `UserProfile.language`
   *  (the existing default — Agent Setup panel dropdown default).
   *  Empty string from the dropdown also means "no opinion" and is
   *  cleared on save. */
  sessionLanguage?: string | null;
  /** ADR-099: Chat Router kill switch (`chat_router.enabled`).
   *  Undefined = design default (enabled). */
  chatRouterEnabled?: boolean;
  /** ADR-099: explicit router model id (`chat_router.model`).
   *  Undefined / empty = the agent's compact-model chain — the router
   *  deliberately does NOT follow the session's chat-model selection. */
  chatRouterModel?: string;
  /** ADR-099: explicit router provider id (`chat_router.provider`).
   *  Undefined / empty = whichever provider serves `chatRouterModel`. */
  chatRouterProvider?: string;
  /** ADR-099: router call timeout in ms (`chat_router.timeout_ms`).
   *  Undefined = built-in default (8000); clamped 1000–60000. */
  chatRouterTimeoutMs?: number;
  // ADR-094 note: `skillsAutoLoad` was removed from the profile
  // shape. Skill mode (Progressive / Manual) now lives entirely
  // on the `load_skill` tool entry in `agent_tools.json` and is
  // mutated by the Tools panel — there is no second switch on
  // the Agent Setup panel any more.
}

const DEFAULT_PROFILE: AgentProfileSettings = {
  avatarIconId: null,
  modelId: undefined,
  providerId: undefined,
  maxTokens: 0,
  maxIterations: 0,
  maxSessions: 0,
  systemPrompt: undefined,
  shellApprovalThreshold: undefined,
  approvalTimeoutSecs: undefined,
};

const STORAGE_KEY = "acowork-agent-profiles";

function loadAllProfiles(): Record<string, AgentProfileSettings> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Record<string, Partial<AgentProfileSettings>>;
      const result: Record<string, AgentProfileSettings> = {};
      for (const [agentId, s] of Object.entries(parsed)) {
        result[agentId] = normalizeProfile(s);
      }
      return result;
    }
  } catch {
    // localStorage unavailable or corrupted
  }
  return {};
}

function saveAllProfiles(profiles: Record<string, AgentProfileSettings>) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(profiles));
  } catch {
    // silently ignore
  }
}

function normalizeProfile(s: Partial<AgentProfileSettings>): AgentProfileSettings {
  return {
    avatarIconId: validateIconId(s.avatarIconId),
    modelId: s.modelId,
    providerId: s.providerId,
    maxTokens: typeof s.maxTokens === "number" && s.maxTokens > 0 ? s.maxTokens : 0,
    maxIterations:
      typeof s.maxIterations === "number" && s.maxIterations > 0
        ? s.maxIterations
        : typeof (s as { toolsLimit?: number }).toolsLimit === "number" &&
          (s as { toolsLimit?: number }).toolsLimit! > 0
          ? (s as { toolsLimit?: number }).toolsLimit!
          : 0,
    maxSessions: typeof s.maxSessions === "number" && s.maxSessions > 0 ? s.maxSessions : 0,
    systemPrompt: s.systemPrompt,
    // Normalize the legacy "never" spelling (pre-rename) to "auto_approve".
    shellApprovalThreshold:
      s.shellApprovalThreshold === "never" ? "auto_approve" : s.shellApprovalThreshold,
    temperature:
      typeof s.temperature === "number" && s.temperature >= 0 && s.temperature <= 2
        ? s.temperature
        : undefined,
    contextWindow:
      typeof s.contextWindow === "number" && s.contextWindow >= 0
        ? s.contextWindow
        : undefined,
    approvalTimeoutSecs:
      typeof s.approvalTimeoutSecs === "number" && s.approvalTimeoutSecs > 0
        ? s.approvalTimeoutSecs
        : undefined,
    globalMaxTokens: typeof s.globalMaxTokens === "number" ? s.globalMaxTokens : undefined,
    activeModel: typeof s.activeModel === "string" ? s.activeModel : undefined,
    activeProvider: typeof s.activeProvider === "string" ? s.activeProvider : undefined,
    // compressionRatioThreshold: 0.05–0.95 (saved share). Undefined = built-in default.
    compressionRatioThreshold:
      typeof s.compressionRatioThreshold === "number" &&
      s.compressionRatioThreshold >= 0.05 &&
      s.compressionRatioThreshold <= 0.95
        ? s.compressionRatioThreshold
        : undefined,
    // ADR-099 Chat Router knobs. `chatRouterModel` / `chatRouterProvider`
    // keep non-empty strings only — "" is the UI "auto" marker and
    // normalizes to undefined (same display, same server semantics).
    chatRouterEnabled:
      typeof s.chatRouterEnabled === "boolean" ? s.chatRouterEnabled : undefined,
    chatRouterModel:
      typeof s.chatRouterModel === "string" && s.chatRouterModel
        ? s.chatRouterModel
        : undefined,
    chatRouterProvider:
      typeof s.chatRouterProvider === "string" && s.chatRouterProvider
        ? s.chatRouterProvider
        : undefined,
    chatRouterTimeoutMs:
      typeof s.chatRouterTimeoutMs === "number" &&
      s.chatRouterTimeoutMs >= 1000 &&
      s.chatRouterTimeoutMs <= 60000
        ? Math.floor(s.chatRouterTimeoutMs)
        : undefined,
  };
}

function validateIconId(id?: unknown): string | null | undefined {
  if (id === null || id === undefined) return id;
  if (typeof id === "string" && BUILTIN_ICON_IDS.includes(id)) return id;
  return null;
}

// ══════════════════════════════════════════════════════════════════════════
// AgentStorage — per-agent data container
// ══════════════════════════════════════════════════════════════════════════

export interface AgentStorage {
  /** Per-agent data: 一个 agent 的全部运行时状态 */
  meta: AgentInfo;
  /** User-customizable profile (persisted to localStorage) */
  profile: AgentProfileSettings;
  /** Sessions belonging to this agent */
  sessions: SessionInfo[];
  /** Latest session title (top of list) for the AgentList sidebar.
   *  undefined = not yet fetched (shows skeleton); null = fetched, no sessions;
   *  string = fetched, latest session title (empty string = untitled). */
  sessionTitle: string | null | undefined;
  /** Pagination for sessions list */
  pagination: {
    currentPage: number;
    totalPages: number;
    totalCount: number;
    pageSize: number;
  };
  /** Currently loading sessions for this agent */
  isLoading: boolean;
  /**
   * ADR-087: the Gateway refused every session call for this agent
   * (403 `not_authorized` / 404 `not found` on a visibility-filtered
   * read). `undefined` = not yet determined, `false` = accessible.
   *
   * Why this exists: `sessionTitle === undefined` is the sidebar's
   * "still loading" signal, and every session call used to swallow its
   * error. A user who could see an agent but not use it therefore sat
   * on a permanent skeleton with no explanation. The backend now
   * filters those agents out of the list entirely (fail-closed), but
   * a revoked grant can still land between two list polls, and the
   * agent must say WHY instead of spinning.
   */
  accessDenied: boolean | undefined;
}

const DEFAULT_PAGINATION = { currentPage: 1, totalPages: 1, totalCount: 0, pageSize: 20 };

function createStorage(meta: AgentInfo, profile: AgentProfileSettings): AgentStorage {
  return {
    meta,
    profile,
    sessions: [],
    sessionTitle: undefined,
    pagination: { ...DEFAULT_PAGINATION },
    isLoading: false,
    accessDenied: undefined,
  };
}

/**
 * ADR-087 D5: the permission middleware answers a visibility-filtered
 * read with a bare `{"error":"not found"}` (404) so the resource's
 * existence is not leaked — deliberately indistinguishable from "no
 * such thing". The Runtime's genuine "SESSIONS_READY but you have no
 * session" answer travels a different body, so a shape check on the
 * error field is what separates the two cases at this call site.
 */
function isVisibilityFiltered(body: string): boolean {
  if (!body) return false;
  try {
    const parsed = JSON.parse(body) as { error?: unknown };
    return parsed?.error === "not found";
  } catch {
    return false;
  }
}

/**
 * Resolve the storage-map key for an agent id.
 *
 * ADR-073: `agents` is keyed by INSTANCE identity (`instance_id`), while
 * every session-control call site only has the PACKAGE id — the two
 * differ as soon as a package has more than one instance, and a
 * `patchAgent(state, agentId, …)` then silently no-ops (the row keeps
 * spinning). Look the instance up by its meta so the patch lands.
 */
function storageKeyFor(state: AgentStoreState, agentId: string): string | null {
  if (state.agents[agentId]) return agentId;
  for (const [key, storage] of Object.entries(state.agents)) {
    if (storage.meta?.agent_id === agentId) return key;
  }
  return null;
}

/**
 * Flag an agent as inaccessible so the UI can explain instead of spin.
 *
 * Module-level (declared before the store) so it can be reached from the
 * module-level `fetchLatestSession` body without a circular reference to
 * the store object itself.
 */
function markAccessDenied(agentId: string) {
  useAgentStore.setState((state) => {
    const key = storageKeyFor(state, agentId);
    return key ? patchAgent(state, key, { accessDenied: true }) : { agents: state.agents };
  });
}

/** Helper: patch a specific agent's storage fields inside agents map */
function patchAgent<S extends Partial<AgentStorage>>(
  state: { agents: Record<string, AgentStorage> },
  agentId: string,
  patch: S,
): { agents: Record<string, AgentStorage> } {
  const existing = state.agents[agentId];
  if (!existing) return { agents: state.agents };
  return {
    agents: {
      ...state.agents,
      [agentId]: { ...existing, ...patch },
    },
  };
}

// ══════════════════════════════════════════════════════════════════════════
// Module-level: in-flight request dedup
// ══════════════════════════════════════════════════════════════════════════

let fetchSessionReqId = 0;

// ══════════════════════════════════════════════════════════════════════════
// Store interface
// ══════════════════════════════════════════════════════════════════════════

interface AgentStoreState {
  // ── Data ──

  /** Unified per-agent storage: agentId → AgentStorage.
   *  Switching agents does NOT mutate this map — UI reads by `selectedAgentId`. */
  agents: Record<string, AgentStorage>;
  /** Currently selected agent ID — the "pointer" that UI uses to read agents[selectedAgentId]. */
  selectedAgentId: string | null;
  /** Loading flag for the master agent list */
  loading: boolean;
  /** Master list fetch error */
  error: string | null;
  /** Remote-mode node topology snapshot (ADR-073 §4 sidebar groups,
   *  ADR-059 §6.3 bootstrap refetch). Owned here — not in a component —
   *  so the Gateway-connection lifecycle (drop → `markNodesOffline`,
   *  rise → `fetchNodes`) can drive it from one place. */
  nodes: NodeInfo[];
  /** Global UI state: whether the SessionPanel dropdown is open. (display-only, cleared on agent switch) */
  isSessionPanelOpen: boolean;
  /** Agents whose `start_agent` round-trip is currently in flight.
   *  Drives both the ChatPanel Play button spinner and the AgentList
   *  "starting…" badge, and acts as the dedup gate inside `tryStartAgent`
   *  so a rapid second click (button mash / right-click + big button /
   *  double-click) never reaches the backend "already running" branch. */
  startingAgentIds: Set<string>;
  /** ADR-085: last FAILED lifecycle detail per agent, latched.
   *  The FAILED stamp is transient — the Runtime publishes it and exits,
   *  and the LWT OFFLINE landing milliseconds later forces
   *  `meta.lifecycle` back to "offline" and wipes `lifecycle_detail`
   *  (the Gateway also drops its running_agents entry, so REST cannot
   *  recover it either). Polling consumers (`waitForAgentReady`) would
   *  only ever see a generic offline; this latch keeps the reason.
   *  Cleared on the next successful start (`sessions_ready` observed)
   *  or when a fresh `startAgent` attempt begins. */
  lastStartupFailure: Record<string, string>;

  // ── Agent meta actions ──

  fetchAgents: () => Promise<void>;
  /** Refetch the node topology from the Gateway. Failure keeps the
   *  previous snapshot (a transient Gateway blip shouldn't empty the
   *  remote-mode sidebar groups); the rise edge / `bootstrapVersion`
   *  resyncs it. */
  fetchNodes: () => Promise<void>;
  /** Mark every known node offline. Gateway drop edge: the snapshot is
   *  stale the moment the Gateway dies, and no new `bootstrap-state`
   *  snapshot will arrive to bump `bootstrapVersion` (the broker lived
   *  in the Gateway) — without this the sidebar group header keeps
   *  showing a dead node as online. */
  markNodesOffline: () => void;
  selectAgent: (id: string | null) => void;
  installAgent: (packagePath: string, nodeId?: string) => Promise<void>;
  uninstallAgent: (agentId: string) => Promise<void>;
  startAgent: (agentId: string, devMode?: boolean) => Promise<void>;
  stopAgent: (agentId: string) => Promise<void>;
  restartAgentInDebug: (agentId: string) => Promise<void>;
  getAgentDetail: (agentId: string) => Promise<AgentDetail>;
  /** Poll fetchAgents until agent.ready === true (max 30×500ms = 15s). */
  waitForAgentReady: (agentId: string) => Promise<void>;
  /** Atomic dedup gate around `startAgent` + session init + UI sync.
   *  Rejects with `false` when the same agent is already starting — so
   *  a button mash inside the 1-3s MQTT-online window neither reaches
   *  the backend's "already running" branch nor stacks two
   *  `startAgentAndSyncUI` calls. Resolves with `true` on success.
   *  `run` (optional) chains extra work that should be covered by the
   *  same in-flight gate — both AgentList and ChatPanel chain the full
   *  `startAgentAndSyncUI` orchestrator so their "starting…" badge /
   *  spinner stays on through session init.
   *  ponytail: Set membership is O(1); if start-button fan-out grows
   *  past ~hundreds of agents, swap for a per-agent AbortController. */
  tryStartAgent: (
    agentId: string,
    opts?: { devMode?: boolean; run?: (agentId: string) => Promise<void> },
  ) => Promise<boolean>;

  // ── Session actions (write to agents[agentId].*) ──

  fetchSessions: (agentId: string, page?: number) => Promise<void>;
  /** Fetch the latest session (by last_active_at desc) without a full disk scan.
   *  The Runtime caches this during startup. Persists the returned title into
   *  `agents[agentId].sessionTitle` so the AgentList sidebar reflects it
   *  without a separate title-only fetch. ADR-085 D4: returns a tri-state —
   *  `no_session` (ready, none exist) is NOT the same as `unavailable`
   *  (still booting / unreachable). */
  fetchLatestSession: (agentId: string) => Promise<LatestSessionResult>;
  /**
   * Single resolver + opener for an agent's active session (single-flight
   * per agent). Consolidates the former chain X / chain Y duplicates:
   * `activeSessionId` present → `noop`; `/latest-session` tri-state
   * (`unavailable` → return WITHOUT creating); 404/`no_session` → fall
   * back to the account's own scope-filtered list (ADR-076 §决策 4);
   * nothing readable → `createSession` (activation rides the
   * `session_created` event); otherwise → `chatStore.openSession`.
   */
  resolveActiveSession: (agentId: string) => Promise<ResolveActiveSessionResult>;
  /**
   * Activate a session that has just been created (Runtime has already
   * confirmed via `session_created` event that the session exists and is
   * Active). This is "fast-path" activation: open the UI tab + send the
   * open_session MQTT message + load messages, atomically. Used by the
   * `session_created` event handler so the user's "+ button" lands them on
   * the fresh chat without an intermediate click.
   */
  activateNewlyCreatedSession: (sessionId: string, agentId: string) => Promise<void>;
  /** Remember the last selected session for an agent (survives remount). */
  saveSessionForAgent: (agentId: string, sessionId: string) => void;
  /**
   * Create a new execution session. `bindToChat` (ADR-099) additionally binds
   * it to this agent's Chat session, so the fresh session enters the Chat
   * Router's candidate list — the `+` on the Chat surface means "start a new
   * task line I can later refer to from this chat", while the automatic
   * creations (first open, closing the last exec tab) stay unbound.
   */
  createSession: (agentId: string, opts?: { bindToChat?: boolean }) => Promise<void>;
  deleteSession: (agentId: string, sessionId: string) => Promise<void>;
  /**
   * Clear every conversation of this agent the caller owns: the Runtime
   * drops each session's SQLite meta row and its conversation JSONL file.
   * Sessions merely shared (read-only) with the caller are left untouched.
   */
  clearAllSessions: (agentId: string) => Promise<void>;
  closeSession: (agentId: string, sessionId: string) => Promise<void>;
  /** Rename a session: optimistic local update + MQTT `update_session_title`. */
  renameSession: (agentId: string, sessionId: string, title: string) => Promise<void>;
  /**
   * ADR-076 §决策 4: flip a session's read visibility (owner only).
   *
   * Optimistic local patch of `visibility` on the session row, then
   * `PUT /sessions/{sid}/visibility`; rolls the local value back and
   * rethrows when the backend refuses, so the caller can toast the real
   * error. The backend is the source of truth on the next `fetchSessions`.
   */
  setSessionVisibility: (
    agentId: string,
    sessionId: string,
    visibility: "public" | "private",
  ) => Promise<void>;

  // ── Agent lifecycle (MQTT-driven) ──

  /** Update agent liveness from the MQTT `agent_status` event
   *  (protobuf `DataEnvelope<AgentStatus>`, ADR-085). Writes the
   *  authoritative `alive` verdict plus the `lifecycle` capability
   *  state into `meta` — UI consumers gate on
   *  `lifecycle === "sessions_ready"`. `offline` (online=false) also
   *  forces `lifecycle = "offline"` (invariant 6). */
  updateAgentLiveness: (
    agentId: string,
    alive: boolean,
    lifecycle?: AgentLifecycleState,
    detail?: string,
  ) => void;
  /** Patch specific meta fields without a full state reload.
   *  `debug_state` is writable because the debug flow (exit DevMode)
   *  may need to align the local cache to the Gateway's confirmed
   *  state when a refresh fails. */
  patchAgentMeta: (agentId: string, meta: Partial<Pick<AgentInfo, "name" | "version" | "avatar" | "builtin_avatar" | "display_name" | "role" | "debug_state">>) => void;

  // ── Profile actions ──

  getProfile: (agentId: string) => AgentProfileSettings;
  setProfile: (agentId: string, settings: Partial<AgentProfileSettings>) => void;
  resetProfile: (agentId: string) => void;

  // ── UI actions ──

  setSessionPanelOpen: (open: boolean) => void;
  toggleSessionPanel: () => void;
  /** Reset display-only state on agent switch.
   *  Per-agent storage (agents map) is NOT touched. */
  reset: () => void;
}

// ══════════════════════════════════════════════════════════════════════════
// Store implementation
// ══════════════════════════════════════════════════════════════════════════

export const useAgentStore = create<AgentStoreState>((set, get) => ({
  // ── Initial state ──

  agents: {},
  selectedAgentId: null,
  loading: false,
  error: null,
  nodes: [],
  isSessionPanelOpen: false,
  startingAgentIds: new Set<string>(),
  lastStartupFailure: {},

  // ════════════════════════════════════════════════════════════════════════
  // Agent meta actions
  // ════════════════════════════════════════════════════════════════════════

  fetchAgents: async () => {
    const t0 = performance.now();
    set({ loading: true, error: null });
    try {
      const list = await invoke<AgentInfo[]>("list_agents");
      const t1 = performance.now();
      const sr = list.find((a: AgentInfo) => a.agent_id === "com.acowork.senior-engineer");
      if (sr) {
        log.debug(
          `[AgentStore] fetchAgents took ${(t1 - t0).toFixed(0)}ms | senior-engineer: alive=${sr.alive} lifecycle=${sr.lifecycle}`,
        );
      }

// Merge with existing agents map
      const storedProfiles = loadAllProfiles();
      set((state) => {
        // ADR-073: the storage map is keyed by INSTANCE identity —
        // the Gateway always supplies `instance_id` on AgentInfo.
        //
        // Empty-list guard: right after a Gateway restart the agent
        // registry may not be populated yet and `list_agents` returns
        // `[]`. That is NOT "all agents uninstalled" — wiping the
        // sidebar here would blank the agent list until the next
        // successful poll (and every rise-edge fetchAgents hits this
        // window). Keep existing entries as-is on an empty list.
        if (list.length === 0 && Object.keys(state.agents).length > 0) {
          return { loading: false };
        }
        const next: Record<string, AgentStorage> = {};
        for (const raw of list) {
          // ADR-048 follow-up: normalise `debug_state` — a Gateway that
          // predates the field omits it, and `undefined` must behave
          // exactly like `"disabled"` for every consumer (Debug Panel
          // gate, "Enable Debug" button, paused banner, settings badge).
          const meta: AgentInfo = {
            ...raw,
            debug_state: raw.debug_state ?? "disabled",
          };
          // ADR-073: the storage map is keyed by INSTANCE identity —
          // the Gateway always supplies `instance_id` on AgentInfo.
          const id = instanceIdOf(meta);
          const existing = state.agents[id];
          if (existing) {
            // `alive` / `ready` come from the Gateway's authoritative
            // MQTT-registry snapshot — no client-side reconciliation
            // needed. The MQTT `agent_status` handler
            // (updateAgentLiveness) provides the realtime path; this
            // poll is the reconcile fallback that converges any gap.
            next[id] = { ...existing, meta };
          } else {
            // ADR-073: profiles persisted pre-multi-instance are keyed by
            // package id — fall back so existing customisations survive
            // the identity-model upgrade.
            const profile =
              storedProfiles[id] ?? storedProfiles[meta.agent_id] ?? { ...DEFAULT_PROFILE };
            next[id] = createStorage(meta, profile);
          }
        }

        // Remove agents that no longer exist
        for (const id of Object.keys(state.agents)) {
          if (!next[id]) {
            delete next[id];
          }
        }

        // Auto-select: always pick the agent with the largest
        // last_interaction_at.  All agents (including system) are equal —
        // the backend owns the truth, the frontend just displays it.
        // Fallback to list[0] (system per sort order) when nothing
        // has ever been interacted with.
        //
        // A null selectedAgentId is ALSO the legitimate state while a
        // project group chat or a 1:1 inbox thread owns the middle pane —
        // the sidebar row handlers clear the agent selection for that
        // mutual exclusion (see `AgentList`'s group / user rows). Auto-
        // selecting over one of those would light up two rows and swap the
        // group/inbox view out for an `AgentPage`, which is exactly what a
        // remount does on the fetch below when the user switches back to
        // the chat tab. Only claim the pane when nothing else has it.
        let selId = state.selectedAgentId;
        const paneOwnedByPeer = useProjectGroupStore.getState().activeGroupId != null;
        if (!selId && !paneOwnedByPeer && list.length > 0) {
          let bestId: string | null = null;
          let bestTs = -1;
          for (const a of list) {
            const ts = a.last_interaction_at ? Date.parse(a.last_interaction_at) : -1;
            if (!Number.isNaN(ts) && ts > bestTs) {
              bestTs = ts;
              bestId = instanceIdOf(a);
            }
          }
          selId = bestId ?? instanceIdOf(list[0]);
        }

        return { agents: next, selectedAgentId: selId, loading: false };
      });

      // Re-bind the selected agent's session when it has none yet
      // (webview reload / post-start window). The resolver is
      // single-flight, so refresh storms (500ms ready-polls, connection
      // edges, inventory signals) coalesce into one chain instead of
      // stacking — and `/latest-session` 404s (ADR-076: the agent-wide
      // newest session belongs to another account) fall back to the
      // account's own list inside the resolver.
      const current = get();
      const selId = current.selectedAgentId;
      if (selId && !useChatStore.getState().agentStates[selId]?.activeSessionId) {
        void current.resolveActiveSession(selId);
      }
    } catch (e) {
      set({ error: String(e), loading: false });
    }
  },

  fetchNodes: async () => {
    try {
      set({ nodes: await fetchNodesApi() });
    } catch {
      // Gateway unreachable — keep the previous snapshot; the rise edge
      // / `bootstrapVersion` refetch will resync when it's back.
    }
  },

  markNodesOffline: () =>
    set((s) => ({ nodes: s.nodes.map((n) => ({ ...n, online: false })) })),

  selectAgent: (id) => {
    if (!id) {
      // Clearing the active agent is a legitimate action — opening a group /
      // DM from the sidebar relies on AppLayout falling through from
      // <AgentPage /> (selectedAgentId != null) to <GroupChatPanel />
      // (activeGroupId != null). Abort any agent-scoped fetches that would
      // otherwise leak back into a dead tree, then drop the selection.
      set({ selectedAgentId: null });
      useFileTreeStore.getState().abortAll();
      return;
    }
    set({ selectedAgentId: id });

    // Group and DM chat sessions share the middle pane (both are `activeGroupId`
    // chat sessions): picking an agent closes an open group / DM (mirror of
    // `AgentList`'s group row and `UserList.openThread`, which clear
    // `selectedAgentId` to fall through to <GroupChatPanel />). Without this the
    // stale session id would re-appear the next time the user clears the agent.
    useProjectGroupStore.getState().clearSelection();

    // The file-tree cache is keyed per (agent, workspace) so an old
    // agent's in-flight fetches can never contaminate the new agent's
    // entries. But they WOULD waste bandwidth and could land AFTER the
    // user has already switched back — abort them so the UI only ever
    // waits on the agent it can actually see. This is the call site
    // the fileTreeStore doc comment promised ("called on workspace
    // switch"); the session/workspace switch paths route through
    // selectAgent as well, so a single abort here covers them.
    //
    // `abortAll(id)` KEEPS the newly-selected agent's own in-flight
    // fetches alive — a blanket abort would cancel the tree fetch the
    // UI is currently showing, dropping the node back to `idle` with
    // no re-fetch scheduled (agent home stuck on "Loading…" forever
    // after the first fetchAgents completes and re-selects the same
    // agent, which fires on every 30s list poll).
    useFileTreeStore.getState().abortAll(id);

    // Guard: business endpoints (fetchLatestSession → openSession →
    // fetchSessionState) all 503 against an unregistered Runtime.
    // Unstarted agents are a legitimate UI state — the user picks the
    // Start button (or double-clicks the list item) to launch them, and
    // `startAgentAndSyncUI` atomically bootstraps the session there.
    //
    // Bug B v3 fix: the ready flag is intentionally NOT part of this
    // gate. It is pushed via MQTT retained and arrives asynchronously
    // to Runtime HTTP readiness. Previously, a selectAgent that landed
    // before the MQTT event could never re-fire even after the Runtime
    // port was up — the gate had latched false. Now we gate only on
    // `running` (the user-driven start transition) and let
    // `fetchLatestSession`/`openSession` ride out transient 503s via
    // `with503Retry` (see lib/httpRetry.ts).
    //
    // ADR-038: `switchSession` was removed; UI-bound session activation
    // now flows through `chatStore.openSession`, which sends the
    // `open_session` MQTT command and reloads messages atomically.
    //
    // Same gate used by ChatPanel's mount effect ("if (!alive) return")
    // — ready is no longer required because the with503Retry loop in
    // the data fetchers handles transient 503s during the boot window.
    const meta = get().agents[id]?.meta;
    if (!meta?.alive) return;

    // 原子化：选 agent 时解析并激活 latest session。解析（tri-state →
    // 本账号列表 fallback → createSession）与打开（UI + 后端激活 + 消息/
    // 配置缓存）都收敛在 `resolveActiveSession` —— 单飞链，与启动编排链
    // （lib/agent-start.ts）共享同一 in-flight promise，并发刷新不再堆叠。
    void get().resolveActiveSession(id);
  },

  resolveActiveSession: async (agentId) => {
    // Single-flight: callers that arrive while a resolution is in flight
    // share the same promise instead of stacking a second chain.
    const existing = inflightResolve.get(agentId);
    if (existing) return existing;
    const promise = (async (): Promise<ResolveActiveSessionResult> => {
      if (useChatStore.getState().agentStates[agentId]?.activeSessionId) {
        return "noop";
      }

      const latest = await get().fetchLatestSession(agentId);
      // ADR-085 D4: tri-state. `unavailable` (still booting / proxy 503
      // after the retry budget) must NOT fall through to createSession —
      // that is the duplicate-session race D7 removes. The retained
      // status envelope or the next call retries.
      if (latest.status === "unavailable") return "unavailable";
      let target = latest.status === "ok" ? latest.session_id : null;

      if (!target) {
        // ADR-076 §决策 4: `/latest-session` answers from an agent-wide
        // cache, so it 404s as soon as the agent's newest session belongs
        // to another account — a private session is neither readable nor
        // writable for a non-owner, so the Runtime refuses to name it
        // rather than leak the id. Retrying cannot fix that, the call is
        // wrong (not early).
        //
        // Fall back to this account's own scope-filtered list. If the
        // account has no rows it gets a fresh untitled session; activation
        // then rides the `session_created` MQTT event, so there is nothing
        // to open here. Without this fallback the chat panel sits on
        // "Loading session…" forever whenever the agent-wide latest session
        // is not ours (multi-user, e.g. right after an account switch).
        await get().fetchSessions(agentId);
        // `fetchSessions` sorts by `created_at` desc → [0] is newest.
        target = get().agents[agentId]?.sessions[0]?.session_id ?? null;
        if (!target) {
          await get().createSession(agentId);
          return "created";
        }
      }

      // ADR-038: opening from the sidebar / start flow is a "first-open"
      // scenario, so we use the full openSession (UI + backend activation
      // + message/config load) instead of the strict setActiveTab.
      // ADR-047: openSession internally calls loadSession (config + state).
      await useChatStore.getState().openSession(agentId, target);
      // Populate the sessions array so the session tab bar and panel
      // display the correct title instead of "Untitled" until the user
      // manually opens the session list (which triggers fetchSessions).
      void get().fetchSessions(agentId);
      return "opened";
    })().finally(() => {
      inflightResolve.delete(agentId);
    });
    inflightResolve.set(agentId, promise);
    return promise;
  },

  installAgent: async (packagePath, nodeId) => {
    try {
      await invoke("install_agent", { packagePath, devMode: true, nodeId: nodeId ?? null });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  uninstallAgent: async (agentId) => {
    // `meta` is read before the call because the store entry is gone once
    // the removal below commits.
    const meta = get().agents[agentId]?.meta;
    try {
      await invoke("uninstall_agent", { agentId });

      // Clean up profile from localStorage (keyed by instance id, with
      // legacy package-id fallback).
      try {
        const raw = localStorage.getItem(STORAGE_KEY);
        if (raw) {
          const profiles = JSON.parse(raw) as Record<string, unknown>;
          const profileKeys = new Set<string>([agentId]);
          if (meta?.agent_id) profileKeys.add(meta.agent_id);
          let changed = false;
          for (const k of profileKeys) {
            if (profiles[k]) {
              delete profiles[k];
              changed = true;
            }
          }
          if (changed) localStorage.setItem(STORAGE_KEY, JSON.stringify(profiles));
        }
      } catch {
        // localStorage unavailable — non-fatal
      }

      // Disconnect WebSocket and remove chatStore agent state
      useChatStore.setState((state) => {
        const next = { ...state.agentStates };
        delete next[agentId];
        return { agentStates: next };
      });

      set((state) => {
        const next = { ...state.agents };
        delete next[agentId];
        let selId = state.selectedAgentId;
        if (selId === agentId) {
          const remaining = Object.values(next);
          // ADR-077 Q2: no System Agent priority. Pick the first remaining
          // agent, identical to every other deletion path.
          selId = remaining[0] ? instanceIdOf(remaining[0].meta) : null;
        }
        return { agents: next, selectedAgentId: selId };
      });
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  startAgent: async (agentId, devMode) => {
    // ADR-085: a fresh start attempt invalidates the latched failure —
    // a stale reason must never be presented as this attempt's failure.
    set((state) => {
      if (!(agentId in state.lastStartupFailure)) return state;
      const next = { ...state.lastStartupFailure };
      delete next[agentId];
      return { lastStartupFailure: next };
    });
    try {
      await invoke("start_agent", { agentId, devMode: devMode ?? false });
      // Gateway /start 只等到 Node 接受控制命令就返回（node_control
      // start_agent + check_reply）；Runtime 真正上线（MQTT online →
      // updateAgentLiveness(alive=true)）还要 1-3s。这里等"上线事件"
      // 而不是轮询状态：`notifyAgentOnline` 由 agent_status 事件触发，
      // 15s 内没等到即启动失败。若事件先到（invoke 返回时已上线），
      // 直接通过。
      const WAIT_ONLINE_MS = 15_000;
      if (get().agents[agentId]?.meta.alive) return;
      await new Promise<void>((resolve, reject) => {
        const onOnline = (): void => {
          clearTimeout(timer);
          resolve();
        };
        const timer = setTimeout(() => {
          const list = onlineWaiters.get(agentId);
          if (list) {
            const idx = list.indexOf(onOnline);
            if (idx >= 0) list.splice(idx, 1);
            if (list.length === 0) onlineWaiters.delete(agentId);
          }
          reject(new Error(`Agent ${agentId} did not come online within ${WAIT_ONLINE_MS / 1000}s of start`));
        }, WAIT_ONLINE_MS);
        const list = onlineWaiters.get(agentId);
        if (list) list.push(onOnline);
        else onlineWaiters.set(agentId, [onOnline]);
      });
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  stopAgent: async (agentId) => {
    try {
      await invoke("stop_agent", { agentId });
      // Drop the cached session runtime state for this agent so the
      // attachment blobs / pending approvals / tool progress lose
      // their refs and can be GC'd. Same cleanup the Gateway-disconnect
      // path uses; without it, stop leaves a stale chat-store footprint
      // behind even though the Runtime is gone.
      useChatStore.getState().clearAgentSessions(agentId);
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  restartAgentInDebug: async (agentId) => {
    try {
      await invoke("restart_agent_in_debug", { agentId });
      await get().fetchAgents();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  getAgentDetail: async (agentId) => {
    return await invoke<AgentDetail>("get_agent_detail", { agentId });
  },

  waitForAgentReady: async (agentId) => {
    for (let attempt = 0; attempt < 30; attempt++) {
      // ponytail: diagnostic
      const __w0 = performance.now();
      await get().fetchAgents();
      const storage = get().agents[agentId];
      // ponytail: diagnostic
      console.warn(
        `[agentStore] waitForAgentReady attempt=${attempt} ` +
          `after ${Math.round(performance.now() - __w0)}ms ` +
          `alive=${storage?.meta.alive} lifecycle=${storage?.meta.lifecycle}`,
      );
      // ADR-085: readiness is the SESSIONS_READY capability, not the
      // retired `ready` bool — HTTP listening (http_ready) is NOT
      // enough: the session slots may still be filling.
      if (storage?.meta.lifecycle === "sessions_ready") return;
      if (!storage?.meta.alive) {
        // ADR-085: the FAILED stamp is transient — the LWT OFFLINE lands
        // milliseconds after it and wipes both the retained state and the
        // Gateway's running_agents entry. Prefer the latched reason so the
        // user sees the actual startup failure, not a generic offline.
        const failure = get().lastStartupFailure[agentId];
        throw new Error(
          failure
            ? `Agent failed to start: ${failure}`
            : "Agent is no longer alive before becoming ready",
        );
      }
      if (storage?.meta.lifecycle === "failed") {
        throw new Error(
          `Agent failed to start: ${storage.meta.lifecycle_detail || get().lastStartupFailure[agentId] || "unknown error"}`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
    throw new Error("Agent did not become ready within 15 seconds");
  },

  tryStartAgent: async (agentId, opts) => {
    const devMode = opts?.devMode ?? false;
    const run = opts?.run;
    // Dedup gate: a second click within the 1-3s MQTT-online window
    // would otherwise reach the backend's "already running" branch and
    // surface as a misleading error toast — the Gateway already returns
    // 200 idempotently for consistent state, but the prior UX was poor
    // because the in-flight start had not yet flipped `alive`. Returning
    // false here lets callers silently no-op a button mash.
    if (get().startingAgentIds.has(agentId)) return false;
    set((state) => {
      const next = new Set(state.startingAgentIds);
      next.add(agentId);
      return { startingAgentIds: next };
    });
    try {
      await get().startAgent(agentId, devMode);
      if (run) await run(agentId);
      return true;
    } finally {
      set((state) => {
        const next = new Set(state.startingAgentIds);
        next.delete(agentId);
        return { startingAgentIds: next };
      });
    }
  },

  // ════════════════════════════════════════════════════════════════════════
  // Session actions
  // ════════════════════════════════════════════════════════════════════════

  fetchSessions: async (agentId: string, page?: number) => {
    const requestId = ++fetchSessionReqId;
    const currentPage = page ?? get().agents[agentId]?.pagination.currentPage ?? 1;
    const pageSize = get().agents[agentId]?.pagination.pageSize ?? 20;

    // Set per-agent loading
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      return patchAgent(state, agentId, { isLoading: true });
    });

    try {
      // ADR-076 §决策 5: an admin viewing another user's sessions appends
      // `?as_user=`; the Gateway only honors it for admin tokens on GET.
      const viewAs = useAuthStore.getState().viewAsUserId;
      const asUser = viewAs ? `&as_user=${encodeURIComponent(viewAs)}` : "";
      // Bug B v3: transient 503 while the Runtime's session manager is
      // still coming up after `inventory-change`. `with503Retry` honours
      // the Gateway's `Retry-After` so the sidebar list repopulates
      // itself instead of clearing to empty on a 1-second startup window.
      const resp = await with503Retry(
        () =>
          fetch(
            `${getGatewayUrl()}/api/agents/${agentId}/sessions?page=${currentPage}&size=${pageSize}${asUser}`,
          ),
        { tag: `AgentStore.fetchSessions(${agentId})`, logger: log },
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as {
        sessions: SessionInfo[];
        total_count: number;
        total_pages: number;
        // Regression t-83afab47: agent_total_* fields are NO LONGER
        // present in this response. Agent-scoped cumulative totals
        // live exclusively on the live `context_usage` push path
        // (every MQTT `session_state` / `ChunkEvent::ContextUsage`
        // carries them via `patch_agent_totals`). HTTP responses
        // follow the "HTTP 拉完整数据, MQTT 推变化数据" rule and
        // therefore do not duplicate data the push already owns.
      };
      const sessions = (data.sessions ?? []).sort(
        (a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime(),
      );
      if (requestId !== fetchSessionReqId) {
        set((state) => patchAgent(state, agentId, { isLoading: false }));
        return; // stale
      }

      const title = sessions.length > 0 ? (sessions[0]?.title ?? "") : null;

      set((state) =>
        patchAgent(state, agentId, {
          sessions,
          isLoading: false,
          sessionTitle: title,
          pagination: {
            currentPage,
            totalPages: data.total_pages ?? 1,
            totalCount: data.total_count ?? 0,
            pageSize,
          },
        }),
      );

      // ADR-014: Pull repair — use backend sessionStatus to correct frontend state
      const chatStore = useChatStore.getState();
      const mismatches = new Map<string, SessionStatus>();
      for (const session of sessions) {
        if (session.status) {
          const sessionState = chatStore.getSessionState(agentId, session.session_id);
          const frontendStatus = sessionState?.sessionStatus;
          if (!frontendStatus) {
            if (isProcessing(session.status)) {
              mismatches.set(session.session_id, session.status);
            }
          } else {
            const prevStatus = JSON.stringify(frontendStatus);
            const newStatus = JSON.stringify(session.status);
            if (prevStatus !== newStatus) {
              mismatches.set(session.session_id, session.status);
            }
          }
        }
      }
      if (mismatches.size > 0) {
        chatStore.batchUpdateSessionStatuses(agentId, mismatches);
      }

      // Tab strip prunes to the Runtime Active set (ADR-038 lifecycle);
      // only `openSession` ever adds a tab. Gate on the Runtime actually
      // reporting `is_active` — an undefined field on every row means an
      // older Runtime that predates this feature, and pruning on that would
      // wipe every tab.
      if (data.sessions !== undefined && sessions.some((s) => s.is_active !== undefined)) {
        const activeIds = sessions.filter((s) => s.is_active).map((s) => s.session_id);
        chatStore.syncActiveTabs(agentId, activeIds);
      }

      // Sync session workspaces
      useWorkspaceStore.getState().syncSessionWorkspaces(sessions);
    } catch (e) {
      if (requestId !== fetchSessionReqId) {
        set((state) => patchAgent(state, agentId, { isLoading: false }));
        return;
      }
      log.error("[AgentStore] Failed to fetch sessions:", e);
      // ADR-085: a failed fetch (503 startup window, network blip) is
      // NOT evidence of "zero sessions" — keep the previous list so a
      // transient failure cannot erase the sidebar or invite a
      // duplicate createSession.
      set((state) => patchAgent(state, agentId, { isLoading: false }));
    }
  },

  /** Fetch the latest session (by last_active_at desc) without a full disk scan.
   *  The Runtime caches this during startup. Persists the returned title into
   *  `agents[agentId].sessionTitle` so the AgentList sidebar reflects it
   *  without a separate title-only fetch. Returns null if no sessions exist
   *  or the agent is not connected. */
  fetchLatestSession: async (agentId: string): Promise<LatestSessionResult> => {
    try {
      // Bug B v3 fix: this endpoint proxies through the Runtime and
      // 503s during the window between Gateway discovering the agent
      // and the Runtime HTTP port being registered in the reverse
      // proxy. `with503Retry` honours the Gateway's Retry-After
      // header so a transient 503 at session-switch time recovers
      // transparently without the UI having to retry manually.
      const resp = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/latest-session`),
        { tag: `AgentStore.fetchLatestSession(${agentId})`, logger: log },
      );
      if (!resp.ok) {
        // ADR-087: 403 means the Gateway refused this account outright
        // (not on the manage list / not `use`-able). Terminal — mark the
        // agent so the UI can say why instead of spinning forever.
        if (resp.status === 403) {
          markAccessDenied(agentId);
          return { status: "unavailable" };
        }
        // ADR-085 D4: a 404 here normally means the Runtime is
        // SESSIONS_READY and there is no readable session. It is ALSO
        // what the ADR-087 D5 visibility filter returns for an agent the
        // caller may not see, and the two are indistinguishable by
        // status — so the body decides. The permission 404 is the
        // middleware's bare `{"error":"not found"}`; a genuine
        // no-session answer comes from the Runtime proxy.
        if (resp.status === 404) {
          const body = await resp.text().catch(() => "");
          if (isVisibilityFiltered(body)) {
            markAccessDenied(agentId);
            return { status: "unavailable" };
          }
          return { status: "no_session" };
        }
        console.warn(
          `[AgentStore] fetchLatestSession(${agentId}) HTTP ${resp.status} @${performance.now().toFixed(0)}ms`,
        );
        return { status: "unavailable" };
      }
      const data = (await resp.json()) as {
        session_id: string;
        title: string | null;
        created_at: string | null;
      };
      const title = data.title ?? null;
      // Persist into the sidebar cache. Empty string matches the legacy
      // `fetchLatestSessionTitle` semantics so UI consumers keep working
      // (the AgentList treats `""` and `null` differently: `""` →
      // untitled, `null` → idle animation). Only running agents reach
      // this branch.
      // A successful read proves the caller may reach this agent, so any
      // `accessDenied` left from a revoked grant is stale — clear it or
      // the row would keep showing "no access" next to a working title.
      set((state) => {
        const key = storageKeyFor(state, agentId);
        return key
          ? patchAgent(state, key, { sessionTitle: title ?? "", accessDenied: false })
          : { agents: state.agents };
      });
      return { status: "ok", session_id: data.session_id, title };
    } catch (e) {
      log.error(`[AgentStore] fetchLatestSession(${agentId}) failed:`, e);
      return { status: "unavailable" };
    }
  },

  // ADR-038: Activate a session that the Runtime has just confirmed via
  // `session_created`. We delegate to `chatStore.openSession` which owns
  // the full UI+backend+lifecycle transition (UI tab open + MQTT open_session
  // + HTTP messages reload). Idempotent: re-invocations on an already-open
  // session no-op the MQTT side and only refresh the message cache.
  activateNewlyCreatedSession: async (sessionId: string, agentId: string) => {
    // ADR-047: openSession now internally calls loadSession (config + state),
    // so the fresh session's backend `idle` state and config are reflected
    // in the UI before the user types anything.
    await useChatStore.getState().openSession(agentId, sessionId, { prepend: true });
    // ADR-014: Refresh the session list so the freshly-created entry is
    // visible in the sidebar/session dropdown.
    get().fetchSessions(agentId);
  },

  saveSessionForAgent: (_agentId: string, _sessionId: string) => {
    // No-op: rememberedSessionId is no longer tracked client-side.
    // The backend /latest-session endpoint is the source of truth
    // for which session is "current", and selectAgent always fetches
    // it fresh on mount / agent switch.
  },

  createSession: async (agentId: string, opts: { bindToChat?: boolean } = {}) => {
    try {
      // Only this agent's list may seed a new session: the array is one
      // global slot and can hold another agent's workspaces (see
      // `WorkspaceState.workspacesAgentId`). A foreign id here would bind the
      // new session to a workspace this agent does not have.
      const wsState = useWorkspaceStore.getState();
      const lastActiveWs =
        (wsState.workspacesAgentId === agentId ? wsState.workspaces : []).find(
          (w) => w.last_active,
        )?.id ?? null;

      // model/provider is managed by Runtime internally via
      // SessionManager::current_model_and_provider() fallback.
      // Frontend MUST NOT cache or pass preferredModel/preferredProvider
      // — that violates the display-only principle.
      const body: Record<string, string> = {};
      if (lastActiveWs) body.workspace_id = lastActiveWs;

      // ADR-099: a `+` pressed on the Chat surface creates a session that
      // belongs to that conversation. The id is read off the store's mirror
      // of the server row (never invented client-side), and the Runtime
      // re-checks the caller's chat membership before stamping the pair —
      // so a stale or forged id degrades to "unbound", not to access.
      if (opts.bindToChat) {
        const chatSessionId =
          useChatSessionStore.getState().byAgent[agentId]?.chatSessionId;
        if (chatSessionId) body.chat_session_id = chatSessionId;
      }

      // ADR-076 §决策 4: over HTTP, so the Gateway authenticates the
      // caller and the Runtime records the session's owner.
      await sessionControl.createSession(agentId, body);

      // The response carries the new session_id, but activation still
      // rides on the `session_created` MQTT event (the Runtime publishes
      // it either way), so the frontend keeps listening for it.
      // Session meta (workspace_id) is applied when that event arrives.
      //
      // Reaching here proves the Gateway accepted us, so clear any
      // accessDenied left over from a revoked grant (the list will
      // re-filter the agent on its own if the grant is really gone).
      set((state) => patchAgent(state, agentId, { accessDenied: false }));
    } catch (e) {
      // ADR-087: a 403/404 here is terminal for this account. Swallowing
      // it as a generic log left the sidebar on its "loading" skeleton
      // forever, with no indication that the real problem is a
      // permission the user does not have.
      if (sessionControl.isAccessDenied(e)) {
        markAccessDenied(agentId);
        log.warn(`[AgentStore] createSession denied for ${agentId}:`, e);
        return;
      }
      log.error("[AgentStore] Failed to create session:", e);
    }
  },

  closeSession: async (agentId: string, sessionId: string) => {
    try {
      // Close session list entry first; the `close_session` call is made
      // by `chatStore.closeTab` (which we call below for UI cleanup) — no
      // double-firing.
      const storage = get().agents[agentId];
      if (!storage) return;
      const isCurrent = useChatStore.getState().getActiveSessionId(agentId) === sessionId;
      const remaining = storage.sessions.filter((s) => s.session_id !== sessionId);
      const openIds = useChatStore.getState().getOpenSessionIds(agentId);
      let newCurrentId: string | null;
      if (isCurrent) {
        // Prefer an already-open tab (e.g. the default session) so the user
        // doesn't see a random old session auto-open. Design 27 (v2.1): when
        // no open tabs remain there is NO fallback to a closed background
        // session any more — that pre-chat rule ("the exec surface must
        // always have a session") made the last exec tab unclosable: closing
        // it re-opened `remaining[0]`, forever. The pinned Chat tab is the
        // landing surface now; the caller (SessionTabBar.finishCloseTab)
        // flips the view to chat when the strip empties.
        const openRemaining = remaining.filter((s) => openIds.includes(s.session_id));
        newCurrentId = openRemaining.length > 0 ? openRemaining[0].session_id : null;
      } else {
        newCurrentId = useChatStore.getState().getActiveSessionId(agentId);
      }

      set((state) => patchAgent(state, agentId, { sessions: remaining }));

      if (openIds.includes(sessionId)) {
        // chatStore.closeTab fires MQTT close_session internally and
        // activates the neighbor tab. await it so the new active session
        // is visible before we re-open via openSession below.
        const afterClose = await useChatStore.getState().closeTab(agentId, sessionId);
        if (afterClose && afterClose !== sessionId) {
          newCurrentId = afterClose;
        }
      } else {
        // Session was not in the open-tab strip (probably a closed-background
        // session). Still tell the backend to release its task.
        try {
          await sessionControl.closeSession(agentId, sessionId);
        } catch (err) {
          log.warn("[AgentStore] close_session failed:", err);
        }
      }

      if (isCurrent) {
        if (newCurrentId) {
          // ADR-038: re-open the new active tab. openSession is idempotent
          // and ensures the backend has the session Active (it might be
          // a session that was previously closed but never re-opened).
          await useChatStore.getState().openSession(agentId, newCurrentId);
        } else {
          useChatStore.getState().clearMessages(agentId);
        }
      }
      useChatStore.getState().removeSessionState(agentId, sessionId);
    } catch (e) {
      log.error("[AgentStore] Failed to close session:", e);
    }
  },

  deleteSession: async (agentId: string, sessionId: string) => {
    try {
      await sessionControl.deleteSession(agentId, sessionId);

      const storage = get().agents[agentId];
      if (!storage) return;
      const isCurrent = useChatStore.getState().getActiveSessionId(agentId) === sessionId;
      const remaining = storage.sessions.filter((s) => s.session_id !== sessionId);
      // Design 27 (v2.1): deleting the current session must not resurrect a
      // closed background session — same unclosable-tab loop as `closeSession`
      // above. Only `closeTab`'s still-open neighbour (assigned below) becomes
      // the new active; otherwise the pinned chat tab is the landing surface
      // (the dropdown caller flips the mode).
      let newCurrentId: string | null = isCurrent
        ? null
        : useChatStore.getState().getActiveSessionId(agentId);

      set((state) => patchAgent(state, agentId, { sessions: remaining }));

      const openIds = useChatStore.getState().getOpenSessionIds(agentId);
      if (openIds.includes(sessionId)) {
        const afterClose = await useChatStore.getState().closeTab(agentId, sessionId);
        if (afterClose && afterClose !== sessionId) {
          newCurrentId = afterClose;
        }
      }

      if (isCurrent) {
        if (newCurrentId) {
          // ADR-038: re-open the new active tab. openSession is idempotent
          // and ensures the backend has the session Active (deleting the
          // current session is destructive — the new current may be a
          // Closed session that needs lazy resume).
          await useChatStore.getState().openSession(agentId, newCurrentId);
        } else {
          useChatStore.getState().clearMessages(agentId);
        }
      }
      useChatStore.getState().removeSessionState(agentId, sessionId);

      // Invalidate session title so it gets re-fetched (undefined = not yet fetched)
      set((state) => patchAgent(state, agentId, { sessionTitle: undefined }));
    } catch (e) {
      log.error("[AgentStore] Failed to delete session:", e);
    }
  },

  clearAllSessions: async (agentId: string) => {
    try {
      // Close open tabs first (sessions still exist, so the per-tab
      // `close_session` inside `closeTab` succeeds instead of 404ing
      // against already-deleted rows), then bulk-delete on the backend.
      const openIds = useChatStore.getState().getOpenSessionIds(agentId);
      for (const sid of openIds) {
        await useChatStore.getState().closeTab(agentId, sid);
      }

      await sessionControl.clearSessions(agentId);

      // The backend dropped every owned session (SQLite meta row + JSONL
      // file). Mirror that locally: drop session states and empty the
      // list. Read-only shared sessions survive the clear server-side,
      // so re-fetch instead of assuming zero rows.
      const storage = get().agents[agentId];
      if (storage) {
        for (const s of storage.sessions) {
          useChatStore.getState().removeSessionState(agentId, s.session_id);
        }
      }
      useChatStore.getState().clearMessages(agentId);
      set((state) =>
        patchAgent(state, agentId, {
          sessions: [],
          sessionTitle: undefined,
          pagination: { ...DEFAULT_PAGINATION },
        }),
      );
      void get().fetchSessions(agentId, 1);
    } catch (e) {
      log.error("[AgentStore] Failed to clear all sessions:", e);
      throw e;
    }
  },

  renameSession: async (agentId: string, sessionId: string, title: string) => {
    // Optimistic local update — the tab strip, session dropdown, and
    // AgentList sidebar all read `agents[agentId].sessions`, so patch it
    // before the MQTT roundtrip. The Runtime persists the same title via
    // `ConversationSession::update_title_force`.
    set((state) => {
      const storage = state.agents[agentId];
      if (!storage) return state;
      const idx = storage.sessions.findIndex((s) => s.session_id === sessionId);
      if (idx === -1) return state;
      const sessions = [...storage.sessions];
      sessions[idx] = { ...sessions[idx], title };
      return patchAgent(state, agentId, {
        sessions,
        // Keep the sidebar's "latest session title" in sync when the renamed
        // session is the most recently created one (same heuristic used by
        // fetchSessions when it derives `sessionTitle` from `sessions[0]`).
        ...(idx === 0 ? { sessionTitle: title } : {}),
      });
    });

    // Also write the live per-session overlay (`chatStore.sessionStates[sid].title`)
    // — the open tab and the execution status card read that first, so a
    // manual rename must land there too, not only in the list snapshot.
    // The backend echoes the persisted title back via `session_config`.
    useChatStore.getState().setSessionTitleLocal(agentId, sessionId, title);

    try {
      // ADR-076 §决策 4: authenticated HTTP write (was MQTT
      // `update_session_title`).
      await sessionControl.patchSessionConfig(agentId, sessionId, { title });
    } catch (e) {
      log.error("[AgentStore] Failed to rename session:", e);
    }
  },

  setSessionVisibility: async (
    agentId: string,
    sessionId: string,
    visibility: "public" | "private",
  ) => {
    // Optimistic flip, mirroring `renameSession`: the composer toggle, the
    // session dropdown and the AgentList sidebar all read
    // `agents[agentId].sessions`, so patch the row before the round-trip.
    let previous: "public" | "private" | null | undefined;
    set((state) => {
      const storage = state.agents[agentId];
      if (!storage) return state;
      const idx = storage.sessions.findIndex((s) => s.session_id === sessionId);
      if (idx === -1) return state;
      const sessions = [...storage.sessions];
      previous = sessions[idx].visibility;
      sessions[idx] = { ...sessions[idx], visibility };
      return patchAgent(state, agentId, { sessions });
    });

    try {
      await sessionControl.setSessionVisibility(agentId, sessionId, visibility);
    } catch (e) {
      set((state) => {
        const storage = state.agents[agentId];
        if (!storage) return state;
        const idx = storage.sessions.findIndex((s) => s.session_id === sessionId);
        if (idx === -1) return state;
        const sessions = [...storage.sessions];
        sessions[idx] = { ...sessions[idx], visibility: previous };
        return patchAgent(state, agentId, { sessions });
      });
      log.error("[AgentStore] Failed to set session visibility:", e);
      throw e;
    }
  },

  // ── Agent lifecycle (MQTT-driven) ──

  updateAgentLiveness: (
    agentId: string,
    alive: boolean,
    lifecycle?: AgentLifecycleState,
    detail?: string,
  ) => {
    // ADR-085: `alive` is the network-level verdict from
    // `acowork/agents/{id}/status` (protobuf `DataEnvelope<AgentStatus>`);
    // `lifecycle` is the capability state riding the SAME envelope
    // (starting → http_ready → sessions_ready, failed terminal).
    // Invariant 6: online=false always means lifecycle=offline.
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      const patched = patchAgent(state, agentId, {
        meta: {
          ...existing.meta,
          alive,
          lifecycle: alive ? (lifecycle ?? existing.meta.lifecycle) : "offline",
          lifecycle_detail: alive ? (detail ?? existing.meta.lifecycle_detail) : "",
        },
      });
      // ADR-085: latch the FAILED reason — the stamp is transient (the
      // Runtime publishes it and exits; the LWT OFFLINE milliseconds
      // later wipes meta.lifecycle_detail and the Gateway drops its
      // running_agents entry, so neither meta nor REST keeps it). A
      // successful start (sessions_ready) invalidates the old reason.
      let lastStartupFailure = state.lastStartupFailure;
      if (lifecycle === "failed" && detail) {
        lastStartupFailure = { ...lastStartupFailure, [agentId]: detail };
      } else if (lifecycle === "sessions_ready" && agentId in lastStartupFailure) {
        lastStartupFailure = { ...lastStartupFailure };
        delete lastStartupFailure[agentId];
      }
      return lastStartupFailure === state.lastStartupFailure
        ? patched
        : { ...patched, lastStartupFailure };
    });
    // One-shot online event for startAgent's waiter (no polling).
    if (alive) notifyAgentOnline(agentId);
  },

  patchAgentMeta: (agentId: string, meta) => {
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      return patchAgent(state, agentId, {
        meta: { ...existing.meta, ...meta },
      });
    });
  },

  // ════════════════════════════════════════════════════════════════════════
  // Profile actions
  // ════════════════════════════════════════════════════════════════════════

  getProfile: (agentId) => {
    const storage = get().agents[agentId];
    return storage?.profile ?? { ...DEFAULT_PROFILE };
  },

  setProfile: (agentId, settings) => {
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      const updated: AgentProfileSettings = {
        ...existing.profile,
        ...settings,
      };
      // Persist to localStorage
      const allProfiles = profilesToRecord(state.agents);
      allProfiles[agentId] = updated;
      saveAllProfiles(allProfiles);

      return patchAgent(state, agentId, { profile: updated });
    });
  },

  resetProfile: (agentId) => {
    set((state) => {
      const existing = state.agents[agentId];
      if (!existing) return state;
      const allProfiles = profilesToRecord(state.agents);
      delete allProfiles[agentId];
      saveAllProfiles(allProfiles);

      return patchAgent(state, agentId, { profile: { ...DEFAULT_PROFILE } });
    });
  },

  // ════════════════════════════════════════════════════════════════════════
  // UI actions
  // ════════════════════════════════════════════════════════════════════════

  setSessionPanelOpen: (open) => {
    set({ isSessionPanelOpen: open });
  },

  toggleSessionPanel: () => {
    set((state) => ({ isSessionPanelOpen: !state.isSessionPanelOpen }));
  },

  reset: () => {
    // Cancel any in-flight fetch
    ++fetchSessionReqId;
    // Only reset display state — per-agent storage is indexed by agentId and
    // switching agents must NOT clear it (that would cause sidebar flicker).
    // `startingAgentIds` is also display-only (it is the dedup gate; the
    // store entry resolves once the in-flight `startAgent` settles, so a
    // missed cleanup there would self-clear in the finally block of
    // `tryStartAgent` regardless).
    set({ isSessionPanelOpen: false, startingAgentIds: new Set<string>() });
  },
}));

// ── Helper ──────────────────────────────────────────────────────────────

function profilesToRecord(storages: Record<string, AgentStorage>): Record<string, AgentProfileSettings> {
  const out: Record<string, AgentProfileSettings> = {};
  for (const [id, s] of Object.entries(storages)) {
    out[id] = s.profile;
  }
  return out;
}
