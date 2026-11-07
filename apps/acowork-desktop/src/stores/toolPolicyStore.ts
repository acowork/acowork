/**
 * Per-session tool narrowing — the state behind the composer's `+` menu.
 *
 * # Where the truth lives
 *
 * The Runtime owns two things: the agent layer (inventory) and the session
 * layer (deny policy). This store owns only a cache of them plus the pending
 * write, because the menu needs to answer "is this switch on?" during a render
 * it must not schedule a fetch from.
 *
 * # Why the policy is keyed by (agent, session) and the inventory by agent
 *
 * A policy belongs to exactly one session; an inventory is shared by every
 * session of an agent. Keeping them in two maps means opening a second session
 * of the same agent does not re-read the agent layer, and closing a session
 * cannot drop the inventory its sibling is still using.
 *
 * # Optimistic writes and their one ordering rule
 *
 * A toggle applies locally, then PUTs, then reconciles with the server's echo
 * (the Runtime normalises — e.g. an all-empty policy is stored as `null`). If
 * the PUT fails the previous policy is restored, so a switch never stays in a
 * state the backend rejected. Concurrent toggles are serialised per session:
 * the request in flight is tracked, and a later toggle supersedes it, because
 * the body is a *complete replacement* and the newest UI state is the only one
 * worth persisting.
 */
import { create } from "zustand";
import { log } from "../lib/logger";
import {
  type AgentToolInventory,
  type SessionToolPolicy,
  type ToolPolicyGroup,
  emptyToolPolicy,
  fetchAgentToolInventory,
  fetchSessionToolPolicy,
  isToolPolicyEmpty,
  policyOrNull,
  saveSessionToolPolicy,
  withBuiltinToolToggled,
  withGroupReset,
  withMcpServerToggled,
  withMcpToolToggled,
} from "../lib/toolInventory";

/** A stable key for one session's policy. */
function policyKey(agentId: string, sessionId: string): string {
  return `${agentId}/${sessionId}`;
}

interface SessionToolPolicyEntry {
  /** Always normalised (never `null`): the UI wants a total value. */
  policy: SessionToolPolicy;
  /**
   * A successful read has landed at least once, so `policy` is the backend's
   * value rather than the empty default. Gates every write: a PUT replaces the
   * whole policy, so committing before the first read would silently erase the
   * denies another surface (or another device) already set.
   */
  loaded: boolean;
  loading: boolean;
  saving: boolean;
  error: string | null;
  /** Bumped on every local edit, so a late server echo for an older edit can
   *  be recognised as stale instead of overwriting the newer local state. */
  revision: number;
}

interface ToolPolicyStore {
  /** agentId → agent layer, or absent when not fetched yet. */
  inventory: Record<string, AgentToolInventory>;
  inventoryLoading: Record<string, boolean>;
  inventoryError: Record<string, string | null>;

  /** `${agentId}/${sessionId}` → session layer. */
  policies: Record<string, SessionToolPolicyEntry>;
  /** In-flight PUT revision per policy key (see the module doc). */
  inFlight: Record<string, number>;

  loadInventory: (agentId: string, opts?: { force?: boolean }) => Promise<void>;
  loadPolicy: (
    agentId: string,
    sessionId: string,
    opts?: { force?: boolean },
  ) => Promise<void>;

  toggleBuiltin: (
    agentId: string,
    sessionId: string,
    toolName: string,
    on: boolean,
  ) => Promise<void>;
  toggleMcpServer: (
    agentId: string,
    sessionId: string,
    serverName: string,
    on: boolean,
  ) => Promise<void>;
  toggleMcpTool: (
    agentId: string,
    sessionId: string,
    serverName: string,
    toolName: string,
    on: boolean,
  ) => Promise<void>;
  resetGroup: (
    agentId: string,
    sessionId: string,
    group: ToolPolicyGroup,
  ) => Promise<void>;
}

const EMPTY_ENTRY: SessionToolPolicyEntry = {
  policy: emptyToolPolicy(),
  loaded: false,
  loading: false,
  saving: false,
  error: null,
  revision: 0,
};

/** Read a policy entry without creating it (stable for Zustand selectors). */
export function selectPolicyEntry(
  state: Pick<ToolPolicyStore, "policies">,
  agentId: string | null | undefined,
  sessionId: string | null | undefined,
): SessionToolPolicyEntry {
  if (!agentId || !sessionId) return EMPTY_ENTRY;
  return state.policies[policyKey(agentId, sessionId)] ?? EMPTY_ENTRY;
}

export const useToolPolicyStore = create<ToolPolicyStore>((set, get) => {
  /** Apply `next` optimistically, persist, reconcile — see the module doc. */
  const commit = async (
    agentId: string,
    sessionId: string,
    next: SessionToolPolicy,
  ) => {
    const key = policyKey(agentId, sessionId);
    const current = get().policies[key] ?? EMPTY_ENTRY;
    // A whole-policy PUT against a value we never read would drop the existing
    // denies. The UI disables the switches in this window; this is the guard
    // that makes it impossible to reach the wire anyway. A background refresh
    // (`loaded && loading`) is *not* a blocker: `policy` still holds the last
    // value the backend accepted, and `loadPolicy` keeps a local edit that lands
    // while the read is in flight.
    if (!current.loaded) return;
    const revision = current.revision + 1;
    const previous = current.policy;

    set((s) => ({
      policies: {
        ...s.policies,
        [key]: {
          ...current,
          policy: next,
          revision,
          saving: true,
          error: null,
        },
      },
      inFlight: { ...s.inFlight, [key]: revision },
    }));

    try {
      const echoed = await saveSessionToolPolicy(
        agentId,
        sessionId,
        policyOrNull(next),
      );
      // A newer edit already superseded this one: its own PUT owns the final
      // state, so this echo must not overwrite it.
      if ((get().inFlight[key] ?? 0) !== revision) return;
      set((s) => {
        const entry = s.policies[key] ?? EMPTY_ENTRY;
        const inFlight = { ...s.inFlight };
        delete inFlight[key];
        return {
          policies: {
            ...s.policies,
            [key]: {
              ...entry,
              policy: echoed ?? emptyToolPolicy(),
              revision,
              saving: false,
              error: null,
            },
          },
          inFlight,
        };
      });
    } catch (e) {
      if ((get().inFlight[key] ?? 0) !== revision) return;
      const message = e instanceof Error ? e.message : "Unknown error";
      log.error(`[toolPolicy] save failed for ${key}: ${message}`);
      set((s) => {
        const entry = s.policies[key] ?? EMPTY_ENTRY;
        const inFlight = { ...s.inFlight };
        delete inFlight[key];
        return {
          policies: {
            ...s.policies,
            [key]: {
              ...entry,
              // Restore the last state the backend accepted.
              policy: previous,
              revision,
              saving: false,
              error: message,
            },
          },
          inFlight,
        };
      });
    }
  };

  return {
    inventory: {},
    inventoryLoading: {},
    inventoryError: {},
    policies: {},
    inFlight: {},

    loadInventory: async (agentId, opts) => {
      if (!agentId) return;
      const cached = get().inventory[agentId];
      if (cached && !opts?.force) return;
      if (get().inventoryLoading[agentId]) return;
      set((s) => ({
        inventoryLoading: { ...s.inventoryLoading, [agentId]: true },
        inventoryError: { ...s.inventoryError, [agentId]: null },
      }));
      try {
        const inventory = await fetchAgentToolInventory(agentId);
        set((s) => ({
          inventory: { ...s.inventory, [agentId]: inventory },
          inventoryLoading: { ...s.inventoryLoading, [agentId]: false },
        }));
      } catch (e) {
        const message = e instanceof Error ? e.message : "Unknown error";
        log.error(`[toolPolicy] inventory fetch failed for ${agentId}: ${message}`);
        set((s) => ({
          inventoryLoading: { ...s.inventoryLoading, [agentId]: false },
          inventoryError: { ...s.inventoryError, [agentId]: message },
        }));
      }
    },

    loadPolicy: async (agentId, sessionId, opts) => {
      if (!agentId || !sessionId) return;
      const key = policyKey(agentId, sessionId);
      const cached = get().policies[key];
      if (cached && !cached.error && !opts?.force) return;
      if (cached?.loading) return;
      const base = cached ?? EMPTY_ENTRY;
      set((s) => ({
        policies: {
          ...s.policies,
          [key]: { ...base, loading: true, error: null },
        },
      }));
      try {
        const policy = await fetchSessionToolPolicy(agentId, sessionId);
        set((s) => {
          const entry = s.policies[key] ?? EMPTY_ENTRY;
          // A local edit landed while the read was in flight: keep it, the
          // read predates the user's intent.
          if (entry.saving) {
            return {
              policies: { ...s.policies, [key]: { ...entry, loading: false } },
            };
          }
          return {
            policies: {
              ...s.policies,
              [key]: {
                ...entry,
                policy: policy ?? emptyToolPolicy(),
                loaded: true,
                loading: false,
                error: null,
              },
            },
          };
        });
      } catch (e) {
        const message = e instanceof Error ? e.message : "Unknown error";
        log.error(`[toolPolicy] policy fetch failed for ${key}: ${message}`);
        set((s) => {
          const entry = s.policies[key] ?? EMPTY_ENTRY;
          return {
            policies: {
              ...s.policies,
              [key]: { ...entry, loading: false, error: message },
            },
          };
        });
      }
    },

    toggleBuiltin: async (agentId, sessionId, toolName, on) => {
      const inventory = get().inventory[agentId];
      const tool = inventory?.builtin.find((t) => t.name === toolName);
      if (!tool) return;
      const key = policyKey(agentId, sessionId);
      const current = (get().policies[key] ?? EMPTY_ENTRY).policy;
      const next = withBuiltinToolToggled(current, tool, on);
      if (next === current) return;
      await commit(agentId, sessionId, next);
    },

    toggleMcpServer: async (agentId, sessionId, serverName, on) => {
      const inventory = get().inventory[agentId];
      const server = inventory?.mcpServers.find((s) => s.name === serverName);
      if (!server) return;
      const key = policyKey(agentId, sessionId);
      const current = (get().policies[key] ?? EMPTY_ENTRY).policy;
      const next = withMcpServerToggled(current, server, on);
      if (next === current) return;
      await commit(agentId, sessionId, next);
    },

    toggleMcpTool: async (agentId, sessionId, serverName, toolName, on) => {
      const inventory = get().inventory[agentId];
      const server = inventory?.mcpServers.find((s) => s.name === serverName);
      const tool = server?.tools.find((t) => t.tool === toolName);
      if (!tool) return;
      const key = policyKey(agentId, sessionId);
      const current = (get().policies[key] ?? EMPTY_ENTRY).policy;
      const next = withMcpToolToggled(current, tool, on);
      if (next === current) return;
      await commit(agentId, sessionId, next);
    },

    resetGroup: async (agentId, sessionId, group) => {
      const key = policyKey(agentId, sessionId);
      const current = (get().policies[key] ?? EMPTY_ENTRY).policy;
      const next = withGroupReset(current, group);
      // Resetting a group that denies nothing is not a write: keep the store
      // (and the meta row) in the "inherit" state it already is.
      if (isToolPolicyEmpty(current) && isToolPolicyEmpty(next)) return;
      if (next === current) return;
      await commit(agentId, sessionId, next);
    },
  };
});
