//! MCP catalog and per-agent activation state management
//!
//! Manages two concerns:
//! 1. Global MCP catalog — server definitions + credentials (analogous to Vault for providers)
//! 2. Per-agent MCP activation — which servers are active for each agent
//!
//! ADR-090: install execution moved to the Agent (Runtime) process. Opening
//! a preset server's toggle triggers ensure (health_check → install →
//! re-check) on the Agent's machine; the Desktop polls
//! `GET /api/agents/{id}/mcp-ensure/{name}` for progress. The former
//! Gateway-side install pipeline (installMcp / install stages) is removed.

import { create } from "zustand";
import { getGatewayUrl } from "../lib/config";
import { with503Retry } from "../lib/httpRetry";
import { log } from "../lib/logger";
import { emitAgentConfigRefresh } from "../lib/refresh";
import type {
  McpCatalogEntryResponse,
  McpServerConfigDef,
  OperationAck,
} from "../lib/types";

/** ADR-090: one ensure status snapshot for a catalog server, mirrored
 * from the Runtime's `GET /agents/{id}/mcp-ensure/{name}`.
 * state: "probing" | "installing" | "usable" | "failed". */
export interface McpEnsureStatus {
  state: "probing" | "installing" | "usable" | "failed";
  /** Detailed failure reason (only when state === "failed"). */
  error?: string;
}

/**
 * Per-agent in-flight `PUT /mcp-servers` controllers.
 *
 * A newer toggle aborts the previous request so a slow response cannot
 * overwrite the optimistic state written by the newer call. This is a
 * module-level Map (not in the store) because AbortController is mutable
 * and shouldn't trigger Zustand subscriptions.
 */
const inflightMcpPuts: Map<string, AbortController> = new Map();

// ── Catalog types ────────────────────────────────────────────────────

interface McpCatalogState {
  /** Server entries from the global catalog */
  catalog: McpCatalogEntryResponse[];
  /** Loading state */
  loading: boolean;
  /** Error message */
  error: string | null;
}

interface McpCatalogActions {
  /** Load the global MCP catalog from Gateway */
  loadCatalog: () => Promise<void>;
  /** Add a single server entry to the catalog */
  addServer: (config: McpServerConfigDef) => Promise<void>;
  /** Update a single server entry in the catalog */
  updateServer: (name: string, config: McpServerConfigDef) => Promise<void>;
  /** Remove a server entry from the catalog */
  removeServer: (name: string) => Promise<void>;
  /** Replace the entire catalog */
  replaceCatalog: (servers: McpServerConfigDef[]) => Promise<void>;
}

interface McpEnsureState {
  /** Last ensure status per agent+server (`${agentId}/${name}` -> status). */
  ensureStatus: Record<string, McpEnsureStatus | undefined>;
}

/** ensureStatus map key. Exported so the Tools panel reads the right slot. */
export const mcpEnsureKey = (agentId: string, name: string) => `${agentId}/${name}`;

/** In-flight ensure watchers, keyed like ensureStatus. Module-level (not
 *  in the store) — timers must not trigger Zustand subscriptions. */
const ensureWatchers: Map<string, ReturnType<typeof setInterval>> = new Map();

interface McpEnsureActions {
  /**
   * Poll the Runtime's ensure status for one server once. Returns the
   * status; `undefined` when no ensure task exists (custom / non-stdio
   * servers never spawn one) or the agent is unreachable.
   */
  pollEnsureStatus: (agentId: string, name: string) => Promise<McpEnsureStatus | undefined>;
  /** Drop a stale ensure status (toggle closed = retry later, D6). */
  clearEnsureStatus: (name: string) => void;
  /**
   * Poll one server's ensure status to a terminal state (usable/failed),
   * updating `ensureStatus` along the way. On failure the Runtime has
   * already removed the server from `active_names` — reload the agent's
   * active list so the toggle springs back, and emit a config refresh so
   * the Tools panel re-fetches tool state. Fire-and-forget; concurrent
   * watchers for the same key are deduplicated.
   */
  watchEnsure: (agentId: string, name: string) => Promise<void>;
}

// ── Per-agent activation types ───────────────────────────────────────

interface McpActivationState {
  /** Active MCP server names per agent (agentId -> server names) */
  activeServers: Record<string, string[]>;
  /** Loading state per agent */
  activationLoading: Record<string, boolean>;
  /**
   * Per-server reconcile-pending flag: agentId → serverName → true when
   * the user toggled that server but the backend has not yet finished
   * reconnect+reconcile (`agent_mcp_tools.json` not yet refreshed) or
   * the subsequent `/mcp-tools` re-fetch has not returned. Cleared by
   * `clearServerLoading` once the Tools panel reload handler observes
   * that the per-server tool list matches the user's intent
   * (on+non-empty / off+empty). Used to drive the in-card spinner so
   * third-party MCP reconnects (e.g. `playwright` ~5s) do not look like
   * a stuck UI.
   */
  perServerLoading: Record<string, Record<string, boolean>>;
}

interface McpActivationActions {
  /** Load active MCP server names for an agent */
  loadActiveServers: (agentId: string) => Promise<void>;
  /** Set active MCP servers for an agent (replaces the entire list) */
  setActiveServers: (agentId: string, serverNames: string[]) => Promise<void>;
  /** Toggle a single MCP server on/off for an agent */
  toggleServer: (agentId: string, serverName: string) => Promise<void>;
  /**
   * Drop the reconcile-pending flag for one server. Called by the
   * Tools panel after a successful `/mcp-tools` re-fetch observes
   * the server state has settled (on+tools present / off+tools
   * cleared). Idempotent.
   */
  clearServerLoading: (agentId: string, serverName: string) => void;
  /**
   * ADR-104: dismiss (or restore) one manifest-declared MCP recommendation.
   *
   * The dismissal flag is the only user-writable part of a recommendation —
   * everything else is re-projected from the manifest on every agent start,
   * so a client-supplied value would be overwritten anyway. Resolves to the
   * Runtime's refreshed `GET /mcp-servers` payload so the caller can render
   * the authoritative state instead of patching its own copy.
   */
  putRecommendation: (
    agentId: string,
    key: string,
    dismissed: boolean,
  ) => Promise<void>;
}

// ── Combined store ───────────────────────────────────────────────────

export type McpStore = McpCatalogState &
  McpCatalogActions &
  McpActivationState &
  McpActivationActions &
  McpEnsureState &
  McpEnsureActions;

export const useMcpStore = create<McpStore>((set, get) => ({
  // ── Catalog state ──
  catalog: [],
  loading: false,
  error: null,

  // ── Activation state ──
  activeServers: {},
  activationLoading: {},
  perServerLoading: {},

  // ── Ensure state (ADR-090) ──
  ensureStatus: {},

  // ── Ensure actions (ADR-090) ──

  pollEnsureStatus: async (agentId, name) => {
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/mcp-ensure/${encodeURIComponent(name)}`,
      );
      // 404 = no ensure task for this server (custom/non-stdio entry).
      if (!resp.ok) return undefined;
      const data = (await resp.json()) as McpEnsureStatus;
      set((s) => ({
        ensureStatus: { ...s.ensureStatus, [mcpEnsureKey(agentId, name)]: data },
      }));
      return data;
    } catch {
      // Network hiccup on a poll — the next tick recovers.
      return undefined;
    }
  },

  clearEnsureStatus: (name) =>
    set((s) => {
      // Drop every agent's slot for this server name (cheap — the map is
      // tiny) and cancel any running watcher (toggle closed = retry later).
      const next: Record<string, McpEnsureStatus | undefined> = {};
      for (const [k, v] of Object.entries(s.ensureStatus)) {
        if (!k.endsWith(`/${name}`)) next[k] = v;
      }
      for (const key of [...ensureWatchers.keys()]) {
        if (key.endsWith(`/${name}`)) {
          clearInterval(ensureWatchers.get(key)!);
          ensureWatchers.delete(key);
        }
      }
      return { ensureStatus: next };
    }),

  watchEnsure: async (agentId, name) => {
    const key = mcpEnsureKey(agentId, name);
    if (ensureWatchers.has(key)) return; // one watcher per server
    const stop = () => {
      const t = ensureWatchers.get(key);
      if (t) clearInterval(t);
      ensureWatchers.delete(key);
    };
    // First poll immediately so a fast local health check resolves the
    // toggle in one frame; then every 2s (cargo install is minutes).
    const first = await get().pollEnsureStatus(agentId, name);
    if (!first || first.state === "usable" || first.state === "failed") return;
    ensureWatchers.set(
      key,
      setInterval(async () => {
        const st = await get().pollEnsureStatus(agentId, name);
        if (st === undefined) {
          // Task vanished (Runtime restarted) — stop watching; the next
          // toggle re-triggers ensure.
          stop();
          return;
        }
        if (st.state === "usable" || st.state === "failed") {
          stop();
          if (st.state === "failed") {
            // Runtime auto-removed the server from active_names (D6
            // spring-back) — resync local state so the Switch snaps off.
            await get().loadActiveServers(agentId);
          }
          emitAgentConfigRefresh();
        }
      }, 2000),
    );
  },

  // ── Catalog actions ──

  loadCatalog: async () => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog`);
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = (await resp.json()) as { servers: McpCatalogEntryResponse[] };
      set({ catalog: data.servers, loading: false });
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  addServer: async (config: McpServerConfigDef) => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...config }),
      });
      if (!resp.ok) {
        // On error Gateway answers with the standard `ApiError` envelope.
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      // On success Gateway answers with `OperationAck` (ADR-059 §7.3).
      // We don't surface the ack — the catalog is reloaded below and
      // the per-agent tool wiring is re-applied via the refresh event,
      // both of which pick up the just-added server. The typed
      // `OperationAck` parse is here purely so the wire-shape contract
      // is enforced at compile time (the value is discarded).
      await resp.json().catch(() => undefined) as OperationAck | undefined;
      // Reload catalog after adding
      await get().loadCatalog();
      emitAgentConfigRefresh();
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  updateServer: async (name: string, config: McpServerConfigDef) => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog/${encodeURIComponent(name)}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ...config }),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      // Reload catalog after updating
      await get().loadCatalog();
      emitAgentConfigRefresh();
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  removeServer: async (name: string) => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog/${encodeURIComponent(name)}`, {
        method: "DELETE",
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      // Reload catalog after removing
      await get().loadCatalog();
      emitAgentConfigRefresh();
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  replaceCatalog: async (servers: McpServerConfigDef[]) => {
    set({ loading: true, error: null });
    try {
      const resp = await fetch(`${getGatewayUrl()}/api/mcp-catalog`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(servers),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      // Reload catalog after replacing
      await get().loadCatalog();
      emitAgentConfigRefresh();
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set({ error: message, loading: false });
    }
  },

  // ── Activation actions ──

  loadActiveServers: async (agentId: string) => {
    set((s) => ({
      activationLoading: { ...s.activationLoading, [agentId]: true },
      error: null,
    }));
    try {
      // Bug B v3: 503 while the Runtime's HTTP endpoint is still being
      // discovered after an agent starts. Retrying rides out the boot
      // window instead of blanking the MCP server list.
      const resp = await with503Retry(
        () => fetch(`${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/tools`),
        { tag: `McpStore.loadActiveServers(${agentId})`, logger: log },
      );
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const data = await resp.json() as { mcp_servers?: string[] };
      set((s) => ({
        activeServers: { ...s.activeServers, [agentId]: data.mcp_servers ?? [] },
        activationLoading: { ...s.activationLoading, [agentId]: false },
      }));
    } catch (e: unknown) {
      const message = e instanceof Error ? e.message : String(e);
      set((s) => ({
        error: message,
        activationLoading: { ...s.activationLoading, [agentId]: false },
        activeServers: { ...s.activeServers, [agentId]: [] },
      }));
    }
  },

  setActiveServers: async (agentId: string, serverNames: string[]) => {
    // Cancel any in-flight PUT for this agent so a slow response from
    // an earlier click cannot overwrite a later toggle's optimistic state.
    const existing = inflightMcpPuts.get(agentId);
    if (existing) existing.abort();
    const controller = new AbortController();
    inflightMcpPuts.set(agentId, controller);

    // Snapshot the previous value for rollback on error.
    const previous = get().activeServers[agentId] ?? [];

    // Optimistic update: write the new active list immediately so the
    // checkbox reflects the click without waiting for the PUT round-trip.
    // The Rust trait layer validates every name against `cfg.merged()`
    // and rejects with HTTP 400 on unknowns — those rejections will
    // trigger the rollback below.
    set((s) => ({
      activationLoading: { ...s.activationLoading, [agentId]: true },
      error: null,
      activeServers: { ...s.activeServers, [agentId]: serverNames },
    }));
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/mcp-servers`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ servers: serverNames }),
          signal: controller.signal,
        },
      );
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      // Clear the in-flight marker only if it's still ours; a newer
      // call may have replaced it while we were awaiting.
      if (inflightMcpPuts.get(agentId) === controller) {
        inflightMcpPuts.delete(agentId);
      }
      set((s) => ({
        activeServers: { ...s.activeServers, [agentId]: serverNames },
        activationLoading: { ...s.activationLoading, [agentId]: false },
      }));
      // ADR-090: the Runtime reports which newly-enabled servers it began
      // ensuring on this machine. Watch each to its terminal state; servers
      // that toggle OFF get their stale status dropped (retry = reopen).
      try {
        const ack = (await resp.json()) as { ensuring?: string[] };
        const ensuring = ack.ensuring ?? [];
        // Servers the user just switched OFF: drop their stale status so a
        // later re-open starts clean (D6). Servers ON but not being ensured
        // (custom / already-usable) have nothing to show either.
        for (const name of previous) {
          if (!serverNames.includes(name)) get().clearEnsureStatus(name);
        }
        for (const name of serverNames) {
          if (!ensuring.includes(name)) get().clearEnsureStatus(name);
        }
        for (const name of ensuring) {
          void get().watchEnsure(agentId, name);
        }
      } catch {
        // Non-JSON ack — nothing to watch; the toggle itself succeeded.
      }
    } catch (e: unknown) {
      // Aborted by a newer call → don't touch the optimistic state; the
      // newer call already wrote its own serverNames.
      if (controller.signal.aborted) return;
      if (inflightMcpPuts.get(agentId) === controller) {
        inflightMcpPuts.delete(agentId);
      }
      const message = e instanceof Error ? e.message : String(e);
      set((s) => {
        // PUT failed → roll back the active list AND clear any
        // per-server reconcile-pending flags that were set for this
        // toggle. The user will see the Switch snap back to its
        // previous state; a stuck spinner would otherwise imply
        // reconciliation is still in flight when in fact the request
        // never succeeded.
        const pending = s.perServerLoading[agentId];
        const clearedPending = pending
          ? Object.fromEntries(
                Object.entries(pending).map(([k]) => [k, false]),
              )
          : undefined;
        return {
          error: message,
          // Roll back to the snapshot taken before the optimistic write.
          activeServers: { ...s.activeServers, [agentId]: previous },
          activationLoading: { ...s.activationLoading, [agentId]: false },
          perServerLoading: clearedPending
            ? { ...s.perServerLoading, [agentId]: clearedPending }
            : s.perServerLoading,
        };
      });
    }
  },

  toggleServer: async (agentId: string, serverName: string) => {
    const currentActive = get().activeServers[agentId] ?? [];
    const isActive = currentActive.includes(serverName);
    const newServers = isActive
      ? currentActive.filter((s) => s !== serverName)
      : [...currentActive, serverName];

    // Mark this server as reconcile-pending BEFORE the PUT so the
    // spinner starts on the same frame as the optimistic Switch
    // flip. `clearServerLoading` will be called by the Tools panel
    // reload handler once the per-server tool list settles (see
    // `acowork:refresh-agent-config` in ToolsTab). We do not gate
    // the loading flag on the PUT outcome — if the PUT fails the
    // store rolls back `activeServers` and the user toggles again,
    // which will overwrite this entry naturally.
    set((s) => ({
      perServerLoading: {
        ...s.perServerLoading,
        [agentId]: {
          ...(s.perServerLoading[agentId] ?? {}),
          [serverName]: true,
        },
      },
    }));

    await get().setActiveServers(agentId, newServers);
  },

  putRecommendation: async (agentId: string, key: string, dismissed: boolean) => {
    const resp = await fetch(
      `${getGatewayUrl()}/api/agents/${agentId}/mcp-recommendations/${encodeURIComponent(key)}`,
      {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ dismissed }),
      },
    );
    if (!resp.ok) {
      const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
      const message = err.error || `HTTP ${resp.status}`;
      set({ error: message });
      throw new Error(message);
    }
    // The dismissal does not change the active set, but it does change the
    // recommendation states — the caller re-reads /tools and needs the
    // per-agent refresh event the other config mutations emit.
    emitAgentConfigRefresh(agentId);
  },

  clearServerLoading: (agentId: string, serverName: string) => {
    // Idempotent — does nothing if the flag is already false/absent
    // (the user may have toggled twice and the second call replaced
    // the flag). Zustand will only notify subscribers if the slice
    // reference actually changes, so we guard with a same-reference
    // early return when the value is already settled.
    set((s) => {
      const agentLoads = s.perServerLoading[agentId];
      if (!agentLoads?.[serverName]) return s;
      return {
        perServerLoading: {
          ...s.perServerLoading,
          [agentId]: {
            ...agentLoads,
            [serverName]: false,
          },
        },
      };
    });
  },

}));
