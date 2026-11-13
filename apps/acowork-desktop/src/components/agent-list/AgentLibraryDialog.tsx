/**
 * Agent library dialog — the single "install an Agent" surface.
 *
 * PRD: docs/prd/zh/prd-agent-hub-install-dialog.md. Two entries (the sidebar
 * `+` menu and an empty node group) open THIS component; the target node is
 * chosen in the footer, not in the menu (D4).
 *
 * Two tabs (PRD §14 / D9):
 *   - "本地" is the machine's agent FILE LIBRARY and the default tab: one row
 *     per `.agent` in the user's library directory, plus a fixed trailing
 *     "+ add an agent file" row. It is rendered from a local directory read, so
 *     it opens with no network request at all (REQ-021/A2).
 *   - "Agent 库" renders the Hub catalog, which only Rust ever fetches
 *     (lib/agentHub.ts explains the content-type trap). Its load is kicked off
 *     when the dialog opens and is never awaited (REQ-022) — a dead Hub must
 *     not delay or break the Local tab.
 *
 * The trailing add row keeps the v0.1 pick-a-file path unchanged: the picked
 * file is installed straight from disk through `/install` and is NOT copied
 * into the library (A5). Only rows that are already in the library go through
 * `ensure` (REQ-026).
 *
 * Drop handling rides the window-level `desktop://file-drop` channel rather
 * than HTML5 `dataTransfer`, because the install path reads a file from an
 * absolute path and the webview never sees a real path from `dataTransfer`.
 * While this dialog is mounted it claims that channel so a dropped package
 * does not ALSO land in the chat as an attachment (lib/fileDropClaim.ts).
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Loader2, MoreHorizontal, Search, ShieldAlert, X } from "lucide-react";
import { open as openFilePicker } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { cn } from "../../lib/utils";
import { log } from "../../lib/logger";
import { useTranslation } from "../../i18n/useTranslation";
import { useAgentStore } from "../../stores/agentStore";
import { useToast } from "../common/ToastProvider";
import { useEscapeClose } from "../../hooks/useEscapeClose";
import { StyledInput } from "../common/StyledInput";
import { Dropdown } from "../common/Dropdown";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { ErrorBox } from "../common/ErrorBox";
import { NoNodeBanner } from "../nodes/NoNodeBanner";
import { useNodeTopologyRefresh } from "../nodes/useNodeTopologyRefresh";
import { usableNodesOf, noNodeDetailOf, noNodeDetailKey } from "../nodes/nodeAvailability";
import { ContextMenu, useContextMenu, type ContextMenuItem } from "../common/ContextMenu";
import { claimFileDrop, releaseFileDrop } from "../../lib/fileDropClaim";
import {
  countInstancesOnNode,
  ensureLocalAgent,
  fetchHubIndex,
  formatSizeBytes,
  highRiskItemsOf,
  hubAssetUrl,
  hubAvatarInitialOf,
  hubChipsOf,
  hubDeprecationOf,
  hubDisplayNameOf,
  installFromHub,
  inspectLocalPackage,
  listLocalAgentPackages,
  searchHubAgents,
  type HubAgent,
  type LocalAgentList,
  type LocalAgentRow,
  type LocalPackageInfo,
} from "../../lib/agentHub";
import { LocalAgentLibrary } from "./LocalAgentLibrary";

type Tab = "library" | "local";

/**
 * What a row's "⋯" menu acts on. The Hub card and the local file row choose
 * the same item (`installAnother`) through different APIs — `/install` from the
 * catalog entry, and the file path from the library — so the payload has to
 * keep them apart.
 */
type RowMenuPayload = { kind: "hub"; agentId: string } | { kind: "local"; path: string };

/** Identity for this dialog in the window-level drop registry. */
const DROP_OWNER = "agent-library-dialog";

/**
 * `installingId` slot for the Local tab's footer button — Hub cards key on
 * their package id, the local path has none until a file is picked.
 */
const LOCAL_INSTALL_KEY = "__local__";

interface AgentLibraryDialogProps {
  open: boolean;
  /** Node pre-selected by the entry point (an empty node group). */
  initialNodeId?: string;
  onClose: () => void;
}

export function AgentLibraryDialog({ open, initialNodeId, onClose }: AgentLibraryDialogProps) {
  const { t } = useTranslation();
  const { addToast } = useToast();
  const nodes = useAgentStore((s) => s.nodes);
  const agentsMap = useAgentStore((s) => s.agents);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const selectAgent = useAgentStore((s) => s.selectAgent);

  // ADR-113 D6: this dialog *decides* whether an install is possible, so it
  // must read the current protocol state instead of trusting the store's
  // mount snapshot. On a cold start the Gateway-spawned node can appear
  // after `AgentList` already fetched an empty list; without this the UI
  // stays on "No available node" until some unrelated action refreshes it.
  useNodeTopologyRefresh(open);

  // D9 / REQ-021: Local is the default — the first screen must not depend on
  // a network round-trip.
  const [tab, setTab] = useState<Tab>("local");
  const [query, setQuery] = useState("");

  // Catalog: `null` = never loaded, `[]` = loaded and genuinely empty.
  // Those are different states (REQ-012) and must not share a render branch.
  const [catalog, setCatalog] = useState<HubAgent[] | null>(null);
  const [baseUrl, setBaseUrl] = useState("");
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [loadingCatalog, setLoadingCatalog] = useState(false);

  // Recovery action for "no usable node". The guide's open/closed state
  // lives inside the banner so it resets whenever the banner unmounts.
  const [startingNode, setStartingNode] = useState(false);

  const [busy, setBusy] = useState(false);
  // Which action `busy` belongs to. Install is serialized (REQ-014), but the
  // in-flight label has to stay on the button the user pressed — painting every
  // card "Installing" reads as N parallel installs and hides which one moved.
  const [installingId, setInstallingId] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [confirmTarget, setConfirmTarget] = useState<{ agent: HubAgent; ensure: boolean } | null>(null);
  const [targetNodeId, setTargetNodeId] = useState<string | null>(null);
  const [localPath, setLocalPath] = useState<string | null>(null);
  const [localInfo, setLocalInfo] = useState<LocalPackageInfo | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);
  // The library listing: `null` = not answered yet, so the first paint can be
  // a skeleton instead of a premature "empty" (REQ-028).
  const [libraryList, setLibraryList] = useState<LocalAgentList | null>(null);
  const [libraryError, setLibraryError] = useState<string | null>(null);
  // `path` of the library row whose ensure is in flight.
  const [installingPath, setInstallingPath] = useState<string | null>(null);

  const libraryTabRef = useRef<HTMLButtonElement>(null);
  const localTabRef = useRef<HTMLButtonElement>(null);
  // One menu instance serves both tabs; the payload says which kind of row
  // produced it (a Hub card or a local file row).
  const cardMenu = useContextMenu<RowMenuPayload>();
  // Stable identity (the hook memoizes `close`), so the reset effect below can
  // depend on it without re-running on every render.
  const cardMenuClose = cardMenu.close;
  // Nothing behind this dialog may take a dismiss gesture while the high-risk
  // confirmation is up — it has no focus trap, so Tab can reach the header `×`
  // and clicking it would leave `confirmTarget` set for the next visit.
  const canDismiss = !busy && confirmTarget === null;

  useEscapeClose(open, () => {
    if (canDismiss) onClose();
  }, canDismiss);

  // Reset to a known state on every open — the catalog cache is deliberately
  // kept (session-scoped, lib/agentHub.ts) but nothing else is.
  //
  // `targetNodeId` goes back to `null` on purpose: the component stays mounted
  // behind the `open` flag, so a leftover selection from the previous visit
  // would otherwise short-circuit the seeding effect below and land the user
  // on the wrong node when they open the dialog from an empty node group
  // (REQ-001). Same for `catalogError` — reopening after the Hub comes back
  // must retry, not strand the user on a stale error state (REQ-012). And the
  // same for the pending confirmation / card menu: `if (!open) return null`
  // hides them, so without a reset a confirmation the user never finished in
  // the previous visit reappears on reopen and installs against the *new*
  // target node (REQ-007).
  useEffect(() => {
    if (!open) return;
    setTab("local");
    setQuery("");
    setActionError(null);
    setCatalogError(null);
    setTargetNodeId(null);
    setLocalPath(null);
    setLocalInfo(null);
    setLocalError(null);
    setLibraryError(null);
    setInstallingPath(null);
    setConfirmTarget(null);
    setInstallingId(null);
    setStartingNode(false);
    cardMenuClose();
  }, [open, cardMenuClose]);

  // ── Target node (REQ-008 / REQ-009) ───────────────────────────────────
  // ADR-113: "usable" is pure protocol state — online, control plane
  // ready (NodeReady / bootstrap `node.{id}`), and manageable by this
  // caller. `control_ready` / `can_manage` are optional so a pre-ADR-113
  // Gateway still works: absence defers the final gate to the Gateway's
  // `ensure_node_ready()` + structured retry.
  const usableNodes = useMemo(() => usableNodesOf(nodes), [nodes]);
  // Derived straight from the protocol state, not from the pending target
  // selection — the banner must not flash on the first frame before the
  // seeding effect below has picked a node.
  const noAvailableNode = usableNodes.length === 0;
  const noNodeDetail = useMemo(() => {
    const key = noNodeDetailKey(noNodeDetailOf(nodes));
    return key ? t(key) : null;
  }, [nodes, t]);

  // Seed / repair the selection whenever the topology moves: prefer the
  // entry point's node, then the first online node. `null` means there is
  // no target at all — installs are disabled, never sent without a node_id.
  useEffect(() => {
    if (!open) return;
    if (targetNodeId && usableNodes.some((n) => n.node_id === targetNodeId)) return;
    const preferred =
      initialNodeId && usableNodes.some((n) => n.node_id === initialNodeId)
        ? initialNodeId
        : usableNodes.length > 0
          ? usableNodes[0].node_id
          : null;
    setTargetNodeId(preferred);
  }, [open, usableNodes, initialNodeId, targetNodeId]);

  const targetNode = usableNodes.find((n) => n.node_id === targetNodeId) ?? null;
  // The selection can briefly name a node that has just become unusable
  // (topology moved between two refreshes). Read that node from the FULL
  // protocol list — looking only inside `usableNodes` would make every
  // refusal branch below unreachable.
  const selectedNode = nodes.find((n) => n.node_id === targetNodeId) ?? null;

  // "No usable target" is a protocol state, not a Gateway-mode condition:
  // an empty node set means the same thing in every topology. The banner
  // explains that state; `targetBlockReason` carries only per-target
  // refusals (REQ-009) so the same sentence is never painted twice.
  const targetBlockReason =
    selectedNode?.can_manage === false
      ? t("agentHub.nodeNotManageable")
      : selectedNode?.control_ready === false
        ? t("agentHub.noNodeDetailNotReady")
        : null;
  const targetBlocked = noAvailableNode || !targetNode || targetBlockReason !== null;
  // Disabled buttons still need a reason on hover. `title` is an attribute,
  // so it does not duplicate the banner's visible text.
  const noNodeReason = noAvailableNode ? t("agentHub.noAvailableNode") : null;

  // ── Catalog load ──────────────────────────────────────────────────────
  const loadCatalog = useCallback(async (force: boolean) => {
    setLoadingCatalog(true);
    setCatalogError(null);
    try {
      const index = await fetchHubIndex({ force });
      setBaseUrl(index.base_url ?? "");
      setCatalog(index.agents ?? []);
    } catch (e) {
      setCatalog(null);
      setCatalogError(String(e));
    } finally {
      setLoadingCatalog(false);
    }
  }, []);

  useEffect(() => {
    if (open && catalog === null && !catalogError && !loadingCatalog) void loadCatalog(false);
  }, [open, catalog, catalogError, loadingCatalog, loadCatalog]);

  const visibleAgents = useMemo(
    () => searchHubAgents(catalog ?? [], query),
    [catalog, query],
  );

  // Instances already living on the target node, per package (REQ-018).
  const instanceEntries = useMemo(
    () =>
      Object.values(agentsMap).map((entry) => ({
        agent_id: entry.meta.agent_id,
        node_id: entry.meta.node_id ?? null,
      })),
    [agentsMap],
  );

  // ── Local library (REQ-021..032) ──────────────────────────────────────
  // One enumeration per open (REQ-032; no watcher, no polling). The command
  // also seeds the directory, so the very first open of a fresh install shows
  // the shipped packages.
  const loadLibrary = useCallback(async () => {
    setLibraryError(null);
    try {
      setLibraryList(await listLocalAgentPackages());
    } catch (e) {
      // IPC-level failure (not a per-file one): keep the empty state and show
      // the raw reason rather than a blank panel (REQ-028).
      setLibraryList({ rows: [], warning: null });
      setLibraryError(String(e));
    }
  }, []);

  useEffect(() => {
    if (!open) return;
    // `null` while in flight so the first paint is a skeleton, not "empty".
    setLibraryList(null);
    void loadLibrary();
  }, [open, loadLibrary]);

  // ── No-node recovery ─────────────────────────────────────────────────
  // Both recovery paths are topology-independent. Starting a node here is
  // a Desktop capability (ADR-087), not a Local-mode feature; the guide
  // explains how to start one elsewhere. This dialog never waits for the
  // node to come online and never chains an install after it.
  const startLocalNode = useCallback(async () => {
    if (startingNode) return;
    setStartingNode(true);
    try {
      // Idempotent: `already_running` means no spawn happened at all. Its
      // result says nothing about whether a node has enrolled yet.
      const res = await invoke<{ already_running?: boolean }>("create_local_node");
      addToast({
        type: "info",
        message: res.already_running
          ? t("agentHub.startLocalNodeAlreadyRunning")
          : t("agentHub.startLocalNodeStarted"),
      });
      // One immediate reconcile; later topology changes reach the store
      // through the bootstrap-driven node refetch (AgentList). A refresh
      // failure is not a start failure — log it, do not block.
      void useAgentStore.getState().fetchNodes().catch((e) => {
        log.warn("[agentLibrary] node list refresh failed:", e);
      });
    } catch (e) {
      // Recovering a node is separate from a failed install, so this never
      // overwrites `actionError`.
      addToast({
        type: "error",
        message: t("agentHub.startLocalNodeFailed", { error: String(e) }),
      });
    } finally {
      setStartingNode(false);
    }
  }, [addToast, startingNode, t]);

  // ── Install ───────────────────────────────────────────────────────────
  const finishInstall = useCallback(
    async (instanceId: string) => {
      await fetchAgents();
      // ADR-073: select by the INSTANCE key the ack returned — the package
      // `agent_id` is ambiguous the moment a node holds two instances.
      if (instanceId) selectAgent(instanceId);
    },
    [fetchAgents, selectAgent],
  );

  const runHubInstall = useCallback(
    async (agent: HubAgent, ensure: boolean) => {
      const pkg = agent.package;
      if (!pkg || !targetNodeId) return;
      setBusy(true);
      setInstallingId(agent.agent_id);
      setActionError(null);
      try {
        const result = await installFromHub({
          agentId: agent.agent_id,
          packageFile: pkg.file,
          sha256: pkg.sha256,
          nodeId: targetNodeId,
          ensure,
        });
        await finishInstall(result.instance_id);
        addToast({
          // `ensure` on a package that is already there created nothing —
          // say so, and do not imply a second install happened (F8).
          type: result.already_present ? "info" : "success",
          message: result.already_present
            ? t("agentHub.alreadyPresent", { name: hubDisplayNameOf(agent) })
            : t("agentHub.installed", { name: hubDisplayNameOf(agent) }),
        });
        onClose();
      } catch (e) {
        // Keep the dialog: the usual next move is picking another node and
        // retrying, which should not require reopening and re-searching.
        setActionError(t("agentHub.errorInstall", { error: String(e) }));
      } finally {
        setBusy(false);
        setInstallingId(null);
      }
    },
    [addToast, finishInstall, onClose, targetNodeId, t],
  );

  const requestHubInstall = useCallback(
    (agent: HubAgent, ensure: boolean) => {
      if (busy || targetBlocked || !agent.package) return;
      const risks = highRiskItemsOf(agent);
      if (risks.length > 0) {
        // Never leave the card menu open underneath the confirmation.
        cardMenu.close();
        setConfirmTarget({ agent, ensure });
        return;
      }
      void runHubInstall(agent, ensure);
    },
    [busy, cardMenu, runHubInstall, targetBlocked],
  );

  const runLocalInstall = useCallback(async () => {
    if (!localPath || busy || targetBlocked || !targetNodeId) return;
    setBusy(true);
    setInstallingId(LOCAL_INSTALL_KEY);
    setActionError(null);
    try {
      const instanceId = await useAgentStore.getState().installAgent(
        localPath,
        targetNodeId,
      );
      // The store already refreshed the inventory, so the only thing left is
      // the selection. ADR-073: select by the instance key the Gateway acked —
      // reverse-looking it up by package id would pick an OLD instance of the
      // same package whenever the node already hosts one (multi-instance is
      // legal), which is exactly what the rule forbids. `null` means a Gateway
      // older than the ack field, where leaving the selection untouched is the
      // only honest move.
      if (instanceId) selectAgent(instanceId);
      addToast({ type: "success", message: t("agentHub.installed", { name: localInfo?.file_name ?? "" }) });
      onClose();
    } catch (e) {
      setActionError(t("agentHub.errorInstall", { error: String(e) }));
    } finally {
      setBusy(false);
      setInstallingId(null);
    }
  }, [addToast, busy, localInfo, localPath, onClose, selectAgent, targetBlocked, targetNodeId, t]);

  const acceptLocalPath = useCallback(
    async (path: string) => {
      setLocalError(null);
      // Rejected client-side first so the message names the file instead of
      // a Rust stack string (REQ-011).
      if (!path.toLowerCase().endsWith(".agent")) {
        setLocalError(t("agentHub.notAgentFile", { name: path.split(/[\\/]/).pop() || path }));
        return;
      }
      try {
        const info = await inspectLocalPackage(path);
        setLocalInfo(info);
        setLocalPath(path);
      } catch (e) {
        setLocalError(String(e));
      }
    },
    [t],
  );

  // A library row's primary button: idempotent `/ensure` for the file at
  // `row.path`. Re-clicking converges on the instance that is already there
  // instead of adding another (D11 / REQ-026) — `already_present` comes from
  // the Gateway, so the toast is its answer, not a guess.
  const runLibraryEnsure = useCallback(
    async (row: LocalAgentRow) => {
      if (busy || targetBlocked || !targetNodeId) return;
      setBusy(true);
      setInstallingPath(row.path);
      setActionError(null);
      try {
        const result = await ensureLocalAgent({ packagePath: row.path, nodeId: targetNodeId });
        // ADR-073 / F8: select the instance the ack named. The package id is
        // ambiguous as soon as the node hosts two instances.
        await finishInstall(result.instance_id);
        const name = row.name ?? row.file_name;
        addToast({
          type: result.already_present ? "info" : "success",
          message: result.already_present
            ? t("agentHub.alreadyPresent", { name })
            : t("agentHub.installed", { name }),
        });
        onClose();
      } catch (e) {
        setActionError(t("agentHub.errorInstall", { error: String(e) }));
      } finally {
        setBusy(false);
        setInstallingPath(null);
      }
    },
    [addToast, busy, finishInstall, onClose, targetBlocked, targetNodeId, t],
  );

  // A library row's "⋯ → install another instance": the NON-idempotent path
  // (REQ-027). Same file, same `install_agent` call the v0.1 local flow used —
  // multi-instance is legal (ADR-073), so this is the one action that is
  // *supposed* to multiply.
  const runLibraryInstallAnother = useCallback(
    async (row: LocalAgentRow) => {
      if (busy || targetBlocked || !targetNodeId) return;
      setBusy(true);
      setInstallingPath(row.path);
      setActionError(null);
      try {
        const instanceId = await useAgentStore
          .getState()
          .installAgent(row.path, targetNodeId);
        if (instanceId) selectAgent(instanceId);
        addToast({
          type: "success",
          message: t("agentHub.installed", { name: row.name ?? row.file_name }),
        });
        onClose();
      } catch (e) {
        setActionError(t("agentHub.errorInstall", { error: String(e) }));
      } finally {
        setBusy(false);
        setInstallingPath(null);
      }
    },
    [addToast, busy, onClose, selectAgent, targetBlocked, targetNodeId, t],
  );

  const pickLocalFile = useCallback(async () => {
    if (busy || targetBlocked) return;
    const selected = await openFilePicker({
      multiple: false,
      filters: [{ name: t("agentList.filterAgentPackage"), extensions: ["agent"] }],
    });
    if (typeof selected === "string" && selected) void acceptLocalPath(selected);
  }, [acceptLocalPath, busy, targetBlocked, t]);

  // ── Window-level drop ownership (REQ-020) ─────────────────────────────
  // `acceptLocalPath` / `tab` are read through refs so this subscription is
  // created once per open instead of being torn down on every keystroke.
  const tabRef = useRef(tab);
  tabRef.current = tab;

  // A window-level drop carries every file the OS handed over, but this dialog
  // installs one package at a time — so name what got dropped instead of
  // discarding it in silence.
  const handleFileDrop = useCallback(
    (paths: string[]) => {
      const path = paths[0];
      if (!path) return;
      const skipped = paths.length - 1;
      if (skipped > 0) {
        addToast({ type: "info", message: t("agentHub.dropOnlyFirst", { count: skipped }) });
      }
      void acceptLocalPath(path);
    },
    [acceptLocalPath, addToast, t],
  );
  const handleDropRef = useRef(handleFileDrop);
  handleDropRef.current = handleFileDrop;

  useEffect(() => {
    if (!open) return;
    claimFileDrop(DROP_OWNER);
    let disposed = false;
    let unlisten: UnlistenFn | undefined;
    (async () => {
      try {
        const fn = await listen<string[]>("desktop://file-drop", (event) => {
          if (tabRef.current !== "local") return;
          handleDropRef.current(event.payload ?? []);
        });
        if (disposed) {
          fn();
          return;
        }
        unlisten = fn;
      } catch {
        // Not running under Tauri (vitest / browser dev) — click-to-pick and
        // the native drop target below still work.
      }
    })();
    return () => {
      disposed = true;
      releaseFileDrop(DROP_OWNER);
      unlisten?.();
    };
  }, [open]);

  // ── Row "⋯" menu (D2: `/install` for an extra instance) ───────────────
  const menuPayload = cardMenu.payload;
  const menuAgent =
    menuPayload?.kind === "hub"
      ? (catalog ?? []).find((a) => a.agent_id === menuPayload.agentId)
      : undefined;
  const menuRow =
    menuPayload?.kind === "local"
      ? (libraryList?.rows ?? []).find((r) => r.path === menuPayload.path)
      : undefined;
  const menuItems = useMemo<ContextMenuItem<RowMenuPayload>[]>(() => {
    if (menuRow) {
      return [
        {
          key: "installAnother",
          label: t("agentHub.installAnother"),
          // A broken file cannot be installed at all — the row says why.
          disabled: busy || targetBlocked || menuRow.error !== null,
          title: targetBlockReason ?? noNodeReason ?? menuRow.error ?? undefined,
          onClick: () => void runLibraryInstallAnother(menuRow),
        },
      ];
    }
    if (!menuAgent) return [];
    return [
      {
        key: "installAnother",
        label: t("agentHub.installAnother"),
        disabled: busy || targetBlocked || !menuAgent.package,
        title: targetBlockReason ?? noNodeReason ?? undefined,
        onClick: () => requestHubInstall(menuAgent, false),
      },
    ];
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    menuAgent,
    menuRow,
    busy,
    targetBlocked,
    targetBlockReason,
    noNodeReason,
    requestHubInstall,
    runLibraryInstallAnother,
    t,
  ]);

  if (!open) return null;

  const onTabKeyDown = (e: React.KeyboardEvent) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    const toLibrary = tab === "local";
    setTab(toLibrary ? "library" : "local");
    (toLibrary ? libraryTabRef : localTabRef).current?.focus();
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={canDismiss ? onClose : undefined} />

      <div
        role="dialog"
        aria-modal="true"
        aria-label={t("agentHub.dialogTitle")}
        className="relative z-10 flex max-h-[85vh] w-full max-w-4xl flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl"
      >
        {/* Header */}
        <header className="flex items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <h3 className="text-sm font-semibold text-text">{t("agentHub.dialogTitle")}</h3>
          <button
            type="button"
            onClick={() => canDismiss && onClose()}
            className="rounded px-1.5 text-sm text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700"
            aria-label={t("common.close")}
          >
            ×
          </button>
        </header>

        {/* Tabs */}
        <div role="tablist" aria-label={t("agentHub.tabLibrary")} className="flex gap-1 border-b border-border-divider px-5">
          <TabButton
            ref={libraryTabRef}
            active={tab === "library"}
            id="agent-hub-tab-library"
            controls="agent-hub-tabpanel"
            label={t("agentHub.tabLibrary")}
            onSelect={() => setTab("library")}
            onKeyDown={onTabKeyDown}
          />
          <TabButton
            ref={localTabRef}
            active={tab === "local"}
            id="agent-hub-tab-local"
            controls="agent-hub-tabpanel"
            label={t("agentHub.tabLocal")}
            onSelect={() => setTab("local")}
            onKeyDown={onTabKeyDown}
          />
        </div>

        {/* Body */}
        <div
          role="tabpanel"
          id="agent-hub-tabpanel"
          aria-labelledby={tab === "library" ? "agent-hub-tab-library" : "agent-hub-tab-local"}
          className="min-h-0 flex-1 overflow-y-auto px-5 py-4"
        >
          {noAvailableNode && (
            <NoNodeBanner
              starting={startingNode}
              onStart={() => void startLocalNode()}
              detail={noNodeDetail}
            />
          )}
          {tab === "library" ? (
            <div className="flex flex-col gap-3">
              <div className="relative">
                <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-text-tertiary" />
                <StyledInput
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  placeholder={t("agentHub.searchPlaceholder")}
                  aria-label={t("agentHub.searchPlaceholder")}
                  className="pl-8"
                />
              </div>

              {loadingCatalog && catalog === null ? (
                <SkeletonGrid />
              ) : catalogError ? (
                <div className="flex flex-col items-center gap-3 py-10 text-center">
                  <AlertTriangle className="h-6 w-6 text-text-tertiary" />
                  <p className="max-w-md text-xs text-text-tertiary">{catalogError}</p>
                  <button
                    type="button"
                    className="rounded-md border border-border-outer px-3 py-1.5 text-xs text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-700"
                    onClick={() => void loadCatalog(true)}
                  >
                    {t("agentHub.retry")}
                  </button>
                </div>
              ) : catalog !== null && visibleAgents.length === 0 ? (
                <div className="flex flex-col items-center gap-2 py-10 text-center">
                  <p className="text-xs text-text-tertiary">
                    {catalog.length === 0 ? t("agentHub.catalogEmpty") : t("agentHub.noResults")}
                  </p>
                  {catalog.length > 0 && (
                    <button
                      type="button"
                      className="text-xs text-[var(--color-accent)] hover:underline"
                      onClick={() => setQuery("")}
                    >
                      {t("agentHub.clearSearch")}
                    </button>
                  )}
                </div>
              ) : (
                <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
                  {visibleAgents.map((agent) => {
                    // Plan A routing: the primary button is idempotent only
                    // while the package has NO instance on the target node.
                    // Once it has one, another click lands another instance —
                    // multi-instance is legal (ADR-073) and "already
                    // installed, nothing happened" reads as a broken button.
                    // With no target there is nothing to count.
                    const instanceCount = targetNodeId
                      ? countInstancesOnNode(instanceEntries, agent.agent_id, targetNodeId)
                      : 0;
                    return (
                      <HubAgentCard
                        key={agent.agent_id}
                        agent={agent}
                        baseUrl={baseUrl}
                        instanceCount={instanceCount}
                        installing={installingId === agent.agent_id}
                        disabled={busy || targetBlocked}
                        disabledReason={targetBlockReason ?? noNodeReason ?? undefined}
                        onInstall={() => requestHubInstall(agent, instanceCount === 0)}
                        onMore={(e) =>
                          cardMenu.openAt(e, { kind: "hub", agentId: agent.agent_id })
                        }
                      />
                    );
                  })}
                </div>
              )}
            </div>
          ) : (
            <div className="flex flex-col gap-3">
              <LocalAgentLibrary
                list={libraryList}
                loadError={libraryError}
                disabled={busy || targetBlocked}
                disabledReason={targetBlockReason ?? noNodeReason ?? undefined}
                busyPath={installingPath}
                onInstall={(row) => void runLibraryEnsure(row)}
                onInstallAnother={(row, e) =>
                  cardMenu.openAt(e, { kind: "local", path: row.path })
                }
                onAddFile={() => void pickLocalFile()}
                pending={
                  localInfo ? (
                    <PickedFileCard
                      info={localInfo}
                      busy={installingId === LOCAL_INSTALL_KEY}
                      disabled={busy || targetBlocked}
                      disabledReason={targetBlockReason ?? noNodeReason ?? undefined}
                      onInstall={() => void runLocalInstall()}
                      onClear={() => {
                        setLocalPath(null);
                        setLocalInfo(null);
                      }}
                    />
                  ) : null
                }
              />
              {localError && (
                <div className="flex items-start gap-2 rounded-md bg-red-50 p-2 text-xs text-red-700 dark:bg-red-900/20 dark:text-red-300">
                  <X className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  {localError}
                </div>
              )}
            </div>
          )}
        </div>

        {/* Footer — install target + action feedback */}
        <div className="flex items-center gap-3 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          {actionError && (
            <div className="min-w-0 flex-1">
              <ErrorBox message={actionError} onClose={() => setActionError(null)} />
            </div>
          )}
          <div className={cn("flex items-center gap-2 text-xs text-text-tertiary", actionError && "shrink-0")}>
            <span>{t("agentHub.targetNode")}</span>
            {usableNodes.length > 1 ? (
              <Dropdown
                value={targetNodeId ?? ""}
                onChange={(v) => setTargetNodeId(v || null)}
                disabled={busy}
                aria-label={t("agentHub.targetNode")}
                size="small"
                className="w-auto rounded-md px-2 py-1 text-xs text-text"
                options={usableNodes.map((node) => ({
                  value: node.node_id,
                  label: node.node_name ?? node.hostname ?? node.node_id,
                }))}
              />
            ) : (
              <span className="text-text-secondary">
                {targetNode
                  ? targetNode.node_name ?? targetNode.hostname ?? targetNode.node_id
                  : t("agentHub.targetNone")}
              </span>
            )}
          </div>
          {targetBlockReason && (
            <span className="flex items-center gap-1.5 text-xs text-text-tertiary">
              <ShieldAlert className="h-3.5 w-3.5 shrink-0" />
              {targetBlockReason}
            </span>
          )}
          {/* Every install commits from the surface it belongs to — a Hub card,
              a library row, or the pending picked-file card — so the footer is
              the dismiss control for both tabs (no second "Install" here: two
              buttons with the same label in one dialog do not say which file
              they commit). */}
          <button
            type="button"
            onClick={onClose}
            disabled={busy || confirmTarget !== null}
            className="ml-auto rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-700"
          >
            {t("common.close")}
          </button>
        </div>
      </div>

      {/* High-risk pre-install confirmation (REQ-007) */}
      <ConfirmDialog
        open={confirmTarget !== null}
        title={t("agentHub.highRiskTitle")}
        // ConfirmDialog renders a plain `<p>`, so the list has to be one
        // line — naming the actual permissions is the point (REQ-007).
        message={`${t("agentHub.highRiskIntro", { name: confirmTarget ? hubDisplayNameOf(confirmTarget.agent) : "" })} ${(confirmTarget ? highRiskItemsOf(confirmTarget.agent) : []).join(", ")}`}
        confirmLabel={t("agentHub.highRiskConfirm")}
        onCancel={() => setConfirmTarget(null)}
        onConfirm={() => {
          const target = confirmTarget;
          setConfirmTarget(null);
          if (target) void runHubInstall(target.agent, target.ensure);
        }}
      />

      <ContextMenu<RowMenuPayload>
        isOpen={cardMenu.isOpen}
        menuProps={cardMenu.menuProps}
        items={menuItems}
        payload={cardMenu.payload}
        selectionAtOpen={cardMenu.selectionAtOpen}
        onClose={cardMenu.close}
        compact
      />
    </div>
  );
}

// ── Sub-views ──────────────────────────────────────────────────────────────

function TabButton({
  ref,
  active,
  id,
  controls,
  label,
  onSelect,
  onKeyDown,
}: {
  ref: React.RefObject<HTMLButtonElement | null>;
  active: boolean;
  id: string;
  controls: string;
  label: string;
  onSelect: () => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
}) {
  return (
    <button
      ref={ref}
      type="button"
      role="tab"
      id={id}
      aria-controls={controls}
      aria-selected={active}
      tabIndex={active ? 0 : -1}
      onClick={onSelect}
      onKeyDown={onKeyDown}
      className={cn(
        "-mb-px border-b-2 px-3 py-2 text-xs font-medium transition-colors",
        active
          ? "border-[var(--color-accent)] text-text"
          : "border-transparent text-text-tertiary hover:text-text-secondary",
      )}
    >
      {label}
    </button>
  );
}

function SkeletonGrid() {
  return (
    <div className="grid grid-cols-1 gap-3 lg:grid-cols-2" aria-busy="true">
      {Array.from({ length: 4 }, (_, i) => (
        <div key={i} className="h-36 animate-pulse rounded-md border border-border-outer bg-panel-block" />
      ))}
    </div>
  );
}

interface HubAgentCardProps {
  agent: HubAgent;
  baseUrl: string;
  instanceCount: number;
  /** This card's own install is in flight — the only one that may render as
   *  "installing". Every other card is disabled instead (REQ-014 serializes
   *  installs; it does not make them all look busy). */
  installing: boolean;
  disabled: boolean;
  disabledReason?: string;
  onInstall: () => void;
  onMore: (e: React.MouseEvent) => void;
}

function HubAgentCard({
  agent,
  baseUrl,
  instanceCount,
  installing,
  disabled,
  disabledReason,
  onInstall,
  onMore,
}: HubAgentCardProps) {
  const { t } = useTranslation();
  const name = hubDisplayNameOf(agent);
  const avatarSrc = hubAssetUrl(baseUrl, agent.avatar);
  const chips = hubChipsOf(agent);
  const deprecation = hubDeprecationOf(agent);
  const noPackage = agent.package === null;
  const blocked = disabled || noPackage;
  // A target-node refusal and an uninstallable entry are different reasons;
  // the button's `title` must say which (REQ-009).
  const blockedReason = disabledReason ?? (noPackage ? t("agentHub.noPackage") : undefined);

  return (
    <article className="flex flex-col gap-2 rounded-md border border-border-outer bg-panel-block p-3">
      <div className="flex items-start gap-3">
        <HubAvatar src={avatarSrc} initial={hubAvatarInitialOf(agent)} name={name} />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <h4 className="truncate text-sm font-semibold text-text">{name}</h4>
            {agent.role && <span className="shrink-0 text-xs text-text-tertiary">{agent.role}</span>}
          </div>
          <p className="mt-0.5 line-clamp-2 text-xs text-text-tertiary">{agent.description}</p>
        </div>
        <button
          type="button"
          onClick={onMore}
          disabled={blocked}
          title={blockedReason}
          aria-label={t("agentHub.moreActions")}
          className="shrink-0 rounded p-1 text-text-tertiary hover:bg-zinc-100 disabled:opacity-40 dark:hover:bg-zinc-700"
        >
          <MoreHorizontal className="h-4 w-4" />
        </button>
      </div>

      {chips.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {chips.map((chip) => (
            <span key={chip} className="rounded bg-zinc-100 px-1.5 py-0.5 text-10 text-text-secondary dark:bg-zinc-700">
              {chip}
            </span>
          ))}
        </div>
      )}

      {deprecation.flagged && (
        <div className="flex items-start gap-1.5 rounded-md bg-amber-50 px-2 py-1 text-xs text-amber-700 dark:bg-amber-900/20 dark:text-amber-300">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>{deprecation.reason ?? t("agentHub.deprecated")}</span>
        </div>
      )}

      <div className="mt-auto flex items-center justify-between gap-3 border-t border-border-outer pt-2">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-10 text-text-tertiary">
          {agent.package && <span>{formatSizeBytes(agent.package.size_bytes)}</span>}
          {agent.version && <span>v{agent.version}</span>}
          {instanceCount > 0 && (
            <span>{t("agentHub.instancesOnNode", { count: instanceCount })}</span>
          )}
        </div>
        <button
          type="button"
          onClick={onInstall}
          disabled={blocked}
          title={blockedReason}
          className="btn-accent flex shrink-0 items-center gap-1.5 rounded px-2.5 py-1 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
        >
          {installing ? <Loader2 className="h-3 w-3 animate-spin" /> : null}
          {installing
            ? t("agentHub.installing")
            : instanceCount > 0
              ? t("agentHub.installAnother")
              : t("agentHub.install")}
        </button>
      </div>
    </article>
  );
}

function HubAvatar({ src, initial, name }: { src: string | null; initial: string; name: string }) {
  const [failed, setFailed] = useState(false);
  // Remote avatar, no auth and no CSP in play. Unreachable → the letter
  // placeholder, never a broken-image glyph (REQ-016).
  if (!src || failed) {
    return (
      <div
        aria-label={name}
        className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-zinc-100 text-sm font-semibold text-text-secondary dark:bg-zinc-700"
      >
        {initial}
      </div>
    );
  }
  return (
    <img
      src={src}
      alt={name}
      loading="lazy"
      onError={() => setFailed(true)}
      className="h-9 w-9 shrink-0 rounded-md object-cover"
    />
  );
}

/**
 * The file picked through the trailing "+ add an agent file" row (or dropped).
 *
 * v0.1 behaviour, unchanged (A5): the picked file is installed straight from
 * its own path through `/install` — it is NOT copied into the library, so the
 * library's file count does not move. It is rendered as a pending row above the
 * add entry, and it installs from its own button: the footer is the dismiss
 * control for both tabs, so no install is committed there.
 */
function PickedFileCard({
  info,
  busy,
  disabled,
  disabledReason,
  onInstall,
  onClear,
}: {
  info: LocalPackageInfo;
  busy: boolean;
  disabled: boolean;
  disabledReason?: string;
  onInstall: () => void;
  onClear: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div
      className="flex items-start gap-3 rounded-md border border-[var(--color-accent)]/40 p-3"
      style={{ backgroundColor: "color-mix(in srgb, var(--color-accent) 10%, transparent)" }}
    >
      <div className="min-w-0 flex-1">
        <div className="truncate text-xs font-medium" style={{ color: "var(--color-accent)" }}>
          {info.file_name}
        </div>
        <div className="text-10 text-text-tertiary">
          {formatSizeBytes(info.size_bytes)} · {info.agent_id}
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-1">
        <button
          type="button"
          onClick={onInstall}
          disabled={disabled || busy}
          title={disabledReason}
          className="btn-accent flex items-center gap-1.5 rounded px-2.5 py-1 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy && <Loader2 className="h-3 w-3 animate-spin" />}
          {busy ? t("agentHub.installing") : t("agentHub.install")}
        </button>
        <button
          type="button"
          onClick={onClear}
          aria-label={t("common.close")}
          className="rounded p-1 text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700"
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </div>
    </div>
  );
}
