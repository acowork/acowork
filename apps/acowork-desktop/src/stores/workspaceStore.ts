import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { useSettingsStore } from "./settingsStore";
import { useChatStore } from "./chatStore";
import { DEFAULT_GATEWAY_URL, isGatewayLocal } from "../lib/config";
import { log } from "../lib/logger";
import { with503Retry } from "../lib/httpRetry";
import { useFileTreeStore } from "./fileTree/fileTreeStore";
import { treeKey, isReadyNode } from "./fileTree/types";

/** Single workspace directory entry — matches Gateway API response */
interface WorkspaceDir {
  id: string;
  path: string;
  alias: string | null;
  access: "read-only" | "read-write";
  added_at: string;
  /** Deprecated: replaced by session-level workspace selection (sessionWorkspaceMap). */
  is_current?: boolean;
  /** Legacy field for backward compat; frontend reads sessionWorkspaceMap instead. */
  last_active?: boolean;
  select_count: number;
  last_selected_at: string | null;
  /** Prompt file to inject into system prompt (e.g. "CLAUDE.md", "AGENTS.md"). */
  prompt_file: string | null;
}

/** Single filename-search result — matches Gateway FindResponse.matches */
export interface FileFindMatch {
  name: string;
  /** Forward-slash relative path within the workspace */
  relPath: string;
  /** "file" | "directory" — currently the endpoint only returns "file" */
  type: string;
  /** Heuristic score (higher = better match). */
  score: number;
}

/** Filename-search API response — matches Gateway FindResponse */
export interface FileFindResponse {
  root: string;
  scanned: number;
  truncated: boolean;
  matches: FileFindMatch[];
}

/**
 * One-shot "reveal this file in the workspace tree" request. Subscribers
 * (FileTree, AppLayout) compare `seq` against their last-consumed value to
 * detect re-clicks even when the same file is requested twice in a row.
 */
export interface LocateRequest {
  agentId: string;
  workspaceId: string;
  sessionId: string;
  /** Forward-slash relative path within the workspace (e.g. "src/foo/bar.ts"). */
  relPath: string;
  /** Monotonically-increasing counter; bumped on every request. */
  seq: number;
}

interface WorkspaceState {
  workspaces: WorkspaceDir[];
  /** Which agent instance `workspaces` describes, or `null` when nothing has
   *  been fetched. The list is a single global array while the UI reads it for
   *  `selectedAgentId`, and starting another agent fetches for that one — so
   *  without this tag a stale array from a different agent renders as if it
   *  were the current agent's, and picking one of those foreign ids writes a
   *  workspace the current agent does not have (the panel then resolves
   *  nothing, which reads as "switching does not work"). Consumers gate on it;
   *  every failure path also re-tags it, so a failed fetch claims the slot
   *  with an empty list instead of leaving the previous agent's on screen. */
  workspacesAgentId: string | null;
  /** Per-session current workspace selection. "__agent_home__" = agent home. */
  sessionWorkspaceMap: Record<string, string>;
  loading: boolean;
  /**
   * ADR-087: the last list failure that is *authorization*, not
   * transport. A non-owner on a shared agent gets 403 from
   * `GET /workspaces` (the route stays `AgentManage` — reading the
   * owner's directory layout is exactly what D4's "read == write" tier
   * withholds), and the empty list it produced was indistinguishable
   * from "this agent has no workspaces". The selector renders an
   * explanation instead. `null` = no such failure (including success).
   *
   * Only 403 sets it; 5xx keeps flowing through the retry path and must
   * not permanently pin the panel into the "denied" copy.
   */
  deniedReason: "manage" | null;

  // Fetch workspace list for a given agent
  fetchWorkspaces: (agentId: string) => Promise<void>;

  // Set current workspace for a specific session (preferred API)
  setSessionWorkspace: (agentId: string, sessionId: string, workspaceId: string) => Promise<void>;

  // Legacy: set current workspace using the active session (backward compat)
  setCurrentWorkspace: (agentId: string, workspaceId: string) => Promise<void>;

  // Synchronous local-only setter — used by chatStore/sessionStore to keep
  // sessionWorkspaceMap consistent without an API roundtrip.
  setSessionWorkspaceLocal: (sessionId: string, workspaceId: string) => void;

  // Bulk-sync session workspaces from fetchSessions / activate_session.
  // Accepts the raw session list; removes stale entries automatically.
  syncSessionWorkspaces: (sessions: Array<{ session_id: string; workspace_id?: string | null }>) => void;

  // Get current workspace ID for a session (defaults to "__agent_home__")
  getSessionWorkspaceId: (sessionId: string) => string;

  // Server-side filename search. The Gateway walks the workspace
  // (gitignore-aware) and returns ranked matches in one request.
  findFiles: (
    agentId: string,
    workspaceId: string,
    query: string,
    limit?: number,
    signal?: AbortSignal,
  ) => Promise<FileFindResponse | null>;

  /**
   * Latest "locate file in tree" request, or null. Each call to
   * `requestLocate` overwrites this value (with a fresh `seq`) so subscribers
   * can re-trigger even when the target path is unchanged.
   */
  locateRequest: LocateRequest | null;
  /** Publish a locate request. Subscribers should compare `seq` to act. */
  requestLocate: (req: Omit<LocateRequest, "seq">) => void;

  // Create a new empty file in the workspace
  createFile: (agentId: string, workspaceId: string, path: string) => Promise<boolean>;

  // Create a new directory in the workspace
  createDir: (agentId: string, workspaceId: string, path: string) => Promise<boolean>;

  // Delete a file from the workspace
  deleteFile: (agentId: string, workspaceId: string, path: string) => Promise<boolean>;

  // Delete a directory from the workspace (recursive)
  deleteDir: (agentId: string, workspaceId: string, path: string) => Promise<boolean>;

  // Copy a file or directory within the workspace
  copyItem: (agentId: string, workspaceId: string, source: string, dest: string) => Promise<boolean>;

  // Rename (move) a file or directory within the workspace — atomic on
  // the same filesystem. Mirrors copyItem's signature so the UI code is
  // symmetric. Returns false on 404 (missing source) or 400 (dest
  // already exists); both are surfaced as a user-visible toast by the
  // caller.
  renameItem: (agentId: string, workspaceId: string, source: string, dest: string) => Promise<boolean>;

  // Clipboard for copy/paste — stores the source entry to be pasted
  copiedEntry: { agentId: string; workspaceId: string; path: string; type: "file" | "directory" } | null;
  setCopiedEntry: (entry: { agentId: string; workspaceId: string; path: string; type: "file" | "directory" } | null) => void;

  // Set/unset prompt file for workspace (e.g. CLAUDE.md, AGENTS.md)
  setPromptFile: (agentId: string, workspaceId: string, promptFile: string | null) => Promise<boolean>;

  /**
   * Open the OS file manager with the given file/folder revealed.
   *
   * **Local-mode only.** Returns `false` (and logs) when the Gateway
   * is remote — the file manager would otherwise open on the Gateway
   * host, not the user's machine. The caller should hide or disable
   * the corresponding menu item in remote mode (see
   * `FileTreeNode.handleReveal` for the UI rule).
   *
   * Returns `true` on success, `false` on any error (path not cached,
   * the file no longer exists, the OS refused to spawn the file
   * manager, …). Errors are logged; the caller surfaces them as a toast.
   */
  revealItem: (agentId: string, workspaceId: string, relPath: string) => Promise<boolean>;

  // Clear state on agent switch
  reset: () => void;
}

/** Helper: resolve Gateway URL from settings store, fallback to default */
function getGatewayUrl(): string {
  return useSettingsStore.getState().gatewayUrl || DEFAULT_GATEWAY_URL;
}

/** Monotonic counters that discard stale async responses (race guard).
 *  Two separate counters on purpose: the list fetch and a workspace write
 *  produce different state, and sharing one counter meant every
 *  `setSessionWorkspace` bumped the sequence and silently discarded the
 *  in-flight `fetchWorkspaces` result — including its `loading: false`, which
 *  left the selector pinned on "Loading…" with the previous agent's list
 *  underneath. Rationale for the ownership tag: `workspacesAgentId`. */
let fetchSeq = 0;
let writeSeq = 0;

export const useWorkspaceStore = create<WorkspaceState>((set, get) => ({
  workspaces: [],
  workspacesAgentId: null,
  sessionWorkspaceMap: {},
  loading: false,
  deniedReason: null,
  locateRequest: null,

  /**
 * Fetch the workspace list for an agent. 503 retries (honouring the
 * Gateway's `Retry-After` header) are handled by `with503Retry` —
 * see `lib/httpRetry.ts` for the policy and §4.13 of
 * docs/zh/protocols/http.md for the wire contract.
 *
 * The store owns the retry loop so any UI element invoking
 * `fetchWorkspaces(agentId)` benefits — see `WorkspaceSelector` and
 * the empty-state render path.
 *
 * Why a background loop rather than a caller-level retry: the caller
 * is a React effect with a finite mount lifetime; the store outlives
 * any individual component, so a 503 during the first paint can be
 * recovered even after the caller has unmounted (e.g. user closes the
 * selector dropdown while the fetch is in flight).
 */
fetchWorkspaces: async (agentId: string) => {
  const seq = ++fetchSeq;
  set({ loading: true, deniedReason: null });
  try {
    const baseUrl = getGatewayUrl();
    const resp = await with503Retry(
      () => fetch(`${baseUrl}/api/agents/${agentId}/workspaces`),
      { tag: `WorkspaceStore.fetchWorkspaces(${agentId})`, logger: log },
    );
    // A newer request may have superseded us while we waited on the
    // retry loop — drop the result so the winner's data wins. The winner
    // owns `loading`, so this path deliberately leaves it set.
    if (seq !== fetchSeq) return;
    if (resp.status === 503) {
      // All retries exhausted; with503Retry returned the last 503. Claim the
      // slot with an empty list: this agent's list is unknown, which is not
      // the same thing as another agent's list.
      log.error(`[WorkspaceStore] fetchWorkspaces(${agentId}) still 503 after retries`);
      set({ workspaces: [], workspacesAgentId: agentId, loading: false });
      return;
    }
    if (resp.status === 403) {
      // ADR-087: shared agent, caller is not on the manage list. Not an
      // error to shout about — a state to render. See `deniedReason`.
      log.info(
        `[WorkspaceStore] fetchWorkspaces: 403 — caller is not on the manage list for ${agentId}`,
      );
      set({ workspaces: [], workspacesAgentId: agentId, loading: false, deniedReason: "manage" });
      return;
    }
    if (!resp.ok) {
      log.error(`[WorkspaceStore] fetchWorkspaces failed:`, resp.status, resp.statusText);
      set({ workspaces: [], workspacesAgentId: agentId, loading: false });
      return;
    }
    const data = (await resp.json()) as { workspaces: WorkspaceDir[] };
    const workspaces = data.workspaces || [];
    // Discard stale response if a newer request has been issued
    if (seq !== fetchSeq) return;
    set({
      workspaces,
      workspacesAgentId: agentId,
      loading: false,
    });
  } catch (e) {
    log.error(`[WorkspaceStore] fetchWorkspaces error:`, e);
    if (seq !== fetchSeq) return;
    set({ workspaces: [], workspacesAgentId: agentId, loading: false });
  }
},

  setSessionWorkspace: async (agentId: string, sessionId: string, workspaceId: string) => {
    log.debug("[WorkspaceStore:DEBUG] setSessionWorkspace called", { agentId, sessionId, workspaceId });
    const seq = ++writeSeq;
    const prevMap = { ...get().sessionWorkspaceMap };
    try {
      // ADR-076 §决策 4: authenticated HTTP write (was MQTT, fire-and-forget).
      useChatStore.getState().setSessionWorkspace(agentId, sessionId, workspaceId);
      // Optimistically update local state (Runtime will confirm via session state event)
      if (seq !== writeSeq) return;
      set({
        sessionWorkspaceMap: {
          ...get().sessionWorkspaceMap,
          [sessionId]: workspaceId,
        },
      });
    } catch (e) {
      log.error("[WorkspaceStore] setSessionWorkspace error:", e);
      if (seq !== writeSeq) return;
      // Only the map is rolled back: this action never writes `workspaces`,
      // and restoring an array it does not own would desync it from
      // `workspacesAgentId`.
      set({ sessionWorkspaceMap: prevMap });
    }
  },


  setCurrentWorkspace: async (agentId: string, workspaceId: string) => {
    // Legacy wrapper: resolve active session ID and delegate to setSessionWorkspace
    const activeSessionId = useChatStore.getState().getActiveSessionId(agentId);
    if (!activeSessionId) {
      log.warn("[WorkspaceStore] setCurrentWorkspace: no active session for agent", agentId);
      return;
    }
    return get().setSessionWorkspace(agentId, activeSessionId, workspaceId);
  },

  setSessionWorkspaceLocal: (sessionId: string, workspaceId: string) => {
    set((state) => ({
      sessionWorkspaceMap: { ...state.sessionWorkspaceMap, [sessionId]: workspaceId },
    }));
  },

  syncSessionWorkspaces: (sessions) => {
    set((state) => {
      const next = { ...state.sessionWorkspaceMap };
      let changed = false;
      for (const s of sessions) {
        const wsId = s.workspace_id;
        if (wsId && wsId !== "__agent_home__") {
          if (next[s.session_id] !== wsId) {
            next[s.session_id] = wsId;
            changed = true;
          }
        } else if (s.session_id in next) {
          delete next[s.session_id];
          changed = true;
        }
      }
      return changed ? { sessionWorkspaceMap: next } : {};
    });
  },

  getSessionWorkspaceId: (sessionId: string) => {
    return get().sessionWorkspaceMap[sessionId] ?? "__agent_home__";
  },

  findFiles: async (
    agentId: string,
    workspaceId: string,
    query: string,
    limit?: number,
    signal?: AbortSignal,
  ) => {
    const trimmed = query.trim();
    if (!trimmed) return null;
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams({ q: trimmed });
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      if (limit && limit > 0) {
        params.set("limit", String(limit));
      }
      const resp = await fetch(
        `${baseUrl}/api/agents/${agentId}/workspaces/find?${params.toString()}`,
        { signal },
      );
      if (!resp.ok) {
        // 404 = agent not running. Treat as no results so the UI stays
        // quiet instead of logging a misleading error.
        if (resp.status === 404) return null;
        log.error("[WorkspaceStore] findFiles failed:", resp.status, resp.statusText);
        return null;
      }
      return (await resp.json()) as FileFindResponse;
    } catch (e) {
      // AbortError is expected when the caller cancels an in-flight
      // request (e.g. user kept typing); don't log it as an error.
      if (e instanceof DOMException && e.name === "AbortError") return null;
      log.error("[WorkspaceStore] findFiles error:", e);
      return null;
    }
  },

  requestLocate: (req) => {
    set((state) => ({
      locateRequest: {
        agentId: req.agentId,
        workspaceId: req.workspaceId,
        sessionId: req.sessionId,
        relPath: req.relPath,
        seq: (state.locateRequest?.seq ?? 0) + 1,
      },
    }));
  },

  createFile: async (agentId: string, workspaceId: string, path: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/file${qs ? `?${qs}` : ""}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        log.error("[WorkspaceStore] createFile failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      log.error("[WorkspaceStore] createFile error:", e);
      return false;
    }
  },

  createDir: async (agentId: string, workspaceId: string, path: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/dir${qs ? `?${qs}` : ""}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        log.error("[WorkspaceStore] createDir failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      log.error("[WorkspaceStore] createDir error:", e);
      return false;
    }
  },

  deleteFile: async (agentId: string, workspaceId: string, path: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/file${qs ? `?${qs}` : ""}`, {
        method: "DELETE",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        log.error("[WorkspaceStore] deleteFile failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      log.error("[WorkspaceStore] deleteFile error:", e);
      return false;
    }
  },

  deleteDir: async (agentId: string, workspaceId: string, path: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/dir${qs ? `?${qs}` : ""}`, {
        method: "DELETE",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        log.error("[WorkspaceStore] deleteDir failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      log.error("[WorkspaceStore] deleteDir error:", e);
      return false;
    }
  },

  copyItem: async (agentId: string, workspaceId: string, source: string, dest: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/copy${qs ? `?${qs}` : ""}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ source, dest }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        log.error("[WorkspaceStore] copyItem failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      log.error("[WorkspaceStore] copyItem error:", e);
      return false;
    }
  },

  renameItem: async (agentId: string, workspaceId: string, source: string, dest: string) => {
    try {
      const baseUrl = getGatewayUrl();
      const params = new URLSearchParams();
      if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
      }
      const qs = params.toString();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/rename${qs ? `?${qs}` : ""}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ source, dest }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        log.error("[WorkspaceStore] renameItem failed:", resp.status, resp.statusText, body);
        return false;
      }
      return true;
    } catch (e) {
      log.error("[WorkspaceStore] renameItem error:", e);
      return false;
    }
  },

  copiedEntry: null,

  setCopiedEntry: (entry) => {
    set({ copiedEntry: entry });
  },

  setPromptFile: async (agentId: string, workspaceId: string, promptFile: string | null) => {
    try {
      const baseUrl = getGatewayUrl();
      const resp = await fetch(`${baseUrl}/api/agents/${agentId}/workspaces/${workspaceId}/prompt-file`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ prompt_file: promptFile }),
      });
      if (!resp.ok) {
        const body = await resp.text().catch(() => "<unreadable>");
        log.error("[WorkspaceStore] setPromptFile failed:", resp.status, resp.statusText, body);
        return false;
      }
      // The Runtime echoes { ok, ws_id } — NOT a full WorkspaceDir. Replacing
      // the local entry with that response corrupted the workspaces list
      // (id/path/access became undefined). Patch the field we already sent.
      await resp.json().catch(() => ({}));
      set((state) => ({
        workspaces: state.workspaces.map((ws) =>
          ws.id === workspaceId ? { ...ws, prompt_file: promptFile } : ws,
        ),
      }));
      return true;
    } catch (e) {
      log.error("[WorkspaceStore] setPromptFile error:", e);
      return false;
    }
  },

  revealItem: async (agentId: string, workspaceId: string, relPath: string) => {
    // Defence in depth: even though FileTreeNode hides this menu
    // item in remote mode, we re-check here. A manipulated frontend
    // (or stale menu) must not be able to open the Gateway host's
    // file manager.
    if (!isGatewayLocal()) {
      log.warn(
        "[WorkspaceStore] revealItem refused: gateway is in remote mode (agentId=%s, workspaceId=%s, path=%s)",
        agentId,
        workspaceId,
        relPath,
      );
      return false;
    }

    const rootKey = `${agentId}:${workspaceId}`;
    // Look up the cached workspace root from the fileTreeStore. The
    // root path is returned by the tree API for the workspace root
    // (relPath="") and is mirrored into the tree cache node.
    const rootNode = useFileTreeStore.getState().getNode(treeKey(agentId, workspaceId, ""));
    if (!isReadyNode(rootNode)) {
      log.warn(
        "[WorkspaceStore] revealItem failed: no cached workspace root for %s — fetch the tree first",
        rootKey,
      );
      return false;
    }
    const root = rootNode.root;

    // TreeResponse.root is documented as forward-slash normalised,
    // even on Windows. `explorer.exe` accepts forward slashes, so we
    // keep the separator uniform instead of swapping to Path.join
    // (which would introduce backslashes on Windows and break the
    // /select,` argument boundary).
    const trimmedRel = relPath.replace(/^\/+/, "").replace(/\/+$/, "");
    const absolute = trimmedRel === "" ? root : `${root}/${trimmedRel}`;

    try {
      await invoke("reveal_in_file_explorer", { path: absolute });
      return true;
    } catch (e) {
      log.error(
        "[WorkspaceStore] revealItem failed (path=%s):",
        absolute,
        e,
      );
      return false;
    }
  },

  reset: () => {
    set({
      workspaces: [],
      workspacesAgentId: null,
      sessionWorkspaceMap: {},
      loading: false,
      deniedReason: null,
      copiedEntry: null,
      locateRequest: null,
    });
  },
}));

export type { WorkspaceDir, WorkspaceState };
