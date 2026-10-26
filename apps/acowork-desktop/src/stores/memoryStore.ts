import { create } from "zustand";
import type {
  MemoryNodeResponse,
  MemoryNodesListResponse,
  MemoryStatsResponse,
  DeleteNodeResponse,
  ConsolidationStatusResponse,
  DistillResponse,
  DistillerStatus,
  ForgettingStatus,
  RebuildScope,
} from "../lib/types";
import { getGatewayUrl } from "../lib/config";
import {
  fetchEmbeddingModels,
  startMigration,
} from "../lib/gateway-api";
import i18n from "../i18n";
import { useGatewayStore } from "./gatewayStore";
import { log } from "../lib/logger";
import { with503Retry, WRITE_503_RETRY } from "../lib/httpRetry";
import { httpApiError } from "../lib/api-error";

interface MemoryFilters {
  type: "All" | "Knowledge" | "Episodic" | "Procedural" | "Autobiographical";
  /**
   * Sub-classification filter (only meaningful when `type` is `Knowledge`,
   * `Autobiographical`, or `Episodic`).
   *
   * Knowledge:    `Fact` | `Preference` | `Relation` | `Procedure`
   * Autobiographical: `Identity` | `Capability` | `Limitation`
   *                | `Preference` | `History` | `Relationship`
   * Episodic:    `Fact` | `Preference` | `Relation` | `Procedure`
   *              — read from the `knowledge_subtype` distillation routing
   *              tag (ADR-068 §3.4.2). Same enum as Knowledge because an
   *              episode tagged with `knowledge_subtype=X` is the
   *              distiller's input for promoting a `X` node into the
   *              semantic layer.
   *
   * `""` = no filter. Ignored by the backend when `type` is `Procedural`
   * (that label has no sub-classification).
   */
  subType: string;
  keyword: string;
  timeRange: "1h" | "1d" | "7d" | "30d" | "all";
}

interface MemoryStore {
  nodes: MemoryNodeResponse[];
  total: number;
  stats: MemoryStatsResponse | null;
  selectedNodeId: number | null;

  filters: MemoryFilters;
  page: number;
  pageSize: number;

  loading: boolean;
  error: string | null;
  consolidateMessage: string | null;

  /**
   * Distiller trigger state (ADR-071) — `GET /memory/consolidation/
   * status` `distiller` payload. `null` until the first successful fetch.
   * Drives the "记忆蒸馏" card (enabled switch reflects the service state;
   * the card also shows backlog / last-run summary).
   *
   * ADR-101: since the panel paths are frozen, the `GET /memory/*` routes
   * below are answered by the standalone memory service through the Gateway's
   * `/api/agents/{id}/memory/*` proxy, not by the agent's Runtime.
   */
  distillerStatus: DistillerStatus | null;

  /**
   * Episodic forgetting state (ADR-057 §5.3 redesign) —
   * `GET /memory/consolidation/status` `forgetting` payload. `null` until
   * the first successful fetch. Drives the "记忆遗忘" card (enabled switch
   * reflects the service state).
   */
  forgettingStatus: ForgettingStatus | null;

  /**
   * Set while a "Rebuild Index" migration is in flight for the currently
   * selected agent. Driven by the same harness /api/embedding-models/{id}/
   * start-migration endpoint the Harness tab already uses — we just call it
   * from the memory panel when stored_dim ≠ model_dim.
   *
   * `rebuildPollingRef` (closure-scoped, not in state) holds the interval
   * handle so we can clear it on completion or on agent switch.
   */
  migrationInProgress: boolean;

  // Actions
  fetchNodes: (agentId: string) => Promise<void>;
  fetchStats: (agentId: string) => Promise<void>;
  /**
   * Fetch a single memory node by id — `GET /memory/nodes/{nid}`. Used by
   * the global search dialog to render a node's full detail inline without
   * depending on it being present in the paginated `nodes` list.
   * Returns `null` on any error (best-effort; the caller shows a fallback).
   */
  fetchNode: (agentId: string, nodeId: number) => Promise<MemoryNodeResponse | null>;
  deleteNode: (agentId: string, nodeId: number) => Promise<void>;
  /**
   * Trigger one manual EpisodicDistiller pass — `POST /memory/distill`
   * (ADR-071 D2). Replaces the retired "合并节点" (legacy `consolidate`)
   * button in the memory panel.
   *
   * Returns the `DistillResponse` on a completed run, `null` when the
   * run could not start (distiller disabled / providers missing / HTTP
   * error). On success the node list, stats and distiller status are
   * refreshed so the panel reflects promoted nodes immediately.
   */
  distill: (agentId: string) => Promise<DistillResponse | null>;
  /**
   * Fetch the distiller trigger state — `GET /memory/
   * consolidation/status`. Best-effort: failures leave the previous
   * status in place (the card reloads the same endpoint itself).
   */
  fetchDistillerStatus: (agentId: string) => Promise<void>;
  /**
   * Fetch the episodic forgetting state — `GET /memory/
   * consolidation/status` `forgetting` payload (ADR-057 §5.3 redesign).
   * Best-effort: failures leave the previous status in place.
   */
  fetchForgettingStatus: (agentId: string) => Promise<void>;
  /**
   * Rebuild the vector index for `agentId` using the currently active
   * embedding model.
   *
   * `scope` decides what the service re-embeds: `"all"` re-embeds every node
   * (a model swap — every vector changes meaning), `"repair"` re-embeds only
   * the rows retrieval cannot see (foreign-width or unembedded). The Memory
   * panel passes the scope its health banner decided on; omitting it asks for
   * `"all"`, which is what a caller that does not know the defect should get.
   *
   * Internally calls `startMigration(activeModelId, [agentId], scope)` on the
   * same endpoint the Harness tab uses, then polls `pollMigrationProgress()`
   * every 2s until the agent reports `done` or an error.
   */
  rebuildIndex: (agentId: string, scope?: RebuildScope) => Promise<void>;

  /**
   * Re-attach to a rebuild that is already running for `agentId`.
   *
   * Called when the memory panel opens or switches agents. Without it the
   * panel forgets an in-flight rebuild on every agent switch (clearMemory
   * drops the poll and the flag) and re-arms a button whose only remaining
   * effect is to queue a second migration over the same store.
   */
  resumeRebuild: (agentId: string) => Promise<void>;
  setFilters: (partial: Partial<MemoryFilters>) => void;
  setPage: (page: number) => void;
  setSelectedNodeId: (id: number | null) => void;
  clearMemory: () => void;
}

// Polling-interval handle for the in-flight Rebuild Index action. Kept outside
// the store so clearing memory / switching agents cancels any active poll
// without needing to thread the handle through state.
let rebuildPollingTimer: ReturnType<typeof setInterval> | null = null;

/** Stop the migration poll, if one is running. */
function stopRebuildPoll() {
  if (rebuildPollingTimer) {
    clearInterval(rebuildPollingTimer);
    rebuildPollingTimer = null;
  }
}

/**
 * Watch the Gateway's migration state for one agent until it settles.
 *
 * Shared by the Rebuild button and the re-attach path below: the rebuild runs
 * detached in the Gateway and the memory service and outlives any particular
 * panel, so this poll is only the UI's handle on it - dropping the handle never
 * stopped the work, which is what made the button lie.
 */
function watchRebuild(agentId: string) {
  stopRebuildPoll();
  // The Gateway records migration state synchronously before start-migration
  // returns, so a missing entry normally means the agent stopped or was
  // removed mid-rebuild. Tolerate a couple of empty polls (the first tick can
  // race the response) before concluding there is nothing left to watch.
  let misses = 0;
  rebuildPollingTimer = setInterval(async () => {
    const gateway = useGatewayStore.getState();
    await gateway.pollMigrationProgress();
    const mine = gateway.migrationProgress[agentId];
    if (!mine) {
      if (++misses < 3) return;
      stopRebuildPoll();
      useMemoryStore.setState({ migrationInProgress: false });
      return;
    }
    misses = 0;
    if (!mine.done && !mine.error) return;
    stopRebuildPoll();
    useMemoryStore.setState({ migrationInProgress: false });
    try {
      // Refresh the stats the banner reads: a finished rebuild is exactly the
      // moment the panel should stop warning about stale vectors.
      await useMemoryStore.getState().fetchStats(agentId);
    } catch {
      // Best-effort - the progress record still says whether it succeeded.
    }
    if (mine.error) {
      useMemoryStore.setState({
        error: `${i18n.t("memoryPanel.rebuildFailed", { error: mine.error })}`,
      });
    }
  }, 2000);
}

export const useMemoryStore = create<MemoryStore>((set, get) => ({
  nodes: [],
  total: 0,
  stats: null,
  selectedNodeId: null,
  filters: { type: "All", subType: "", keyword: "", timeRange: "all" },
  page: 1,
  pageSize: 20,
  loading: false,
  error: null,
  consolidateMessage: null,
  distillerStatus: null,
  forgettingStatus: null,
  migrationInProgress: false,

  fetchNodes: async (agentId) => {
    const { page, pageSize, filters } = get();
    set({ loading: true, error: null });
    try {
      const params = new URLSearchParams({
        page: String(page),
        size: String(pageSize),
      });
      if (filters.type !== "All") params.set("type", filters.type);
      if (filters.subType) params.set("sub_type", filters.subType);
      if (filters.keyword) params.set("keyword", filters.keyword);
      if (filters.timeRange !== "all") params.set("time_range", filters.timeRange);

      // Bug B v3 fix: the memory endpoints are proxied to the memory
      // service (ADR-101) and answer 503 during the boot window between
      // Gateway discovery and the supervisor publishing the service port.
      // `with503Retry` rides out
      // transient 503s transparently so the MemoryPanel does not
      // have to gate on a UI-side `isAgentReady` flag (see
      // MemoryPanel.tsx — `isAgentReady` was removed in v3).
      const res = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/nodes?${params}`),
        { tag: `MemoryStore.fetchNodes(${agentId})`, logger: log },
      );
      if (!res.ok) throw await httpApiError(res);
      const data: MemoryNodesListResponse = await res.json();
      set({ nodes: data.nodes, total: data.total, loading: false });
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : "Unknown error" });
    }
  },

  fetchStats: async (agentId) => {
    try {
      // Same 503 retry rationale as fetchNodes above.
      const res = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/stats`),
        { tag: `MemoryStore.fetchStats(${agentId})`, logger: log },
      );
      if (!res.ok) throw await httpApiError(res);
      const data: MemoryStatsResponse = await res.json();
      set({ stats: data });
    } catch (e) {
      log.error("Failed to fetch memory stats:", e);
    }
  },

  fetchNode: async (agentId, nodeId) => {
    try {
      const res = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/nodes/${nodeId}`),
        { tag: `MemoryStore.fetchNode(${agentId},${nodeId})`, logger: log },
      );
      if (!res.ok) throw await httpApiError(res);
      // `GET /memory/nodes/{nid}` returns the raw node JSON (with a `found`
      // flag). It carries every field MemoryNodeDetail reads.
      const data = (await res.json()) as MemoryNodeResponse & { found?: boolean };
      if (data.found === false) return null;
      return data;
    } catch (e) {
      log.error("Failed to fetch memory node:", e);
      return null;
    }
  },

  deleteNode: async (agentId, nodeId) => {
    try {
      // Bug B v3: the Gateway answers 503 while its memory supervisor holds
      // no live process (`memory_process` is `None`), and that check runs
      // BEFORE the request is forwarded — so a retry can never double-delete.
      // Uses the write policy: the user is watching a button, not a background
      // list.
      const res = await with503Retry(
        () =>
          fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/nodes/${nodeId}`, {
            method: "DELETE",
          }),
        { policy: WRITE_503_RETRY, tag: `MemoryStore.deleteNode(${nodeId})`, logger: log },
      );
      if (!res.ok) throw await httpApiError(res);
      const data: DeleteNodeResponse = await res.json();
      if (data.deleted) {
        set((s) => ({
          nodes: s.nodes.filter((n) => n.node_id !== nodeId),
          total: s.total - 1,
          selectedNodeId: s.selectedNodeId === nodeId ? null : s.selectedNodeId,
        }));
        // Refresh stats
        get().fetchStats(agentId);
      }
    } catch (e) {
      set({ error: e instanceof Error ? e.message : "Delete failed" });
    }
  },

  distill: async (agentId) => {
    set({ loading: true, error: null, consolidateMessage: null });
    try {
      const res = await with503Retry(
        () =>
          fetch(`${getGatewayUrl()}/api/agents/${agentId}/memory/distill`, {
            method: "POST",
          }),
        { tag: `MemoryStore.distill(${agentId})`, logger: log },
      );
      if (!res.ok) {
        // 403 permission denial → localized "ask the owner" copy;
        // 409 "distiller is disabled" / 503 "not ready" keep the backend
        // error string so the user knows why the run didn't go.
        const err = await httpApiError(res);
        set({
          loading: false,
          consolidateMessage: err.message,
        });
        return null;
      }
      const data: DistillResponse = await res.json();
      // Refresh everything the run may have changed: node list (promoted
      // nodes), stats (episode → node counts) and the distiller status
      // (last_run / secs_since_distill reset).
      await get().fetchNodes(agentId);
      await get().fetchStats(agentId);
      await get().fetchDistillerStatus(agentId);
      set({ loading: false });
      return data;
    } catch (e) {
      set({
        loading: false,
        error: e instanceof Error ? e.message : "Distill failed",
      });
      return null;
    }
  },

  fetchDistillerStatus: async (agentId) => {
    try {
      const res = await with503Retry(
        () =>
          fetch(
            `${getGatewayUrl()}/api/agents/${agentId}/memory/consolidation/status`,
          ),
        { tag: `MemoryStore.fetchDistillerStatus(${agentId})`, logger: log },
      );
      if (!res.ok) return;
      const data: ConsolidationStatusResponse = await res.json();
      set({ distillerStatus: data.distiller });
    } catch {
      // Best-effort — the card re-reads the same endpoint on mount.
    }
  },

  fetchForgettingStatus: async (agentId) => {
    try {
      const res = await with503Retry(
        () =>
          fetch(
            `${getGatewayUrl()}/api/agents/${agentId}/memory/consolidation/status`,
          ),
        { tag: `MemoryStore.fetchForgettingStatus(${agentId})`, logger: log },
      );
      if (!res.ok) return;
      const data: ConsolidationStatusResponse = await res.json();
      set({ forgettingStatus: data.forgetting });
    } catch {
      // Best-effort — the card re-reads the same endpoint on mount.
    }
  },

  rebuildIndex: async (agentId: string, scope?: RebuildScope) => {
    // Guard: cancel any in-flight poll before starting a new rebuild.
    stopRebuildPoll();
    set({ migrationInProgress: true, error: null });
    try {
      // Resolve the currently active embedding model — start-migration ignores
      // this id for vector dim/endpoint (it uses gw.embed_process directly) but
      // still requires a non-empty model_id to satisfy the route.
      const models = await fetchEmbeddingModels();
      const activeModelId = models.active_model_id;
      if (!activeModelId) {
        throw new Error(
          "No active embedding model is configured. Configure one in the Harness tab first.",
        );
      }
      // `scope` decides what the service re-embeds: the panel passes the scope
      // its health banner chose (a two-row defect must not cost a 5000-row
      // re-embed); callers that do not know the defect omit it and get "all".
      const resp = await startMigration(activeModelId, [agentId], scope);
      // Backend `start_migration` (gateway/src/http/embedding_api.rs) returns
      // `{"status":"ok", ...}` on success and `{"status":"error", ...}` on
      // failure. The previous strict whitelist ("migration_started" / "loaded")
      // did not match the live contract, so the catch branch always fired and
      // the progress setInterval never started, leaving the panel stuck.
      if (resp.status === "error") {
        throw new Error(resp.message || `Migration start failed: ${resp.status}`);
      }
      // An agent whose migration is still running is reported per-agent as
      // `already_running` rather than as a request error - the Gateway refuses
      // to stack a second migration on it, and watching the existing one is
      // what the user asked for either way.
      watchRebuild(agentId);
    } catch (e) {
      stopRebuildPoll();
      set({
        migrationInProgress: false,
        error: e instanceof Error ? e.message : "Rebuild index failed",
      });
    }
  },

  resumeRebuild: async (agentId: string) => {
    if (get().migrationInProgress) return; // already watching
    // `pollMigrationProgress` leaves the cached map untouched when the Gateway
    // is unreachable, so a stale entry would otherwise read as "still running".
    if (useGatewayStore.getState().status !== "connected") return;
    try {
      await useGatewayStore.getState().pollMigrationProgress();
    } catch {
      return; // best-effort; the next panel interaction retries
    }
    const mine = useGatewayStore.getState().migrationProgress[agentId];
    if (mine && !mine.done && !mine.error) {
      set({ migrationInProgress: true });
      watchRebuild(agentId);
    }
  },

  setFilters: (partial) => {
    set((s) => ({ filters: { ...s.filters, ...partial }, page: 1 }));
  },

  setPage: (page) => set({ page }),

  setSelectedNodeId: (id) => set({ selectedNodeId: id }),

  clearMemory: () => {
    if (rebuildPollingTimer) {
      clearInterval(rebuildPollingTimer);
      rebuildPollingTimer = null;
    }
    return set({
      nodes: [],
      total: 0,
      stats: null,
      selectedNodeId: null,
      page: 1,
      error: null,
      consolidateMessage: null,
      distillerStatus: null,
      forgettingStatus: null,
      migrationInProgress: false,
    });
  },
}));

