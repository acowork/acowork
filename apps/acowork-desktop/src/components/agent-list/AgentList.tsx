import { useEffect, useState, useRef, useMemo, useCallback, Fragment } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useToast } from "../common/ToastProvider";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { AgentDetailDialog } from "./AgentDetailDialog";
import { CloneDialog } from "./CloneDialog";
import { PermissionDialog, type PermissionTarget } from "./PermissionDialog";
import { PublishWizard } from "./PublishWizard";
import { CreateWizard } from "./CreateWizard";
import { AgentAvatar } from "../common/AgentAvatar";
import { Tooltip } from "../common/Tooltip";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import { Play, Square, Trash2, Info, Copy, Plus, Search, Package, Sparkles, Bug, ChevronRight, UserCog, Settings, Server } from "lucide-react";
import { StyledInput } from "../common/StyledInput";
import { open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { isProcessing, instanceIdOf, type AgentInfo, type CloneResponse, type NodeInfo } from "../../lib/types";
import { startAgentAndSyncUI } from "../../lib/agent-start";
import { fetchNodes } from "../../lib/gateway-api";
import { partitionAgentsByNode, nodeDisplayName } from "./partitionAgentsByNode";
import { UserList, type UserListHandle } from "../user-list/UserList";
import { useAuthStore } from "../../stores/authStore";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../common/ContextMenu";

interface AgentListProps {
  width?: number;
}

export function AgentList({ width }: AgentListProps) {
  const { t } = useTranslation();
  const isCollapsed = width !== undefined && width <= 80;
  const { selectedAgentId, loading, fetchAgents, selectAgent, stopAgent, uninstallAgent, fetchLatestSession } =
    useAgentStore();
  const agentsMap = useAgentStore((s) => s.agents);

  // ADR-014 derived view for the sidebar status dot — an agent shows the
  // dot when *any* of its cached sessions is not in `idle` (i.e. streaming,
  // waiting_approval, or paused). This is the IM-style "needs attention"
  // semantic: dot disappears when everything is idle, regardless of whether
  // the agent process itself is running.
  //
  // Source: `chatStore.agentStates[aid].sessionStates[sid].sessionStatus`,
  // kept up-to-date by `fetchSessions`'s ADR-014 Pull repair and by MQTT
  // `session_status_changed` events. Agents that have never been opened
  // have an empty sessionStates map — they show no dot until the user
  // opens them, which is the correct IM semantic (red dot = something
  // the user can act on).
  const sessionStatesByAgent = useChatStore((s) => s.agentStates);
  const activeAgentIds = useMemo(() => {
    const ids = new Set<string>();
    for (const [agentId, agentState] of Object.entries(sessionStatesByAgent)) {
      const sessionStates = agentState.sessionStates ?? {};
      for (const sess of Object.values(sessionStates)) {
        if (isProcessing(sess.sessionStatus)) {
          ids.add(agentId);
          break;
        }
      }
    }
    return ids;
  }, [sessionStatesByAgent]);
  const agentsList = useMemo(() => Object.values(agentsMap).map((s) => s.meta), [agentsMap]);

  // ADR-073 §4: view mode is decided automatically by `gatewayMode`. In
  // an off-site mode (remote LAN or relay — multi-node Gateway) the
  // sidebar groups agents by node and shows a collapsible 1/3-height
  // header per node; in local mode the pre-existing flat list renders
  // unchanged.
  const gatewayMode = useSettingsStore((s) => s.gatewayMode);
  const isRemoteMode = gatewayMode !== "local";

  // Node topology snapshot — owned by `agentStore` so the Gateway
  // connection lifecycle (drop → markNodesOffline, rise → fetchNodes)
  // drives it from one place; the ADR-059 `bootstrapVersion` realtime
  // path below refetches it on per-node online/offline transitions.
  const nodes = useAgentStore((s) => s.nodes);

  // ADR-073 §4: collapsible per-node groups. Default = all expanded (empty
  // Set = nothing collapsed). State is component-local — switching modes
  // or remounting resets it, which is acceptable for a sidebar view.
  const [collapsedNodes, setCollapsedNodes] = useState<Set<string>>(new Set());
  const toggleNode = useCallback((nodeId: string) => {
    setCollapsedNodes((prev) => {
      const next = new Set(prev);
      if (next.has(nodeId)) next.delete(nodeId);
      else next.add(nodeId);
      return next;
    });
  }, []);

  const { addToast } = useToast();
  const agentMenu = useContextMenu<{ agentId: string }>();
  const [installing, setInstalling] = useState(false);
  const addMenuRef = useRef<HTMLDivElement>(null);
  // Imported for the agent + button → "Create account" menu item. The actual
  // CreateAccountModal stays inside <UserList>; this ref opens it.
  const userListRef = useRef<UserListHandle>(null);
  const [searchQuery, setSearchQuery] = useState("");
  const [addMenuOpen, setAddMenuOpen] = useState(false);
  // ADR-055 §6.13.3: when installing with >1 online node, the add-menu
  // switches to a node picker (`NodeInfo[]`); `null` = default menu.
  const [installNodes, setInstallNodes] = useState<NodeInfo[] | null>(null);

  // "Create account" menu item gate. Mirrors the same canInvite logic used
  // by the old user-list banner "+" (see ADR-076 §决策 6): admins always,
  // non-admins only while self-registration is open.
  const selfAccount = useAuthStore((s) => s.account);
  const registrationOpen = useAuthStore((s) => s.registrationOpen);
  const canInviteUser = selfAccount?.role === "admin" || registrationOpen;

  // Read the shared in-flight set from the store so the sidebar's
  // "starting…" badge and the ChatPanel Play button spinner see the
  // same dedup gate — see `useAgentStore.tryStartAgent`. Reading the Set
  // directly (not via a `has(id)` selector) keeps the row's other
  // selector subscriptions untouched.
  const startingAgentIds = useAgentStore((s) => s.startingAgentIds);

  // Confirm dialog state
  const [confirmDialog, setConfirmDialog] = useState<{
    open: boolean;
    title: string;
    message: string;
    confirmLabel: string;
    destructive: boolean;
    onConfirm: () => void;
  }>({
    open: false,
    title: "",
    message: "",
      confirmLabel: t("common.confirm"),
    destructive: false,
    onConfirm: () => { },
  });

  // Agent detail dialog state
  const [detailAgentId, setDetailAgentId] = useState<string | null>(null);
  // ADR-087: agent/node permissions dialog target (owner/guests/visibility).
  const [permTarget, setPermTarget] = useState<PermissionTarget | null>(null);

  // Clone dialog state
  const [cloneSource, setCloneSource] = useState<{ agentId: string; packageId: string; agentName: string } | null>(null);

  // Publish wizard state
  const [publishTarget, setPublishTarget] = useState<{ agentId: string; agentName: string } | null>(null);

  // Create wizard state
  const [showCreateWizard, setShowCreateWizard] = useState(false);

  // ADR-059: realtime trigger for `/api/nodes` refetch. Every retained
  // `bootstrap-state` snapshot the Gateway republishes (incl. per-node
  // online/offline transitions) bumps this counter; the effect below
  // refetches the node topology so a kill turns the dot gray within ~1s
  // and a reboot turns it green again within ~1s — no 30s polling
  // fallback needed (the original loop polled both agents and nodes).
  const bootstrapVersion = useChatStore((s) => s.bootstrapVersion);

  // Inventory-change counter — bumped by the `inventory-changed` Tauri
  // event (see chatStore.ts listener) and by every MQTT transition into
  // `connected`. The Gateway sends a live (non-retained)
  // `acowork/desktop/inventory` signal whenever its aggregated
  // `installed_agents` table mutates (remote Node finishes install /
  // uninstall, Node replays its retained inventory on reconnect, HTTP
  // DELETE /api/agents/{id}); the connection edge is the catch-up for
  // signals missed while disconnected. On bump we refetch the list —
  // replaces the previous mount-fetch + 30s `setInterval` polling
  // fallback that left the sidebar stale until a tab-switch remount.
  const inventoryVersion = useChatStore((s) => s.inventoryVersion);

  useEffect(() => {
    // Initial mount fetch — populates the list before any realtime
    // signal arrives, and the only fetch path when MQTT is unavailable.
    fetchAgents();
  }, [fetchAgents]);

  // Realtime refetch on every inventory change (signal or reconnect).
  useEffect(() => {
    if (inventoryVersion === 0) return; // Skip the initial 0 (mount fetch already ran)
    fetchAgents();
  }, [inventoryVersion, fetchAgents]);

  // Refetch the node topology on mount and on every bootstrap snapshot
  // transition. `bootstrapVersion` increments drive the realtime path
  // (ADR-059: per-node online/offline transitions republish the
  // snapshot); the Gateway drop/rise edges are handled globally by
  // `applyGatewayTransition` → `markNodesOffline` / `fetchNodes`.
  useEffect(() => {
    void useAgentStore.getState().fetchNodes();
  }, [bootstrapVersion]);

  // Ensure every ready agent's latest session title is loaded so the
  // sidebar shows it without requiring the user to click the agent.
  //
  // Two scenarios produce a stuck "skeleton" placeholder otherwise:
  //   1. System Agent (and other auto-started agents) whose lifecycle
  //      never goes through `startAgentAndSyncUI` → `initSessionForAgent`,
  //      so `sessionTitle` is never populated.
  //   2. Agents whose startup scan outlasts the 10-retry budget inside
  //      `initSessionForAgent`. The skeleton would otherwise persist until
  //      the user happens to click the agent.
  //
  // We key the effect on the *set* of agents that still need a fetch
  // (running && sessionTitle === undefined), so unrelated store
  // churn (sessions list updates, profile edits, MQTT online flips) does
  // not re-fire the requests.
  // Bug B v3 fix: only gate on `running` (the user-driven start
  // transition), not on `ready`. `ready` is pushed via MQTT retained
  // and arrives asynchronously to Runtime HTTP readiness. Previously,
  // if the sidebar list re-rendered with `ready=false`, we never
  // fetched the title even after the Runtime came up — the gate had
  // latched false. The fetcher `fetchLatestSession` now owns the 503
  // retry loop via `with503Retry`, so a transient 503 during the boot
  // window recovers transparently.
  const agentsNeedingTitle = useMemo(() => {
    const ids: string[] = [];
    for (const [id, storage] of Object.entries(agentsMap)) {
      if (
        storage.meta.alive &&
        storage.sessionTitle === undefined
      ) {
        ids.push(id);
      }
    }
    return ids;
  }, [agentsMap]);

  useEffect(() => {
    if (agentsNeedingTitle.length === 0) return;
    for (const id of agentsNeedingTitle) {
      void fetchLatestSession(id);
    }
  }, [agentsNeedingTitle, fetchLatestSession]);

  // Close the "+ add agent" popover on outside click. The agent right-click
  // menu handles its own close inside `useContextMenu`.
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (addMenuRef.current && !addMenuRef.current.contains(e.target as Node)) {
        setAddMenuOpen(false);
        setInstallNodes(null);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, []);

  /** Pick a .agent file and install it (optionally to a specific node). */
  const doInstall = async (nodeId?: string) => {
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: t("agentList.filterAgentPackage"), extensions: ["agent"] }],
      });
      if (selected) {
        setInstalling(true);
        await useAgentStore.getState().installAgent(selected, nodeId);
        addToast({ type: "success", message: t("agentList.agentInstalled") });
        // Auto-select the newly installed agent
        await fetchAgents();
        const agentsNow = useAgentStore.getState().agents;
        const ids = Object.keys(agentsNow);
        if (ids.length > 0) {
          selectAgent(ids[ids.length - 1]);
        }
      }
    } catch (e) {
      addToast({ type: "error", message: t("agentList.errorFailedToInstallAgent", { error: String(e) }) });
    } finally {
      setInstalling(false);
    }
  };

  /**
   * "Install Agent" menu action (ADR-055 §6.13.3): resolve the online
   * nodes first. With >1 online node, switch the menu to a node picker;
   * otherwise install straight to the sole/default node.
   */
  const handleInstall = async () => {
    let onlineNodes: NodeInfo[] = [];
    try {
      onlineNodes = (await fetchNodes()).filter((n) => n.online);
    } catch {
      // Gateway unreachable — fall through to the default node; the
      // install itself will surface the connection error.
    }
    if (onlineNodes.length > 1) {
      setInstallNodes(onlineNodes);
      return;
    }
    setAddMenuOpen(false);
    await doInstall(onlineNodes.length === 1 ? onlineNodes[0].node_id : undefined);
  };

  // ADR-087: "Create local Node" — spawns the bundled acowork-node on this
  // machine, bound to the signed-in account. The enrollment token is minted
  // and consumed inside the command; it never surfaces in the UI.
  const [creatingNode, setCreatingNode] = useState(false);

  // ADR-087 follow-up: first-run banner. In remote mode the Desktop
  // auto-start path deliberately only RESUMES an existing enrollment and
  // skips first-time enrollment silently — so when this machine has no
  // enrolled node, prompt for the explicit action here. The state is read
  // from the Rust side (identity.json + Gateway node-list reconciliation),
  // not from the node list, because an unenrolled machine has no row to
  // look up. Amber banner style mirrors the app's reminder language (translucent bg +
  // 1px border) — the app's shared non-modal reminder language.
  const [localNode, setLocalNode] = useState<{ enrolled: boolean; running: boolean } | null>(null);
  const refreshLocalNode = useCallback(() => {
    if (!isRemoteMode || !selfAccount) {
      setLocalNode(null);
      return;
    }
    invoke<{ enrolled: boolean; running: boolean }>("local_node_status")
      .then(setLocalNode)
      .catch(() => setLocalNode(null));
  }, [isRemoteMode, selfAccount]);
  useEffect(() => {
    refreshLocalNode();
  }, [refreshLocalNode]);

  const handleCreateLocalNode = async () => {
    setAddMenuOpen(false);
    setCreatingNode(true);
    try {
      const res = await invoke<{ pid?: number; was_enrolled?: boolean; already_running?: boolean }>(
        "create_local_node",
      );
      if (res.already_running) {
        addToast({ type: "info", message: t("agentList.localNodeAlreadyRunning") });
        return;
      }
      addToast({
        type: "success",
        message: res.was_enrolled ? t("agentList.localNodeStarted") : t("agentList.localNodeCreated"),
      });
      await useAgentStore.getState().fetchNodes();
      refreshLocalNode();
    } catch (e) {
      addToast({ type: "error", message: t("agentList.errorCreateLocalNode", { error: String(e) }) });
    } finally {
      setCreatingNode(false);
    }
  };

  const handleStart = async (agentId: string) => {
    // tryStartAgent is the dedup gate — a second click inside the
    // 1-3s MQTT-online window returns false silently instead of
    // hitting the backend "already running" branch. The full
    // orchestrator runs inside the in-flight window so the sidebar
    // badge stays on through session init.
    try {
      const started = await useAgentStore.getState().tryStartAgent(agentId, {
        run: (id) => startAgentAndSyncUI(id),
      });
      if (!started) return;
      addToast({ type: "success", message: t("agentList.agentStarted") });
    } catch (e: any) {
      addToast({ type: "error", message: e?.message ?? String(e) });
    }
  };

  const handleDebugStart = async (agentId: string) => {
    try {
      const started = await useAgentStore.getState().tryStartAgent(agentId, {
        devMode: true,
        run: (id) => startAgentAndSyncUI(id),
      });
      if (!started) return;
      addToast({ type: "success", message: t("agentList.agentStartedDebug") });
    } catch (e: any) {
      addToast({ type: "error", message: e?.message ?? String(e) });
    }
  };

  const handleStop = async (agentId: string) => {
    const agent = agentsMap[agentId]?.meta;
    setConfirmDialog({
      open: true,
      title: t("agentList.titleStopAgent"),
      message: t("agentList.stopConfirm", { agent: agent?.name ?? agentId }),
      confirmLabel: t("agentList.confirmStop"),
      destructive: true,
      onConfirm: async () => {
        setConfirmDialog((prev) => ({ ...prev, open: false }));
        try {
          await stopAgent(agentId);
          addToast({ type: "success", message: t("agentList.agentStopped") });
        } catch (e) {
          addToast({ type: "error", message: t("agentList.errorFailedToStopAgent", { error: String(e) }) });
        }
      },
    });
  };

  const handleUninstall = (agentId: string) => {
    // ADR-077: System Agent is a regular bundled agent; no uninstall
    // guard. Removing it goes through the same confirm dialog as every
    // other agent.
    const agent = agentsMap[agentId]?.meta;
    setConfirmDialog({
      open: true,
      title: t("agentList.titleUninstallAgent"),
      message: t("agentList.uninstallConfirm", { agent: agent?.name ?? agentId }),
      confirmLabel: t("agentList.confirmUninstall"),
      destructive: true,
      onConfirm: async () => {
        setConfirmDialog((prev) => ({ ...prev, open: false }));
        try {
          await uninstallAgent(agentId);
          addToast({ type: "success", message: t("agentList.agentUninstalled") });
        } catch (e) {
          addToast({ type: "error", message: t("agentList.errorFailedToUninstallAgent", { error: String(e) }) });
        }
      },
    });
  };

  // Open the unified context menu. `useContextMenu.openAt` handles
  // preventDefault / stopPropagation / payload capture / selection snapshot
  // — see src/components/common/ContextMenu/useContextMenu.ts.
  const handleContextMenu = useCallback(
    (e: React.MouseEvent, agentId: string) => {
      agentMenu.openAt(e, { agentId });
    },
    [agentMenu],
  );

  const contextAgent = agentMenu.payload?.agentId
    ? agentsMap[agentMenu.payload.agentId]?.meta
    : undefined;

  // Memoised menu items. Built only when the resolved `contextAgent`
  // changes (so the Start / Stop / Uninstall variants flip correctly when
  // the user right-clicks a different agent) or when translations change.
  const agentMenuItems = useMemo<ContextMenuItem<{ agentId: string }>[]>(() => {
    const aid = agentMenu.payload?.agentId;
    if (!aid) return [];
    const items: ContextMenuItem<{ agentId: string }>[] = [];

    if (contextAgent && !contextAgent.alive) {
      items.push({
        key: "start",
        icon: <Play size={14} />,
        label: t("agentList.contextStart"),
        onClick: ({ payload }) => payload && handleStart(payload.agentId),
      });
      items.push({
        key: "start-debug",
        icon: <Bug size={14} />,
        label: t("agentList.contextStartInDebug"),
        variant: "warning",
        onClick: ({ payload }) => payload && handleDebugStart(payload.agentId),
      });
    }
    if (contextAgent && contextAgent.alive) {
      items.push({
        key: "stop",
        icon: <Square size={14} />,
        label: t("agentList.contextStop"),
        onClick: ({ payload }) => payload && handleStop(payload.agentId),
      });
    }
    items.push({
      key: "details",
      icon: <Info size={14} />,
      label: t("agentList.contextDetails"),
      onClick: ({ payload }) => payload && setDetailAgentId(payload.agentId),
    });
    // ADR-087: permissions entry — hidden for callers without manage on
    // this agent. `can_manage` absent/null = pre-087 Gateway or Local
    // single-user mode, both of which are manageable → show.
    if (contextAgent && contextAgent.can_manage !== false) {
      items.push({
        key: "permissions",
        icon: <UserCog size={14} />,
        label: t("agentList.contextPermissions"),
        onClick: ({ payload }) => {
          if (!payload || !contextAgent) return;
          setPermTarget({
            kind: "agent",
            // Instance key (ADR-073) — the ownership row is keyed by it.
            id: payload.agentId,
            name: contextAgent.display_name ?? contextAgent.name,
          });
        },
      });
    }
    items.push({
      key: "clone",
      icon: <Copy size={14} />,
      label: t("agentList.contextClone"),
      onClick: () => {
        if (!contextAgent) return;
        setCloneSource({
          // ADR-073: clone source is instance-scoped — the Gateway route
          // resolves through the installed table, so use the row's instance
          // key (aid) and never the package `agent_id` (ambiguous in
          // multi-instance deployments). The package id travels alongside
          // for naming/display only.
          agentId: aid,
          packageId: contextAgent.agent_id,
          agentName: contextAgent.display_name ?? contextAgent.name,
        });
      },
    });
    items.push({
      key: "publish",
      icon: <Package size={14} />,
      label: t("agentList.contextPublish"),
      onClick: () => {
        if (!contextAgent) return;
        setPublishTarget({
          // ADR-073: publish prepare/execute and avatar upload are
          // instance-scoped routes — use the row's instance key (aid).
          agentId: aid,
          agentName: contextAgent.display_name ?? contextAgent.name,
        });
      },
    });
    // ADR-077: System Agent gets the same context menu as every other
    // agent — including Uninstall. No special-case omission.
    if (contextAgent) {
      items.push({
        key: "uninstall",
        icon: <Trash2 size={14} />,
        label: t("agentList.contextUninstall"),
        variant: "danger",
        dividerBefore: true,
        onClick: ({ payload }) => payload && handleUninstall(payload.agentId),
      });
    }
    return items;
    // contextAgent is the only signal that changes which items appear;
    // handlers are stable references from React state machinery below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentMenu.payload?.agentId, contextAgent, t]);
  const filteredAgents = agentsList.filter((a) =>
    a.name.toLowerCase().includes(searchQuery.toLowerCase()),
  );

  // ADR-073 §4: in remote mode we partition agents by `node_id` so each
  // group renders below a collapsible 1/3-height header. Groups are
  // emitted in `nodes` order (Gateway's natural ordering); any agents
  // whose `node_id` is missing or unknown to the Gateway fall into a
  // trailing "unknown" bucket so they never silently disappear.
  const nodeGroups = useMemo(() => {
    if (!isRemoteMode) return null;
    return partitionAgentsByNode(filteredAgents, nodes);
  }, [isRemoteMode, filteredAgents, nodes]);

  // A search is an agent-first intent ("find agent X"), so agent-less node
  // groups are dropped while one is active — otherwise the matches are
  // buried under empty headers. Outside a search every node is listed, so
  // an empty node stays reachable (e.g. to install its first agent).
  const searching = searchQuery.trim().length > 0;
  const visibleGroups = useMemo(() => {
    if (!nodeGroups) return null;
    return searching ? nodeGroups.filter((g) => g.agents.length > 0) : nodeGroups;
  }, [nodeGroups, searching]);

  // Shared row renderer for both local (flat) and remote (grouped) modes.
  // `total` lets the row compute whether it is the last in its visual
  // scope (so the divider is drawn correctly inside remote groups too).
  const renderAgentItem = (agent: AgentInfo, index: number, total: number) => {
    // ADR-073: the sidebar row is an INSTANCE. `id` is the
    // canonical addressing key (instance_id, with legacy
    // agent_id fallback); `agent.agent_id` is display-only.
    const id = instanceIdOf(agent);
    const sessionTitle = agentsMap[id]?.sessionTitle;
    // ADR-087: set when the Gateway refused this agent's session calls.
    const accessDenied = agentsMap[id]?.accessDenied === true;

    return (
      <div
        key={id}
        className={cn(
          "relative flex cursor-pointer items-center rounded-md px-3 py-2 transition-colors duration-150",
          isCollapsed ? "gap-0" : "gap-3",
          selectedAgentId === id
            ? "bg-[var(--color-accent)]/90 text-white"
            : listRowHover,
          index < total - 1 && "row-divider-b"
        )}
        onClick={() => selectAgent(id)}
        onDoubleClick={() => {
          // Convenience: double-click a stopped agent to start it.
          // Running/starting agents ignore this — use context menu for Stop.
          if (!agent.alive && !startingAgentIds.has(id)) {
            void handleStart(id);
          }
        }}
        title={agent.alive ? undefined : t("agentList.doubleClickToStart")}
        onContextMenu={(e) => handleContextMenu(e, id)}
        role="listitem"
      >
        {/* Avatar */}
        <Tooltip
          content={isCollapsed ? (agent.display_name ?? agent.name) : ""}
          variant="plain"
          position="right"
          delayMs={0}
        >
          <div className="relative inline-flex">
            <AgentAvatar
              agentId={id}
              displayName={agent.display_name ?? agent.name}
              avatarUrl={agent.avatar}
              version={agent.version}
              builtinAvatarId={agent.builtin_avatar}
              size={40}
              className={isCollapsed ? "mx-auto" : ""}
            />
            {/* IM-style "needs attention" indicator dot — solid emerald,
                * borderless, matching the node online/offline dot below
                * (NodeGroupHeader). Shown when the agent is alive AND has
                * at least one session in a non-idle status (streaming /
                * waiting_approval / paused), per ADR-014. Disappears
                * once every session returns to idle. Offline agents
                * (alive === false) show no dot.
                *
                * Auto-sleep was retired in Sept 2026 — the `sleeping`
                * carve-out is gone; an agent that is alive IS running.
                *
                * Hidden on the selected row: the selection highlight fills
                * the row, and an emerald dot on top of it reads as a smudge
                * rather than a status. The row is already fully "marked"
                * by its background; the dot comes back the moment focus
                * moves elsewhere. */}
            {agent.alive && activeAgentIds.has(id) && selectedAgentId !== id && (
                <span
                  className="absolute -bottom-0.5 -right-0.5 h-2.5 w-2.5 rounded-full bg-emerald-500"
                />
              )}
          </div>
        </Tooltip>

        {/* Content area — width-collapsed when sidebar is collapsed to preserve item height */}
        <div className={cn("min-w-0 overflow-hidden", isCollapsed ? "w-0" : "flex-1")}>
            {/* Top row: name */}
            <div className="flex items-center justify-between gap-2">
              <div className="min-w-0 flex items-center gap-1.5">
                {/* `text-xs`, not an inline `var(--ui-font-size)`: every other
                    sidebar list (pm / doc / harness / settings / extensions)
                    renders its row name at `text-xs`, and the agent name was
                    the lone 14px outlier, so the chat column read as a
                    different scale from the rest of the nav. */}
                <span className={cn("truncate font-medium text-xs", selectedAgentId === id ? "text-white" : agent.alive ? "text-text-secondary " : "text-text-tertiary ")}>{agent.display_name ?? agent.name}</span>
              </div>
            </div>
            {/* Bottom row: current session title.
                * `text-10` is the app's standard meta size (same as the pm
                * sidebar's badge / footer lines) and replaces the old
                * `calc(var(--ui-font-size) * 0.85)` magic number, which
                * resolved to ~11.9px — a size nothing else used.
                * min-height + animate-pulse skeleton locks the row height so
                * the agent name above does not jump when the async session
                * title loads. The min-height stays a calc() so it keeps
                * tracking --ui-font-size through the em-based token. */}
            <div
              className="mt-0.5 flex items-center text-10"
              style={{
                minHeight: "calc(var(--ui-font-size, 0.875rem) * 0.85 * 1.5)",
              }}
            >
              {agent.alive ? (
                accessDenied ? (
                  // ADR-087: the Gateway refused every session call for
                  // this agent. Showing the pulse skeleton here was
                  // indistinguishable from "still loading" — the row
                  // animated forever while nothing could ever arrive.
                  <span
                    className={cn(
                      "block truncate",
                      selectedAgentId === id
                        ? "text-white/50"
                        : "text-text-tertiary/70",
                    )}
                  >
                    {t("agentList.noAccess")}
                  </span>
                ) : sessionTitle === undefined ? (
                  <span
                    aria-hidden
                    className={cn(
                      "block h-2.5 w-2/3 animate-pulse rounded",
                      selectedAgentId === id
                        ? "bg-modal-surface/40"
                        : "bg-zinc-300/60 dark:bg-zinc-600/60",
                    )}
                  />
                ) : (
                  <span
                    className={cn(
                      "block truncate",
                      selectedAgentId === id
                        ? "text-white/70"
                        : "text-text-tertiary ",
                    )}
                  >
                    {sessionTitle === null ? (
                      <span aria-label="agent idle" className="inline-flex items-baseline">
                        <span className="zzz-n">z</span>
                        <span className="zzz-n">z</span>
                        <span className="zzz-n">z</span>
                        <span className="zzz-n">z</span>
                        <span className="zzz-n">z</span>
                      </span>
                    ) : (sessionTitle || t("sessionTabBar.untitled"))}
                  </span>
                )
              ) : (
                // Stopped agent — render the idle animation directly
                // rather than the loading skeleton. A stopped agent will
                // never have its sessionTitle populated by the backend
                // (Runtime HTTP server is not listening), so the
                // `undefined → skeleton` branch would otherwise stay
                // stuck forever, misleading the user into thinking a
                // session is still being fetched.
                <span
                  aria-label="agent stopped"
                  className={cn(
                    "block truncate",
                    selectedAgentId === id
                      ? "text-white/70"
                      : "text-text-tertiary ",
                  )}
                >
                  <span className="inline-flex items-baseline">
                    <span className="zzz-n">z</span>
                    <span className="zzz-n">z</span>
                    <span className="zzz-n">z</span>
                    <span className="zzz-n">z</span>
                    <span className="zzz-n">z</span>
                  </span>
                </span>
              )}
            </div>
          </div>
      </div>
    );
  };

  return (
    <div
      className="flex flex-col shrink-0 bg-nav-surface rounded-xl border-r border-agentlist-border"
      style={{ width: width ?? 240 }}
    >
      {/* ADR-087 follow-up: remote mode + signed-in + machine not enrolled
          → ask for the one explicit action that starts everything. Hides
          itself the moment enrollment lands (refreshLocalNode after
          create); no dismiss control — a closable first-run reminder
          would just re-appear every launch. */}
      {!isCollapsed && localNode && !localNode.enrolled && (
        <div className="flex items-start gap-2 border-b border-amber-300/60 bg-amber-50/80 px-3 py-2 text-xs text-amber-900 backdrop-blur-sm dark:border-amber-700/60 dark:bg-amber-950/40 dark:text-amber-100">
          <Server className="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber-500 dark:text-amber-300" />
          <span className="min-w-0 flex-1">{t("agentList.localNodeBanner")}</span>
          <button
            onClick={() => void handleCreateLocalNode()}
            disabled={creatingNode}
            className="shrink-0 rounded-md px-2 py-0.5 text-xs font-medium transition-colors hover:bg-amber-200/60 disabled:opacity-50 dark:hover:bg-amber-900/60"
          >
            {t("agentList.createLocalNode")}
          </button>
        </div>
      )}
      {/* Header — search input */}
      {/* Search band: `min-h-[var(--ui-list-header-h)]` + `items-center`
          matches pm / extensions / doc / settings, whose search boxes sit in
          the same token-sized band. The ad-hoc `px-3 py-2` made this
          header taller than its siblings at the same global font size, so
          the chat column's search box read as oversized next to pm's.
          Collapsed keeps its own tighter padding (icon-only mode).
          `text-xs` here is what actually closes the remaining gap: the pm /
          extensions / doc panes set `text-xs` on the PANE root, and since
          every `--text-*` token is em-based (relative to the parent), their
          `StyledInput` — which is itself `text-xs` — computes 0.857 × 12 ≈
          10.3px. This column cannot carry `text-xs` on its root (the
          ConfirmDialog / PublishWizard it renders would shrink with it), so
          the band takes the step locally and the box matches its siblings
          byte for byte. Row names are unaffected — they already declare
          `text-xs` against the 14px base, i.e. the same 12px pm's rows
          inherit. */}
      <div className={cn(
        "flex min-h-[var(--ui-list-header-h)] items-center border-b border-border-divider text-xs",
        isCollapsed ? "px-1.5" : "px-3",
      )}>
        <div className="relative min-w-0 flex-1">
          <Search
            className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-text-tertiary "
          />
          <StyledInput
            type="text"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            placeholder={isCollapsed ? "" : t("agentList.searchPlaceholder")}
            aria-label={t("agentList.searchPlaceholder")}
            className={cn(
              "rounded-md bg-input-bg pl-7 py-1.5",
              isCollapsed ? "min-w-0 pr-0" : "pr-2",
            )}
          />
        </div>
      </div>

      {/* Agent list */}
      <div className="flex-1 overflow-y-auto overflow-x-hidden" role="list" aria-label={t("agentList.ariaLabelAgentList")}>

        {loading && agentsList.length === 0 && (
          <div className="flex items-center justify-center py-8">
            <div className="h-5 w-5 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
          </div>
        )}

        {visibleGroups
          ? visibleGroups.map((group) => {
              const collapsed = collapsedNodes.has(group.nodeId);
              const nodeOnline = group.node?.online ?? false;
              return (
                <Fragment key={group.nodeId}>
                  <NodeGroupHeader
                    nodeName={nodeDisplayName(group)}
                    online={nodeOnline}
                    statusLabel={t(
                      nodeOnline ? "settings.nodesOnline" : "settings.nodesOffline",
                    )}
                    collapsed={collapsed}
                    onToggle={() => toggleNode(group.nodeId)}
                    // ADR-087: `can_manage` undefined = pre-087 Gateway →
                    // treat as manageable (show); false = explicitly denied.
                    canManage={group.node?.can_manage !== false}
                    onManagePermissions={() =>
                      setPermTarget({
                        kind: "node",
                        id: group.nodeId,
                        name: nodeDisplayName(group),
                      })
                    }
                    permissionsLabel={t("agentList.nodePermissions")}
                  />
                  {/* ADR-087 D5: install is a NODE-manage action
                      (`check_node_manage` in the install handler), so the
                      entry must not be offered to a caller who cannot use
                      it. `public` nodes are visible to everyone but are
                      still not manageable — previously this button showed
                      regardless and only failed on click. */}
                  {!collapsed &&
                    group.agents.length === 0 &&
                    group.node?.can_manage !== false && (
                      <button
                        type="button"
                        onClick={() => void doInstall(group.nodeId)}
                        disabled={!nodeOnline || installing}
                        title={t("agentList.installAgent")}
                        data-testid="node-group-install"
                        className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-text-tertiary transition-colors hover:bg-nav-item-hover hover:text-text-secondary disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent"
                      >
                        <Plus className="h-3.5 w-3.5 shrink-0" />
                        <span className="truncate">{t("agentList.installAgent")}</span>
                      </button>
                    )}
                  {!collapsed &&
                    group.agents.map((agent, index) => renderAgentItem(agent, index, group.agents.length))}
                </Fragment>
              );
            })
          : filteredAgents.map((agent, index) => renderAgentItem(agent, index, filteredAgents.length))}

{!loading &&
          (visibleGroups ? visibleGroups.length === 0 : filteredAgents.length === 0) && (
            <div className="px-3 py-8 text-center text-xs text-text-tertiary ">
              {agentsList.length === 0 ? t("agentList.noAgentsInstalled") : t("agentList.noMatchingAgents")}
            </div>
          )}

        {/* ADR-076 §决策 7: account group, below the agent groups. */}
        <UserList ref={userListRef} />
      </div>

      <div ref={addMenuRef} className="relative p-1.5">
        <button
          onClick={() => {
            setAddMenuOpen(!addMenuOpen);
            setInstallNodes(null);
          }}
          className="flex w-full items-center justify-center rounded-md px-0 py-[var(--ui-btn-py)] text-xs font-medium text-text-secondary transition-colors hover:bg-nav-control focus-visible:bg-nav-control "
          aria-label={t("agentList.ariaLabelAddAgent")}
        >
          <Plus className="h-3.5 w-3.5" />
        </button>
        {addMenuOpen && (
          <div className="absolute bottom-full left-1 z-50 mb-1 w-max rounded-md border border-border-outer bg-modal-surface py-1 shadow-lg">
            {installNodes !== null ? (
              <>
                <div className="px-3 py-1.5 text-10 font-medium uppercase tracking-wide text-text-tertiary">
                  {t("agentList.selectNode")}
                </div>
                {installNodes.map((node) => (
                  <button
                    key={node.node_id}
                    onClick={() => {
                      setAddMenuOpen(false);
                      setInstallNodes(null);
                      void doInstall(node.node_id);
                    }}
                    className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-text-secondary transition-colors hover:bg-[var(--color-accent)]/5 dark:hover:bg-[var(--color-accent)]/10"
                  >
                    {/* Node install picker only lists online nodes, so the
                        dot renders solid emerald — same online/offline
                        badge as the group header (gray when offline). */}
                    <span className="h-2 w-2 rounded-full bg-emerald-500" />
                    {node.node_name ?? node.hostname ?? node.node_id}
                    <span className="ml-auto text-text-tertiary">
                      {node.os ?? ""} {node.arch ?? ""}
                    </span>
                  </button>
                ))}
                <button
                  onClick={() => setInstallNodes(null)}
                  className="flex w-full items-center gap-2 border-t border-border-divider px-3 py-1.5 text-xs text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/5 dark:hover:bg-[var(--color-accent)]/10"
                >
                  {t("agentList.back")}
                </button>
              </>
            ) : (
              <>
                <button
                  onClick={() => {
                    setAddMenuOpen(false);
                    setShowCreateWizard(true);
                  }}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-text-secondary transition-colors hover:bg-[var(--color-accent)]/5 dark:hover:bg-[var(--color-accent)]/10"
                >
                  <Sparkles className="h-3.5 w-3.5" />
                  {t("agentList.createAgent")}
                </button>
                <button
                  onClick={() => {
                    void handleInstall();
                  }}
                  disabled={installing}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-text-secondary transition-colors hover:bg-[var(--color-accent)]/5 dark:hover:bg-[var(--color-accent)]/10"
                >
                  <Plus className="h-3.5 w-3.5" />
                  {t("agentList.installAgent")}
                </button>
                {/* ADR-087: only meaningful in multi-user mode — in local
                    mode the Gateway already auto-spawns the local node and
                    there is no account to bind an owner to. */}
                {selfAccount && (
                  <button
                    onClick={() => {
                      void handleCreateLocalNode();
                    }}
                    disabled={creatingNode}
                    className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-text-secondary transition-colors hover:bg-[var(--color-accent)]/5 dark:hover:bg-[var(--color-accent)]/10"
                  >
                    <Server className="h-3.5 w-3.5" />
                    {t("agentList.createLocalNode")}
                  </button>
                )}
                {canInviteUser && (
                  <button
                    onClick={() => {
                      setAddMenuOpen(false);
                      userListRef.current?.openCreate();
                    }}
                    data-testid="add-menu-create-user"
                    className="flex w-full items-center gap-2 border-t border-border-divider px-3 py-1.5 text-xs text-text-secondary transition-colors hover:bg-[var(--color-accent)]/5 dark:hover:bg-[var(--color-accent)]/10"
                  >
                    <Plus className="h-3.5 w-3.5" />
                    {t("account.createAccount")}
                  </button>
                )}
              </>
            )}
          </div>
        )}
      </div>

      {/* Unified context menu — items only depend on the right-clicked agent. */}
      <ContextMenu<{ agentId: string }>
        isOpen={agentMenu.isOpen}
        menuProps={agentMenu.menuProps}
        items={agentMenuItems}
        payload={agentMenu.payload}
        selectionAtOpen={agentMenu.selectionAtOpen}
        onClose={agentMenu.close}
      />

      {/* Confirm dialog */}
      <ConfirmDialog
        open={confirmDialog.open}
        title={confirmDialog.title}
        message={confirmDialog.message}
        confirmLabel={confirmDialog.confirmLabel}
        destructive={confirmDialog.destructive}
        onConfirm={confirmDialog.onConfirm}
        onCancel={() => setConfirmDialog((prev) => ({ ...prev, open: false }))}
      />

      {/* Agent detail dialog */}
      <AgentDetailDialog
        open={!!detailAgentId}
        agentId={detailAgentId}
        onClose={() => setDetailAgentId(null)}
      />

      {/* ADR-087: agent / node permissions dialog */}
      <PermissionDialog
        open={!!permTarget}
        target={permTarget}
        onClose={() => setPermTarget(null)}
        // ADR-075 D4: a rename lands in the node's identity.json and the
        // node republishes its info snapshot, so the Gateway's view is
        // already correct — re-pull it so the group header above stops
        // rendering the old name.
        onRenamedNode={() => void useAgentStore.getState().fetchNodes()}
      />

      {/* Clone dialog */}
      <CloneDialog
        open={!!cloneSource}
        agentId={cloneSource?.agentId ?? ""}
        packageId={cloneSource?.packageId ?? ""}
        agentName={cloneSource?.agentName ?? ""}
        onCloned={(result: CloneResponse) => {
          setCloneSource(null);
          addToast({ type: "success", message: t("agentList.agentCloned", { agentId: result.agent_id }) });
          void fetchAgents().then(() => {
            // ADR-073: select by INSTANCE identity — the clone response
            // carries the new package id, so match the freshly installed
            // row through the manifest agent_id and select its instance key.
            const entry = Object.entries(useAgentStore.getState().agents).find(
              ([, s]) => s.meta.agent_id === result.agent_id,
            );
            if (entry) selectAgent(entry[0]);
          });
        }}
        onClose={() => setCloneSource(null)}
      />

      {/* Publish wizard */}
      <PublishWizard
        open={!!publishTarget}
        agentId={publishTarget?.agentId ?? ""}
        agentName={publishTarget?.agentName ?? ""}
        onClose={() => setPublishTarget(null)}
      />

      {/* Create wizard */}
      <CreateWizard
        open={showCreateWizard}
        onCreated={(agentId) => {
          setShowCreateWizard(false);
          addToast({ type: "success", message: t("agentList.agentCreated", { agentId }) });
          void fetchAgents().then(() => {
            selectAgent(agentId);
          });
        }}
        onClose={() => setShowCreateWizard(false)}
      />
    </div>
  );
}

/**
 * ADR-073 §4: collapsible group header shown above each node bucket in
 * remote-mode view — compact (h-6 = 24px vs the agent row's ~56px), no
 * background, bordered top and bottom with the same divider color the
 * agent rows use, plus a chevron + the node display name. Click anywhere
 * on the row to toggle; default state is collapsed=false (expanded).
 *
 * The leading dot doubles as the node's online/offline badge: solid
 * emerald while the node's MQTT session is alive, solid gray once the
 * Gateway has marked it offline. `statusLabel` is surfaced through the
 * native `title` tooltip so the dot's meaning is discoverable on hover.
 */
interface NodeGroupHeaderProps {
  nodeName: string;
  online: boolean;
  statusLabel: string;
  collapsed: boolean;
  onToggle: () => void;
  /** ADR-087: caller may manage this node → show the permissions icon. */
  canManage: boolean;
  /** Opens the node permissions dialog. */
  onManagePermissions: () => void;
  /** Tooltip for the permissions icon. */
  permissionsLabel: string;
}

function NodeGroupHeader({
  nodeName,
  online,
  statusLabel,
  collapsed,
  onToggle,
  canManage,
  onManagePermissions,
  permissionsLabel,
}: NodeGroupHeaderProps) {
  return (
    <div
      data-testid="node-group-header"
      className={cn(
        // h-6 (24px) — a touch taller than a third of the agent row's
        // ~56px, so the node label has comfortable breathing room.
        "flex h-6 w-full items-center text-left",
        // `text-xs` + normal case, matching the agent / user row names
        // right below it. This header was `text-10 uppercase tracking-wide`
        // — nominally a size SMALLER than the rows, but caps + letter
        // spacing inflated its visual mass, so the group row read heavier
        // than the names it introduces. A node name is also an identifier
        // (`node-01`, host names), not a section label, so it must not be
        // case-folded either.
        "text-xs font-medium",
        "text-text-tertiary ",
        "hover:text-zinc-600 dark:hover:text-zinc-300",
        "transition-colors duration-150",
        // Same fill as the right-panel card title bars (ListBox's
        // `bg-panel-block`), one step above `nav-surface`, so the node
        // strip reads as a raised band instead of a bare hairline on the
        // sidebar. Both themes flip via the token — no `dark:` pair.
        "bg-panel-block",
        // Dedicated divider on BOTH edges so the header reads as its own
        // row when it sits between agent rows / above an empty group.
        "border-y border-nav-divider/40 dark:border-zinc-600/40",
      )}
    >
      {/* Collapse toggle — a real button filling the row; the permissions
          icon sits beside it (buttons must not nest). */}
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={!collapsed}
        aria-label={`Toggle node group: ${nodeName}`}
        title={statusLabel}
        className="flex min-w-0 flex-1 items-center gap-1.5 px-3 py-1 text-left"
      >
        <ChevronRight
          className={cn(
            "h-3 w-3 shrink-0 transition-transform duration-150",
            !collapsed && "rotate-90",
          )}
        />
        <span
          className={cn(
            "h-1.5 w-1.5 shrink-0 rounded-full",
            online ? "bg-emerald-500" : "bg-zinc-400 dark:bg-zinc-500",
          )}
          aria-hidden
        />
        <span className="truncate">{nodeName}</span>
      </button>
      {/* ADR-087: node permissions entry — hidden unless the caller may
          manage this node (owner / guest / admin; Local mode counts). */}
      {canManage && (
        <button
          type="button"
          onClick={onManagePermissions}
          title={permissionsLabel}
          aria-label={permissionsLabel}
          data-testid="node-permissions-btn"
          className="mr-2 flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-text-tertiary transition-colors hover:bg-nav-item-hover hover:text-zinc-600 dark:hover:text-zinc-300"
        >
          {/* ponytail: icon only shrinks to 90% (14 -> 12.6px) for a lighter
              glyph; the button box stays h-6 w-6 so padding + hit area are
              unchanged. Swap back to h-3.5 w-3.5 if the row density changes. */}
          <Settings className="h-[12.6px] w-[12.6px]" />
        </button>
      )}
    </div>
  );
}
