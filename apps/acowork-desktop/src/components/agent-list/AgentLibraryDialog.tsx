/**
 * Agent library dialog — the single "install an Agent" surface.
 *
 * PRD: docs/prd/zh/prd-agent-hub-install-dialog.md. Two entries (the sidebar
 * `+` menu and an empty node group) open THIS component; the target node is
 * chosen in the footer, not in the menu (D4).
 *
 * Two tabs:
 *   - "Agent 库" renders the Hub catalog, which only Rust ever fetches
 *     (lib/agentHub.ts explains the content-type trap).
 *   - "本地文件" is a landing pad for a `.agent` on disk — the same shape as
 *     the Import Skills dialog: click to pick, or drop.
 *
 * Drop handling rides the window-level `desktop://file-drop` channel rather
 * than HTML5 `dataTransfer`, because the install path reads a file from an
 * absolute path and the webview never sees a real path from `dataTransfer`.
 * While this dialog is mounted it claims that channel so a dropped package
 * does not ALSO land in the chat as an attachment (lib/fileDropClaim.ts).
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, FolderPlus, Loader2, MoreHorizontal, Search, ShieldAlert, X } from "lucide-react";
import { open as openFilePicker } from "@tauri-apps/plugin-dialog";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { cn } from "../../lib/utils";
import { useTranslation } from "../../i18n/useTranslation";
import { useAgentStore } from "../../stores/agentStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useToast } from "../common/ToastProvider";
import { useEscapeClose } from "../../hooks/useEscapeClose";
import { StyledInput } from "../common/StyledInput";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { ErrorBox } from "../common/ErrorBox";
import { ContextMenu, useContextMenu, type ContextMenuItem } from "../common/ContextMenu";
import { claimFileDrop, releaseFileDrop } from "../../lib/fileDropClaim";
import {
  countInstancesOnNode,
  fetchHubIndex,
  formatSizeBytes,
  highRiskItemsOf,
  hubAssetUrl,
  hubAvatarInitialOf,
  hubChipsOf,
  hubDeprecationOf,
  hubDisplayNameOf,
  hubSignatureStatusKey,
  installFromHub,
  inspectLocalPackage,
  searchHubAgents,
  type HubAgent,
  type LocalPackageInfo,
} from "../../lib/agentHub";

type Tab = "library" | "local";

/** Identity for this dialog in the window-level drop registry. */
const DROP_OWNER = "agent-library-dialog";

/**
 * `installingId` slot for the Local tab's footer button — Hub cards key on
 * their package id, the local path has none until a file is picked.
 */
const LOCAL_INSTALL_KEY = "__local__";

/**
 * The node_id the Gateway inventory anchors its own machine's instances on
 * when no enrolled node resolves (ADR-075 D6). Mirrors `AgentInfo.node_id`
 * in lib/types.ts.
 */
const LOCAL_NODE_ANCHOR = "local";

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
  const gatewayMode = useSettingsStore((s) => s.gatewayMode);

  const [tab, setTab] = useState<Tab>("library");
  const [query, setQuery] = useState("");

  // Catalog: `null` = never loaded, `[]` = loaded and genuinely empty.
  // Those are different states (REQ-012) and must not share a render branch.
  const [catalog, setCatalog] = useState<HubAgent[] | null>(null);
  const [baseUrl, setBaseUrl] = useState("");
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [loadingCatalog, setLoadingCatalog] = useState(false);

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

  const libraryTabRef = useRef<HTMLButtonElement>(null);
  const localTabRef = useRef<HTMLButtonElement>(null);
  const cardMenu = useContextMenu<{ agentId: string }>();
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
    setTab("library");
    setQuery("");
    setActionError(null);
    setCatalogError(null);
    setTargetNodeId(null);
    setLocalPath(null);
    setLocalInfo(null);
    setLocalError(null);
    setConfirmTarget(null);
    setInstallingId(null);
    cardMenuClose();
  }, [open, cardMenuClose]);

  // ── Target node (REQ-008 / REQ-009) ───────────────────────────────────
  const onlineNodes = useMemo(() => nodes.filter((n) => n.online), [nodes]);

  // Seed / repair the selection whenever the topology moves: prefer the
  // entry point's node, then the only online node. `null` is the Gateway's
  // default node, which is what Local mode has always installed to.
  useEffect(() => {
    if (!open) return;
    if (targetNodeId && onlineNodes.some((n) => n.node_id === targetNodeId)) return;
    const preferred =
      initialNodeId && onlineNodes.some((n) => n.node_id === initialNodeId)
        ? initialNodeId
        : onlineNodes.length > 0
          ? onlineNodes[0].node_id
          : null;
    setTargetNodeId(preferred);
  }, [open, onlineNodes, initialNodeId, targetNodeId]);

  const targetNode = onlineNodes.find((n) => n.node_id === targetNodeId) ?? null;

  // Why an install cannot start right now, in the user's words. Remote mode
  // with nothing online is a different reason from "you cannot manage this
  // node" and they must not be conflated (REQ-009).
  const targetBlockReason =
    gatewayMode !== "local" && onlineNodes.length === 0
      ? t("agentHub.noOnlineNode")
      : targetNode && targetNode.can_manage === false
        ? t("agentHub.nodeNotManageable")
        : null;
  const targetBlocked = targetBlockReason !== null;

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
      if (!pkg) return;
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
    if (!localPath || busy || targetBlocked) return;
    setBusy(true);
    setInstallingId(LOCAL_INSTALL_KEY);
    setActionError(null);
    try {
      const instanceId = await useAgentStore.getState().installAgent(
        localPath,
        targetNodeId ?? undefined,
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

  // ── Card "⋯" menu (D2: `/install` for an extra instance) ──────────────
  const menuAgent = cardMenu.payload?.agentId
    ? (catalog ?? []).find((a) => a.agent_id === cardMenu.payload?.agentId)
    : undefined;
  const menuItems = useMemo<ContextMenuItem<{ agentId: string }>[]>(() => {
    const agent = menuAgent;
    if (!agent) return [];
    return [
      {
        key: "installAnother",
        label: t("agentHub.installAnother"),
        disabled: busy || targetBlocked || !agent.package,
        title: targetBlockReason ?? undefined,
        onClick: () => requestHubInstall(agent, false),
      },
    ];
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [menuAgent, busy, targetBlocked, targetBlockReason, requestHubInstall, t]);

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
                    // `null` targetNodeId is the Gateway default node, whose
                    // inventory rows are anchored on "local" (ADR-075 D6).
                    const instanceCount = countInstancesOnNode(
                      instanceEntries,
                      agent.agent_id,
                      targetNodeId ?? LOCAL_NODE_ANCHOR,
                    );
                    return (
                      <HubAgentCard
                        key={agent.agent_id}
                        agent={agent}
                        baseUrl={baseUrl}
                        instanceCount={instanceCount}
                        installing={installingId === agent.agent_id}
                        disabled={busy || targetBlocked}
                        disabledReason={targetBlockReason ?? undefined}
                        onInstall={() => requestHubInstall(agent, instanceCount === 0)}
                        onMore={(e) => cardMenu.openAt(e, { agentId: agent.agent_id })}
                      />
                    );
                  })}
                </div>
              )}
            </div>
          ) : (
            <LocalPane
              info={localInfo}
              error={localError}
              onPick={() => void pickLocalFile()}
              dragEnabled={!busy && !targetBlocked}
            />
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
            {onlineNodes.length > 1 ? (
              <select
                value={targetNodeId ?? ""}
                onChange={(e) => setTargetNodeId(e.target.value || null)}
                disabled={busy}
                aria-label={t("agentHub.targetNode")}
                className="rounded-md border border-input-border bg-input-bg px-2 py-1 text-xs text-text outline-none focus:border-[var(--color-accent)]"
              >
                {onlineNodes.map((node) => (
                  <option key={node.node_id} value={node.node_id}>
                    {node.node_name ?? node.hostname ?? node.node_id}
                  </option>
                ))}
              </select>
            ) : (
              <span className="text-text-secondary">
                {targetNode
                  ? targetNode.node_name ?? targetNode.hostname ?? targetNode.node_id
                  : gatewayMode === "local"
                    ? t("agentHub.targetThisComputer")
                    : null}
              </span>
            )}
          </div>
          {targetBlockReason && (
            <span className="flex items-center gap-1.5 text-xs text-text-tertiary">
              <ShieldAlert className="h-3.5 w-3.5 shrink-0" />
              {targetBlockReason}
            </span>
          )}
          {/* The Library tab installs from the card; the Local tab needs an
              explicit commit because its drop zone is only a selector. */}
          {tab === "library" && (
            <button
              type="button"
              onClick={onClose}
              disabled={busy || confirmTarget !== null}
              className="ml-auto rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-700"
            >
              {t("common.close")}
            </button>
          )}
          {tab === "local" && (
            <div className="ml-auto flex items-center gap-2">
              <button
                type="button"
                onClick={onClose}
                disabled={busy || confirmTarget !== null}
                className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-700"
              >
                {t("common.cancel")}
              </button>
              <button
                type="button"
                onClick={() => void runLocalInstall()}
                disabled={busy || !localPath || targetBlocked}
                title={targetBlockReason ?? (localPath ? undefined : t("agentHub.localPickFirst"))}
                className="btn-accent flex items-center gap-2 rounded px-3 py-1.5 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
              >
                {installingId === LOCAL_INSTALL_KEY && <Loader2 className="h-3 w-3 animate-spin" />}
                {installingId === LOCAL_INSTALL_KEY ? t("agentHub.installing") : t("agentHub.install")}
              </button>
            </div>
          )}
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

      <ContextMenu<{ agentId: string }>
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
  const channel = hubSignatureStatusKey(agent);
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
          {channel !== "unknown" && (
            <span>{t(channel === "official" ? "agentHub.channelOfficial" : "agentHub.channelThirdParty")}</span>
          )}
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

interface LocalPaneProps {
  info: LocalPackageInfo | null;
  error: string | null;
  dragEnabled: boolean;
  onPick: () => void;
}

function LocalPane({ info, error, dragEnabled, onPick }: LocalPaneProps) {
  const { t } = useTranslation();
  const [dragOver, setDragOver] = useState(false);
  return (
    <div className="flex flex-col gap-3">
      <p className="text-xs text-text-tertiary">{t("agentHub.localDesc")}</p>

      <div
        role="button"
        tabIndex={0}
        onClick={() => dragEnabled && onPick()}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            if (dragEnabled) onPick();
          }
        }}
        // Visual affordance only: the real drop is the window-level
        // `desktop://file-drop` channel, which is the only one that carries an
        // absolute path (`dataTransfer` never does, and `dataTransfer.path` is
        // not a standard field WebView2 populates). `preventDefault` stops the
        // browser navigating to the dropped file instead.
        onDragOver={(e) => {
          e.preventDefault();
          if (dragEnabled) setDragOver(true);
        }}
        onDragLeave={() => setDragOver(false)}
        onDrop={(e) => {
          e.preventDefault();
          setDragOver(false);
        }}
        className={cn(
          "cursor-pointer rounded-md border-2 border-dashed p-6 text-center transition-colors",
          info ? "border-[var(--color-accent)]/40" : "border-zinc-300 hover:border-zinc-400 dark:border-zinc-600 dark:hover:border-zinc-500",
          dragOver && "border-[var(--color-accent)]/60",
        )}
        style={info ? { backgroundColor: "color-mix(in srgb, var(--color-accent) 10%, transparent)" } : undefined}
      >
        {info ? (
          <div className="text-xs">
            <div className="mb-1 font-medium" style={{ color: "var(--color-accent)" }}>
              {info.file_name}
            </div>
            <div className="text-text-tertiary">
              {formatSizeBytes(info.size_bytes)} · {info.agent_id}
            </div>
          </div>
        ) : (
          <div className="text-xs text-text-tertiary">
            <FolderPlus className="mx-auto mb-2 h-6 w-6" />
            <div>{t("agentHub.localDropHint")}</div>
          </div>
        )}
      </div>

      {error && (
        <div className="flex items-start gap-2 rounded-md bg-red-50 p-2 text-xs text-red-700 dark:bg-red-900/20 dark:text-red-300">
          <X className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          {error}
        </div>
      )}
    </div>
  );
}
